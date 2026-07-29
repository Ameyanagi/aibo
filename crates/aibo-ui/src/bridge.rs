//! The UI ↔ tokio bridge (§6).
//!
//! §6 fixes the shape: "Bridge tokio → UI with `iced::Subscription::run` over an
//! mpsc receiver." Both directions are bounded: provider output applies async
//! backpressure all the way to the session driver, while the UI uses
//! non-blocking delivery for human input and reserves part of its fixed queue
//! for cancellation and lifecycle signals. This module is the *vocabulary* that
//! crosses that boundary, and it is deliberately the only coupling between
//! `aibo-ui` and the rest of the workspace beyond `aibo-core`'s domain types.
//!
//! Two properties are load-bearing:
//!
//! * **Nothing here blocks.** Every variant is a plain data message. The UI
//!   thread never awaits a provider, an AX read or a SQLite write; §6 is
//!   explicit that UI Automation and macOS AX must not run on the event loop.
//! * **Errors cross as `Arc`.** [`aibo_core::AiboError`] is not `Clone` (it can
//!   carry a boxed source), and iced messages want to be. Arc-wrapping keeps the
//!   original error intact for logging instead of flattening it to a string at
//!   the boundary.

use std::sync::Arc;

use aibo_core::AiboError;
use aibo_core::context::Turn;
use aibo_core::types::{
    AgentStep, AppInfo, Attachment, ClipboardItem, DisplayInfo, FieldContext, Health, ModelBinding,
    Permission, PermissionStatus, ProviderId, Role, StreamEvent, Surface, Usage,
};
use secrecy::SecretString;
use uuid::Uuid;

use crate::i18n::Lang;

/// Human-scale UI requests allowed to wait for the runtime.
pub const UI_REQUEST_CHANNEL_CAPACITY: usize = 64;

/// Runtime events allowed to wait for iced's subscription.
///
/// Stream and agent pumps await capacity, so this is a hard memory boundary,
/// not a threshold after which model content is dropped.
pub const UI_EVENT_CHANNEL_CAPACITY: usize = 128;

/// Correlates a panel invocation with the work it started.
///
/// §13: "one panel, one session". Pressing the hotkey while a Complete is
/// streaming cancels the in-flight request and discards the old session — so
/// every event carries the session it belongs to and the UI drops anything for
/// a session it has moved on from. Without this, a slow response from a
/// cancelled request overwrites a fresh one.
pub type SessionId = Uuid;

/// One model the runtime permits the panel to select.
///
/// The backend, not the renderer, owns this list. That keeps provider-specific
/// allowlists and configuration validation on the trusted side of the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    /// Concrete provider/model pair written to configuration.
    pub binding: ModelBinding,
    /// Human-readable model name.
    pub display_name: String,
    /// Measured first-token latency, when the shipped catalogue has one.
    pub latency_ms: Option<u32>,
}

impl std::fmt::Display for ModelOption {
    /// `provider · model · latency`.
    ///
    /// **The provider is named, and it has to be.** The picker used to show the
    /// model alone, which was unambiguous only while Codex was the sole
    /// provider. With several configured, "gpt-5" could be OpenAI directly or
    /// OpenRouter fronting it — at different prices, context windows and trust
    /// boundaries (§14) — and the row gave no way to tell. Worse, the label
    /// could name a Codex model while the request had actually been routed
    /// elsewhere by role, which is how a vision request appeared to come from
    /// "GPT-5.6 Sol".
    ///
    /// Latency stays last because only the shipped Codex entries have a measured
    /// figure; no `/models` endpoint returns one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} · {}",
            self.binding.provider.as_str(),
            self.display_name
        )?;
        if let Some(latency) = self.latency_ms {
            write!(f, " · {latency} ms")?;
        }
        Ok(())
    }
}

