//! Providers, roles, budgets, permissions, history (§6, §16).
//!
//! §16: "the settings window has its own information architecture — providers,
//! roles, budgets, permissions, actions, history, about/license — and it was a
//! single bullet in the first draft's P1." [`Section`] is that IA, made
//! explicit so the navigation cannot quietly diverge from the plan.
//!
//! This module implements the **shell**: the window, the section list, the
//! per-section frames, and the two things that must work before any provider is
//! configured — the permission states (§8, §17) and the hotkey status (§9).
//! The forms themselves are per-section product work.

use aibo_core::types::{Health, ModelBinding, Permission, PermissionStatus, ProviderId, Role};
use iced::widget::{
    Space, button, column, container, pick_list, row, rule, scrollable, text, text_editor,
    text_input,
};
use iced::{Element, Length};
use secrecy::{ExposeSecret as _, SecretString};

use crate::hotkey::{Binding, FailureReason, HotkeyAction, HotkeyStatus};
use crate::i18n::{self, Key, Lang};
use crate::theme::{self, Severity, space, type_scale};
use crate::widgets::{self, Action};

/// The settings information architecture (§16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Section {
    /// Provider credentials and endpoints (§10).
    #[default]
    Providers,
    /// The model catalogue and its pinned favourites (§4).
    Models,
    /// Role bindings and fallback chains (§4).
    Roles,
    /// Per-role and global spend caps (§14).
    Budgets,
    /// OS permissions and their recovery paths (§8, §17).
    Permissions,
    /// Saved actions and custom verbs (§12).
    Actions,
    /// Conversation history and FTS search (§12).
    History,
    /// The `@` finder's search roots (§P9+).
    Files,
    /// Dictation source (§P9+, owner request 2026-08-02).
    Dictation,
    /// Dark, light, or follow the system (owner request, 2026-08-03).
    Appearance,
    /// UI language (§9).
    Language,
    /// Version, licence, diagnostics (§19).
    About,
}

impl Section {
    /// Every section, in navigation order.
    pub const ALL: [Section; 12] = [
        Section::Providers,
        Section::Models,
        Section::Roles,
        Section::Budgets,
        Section::Permissions,
        Section::Actions,
        Section::History,
        Section::Files,
        Section::Dictation,
        Section::Appearance,
        Section::Language,
        Section::About,
    ];

    /// Sections that currently provide real settings controls.
    ///
    /// Roles and Actions remain in the durable information architecture, but
    /// showing them before their editors exist creates navigation that ends in
    /// unrelated generic empty-state copy. Add them here when their controls
    /// ship.
    pub const VISIBLE: [Section; 10] = [
        Section::Providers,
        Section::Models,
        Section::Budgets,
        Section::Permissions,
        Section::History,
        Section::Files,
        Section::Dictation,
        Section::Appearance,
        Section::Language,
        Section::About,
    ];

    /// Catalogue key for the section's title.
    pub const fn title(self) -> Key {
        match self {
            Section::Providers => Key::SettingsProviders,
            Section::Models => Key::SettingsModels,
            Section::Roles => Key::SettingsRoles,
            Section::Budgets => Key::SettingsBudgets,
            Section::Permissions => Key::SettingsPermissions,
            Section::Actions => Key::SettingsActions,
            Section::History => Key::SettingsHistory,
            Section::Files => Key::SettingsFiles,
            Section::Dictation => Key::SettingsDictation,
            Section::Appearance => Key::SettingsAppearance,
            Section::Language => Key::SettingsLanguage,
            Section::About => Key::SettingsAbout,
        }
    }
}

// ---------------------------------------------------------------------------
// Codex sign-in (§3a)
// ---------------------------------------------------------------------------

/// Separates the human sentence from the machine tag inside the Codex row's
/// [`Health::Degraded`] reason.
///
/// **Why the state travels inside `Health` at all.** The device-code flow runs
/// on the tokio side and its progress has to reach this window, but the only
/// per-provider push channel that exists is `UiEvent::ProviderHealth`, whose
/// payload is a [`Health`] — and [`ProviderRow`] is the only thing the settings
/// window receives. So the phase rides in `reason`, and it rides *after* a
/// plain-English sentence: any renderer that ignores the tag still shows
/// something a user can act on, and [`CodexPhase::read`] degrades to
/// [`CodexPhase::from_health`]'s honest reading rather than to nonsense.
///
/// U+001F (unit separator) cannot occur in a user code, a URL or a §13 error
/// string, so the split is unambiguous.
///
/// TODO(bridge): replace with a `UiEvent::CodexSignIn` variant once
/// `bridge.rs` and `app.rs` are in scope. This encoding is the interim.
const CODEX_MARKER: &str = "\u{1f}aibo-codex\u{1f}";

/// Where a Codex device-code login has got to (§3a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodexPhase {
    /// No token pair is held. The button starts a login.
    #[default]
    SignedOut,
    /// `POST …/deviceauth/usercode` is in flight.
    Starting,
    /// The user has a code and must approve it at `auth.openai.com/codex/device`.
    /// §3a deviation 4: pending approval is HTTP 403, so this phase can last
    /// the whole life of the code without anything looking like an error.
    AwaitingApproval,
    /// Approved; the step-4 form-encoded `/oauth/token` exchange is running.
    Exchanging,
    /// A usable token pair is held and the provider is in the registry.
    SignedIn,
    /// The attempt ended badly — expiry, a refused client id, a network fault.
    Failed,
}

impl CodexPhase {
    /// The stable tag written into [`Health::Degraded`]'s reason.
    const fn tag(self) -> &'static str {
        match self {
            Self::SignedOut => "signed-out",
            Self::Starting => "starting",
            Self::AwaitingApproval => "awaiting",
            Self::Exchanging => "exchanging",
            Self::SignedIn => "signed-in",
            Self::Failed => "failed",
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "signed-out" => Self::SignedOut,
            "starting" => Self::Starting,
            "awaiting" => Self::AwaitingApproval,
            "exchanging" => Self::Exchanging,
            "signed-in" => Self::SignedIn,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    /// Whether a login is in flight, so the one button means "cancel".
    pub const fn in_flight(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::AwaitingApproval | Self::Exchanging
        )
    }

    /// Encode a phase and its human sentence as the health a backend publishes.
    ///
    /// [`CodexPhase::SignedIn`] becomes [`Health::Ok`] so the row reads as a
    /// working provider to anything that only understands `Health`; every other
    /// phase is `Degraded`, which is true — Codex cannot serve a request until
    /// the login finishes.
    ///
    /// **`SignedIn` discards `detail`, and that is the honest trade.**
    /// [`Health::Ok`] has nowhere to put a sentence, and widening `SignedIn` to
    /// `Degraded` so one could ride along would publish a *working* provider as
    /// degraded — a lie in the one field whose whole job is to say whether a
    /// provider works (§13). So the signed-in row reads
    /// [`Key::SettingsCodexSignedIn`] and anything more specific (the account id,
    /// §3a's plan-type claim) needs a channel that can carry it.
    ///
    /// TODO(bridge): a `UiEvent::CodexSignIn` variant would carry both, and is
    /// where the §3a quota readout belongs.
    pub fn to_health(self, detail: &str) -> Health {
        if self == Self::SignedIn {
            return Health::Ok {
                latency: std::time::Duration::ZERO,
            };
        }
        Health::Degraded {
            reason: format!("{detail}{CODEX_MARKER}{}", self.tag()),
            consecutive_failures: 0,
        }
    }

    /// Read a phase and its human sentence back out of a [`Health`].
    ///
    /// The fallback is deliberate rather than defensive: a Codex row whose
    /// health came from an ordinary §13 probe — a revoked token, an unreachable
    /// endpoint — must still render as something the user can act on, and
    /// "there is a problem, here is the sentence, here is Sign in" is exactly
    /// that. Silently showing "signed out" for a revoked token would hide the
    /// one fact that matters.
    pub fn read(health: &Health) -> (Self, String) {
        if let Health::Degraded { reason, .. } = health
            && let Some((detail, tag)) = reason.split_once(CODEX_MARKER)
            && let Some(phase) = Self::from_tag(tag)
        {
            return (phase, detail.to_owned());
        }
        Self::from_health(health)
    }

    /// The reading for a health that carries no phase tag.
    fn from_health(health: &Health) -> (Self, String) {
        match health {
            Health::Ok { .. } => (
                Self::SignedIn,
                codex_default_detail(Self::SignedIn).to_owned(),
            ),
            Health::Degraded { reason, .. } | Health::Unavailable { reason } => {
                (Self::Failed, reason.clone())
            }
            Health::Unknown => (
                Self::SignedOut,
                codex_default_detail(Self::SignedOut).to_owned(),
            ),
        }
    }
}

/// Non-localisable Codex endpoint copy shared with the application bridge.
pub mod codex_text {
    /// The page the user approves the code on (§3a).
    ///
    /// Duplicated from `aibo_provider::codex::VERIFICATION_URI` rather than
    /// imported: `aibo-ui` does not depend on `aibo-provider`, and adding that
    /// edge to reach one constant would invert the layering for the sake of a
    /// string. The binary asserts the two agree.
    pub const VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
}

/// The catalogue key on the Codex card's single button, for a given phase.
///
/// Public because the backend owns the *action* that button triggers and this
/// owns its *wording*, and the two must never disagree — a button that says
/// "Sign in" while pressing it signs the user out is the failure mode of
/// collapsing three actions into one control. The binary asserts the agreement.
pub const fn codex_action_key(phase: CodexPhase) -> Key {
    match phase {
        CodexPhase::SignedIn => Key::SettingsCodexSignOut,
        CodexPhase::Starting | CodexPhase::AwaitingApproval | CodexPhase::Exchanging => {
            Key::SettingsCodexCancelSignIn
        }
        CodexPhase::SignedOut | CodexPhase::Failed => Key::SettingsCodexSignIn,
    }
}

/// Localised label for [`codex_action_key`].
pub fn codex_action_label(phase: CodexPhase) -> &'static str {
    i18n::t(codex_action_key(phase))
}

