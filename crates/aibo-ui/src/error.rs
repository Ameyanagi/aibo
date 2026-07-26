//! Failures that belong to the shell itself rather than to a request.
//!
//! [`aibo_core::AiboError`] covers everything the *product* can fail at and has
//! a fixed user-facing treatment per §13. This enum covers the far smaller set
//! of things that can go wrong while wiring the shell up: the hotkey the OS
//! refused, the tray the platform would not give us, the event loop that died.
//! They are reported once, at startup, and never inside a panel response.

use thiserror::Error;

/// A shell-level failure (§6, §8).
#[derive(Debug, Error)]
pub enum UiError {
    /// The OS refused the global hotkey. Almost always a conflict with another
    /// launcher (Raycast, Alfred, Spotlight, PowerToys Run) — §9 requires first-
    /// run conflict detection rather than a silent no-op.
    #[error("could not register the global hotkey `{combo}`: {reason}")]
    HotkeyRegistration {
        /// Human-readable combination, e.g. `⌥Space`.
        combo: String,
        /// Platform reason, already redacted for display.
        reason: String,
    },

    /// The configured combination could not be parsed into a `HotKey`.
    ///
    /// §9: key codes are keyboard-layout dependent, so a combination stored on
    /// a JIS layout may not resolve on US-QWERTY.
    #[error("`{0}` is not a valid hotkey combination")]
    HotkeyParse(String),

    /// The tray icon could not be created.
    ///
    /// §6: `tray-icon` needs the event loop *already running* and, on macOS,
    /// the main thread. Creating it in `boot` fails here every time.
    #[error("could not create the tray icon: {0}")]
    Tray(String),

    /// The iced runtime failed to start or exited abnormally.
    #[error("the UI runtime failed: {0}")]
    Runtime(String),

    /// [`crate::app::run`] was called more than once in a process. The daemon
    /// owns process-global state (hotkey manager, tray, event channels) and is
    /// single-instance by construction (§6).
    #[error("the aibo UI is already running in this process")]
    AlreadyRunning,
}

/// Shell-level result alias.
pub type Result<T, E = UiError> = std::result::Result<T, E>;
