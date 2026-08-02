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
    /// What the model can do, from §10's catalogue.
    ///
    /// Shown as badges in the quick-pick, because "can this one see an image?"
    /// is the question that actually decides a choice, and the alternative was
    /// finding out from a `VisionUnsupported` error after the fact.
    pub abilities: Abilities,
    /// When the provider says it was released, as a Unix timestamp.
    ///
    /// The ordering signal: newest first. `None` sorts last, which is the honest
    /// place for a release date the provider does not report.
    pub released_at: Option<u64>,
    /// Roughly what it costs to run, from §14's price table.
    ///
    /// `None` means **unpriced, not free**. §14 is explicit that reporting
    /// $0.00 for a model whose price aibo does not know is worse than saying
    /// nothing, so the picker renders nothing rather than a zero.
    pub cost: Option<CostTier>,
}

/// The capabilities worth showing next to a model name.
///
/// A deliberate subset of `aibo_core::types::Capabilities`: these three change
/// what the user can *ask for*. Context window and output cap matter too, but
/// they do not belong on a one-line row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Abilities {
    /// Accepts image input.
    pub vision: bool,
    /// Can be given tools.
    pub tools: bool,
    /// Exposes a reasoning-effort control.
    pub reasoning: bool,
}

/// A coarse price band, for comparing models at a glance (§14).
///
/// Derived from the **output** rate, which dominates the bill for chat: a reply
/// is mostly output tokens, and the input side is usually cached or small. The
/// bands are wide on purpose — this is for "which of these is the cheap one",
/// not for budgeting, which §14's spend meter does with real numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CostTier {
    /// Under $1 per million output tokens.
    Low,
    /// Under $5.
    Moderate,
    /// Under $20.
    High,
    /// $20 or more.
    Premium,
}

impl CostTier {
    /// Band an output rate given in micros per million tokens.
    pub fn from_output_micros(micros_per_mtok: u64) -> Self {
        // 1 USD = 1_000_000 micros.
        // Disjoint bounds rather than open-ended `..=`, which would each start
        // at zero and overlap: the arms happen to resolve correctly by order,
        // but a reader cannot see that and a reordering would silently change
        // every band.
        match micros_per_mtok {
            0..=1_000_000 => CostTier::Low,
            1_000_001..=5_000_000 => CostTier::Moderate,
            5_000_001..=20_000_000 => CostTier::High,
            _ => CostTier::Premium,
        }
    }