/// Catalogue key for a Codex phase's default supporting sentence.
pub const fn codex_default_detail_key(phase: CodexPhase) -> Key {
    match phase {
        CodexPhase::SignedOut => Key::SettingsCodexSignedOut,
        CodexPhase::Starting => Key::SettingsCodexStarting,
        CodexPhase::AwaitingApproval => Key::SettingsCodexAwaitingApproval,
        CodexPhase::Exchanging => Key::SettingsCodexExchanging,
        CodexPhase::SignedIn => Key::SettingsCodexSignedIn,
        CodexPhase::Failed => Key::SettingsCodexFailed,
    }
}

/// Localised default supporting sentence for a Codex phase.
///
/// A backend-provided failure or live device-code sentence should take
/// precedence; this covers phase transitions before more specific detail
/// arrives and keeps that bridge-owned copy out of hard-coded English.
pub fn codex_default_detail(phase: CodexPhase) -> &'static str {
    i18n::t(codex_default_detail_key(phase))
}

/// A provider as the settings list shows it.
#[derive(Debug, Clone)]
pub struct ProviderRow {
    /// The provider.
    pub id: ProviderId,
    /// Whether a credential is present. The credential itself never reaches
    /// the UI — §12 keeps secrets in the keyring and out of process memory
    /// wherever it can.
    pub configured: bool,
    /// Health from the last probe (§13, per provider with hysteresis).
    pub health: aibo_core::types::Health,
}

/// A permission and its current state (§8, §17).
#[derive(Debug, Clone, Copy)]
pub struct PermissionRow {
    /// Which permission.
    pub permission: Permission,
    /// Its status.
    pub status: PermissionStatus,
}

/// Settings window state.
#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    /// The visible section.
    pub section: Section,
    /// Providers, for the providers section.
    pub providers: Vec<ProviderRow>,
    /// Permissions, for the permissions section.
    pub permissions: Vec<PermissionRow>,
    /// Formatted spend, for the budgets section (§14).
    pub spend_label: String,
    /// Spend as a fraction of the cap, if a cap is set.
    pub spend_fraction: Option<f32>,
    /// Whether the panel hotkey registered, and under what label (§9).
    pub hotkey: Option<HotkeyStatus>,
    /// The active UI language.
    pub language: Lang,
    /// The appearance preference (`[ui] appearance`), as chosen — System
    /// stays System here even while it resolves to a concrete palette.
    pub appearance: theme::AppearancePreference,
    /// The dictation backend choice (`[stt] backend`).
    pub stt_backend: SttChoice,
    /// Whether ⏎ ends a live dictation turn (`[stt] end_on_send`, owner
    /// 2026-08-03). The shell's submit path reads this.
    pub stt_end_on_send: bool,
    /// Whether this window is the first-run setup rather than a later visit.
    pub onboarding: bool,
    /// Whether the optional Codex security/storage explanation is expanded.
    pub codex_details_expanded: bool,
    /// Current selectable Codex device code, when approval is pending.
    device_code: Option<String>,
    /// Read-only selection state for [`Self::device_code`].
    device_code_editor: text_editor::Content,
    /// Encrypted history is available to the session engine.
    pub history_ready: bool,
    /// A setup operation is in flight.
    pub history_initializing: bool,
    /// Setup failed; details remain in diagnostics.
    pub history_failed: bool,
    /// Newly generated recovery code. Kept redacted and shown only this run.
    pub recovery_code: Option<SecretString>,
    /// The provider being added or edited, if any.
    pub draft: Option<ProviderDraft>,
    /// The provider a first Forget press armed. Deleting a credential is
    /// irreversible, so the row's action asks to be pressed twice; navigating
    /// away or Escape disarms it.
    pub forget_armed: Option<ProviderId>,
    /// The model catalogue, mirrored from the panel for the Models section.
    pub models: Vec<crate::bridge::ModelOption>,
    /// The pinned set the quick-pick shows, including derived defaults —
    /// mirrored so the stars here and the pins there cannot disagree.
    pub favourite_models: Vec<ModelBinding>,
    /// Which copy affordance most recently fired, for the momentary
    /// `✓ copied` confirmation (`design.md` §6b — "silent copying leaves
    /// people pressing it twice").
    pub copied_badge: Option<CopiedBadge>,
    /// Monotonic copy count, so a stale expiry task cannot clear the badge a
    /// newer copy just set.
    pub copied_epoch: u64,
    /// §8's accessibility-activation opt-in. Applies at the next start — the
    /// flag is baked into the platform worker at construction — and the row
    /// says so rather than pretending otherwise.
    pub ax_tree_activation: bool,
    /// The `@` finder's configured roots; `None` means the defaults apply.
    pub file_roots: Option<Vec<String>>,
    /// What those defaults are, so the unset state shows real paths instead
    /// of the word "default".
    pub default_file_roots: Vec<String>,
    /// A root being typed, not yet added.
    pub root_draft: String,
    /// The hotkey spec being typed, in `hotkey::parse` syntax.
    pub hotkey_draft: String,
    /// Whether the current [`Self::hotkey_draft`] failed to parse on apply.
    pub hotkey_draft_invalid: bool,
    /// The monthly ceiling being typed, in whole currency units (`"15"`).
    pub budget_limit_draft: String,
    /// The warn threshold being typed, percent.
    pub budget_warn_draft: String,
    /// Refuse requests past the limit (§14 hard stop).
    pub budget_hard_stop: bool,
    /// Whether a monthly ceiling is currently applied, so "remove" is only
    /// offered when there is something to remove.
    pub budget_configured: bool,
}

/// A copy affordance that confirms itself with a momentary `✓ copied`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopiedBadge {
    /// The §6b device code.
    DeviceCode,
    /// The one-time history recovery code.
    RecoveryCode,
}

/// A key-based provider the user is part-way through configuring.
///
/// §12 keeps secrets out of `config.toml` and out of diagnostics. It cannot
/// keep them out of a text field — a key has to be typed somewhere — so the
/// rules here are narrower and enforced by construction: the field renders
/// masked, the value is never logged and never rendered back after saving, and
/// [`ProviderDraft::take_key`] moves it out and overwrites what is left rather
/// than letting a `String` drop with the bytes still on the heap.
#[derive(Clone, Default)]
pub struct ProviderDraft {
    /// Which backend.
    pub backend: Backend,
    /// Explicit id, for a second endpoint of a backend already configured.
    pub id: String,
    /// Base URL. Required for a custom endpoint.
    pub base_url: String,
    /// The API key, as typed.
    key: String,
}

impl ProviderDraft {
    /// Start a draft for `backend`.
    pub fn new(backend: Backend) -> Self {
        // Field-by-field rather than `..Self::default()`: `Drop` scrubs the
        // key, and a type that implements `Drop` cannot be partially moved out
        // of a temporary.
        Self {
            backend,
            id: String::new(),
            base_url: String::new(),
            key: String::new(),
        }
    }

    /// The typed key, for the text field to render.
    ///
    /// A password field has to be bound to the real value or editing it does
    /// not work — `secure(true)` is what stops it being *displayed*. This is
    /// the only reader, and it must stay that way: nothing may log this, put it
    /// in a `Debug`, or copy it into another struct.
    pub fn key_field(&self) -> &str {
        &self.key
    }

    /// Replace the typed key, scrubbing the previous value.
    pub fn set_key(&mut self, key: String) {
        self.scrub_key();
        self.key = key;
    }

    /// Move the key out, leaving nothing recoverable behind.
    pub fn take_key(&mut self) -> SecretString {
        let key = std::mem::take(&mut self.key);
        let secret = SecretString::from(key.as_str().to_owned());
        let mut leftover = key;
        scrub(&mut leftover);
        secret
    }

    fn scrub_key(&mut self) {
        let mut old = std::mem::take(&mut self.key);
        scrub(&mut old);
    }

    /// Whether this draft is complete enough to save.
    pub fn is_saveable(&self) -> bool {
        if self.key.trim().is_empty() {
            return false;
        }
        // A custom endpoint is defined by its URL and needs a name to be
        // addressed by; every other backend has both compiled in.
        !(self.backend == Backend::Custom
            && (self.base_url.trim().is_empty() || self.id.trim().is_empty()))
    }
}

/// Redacts the key.
///
/// **Not `#[derive(Debug)]`, and a test enforces that.** The derive prints
/// every field, so a draft that reached a `tracing` call, a panic message or
/// §19's diagnostics bundle would carry the user's API key in clear text — the
/// precise failure §12 exists to prevent, arriving through a line nobody would
/// think to audit. Only the key's presence is ever worth reporting.
impl std::fmt::Debug for ProviderDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderDraft")
            .field("backend", &self.backend)
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field(
                "key",
                &if self.key.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .finish()
    }
}

impl Drop for ProviderDraft {
    fn drop(&mut self) {
        self.scrub_key();
    }
}

/// Overwrite a string's bytes in place before its allocation is released.
///
/// `String::clear` and `String::drop` both leave the original bytes in the
/// freed allocation, where the next allocation of that size can read them.
/// `zeroize` overwrites through a volatile write the optimiser is not allowed
/// to elide — which a hand-rolled loop is, since writing to memory that is
/// about to be freed is exactly the kind of store a compiler may drop.
fn scrub(value: &mut String) {
    use zeroize::Zeroize as _;
    value.zeroize();
}

/// The key-based backends settings can configure.
///
/// Codex is absent deliberately: §3a authenticates it by device code, not by a
/// key, and offering a key field for it would invite someone to paste an OpenAI
/// API key into a flow that cannot use one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// OpenRouter: one key, many upstream vendors.
    #[default]
    OpenRouter,
    /// Google Gemini, direct.
    Gemini,
    /// Anthropic native `messages`.
    Anthropic,
    /// OpenAI.
    OpenAi,
    /// Groq.
    Groq,
    /// Cerebras.
    Cerebras,
    /// xAI / Grok.
    Xai,
    /// Any other OpenAI-compatible endpoint — DeepSeek, Mistral, LM Studio,
    /// llama.cpp, vLLM. §10 keeps the provider set open, which is why this is
    /// a first-class choice rather than a hidden escape hatch.
    Custom,
}

impl Backend {
    /// Everything offerable, in the order the picker shows them.
    pub const ALL: [Backend; 8] = [
        Backend::OpenRouter,
        Backend::Gemini,
        Backend::Anthropic,
        Backend::OpenAi,
        Backend::Groq,
        Backend::Cerebras,
        Backend::Xai,
        Backend::Custom,
    ];

