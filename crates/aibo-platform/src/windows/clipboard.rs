//! The clipboard worker (§8 insert path, §12 hygiene).
//!
//! Clipboard access on Windows is a global, single-owner lock: `OpenClipboard`
//! fails if another process holds it, and holding it blocks every other app. So
//! it gets its own thread with a serialised queue, and every operation opens,
//! reads and closes immediately.
//!
//! Three §12 rules are implemented here rather than left to callers:
//!
//! 1. **Concealed content is never captured.** Password managers mark their
//!    payload with `ExcludeClipboardContentFromMonitorProcessing`; when it is
//!    present, [`ClipboardItem::text`] is `None`, always.
//! 2. **Restore is a race, not an assignment.** The paste path saves the
//!    previous contents and the sequence number, and restores *only* if the
//!    sequence is still the one aibo itself produced. If anything else wrote
//!    the clipboard in between, the user's copy wins.
//! 3. **Decline rather than downgrade.** If the clipboard held formats aibo
//!    cannot reproduce (HTML, RTF, delayed rendering), `restorable` is false —
//!    replacing rich content with plain text is data loss dressed up as a
//!    feature.

use std::time::{Duration, Instant};

use aibo_core::types::{ClipboardItem, ClipboardKind};
use tokio::sync::{mpsc, oneshot};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows_core::w;

use super::error::{WinResult, WindowsPlatformError};

/// `CF_HDROP`. Used as a bare `u32` because that is what the clipboard APIs
/// take, which keeps one more namespace out of the import list.
const CF_HDROP: u32 = 15;
/// `CF_UNICODETEXT`.
const CF_UNICODETEXT: u32 = 13;
/// `CF_TEXT`.
const CF_TEXT: u32 = 1;
/// `CF_OEMTEXT`.
const CF_OEMTEXT: u32 = 7;
/// `CF_LOCALE`.
const CF_LOCALE: u32 = 16;

/// Formats aibo can put back byte-for-byte. Anything else present means a
/// restore would silently downgrade the user's clipboard (§12).
const RESTORABLE_FORMATS: [u32; 4] = [CF_UNICODETEXT, CF_TEXT, CF_OEMTEXT, CF_LOCALE];

/// Maximum clipboard operations waiting behind the in-flight operation.
///
/// Normal insert/capture traffic has at most a save, write, and restore in
/// sequence. Eight slots tolerate several concurrent callers while bounding
/// retained clipboard text if the global clipboard lock stalls the worker.
const CLIPBOARD_QUEUE_CAPACITY: usize = 8;

/// What the worker was asked to do.
enum ClipOp {
    /// Read the current contents, with hygiene flags.
    Read {
        /// The focused executable is a password manager. This is decided
        /// before the worker touches payload bytes.
        conceal_source: bool,
        reply: oneshot::Sender<WinResult<ClipboardItem>>,
    },
    /// Write text; replies with the resulting sequence number so the caller can
    /// later prove nothing else has written since.
    Write {
        text: String,
        reply: oneshot::Sender<WinResult<u32>>,
    },
    /// Put `previous` back, but only if the sequence number is still
    /// `expect_sequence` (§12).
    Restore {
        previous: Option<String>,
        expect_sequence: u32,
        reply: oneshot::Sender<WinResult<bool>>,
    },
}

struct ClipJob {
    deadline: Instant,
    budget: Duration,
    op: ClipOp,
}

/// Handle to the clipboard worker thread.
#[derive(Debug, Clone)]
pub(crate) struct ClipboardHandle {
    tx: mpsc::Sender<ClipJob>,
}

impl ClipboardHandle {
    /// Start the worker.
    pub(crate) fn spawn() -> WinResult<Self> {
        let (tx, rx) = mpsc::channel(CLIPBOARD_QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("aibo-win-clipboard".into())
            .spawn(move || worker(rx))
            .map_err(|e| {
                WindowsPlatformError::win32_bare("CreateThread", format!("clipboard worker: {e}"))
            })?;
        Ok(Self { tx })
    }

    fn submit(&self, deadline: Instant, op: ClipOp) -> WinResult<()> {
        let budget = deadline.saturating_duration_since(Instant::now());
        match self.tx.try_send(ClipJob {
            deadline,
            budget,
            op,
        }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(WindowsPlatformError::WorkerBusy {
                worker: "clipboard",
            }),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(WindowsPlatformError::ClipboardThreadGone)
            }
        }
    }

    /// Read the clipboard.
    pub(crate) async fn read(
        &self,
        deadline: Instant,
        conceal_source: bool,
    ) -> WinResult<ClipboardItem> {
        let (reply, rx) = oneshot::channel();
        self.submit(
            deadline,
            ClipOp::Read {
                conceal_source,
                reply,
            },
        )?;
        rx.await
            .map_err(|_| WindowsPlatformError::ClipboardThreadGone)?
    }

