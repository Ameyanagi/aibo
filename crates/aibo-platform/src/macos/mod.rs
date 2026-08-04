//! macOS backend: objc2 AX, NSPasteboard, CGEvent, TCC (§8, §9, §17).
//!
//! # Shape
//!
//! [`MacosBackend`] is a **channel handle**, not a bag of platform objects.
//! `AXUIElement` is `!Send`/`!Sync` (it is a raw CoreFoundation pointer) and,
//! more importantly, a synchronous AX read against a busy app blocks for
//! *seconds* — §8 rewrote the capture rule specifically because a synchronous
//! design "guaranteed a failure mode where pressing the hotkey freezes the
//! target app and no panel ever appears".
//!
//! So: the AX objects live on one dedicated thread ([`thread::PlatformThread`]),
//! every `async` method sends a closure and awaits the reply under a hard
//! deadline, and the *synchronous* trait methods are exactly those that touch
//! only window-server-local state and therefore cannot block on another
//! process.
//!
//! # Deadlines
//!
//! §8 fixes two budgets: **120 ms** for an AX read and **250 ms** including the
//! clipboard fallback. Both are supplied by the caller as the `timeout`
//! argument; [`AX_DEADLINE`] and [`CAPTURE_DEADLINE`] are those numbers, and
//! `selected_text` uses the value it is handed to decide whether the
//! synthetic-`⌘C` fallback is allowed at all.
//!
//! # What is still unvalidated
//!
//! Every `// SPIKE: Sx` marker in this module is a place where the plan says
//! "measure this before believing it":
//!
//! * **S2** — the app matrix for `text_field_context` (Safari, Chrome, VS Code,
//!   Slack, Word, Notion, Terminal) and the Chromium/Electron bundle-id lists.
//! * **S4** — insert reliability: `CGEventPost` has no delivery confirmation,
//!   the paste settle interval is a guess, and clipboard save/restore
//!   round-tripping is unproven.
//! * **S7** — IME composition detection. §9 states outright that macOS has no
//!   clean cross-process API; what is here is a probe for two non-standard AX
//!   attributes and nothing more.

mod apps;
mod ax;
mod cf;
mod clipboard;
mod display;
mod error;
mod keys;
pub(crate) mod overlay;
mod permissions;
mod thread;
mod worker;

use std::time::Duration;

use aibo_core::error::Result;
use aibo_core::traits::PlatformBackend;
use aibo_core::types::{
    AppInfo, AppRef, BoxStream, ClipboardItem, DisplayInfo, DocumentBudget, DocumentText,
    FieldContext, InsertMode, InsertTarget, Permission, PermissionStatus, PowerEvent,
};
use async_trait::async_trait;

pub use apps::{AxActivation, ax_activation_for, is_clipboard_denylisted, is_code_app};
pub use clipboard::{
    NS_PASTEBOARD_AUTO_GENERATED_TYPE, NS_PASTEBOARD_CONCEALED_TYPE, NS_PASTEBOARD_TRANSIENT_TYPE,
    RestoreOutcome,
};
pub use error::MacosError;
pub use permissions::{
    ACCESSIBILITY_SETTINGS_URL, is_trusted, open_accessibility_settings, secure_input_active,
};
pub use worker::{MacosConfig, content_hash};

/// §8: the hard deadline for a pure AX read.
pub const AX_DEADLINE: Duration = Duration::from_millis(120);

/// The synchronous focus snapshot used by the UI before it presents aibo.
pub(crate) fn frontmost_app_ref() -> Result<AppRef> {
    worker::Worker::focused_app_ref()
        .map_err(|error| error.into_capture_error(&MacosBackend::frontmost_identifier()))
}

/// §8: the hard deadline for a capture that is allowed to fall back to the
/// clipboard.
pub const CAPTURE_DEADLINE: Duration = Duration::from_millis(250);

/// The macOS implementation of [`PlatformBackend`].
///
/// Share it behind an `Arc` rather than deriving `Clone`: one process wants
/// exactly one platform thread, and a `Clone` that silently spawned a second
/// one would be a trap.
pub struct MacosBackend {
    thread: thread::PlatformThread,
}

impl MacosBackend {
    /// Start the backend and its dedicated AX thread.
    pub fn new(config: MacosConfig) -> Self {
        Self {
            thread: thread::PlatformThread::spawn(config),
        }
    }

    /// The bundle identifier of the frontmost app, for error messages.
    ///
    /// Only correct for the paths that genuinely act on "whatever is frontmost
    /// right now" — the step-1 snapshot and the permission prompt. Every
    /// deferred capture must label its errors with [`Self::identifier_of`]
    /// instead: `CaptureFailed { app }` naming aibo is worse than useless, it
    /// tells the user the wrong app is broken.
    fn frontmost_identifier() -> String {
        apps::frontmost_application()
            .map(|(_, identifier, _)| identifier)
            .unwrap_or_else(|| "unknown".to_owned())
    }