    /// The `backend = "…"` value `config.toml` expects.
    ///
    /// These strings are the serde `kebab-case` spellings of
    /// `aibo_session::Backend`; a mismatch here writes a config the loader
    /// rejects, which is why a test asserts every one of them round-trips.
    pub const fn config_value(self) -> &'static str {
        match self {
            Backend::OpenRouter => "open-router",
            Backend::Gemini => "gemini",
            Backend::Anthropic => "anthropic",
            Backend::OpenAi => "open-ai",
            Backend::Groq => "groq",
            Backend::Cerebras => "cerebras",
            Backend::Xai => "xai",
            Backend::Custom => "custom",
        }
    }

    /// Name for the picker. Not localised: these are product names.
    pub const fn display_name(self) -> &'static str {
        match self {
            Backend::OpenRouter => "OpenRouter",
            Backend::Gemini => "Google Gemini",
            Backend::Anthropic => "Anthropic",
            Backend::OpenAi => "OpenAI",
            Backend::Groq => "Groq",
            Backend::Cerebras => "Cerebras",
            Backend::Xai => "xAI",
            Backend::Custom => "OpenAI-compatible endpoint",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_name())
    }
}

impl SettingsState {
    /// Current device code, exposed only to the semantic tree and copy action.
    ///
    /// This value is never formatted by infrastructure or retained after the
    /// provider leaves the approval phase.
    pub(crate) fn device_code(&self) -> Option<&str> {
        self.device_code.as_deref()
    }

    /// Synchronize the selectable device-code surface after provider health
    /// changes.
    pub fn sync_device_code(&mut self) {
        let next = self
            .providers
            .iter()
            .find(|provider| provider.id == ProviderId::CODEX)
            .and_then(|provider| {
                let (phase, detail) = CodexPhase::read(&provider.health);
                (phase == CodexPhase::AwaitingApproval)
                    .then(|| device_code_in(&detail))
                    .flatten()
            });
        if next != self.device_code {
            self.device_code_editor =
                text_editor::Content::with_text(next.as_deref().unwrap_or_default());
            self.device_code = next;
        }
    }

    /// Apply selection/cursor actions while keeping the server-issued code
    /// immutable.
    pub fn perform_device_code_action(&mut self, action: text_editor::Action) {
        if !action.is_edit() {
            self.device_code_editor.perform(action);
        }
    }
}

/// What the settings window emits.
#[derive(Debug, Clone)]
pub enum Message {
    /// Navigate to a section.
    Select(Section),
    /// Start the sign-in flow for a provider.
    SignIn(ProviderId),
    /// Begin adding a provider, or switch which backend the draft is for.
    DraftBackend(Backend),
    /// Abandon the draft, scrubbing whatever key was typed.
    DraftCancel,
    /// The draft's id field changed.
    DraftId(String),
    /// The draft's base-URL field changed.
    DraftBaseUrl(String),
    /// The draft's key field changed.
    DraftKey(String),
    /// Save the draft: store the key, write the config entry, rebuild.
    DraftSave,
    /// Forget a configured provider.
    ForgetProvider(ProviderId),
    /// Pin or unpin a model from the Models section.
    ToggleFavourite(ModelBinding),
    /// Expand or collapse the detailed Codex sign-in explanation.
    ToggleCodexDetails,
    /// Open the OS privacy pane for a permission.
    OpenSystemSettings(Permission),
    /// Change the UI language.
    SetLanguage(Lang),
    /// Change the appearance preference (owner request, 2026-08-03).
    SetAppearance(theme::AppearancePreference),
    /// Change the dictation backend.
    SetSttBackend(SttChoice),
    /// Toggle whether ⏎ ends a live dictation turn.
    SttEndOnSendToggle(bool),
    /// Copy the device-code to the clipboard.
    ///
    /// §3a's code looks like `RJF3-XIERE`, and the verification page expects it
    /// **exactly as issued, hyphen included**. Retyping a ten-character code is
    /// where people slip, and the failure is silent: the page just says the code
    /// is wrong, with no hint whether the code or the app is at fault.
    CopyDeviceCode(String),
    /// Selection, cursor, or an ignored edit attempt in the device code.
    DeviceCodeAction(text_editor::Action),
    /// Open `auth.openai.com/codex/device` in the browser, so the URL does not
    /// have to be transcribed either.
    OpenDeviceUrl,
    /// Copy a redacted diagnostics bundle (§19).
    CopyDiagnostics,
    /// Enable local SQLCipher history after an explicit user gesture.
    InitializeHistory,
    /// Copy the one-time recovery code.
    CopyRecoveryCode,
    /// A `✓ copied` badge reached the end of its moment. Ignored unless the
    /// epoch matches the most recent copy.
    CopiedBadgeExpired(u64),
    /// Toggle §8's accessibility-activation opt-in.
    AxTreeToggle(bool),
    /// The finder-root draft changed.
    RootDraft(String),
    /// Add the drafted finder root.
    RootAdd,
    /// Remove one finder root by position.
    RootRemove(usize),
    /// Return the finder roots to the platform defaults.
    RootsReset,
    /// The hotkey spec draft changed.
    HotkeyDraft(String),
    /// Parse, re-register and persist the drafted hotkey.
    HotkeyApply,
    /// The budget ceiling draft changed.
    BudgetLimitDraft(String),
    /// The warn-percent draft changed.
    BudgetWarnDraft(String),
    /// Toggle the §14 hard stop.
    BudgetHardStop(bool),
    /// Parse and apply the drafted budget.
    BudgetApply,
    /// Remove the monthly ceiling entirely.
    BudgetRemove,
    /// Close the window.
    Close,
}

/// Render the settings window.
pub fn view(state: &SettingsState) -> Element<'_, Message> {
    // `design.md` §7: "Sidebar on `ink-raised`, content on `ink`, one hairline
    // between. No card borders anywhere; group with space and a single
    // hairline." The sidebar shared the content's ground and had no separator,
    // so the two regions read as one undifferentiated field with a floating
    // selection box in it.
    let body = row![
        container(navigation(state))
            .width(Length::Fixed(180.0))
            .height(Length::Fill)
            .padding(space(2.0))
            .style(theme::raised),
        rule::vertical(1).style(theme::separator),
        container(scrollable(section_body(state)).style(theme::scroller))
            .width(Length::Fill)
            .padding(space(3.0)),
    ];

    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::panel_surface)
        .into()
}

/// The section list (`design.md` §7).
///
/// §7: "The active sidebar item is marked by an amber rail segment — the
/// identity element, reused, so the two windows read as one product." It was a
/// filled amber box, which is the treatment §9 removes everywhere else; a
/// settings window that boxes its selection while the panel draws a rail reads
/// as two products stapled together.
///
/// The checkmark stays. §16 does not let selection depend on perceiving a
/// colour, and a 3 pt bar is exactly the sort of cue that disappears for a
/// colour-blind user or on a dim external display.
fn navigation(state: &SettingsState) -> Element<'_, Message> {
    let mut list = column![];
    for section in Section::VISIBLE {
        let selected = section == state.section;
        let row = button(
            row![
                text(selection_marker(selected))
                    .width(Length::Fixed(space(3.0)))
                    .size(type_scale::META)
                    .style(theme::text_primary),
                text(i18n::t(section.title()))
                    .size(type_scale::BODY)
                    .style(if selected {
                        theme::text_primary
                    } else {
                        theme::text_dim
                    }),
            ]
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fixed(theme::MIN_HIT_TARGET))
        .padding([space(1.5), space(2.0)])
        .style(theme::action_button)
        .on_press(Message::Select(section));

        // The same `railed` the panel uses, so the amber segment means the same
        // thing in both windows: this is where you are.
        list = list.push(widgets::railed(
            if selected {
                widgets::RailState::Active
            } else {
                widgets::RailState::Inactive
            },
            row,
        ));
    }
    list.into()
}

fn section_body(state: &SettingsState) -> Element<'_, Message> {
    let heading = widgets::section::<Message>(state.section.title());
    let content: Element<'_, Message> = match state.section {
        Section::Providers => providers(state),
        Section::Models => models(state),
        Section::Permissions => permissions(state),
        Section::Budgets => budgets(state),
        Section::Dictation => dictation(state),
        Section::Appearance => appearance(state),
        Section::Language => language(state),
        Section::About => about(state),
        Section::History => history(state),
        Section::Files => files(state),
        // TODO(§4, §12, §14): role chains and saved actions
        // browser are per-section product work. The IA slot exists so
        // navigation is complete and the sections cannot drift from §16.
        Section::Roles | Section::Actions => widgets::state_block(
            Severity::Info,
            i18n::t(Key::StateEmptyTitle),
            Some(i18n::t(Key::StateEmptyBody)),
            Vec::new(),
        ),
    };

    column![heading, content].spacing(space(3.0)).into()
}

fn history(state: &SettingsState) -> Element<'_, Message> {
    if let Some(code) = &state.recovery_code {
        return column![
            widgets::state_block(
                Severity::Warning,
                i18n::t(Key::SettingsRecoveryTitle),
                Some(i18n::t(Key::SettingsRecoveryBody)),
                Vec::new(),
            ),
            container(
                text(code.expose_secret())
                    .size(type_scale::BODY)
                    .font(theme::MONO_FONT)
                    .style(theme::text_primary)
            )
            .width(Length::Fill)
            .padding(space(2.0))
            .style(theme::raised),
            widgets::action_list(vec![Action::new(
                // §6b's rule, applied to the code it matters most for:
                // silent copying of a one-time key leaves the user unsure
                // whether they saved it.
                if state.copied_badge == Some(CopiedBadge::RecoveryCode) {
                    Key::ActionCopied
                } else {
                    Key::ActionCopyRecoveryCode
                },
                widgets::primary_shortcut("⌘C", "Ctrl+C"),
                Message::CopyRecoveryCode,
            )]),
        ]
        .spacing(space(2.0))
        .into();
    }

    if state.history_ready {
        return widgets::state_block(
            Severity::Success,
            i18n::t(Key::SettingsHistoryReady),
            None,
            Vec::new(),
        );
    }

    let actions = if state.history_initializing {
        Vec::new()
    } else {
        vec![Action::new(
            Key::ActionEnableHistory,
            widgets::ENTER_KEY,
            Message::InitializeHistory,
        )]
    };
    widgets::state_block(
        if state.history_failed {
            Severity::Danger
        } else {
            Severity::Info
        },
        i18n::t(if state.history_failed {
            Key::SettingsHistoryFailed
        } else {
            Key::SettingsHistorySetupTitle
        }),
        Some(i18n::t(Key::SettingsHistorySetupBody)),
        actions,
    )
}

