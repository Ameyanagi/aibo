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

use std::sync::mpsc::{self, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;

use super::error::{MacosError, MacosResult};
use super::worker::{MacosConfig, Worker};

type Job = Box<dyn FnOnce(&mut Worker) + Send + 'static>;

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
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.submit(Box::new(move |worker| {
            let _ = reply_tx.send(f(worker));
        }))?;

        match tokio::time::timeout(deadline, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(MacosError::ThreadGone),
            Err(_) => Err(MacosError::Deadline(deadline.as_millis() as u64)),
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
}