    /// Write text, returning the sequence number it produced.
    pub(crate) async fn write(&self, text: String, deadline: Instant) -> WinResult<u32> {
        let (reply, rx) = oneshot::channel();
        self.submit(deadline, ClipOp::Write { text, reply })?;
        rx.await
            .map_err(|_| WindowsPlatformError::ClipboardThreadGone)?
    }

    /// Restore a previous payload if and only if nothing else has written since
    /// `expect_sequence`. Returns whether the restore actually happened.
    pub(crate) async fn restore(
        &self,
        previous: Option<String>,
        expect_sequence: u32,
        deadline: Instant,
    ) -> WinResult<bool> {
        let (reply, rx) = oneshot::channel();
        self.submit(
            deadline,
            ClipOp::Restore {
                previous,
                expect_sequence,
                reply,
            },
        )?;
        rx.await
            .map_err(|_| WindowsPlatformError::ClipboardThreadGone)?
    }

    /// The current sequence number, without touching the clipboard lock.
    pub(crate) fn sequence() -> u32 {
        // SAFETY: no arguments, no pointers, no clipboard lock taken.
        unsafe { GetClipboardSequenceNumber() }
    }
}

fn worker(mut rx: mpsc::Receiver<ClipJob>) {
    while let Some(job) = rx.blocking_recv() {
        // The caller has already given up; doing the work would only take the
        // global clipboard lock for nothing.
        if Instant::now() > job.deadline {
            tracing::debug!("clipboard job dropped: past deadline before it started");
            reply_deadline(job.op, job.budget);
            continue;
        }
        let deadline = job.deadline;
        let budget = job.budget;
        match job.op {
            ClipOp::Read {
                conceal_source,
                reply,
            } => {
                let _ = reply.send(read_now(conceal_source, deadline, budget));
            }
            ClipOp::Write { text, reply } => {
                let _ = reply.send(write_now(Some(&text), deadline, budget));
            }
            ClipOp::Restore {
                previous,
                expect_sequence,
                reply,
            } => {
                let result = if ClipboardHandle::sequence() != expect_sequence {
                    // §12: something else wrote the clipboard. The user's copy
                    // wins; aibo's restore is abandoned, not forced.
                    tracing::debug!("clipboard restore skipped: sequence moved");
                    Ok(false)
                } else {
                    // `None` is an explicitly empty prior clipboard. Callers
                    // only enqueue a restore for faithfully restorable items,
                    // so it is safe (and necessary) to clear aibo's payload.
                    write_now(previous.as_deref(), deadline, budget).map(|_| true)
                };
                let _ = reply.send(result);
            }
        }
    }
}

fn reply_deadline(op: ClipOp, budget: Duration) {
    let error = || WindowsPlatformError::Deadline(budget);
    match op {
        ClipOp::Read { reply, .. } => {
            let _ = reply.send(Err(error()));
        }
        ClipOp::Write { reply, .. } => {
            let _ = reply.send(Err(error()));
        }
        ClipOp::Restore { reply, .. } => {
            let _ = reply.send(Err(error()));
        }
    }
}

/// An open clipboard lock. Windows makes the lock process-global, so closing
/// it in `Drop` is more than cleanup: every missed close would wedge all later
/// clipboard operations in aibo and other applications.
struct OpenClipboardGuard;

impl OpenClipboardGuard {
    fn acquire(deadline: Instant, budget: Duration) -> WinResult<Self> {
        loop {
            // SAFETY: a null owner is valid for a short-lived read/write operation.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Self);
            }
            if Instant::now() >= deadline {
                return Err(WindowsPlatformError::Deadline(budget));
            }
            // This is the dedicated clipboard thread. A small bounded sleep
            // avoids turning another process's brief lock into a false failure.
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: this value is only constructed after OpenClipboard succeeds.
        let _ = unsafe { CloseClipboard() };
    }
}

/// Movable global memory prepared for `SetClipboardData`.
///
/// Until ownership is transferred to the clipboard, failures free the block.
struct ClipboardMemory(Option<HGLOBAL>);

impl ClipboardMemory {
    fn unicode(text: &str) -> WinResult<Self> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let byte_len = wide.len() * std::mem::size_of::<u16>();
        // SAFETY: the size is derived from a live vector and GMEM_MOVEABLE is
        // required by SetClipboardData.
        let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) }
            .map_err(|e| WindowsPlatformError::win32("GlobalAlloc", e))?;
        let owned = Self(Some(memory));
        // SAFETY: `memory` was just allocated for exactly `byte_len` bytes.
        let ptr = unsafe { GlobalLock(memory) };
        if ptr.is_null() {
            return Err(WindowsPlatformError::win32_bare(
                "GlobalLock",
                "could not lock clipboard text memory",
            ));
        }
        // SAFETY: source and destination are valid for `byte_len` bytes and do
        // not overlap. The memory is unlocked before it is handed to Windows.
        unsafe {
            std::ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), ptr.cast(), byte_len);
            let _ = GlobalUnlock(memory);
        }
        Ok(owned)
    }

    fn transfer(mut self) -> HGLOBAL {
        self.0.take().expect("clipboard memory already transferred")
    }
}