/// The model catalogue with its pins as clickable stars.
///
/// The quick-pick is where a pin is *used*; this is where the set is curated
/// without racing a keyboard highlight. Same data, same toggle, mirrored by
/// `sync_settings_models` so the two views cannot disagree.
fn models(state: &SettingsState) -> Element<'_, Message> {
    if state.models.is_empty() {
        return widgets::state_block(
            Severity::Info,
            i18n::t(Key::StateEmptyTitle),
            Some(i18n::t(Key::StateEmptyBody)),
            Vec::new(),
        );
    }

    let mut list = column![
        text(i18n::t(Key::SettingsModelsHint))
            .size(type_scale::META)
            .style(theme::text_dim),
    ]
    .spacing(space(1.0));

    for option in &state.models {
        let pinned = state.favourite_models.contains(&option.binding);
        let provider = option.binding.provider.as_str();
        list = list.push(
            container(
                row![
                    button(
                        text(if pinned { "\u{2605}" } else { "\u{2606}" })
                            .size(type_scale::BODY)
                            .style(if pinned {
                                theme::text_accent
                            } else {
                                theme::text_faint
                            }),
                    )
                    .padding([space(0.5), space(1.0)])
                    .style(theme::list_row_button(false))
                    .on_press(Message::ToggleFavourite(option.binding.clone())),
                    widgets::provider_logo(
                        provider,
                        crate::model_picker::provider_mark(provider),
                        false
                    ),
                    text(option.display_name.clone())
                        .size(type_scale::BODY)
                        .style(theme::text_primary),
                    Space::new().width(Length::Fill),
                    text(provider.to_owned())
                        .size(type_scale::META)
                        .font(theme::MONO_FONT)
                        .style(theme::text_dim),
                ]
                .spacing(space(1.5))
                .align_y(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .padding([space(0.5), space(1.0)]),
        );
    }
    list.into()
}

/// The Codex sign-in card (§3a).
///
/// Always rendered, and rendered **first**, including on a fresh install with
/// nothing configured. That is the point of it: the one credential a
/// ChatGPT-subscription user has is not an API key, so a Providers tab that
/// only shows already-configured rows offers them no way in at all — which is
/// the state this card was added to end.
/// Pull the device code out of the phase detail.
///
/// The backend reports progress as one human sentence ("Enter code ABCD-1234
/// at https://…"), which is right for a log and wrong for a control: a code the
/// user must copy has to be a *value*, not a substring of a paragraph. Parsed
/// here rather than restructuring the bridge, and shaped so a format change
/// degrades to "no copy button" instead of a wrong one.
fn device_code_in(detail: &str) -> Option<String> {
    detail
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-'))
        .find(|word| {
            // XXXX-XXXX: two groups, one hyphen, uppercase alphanumerics.
            let mut parts = word.split('-');
            let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
                return false;
            };
            a.len() >= 3
                && b.len() >= 3
                && a.chars()
                    .chain(b.chars())
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        })
        .map(str::to_owned)
}

fn codex_card(state: &SettingsState) -> Element<'_, Message> {
    let health = state
        .providers
        .iter()
        .find(|p| p.id == ProviderId::CODEX)
        .map(|p| p.health.clone())
        .unwrap_or(Health::Unknown);
    let (phase, detail) = CodexPhase::read(&health);

    let severity = match phase {
        CodexPhase::SignedIn => Severity::Success,
        CodexPhase::Failed => Severity::Danger,
        CodexPhase::SignedOut => Severity::Info,
        _ => Severity::Warning,
    };

    // One button, and its label always states what pressing it does. The
    // settings vocabulary carries a single per-provider action
    // (`Message::SignIn`), so the meaning is disambiguated by the label rather
    // than by a second control the bridge cannot express.
    let label = codex_action_label(phase);

    let mut body = column![
        text(i18n::t(Key::SettingsCodexTitle))
            .size(type_scale::BODY)
            .style(theme::text_primary),
        text(detail.clone())
            .size(type_scale::META)
            .style(theme::text_severity(severity)),
    ]
    .spacing(space(1.5));

    // The provider action is the first interactive object in the card. On a
    // fresh install it is the one thing the user came here to do; burying it
    // below two security paragraphs made it look like a secondary link.
    let primary = matches!(phase, CodexPhase::SignedOut | CodexPhase::Failed);
    let action = button(text(label).size(type_scale::META).style(if primary {
        theme::text_on_primary
    } else {
        theme::text_accent
    }))
    .height(Length::Fixed(theme::MIN_HIT_TARGET))
    .padding([space(1.5), space(2.0)])
    .style(if primary {
        theme::primary_button
    } else {
        theme::action_button
    })
    .on_press(Message::SignIn(ProviderId::CODEX));
    body = body.push(action);

    // While waiting for approval the code is the only thing that matters, so it
    // gets display scale and its own controls. This is the one screen where a
    // transcription error costs a full 15-minute retry cycle.
    if phase == CodexPhase::AwaitingApproval
        && let Some(code) = device_code_in(&detail)
    {
        if state.device_code.as_deref() == Some(code.as_str()) {
            body = body.push(
                text_editor(&state.device_code_editor)
                    .on_action(Message::DeviceCodeAction)
                    .height(Length::Fixed(theme::MIN_HIT_TARGET))
                    .padding(space(1.0))
                    .size(type_scale::DISPLAY)
                    .font(theme::MONO_FONT)
                    .style(theme::answer_editor),
            );
        } else {
            body = body.push(
                text(code.clone())
                    .size(type_scale::DISPLAY)
                    .font(theme::MONO_FONT)
                    .style(theme::text_accent),
            );
        }
        // §6b: the code is copyable by button and by ⌘C, the page opens on ⏎,
        // and a fired copy confirms itself — "silent copying leaves people
        // pressing it twice". The key hints are real: `window_shortcut` routes
        // both chords here while the code is on screen.
        let copy_label = if state.copied_badge == Some(CopiedBadge::DeviceCode) {
            i18n::t(Key::ActionCopied).to_owned()
        } else {
            format!(
                "⧉ {} {}",
                widgets::primary_shortcut("⌘C", "Ctrl+C"),
                i18n::t(Key::SettingsCopyDeviceCode)
            )
        };
        body = body.push(
            row![
                button(
                    text(copy_label)
                        .size(type_scale::META)
                        .style(theme::text_accent)
                )
                .height(Length::Fixed(theme::MIN_HIT_TARGET))
                .padding([space(1.0), space(2.0)])
                .style(theme::action_button)
                .on_press(Message::CopyDeviceCode(code)),
                button(
                    text(format!(
                        "{} {}",
                        widgets::ENTER_KEY,
                        i18n::t(Key::SettingsOpenDevicePage)
                    ))
                    .size(type_scale::META)
                    .style(theme::text_accent)
                )
                .height(Length::Fixed(theme::MIN_HIT_TARGET))
                .padding([space(1.0), space(2.0)])
                .style(theme::action_button)
                .on_press(Message::OpenDeviceUrl),
            ]
            .spacing(space(1.0)),
        );
    }

    let disclosure_marker = if state.codex_details_expanded {
        "▾"
    } else {
        "▸"
    };
    body = body.push(
        button(
            text(format!(
                "{disclosure_marker} {}",
                i18n::t(Key::SettingsCodexHowSignInWorks)
            ))
            .size(type_scale::META)
            .style(theme::text_dim),
        )
        .height(Length::Fixed(theme::MIN_HIT_TARGET))
        .padding([space(1.0), space(1.5)])
        .style(theme::action_button)
        .on_press(Message::ToggleCodexDetails),
    );
    if state.codex_details_expanded {
        body = body.push(
            text(i18n::t(Key::SettingsCodexConsentNote))
                .size(type_scale::META)
                .style(theme::text_dim),
        );
    }

    container(body)
        .width(Length::Fill)
        .padding(space(2.0))
        .style(theme::banner(severity))
        .into()
}

