//! The three traits every backend implements (§7).
//!
//! They live in `aibo-core` rather than beside their implementations so that
//! `aibo-core` — which has no platform and no network dependencies — remains
//! the single place the contract is defined, and so the router, prompt
//! assembly and the UI can be tested against fakes.

use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::types::{
    AgentFeatures, AgentLimits, AgentStep, AgentTask, AppInfo, AppRef, BoxStream, Capabilities,
    ChatRequest, ClipboardItem, DisplayInfo, FieldContext, Health, InsertMode, InsertTarget,
    ModelInfo, Permission, PermissionStatus, PowerEvent, ProviderId, StreamEvent,
};

/// A model backend. One implementation per provider — no
/// lowest-common-denominator layer (§7, §10).
#[async_trait]
pub trait Provider: Send + Sync {
    /// Which provider this is.
    fn id(&self) -> ProviderId;

    /// The provider's **default** capabilities.
    ///
    /// §10: capabilities are per-model, not per-provider. The authoritative
    /// values live on [`ModelInfo::capabilities`]; this is only the fallback
    /// used before the catalogue has been fetched.
    ///
    /// [`ModelInfo::capabilities`]: crate::types::ModelInfo::capabilities
    fn capabilities(&self) -> Capabilities;

    /// Start a streaming completion.
    ///
    /// The token is not optional and not an afterthought: `esc` must abort
    /// in-flight work immediately, and a new submission cancels the previous
    /// one (§13). Dropping the returned stream must also stop the request.
    ///
    /// Implementations must not auto-retry a 4xx — a 400 is a bug in aibo and
    /// has to surface as one rather than silently falling back (§4).
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>>;

    /// The provider's live model catalogue. Catalogues rot; this is the runtime
    /// fallback when a shipped manifest entry has been retired (§10).
    async fn models(&self) -> Result<Vec<ModelInfo>>;

    /// Probe reachability. Feeds the per-provider hysteresis in §13 — callers,
    /// not implementations, decide when a provider is considered degraded.
    async fn health(&self) -> Result<Health>;
}

/// The OS integration surface. The only trait with platform-specific
/// implementations (§7).
///
/// **Implementations are a channel handle to a dedicated platform thread, not
/// the platform objects themselves.** `uiautomation`'s types are explicitly
/// `!Send` and `!Sync` because UI Automation is apartment-threaded COM, and
/// macOS AX has the same blocking property. The real UIA/AX objects live on one
/// dedicated thread (MTA on Windows); every method here sends a request over a
/// channel and awaits a reply. That is also where the per-call timeouts belong.
/// Calling these APIs from the UI event loop invites deadlock — a synchronous
/// `AXUIElementCopyAttributeValue` against a busy app blocks for *seconds* (§6,
/// §8). This is a shape requirement, not a suggestion.
///
/// Capture ordering (§8):
/// 1. On hotkey-down take only [`PlatformBackend::focused_app_ref`] and
///    [`PlatformBackend::active_display`] — instant and unchangeable.
/// 2. Show the panel immediately.
/// 3. Run the `async` capture methods with a hard deadline: **120 ms** for
///    AX/UIA, **250 ms** including the clipboard fallback. **Every one of them
///    takes the [`AppRef`] from step 1** — see below.
/// 4. Context arrives late, empty, or not at all — every view must tolerate all
///    three.
///
/// # Deferred capture reads the app from step 1, never "the frontmost app"
///
/// §7 spells this out because getting it wrong is silent and total:
///
/// > every one of these takes the `AppRef` captured in step 1 and reads from
/// > THAT application. An earlier version of this plan omitted the parameter,
/// > and the resulting implementations all re-resolved "frontmost" at call time
/// > — by which point the panel has taken focus, so the frontmost app is aibo.
///
/// The failure is not theoretical, and it is not partial. By step 3 the panel
/// is up and holds focus, so a `frontmost`-based implementation reports
/// [`AppInfo::identifier`] as aibo's own bundle id (breaking `is_code_app`
/// routing, §5's source-app prompt line and `Exchange::source_app`), resolves
/// `AXFocusedUIElement` to aibo's own panel text field (so [`FieldContext`]'s
/// `prefix` is the user's typed query rather than their document), and makes
/// §12's clipboard app-denylist unmatchable because the attributed owner is
/// always aibo. The parameter is what makes deferred capture *correct* rather
/// than merely *late*.
///
/// [`AppInfo::identifier`]: crate::types::AppInfo::identifier
#[async_trait]
pub trait PlatformBackend: Send + Sync {
    // -- instant snapshot ----------------------------------------------------

    /// The focused app and window. Cheap, cannot fail slowly, taken on
    /// hotkey-down before the panel is shown (§8 step 1).
    fn focused_app_ref(&self) -> Result<AppRef>;

    /// The display to place the panel on (§9). Also taken on hotkey-down.
    fn active_display(&self) -> Result<DisplayInfo>;

    /// macOS `IsSecureEventInputEnabled()`, or the Windows equivalent.
    ///
    /// When true, keystroke synthesis and AX reads fail *silently*, and other
    /// apps can leave the flag stuck globally — so it must be checked and
    /// explained rather than discovered (§8).
    fn secure_input_active(&self) -> bool;

    // -- deadline-bounded capture -------------------------------------------
    //
    // `of` is the step-1 snapshot. It is the *subject* of the read, not a hint:
    // an implementation that ignores it and asks the window server which app is
    // frontmost is reading aibo's own panel. See the trait-level note.

