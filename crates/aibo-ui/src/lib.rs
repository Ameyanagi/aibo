//! `aibo-ui` — the iced 0.14 daemon and every view (§6, §16).
//!
//! Three windows, not one (§6): a transient [`panel`], a [`task_window`] that
//! outlives it for agent runs, and [`settings`]. The panel is pre-created hidden
//! at startup so a hotkey press only has to position, show and focus.
//!
//! The tray cannot be created in `boot` — `tray-icon` needs the event loop
//! already running, and on macOS the tray must be created on the main thread.
//! Create it from the first `update` tick instead (§6).
//!
//! Strings are externalised from day one via [`i18n`]; the panel width grows
//! within bounds rather than being fixed at 680 pt (§9).
//!
//! # Layout
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`app`] | the `daemon`: state, `boot`/`update`/`view`, subscriptions, entry point |
//! | [`bridge`] | the UI ↔ tokio message vocabulary (§6) |
//! | [`hotkey`] | per-platform defaults and registration failure (§8, §9) |
//! | [`tray`] | the tray icon, built from the first update tick (§6) |
//! | [`placement`] | caret anchoring, display selection and clamping (§9) |
//! | [`theme`] | the palette, scale, type ramp and motion constants (§16) |
//! | [`widgets`] | the §16 component inventory |
//! | [`panel`], [`task_window`], [`settings`] | the three surfaces |
//! | [`i18n`] | the string catalogue (§9) |
//! | [`error`] | shell-level failures, distinct from `AiboError` (§13) |
//!
//! # Spikes this crate depends on
//!
//! Per §20 these are unverified and the code marks them `// SPIKE: Sx` rather
//! than assuming an outcome:
//!
//! * **S1** — native overlay configuration now crosses an audited
//!   `raw-window-handle` boundary in `aibo-platform`; the remaining unverified
//!   part is whether AX/UIA caret bounds stay correct across mixed-DPI
//!   multi-display layouts.
//! * **S3** — the ≤ 80 ms hotkey-to-visible budget the cold-start trick exists
//!   to meet (§15). The mechanism is sound; the number is not yet measured.
//! * **S7** — IME composition detection, which has no clean cross-process API
//!   on macOS (§9).
//! * **S10** — typing Japanese *into* aibo's own panel: winit/iced `Ime`
//!   events, `set_ime_allowed` and `set_ime_cursor_area`. On the critical path.

#![warn(missing_docs)]

mod a11y;
pub mod app;
pub mod bridge;
pub mod error;
pub mod history_ring;
pub mod hotkey;
pub mod i18n;
pub mod model_picker;
pub mod panel;
pub mod placement;
pub mod settings;
pub mod task_window;
pub mod theme;
pub mod tray;
pub mod widgets;

pub use app::{Message, UiConfig, UiHandles, run};
pub use bridge::{ModelOption, SessionId, UiEvent, UiRequest};
pub use error::{Result, UiError};
pub use i18n::Lang;
pub use theme::Appearance;