/// Something the UI asks the runtime to do.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum UiRequest {
    /// The tray and pre-created panel window are ready.
    ///
    /// Provider prewarming waits for this signal so startup work cannot delay
    /// the shell becoming visible.
    UiReady,

    /// Create the encrypted history key and database after an explicit setup
    /// gesture.
    InitializeHistory,

    /// The hotkey fired. Capture context for the frontmost app.
    ///
    /// §8 splits capture in two: the cheap synchronous part (app ref, displays)
    /// and everything else behind a deadline. The UI shows the panel without
    /// waiting for either.
    CaptureContext {
        /// The session this capture belongs to.
        session: SessionId,
    },

    /// Run the user's instruction.
    Submit {
        /// Session.
        session: SessionId,
        /// Exactly what the user typed, untouched. §5: this is the only
        /// content permitted to authorise a tool call.
        instruction: String,
        /// The surface the panel resolved, frozen for the session (§1).
        surface: Surface,
        /// `@model` / `⌘1..4` override; wins over every routing rule (§4).
        role_override: Option<Role>,
        /// Images the user deliberately attached to this turn.
        ///
        /// Ambient clipboard state never populates this field. A crop shortcut
        /// or explicit attach gesture does, and prompt assembly keeps the pixels
        /// fenced as untrusted context.
        attachments: Vec<Attachment>,
        /// Completed chat turns, oldest first.
        ///
        /// The selected text remains separate captured context; history is
        /// only what the user and assistant said inside this panel session.
        history: Vec<Turn>,
        /// Whether the pinned captured selection should be included.
        ///
        /// Removing the context card changes this to `false` without mutating
        /// the backend's captured insertion target.
        include_selection: bool,
    },

    /// Cancel in-flight work for a session (`esc`, or a new submission).
    Cancel {
        /// Session.
        session: SessionId,
    },

    /// Forget all backend state for a panel session.
    ///
    /// Cancellation stops active generation; discard additionally retires the
    /// captured insertion target and rejects capture results that arrive after
    /// the panel has moved on.
    DiscardSession {
        /// Session whose capture, replay and insertion state is no longer live.
        session: SessionId,
    },

    /// Insert the accepted text into the source app.
    ///
    /// §13 invariant: aibo never streams into a third-party app. This is sent
    /// once, on accept, with the complete string — never per chunk.
    Insert {
        /// Session, so the runtime can re-validate the insert target (§8).
        session: SessionId,
        /// The full text.
        text: String,
    },

    /// Put text on the clipboard.
    Copy {
        /// The text.
        text: String,
    },

    /// Re-run a session against a different role (the "Retry with Smart"
    /// inline action from §13).
    Retry {
        /// Session.
        session: SessionId,
        /// Role to use, or `None` to repeat the original routing.
        role: Option<Role>,
    },

    /// Start the re-authentication flow for a provider.
    SignIn {
        /// Provider whose credential was rejected.
        provider: ProviderId,
    },

    /// Add or replace an API-key provider from the settings window (§10, §12).
    ///
    /// The key travels as a [`SecretString`] and is consumed by the credential
    /// store on arrival. §12 keeps it out of `config.toml`, out of the UI's
    /// retained state once sent, and out of diagnostics — `SecretString`'s own
    /// `Debug` redacts, so the enum's derive cannot leak it into a log line.
    SetProviderKey {
        /// Which backend, spelled as `config.toml`'s `backend = "…"` value.
        backend: String,
        /// Explicit id, for a second endpoint of a backend already configured.
        id: Option<String>,
        /// Base URL. Required for a custom endpoint, ignored for the rest.
        base_url: Option<String>,
        /// The key. Empty means "the user cleared the field", which is a
        /// removal rather than an empty credential.
        key: SecretString,
    },

    /// Forget a provider: drop its credential and its `config.toml` entry.
    RemoveProvider {
        /// The id the provider is addressed by.
        id: String,
    },

    /// Open the OS privacy pane for a permission (§17).
    OpenSystemSettings {
        /// Which permission.
        permission: Permission,
    },

    /// Open a URL in the user's browser.
    ///
    /// Exists for the device-code sign-in screen (§3a): the verification page
    /// is the one place aibo sends the user out of the app, and asking them to
    /// transcribe `auth.openai.com/codex/device` alongside a ten-character code
    /// is two chances to mistype instead of one.
    OpenUrl {
        /// The URL. Only ever a constant from the provider crate — never
        /// anything derived from a model response or captured content (§5).
        url: String,
    },

    /// Answer a blocking approval request (§11).
    Approve {
        /// Task the approval belongs to.
        task: Uuid,
        /// Backend-assigned approval id.
        approval: String,
        /// The user's answer.
        decision: aibo_core::types::ApprovalDecision,
        /// Exact destructive-command confirmation, when the prompt required it.
        typed_confirmation: Option<String>,
    },

    /// Cancel an agent run.
    CancelTask {
        /// Task id.
        task: Uuid,
    },

    /// Copy a redacted diagnostics bundle (§13 `Internal`, §19).
    CopyDiagnostics,

    /// Persist and apply a new UI language.
    SetLanguage(Lang),

    /// Persist and immediately activate a model offered by [`UiEvent::ModelOptions`].
    SetModel {
        /// Provider/model pair selected in the panel.
        binding: ModelBinding,
    },

    /// Begin an orderly shutdown: cancel runs, reap children, close the
    /// database. §6 — child processes must not outlive aibo.
    Quit,
}