    /// The bundle identifier of the app a deferred capture was aimed at.
    fn identifier_of(app_ref: &AppRef) -> String {
        apps::application_identity(app_ref.pid)
            .map(|(identifier, _)| identifier)
            .unwrap_or_else(|| "unknown".to_owned())
    }
}

impl Default for MacosBackend {
    fn default() -> Self {
        Self::new(MacosConfig::default())
    }
}

#[async_trait]
impl PlatformBackend for MacosBackend {
    // -- instant snapshot ----------------------------------------------------
    //
    // These three run inline on the caller's thread. That is not a shortcut:
    // `NSWorkspace`, `CGWindowListCopyWindowInfo`, `CGDisplay` and
    // `AXIsProcessTrusted` all answer from state the window server or TCC keeps
    // locally, so none of them can block on a hung target app — which is the
    // whole reason §8 step 1 is allowed to be synchronous. Routing them through
    // the channel would only add latency to the one path that must be instant.
    //
    // `active_display` additionally *wants* to be on the UI thread: `NSScreen`
    // is main-thread-only in objc2 0.3, and it is the only source of
    // `visibleFrame` (bounds minus menu bar and Dock) that §9 clamps against.

    fn focused_app_ref(&self) -> Result<AppRef> {
        frontmost_app_ref()
    }

    fn active_display(&self) -> Result<DisplayInfo> {
        let window = apps::frontmost_application()
            .and_then(|(pid, _, _)| display::frontmost_window_for_pid(pid));
        Ok(display::active_display(window))
    }

    fn secure_input_active(&self) -> bool {
        permissions::secure_input_active()
    }

    // -- deadline-bounded capture -------------------------------------------
    //
    // §7: every one of these takes the `AppRef` snapshotted on hotkey-down and
    // reads from THAT application. They run after the panel is visible, so
    // "frontmost" here means aibo — see the trait's own note for the full list
    // of things that silently break when these re-resolve it.

    async fn focused_app(&self, of: &AppRef, timeout: Duration) -> Result<AppInfo> {
        let of = of.clone();
        let label = of.clone();
        self.thread
            .call(timeout, move |w| w.focused_app(&of))
            .await
            .map_err(|e| e.into_capture_error(&Self::identifier_of(&label)))
    }

    /// Read the selection inside `of`.
    ///
    /// The synthetic-`⌘C` fallback is enabled only when `timeout` exceeds
    /// [`AX_DEADLINE`] — passing 120 ms means "AX only, do not touch the user's
    /// clipboard", passing 250 ms means "the fallback is in budget" (§8) — and
    /// the worker additionally declines it whenever `of` is no longer frontmost,
    /// because a synthetic chord cannot be aimed at a background app.
    ///
    /// A deadline expiry returns `Ok(None)`, not an error: §8 step 4 requires
    /// every view to tolerate context arriving late, empty, or not at all, and
    /// "the app swallowed the shortcut" is an ordinary outcome.
    async fn selected_text(&self, of: &AppRef, timeout: Duration) -> Result<Option<String>> {
        let allow_fallback = timeout > AX_DEADLINE;
        // The worker gets slightly less than the caller so its own clipboard
        // poll ends before the outer timeout abandons the reply.
        let inner = timeout.mul_f32(0.9);
        let of = of.clone();
        let label = of.clone();
        let capture = if allow_fallback {
            self.thread
                .call_side_effect(timeout, move |w| {
                    w.selected_text(&of, inner, allow_fallback)
                })
                .await
        } else {
            self.thread
                .call(timeout, move |w| {
                    w.selected_text(&of, inner, allow_fallback)
                })
                .await
        };
        match capture {
            Ok(text) => Ok(text),
            Err(MacosError::Deadline(_)) => Ok(None),
            Err(err) => Err(err.into_capture_error(&Self::identifier_of(&label))),
        }
    }

    async fn read_document(
        &self,
        of: &AppRef,
        budget: DocumentBudget,
        timeout: Duration,
    ) -> Result<Option<DocumentText>> {
        let of = of.clone();
        let label = of.clone();
        match self
            .thread
            .call(timeout, move |w| w.read_document(&of, budget))
            .await
        {
            Ok(document) => Ok(document),
            // A document read that ran out of time is not a failure: the caller
            // asked for as much as could be had inside the budget, and "none"
            // is a truthful answer to that.
            Err(MacosError::Deadline(_)) => Ok(None),
            Err(err) => Err(err.into_capture_error(&Self::identifier_of(&label))),
        }
    }

    async fn focused_element_id(&self, of: &AppRef, timeout: Duration) -> Result<Option<String>> {
        let of = of.clone();
        let label = of.clone();
        match self
            .thread
            .call(timeout, move |w| w.focused_element_id(&of))
            .await
        {
            Ok(id) => Ok(id),
            // A deadline here weakens validation rather than breaking it.
            Err(MacosError::Deadline(_)) => Ok(None),
            Err(err) => Err(err.into_capture_error(&Self::identifier_of(&label))),
        }
    }