impl Drop for ClipboardMemory {
    fn drop(&mut self) {
        if let Some(memory) = self.0.take() {
            // SAFETY: ownership has not been transferred to SetClipboardData.
            let _ = unsafe { GlobalFree(Some(memory)) };
        }
    }
}

/// Replace the clipboard with text, or clear it for an explicitly empty saved
/// clipboard. Returns the generation while the clipboard lock is still held,
/// so another process cannot interpose a write between ours and the snapshot.
fn write_now(text: Option<&str>, deadline: Instant, budget: Duration) -> WinResult<u32> {
    // Allocate before EmptyClipboard: an allocation failure must not destroy
    // the user's existing contents.
    let memory = text.map(ClipboardMemory::unicode).transpose()?;
    let _clipboard = OpenClipboardGuard::acquire(deadline, budget)?;
    // SAFETY: the current thread owns the clipboard lock.
    unsafe { EmptyClipboard() }.map_err(|e| WindowsPlatformError::win32("EmptyClipboard", e))?;
    if let Some(memory) = memory {
        let memory = memory.transfer();
        // SAFETY: the global block is movable, unlocked, NUL-terminated UTF-16.
        // A successful call transfers ownership to Windows.
        if let Err(error) = unsafe { SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))) } {
            // SetClipboardData did not take ownership on failure.
            let _ = unsafe { GlobalFree(Some(memory)) };
            return Err(WindowsPlatformError::win32("SetClipboardData", error));
        }
    }
    Ok(ClipboardHandle::sequence())
}

fn read_now(conceal_source: bool, deadline: Instant, budget: Duration) -> WinResult<ClipboardItem> {
    // Hygiene flags, formats, payload and generation must describe one locked
    // clipboard state. Splitting these reads can combine a password manager's
    // flags with another application's text (or vice versa).
    let _clipboard = OpenClipboardGuard::acquire(deadline, budget)?;
    let sequence = ClipboardHandle::sequence();
    let mut hygiene = read_hygiene();
    hygiene.concealed |= conceal_source;

    let files = if hygiene.concealed {
        Vec::new()
    } else {
        read_files()
    };
    let text = if hygiene.concealed {
        // §12: concealed items are never recorded and never sent. Not read at
        // all, so there is nothing to leak into a log or a prompt.
        None
    } else {
        read_text()
    };
    if hygiene.any_format && hygiene.restorable && text.is_none() {
        // A Unicode format existed but could not be decoded/read. `None` must
        // remain reserved for a genuinely empty clipboard during restoration.
        hygiene.restorable = false;
    }

    let kind = if text.is_some() {
        ClipboardKind::Text
    } else if !files.is_empty() {
        ClipboardKind::Files
    } else if hygiene.any_format {
        ClipboardKind::Unsupported
    } else {
        ClipboardKind::Empty
    };

    Ok(ClipboardItem {
        kind,
        text,
        files,
        concealed: hygiene.concealed,
        transient: hygiene.transient,
        // SPIKE: S4 — attributing the payload to a source app needs
        // `GetClipboardOwner` plus a pid lookup, and the owner is frequently
        // NULL (delayed rendering, or an owner that has already exited). Left
        // `None` rather than guessed; §12's source denylist needs this filled
        // in before it can work.
        source_app: None,
        sequence: u64::from(sequence),
        restorable: hygiene.restorable,
    })
}

#[derive(Default)]
struct Hygiene {
    concealed: bool,
    transient: bool,
    restorable: bool,
    any_format: bool,
}

/// Read §12's hygiene markers.
///
/// The markers are registered clipboard formats, not data: presence of
/// `ExcludeClipboardContentFromMonitorProcessing` means "do not process this at
/// all", while `CanIncludeInClipboardHistory` and `CanUploadToCloudClipboard`
/// carry a `DWORD` that is zero when the source opted out.
fn read_hygiene() -> Hygiene {
    let mut h = Hygiene {
        restorable: true,
        ..Default::default()
    };

    // SAFETY: `read_now` holds the clipboard lock for this entire snapshot.
    unsafe {
        let exclude = RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
        let history = RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory"));
        let cloud = RegisterClipboardFormatW(w!("CanUploadToCloudClipboard"));

        h.concealed = exclude != 0 && IsClipboardFormatAvailable(exclude).is_ok();
        h.transient = [history, cloud]
            .into_iter()
            .filter(|f| *f != 0)
            .any(|f| read_dword(f) == Some(0));

        // Enumerate what is on the clipboard: anything outside the plain-text
        // family means a plain-text restore would lose information (§12).
        let mut format = EnumClipboardFormats(0);
        let mut saw_unicode = false;
        while format != 0 {
            h.any_format = true;
            saw_unicode |= format == CF_UNICODETEXT;
            if !RESTORABLE_FORMATS.contains(&format) {
                h.restorable = false;
            }
            format = EnumClipboardFormats(format);
        }
        // The native restore path reproduces Unicode text. A legacy ANSI/OEM
        // item without CF_UNICODETEXT cannot be reconstructed without knowing
        // its source code page, so it is not faithfully restorable.
        if h.any_format && !saw_unicode {
            h.restorable = false;
        }
    }
    h
}