    /// `$` to `$$$$`.
    pub const fn glyphs(self) -> &'static str {
        match self {
            CostTier::Low => "$",
            CostTier::Moderate => "$$",
            CostTier::High => "$$$",
            CostTier::Premium => "$$$$",
        }
    }
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
        /// The working directory an agent run should start in (owner
        /// redesign, 2026-08-02). `None` keeps the default — the first
        /// configured root. Ignored for chat surfaces.
        workdir: Option<std::path::PathBuf>,
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

    /// Queue a mid-run instruction into a running agent task (steering,
    /// pi's queuing model): consumed at the loop's next turn boundary.
    SteerTask {
        /// The run to steer.
        task: Uuid,
        /// The user's typed text, verbatim.
        text: String,
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

    /// Persist the appearance preference (`ui.appearance`). The UI applies
    /// the palette itself; the runtime only writes the file.
    SetAppearance(crate::theme::AppearancePreference),

    /// Persist and immediately activate a model offered by [`UiEvent::ModelOptions`].
    SetModel {
        /// Provider/model pair selected in the panel.
        binding: ModelBinding,
    },

    /// Walk the configured file roots for the `@` finder (§P9+). Answered by
    /// [`UiEvent::FileCandidates`]; the walk is bounded runtime-side.
    ListFiles,

    /// Read one picked file so it can ride the fenced selection pipeline.
    /// Answered by [`UiEvent::FileAttached`] or [`UiEvent::FileAttachFailed`].
    AttachFile {
        /// Absolute path, exactly as [`UiEvent::FileCandidates`] reported it.
        path: String,
    },

    /// List candidate agent working directories (owner redesign, 2026-08-02):
    /// recently used first, then the configured roots and their immediate
    /// subdirectories. Answered by [`UiEvent::WorkdirCandidates`].
    ListWorkdirs,

    /// List installed skills for the `/skills` overlay. Answered by
    /// [`UiEvent::SkillCatalog`].
    ListSkills,

    /// Persist the dictation backend choice: `"openai"`, `"chatgpt"`, or
    /// `None` for auto.
    SetSttBackend {
        /// The choice, as `[stt] backend` spells it.
        backend: Option<String>,
    },

    /// Persist the quick-pick pin set. Sent on every deliberate toggle: a pin
    /// must survive a restart, or pinning is pointless.
    SetPinnedModels {
        /// The full pinned set, in pin order. May be empty — an explicitly
        /// emptied set is a choice, not an absence.
        pins: Vec<ModelBinding>,
    },

    /// Persist a rebound panel hotkey (`[ui] panel_hotkey`), or `None` to
    /// return to the platform default. Registration already happened UI-side —
    /// the runtime's only job is the file.
    SetPanelHotkey {
        /// The spec in `aibo_ui::hotkey::parse` syntax, e.g.
        /// `"control+alt+Space"`.
        spec: Option<String>,
    },

    /// Persist the §8 accessibility-activation opt-in. Applies at the next
    /// start: the flag is baked into the platform worker at construction, and
    /// the settings row says so.
    SetAxTreeActivation {
        /// Whether aibo may switch on an app's AX tree to read its content.
        enabled: bool,
    },

    /// Persist the `@` finder's search roots (`[files] roots`) and use them
    /// for every walk from now on. `None` returns to the platform defaults.
    SetFileRoots {
        /// Directories, `~/` prefixes allowed. Empty means "index nothing",
        /// which is a choice the user may make.
        roots: Option<Vec<String>>,
    },

    /// Persist the §14 monthly budget and enforce it from the next request.
    /// `None` removes the ceiling.
    SetMonthlyBudget {
        /// Ceiling in millionths of a currency unit.
        limit_micros: Option<u64>,
        /// Warn at this percentage of the limit.
        warn_at_percent: u8,
        /// Refuse new requests past the limit.
        hard_stop: bool,
    },

    /// Begin push-to-talk dictation: microphone → realtime transcription,
    /// streaming text back as [`UiEvent::DictationDelta`]s (§P9+).
    StartDictation,

    /// Finish dictation: commit the audio turn and close the stream. The final
    /// text arrives through the same delta events before
    /// [`UiEvent::DictationEnded`].
    StopDictation,

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
        /// The panel session the run was started from — the conversation its
        /// activity card renders in (owner redesign, 2026-08-02).
        session: SessionId,
        /// The instruction, shown as the card's subject and as approval
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

    /// Text from `UiRequest::Copy` reached the clipboard. §16: an action that
    /// produces no visible change must still produce visible confirmation —
    /// without this, the panel's most-used action is silent.
    Copied,

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

    /// The microphone is live and streaming to the transcriber (§P9+).
    DictationStarted,

    /// A fragment of transcribed speech. Appended to the panel input verbatim.
    DictationDelta {
        /// The new text, already in reading order.
        text: String,
    },

    /// Dictation finished: the final transcript has been delivered through the
    /// deltas and the microphone is closed.
    DictationEnded,

    /// Dictation could not start or died. Typed so the UI owns the copy; the
    /// runtime never localises (§9).
    DictationFailed {
        /// What went wrong, in the panel's vocabulary.
        failure: DictationFailure,
    },

    /// The persisted pin set, published once at startup — and only when the
    /// user has ever customised pins. Its absence is what lets the derived
    /// defaults apply on a fresh install.
    PinnedModelsLoaded {
        /// The pins, in pin order. May be empty.
        pins: Vec<ModelBinding>,
    },

    /// The `@` finder's candidate list, answering [`UiRequest::ListFiles`].
    FileCandidates {
        /// Bounded by the runtime's walk limits.
        files: Vec<FileCandidate>,
    },

    /// Candidate agent working directories, answering
    /// [`UiRequest::ListWorkdirs`].
    WorkdirCandidates {
        /// Recently used, most recent first — the "pick up where I was" rows.
        recents: Vec<std::path::PathBuf>,
        /// The configured roots and their immediate subdirectories.
        dirs: Vec<std::path::PathBuf>,
    },

    /// The installed skills, answering [`UiRequest::ListSkills`]:
    /// `(name, description)` pairs plus the folder they live in — the folder
    /// is the backup story, so the overlay shows it.
    SkillCatalog {
        /// Installed skills, sorted by name.
        skills: Vec<(String, String)>,
        /// The skills folder.
        dir: std::path::PathBuf,
    },

    /// The persisted settings the runtime owns, published once at startup so
    /// the settings window edits real values instead of blanks.
    SettingsLoaded {
        /// `[ui] allow_ax_tree_activation`.
        ax_tree_activation: bool,
        /// `[files] roots` exactly as configured; `None` means the defaults.
        file_roots: Option<Vec<String>>,
        /// The defaults those roots fall back to, for honest display.
        default_file_roots: Vec<String>,
        /// `[budget]`, as (limit_micros, warn_at_percent, hard_stop).
        budget: Option<(u64, u8, bool)>,
        /// `[stt] backend`: `"openai"`, `"chatgpt"`, or `None` for auto.
        stt_backend: Option<String>,
    },

    /// A picked file's content, size-capped and text-decoded. The UI routes
    /// it into the fenced selection slot — file bytes are §5 untrusted
    /// context, and the selection pipeline already treats them as exactly
    /// that.
    FileAttached {
        /// The file's name, for the toast.
        name: String,
        /// The content, capped runtime-side.
        content: String,
    },

    /// The picked file could not be read as bounded text.
    FileAttachFailed {
        /// The file's name, for the toast.
        name: String,
    },
}