fn providers(state: &SettingsState) -> Element<'_, Message> {
    let mut list = column![].spacing(space(2.0));
    if state.onboarding {
        list = list.push(onboarding_steps(state));
    }
    list = list.push(codex_card(state));

    let others: Vec<&ProviderRow> = state
        .providers
        .iter()
        .filter(|p| p.id != ProviderId::CODEX)
        .collect();
    let single_forgettable = others.len() == 1;

    for provider in others {
        // §13: health is per provider with hysteresis, never one global
        // "offline" boolean — so each row carries its own state.
        let (severity, status) = match (&provider.health, provider.configured) {
            (_, false) => (Severity::Info, i18n::t(Key::PermissionNotDetermined)),
            (Health::Ok { .. }, _) => (Severity::Success, i18n::t(Key::PermissionGranted)),
            (Health::Degraded { .. }, _) => (Severity::Warning, i18n::t(Key::ErrRateLimited)),
            (Health::Unavailable { .. }, _) => {
                (Severity::Danger, i18n::t(Key::ErrProviderUnavailable))
            }
            (Health::Unknown, _) => (Severity::Info, i18n::t(Key::PermissionNotDetermined)),
        };

        list = list.push(
            container(
                row![
                    text(provider.id.to_string())
                        .size(type_scale::BODY)
                        .style(theme::text_primary),
                    Space::new().width(Length::Fill),
                    text(status)
                        .size(type_scale::META)
                        .style(theme::text_severity(severity)),
                ]
                .spacing(space(2.0))
                .align_y(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .padding(space(2.0))
            .style(theme::raised),
        );

        // Deleting a credential is irreversible, so the first press arms and
        // the label says what the second one will do. The ⌫ hint is shown only
        // while it is unambiguous — with several rows a global key cannot know
        // which provider it means, and a hint that fires on the wrong row is
        // worse than none.
        let armed = state.forget_armed.as_ref() == Some(&provider.id);
        list = list.push(widgets::action_list(vec![
            Action::new(
                if armed {
                    Key::ActionConfirmForget
                } else {
                    Key::ActionForgetProvider
                },
                if single_forgettable {
                    widgets::BACKSPACE_KEY
                } else {
                    ""
                },
                Message::ForgetProvider(provider.id.clone()),
            )
            .destructive(),
        ]));
    }

    list = list.push(provider_draft(state));
    list.into()
}

/// The add-a-provider form (§10, §17).
///
/// Until this existed, `config.toml` was the only way to add a provider and a
/// credential file the only way to give it a key — so "configure a provider"
/// meant "quit aibo and edit two files by hand", which §17 does not consider an
/// onboarding flow.
fn provider_draft(state: &SettingsState) -> Element<'_, Message> {
    let Some(draft) = &state.draft else {
        return widgets::action_list(vec![
            Action::new(
                Key::ActionAddProvider,
                widgets::primary_shortcut("⌘N", "Ctrl+N"),
                Message::DraftBackend(Backend::default()),
            )
            .primary(),
        ]);
    };

    let mut form = column![
        text(i18n::t(Key::SettingsAddProvider))
            .size(type_scale::HEADING)
            .style(theme::text_primary),
        pick_list(
            &Backend::ALL[..],
            Some(draft.backend),
            Message::DraftBackend,
        )
        .text_size(type_scale::META)
        .padding([space(1.0), space(1.5)])
        .style(theme::model_picker)
        .menu_style(theme::model_picker_menu),
    ]
    .spacing(space(2.0));

    // A custom endpoint is the only one that needs to be told where it is, and
    // the only one that needs a name — the rest are addressed by their backend.
    if draft.backend == Backend::Custom {
        form = form
            .push(
                text_input(i18n::t(Key::ProviderIdPlaceholder), &draft.id)
                    .on_input(Message::DraftId)
                    .size(type_scale::BODY)
                    .font(theme::MONO_FONT)
                    .padding([space(2.0), space(2.0)])
                    .style(theme::field),
            )
            .push(
                text_input(i18n::t(Key::ProviderBaseUrlPlaceholder), &draft.base_url)
                    .on_input(Message::DraftBaseUrl)
                    .size(type_scale::BODY)
                    .font(theme::MONO_FONT)
                    .padding([space(2.0), space(2.0)])
                    .style(theme::field),
            );
    }

    // `secure` is not cosmetic. A key pasted into a settings window sits on
    // screen until the window closes, and these windows get screen-shared.
    form = form.push(
        text_input(i18n::t(Key::ProviderKeyPlaceholder), draft.key_field())
            .on_input(Message::DraftKey)
            .secure(true)
            .size(type_scale::BODY)
            .font(theme::MONO_FONT)
            .padding([space(2.0), space(2.0)])
            .style(theme::field),
    );

    let save = Action::new(
        Key::ActionSaveProvider,
        widgets::ENTER_KEY,
        Message::DraftSave,
    )
    .primary();
    let save = if draft.is_saveable() {
        save
    } else {
        save.disabled()
    };
    form = form.push(widgets::action_list(vec![
        save,
        Action::new(Key::ActionDismiss, "esc", Message::DraftCancel),
    ]));

    container(form).width(Length::Fill).into()
}

fn onboarding_steps(state: &SettingsState) -> Element<'_, Message> {
    let connected = state
        .providers
        .iter()
        .any(|provider| matches!(provider.health, Health::Ok { .. }));
    let permissions_ready = state.permissions.iter().any(|row| {
        row.permission == Permission::Accessibility && row.status == PermissionStatus::Granted
    });
    let completed = [connected, connected && permissions_ready, false];
    // The hotkey step names the *live* binding. The hardcoded "⌥Space" this
    // replaces was wrong on Windows (`Ctrl+Shift+Space`) and wrong for anyone
    // whose config.toml rebinds it — an onboarding step that teaches a
    // shortcut that does nothing is how first-run ends.
    let combo = state
        .hotkey
        .as_ref()
        .map(|status| match status {
            HotkeyStatus::Registered { combo, .. } | HotkeyStatus::Failed { combo, .. } => {
                combo.clone()
            }
        })
        .or_else(|| Binding::default_for(HotkeyAction::TogglePanel).map(|binding| binding.display))
        .unwrap_or_default();
    let labels = [
        i18n::t(Key::SettingsSetupConnect).to_owned(),
        i18n::t(Key::SettingsSetupPermissions).to_owned(),
        i18n::t1(Key::SettingsSetupTryHotkey, &combo),
    ];
    let current = completed.iter().position(|done| !done).unwrap_or(2);

    let mut steps = column![
        text(i18n::t(Key::SettingsWelcomeTitle))
            .size(type_scale::BODY)
            .style(theme::text_primary),
        text(i18n::t(Key::SettingsWelcomeBody))
            .size(type_scale::META)
            .style(theme::text_dim),
    ]
    .spacing(space(1.5));

    for (index, label) in labels.into_iter().enumerate() {
        let marker = if completed[index] {
            "✓".to_owned()
        } else {
            (index + 1).to_string()
        };
        steps = steps.push(
            row![
                text(marker)
                    .width(Length::Fixed(theme::MIN_HIT_TARGET))
                    .size(type_scale::BODY)
                    .style(if completed[index] || index == current {
                        theme::text_accent
                    } else {
                        theme::text_dim
                    }),
                text(label)
                    .size(type_scale::BODY)
                    .style(if index == current {
                        theme::text_primary
                    } else {
                        theme::text_dim
                    }),
            ]
            .height(Length::Fixed(theme::MIN_HIT_TARGET))
            .align_y(iced::Alignment::Center),
        );
    }

    container(steps)
        .width(Length::Fill)
        .padding(space(2.0))
        .style(theme::raised)
        .into()
}

fn permissions(state: &SettingsState) -> Element<'_, Message> {
    let mut list = column![].spacing(space(2.0));

    // The hotkey belongs here rather than in a picker of its own: §9 wants
    // conflict detection surfaced at first run, and this is where a user who
    // has lost ⌥Space to Raycast will come looking.
    if let Some(status) = &state.hotkey {
        // No rebind action here any more: `Message::RebindHotkey` was a
        // no-op behind a button labelled "Open settings" *inside* settings —
        // on a failed registration, the only advertised recovery did nothing.
        // The audit plan's residual-risk policy says a nonfunctional control
        // must be absent; the honest recovery is config.toml, so the failure
        // body now says so.
        list = list.push(match status {
            // §9: a shift/option-only combination gets a **soft warning**, not
            // a rejection — it is registered and working, including the shipped
            // `⌥Space` default.
            HotkeyStatus::Registered { combo, caution } => widgets::state_block(
                match caution {
                    Some(_) => Severity::Warning,
                    None => Severity::Success,
                },
                combo,
                caution.map(|c| c.explanation()),
                Vec::new(),
            ),
            HotkeyStatus::Failed { combo, reason } => {
                let body = format!(
                    "{} {}",
                    failure_body(reason),
                    i18n::t(Key::HotkeyChangeHint)
                );
                widgets::state_block(
                    Severity::Danger,
                    &i18n::t1(Key::HotkeyFailedTitle, combo),
                    Some(&body),
                    Vec::new(),
                )
            }
        });
    }

    // The rebind that used to be missing: a field in `hotkey::parse` syntax,
    // applied live through the one process-wide registrar and persisted to
    // `[ui] panel_hotkey`. The status block above reports the outcome.
    list = list.push(
        row![
            text_input("control+alt+Space", &state.hotkey_draft)
                .on_input(Message::HotkeyDraft)
                .on_submit(Message::HotkeyApply)
                .size(type_scale::BODY)
                .font(theme::MONO_FONT)
                .padding([space(1.5), space(2.0)])
                .style(theme::field),
            {
                let apply = button(
                    text(i18n::t(Key::ActionApply))
                        .size(type_scale::BODY)
                        .style(theme::text_primary),
                )
                .padding([space(1.5), space(2.0)])
                .style(theme::action_button);
                match state.hotkey_draft.trim().is_empty() {
                    true => apply,
                    false => apply.on_press(Message::HotkeyApply),
                }
            },
        ]
        .align_y(iced::Alignment::Center)
        .spacing(space(2.0)),
    );
    list = list.push(
        text(i18n::t(if state.hotkey_draft_invalid {
            Key::SettingsHotkeyInvalid
        } else {
            Key::SettingsHotkeyHint
        }))
        .size(type_scale::META)
        .style(if state.hotkey_draft_invalid {
            theme::text_primary
        } else {
            theme::text_dim
        }),
    );

    for row in &state.permissions {
        list = list.push(widgets::permission_banner(
            row.status,
            i18n::t(permission_key(row.permission)),
            Some(Message::OpenSystemSettings(row.permission)),
        ));
    }

    // §8's AX-tree opt-in, at last a control instead of a config key. The
    // body says it applies at the next start — the flag is baked into the
    // capture worker at construction, and a toggle that pretended to be live
    // would be the dishonest control this window refuses to ship.
    list = list.push(
        button(
            column![
                row![
                    text(selection_marker(state.ax_tree_activation))
                        .width(Length::Fixed(space(3.0)))
                        .size(type_scale::BODY)
                        .style(theme::text_primary),
                    text(i18n::t(Key::SettingsAxTitle))
                        .size(type_scale::BODY)
                        .style(theme::text_primary),
                ]
                .align_y(iced::Alignment::Center),
                text(i18n::t(Key::SettingsAxBody))
                    .size(type_scale::META)
                    .style(theme::text_dim),
            ]
            .spacing(space(0.5)),
        )
        .width(Length::Fill)
        .padding([space(1.5), space(2.0)])
        .style(theme::action_button)
        .on_press(Message::AxTreeToggle(!state.ax_tree_activation)),
    );

    list.into()
}

/// The supporting line for a refused hotkey.
///
/// §8 names the two messages a user actually needs — "choose different
/// modifiers" (`-9868`) and "another app already owns this shortcut" (`-9878`)
/// — and they lead to different actions, so they must not share copy. This used
/// to render `global_hotkey::Error`'s `Display` verbatim, which is one string
/// for both and is in English regardless of the UI language.
fn failure_body(reason: &FailureReason) -> &str {
    match reason {
        // "macOS does not accept this combination as a global shortcut."
        FailureReason::ModifiersRejected => i18n::t(Key::HotkeyRejectedByOs),
        // "Another app has already claimed it. Pick a different shortcut."
        FailureReason::AlreadyOwned => i18n::t(Key::HotkeyFailedBody),
        // Developer-facing and platform-specific.
        FailureReason::BreaksOsShortcut(why) => why,
        FailureReason::Unclassified(raw) => raw,
    }
}