/// Something the runtime tells the UI.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum UiEvent {
    /// Context capture finished, partially or fully.
    ///
    /// §8: the panel "tolerates context arriving late, empty or never". Every
    /// field is optional for that reason.
    Context {
        /// Session.
        session: SessionId,
        /// The frontmost app, if it could be identified.
        app: Option<AppInfo>,
        /// The focused field, if readable.
        field: Option<Box<FieldContext>>,
        /// The selection, if any.
        selection: Option<String>,
        /// The clipboard, if it was consulted.
        clipboard: Option<Box<ClipboardItem>>,
    },

    /// Capture failed. Rendered as a toast, never blocking (§13).
    ContextFailed {
        /// Session.
        session: SessionId,
        /// The failure.
        error: Arc<AiboError>,
    },

    /// The request was dispatched; the panel switches to its loading state.
    Dispatched {
        /// Session.
        session: SessionId,
        /// Provider that took it.
        provider: ProviderId,
        /// Wire model id.
        model: String,
        /// Set when the role chain fell back; §13 requires a subtle footnote
        /// naming the substitute rather than silence.
        substituted_for: Option<ProviderId>,
    },

    /// A provider stream event (§7).
    Stream {
        /// Session.
        session: SessionId,
        /// The event.
        event: Box<StreamEvent>,
    },

    /// Latency to the first token, for the §16 metadata line and the §15
    /// budget.
    FirstToken {
        /// Session.
        session: SessionId,
        /// Milliseconds from hotkey-down.
        elapsed_ms: u64,
    },

    /// Running cost for the session, in the user's display currency (§14).
    Cost {
        /// Session.
        session: SessionId,
        /// Already formatted — currency and precision are a settings concern.
        label: String,
        /// Token accounting behind the label.
        usage: Usage,
    },

    /// The request failed. The UI maps it to a §13 treatment; it never renders
    /// the error's own `Display` for `Internal`.
    Failed {
        /// Session.
        session: SessionId,
        /// The failure.
        error: Arc<AiboError>,
    },

    /// The insert succeeded; the panel may close.
    Inserted {
        /// Session.
        session: SessionId,
    },

    /// An agent run started and needs a task window (§6).
    TaskStarted {
        /// Task id.
        task: Uuid,
        /// The instruction, shown as the window's subject and as approval
        /// provenance (§5 rule 3).
        instruction: String,
    },

    /// A step from an agent run.
    TaskStep {
        /// Task id.
        task: Uuid,
        /// The step.
        step: Box<AgentStep>,
    },

    /// Attached displays changed; re-clamp the panel (§9, §13 `DisplaysChanged`).
    DisplaysChanged {
        /// The new display set.
        displays: Vec<DisplayInfo>,
    },

    /// A permission's status changed — including the §17 revoked-after-update
    /// case, which gets its own recovery treatment.
    PermissionChanged {
        /// Which permission.
        permission: Permission,
        /// Its new status.
        status: PermissionStatus,
    },

    /// A provider's health changed (§13: per provider, with hysteresis).
    /// A provider was removed in settings and its row must go.
    ///
    /// [`UiEvent::ProviderHealth`] only ever adds or updates a row, so without
    /// this a forgotten provider stays on screen looking configured — an
    /// advertised control backed by nothing, which §17 treats as worse than an
    /// absent one.
    ProviderRemoved {
        /// The provider that no longer exists.
        provider: ProviderId,
    },

    /// A provider's reachability changed (§13, per provider with hysteresis).
    ProviderHealth {
        /// The provider.
        provider: ProviderId,
        /// Its health.
        health: Health,
    },

    /// Models the panel may select, plus the currently configured binding.
    ModelOptions {
        /// Backend-validated choices.
        options: Vec<ModelOption>,
        /// Active choice, including a previously configured future model that
        /// is not yet part of the shipped catalogue.
        selected: Option<ModelBinding>,
    },

    /// Persisted language loaded after the shell was already brought up.
    LanguageChanged {
        /// Language now in force.
        language: Lang,
    },

    /// Spend against the configured cap, for the meter (§14).
    Spend {
        /// Formatted amount.
        label: String,
        /// Fraction of the cap, if one is set.
        fraction_of_cap: Option<f32>,
    },

    /// aibo restarted after a panic (§6). Shown once, with a diagnostics link.
    RecoveredFromCrash,

    /// A redacted diagnostics bundle reached the clipboard.
    DiagnosticsCopied,

    /// No usable provider is configured; open the functional setup surface.
    OnboardingRequired,

    /// A second process launch asked this instance to show its panel.
    OpenPanel,

    /// Encrypted history setup finished.
    HistoryReady {
        /// Present only when a new key was generated. `Debug` stays redacted.
        recovery_code: Option<SecretString>,
    },

    /// Encrypted history setup failed; details remain diagnostics-only.
    HistorySetupFailed,
}