/// One entry the `@` file finder can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCandidate {
    /// Home-relative display form, e.g. `~/Documents/統計資料.pdf`.
    pub display: String,
    /// Absolute path handed back to [`UiRequest::AttachFile`].
    pub path: String,
}

/// Why dictation failed, small enough to map one-to-one onto §13 copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictationFailure {
    /// No OpenAI API key is configured; the transcriber has nothing to talk to.
    NoOpenAiKey,
    /// The microphone could not be opened — missing device or OS permission.
    Microphone,
    /// The websocket to the transcriber failed to connect or dropped mid-turn.
    Connection,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bands must be disjoint and monotonic, or two models with different
    /// prices can read as the same cost.
    #[test]
    fn cost_bands_are_monotonic_and_disjoint() {
        assert_eq!(CostTier::from_output_micros(400_000), CostTier::Low);
        assert_eq!(CostTier::from_output_micros(2_000_000), CostTier::Moderate);
        assert_eq!(CostTier::from_output_micros(10_000_000), CostTier::High);
        assert_eq!(CostTier::from_output_micros(75_000_000), CostTier::Premium);

        // Boundaries land in the cheaper band, and never in two.
        assert_eq!(CostTier::from_output_micros(1_000_000), CostTier::Low);
        assert_eq!(CostTier::from_output_micros(1_000_001), CostTier::Moderate);
        assert_eq!(CostTier::from_output_micros(0), CostTier::Low);

        assert!(CostTier::Low < CostTier::Moderate);
        assert!(CostTier::High < CostTier::Premium);
    }

    #[test]
    fn cost_glyphs_grow_with_the_band() {
        assert_eq!(CostTier::Low.glyphs().len(), 1);
        assert_eq!(CostTier::Premium.glyphs().len(), 4);
    }
}
