//! Typed failures for the Windows backend (§8, §13).
//!
//! The important one is [`WindowsPlatformError::UipiBlocked`]. §8 is explicit
//! that the Windows permission story is **not "none"**: User Interface
//! Privilege Isolation stops a normal-integrity process from reading — or
//! `SendInput`-ing to — a window owned by an elevated process (Task Manager,
//! admin consoles, installers, IT tooling). Win32 reports this as an ordinary
//! "nothing happened": `SendInput` returns 0, UIA returns empty trees. If that
//! is allowed to look like success, aibo silently does nothing in exactly the
//! power-user contexts it is sold into. So it is a variant, it is surfaced, and
//! it is never swallowed.

use std::time::Duration;

use aibo_core::error::{AiboError, CaptureFailure, InsertFailure};
use thiserror::Error;

/// Everything the Windows platform layer can fail with.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WindowsPlatformError {
    /// The target window belongs to a process at a higher integrity level.
    ///
    /// UIPI blocks the read or the synthetic input. Elevating aibo is not the
    /// answer; `uiAccess=true` requires Authenticode signing *and* installation
    /// under `%ProgramFiles%` (§8), so for most builds this is permanent and
    /// must be explained rather than retried.
    #[error("blocked by UIPI: process {pid} runs at a higher integrity level")]
    UipiBlocked {
        /// Process id of the target window's owner, when it could be read.
        pid: u32,
    },

    /// The dedicated UIA (MTA) thread is not running — it failed to start, or
    /// it panicked and the handle is now dead.
    #[error("the UI Automation thread is not running")]
    UiaThreadGone,

    /// The clipboard worker thread is not running.
    #[error("the clipboard thread is not running")]
    ClipboardThreadGone,

    /// The call did not answer inside its deadline (§8: 120 ms for UIA, 250 ms
    /// including the clipboard fallback).
    #[error("the platform call exceeded its {0:?} deadline")]
    Deadline(Duration),

    /// A UI Automation call failed.
    #[error("UI Automation: {0}")]
    Uia(String),

    /// The focused control has no usable UIA text pattern at all.
    #[error("the focused control exposes no UI Automation text pattern")]
    NoTextPattern,

    /// The control's `SupportedTextSelection` is `None`.
    ///
    /// This is the trap §8 names: `GetSelection` on such a control returns
    /// **success with NULL ranges**, not an error, so the gate has to be the
    /// property, not the call's return value.
    #[error("the focused control does not support text selection")]
    NoTextSelectionSupport,

    /// An IME composition is active on the foreground window (§9). aibo neither
    /// reads nor inserts while composing.
    #[error("an IME composition is active")]
    ImeActive,

    /// The focused control is a password field (`IsPassword`), so §5 forbids
    /// reading it.
    ///
    /// This used to be `Ok(None)` — "nothing selected". That was wrong twice
    /// over: the panel could not tell the user *why* it had nothing, and
    /// `selected_text`'s synthetic-`Ctrl+C` fallback treats `Ok(None)` as
    /// "primary read found nothing, try harder" and would have copied the
    /// password onto the clipboard. A refusal has to be a value the caller
    /// cannot mistake for an empty success.
    ///
    /// The macOS twin is `MacosError::SecureInput`.
    #[error("the focused control is a password field")]
    SecureField,

    /// A raw Win32 call failed.
    #[error("Win32 `{call}` failed")]
    Win32 {
        /// Name of the API that failed, for diagnostics.
        call: &'static str,
        /// The underlying `HRESULT` / `GetLastError` value.
        #[source]
        source: windows_core::Error,
    },

    /// A Win32 call reported failure without an error code worth carrying.
    #[error("Win32 `{call}` failed: {detail}")]
    Win32Bare {
        /// Name of the API that failed.
        call: &'static str,
        /// What went wrong.
        detail: String,
    },

    /// A clipboard read or write failed.
    #[error("clipboard: {0}")]
    Clipboard(String),

    /// Focus could not be confirmed back on the capture target within the
    /// retry budget (§8). Never paste after this — an unconfirmed restore
    /// races and pastes into the wrong window.
    #[error("could not confirm focus returned to the target window")]
    FocusNotRestored,

    /// The capture target changed between capture and insert (§8).
    #[error("the insert target changed since capture")]
    TargetChanged,
}

