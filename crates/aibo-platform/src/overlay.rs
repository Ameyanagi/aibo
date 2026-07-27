//! Safe, platform-neutral entry points for transient panel windows.
//!
//! The caller obtains a borrowed [`WindowHandle`] from iced/winit and calls
//! these functions while the native window is still alive. Keeping the borrow
//! is important: accepting a bare `RawWindowHandle` in a safe API would allow a
//! caller to manufacture a dangling AppKit pointer or stale `HWND`.

use raw_window_handle::{RawWindowHandle, WindowHandle};
use thiserror::Error;

/// Whether the OS accepted the native backdrop request.
///
/// Backdrops are cosmetic and deliberately do not make panel setup fail on an
/// older OS or a compositor that does not support them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackdropStatus {
    /// The platform accepted its HUD/acrylic backdrop.
    Applied,
    /// The window remains usable with its ordinary transparent background.
    Unavailable,
}

/// Result of applying the durable panel-window policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayWindowConfiguration {
    /// Native backdrop availability for diagnostics and graceful fallback.
    pub backdrop: BackdropStatus,
}

/// Failure to configure or present the native panel window.
#[derive(Debug, Error)]
pub enum OverlayWindowError {
    /// The handle did not belong to the platform this binary was built for.
    #[error("expected a {expected} window handle, got {actual}")]
    UnsupportedHandle {
        /// Handle kind required by this target.
        expected: &'static str,
        /// Handle kind supplied by the caller.
        actual: &'static str,
    },
    /// AppKit window mutation is main-thread-only.
    #[error("AppKit panel configuration must run on the main thread")]
    MainThreadRequired,
    /// winit's native view exists but has not joined an `NSWindow` yet.
    #[error("the native view is not attached to a window yet")]
    DetachedNativeView,
    /// A required native operation failed.
    #[error("{operation} failed: {reason}")]
    Native {
        /// Short, content-free operation label.
        operation: &'static str,
        /// OS error text. This never contains panel or announcement content.
        reason: String,
    },
}

/// Apply the durable utility-overlay policy to the panel.
///
/// This configures native backdrop treatment, floating/utility behavior,
/// taskbar or window-menu exclusion, and macOS all-Spaces behavior. It does
/// not show or activate the window.
///
/// Call from `iced::window::run` after the panel's `WindowOpened` event. The
/// borrowed [`WindowHandle`] guarantees that native pointers remain live for
/// the duration of this call.
pub fn configure_panel_window(
    handle: WindowHandle<'_>,
) -> Result<OverlayWindowConfiguration, OverlayWindowError> {
    let raw = handle.as_raw();
    #[cfg(target_os = "macos")]
    {
        let RawWindowHandle::AppKit(handle) = raw else {
            return Err(wrong_handle("AppKit", raw));
        };
        crate::macos::overlay::configure_panel_window(handle)
    }
    #[cfg(target_os = "windows")]
    {
        let RawWindowHandle::Win32(handle) = raw else {
            return Err(wrong_handle("Win32", raw));
        };
        crate::windows::overlay::configure_panel_window(handle)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err(wrong_handle("AppKit or Win32", raw))
    }
}

/// Show the panel without activating aibo or replacing the foreground app.
///
/// The panel remains capable of becoming key/foreground later when the user
/// explicitly clicks it. In particular, Windows does **not** permanently set
/// `WS_EX_NOACTIVATE`, because doing so would break text input.
pub fn present_panel_without_activation(
    handle: WindowHandle<'_>,
) -> Result<(), OverlayWindowError> {
    let raw = handle.as_raw();
    #[cfg(target_os = "macos")]
    {
        let RawWindowHandle::AppKit(handle) = raw else {
            return Err(wrong_handle("AppKit", raw));
        };
        crate::macos::overlay::present_panel_without_activation(handle)
    }
    #[cfg(target_os = "windows")]
    {
        let RawWindowHandle::Win32(handle) = raw else {
            return Err(wrong_handle("Win32", raw));
        };
        crate::windows::overlay::present_panel_without_activation(handle)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err(wrong_handle("AppKit or Win32", raw))
    }
}

/// Return the authoritative OS reduce-motion preference.
///
/// Failure is conservative: if the preference cannot be read, motion is
/// treated as reduced rather than surprising a motion-sensitive user.
pub fn reduced_motion_preferred() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::macos::overlay::reduced_motion_preferred()
    }
    #[cfg(target_os = "windows")]
    {
        crate::windows::overlay::reduced_motion_preferred()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        true
    }
}

/// Ask the native accessibility stack to announce `message`.
///
/// This is intentionally best-effort: lack of a screen reader, a missing
/// provider, an off-main-thread macOS call, or an older OS all become a no-op.
/// The message is never logged.
pub fn announce_accessibility(message: &str) {
    if message.is_empty() {
        return;
    }
    #[cfg(target_os = "macos")]
    crate::macos::overlay::announce_accessibility(message);
    #[cfg(target_os = "windows")]
    crate::windows::overlay::announce_accessibility(message);
}

fn wrong_handle(expected: &'static str, actual: RawWindowHandle) -> OverlayWindowError {
    OverlayWindowError::UnsupportedHandle {
        expected,
        actual: handle_kind(actual),
    }
}

fn handle_kind(handle: RawWindowHandle) -> &'static str {
    match handle {
        RawWindowHandle::UiKit(_) => "UiKit",
        RawWindowHandle::AppKit(_) => "AppKit",
        RawWindowHandle::Orbital(_) => "Orbital",
        RawWindowHandle::OhosNdk(_) => "OhosNdk",
        RawWindowHandle::Xlib(_) => "Xlib",
        RawWindowHandle::Xcb(_) => "Xcb",
        RawWindowHandle::Wayland(_) => "Wayland",
        RawWindowHandle::Drm(_) => "Drm",
        RawWindowHandle::Gbm(_) => "Gbm",
        RawWindowHandle::Win32(_) => "Win32",
        RawWindowHandle::WinRt(_) => "WinRt",
        RawWindowHandle::Web(_) => "Web",
        RawWindowHandle::WebCanvas(_) => "WebCanvas",
        RawWindowHandle::WebOffscreenCanvas(_) => "WebOffscreenCanvas",
        RawWindowHandle::AndroidNdk(_) => "AndroidNdk",
        RawWindowHandle::Haiku(_) => "Haiku",
        _ => "unknown",
    }
}
