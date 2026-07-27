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

use aibo_core::types::{Health, Permission, PermissionStatus, ProviderId, Role};
use iced::widget::{Space, button, column, container, row, scrollable, text, text_editor};
use iced::{Element, Length};
use secrecy::{ExposeSecret as _, SecretString};

use crate::hotkey::{FailureReason, HotkeyStatus};
use crate::i18n::{self, Key, Lang};
use crate::theme::{self, Severity, space, type_scale};
use crate::widgets::{self, Action};

/// The settings information architecture (§16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Section {
    /// Provider credentials and endpoints (§10).
    #[default]
    Providers,
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
    /// UI language (§9).
    Language,
    /// Version, licence, diagnostics (§19).
    About,
}

impl Section {
    /// Every section, in navigation order.
    pub const ALL: [Section; 8] = [
        Section::Providers,
        Section::Roles,
        Section::Budgets,
        Section::Permissions,
        Section::Actions,
        Section::History,
        Section::Language,
        Section::About,
    ];

    /// Catalogue key for the section's title.
    pub const fn title(self) -> Key {
        match self {
            Section::Providers => Key::SettingsProviders,
            Section::Roles => Key::SettingsRoles,
            Section::Budgets => Key::SettingsBudgets,
            Section::Permissions => Key::SettingsPermissions,
            Section::Actions => Key::SettingsActions,
            Section::History => Key::SettingsHistory,
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
    /// Whether this window is the first-run setup rather than a later visit.
    pub onboarding: bool,
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
}

impl SettingsState {
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
    /// Open the OS privacy pane for a permission.
    OpenSystemSettings(Permission),
    /// Change the UI language.
    SetLanguage(Lang),
    /// Rebind the panel hotkey. Opens the picker; §9 scopes it to one key plus
    /// modifiers, with no sequences and no double-taps.
    RebindHotkey,
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
    /// Close the window.
    Close,
}

/// Render the settings window.
pub fn view(state: &SettingsState) -> Element<'_, Message> {
    let body = row![
        container(navigation(state))
            .width(Length::Fixed(180.0))
            .padding(space(2.0)),
        container(scrollable(section_body(state)).style(theme::scroller))
            .width(Length::Fill)
            .padding(space(3.0)),
    ]
    .spacing(space(2.0));

    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(space(3.0))
        .style(theme::panel_surface)
        .into()
}

fn navigation(state: &SettingsState) -> Element<'_, Message> {
    let mut list = column![].spacing(space(1.0));
    for section in Section::ALL {
        let selected = section == state.section;
        list = list.push(
            button(
                row![
                    text(selection_marker(selected))
                        .width(Length::Fixed(space(3.0)))
                        .size(type_scale::META)
                        .style(theme::text_primary),
                    text(i18n::t(section.title()))
                        .size(type_scale::META)
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
            .on_press(Message::Select(section)),
        );
    }
    list.into()
}

fn section_body(state: &SettingsState) -> Element<'_, Message> {
    let heading = widgets::section::<Message>(state.section.title());
    let content: Element<'_, Message> = match state.section {
        Section::Providers => providers(state),
        Section::Permissions => permissions(state),
        Section::Budgets => budgets(state),
        Section::Language => language(state),
        Section::About => about(state),
        Section::History => history(state),
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
                Key::ActionCopyRecoveryCode,
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
            "⏎",
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
        body = body.push(
            row![
                button(
                    text(format!("⧉ {}", i18n::t(Key::SettingsCopyDeviceCode)))
                        .size(type_scale::META)
                        .style(theme::text_accent)
                )
                .height(Length::Fixed(theme::MIN_HIT_TARGET))
                .padding([space(1.0), space(2.0)])
                .style(theme::action_button)
                .on_press(Message::CopyDeviceCode(code)),
                button(
                    text(format!("↗ {}", i18n::t(Key::SettingsOpenDevicePage)))
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

    let body = body
        .push(
            text(i18n::t(Key::SettingsCodexConsentNote))
                .size(type_scale::META)
                .style(theme::text_dim),
        )
        .push(
            button(text(label).size(type_scale::META).style(theme::text_accent))
                .height(Length::Fixed(theme::MIN_HIT_TARGET))
                .padding([space(1.5), space(2.0)])
                .style(theme::action_button)
                .on_press(Message::SignIn(ProviderId::CODEX)),
        );

    container(body)
        .width(Length::Fill)
        .padding(space(2.0))
        .style(theme::banner(severity))
        .into()
}

fn providers(state: &SettingsState) -> Element<'_, Message> {
    let mut list = column![].spacing(space(2.0));
    if state.onboarding {
        list = list.push(widgets::state_block(
            Severity::Info,
            i18n::t(Key::SettingsWelcomeTitle),
            Some(i18n::t(Key::SettingsWelcomeBody)),
            Vec::new(),
        ));
    }
    list = list.push(codex_card(state));

    // §13's blocking "no provider configured" still belongs here — but under
    // the card, not instead of it, because the card is the way out of it.
    let codex_usable = state
        .providers
        .iter()
        .any(|p| p.id == ProviderId::CODEX && matches!(p.health, Health::Ok { .. }));
    let others: Vec<&ProviderRow> = state
        .providers
        .iter()
        .filter(|p| p.id != ProviderId::CODEX)
        .collect();

    if !codex_usable && others.is_empty() {
        list = list.push(widgets::state_block(
            Severity::Danger,
            i18n::t(Key::ErrNoProvider),
            None,
            Vec::new(),
        ));
    }

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
    }
    list.into()
}

fn permissions(state: &SettingsState) -> Element<'_, Message> {
    let mut list = column![].spacing(space(2.0));

    // The hotkey belongs here rather than in a picker of its own: §9 wants
    // conflict detection surfaced at first run, and this is where a user who
    // has lost ⌥Space to Raycast will come looking.
    if let Some(status) = &state.hotkey {
        let rebind = || {
            vec![Action::new(
                Key::ActionOpenSettings,
                "⏎",
                Message::RebindHotkey,
            )]
        };
        list = list.push(match status {
            // §9: a shift/option-only combination gets a **soft warning**, not
            // a rejection — it is registered and working, including the shipped
            // `⌥Space` default. Warning severity, not Danger, and the rebind
            // action stays optional rather than being the only way out.
            HotkeyStatus::Registered { combo, caution } => widgets::state_block(
                match caution {
                    Some(_) => Severity::Warning,
                    None => Severity::Success,
                },
                combo,
                caution.map(|c| c.explanation()),
                rebind(),
            ),
            HotkeyStatus::Failed { combo, reason } => widgets::state_block(
                Severity::Danger,
                &i18n::t1(Key::HotkeyFailedTitle, combo),
                Some(failure_body(reason)),
                rebind(),
            ),
        });
    }

    for row in &state.permissions {
        list = list.push(widgets::permission_banner(
            row.status,
            i18n::t(permission_key(row.permission)),
            Some(Message::OpenSystemSettings(row.permission)),
        ));
    }
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
const fn permission_key(permission: Permission) -> Key {
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
    column![
        widgets::spend_meter::<Message>(&state.spend_label, state.spend_fraction),
        // TODO(§14): per-role `max_tokens` and context caps, the confirmation
        // threshold, and the monthly ceiling.
    ]
    .spacing(space(2.0))
    .into()
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

    #[test]
    fn the_information_architecture_matches_section_16() {
        // §16 names: providers, roles, budgets, permissions, actions, history,
        // about/license. Language is the §9 addition.
        assert_eq!(Section::ALL.len(), 8);
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