impl WindowsPlatformError {
    /// Convenience constructor for a failed Win32 call.
    pub(crate) fn win32(call: &'static str, source: windows_core::Error) -> Self {
        Self::Win32 { call, source }
    }

    /// Convenience constructor for a Win32 call that only reports a boolean.
    pub(crate) fn win32_bare(call: &'static str, detail: impl Into<String>) -> Self {
        Self::Win32Bare {
            call,
            detail: detail.into(),
        }
    }

    /// True when the failure is UIPI, i.e. permanent for a non-`uiAccess`
    /// build and worth its own user-facing explanation.
    pub fn is_uipi(&self) -> bool {
        matches!(self, Self::UipiBlocked { .. })
    }

    /// True when §5's secure-field rule refused the read.
    ///
    /// Callers must check this before any "try harder" fallback: the
    /// synthetic-`Ctrl+C` path in [`super::WindowsBackend::selected_text`] runs
    /// precisely when the primary read produced nothing, which is exactly the
    /// shape a refusal would take if it were flattened into `Ok(None)`.
    pub fn is_secure_field(&self) -> bool {
        matches!(self, Self::SecureField)
    }

    /// Map onto [`AiboError::CaptureFailed`] with the app the read targeted.
    pub fn into_capture_error(self, app: impl Into<String>) -> AiboError {
        let reason = match self {
            Self::UipiBlocked { .. } | Self::SecureField => CaptureFailure::Denied,
            Self::ImeActive => CaptureFailure::ImeActive,
            Self::NoTextPattern | Self::NoTextSelectionSupport | Self::Uia(_) => {
                CaptureFailure::NoAxTree
            }
            _ => CaptureFailure::NoAxTree,
        };
        AiboError::CaptureFailed {
            app: app.into(),
            reason,
        }
    }

    /// Map onto [`AiboError::InsertFailed`].
    pub fn into_insert_error(self) -> AiboError {
        let reason = match self {
            Self::UipiBlocked { .. } | Self::SecureField => InsertFailure::PermissionDenied,
            Self::ImeActive => InsertFailure::ImeActive,
            Self::TargetChanged | Self::FocusNotRestored => InsertFailure::Cancelled,
            _ => InsertFailure::AppRejected,
        };
        AiboError::InsertFailed { reason }
    }
}

impl From<WindowsPlatformError> for AiboError {
    fn from(e: WindowsPlatformError) -> Self {
        AiboError::Internal(Box::new(e))
    }
}

/// Result alias for the Windows backend's internal plumbing.
pub type WinResult<T> = std::result::Result<T, WindowsPlatformError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// §5's refusal has to reach the failure model as a *denial*, not as
    /// "this app has no accessibility tree" — the two get different copy and
    /// only one of them is correct for a password field.
    #[test]
    fn a_secure_field_is_a_denial_on_both_halves() {
        assert!(matches!(
            WindowsPlatformError::SecureField.into_capture_error("chrome.exe"),
            AiboError::CaptureFailed {
                reason: CaptureFailure::Denied,
                ..
            }
        ));
        assert!(matches!(
            WindowsPlatformError::SecureField.into_insert_error(),
            AiboError::InsertFailed {
                reason: InsertFailure::PermissionDenied,
            }
        ));
    }

    /// The predicate the synthetic-Ctrl+C fallback is gated on. If this ever
    /// returns `false` for `SecureField`, `WindowsBackend::selected_text`
    /// silently starts copying passwords to the clipboard.
    #[test]
    fn is_secure_field_discriminates() {
        assert!(WindowsPlatformError::SecureField.is_secure_field());
        assert!(!WindowsPlatformError::SecureField.is_uipi());
        assert!(!WindowsPlatformError::NoTextPattern.is_secure_field());
        assert!(!WindowsPlatformError::UipiBlocked { pid: 4 }.is_secure_field());
    }
}
