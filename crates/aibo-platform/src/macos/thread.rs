//! The dedicated platform thread and its channel handle (§7).
//!
//! §7 is explicit that this is a shape requirement, not a detail:
//!
//! > The trait must be a **handle to a dedicated platform thread**: the real
//! > UIA/AX objects live on that thread and the trait's methods send requests
//! > over a channel and await a reply, which is also where the per-call
//! > timeouts from §8 belong.
//!
//! macOS needs it for the same reason Windows does, if not the same mechanism:
//! a synchronous `AXUIElementCopyAttributeValue` against a busy app blocks for
//! *seconds*, and doing that on the iced event loop freezes the panel that was
//! supposed to appear instantly.
//!
//! The deadline is enforced twice, on purpose:
//!
//! 1. `AXUIElementSetMessagingTimeout` on the worker's elements, so a hung app
//!    cannot wedge the thread and starve the *next* request;
//! 2. `tokio::time::timeout` on the reply channel here, so the caller is never
//!    blocked past its §8 budget even if step 1 is not honoured.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::error::{MacosError, MacosResult};
use super::worker::{MacosConfig, Worker};

type Job = Box<dyn FnOnce(&mut Worker) + Send + 'static>;

const JOB_QUEUED: u8 = 0;
const JOB_RUNNING: u8 = 1;
const JOB_CANCELLED: u8 = 2;

/// Maximum AX operations waiting behind the in-flight operation.
///
/// A capture fans out to only a handful of AX reads. Eight buffered jobs absorb
/// overlapping hotkeys without retaining an unbounded number of captured app
/// references when a foreign accessibility tree stalls.
const PLATFORM_QUEUE_CAPACITY: usize = 8;

/// A `Send + Sync` handle to the thread that owns the AX objects.
pub(crate) struct PlatformThread {
    tx: SyncSender<Job>,
    handle: Option<JoinHandle<()>>,
}

impl PlatformThread {
    /// Start the thread.
    pub(crate) fn spawn(config: MacosConfig) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Job>(PLATFORM_QUEUE_CAPACITY);
        let handle = std::thread::Builder::new()
            .name("aibo-macos-ax".to_owned())
            .spawn(move || {
                let mut worker = Worker::new(config);
                // The channel closing is the shutdown signal; `Drop` below
                // arranges it.
                while let Ok(job) = rx.recv() {
                    job(&mut worker);
                }
            })
            .expect("spawning the macOS platform thread");
        Self {
            tx,
            handle: Some(handle),
        }
    }

    fn submit(&self, job: Job) -> MacosResult<()> {
        match self.tx.try_send(job) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => Err(MacosError::WorkerBusy),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(MacosError::ThreadGone),
        }
    }

    /// Run `f` on the platform thread and await its result within `deadline`.
    ///
    /// A deadline expiry does **not** cancel the work — AX has no cancellation
    /// — it abandons the reply. The worker finishes and drops the result, which
    /// is why the closure must not have side effects the caller depends on
    /// observing.
    pub(crate) async fn call<T, F>(&self, deadline: Duration, f: F) -> MacosResult<T>
    where
        F: FnOnce(&mut Worker) -> MacosResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let expires = Instant::now() + deadline;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.submit(Box::new(move |worker| {
            if Instant::now() >= expires {
                let _ = reply_tx.send(Err(MacosError::Deadline(deadline.as_millis() as u64)));
                return;
            }
            let _ = reply_tx.send(f(worker));
        }))?;

        match tokio::time::timeout(deadline, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(MacosError::ThreadGone),
            Err(_) => Err(MacosError::Deadline(deadline.as_millis() as u64)),
        }
    }

    /// Run a side-effecting operation without allowing an abandoned queued job
    /// to execute later.
    ///
    /// If the caller's deadline expires while the job is still queued, the job
    /// is atomically cancelled. If it has already begun, the caller waits for
    /// the result instead of reporting failure while the mutation continues in
    /// the background. Side-effecting worker operations are themselves bounded.
    pub(crate) async fn call_side_effect<T, F>(&self, deadline: Duration, f: F) -> MacosResult<T>
    where
        F: FnOnce(&mut Worker) -> MacosResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let expires = Instant::now() + deadline;
        let state = Arc::new(AtomicU8::new(JOB_QUEUED));
        let worker_state = Arc::clone(&state);
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        self.submit(Box::new(move |worker| {
            if Instant::now() >= expires
                || worker_state
                    .compare_exchange(JOB_QUEUED, JOB_RUNNING, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                let _ = reply_tx.send(Err(MacosError::Deadline(deadline.as_millis() as u64)));
                return;
            }
            let _ = reply_tx.send(f(worker));
        }))?;

        match tokio::time::timeout(deadline, &mut reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(MacosError::ThreadGone),
            Err(_) => {
                if state
                    .compare_exchange(
                        JOB_QUEUED,
                        JOB_CANCELLED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    Err(MacosError::Deadline(deadline.as_millis() as u64))
                } else {
                    reply_rx.await.map_err(|_| MacosError::ThreadGone)?
                }
            }
        }
    }
}

impl Drop for PlatformThread {
    fn drop(&mut self) {
        // Dropping the sender ends the worker's `recv` loop.
        let (dead, _) = mpsc::sync_channel::<Job>(1);
        let _ = std::mem::replace(&mut self.tx, dead);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn no_op() -> Job {
        Box::new(|_| {})
    }

    #[test]
    fn saturated_queue_fails_immediately_with_sanitized_error() {
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let thread = PlatformThread { tx, handle: None };

        thread.submit(no_op()).unwrap();
        let error = thread.submit(no_op()).unwrap_err();

        assert!(matches!(error, MacosError::WorkerBusy));
        assert_eq!(error.to_string(), "the macOS platform worker queue is busy");
        assert!(rx.try_recv().is_ok(), "the first queued job is preserved");
    }

    #[test]
    fn closed_queue_reports_the_dead_worker() {
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        drop(rx);
        let thread = PlatformThread { tx, handle: None };

        assert!(matches!(
            thread.submit(no_op()),
            Err(MacosError::ThreadGone)
        ));
    }

    #[tokio::test]
    async fn expired_side_effect_is_cancelled_before_it_starts() {
        let thread = PlatformThread::spawn(MacosConfig::default());
        thread
            .submit(Box::new(|_| {
                std::thread::sleep(Duration::from_millis(40));
            }))
            .unwrap();
        let mutations = Arc::new(AtomicUsize::new(0));
        let worker_mutations = Arc::clone(&mutations);

        assert!(matches!(
            thread
                .call_side_effect(Duration::from_millis(5), move |_| {
                    worker_mutations.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                })
                .await,
            Err(MacosError::Deadline(_))
        ));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(mutations.load(Ordering::Relaxed), 0);
    }
}
