//! `aibo-platform` — the OS integration layer, and the only crate that is
//! allowed to contain `#[cfg(target_os)]` (§7).
//!
//! Implementations of [`PlatformBackend`] are **channel handles to a dedicated
//! platform thread**, never the platform objects themselves: `uiautomation`'s
//! types are `!Send` and `!Sync` (apartment-threaded COM) and macOS AX blocks
//! for seconds against a busy app. The per-call timeouts from §8 belong on that
//! thread's request loop.
//!
//! [`PlatformBackend`]: aibo_core::traits::PlatformBackend

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

mod accessibility;
mod overlay;
mod screen_capture;

pub use accessibility::{
    AccessibilityError, AccessibilityEvent, AccessibilitySurface, attach_accessibility,
    detach_accessibility, set_accessibility_focus, update_accessibility,
};
pub use overlay::{
    BackdropStatus, OverlayWindowConfiguration, OverlayWindowError, activate_self,
    announce_accessibility, configure_panel_window, present_panel_without_activation,
    reduced_motion_preferred, set_panel_backdrop_height,
};
pub use screen_capture::{ScreenCaptureError, capture_screen_region};

/// The proxy the OS says to use for HTTPS, or `None` for a direct connection.
///
/// §13: a managed network is a supported environment, not an edge case. reqwest
/// reads `HTTPS_PROXY` and nothing else, and Windows does not set it — a machine
/// configured through Internet Settings or Group Policy looked to aibo like a
/// machine with no route, which was reported as "offline" on a machine that was
/// online.
///
/// macOS returns `None` on purpose. Its proxy configuration lives in
/// `SCDynamicStore` and, unlike Windows, an inspecting proxy there installs its
/// root into the login keychain — which the native certificate roots already
/// handle. Reading `SCDynamicStore` is worth doing when there is a report that
/// needs it, and not before.
pub fn system_proxy() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        windows::proxy::system_proxy()
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}