    /// Resolve the identity of the application snapshotted in step 1.
    ///
    /// The returned [`AppInfo::app_ref`] describes `of`, never whatever happens
    /// to be frontmost when the call lands.
    ///
    /// [`AppInfo::app_ref`]: crate::types::AppInfo::app_ref
    async fn focused_app(&self, of: &AppRef, timeout: Duration) -> Result<AppInfo>;

    /// Read the selection **inside `of`**. Falls back to a synthetic copy
    /// chord, which mutates the clipboard and must save/restore under the §12
    /// race rules — and which is only legal while `of` is still frontmost,
    /// since a synthetic chord goes to whichever app has focus.
    async fn selected_text(&self, of: &AppRef, timeout: Duration) -> Result<Option<String>>;

    /// Read a bounded window of the text field focused **inside `of`**.
    ///
    /// Must return `Ok(None)` — never captured text — when the field is secure
    /// or an IME composition is active (§5, §9). Ask for a bounded range around
    /// the caret; never the whole document (§5, §15).
    async fn text_field_context(
        &self,
        of: &AppRef,
        timeout: Duration,
    ) -> Result<Option<FieldContext>>;

    /// Read the clipboard, honouring the concealed/transient markers (§12).
    ///
    /// `owner_hint` is the step-1 snapshot: neither macOS nor Windows reports
    /// who put the current item on the clipboard, so the app that had focus
    /// when the hotkey fired is the best attribution available, and it is the
    /// one §12's app denylist is matched against. Attributing it to the
    /// frontmost app instead makes the denylist inert — by capture time the
    /// frontmost app is aibo, which is on nobody's denylist.
    ///
    /// §7's sketch takes no `timeout` here; it is kept for the same reason
    /// [`PlatformBackend::restore_focus`] is `async` — §8 requires *every*
    /// deferred capture to be deadline-bounded, and this one is queued behind
    /// the same platform thread as the rest.
    async fn clipboard(&self, owner_hint: &AppRef, timeout: Duration) -> Result<ClipboardItem>;

    // -- write-back ----------------------------------------------------------
    //
    // §8's insert sequence is ordered, and the order is load-bearing:
    //
    //   1. hide the panel
    //   2. `restore_focus(target)` — and CONFIRM it landed, with a bounded retry
    //   3. `validate_target(target)`
    //   4. one atomic paste
    //
    // **Validate comes after restore.** Validation's first check is
    // `frontmost pid == target pid`; run it while aibo still holds focus and it
    // compares aibo's pid against the target's and fails every single time, so
    // every insert returns "target changed, copy instead" and the feature can
    // never work. The confirm-and-retry loop inside `restore_focus` is itself
    // proof that aibo is expected to hold focus at that point.

    /// Insert text at the caret.
    ///
    /// Atomic from the user's perspective: build the full string, then one
    /// paste. Never chunked, never incremental — that invariant is what makes
    /// undo, cancellation and partial-failure tractable (§13).
    async fn insert_text(&self, text: &str, mode: InsertMode) -> Result<()>;

    /// Replace the current selection.
    async fn replace_selection(&self, text: &str) -> Result<()>;

    /// Confirm that everything captured is still true, immediately before an
    /// insert (§8) — and **after** [`PlatformBackend::restore_focus`] has
    /// confirmed the target is frontmost again.
    ///
    /// Returns `false` when the pid, window, focused element or content hash
    /// has changed. A `false` *after a confirmed restore* is a real target
    /// change — the user switched apps, closed a tab, or edited the text — so
    /// callers must then offer "copy instead" rather than insert; pasting over
    /// the wrong content is unrecoverable. A `false` from calling this while
    /// aibo still holds focus is not information, it is a bug in the caller.
    async fn validate_target(&self, target: &InsertTarget) -> Result<bool>;

    /// Give focus back to the app that had it, and **confirm** it landed.
    ///
    /// §7 sketches this as synchronous; §8 requires a bounded confirm-and-retry
    /// before pasting, which cannot be synchronous on a channel-backed handle.
    /// An unconfirmed restore races and pastes into the wrong window.
    ///
    /// This runs *before* [`PlatformBackend::validate_target`], not after.
    async fn restore_focus(&self, prev: &AppRef, timeout: Duration) -> Result<()>;

    // -- permissions and power ----------------------------------------------

    /// Current status of an OS permission.
    fn permission_status(&self, p: Permission) -> PermissionStatus;

    /// Trigger the OS prompt, or open the relevant settings pane when the
    /// permission can no longer be requested programmatically.
    fn request_permission(&self, p: Permission) -> Result<()>;

    /// Sleep/wake/display notifications.
    ///
    /// §13: after a lid-close the pooled HTTPS connections are dead, so the
    /// first hotkey of the day misses the latency budget unless the pool is
    /// re-warmed and provider health re-probed on wake.
    fn power_events(&self) -> Result<BoxStream<'static, PowerEvent>>;
}

/// An agentic execution backend (§7).
///
/// Implementations: `CodexAppServer`, `NativeLoop`, `ClaudeCodeCli`.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Start a run and stream its steps.
    ///
    /// `limits` is mandatory, not advisory: exceeding one stops the run with
    /// [`AiboError::BudgetExceeded`] and a "continue anyway" affordance. The
    /// delegate's own limits apply too, but aibo must not depend on them (§14).
    ///
    /// [`AiboError::BudgetExceeded`]: crate::error::AiboError::BudgetExceeded
    async fn run(
        &self,
        task: AgentTask,
        limits: AgentLimits,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<AgentStep>>>;

    /// What this backend can do, so the UI can hide affordances it lacks.
    fn supports(&self) -> AgentFeatures;
}