/// Catalogue key for a user-visible OS permission name.
pub(crate) const fn permission_key(permission: Permission) -> Key {
    match permission {
        Permission::Accessibility => Key::SettingsPermissionAccessibility,
        Permission::PostEvents => Key::SettingsPermissionInputMonitoring,
        Permission::ElevatedWindowAccess => Key::SettingsPermissionElevatedWindowAccess,
        Permission::Notifications => Key::SettingsPermissionNotifications,
        Permission::Autostart => Key::SettingsPermissionAutostart,
    }
}

fn budgets(state: &SettingsState) -> Element<'_, Message> {
    // §14: BYOK means the user pays for every mistake aibo makes, so the meter
    // is the first thing in the section, not a footnote.
    let mut list = column![widgets::spend_meter::<Message>(
        &state.spend_label,
        state.spend_fraction
    )]
    .spacing(space(2.0));

    list = list.push(
        text(i18n::t(Key::SettingsBudgetHint))
            .size(type_scale::META)
            .style(theme::text_dim),
    );
    list = list.push(
        row![
            text(i18n::t(Key::SettingsBudgetLimitLabel))
                .size(type_scale::BODY)
                .style(theme::text_primary)
                .width(Length::Fill),
            text_input("20", &state.budget_limit_draft)
                .on_input(Message::BudgetLimitDraft)
                .size(type_scale::BODY)
                .font(theme::MONO_FONT)
                .width(Length::Fixed(120.0))
                .padding([space(1.5), space(2.0)])
                .style(theme::field),
        ]
        .align_y(iced::Alignment::Center)
        .spacing(space(2.0)),
    );
    list = list.push(
        row![
            text(i18n::t(Key::SettingsBudgetWarnLabel))
                .size(type_scale::BODY)
                .style(theme::text_primary)
                .width(Length::Fill),
            text_input("80", &state.budget_warn_draft)
                .on_input(Message::BudgetWarnDraft)
                .size(type_scale::BODY)
                .font(theme::MONO_FONT)
                .width(Length::Fixed(120.0))
                .padding([space(1.5), space(2.0)])
                .style(theme::field),
        ]
        .align_y(iced::Alignment::Center)
        .spacing(space(2.0)),
    );
    list = list.push(
        button(
            row![
                text(selection_marker(state.budget_hard_stop))
                    .width(Length::Fixed(space(3.0)))
                    .size(type_scale::BODY)
                    .style(theme::text_primary),
                text(i18n::t(Key::SettingsBudgetHardStop))
                    .size(type_scale::BODY)
                    .style(theme::text_primary),
            ]
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding([space(1.5), space(2.0)])
        .style(theme::action_button)
        .on_press(Message::BudgetHardStop(!state.budget_hard_stop)),
    );

    let mut actions = row![].spacing(space(2.0));
    let apply = button(
        text(i18n::t(Key::ActionApply))
            .size(type_scale::BODY)
            .style(theme::text_primary),
    )
    .padding([space(1.5), space(2.0)])
    .style(theme::action_button);
    actions = actions.push(if parsed_budget(state).is_some() {
        apply.on_press(Message::BudgetApply)
    } else {
        apply
    });
    if state.budget_configured {
        actions = actions.push(
            button(
                text(i18n::t(Key::ActionRemoveBudget))
                    .size(type_scale::BODY)
                    .style(theme::text_dim),
            )
            .padding([space(1.5), space(2.0)])
            .style(theme::action_button)
            .on_press(Message::BudgetRemove),
        );
    }
    list.push(actions).into()
}

/// The drafted budget, if the drafts parse: (limit_micros, warn_at_percent).
///
/// The ceiling is typed in whole currency units and held in micros, §14's
/// unit. Zero is not a budget — "spend nothing" is the file's `hard_stop`
/// with a tiny limit, and a `0` here almost always means a cleared field.
pub fn parsed_budget(state: &SettingsState) -> Option<(u64, u8)> {
    let limit: f64 = state.budget_limit_draft.trim().parse().ok()?;
    if !limit.is_finite() || limit <= 0.0 || limit > 1_000_000.0 {
        return None;
    }
    let warn: u8 = match state.budget_warn_draft.trim() {
        "" => 80,
        raw => raw.parse().ok()?,
    };
    if warn == 0 || warn > 100 {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded above and positive"
    )]
    Some(((limit * 1_000_000.0).round() as u64, warn))
}

/// The `@` finder's search roots (§P9+): the walk indexes exactly these.
fn files(state: &SettingsState) -> Element<'_, Message> {
    let mut list = column![
        text(i18n::t(Key::SettingsFilesHint))
            .size(type_scale::META)
            .style(theme::text_dim),
    ]
    .spacing(space(1.0));

    let (roots, customised): (&[String], bool) = match &state.file_roots {
        Some(roots) => (roots, true),
        None => (&state.default_file_roots, false),
    };
    for (index, root) in roots.iter().enumerate() {
        let mut row = row![
            text(root)
                .size(type_scale::BODY)
                .font(theme::MONO_FONT)
                .style(theme::text_primary)
                .width(Length::Fill),
        ]
        .align_y(iced::Alignment::Center)
        .spacing(space(2.0));
        if !customised {
            row = row.push(
                text(i18n::t(Key::SettingsFilesDefaultBadge))
                    .size(type_scale::META)
                    .style(theme::text_dim),
            );
        }
        row = row.push(
            button(text("✕").size(type_scale::BODY).style(theme::text_dim))
                .padding([space(1.0), space(1.5)])
                .style(theme::action_button)
                .on_press(Message::RootRemove(index)),
        );
        list = list.push(
            container(row)
                .width(Length::Fill)
                .padding([space(1.0), space(2.0)]),
        );
    }

    list = list.push(
        row![
            text_input(
                i18n::t(Key::SettingsFilesRootPlaceholder),
                &state.root_draft
            )
            .on_input(Message::RootDraft)
            .on_submit(Message::RootAdd)
            .size(type_scale::BODY)
            .font(theme::MONO_FONT)
            .padding([space(1.5), space(2.0)])
            .style(theme::field),
            button(
                text(i18n::t(Key::ActionAddRoot))
                    .size(type_scale::BODY)
                    .style(theme::text_primary),
            )
            .padding([space(1.5), space(2.0)])
            .style(theme::action_button)
            .on_press_maybe((!state.root_draft.trim().is_empty()).then_some(Message::RootAdd)),
        ]
        .align_y(iced::Alignment::Center)
        .spacing(space(2.0)),
    );
    if customised {
        list = list.push(
            button(
                text(i18n::t(Key::ActionResetDefaults))
                    .size(type_scale::BODY)
                    .style(theme::text_dim),
            )
            .padding([space(1.5), space(2.0)])
            .style(theme::action_button)
            .on_press(Message::RootsReset),
        );
    }
    list.into()
}

/// The dictation source (`[stt] backend`), as a three-way choice.
fn dictation(state: &SettingsState) -> Element<'_, Message> {
    let mut list = column![].spacing(space(1.0));
    for choice in SttChoice::ALL {
        let selected = choice == state.stt_backend;
        list = list.push(
            button(
                row![
                    text(selection_marker(selected))
                        .width(Length::Fixed(space(3.0)))
                        .size(type_scale::BODY)
                        .style(theme::text_primary),
                    column![
                        text(i18n::t(choice.label()))
                            .size(type_scale::BODY)
                            .style(if selected {
                                theme::text_primary
                            } else {
                                theme::text_dim
                            }),
                        text(i18n::t(choice.detail()))
                            .size(type_scale::META)
                            .style(theme::text_faint),
                    ]
                    .spacing(space(0.25)),
                ]
                .align_y(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .padding([space(1.5), space(2.0)])
            .style(if selected {
                theme::selected_button
            } else {
                theme::action_button
            })
            .on_press(Message::SetSttBackend(choice)),
        );
    }

    // ⏎ ends the turn (owner, 2026-08-03): same control shape as the AX
    // opt-in — a checkmark row with its explanation underneath.
    list = list.push(
        button(
            column![
                row![
                    text(selection_marker(state.stt_end_on_send))
                        .width(Length::Fixed(space(3.0)))
                        .size(type_scale::BODY)
                        .style(theme::text_primary),
                    text(i18n::t(Key::SttEndOnSendTitle))
                        .size(type_scale::BODY)
                        .style(theme::text_primary),
                ]
                .align_y(iced::Alignment::Center),
                text(i18n::t(Key::SttEndOnSendBody))
                    .size(type_scale::META)
                    .style(theme::text_dim),
            ]
            .spacing(space(0.5)),
        )
        .width(Length::Fill)
        .padding([space(1.5), space(2.0)])
        .style(theme::action_button)
        .on_press(Message::SttEndOnSendToggle(!state.stt_end_on_send)),
    );

    list.into()
}

/// The dictation backend, as the settings window models it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SttChoice {
    /// Prefer the OpenAI key, fall back to the ChatGPT plan.
    #[default]
    Auto,
    /// The realtime API with the OpenAI key — streaming, word by word.
    OpenAi,
    /// The ChatGPT plan's transcription endpoint via the Codex sign-in —
    /// the text arrives when the turn ends.
    ChatGpt,
    /// An Azure Foundry deployment of the same live model — streaming, with
    /// a batch fallback (owner request, 2026-08-03).
    Azure,
}

impl SttChoice {
    /// Every choice, in display order.
    pub const ALL: [SttChoice; 4] = [
        SttChoice::Auto,
        SttChoice::OpenAi,
        SttChoice::ChatGpt,
        SttChoice::Azure,
    ];

