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

use std::time::Instant;

use aibo_core::types::{ClipboardItem, ClipboardKind};
use tokio::sync::{mpsc, oneshot};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
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

/// What the worker was asked to do.
enum ClipOp {
    /// Read the current contents, with hygiene flags.
    Read(oneshot::Sender<WinResult<ClipboardItem>>),
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
    op: ClipOp,
}

/// Handle to the clipboard worker thread.
#[derive(Debug, Clone)]
pub(crate) struct ClipboardHandle {
    tx: mpsc::UnboundedSender<ClipJob>,
}

impl ClipboardHandle {
    /// Start the worker.
    pub(crate) fn spawn() -> WinResult<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("aibo-win-clipboard".into())
            .spawn(move || worker(rx))
            .map_err(|e| {
                WindowsPlatformError::win32_bare("CreateThread", format!("clipboard worker: {e}"))
            })?;
        Ok(Self { tx })
    }

    fn submit(&self, deadline: Instant, op: ClipOp) -> WinResult<()> {
        self.tx
            .send(ClipJob { deadline, op })
            .map_err(|_| WindowsPlatformError::ClipboardThreadGone)
    }

    /// Read the clipboard.
    pub(crate) async fn read(&self, deadline: Instant) -> WinResult<ClipboardItem> {
        let (reply, rx) = oneshot::channel();
        self.submit(deadline, ClipOp::Read(reply))?;
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

fn worker(mut rx: mpsc::UnboundedReceiver<ClipJob>) {
    while let Some(job) = rx.blocking_recv() {
        // The caller has already given up; doing the work would only take the
        // global clipboard lock for nothing.
        if Instant::now() > job.deadline {
            tracing::debug!("clipboard job dropped: past deadline before it started");
            continue;
        }
        match job.op {
            ClipOp::Read(reply) => {
                let _ = reply.send(read_now());
            }
            ClipOp::Write { text, reply } => {
                let _ = reply.send(write_now(&text).map(|()| ClipboardHandle::sequence()));
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
                    match previous {
                        Some(text) => write_now(&text).map(|()| true),
                        // Nothing to put back — leaving aibo's payload is the
                        // lesser evil versus clearing something.
                        None => Ok(false),
                    }
                };
                let _ = reply.send(result);
            }
        }
    }
}

fn write_now(text: &str) -> WinResult<()> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|e| WindowsPlatformError::Clipboard(format!("open for write: {e}")))?;
    clipboard
        .set_text(text.to_owned())
        .map_err(|e| WindowsPlatformError::Clipboard(format!("set text: {e}")))
}

fn read_now() -> WinResult<ClipboardItem> {
    let sequence = ClipboardHandle::sequence();
    let hygiene = read_hygiene();

    let files = read_files();
    let text = if hygiene.concealed {
        // §12: concealed items are never recorded and never sent. Not read at
        // all, so there is nothing to leak into a log or a prompt.
        None
    } else {
        arboard::Clipboard::new()
            .and_then(|mut c| c.get_text())
            .ok()
            .filter(|t| !t.is_empty())
    };

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

    // SAFETY: the clipboard is opened with a null owner, every read is bounded
    // by the format enumeration, and `CloseClipboard` runs on every path.
    unsafe {
        let exclude = RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
        let history = RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory"));
        let cloud = RegisterClipboardFormatW(w!("CanUploadToCloudClipboard"));

        if OpenClipboard(None).is_err() {
            // Another process holds the lock. Fail closed: assume concealed
            // rather than reading a payload aibo could not vet.
            h.concealed = true;
            h.restorable = false;
            return h;
        }

        h.concealed = exclude != 0 && IsClipboardFormatAvailable(exclude).is_ok();
        h.transient = [history, cloud]
            .into_iter()
            .filter(|f| *f != 0)
            .any(|f| read_dword(f) == Some(0));

        // Enumerate what is on the clipboard: anything outside the plain-text
        // family means a plain-text restore would lose information (§12).
        let mut format = EnumClipboardFormats(0);
        while format != 0 {
            h.any_format = true;
            if !RESTORABLE_FORMATS.contains(&format) {
                h.restorable = false;
            }
            format = EnumClipboardFormats(format);
        }

        let _ = CloseClipboard();
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
        let hglobal = windows::Win32::Foundation::HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return None;
        }
        let value = ptr.cast::<u32>().read_unaligned();
        let _ = GlobalUnlock(hglobal);
        Some(value)
    }
}

/// Read `CF_HDROP` as a list of paths.
fn read_files() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    // SAFETY: the clipboard is opened and closed around the whole read;
    // `DragQueryFileW` is called first for the count, then per index with a
    // buffer sized from the same API.
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP).is_err() || OpenClipboard(None).is_err() {
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
        let _ = CloseClipboard();
    }
    out
}
