//! Internal macOS failures and their fixed mapping onto [`AiboError`].
//!
//! `AiboError` is `#[non_exhaustive]` and owned by `aibo-core`; this crate does
//! not get to add variants. The mapping below is therefore the *whole*
//! contract between the platform layer and the failure model (§13), and it is
//! written once, here, rather than at every call site.

use aibo_core::error::{AiboError, CaptureFailure, InsertFailure};
use thiserror::Error;

/// Every way a macOS platform call can fail.
#[derive(Debug, Error)]
pub enum MacosError {
    /// The Accessibility TCC grant is missing or was revoked (§17).
    #[error("accessibility permission not granted")]
    NotTrusted,

    /// `IsSecureEventInputEnabled()` is on — password fields, Terminal and
    /// password managers block both keystroke synthesis and AX reads, and
    /// another app can leave the flag stuck globally (§8).
    #[error("secure event input is enabled")]
    SecureInput,

    /// The per-call deadline expired (120 ms AX, 250 ms with the clipboard
    /// fallback — §8).
    #[error("platform call exceeded its {0} ms deadline")]
    Deadline(u64),

    /// The dedicated AX thread is gone. Unrecoverable for the process.
    #[error("the macOS platform thread is not running")]
    ThreadGone,

    /// The bounded AX queue is full because the worker is still occupied.
    #[error("the macOS platform worker queue is busy")]
    WorkerBusy,

    /// The app exposes no usable accessibility tree, or the specific attribute
    /// is unsupported (`kAXErrorAttributeUnsupported`).
    #[error("accessibility read failed: {0}")]
    Ax(&'static str),

    /// An IME composition is active. aibo neither reads nor inserts (§9).
    #[error("an IME composition is active")]
    ImeActive,

    /// The target app, window, focused element or content changed between
    /// capture and insert (§8). Never insert in this case.
    #[error("the insert target changed since capture")]
    TargetChanged,

    /// The app swallowed the synthetic chord, or focus never came back.
    #[error("the target application rejected the operation")]
    AppRejected,

    /// A CoreGraphics/AppKit call that should not fail did.
    #[error("{0}")]
    Platform(String),
}

impl MacosError {
    /// Map onto the capture half of the failure model.
    ///
    /// `app` is the bundle identifier of the target, used in the diagnostic
    /// string. A deadline expiry maps to [`CaptureFailure::NoAxTree`]: from the
    /// user's side "the app did not answer in 120 ms" and "the app has no AX
    /// tree" are the same event, and §8 requires the panel to keep working
    /// either way.
    pub fn into_capture_error(self, app: &str) -> AiboError {
        let reason = match self {
            MacosError::NotTrusted | MacosError::SecureInput => CaptureFailure::Denied,
            MacosError::ImeActive => CaptureFailure::ImeActive,
            _ => CaptureFailure::NoAxTree,
        };
        AiboError::CaptureFailed {
            app: app.to_owned(),
            reason,
        }
    }

    /// Map onto the write-back half of the failure model.
    pub fn into_insert_error(self) -> AiboError {
        let reason = match self {
            MacosError::NotTrusted | MacosError::SecureInput => InsertFailure::PermissionDenied,
            MacosError::ImeActive => InsertFailure::ImeActive,
            MacosError::TargetChanged => InsertFailure::Cancelled,
            _ => InsertFailure::AppRejected,
        };
        AiboError::InsertFailed { reason }
    }
}

/// Result alias for the macOS-internal layer.
pub type MacosResult<T> = std::result::Result<T, MacosError>;

/// Translate an `AXError` into a [`MacosError`], preserving Apple's own name
/// for the code so the diagnostic is greppable against the SDK headers.
pub(crate) fn ax_result(code: accessibility_sys::AXError) -> MacosResult<()> {
    use accessibility_sys::{kAXErrorAPIDisabled, kAXErrorSuccess};
    match code {
        c if c == kAXErrorSuccess => Ok(()),
        c if c == kAXErrorAPIDisabled => Err(MacosError::NotTrusted),
        c => Err(MacosError::Ax(accessibility_sys::error_string(c))),
    }
}