    /// The `[stt] backend` spelling; `None` is auto.
    pub fn tag(self) -> Option<&'static str> {
        match self {
            SttChoice::Auto => None,
            SttChoice::OpenAi => Some("openai"),
            SttChoice::ChatGpt => Some("chatgpt"),
            SttChoice::Azure => Some("azure"),
        }
    }

    /// Parse the config spelling; unknown values read as auto.
    pub fn from_tag(tag: Option<&str>) -> SttChoice {
        match tag {
            Some("openai") => SttChoice::OpenAi,
            Some("chatgpt") => SttChoice::ChatGpt,
            Some("azure") => SttChoice::Azure,
            _ => SttChoice::Auto,
        }
    }

    /// The row's title.
    pub const fn label(self) -> Key {
        match self {
            SttChoice::Auto => Key::SttAuto,
            SttChoice::OpenAi => Key::SttOpenAi,
            SttChoice::ChatGpt => Key::SttChatGpt,
            SttChoice::Azure => Key::SttAzure,
        }
    }

    /// The row's one-line explanation.
    pub const fn detail(self) -> Key {
        match self {
            SttChoice::Auto => Key::SttAutoDetail,
            SttChoice::OpenAi => Key::SttOpenAiDetail,
            SttChoice::ChatGpt => Key::SttChatGptDetail,
            SttChoice::Azure => Key::SttAzureDetail,
        }
    }
}

/// The appearance selector: System, Dark, Light — same shape as the
/// language list below (owner request, 2026-08-03: "a theme selector, and
/// also darkmode toggle").
fn appearance(state: &SettingsState) -> Element<'_, Message> {
    let mut list = column![].spacing(space(1.0));
    for preference in theme::AppearancePreference::ALL {
        let selected = preference == state.appearance;
        list = list.push(
            button(
                row![
                    text(selection_marker(selected))
                        .width(Length::Fixed(space(3.0)))
                        .size(type_scale::BODY)
                        .style(theme::text_primary),
                    text(i18n::t(preference.label()))
                        .size(type_scale::BODY)
                        .style(if selected {
                            theme::text_primary
                        } else {
                            theme::text_dim
                        }),
                ]
                .align_y(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fixed(theme::MIN_HIT_TARGET))
            .padding([space(1.5), space(2.0)])
            .style(if selected {
                theme::selected_button
            } else {
                theme::action_button
            })
            .on_press(Message::SetAppearance(preference)),
        );
    }
    list.into()
}

fn language(state: &SettingsState) -> Element<'_, Message> {
    let mut list = column![].spacing(space(1.0));
    for lang in Lang::ALL {
        let selected = *lang == state.language;
        list = list.push(
            button(
                row![
                    text(selection_marker(selected))
                        .width(Length::Fixed(space(3.0)))
                        .size(type_scale::BODY)
                        .style(theme::text_primary),
                    text(lang.endonym())
                        .size(type_scale::BODY)
                        .style(if selected {
                            theme::text_primary
                        } else {
                            theme::text_dim
                        }),
                ]
                .align_y(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fixed(theme::MIN_HIT_TARGET))
            .padding([space(1.5), space(2.0)])
            .style(if selected {
                theme::selected_button
            } else {
                theme::action_button
            })
            .on_press(Message::SetLanguage(*lang)),
        );
    }
    list.into()
}

const fn selection_marker(selected: bool) -> &'static str {
    if selected { "✓" } else { "" }
}

fn about<'a>(_state: &'a SettingsState) -> Element<'a, Message> {
    column![
        text(i18n::t(Key::AppName))
            .size(type_scale::HEADING)
            .style(theme::text_primary),
        text(env!("CARGO_PKG_VERSION"))
            .size(type_scale::META)
            .style(theme::text_dim),
        widgets::action_list(vec![Action::new(
            Key::ActionCopyDiagnostics,
            widgets::primary_shortcut("⌘C", "Ctrl+C"),
            Message::CopyDiagnostics,
        )]),
    ]
    .spacing(space(2.0))
    .into()
}