    /// Read a bounded window of the focused field.
    ///
    /// A secure field or an active composition yields a [`FieldContext`] whose
    /// `prefix`/`suffix` are empty and whose `is_secure`/`ime_active` flag is
    /// set, so the panel can say *why* it has nothing rather than showing an
    /// unexplained blank (§5, §9). `Ok(None)` means there is no focused text
    /// field at all, or the app exposes no AX tree.
    async fn text_field_context(
        &self,
        of: &AppRef,
        timeout: Duration,
    ) -> Result<Option<FieldContext>> {
        let of = of.clone();
        let label = of.clone();
        match self
            .thread
            .call(timeout, move |w| w.text_field_context(&of))
            .await
        {
            Ok(context) => Ok(context),
            Err(MacosError::Deadline(_)) => Ok(None),
            Err(err) => Err(err.into_capture_error(&Self::identifier_of(&label))),
        }
    }

    async fn clipboard(&self, owner_hint: &AppRef, timeout: Duration) -> Result<ClipboardItem> {
        let owner_hint = owner_hint.clone();
        let label = owner_hint.clone();
        self.thread
            .call(timeout, move |w| w.clipboard(&owner_hint))
            .await
            .map_err(|e| e.into_capture_error(&Self::identifier_of(&label)))
    }

    // -- write-back ----------------------------------------------------------

    async fn insert_text(&self, text: &str, mode: InsertMode) -> Result<()> {
        let text = text.to_owned();
        let budget = insert_budget(text.len());
        self.thread
            .call_side_effect(budget, move |w| w.insert_text(&text, mode))
            .await
            .map_err(MacosError::into_insert_error)
    }

    async fn replace_selection(&self, text: &str) -> Result<()> {
        let text = text.to_owned();
        let budget = insert_budget(text.len());
        self.thread
            .call_side_effect(budget, move |w| w.replace_selection(&text))
            .await
            .map_err(MacosError::into_insert_error)
    }

    async fn validate_target(&self, target: &InsertTarget) -> Result<bool> {
        let target = target.clone();
        self.thread
            .call(AX_DEADLINE, move |w| w.validate_target(&target))
            .await
            .map_err(MacosError::into_insert_error)
    }

    async fn restore_focus(&self, prev: &AppRef, timeout: Duration) -> Result<()> {
        let prev = prev.clone();
        // The worker's own confirm-and-retry loop must end before the outer
        // timeout abandons the reply, or a successful late restore would be
        // reported as a failure and the caller would refuse to paste.
        let inner = timeout.mul_f32(0.9);
        self.thread
            .call_side_effect(timeout, move |w| w.restore_focus(&prev, inner))
            .await
            .map_err(MacosError::into_insert_error)
    }

    // -- permissions and power ----------------------------------------------

    fn permission_status(&self, p: Permission) -> PermissionStatus {
        permissions::status(p)
    }

    fn request_permission(&self, p: Permission) -> Result<()> {
        permissions::request(p).map_err(|e| e.into_capture_error(&Self::frontmost_identifier()))
    }

    /// Sleep/wake/display notifications.
    ///
    /// **Not yet wired.** The real implementation observes
    /// `NSWorkspaceWillSleepNotification` / `NSWorkspaceDidWakeNotification` on
    /// `NSWorkspace.notificationCenter` and registers a
    /// `CGDisplayRegisterReconfigurationCallback`. Both need an Objective-C
    /// observer class (`objc2::define_class!`) and a live run loop on the main
    /// thread — i.e. they belong with the tray/window bootstrap, not with the
    /// AX thread, and that bootstrap does not exist yet.
    ///
    /// Returning an immediately-empty stream rather than a never-firing one is
    /// deliberate: §13's "first hotkey of the day" recovery must be able to
    /// tell "no events yet" from "this source will never produce events".
    fn power_events(&self) -> Result<BoxStream<'static, PowerEvent>> {
        tracing::warn!(
            target: "aibo::platform::macos",
            "power_events is not wired yet; sleep/wake connection re-warming (§13) is inactive"
        );
        Ok(Box::pin(futures::stream::empty()))
    }
}

/// Budget for one insert.
///
/// An insert is not a capture: there is no 120 ms guarantee to keep, and the
/// path includes a synthetic chord plus a settle interval. The size term covers
/// the pasteboard write for large payloads (§13 caps a selection at a few
/// thousand characters, so this stays well under a second in practice).
fn insert_budget(len: usize) -> Duration {
    Duration::from_millis(400) + Duration::from_micros(len as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadlines_match_the_plan() {
        assert_eq!(AX_DEADLINE, Duration::from_millis(120));
        assert_eq!(CAPTURE_DEADLINE, Duration::from_millis(250));
        // The fallback rule the trait impl relies on.
        assert!(CAPTURE_DEADLINE > AX_DEADLINE);
    }

    #[test]
    fn insert_budget_grows_with_payload() {
        assert!(insert_budget(5000) > insert_budget(0));
    }
}