/// Read a `DWORD`-valued clipboard format. The clipboard must already be open.
///
/// # Safety
/// The caller holds the clipboard lock, and `format` is a registered format id.
unsafe fn read_dword(format: u32) -> Option<u32> {
    unsafe {
        if IsClipboardFormatAvailable(format).is_err() {
            return None;
        }
        let handle = GetClipboardData(format).ok()?;
        let hglobal = HGLOBAL(handle.0);
        if GlobalSize(hglobal) < std::mem::size_of::<u32>() {
            return None;
        }
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return None;
        }
        let value = ptr.cast::<u32>().read_unaligned();
        let _ = GlobalUnlock(hglobal);
        Some(value)
    }
}

/// Read `CF_UNICODETEXT`. The clipboard must already be open.
fn read_text() -> Option<String> {
    // SAFETY: `read_now` owns the clipboard lock. GlobalSize bounds the slice,
    // and the locked handle is released on every path after a successful lock.
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() {
            return None;
        }
        let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
        let memory = HGLOBAL(handle.0);
        let units = GlobalSize(memory) / std::mem::size_of::<u16>();
        if units == 0 {
            return None;
        }
        let ptr = GlobalLock(memory);
        if ptr.is_null() {
            return None;
        }
        let slice = std::slice::from_raw_parts(ptr.cast::<u16>(), units);
        let end = slice.iter().position(|unit| *unit == 0).unwrap_or(units);
        let text = String::from_utf16(&slice[..end]).ok();
        let _ = GlobalUnlock(memory);
        text
    }
}

/// Read `CF_HDROP` as a list of paths.
fn read_files() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    // SAFETY: `read_now` holds the clipboard lock; `DragQueryFileW` is called
    // first for the count, then per index with a buffer sized from the same API.
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP).is_err() {
            return out;
        }
        if let Ok(handle) = GetClipboardData(CF_HDROP) {
            let hdrop = HDROP(handle.0);
            let count = DragQueryFileW(hdrop, u32::MAX, None);
            for index in 0..count {
                let needed = DragQueryFileW(hdrop, index, None) as usize;
                if needed == 0 {
                    continue;
                }
                let mut buf = vec![0u16; needed + 1];
                let written = DragQueryFileW(hdrop, index, Some(&mut buf)) as usize;
                if written > 0 {
                    out.push(std::path::PathBuf::from(String::from_utf16_lossy(
                        &buf[..written],
                    )));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    fn read_op() -> ClipOp {
        let (reply, _reply_rx) = oneshot::channel();
        ClipOp::Read {
            conceal_source: false,
            reply,
        }
    }

    #[test]
    fn saturated_queue_fails_immediately_with_sanitized_error() {
        let (tx, mut rx) = mpsc::channel(1);
        let handle = ClipboardHandle { tx };
        let deadline = Instant::now() + std::time::Duration::from_secs(1);

        handle.submit(deadline, read_op()).unwrap();
        let error = handle.submit(deadline, read_op()).unwrap_err();

        assert!(matches!(
            error,
            WindowsPlatformError::WorkerBusy {
                worker: "clipboard"
            }
        ));
        assert_eq!(error.to_string(), "the clipboard worker queue is busy");
        assert!(rx.try_recv().is_ok(), "the first queued job is preserved");
    }

    #[test]
    fn closed_queue_reports_the_dead_worker() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let handle = ClipboardHandle { tx };

        assert!(matches!(
            handle.submit(Instant::now(), read_op()),
            Err(WindowsPlatformError::ClipboardThreadGone)
        ));
    }

    #[test]
    fn stale_job_receives_a_typed_deadline_reply() {
        let (reply, reply_rx) = oneshot::channel();
        let budget = Duration::from_millis(123);
        reply_deadline(
            ClipOp::Write {
                text: "secret payload is never formatted into the error".to_owned(),
                reply,
            },
            budget,
        );

        assert!(matches!(
            reply_rx.blocking_recv(),
            Ok(Err(WindowsPlatformError::Deadline(value))) if value == budget
        ));
    }
}