/// Roles the settings UI can bind (§4). Exposed so the picker cannot fall out
/// of step with the router's enum.
pub const BINDABLE_ROLES: [Role; 5] = [
    Role::Fast,
    Role::Smart,
    Role::Cheap,
    Role::Vision,
    Role::Agent,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every backend the picker offers must be a spelling `config.toml`'s
    /// loader accepts.
    ///
    /// This is the seam where a typo is invisible: the UI writes
    /// `backend = "open-router"`, the write succeeds, the engine rebuild then
    /// fails to parse the file, and the user sees a provider that vanishes
    /// rather than an error. `aibo-ui` cannot depend on `aibo-session`, so the
    /// spellings cannot be shared — which is exactly why they must be asserted.
    #[test]
    fn every_offered_backend_is_a_spelling_the_config_loader_accepts() {
        // Kept in sync by hand with `aibo_session::Backend`'s serde
        // `rename_all = "kebab-case"` derivation.
        const ACCEPTED: &[&str] = &[
            "cerebras",
            "samba-nova",
            "groq",
            "xai",
            "open-router",
            "gemini",
            "open-ai",
            "open-ai-chat-completions",
            "anthropic",
            "azure",
            "ollama",
            "custom",
        ];
        for backend in Backend::ALL {
            assert!(
                ACCEPTED.contains(&backend.config_value()),
                "{backend:?} writes backend = {:?}, which the loader would reject",
                backend.config_value()
            );
        }
    }

    /// A draft is only saveable once it can actually produce a working
    /// provider. §17: an action that is offered must work.
    #[test]
    fn a_custom_endpoint_needs_a_name_and_a_url_before_it_can_be_saved() {
        let mut draft = ProviderDraft::new(Backend::Custom);
        draft.set_key("sk-test".to_owned());
        assert!(!draft.is_saveable(), "no name, no url");

        draft.id = "deepseek".to_owned();
        assert!(!draft.is_saveable(), "still no url");

        draft.base_url = "https://api.deepseek.com/v1".to_owned();
        assert!(draft.is_saveable());
    }

    /// A named backend has its URL compiled in, so a key alone is enough.
    #[test]
    fn a_known_backend_needs_only_a_key() {
        let mut draft = ProviderDraft::new(Backend::OpenRouter);
        assert!(
            !draft.is_saveable(),
            "a provider with no key is not saveable"
        );
        draft.set_key("sk-or-test".to_owned());
        assert!(draft.is_saveable());
    }

    /// §12: taking the key must leave the draft holding nothing.
    #[test]
    fn taking_the_key_empties_the_draft() {
        let mut draft = ProviderDraft::new(Backend::Groq);
        draft.set_key("gsk-secret".to_owned());

        let taken = draft.take_key();
        assert_eq!(taken.expose_secret(), "gsk-secret");
        assert!(draft.key_field().is_empty(), "the draft must not retain it");
        assert!(
            !draft.is_saveable(),
            "and must not look saveable afterwards"
        );
    }

    /// The key must not reach a log line through the type's own `Debug`.
    #[test]
    fn debug_output_never_contains_the_key() {
        let mut draft = ProviderDraft::new(Backend::Anthropic);
        draft.set_key("sk-ant-do-not-log-me".to_owned());
        // `ProviderDraft` derives `Debug` for diagnostics, so this is the
        // check that the derive was not the wrong call.
        assert!(
            !format!("{draft:?}").contains("do-not-log-me"),
            "the key reached Debug output"
        );
    }

    #[test]
    fn a_budget_draft_parses_in_currency_units_and_rejects_nonsense() {
        let mut state = SettingsState {
            budget_limit_draft: "15".to_owned(),
            ..SettingsState::default()
        };
        assert_eq!(
            parsed_budget(&state),
            Some((15_000_000, 80)),
            "warn defaults to §14's 80"
        );

        state.budget_limit_draft = "0.5".to_owned();
        state.budget_warn_draft = "50".to_owned();
        assert_eq!(parsed_budget(&state), Some((500_000, 50)));

        for bad in ["", "0", "-3", "abc", "1e99", "NaN"] {
            state.budget_limit_draft = bad.to_owned();
            assert_eq!(parsed_budget(&state), None, "{bad:?} is not a ceiling");
        }
        state.budget_limit_draft = "10".to_owned();
        for bad in ["0", "101", "x"] {
            state.budget_warn_draft = bad.to_owned();
            assert_eq!(parsed_budget(&state), None, "{bad:?} is not a percent");
        }
    }

    #[test]
    fn the_information_architecture_matches_section_16() {
        // §16 names: providers, roles, budgets, permissions, actions, history,
        // about/license. Language is the §9 addition; Models and Files are the
        // owner's 2026-08-01 additions (quick-pick pins, @ finder roots);
        // Dictation is the owner's 2026-08-02 addition (STT method);
        // Appearance is the owner's 2026-08-03 addition (dark/light/system).
        // Unfinished editors stay in the durable enum without creating
        // dead-end navigation.
        assert_eq!(Section::ALL.len(), 12);
        assert_eq!(Section::VISIBLE.len(), 10);
        assert!(Section::VISIBLE.contains(&Section::Models));
        assert!(Section::VISIBLE.contains(&Section::Files));
        assert!(Section::VISIBLE.contains(&Section::Dictation));
        assert!(Section::VISIBLE.contains(&Section::Appearance));
        assert!(!Section::VISIBLE.contains(&Section::Roles));
        assert!(!Section::VISIBLE.contains(&Section::Actions));
        assert_eq!(Section::default(), Section::Providers);
        for section in Section::ALL {
            assert!(!i18n::lookup(section.title(), Lang::En).is_empty());
            assert!(!i18n::lookup(section.title(), Lang::Ja).is_empty());
        }
    }

    /// Regression, F6. `-9868` and `-9878` used to arrive as
    /// `global_hotkey::Error`'s `Display` string and render identically, so
    /// "choose different modifiers" and "another app owns this shortcut" — the
    /// two messages §8 says a user actually needs — were indistinguishable.
    #[test]
    fn the_two_failure_reasons_render_different_copy() {
        let modifiers = failure_body(&FailureReason::ModifiersRejected);
        let owned = failure_body(&FailureReason::AlreadyOwned);

        assert!(!modifiers.is_empty());
        assert!(!owned.is_empty());
        assert_ne!(modifiers, owned);

        // Both come from the catalogue, so both follow the UI language.
        for lang in Lang::ALL {
            i18n::set_language(*lang);
            assert_ne!(
                failure_body(&FailureReason::ModifiersRejected),
                failure_body(&FailureReason::AlreadyOwned)
            );
            assert_eq!(
                failure_body(&FailureReason::ModifiersRejected),
                i18n::lookup(Key::HotkeyRejectedByOs, *lang)
            );
            assert_eq!(
                failure_body(&FailureReason::AlreadyOwned),
                i18n::lookup(Key::HotkeyFailedBody, *lang)
            );
        }
        i18n::set_language(Lang::default());
    }

    // -- Codex sign-in (§3a) ------------------------------------------------

    /// **The blocker, from the UI side.** Before this card existed the
    /// Providers tab offered no way to sign in, so a user holding only a
    /// ChatGPT subscription could reach `aibo-provider`'s verified device flow
    /// from nowhere in the app. An empty provider list must still render an
    /// actionable sign-in, not just §13's dead-end "no provider configured".
    #[test]
    fn a_fresh_install_still_offers_a_way_in() {
        let state = SettingsState::default();
        assert!(state.providers.is_empty());

        let (phase, detail) = CodexPhase::read(&Health::Unknown);
        assert_eq!(phase, CodexPhase::SignedOut);
        assert!(!detail.is_empty());
        // The card is rendered, and it is rendered with a live action.
        let _ = providers(&state);
        let _ = codex_card(&state);
    }

    /// The single action must always state what it does: one press means
    /// "sign in", "cancel" or "sign out" depending on the phase, and the label
    /// is what distinguishes them.
    #[test]
    fn the_one_button_says_which_of_the_three_things_it_does() {
        let keys = [
            (CodexPhase::SignedOut, Key::SettingsCodexSignIn),
            (CodexPhase::Starting, Key::SettingsCodexCancelSignIn),
            (CodexPhase::AwaitingApproval, Key::SettingsCodexCancelSignIn),
            (CodexPhase::Exchanging, Key::SettingsCodexCancelSignIn),
            (CodexPhase::SignedIn, Key::SettingsCodexSignOut),
            (CodexPhase::Failed, Key::SettingsCodexSignIn),
        ];
        for (phase, expected) in keys {
            let state = SettingsState {
                providers: vec![ProviderRow {
                    id: ProviderId::CODEX,
                    configured: phase == CodexPhase::SignedIn,
                    health: phase.to_health("detail"),
                }],
                ..SettingsState::default()
            };
            let (read, _) = CodexPhase::read(&state.providers[0].health);
            assert_eq!(read, phase, "{phase:?} did not survive the health channel");
            assert_eq!(codex_action_key(read), expected);
            assert_eq!(codex_action_label(read), i18n::t(expected));
            let _ = codex_card(&state);
        }
        // …and the three labels are genuinely different strings, so "sign in"
        // can never be pressed when it would in fact sign the user out.
        assert_ne!(Key::SettingsCodexSignIn, Key::SettingsCodexSignOut);
        assert_ne!(Key::SettingsCodexSignIn, Key::SettingsCodexCancelSignIn);
        assert_ne!(Key::SettingsCodexCancelSignIn, Key::SettingsCodexSignOut);
    }

    #[test]
    fn every_codex_phase_has_localised_default_detail() {
        for phase in [
            CodexPhase::SignedOut,
            CodexPhase::Starting,
            CodexPhase::AwaitingApproval,
            CodexPhase::Exchanging,
            CodexPhase::SignedIn,
            CodexPhase::Failed,
        ] {
            let key = codex_default_detail_key(phase);
            assert!(!codex_default_detail(phase).is_empty());
            assert_ne!(
                i18n::lookup(key, Lang::En),
                i18n::lookup(key, Lang::Ja),
                "{phase:?} was left as hard-coded English"
            );
        }
    }

    /// The phase channel must round-trip the sentence the backend wrote,
    /// including a user code and a URL — that sentence is the *only* place the
    /// user sees either of them.
    #[test]
    fn the_user_code_and_verification_url_survive_the_round_trip() {
        let detail = "Enter code ABCD-1234 at https://auth.openai.com/codex/device \
                      — waiting for approval (9m 45s left)";
        let health = CodexPhase::AwaitingApproval.to_health(detail);
        let (phase, read) = CodexPhase::read(&health);

        assert_eq!(phase, CodexPhase::AwaitingApproval);
        assert_eq!(read, detail);
        assert!(read.contains("ABCD-1234"));
        assert!(read.contains("https://auth.openai.com/codex/device"));
        // The tag must not leak into what the user reads.
        assert!(!read.contains("awaiting"));
        assert!(!read.contains(CODEX_MARKER));
    }

    /// A health that carries no tag — an ordinary §13 probe result — must still
    /// render as something actionable. Reading a revoked token as "signed out"
    /// would hide the one fact the user needs.
    #[test]
    fn an_untagged_health_is_read_honestly_rather_than_as_signed_out() {
        let (phase, detail) = CodexPhase::read(&Health::Degraded {
            reason: "sign-in required (revoked)".to_owned(),
            consecutive_failures: 3,
        });
        assert_eq!(phase, CodexPhase::Failed);
        assert_eq!(detail, "sign-in required (revoked)");

        let (phase, _) = CodexPhase::read(&Health::Unavailable {
            reason: "connect failed".to_owned(),
        });
        assert_eq!(phase, CodexPhase::Failed);

        let (phase, _) = CodexPhase::read(&Health::Ok {
            latency: std::time::Duration::from_millis(435),
        });
        assert_eq!(
            phase,
            CodexPhase::SignedIn,
            "a working Codex probe means a live token pair"
        );
    }

    /// `SignedIn` deliberately carries no sentence of its own, and the test
    /// says so rather than passing because the caller happened to hand over the
    /// canonical string. Any *other* phase must carry its detail verbatim.
    #[test]
    fn signed_in_reports_health_rather_than_a_sentence() {
        let health = CodexPhase::SignedIn.to_health("account acct_42, plan pro");
        assert!(
            matches!(health, Health::Ok { .. }),
            "a working provider must not be published as Degraded so a string can ride along"
        );

        let (phase, detail) = CodexPhase::read(&health);
        assert_eq!(phase, CodexPhase::SignedIn);
        assert_eq!(
            detail,
            i18n::t(Key::SettingsCodexSignedIn),
            "Health::Ok has nowhere to put a sentence, so the canonical copy is what shows"
        );
        assert!(!detail.contains("acct_42"));

        // Every phase that *is* Degraded does keep its sentence.
        for phase in [
            CodexPhase::SignedOut,
            CodexPhase::Starting,
            CodexPhase::AwaitingApproval,
            CodexPhase::Exchanging,
            CodexPhase::Failed,
        ] {
            let unique = format!("sentence for {}", phase.tag());
            let (read_phase, read_detail) = CodexPhase::read(&phase.to_health(&unique));
            assert_eq!(read_phase, phase);
            assert_eq!(read_detail, unique);
        }
    }

    /// §13's blocking state must not swallow the card, and must disappear once
    /// something usable exists.
    #[test]
    fn the_no_provider_block_yields_once_codex_is_signed_in() {
        let signed_in = SettingsState {
            providers: vec![ProviderRow {
                id: ProviderId::CODEX,
                configured: true,
                health: CodexPhase::SignedIn.to_health("signed in"),
            }],
            ..SettingsState::default()
        };
        let (phase, _) = CodexPhase::read(&signed_in.providers[0].health);
        assert_eq!(phase, CodexPhase::SignedIn);
        assert!(matches!(signed_in.providers[0].health, Health::Ok { .. }));
        let _ = providers(&signed_in);
    }

    /// Regression, F5. A shift/option-only combination registers, so it is a
    /// `Warning` on a live shortcut — never `Danger`, and never a `Failed`
    /// state. The shipped macOS default lands here.
    #[test]
    fn the_shift_or_option_caution_is_soft() {
        use crate::hotkey::Caution;

        let status = HotkeyStatus::Registered {
            combo: "⌥Space".to_owned(),
            caution: Some(Caution::ShiftOrOptionOnly),
        };
        let HotkeyStatus::Registered { caution, .. } = &status else {
            panic!("a caution never produces a Failed status");
        };
        assert_eq!(*caution, Some(Caution::ShiftOrOptionOnly));
        assert!(!Caution::ShiftOrOptionOnly.explanation().is_empty());

        // And it renders: the permissions section must not silently drop it.
        let state = SettingsState {
            section: Section::Permissions,
            hotkey: Some(status),
            ..SettingsState::default()
        };
        let _ = permissions(&state);
    }

    #[test]
    fn selected_choices_have_a_non_colour_marker() {
        assert_eq!(selection_marker(true), "✓");
        assert_eq!(selection_marker(false), "");
    }

    #[test]
    fn permission_names_follow_the_active_language() {
        for permission in [
            Permission::Accessibility,
            Permission::PostEvents,
            Permission::ElevatedWindowAccess,
            Permission::Notifications,
            Permission::Autostart,
        ] {
            let key = permission_key(permission);
            assert_ne!(
                i18n::lookup(key, Lang::En),
                i18n::lookup(key, Lang::Ja),
                "{permission:?} was left as hard-coded English"
            );
        }
    }

    #[test]
    fn device_code_is_selectable_but_not_editable() {
        let mut state = SettingsState {
            providers: vec![ProviderRow {
                id: ProviderId::CODEX,
                configured: false,
                health: CodexPhase::AwaitingApproval
                    .to_health("Enter code ABCD-1234 at the approval page"),
            }],
            ..SettingsState::default()
        };
        state.sync_device_code();
        state.perform_device_code_action(text_editor::Action::SelectAll);
        assert_eq!(
            state.device_code_editor.selection().as_deref(),
            Some("ABCD-1234")
        );
        state.perform_device_code_action(text_editor::Action::Edit(text_editor::Edit::Insert('X')));
        assert_eq!(state.device_code_editor.text(), "ABCD-1234");
    }

    #[test]
    fn history_recovery_code_is_redacted_from_debug_output() {
        let secret = "alpha-bravo-charlie-delta";
        let state = SettingsState {
            section: Section::History,
            history_ready: true,
            recovery_code: Some(SecretString::from(secret.to_owned())),
            ..SettingsState::default()
        };

        assert!(
            !format!("{state:?}").contains(secret),
            "a debug or panic report must not disclose a recovery code"
        );
        let _ = history(&state);
    }
}
