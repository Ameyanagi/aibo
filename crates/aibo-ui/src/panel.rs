//! The transient overlay panel; tolerates context arriving late, empty or never (§8).
//!
//! The panel is the product. It is also the surface with the tightest budget —
//! ≤ 80 ms to visible (§15, **S3**) and ≤ 250 ms to first token for Complete
//! (§1) — which is why it is *pre-created hidden* at startup and why showing it
//! does only position + show + focus (§6, the cold-start trick).
//!
//! §16 is explicit that "one polished mock is not a design system": every
//! surface needs loading, streaming, error, empty, permission-denied,
//! context-unavailable, truncated-output and long-output-with-scroll. All eight
//! live in [`Phase`] / [`ContextState`] here and all eight render in [`view`].
//!
//! Two invariants from §13 are enforced by the *types*, not by care:
//!
//! * A partial or cancelled response can never be accepted — [`PanelState::can_accept`]
//!   is false unless the stream ended cleanly, and it is what gates the Replace
//!   action.
//! * `AiboError::Internal` never reaches the screen raw. Errors are converted
//!   into [`ErrorView`] at the boundary, and that conversion drops the source.

use std::sync::Arc;

use aibo_core::error::Treatment;
use aibo_core::types::{AppInfo, PermissionStatus, ProviderId, Rect, Role, StopReason, Surface};
use aibo_core::{AiboError, types::Usage};
use iced::widget::{Space, column, container, row, text, text_input};
use iced::{Alignment, Element, Length};

use crate::bridge::SessionId;
use crate::i18n::{self, Key};
use crate::theme::{self, Severity, space, type_scale};
use crate::widgets::{self, Action};

/// The id of the panel's text input, so focus can be requested on show.
pub const INPUT_ID: &str = "aibo.panel.input";

/// Where the panel is in its lifecycle.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Phase {
    /// Created, painted once, hidden. The steady state (§6).
    #[default]
    Hidden,

    /// The throwaway first frame.
    ///
    /// §6: a window created on hotkey press costs surface creation plus
    /// first-frame pipeline compile and will miss the budget. So the panel is
    /// created hidden at startup and rendered *while hidden* — `iced_winit`
    /// always creates windows with `with_visible(false)` and then flips them,
    /// and winit's macOS backend drives redraws off its own queue rather than
    /// `drawRect:`, so a hidden window is genuinely painted.
    ///
    /// The warm-up view deliberately instantiates one of every widget the real
    /// views use, so every wgpu pipeline is compiled before the first hotkey.
    ///
    /// SPIKE: S3 — the mechanism is sound; the ≤ 80 ms number is unverified.
    /// S3 measures hotkey-down to panel-visible on a cold app.
    WarmingUp {
        /// Frames still to render before the panel is considered warm.
        frames_left: u8,
    },

    /// Visible, nothing submitted. The empty state.
    Idle,

    /// Submitted, no first token yet. The loading state.
    Loading,

    /// Receiving tokens.
    Streaming,

    /// The stream ended.
    Finished {
        /// Why it ended. Anything but `EndTurn` blocks Replace (§13).
        reason: StopReason,
    },

    /// The request failed; [`PanelState::error`] carries the treatment.
    Failed,
}

/// What the panel knows about the app it was invoked over (§8).
///
/// The panel is shown before capture completes, so `Pending` is the *normal*
/// first state, not an error.
#[derive(Debug, Clone, Default)]
pub enum ContextState {
    /// Capture is in flight behind its deadline.
    #[default]
    Pending,

    /// Capture succeeded.
    Available {
        /// The app the hotkey fired over.
        app: Option<AppInfo>,
        /// A one-line excerpt for the chip.
        excerpt: Option<String>,
        /// The captured payload was middle-out truncated to fit the budget (§5).
        truncated: bool,
        /// Caret or selection bounds, when the platform layer supplied them.
        ///
        /// This is what makes Complete and Transform feel attached to what the
        /// user is doing (§9). It is `None` far more often than not, which is
        /// why [`crate::placement`] keeps the fallback path first-class.
        ///
        /// SPIKE: S1 — whether these are obtainable and correct under mixed DPI
        /// is what S1 measures. Do not assume `Some`.
        caret_bounds: Option<Rect>,
    },

    /// The app exposes nothing usable. aibo works from what the user types.
    Unavailable {
        /// App name, for the message. `None` when even that is unknown.
        app: Option<String>,
    },

    /// The OS permission is missing (§8, §17). Distinct from `Unavailable`
    /// because it is fixable and the copy has to say how.
    PermissionDenied {
        /// Current status — `Revoked` gets different copy from `Denied` (§17).
        status: PermissionStatus,
    },

    /// An IME composition is active (§9).
    ///
    /// §9's rule, and it is absolute: aibo does not read the field and does not
    /// insert while composing. Reading returns either the pre-composition text
    /// or the uncommitted reading, and neither is what the user sees; inserting
    /// interleaves with the pending composition unpredictably.
    ///
    /// SPIKE: S7 — Windows detection is `ImmGetContext` + `ImmGetCompositionString`
    /// on the foreground window. **macOS has no clean cross-process API for
    /// this**, which is why S7 exists and why this state must be reachable
    /// without assuming detection works.
    ImeActive,
}

/// What action an inline error offers (§13: "one sentence + one action button").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorAction {
    /// Re-run the request unchanged.
    Retry,
    /// Re-run against `Smart`.
    RetryWith(Role),
    /// Re-authenticate.
    SignIn(ProviderId),
    /// Shorten the selection to fit the context budget.
    TrimSelection,
    /// Open settings. The blocking treatment.
    OpenSettings,
    /// Rebind the failed request to a model the provider does accept (§13's one
    /// action for [`AiboError::ModelRejected`]).
    ///
    /// Carries the replacement rather than only naming it in the sentence: the
    /// point of typing the error was that the UI can *act*, and an action that
    /// merely opens settings and hopes the user remembers the id is the same
    /// dead end in nicer prose.
    UseModel {
        /// The provider whose binding is wrong.
        provider: ProviderId,
        /// A model that provider is known to accept.
        model: String,
    },
    /// Copy a redacted diagnostics bundle. The only action `Internal` offers.
    CopyDiagnostics,
    /// Continue past a budget ceiling (§14).
    ContinueAnyway,
}

/// An error, already reduced to what the UI is allowed to show (§13).
///
/// Constructed by [`ErrorView::from_error`], which is the single place the §13
/// treatment table is applied. Building one of these by hand in a view would be
/// how the table drifts.
#[derive(Debug, Clone)]
pub struct ErrorView {
    /// The §13 treatment.
    pub treatment: Treatment,
    /// Severity for the theme.
    pub severity: Severity,
    /// One sentence. Never the raw `Display` of `Internal`.
    pub headline: String,
    /// The single offered action, if any.
    pub action: Option<ErrorAction>,
}

impl ErrorView {
    /// Reduce an [`AiboError`] to its §13 treatment.
    ///
    /// The mapping lives here rather than in `view` so it is testable and so
    /// the "`Internal` is never shown raw" rule is enforced in exactly one
    /// place: the source is deliberately dropped on the floor for display and
    /// survives only in the log and the diagnostics bundle.
    pub fn from_error(error: &AiboError) -> Self {
        let treatment = error.treatment();
        let (severity, headline, action) = match error {
            AiboError::NoProviderConfigured => (
                Severity::Danger,
                i18n::t(Key::ErrNoProvider).to_owned(),
                Some(ErrorAction::OpenSettings),
            ),
            AiboError::Auth { provider, .. } => (
                Severity::Danger,
                i18n::t1(Key::ErrAuth, provider.as_str()),
                Some(ErrorAction::SignIn(provider.clone())),
            ),
            AiboError::RateLimited { provider, .. } => (
                Severity::Warning,
                i18n::t1(Key::ErrRateLimited, provider.as_str()),
                Some(ErrorAction::Retry),
            ),
            AiboError::Offline => (
                Severity::Warning,
                i18n::t(Key::ErrOffline).to_owned(),
                Some(ErrorAction::Retry),
            ),
            AiboError::ProviderUnavailable { provider, .. } => (
                Severity::Warning,
                i18n::t1(Key::ErrProviderUnavailable, provider.as_str()),
                Some(ErrorAction::Retry),
            ),
            AiboError::ContextTooLarge { .. } => (
                Severity::Warning,
                i18n::t(Key::ErrContextTooLarge).to_owned(),
                Some(ErrorAction::TrimSelection),
            ),
            AiboError::Timeout { .. } => (
                Severity::Warning,
                i18n::t(Key::ErrTimeout).to_owned(),
                Some(ErrorAction::RetryWith(Role::Smart)),
            ),
            AiboError::CaptureFailed { app, .. } => {
                (Severity::Info, i18n::t1(Key::ErrCaptureFailed, app), None)
            }
            AiboError::InsertFailed { .. } => (
                Severity::Info,
                i18n::t(Key::ErrInsertFailed).to_owned(),
                None,
            ),
            AiboError::Sandbox { .. } => (
                Severity::Warning,
                i18n::t(Key::ErrSandbox).to_owned(),
                Some(ErrorAction::Retry),
            ),
            AiboError::AgentBackendMissing { which } => (
                Severity::Danger,
                i18n::t1(Key::ErrAgentBackendMissing, which),
                Some(ErrorAction::OpenSettings),
            ),
            AiboError::BudgetExceeded { .. } => (
                Severity::Warning,
                i18n::t(Key::ErrBudgetExceeded).to_owned(),
                Some(ErrorAction::ContinueAnyway),
            ),
            // §13 inline: one sentence, one action — and the action is a model
            // that works, not "copy diagnostics". A binding the provider
            // refuses cannot be retried and §4 does not fall back on it, so if
            // this arm does not name a way out there is none.
            AiboError::ModelRejected {
                provider,
                model,
                alternatives,
                ..
            } => match alternatives.first() {
                Some(replacement) => (
                    Severity::Danger,
                    i18n::t2(Key::ErrModelRejectedUse, model, replacement),
                    Some(ErrorAction::UseModel {
                        provider: provider.clone(),
                        model: replacement.clone(),
                    }),
                ),
                // Nothing known to work: settings is the only honest offer.
                None => (
                    Severity::Danger,
                    i18n::t1(Key::ErrModelRejected, model),
                    Some(ErrorAction::OpenSettings),
                ),
            },
            // §13: never shown raw. Generic message plus "copy diagnostics".
            AiboError::Internal(_) => (
                Severity::Danger,
                i18n::t(Key::ErrInternal).to_owned(),
                Some(ErrorAction::CopyDiagnostics),
            ),
            // `AiboError` is `#[non_exhaustive]`. A variant added in `aibo-core`
            // after this match was written must still be *shown*, and it must be
            // shown under the strictest rule in §13: never raw. It therefore
            // lands on the same generic message as `Internal`.
            _ => (
                Severity::Danger,
                i18n::t(Key::ErrInternal).to_owned(),
                Some(ErrorAction::CopyDiagnostics),
            ),
        };

        Self {
            treatment,
            severity,
            headline,
            action,
        }
    }
}

/// A non-blocking toast (§13: `InsertFailed`, `CaptureFailed`).
#[derive(Debug, Clone)]
pub struct ToastView {
    /// Severity for the theme.
    pub severity: Severity,
    /// The message.
    pub body: String,
}

/// Which model answered, for the §16 metadata line.
#[derive(Debug, Clone, Default)]
pub struct Attribution {
    /// Provider that answered.
    pub provider: Option<ProviderId>,
    /// Wire model id.
    pub model: Option<String>,
    /// First-token latency in milliseconds.
    pub latency_ms: Option<u64>,
    /// Formatted cost (§14).
    pub cost_label: Option<String>,
    /// Set when the role chain fell back; §13 requires a footnote naming the
    /// substitute rather than silently swapping models under the user.
    pub substituted_for: Option<ProviderId>,
}

/// Everything the panel renders from.
#[derive(Debug, Clone)]
pub struct PanelState {
    /// The session this panel is bound to. §13: one panel, one session.
    pub session: SessionId,
    /// Lifecycle.
    pub phase: Phase,
    /// What was captured.
    pub context: ContextState,
    /// The user's instruction, exactly as typed. §5: the only content allowed
    /// to authorise a tool call.
    pub input: String,
    /// The surface, frozen for the session once resolved (§1).
    pub surface: Surface,
    /// Accumulated response text.
    pub response: String,
    /// Reasoning tokens, kept on their own channel: rendered collapsed, never
    /// inserted (§7).
    pub reasoning: String,
    /// Height reserved for the answer box, fixed on the first chunk (§16
    /// "streaming must not reflow") and only grown in discrete steps.
    pub reserved_answer_height: f32,
    /// Which model answered.
    pub attribution: Attribution,
    /// Token accounting so far (§14).
    pub usage: Usage,
    /// The current inline/blocking error, if any.
    pub error: Option<ErrorView>,
    /// The current toast, if any.
    pub toast: Option<ToastView>,
    /// A Do run was launched from this panel and lives in the task window (§6).
    pub handed_off_to_task: bool,
}

impl PanelState {
    /// A fresh panel for `session`, hidden and warming up.
    pub fn new(session: SessionId) -> Self {
        Self {
            session,
            phase: Phase::WarmingUp {
                frames_left: WARMUP_FRAMES,
            },
            context: ContextState::Pending,
            input: String::new(),
            surface: Surface::Ask,
            response: String::new(),
            reasoning: String::new(),
            reserved_answer_height: theme::ANSWER_BOX_MIN_HEIGHT,
            attribution: Attribution::default(),
            usage: Usage::default(),
            error: None,
            toast: None,
            handed_off_to_task: false,
        }
    }

    /// Reset for a new invocation, keeping nothing from the previous session.
    ///
    /// §13: "pressing the hotkey while a Complete is streaming — the in-flight
    /// request is cancelled, the panel is re-captured for the new context, and
    /// the old session is discarded." Discarded means discarded; carrying the
    /// old response forward is how a rewrite of the wrong text gets pasted.
    pub fn reset(&mut self, session: SessionId) {
        let warm = !matches!(self.phase, Phase::WarmingUp { .. });
        *self = Self::new(session);
        if warm {
            self.phase = Phase::Idle;
        }
    }

    /// Whether the response may be inserted into the source app.
    ///
    /// False while streaming, false on any non-`EndTurn` stop reason, false
    /// while an IME composition is active. §13: "a partial stream is never
    /// auto-inserted … silent insertion of half a rewrite over a user's
    /// selection is the worst failure this product can have."
    pub fn can_accept(&self) -> bool {
        matches!(
            self.phase,
            Phase::Finished {
                reason: StopReason::EndTurn
            }
        ) && !self.response.is_empty()
            && !matches!(self.context, ContextState::ImeActive)
    }

    /// Whether the response ended early and must be marked truncated.
    pub fn is_truncated(&self) -> bool {
        matches!(
            self.phase,
            Phase::Finished {
                reason: StopReason::Length
                    | StopReason::Cancelled
                    | StopReason::ContentFilter
                    | StopReason::StopSequence
            }
        )
    }

    /// Whether copying is possible — true whenever there is any text at all,
    /// including a partial one. §13 keeps the partial result in the panel
    /// precisely so the user can copy it manually.
    pub fn can_copy(&self) -> bool {
        !self.response.is_empty()
    }

    /// Grow the reserved answer height in discrete steps, never per token.
    ///
    /// Returns whether the height changed, so the caller can decide to resize
    /// the window — the height animation is one of the three things §16 allows
    /// to move.
    pub fn reserve_for(&mut self, needed: f32) -> bool {
        const STEP: f32 = 48.0;
        let target = (needed / STEP).ceil() * STEP;
        let clamped = target.clamp(theme::ANSWER_BOX_MIN_HEIGHT, theme::PANEL_HEIGHT_MAX);
        if clamped > self.reserved_answer_height {
            self.reserved_answer_height = clamped;
            true
        } else {
            false
        }
    }

    /// Record a failure, applying the §13 treatment.
    ///
    /// Toast-treated errors do not disturb the response; inline and blocking
    /// ones replace it. That distinction is the whole point of the table.
    pub fn fail(&mut self, error: &Arc<AiboError>) {
        let view = ErrorView::from_error(error);
        match view.treatment {
            Treatment::Toast => {
                self.toast = Some(ToastView {
                    severity: view.severity,
                    body: view.headline,
                });
            }
            Treatment::SilentFallback | Treatment::Inline | Treatment::Blocking => {
                self.phase = Phase::Failed;
                self.error = Some(view);
            }
        }
    }

    /// Caret bounds to anchor to, if the capture supplied any (§9).
    ///
    /// Deliberately not offered while an IME composition is active or the
    /// permission is missing: in both cases the field read is untrustworthy, and
    /// anchoring to a stale caret is worse than the centred fallback.
    pub fn caret_bounds(&self) -> Option<Rect> {
        match &self.context {
            ContextState::Available { caret_bounds, .. } => *caret_bounds,
            _ => None,
        }
    }

    /// The height the panel wants, for [`crate::placement::PlacementRequest`].
    pub fn desired_height(&self) -> f32 {
        match self.phase {
            Phase::Hidden | Phase::WarmingUp { .. } | Phase::Idle => theme::PANEL_HEIGHT_COLLAPSED,
            // `COLLAPSED` is input-plus-chrome only. Everything `footer()`
            // renders — the attribution line, any footnotes, and the action row
            // — sits *below* the answer box and must be counted, or the bottom
            // of the stack is clipped by the window edge. Counting only the
            // action row still cut it off; the rows above it push it down.
            _ => (theme::PANEL_HEIGHT_COLLAPSED
                + self.reserved_answer_height
                + self.footer_height())
                .min(theme::PANEL_HEIGHT_MAX),
        }
    }
}

/// Frames to render before the panel counts as warm.
///
/// Two: the first builds the UI tree and compiles the pipelines, the second
/// proves the swapchain survived a present. One is probably enough and three is
/// waste; **S3** decides.
const WARMUP_FRAMES: u8 = 2;

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// What the panel emits. Mapped into the app message in [`crate::app`].
#[derive(Debug, Clone)]
pub enum Message {
    /// The instruction changed.
    InputChanged(String),
    /// `⏎` on the input.
    Submit,
    /// Insert the response into the source app.
    Accept,
    /// Copy the response.
    Copy,
    /// Re-run against `Smart`.
    Escalate,
    /// `esc`: cancel in-flight work and close (§13).
    Dismiss,
    /// The single action offered by an inline error.
    Error(ErrorAction),
    /// Open the OS privacy pane for the missing permission.
    OpenSystemSettings,
    /// Dismiss the toast.
    DismissToast,
    /// Bring the task window forward (§6).
    ShowTask,
}

/// Render the panel.
///
/// Dispatches on [`Phase`] and [`ContextState`] so every §16 state has exactly
/// one code path; nothing here reaches into a fallback "and otherwise show the
/// happy path" branch.
pub fn view(state: &PanelState) -> Element<'_, Message> {
    if let Phase::WarmingUp { .. } = state.phase {
        return warm_up_view();
    }

    let body = column![
        chip_row(state),
        input_row(state),
        content(state),
        footer(state),
    ]
    .spacing(space(2.0));

    let mut stack = column![body].spacing(space(2.0));
    if let Some(toast) = &state.toast {
        stack = stack.push(widgets::toast(
            toast.severity,
            &toast.body,
            Some(Action::new(
                Key::ActionDismiss,
                "esc",
                Message::DismissToast,
            )),
        ));
    }

    container(stack)
        .width(Length::Fill)
        .height(Length::Shrink)
        .padding(space(4.0))
        .style(theme::panel_surface)
        .into()
}

fn chip_row(state: &PanelState) -> Element<'_, Message> {
    match &state.context {
        ContextState::Available { app, excerpt, .. } => widgets::context_chip(
            app.as_ref().map(|a| a.display_name.as_str()),
            excerpt.as_deref(),
        ),
        // Capture in flight: render the chip in its "no context" form rather
        // than nothing, so the layout does not shift when it lands (§8).
        ContextState::Pending => widgets::context_chip(None, None),
        ContextState::Unavailable { app } => widgets::context_chip(app.as_deref(), None),
        ContextState::PermissionDenied { .. } | ContextState::ImeActive => {
            widgets::context_chip(None, None)
        }
    }
}

fn input_row(state: &PanelState) -> Element<'_, Message> {
    // §9: aibo must accept Japanese input in its own panel, which needs winit
    // `Ime` events, `set_ime_allowed` and `set_ime_cursor_area` for candidate
    // window placement. "If a Japanese user cannot type Japanese into the
    // panel, the Japanese market is closed."
    //
    // SPIKE: S10 — iced's IME support has historically been incomplete and an
    // overlay window makes it harder. On the critical path, not a nicety.
    text_input(i18n::t(Key::PanelPlaceholder), &state.input)
        .id(INPUT_ID)
        .on_input(Message::InputChanged)
        .on_submit(Message::Submit)
        .size(type_scale::BODY)
        .font(theme::MONO_FONT)
        .padding(space(2.5))
        .style(theme::input)
        .into()
}

fn content(state: &PanelState) -> Element<'_, Message> {
    // Context problems outrank response state: there is no point streaming a
    // rewrite of text aibo could not read.
    match &state.context {
        ContextState::ImeActive => {
            return widgets::state_block(
                Severity::Warning,
                i18n::t(Key::StateImeActive),
                None,
                Vec::new(),
            );
        }
        ContextState::PermissionDenied { status } => {
            return widgets::permission_banner(
                *status,
                i18n::t(Key::StatePermissionDeniedBody),
                Some(Message::OpenSystemSettings),
            );
        }
        _ => {}
    }

    match &state.phase {
        Phase::Hidden | Phase::WarmingUp { .. } | Phase::Idle => {
            if matches!(state.context, ContextState::Unavailable { .. }) {
                widgets::state_block(
                    Severity::Info,
                    i18n::t(Key::StateContextUnavailableTitle),
                    Some(i18n::t(Key::StateContextUnavailableBody)),
                    Vec::new(),
                )
            } else {
                widgets::state_block(
                    Severity::Info,
                    i18n::t(Key::StateEmptyTitle),
                    Some(i18n::t(Key::StateEmptyBody)),
                    Vec::new(),
                )
            }
        }

        // Loading reserves the same height the answer box will use, so the
        // first token does not move anything (§16).
        Phase::Loading => container(
            row![
                text(i18n::t(Key::StateLoading))
                    .size(type_scale::BODY)
                    .style(theme::text_dim),
                Space::new().width(Length::Fill),
            ]
            .align_y(Alignment::Center),
        )
        .height(Length::Fixed(state.reserved_answer_height))
        .padding(space(2.0))
        .style(theme::raised)
        .into(),

        Phase::Streaming => widgets::answer(&state.response, state.reserved_answer_height, false),

        Phase::Finished { .. } => widgets::answer(
            &state.response,
            state.reserved_answer_height,
            state.is_truncated(),
        ),

        Phase::Failed => {
            let Some(error) = &state.error else {
                return widgets::state_block(
                    Severity::Danger,
                    i18n::t(Key::ErrInternal),
                    None,
                    Vec::new(),
                );
            };

            let actions = match &error.action {
                Some(action) => vec![error_action(action.clone())],
                None => Vec::new(),
            };

            // A partial response survives the failure and stays copyable (§13).
            if state.response.is_empty() {
                widgets::state_block(error.severity, &error.headline, None, actions)
            } else {
                column![
                    widgets::state_block(error.severity, &error.headline, None, actions),
                    widgets::answer(&state.response, state.reserved_answer_height, true),
                ]
                .spacing(space(2.0))
                .into()
            }
        }
    }
}

fn error_action(action: ErrorAction) -> Action<Message> {
    let (label, key) = match &action {
        ErrorAction::Retry => (Key::ActionRetry, "⌘R"),
        ErrorAction::RetryWith(_) => (Key::ActionSmartModel, "⌘↩"),
        ErrorAction::SignIn(_) => (Key::ActionSignIn, "⏎"),
        ErrorAction::TrimSelection => (Key::ActionTrimSelection, "⏎"),
        ErrorAction::OpenSettings => (Key::ActionOpenSettings, "⏎"),
        ErrorAction::UseModel { .. } => (Key::ActionSwitchModel, "⏎"),
        ErrorAction::CopyDiagnostics => (Key::ActionCopyDiagnostics, "⌘C"),
        ErrorAction::ContinueAnyway => (Key::ActionContinueAnyway, "⏎"),
    };
    Action::new(label, key, Message::Error(action)).primary()
}

impl PanelState {
    /// Height of everything [`footer`] renders below the answer box.
    ///
    /// Kept next to `footer` deliberately: the two must agree, and the failure
    /// mode when they drift is silent clipping at the window edge rather than a
    /// layout error anyone would notice in a test.
    fn footer_height(&self) -> f32 {
        let mut rows = 0.0;
        if self.attribution.provider.is_some() && self.attribution.model.is_some() {
            rows += theme::META_LINE_HEIGHT;
        }
        if self.attribution.substituted_for.is_some() && self.attribution.provider.is_some() {
            rows += theme::META_LINE_HEIGHT;
        }
        if matches!(self.context, ContextState::Available { truncated: true, .. }) {
            rows += theme::META_LINE_HEIGHT;
        }
        rows + theme::ACTION_ROW_HEIGHT
    }
}

fn footer(state: &PanelState) -> Element<'_, Message> {
    let mut stack = column![].spacing(space(1.5));

    // The §16 metadata line: model, latency, cost.
    if let (Some(provider), Some(model)) = (&state.attribution.provider, &state.attribution.model) {
        stack = stack.push(widgets::meta_line(
            provider.as_str(),
            model,
            state.attribution.latency_ms,
            state.attribution.cost_label.as_deref(),
        ));
    }

    // §13: a fallback is never silent to the point of invisibility.
    if let Some(original) = &state.attribution.substituted_for
        && let Some(actual) = &state.attribution.provider
    {
        let _ = original;
        stack = stack.push(widgets::footnote::<Message>(i18n::t1(
            Key::FootnoteFallback,
            actual.as_str(),
        )));
    }

    if let ContextState::Available {
        truncated: true, ..
    } = &state.context
    {
        stack = stack.push(widgets::footnote::<Message>(
            i18n::t(Key::FootnoteInputTruncated).to_owned(),
        ));
    }

    stack.push(widgets::action_list(actions_for(state))).into()
}

/// The action list for the current state.
///
/// §16: "every action has a key, shown". Actions that are not currently
/// available are rendered disabled rather than removed — a list that changes
/// length under the user's fingers defeats the point of a keyboard-first tool.
fn actions_for(state: &PanelState) -> Vec<Action<Message>> {
    let mut actions = Vec::new();

    if state.handed_off_to_task {
        actions.push(Action::new(Key::ActionShowTask, "⌘T", Message::ShowTask));
    }

    let replace = Action::new(Key::ActionReplace, "⏎", Message::Accept).primary();
    actions.push(if state.can_accept() {
        replace
    } else {
        replace.disabled()
    });

    let copy = Action::new(Key::ActionCopy, "⌘C", Message::Copy);
    actions.push(if state.can_copy() {
        copy
    } else {
        copy.disabled()
    });

    if matches!(state.phase, Phase::Loading | Phase::Streaming) {
        actions.push(Action::new(Key::ActionCancel, "esc", Message::Dismiss));
    } else {
        actions.push(Action::new(Key::ActionSmartModel, "⌘↩", Message::Escalate));
        actions.push(Action::new(Key::ActionDismiss, "esc", Message::Dismiss));
    }

    actions
}

/// The throwaway first frame (§6).
///
/// Instantiates one of every widget kind the real views use — text, container,
/// button, scrollable, text input — so their wgpu pipelines and glyph atlases
/// are built while the window is still hidden. It is never seen; correctness
/// here means *coverage*, not appearance.
fn warm_up_view<'a>() -> Element<'a, Message> {
    container(
        column![
            widgets::context_chip::<Message>(Some("aibo"), Some("warm")),
            text_input("", "")
                .size(type_scale::BODY)
                .font(theme::MONO_FONT)
                .style(theme::input),
            widgets::answer::<Message>("warm", theme::ANSWER_BOX_MIN_HEIGHT, true),
            widgets::meta_line::<Message>("aibo", "warm", Some(0), Some("0")),
            widgets::action_list(vec![
                Action::new(Key::ActionReplace, "⏎", Message::Dismiss).primary(),
                Action::new(Key::ActionCopy, "⌘C", Message::Dismiss),
                Action::new(Key::ActionDeny, "esc", Message::Dismiss).destructive(),
            ]),
        ]
        .spacing(space(2.0)),
    )
    .padding(space(4.0))
    .style(theme::panel_surface)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aibo_core::error::{AuthKind, InsertFailure, TimeoutPhase};

    fn panel() -> PanelState {
        let mut state = PanelState::new(SessionId::from_u128(1));
        state.phase = Phase::Idle;
        state
    }

    #[test]
    fn a_partial_response_is_never_acceptable() {
        let mut state = panel();
        state.response = "half a rewr".to_owned();
        state.phase = Phase::Streaming;
        assert!(!state.can_accept());
        assert!(state.can_copy(), "the partial must stay copyable (§13)");

        state.phase = Phase::Finished {
            reason: StopReason::Cancelled,
        };
        assert!(!state.can_accept());
        assert!(state.is_truncated());

        state.phase = Phase::Finished {
            reason: StopReason::EndTurn,
        };
        assert!(state.can_accept());
        assert!(!state.is_truncated());
    }

    #[test]
    fn an_active_ime_composition_blocks_insertion() {
        let mut state = panel();
        state.response = "書き換え".to_owned();
        state.phase = Phase::Finished {
            reason: StopReason::EndTurn,
        };
        state.context = ContextState::ImeActive;
        assert!(!state.can_accept());
    }

    #[test]
    fn internal_errors_are_never_shown_raw() {
        let error = AiboError::Internal(Box::new(std::io::Error::other(
            "postgres://user:hunter2@db.internal/secrets",
        )));
        let view = ErrorView::from_error(&error);
        assert!(!view.headline.contains("hunter2"));
        assert!(!view.headline.contains("postgres"));
        assert_eq!(view.action, Some(ErrorAction::CopyDiagnostics));
    }

    #[test]
    fn treatments_follow_the_section_13_table() {
        let mut state = panel();

        // Toast: the response survives untouched.
        state.response = "keep me".to_owned();
        state.phase = Phase::Finished {
            reason: StopReason::EndTurn,
        };
        state.fail(&Arc::new(AiboError::InsertFailed {
            reason: InsertFailure::AppRejected,
        }));
        assert!(state.toast.is_some());
        assert_eq!(state.response, "keep me");
        assert!(state.error.is_none());

        // Inline: the phase moves to Failed.
        state.fail(&Arc::new(AiboError::Timeout {
            phase: TimeoutPhase::FirstToken,
        }));
        assert_eq!(state.phase, Phase::Failed);
        assert_eq!(
            state.error.as_ref().map(|e| e.treatment),
            Some(Treatment::Inline)
        );

        // Blocking: only NoProviderConfigured.
        state.fail(&Arc::new(AiboError::NoProviderConfigured));
        assert_eq!(
            state.error.as_ref().map(|e| e.treatment),
            Some(Treatment::Blocking)
        );
        assert_eq!(
            state.error.as_ref().and_then(|e| e.action.clone()),
            Some(ErrorAction::OpenSettings)
        );
    }

    fn rejected_codex_binding(alternatives: &[&str]) -> AiboError {
        AiboError::ModelRejected {
            provider: ProviderId::CODEX,
            model: "gpt-5-codex".to_owned(),
            constraint: "not supported when using Codex with a ChatGPT account".to_owned(),
            alternatives: alternatives.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn a_rejected_model_is_actionable_not_something_went_wrong() {
        // The regression: `check_codex_model` used to box its refusal inside
        // `AiboError::Internal`, which lands on the generic arm below — so a
        // user who bound `gpt-5-codex` got "Something went wrong inside aibo."
        // plus "Copy diagnostics". §4 does not fall back on a 400, so that was
        // the end of the road.
        i18n::set_language(i18n::Lang::En);
        let view = ErrorView::from_error(&rejected_codex_binding(&[
            "gpt-5.5",
            "gpt-5.6-terra",
            "gpt-5.6-sol",
        ]));

        assert_eq!(view.treatment, Treatment::Inline);
        assert_ne!(view.headline, i18n::t(Key::ErrInternal));
        assert_ne!(view.action, Some(ErrorAction::CopyDiagnostics));

        // One sentence that names both the id that failed and one that works.
        assert!(view.headline.contains("gpt-5-codex"), "{}", view.headline);
        assert!(view.headline.contains("gpt-5.5"), "{}", view.headline);

        // One action, and it carries the replacement rather than only saying it.
        assert_eq!(
            view.action,
            Some(ErrorAction::UseModel {
                provider: ProviderId::CODEX,
                model: "gpt-5.5".to_owned(),
            })
        );
    }

    #[test]
    fn a_rejected_model_offers_a_verified_working_id() {
        // §3a's measured sets. The offer must come from the working one — an
        // action that swaps one refused id for another is worse than none.
        const WORKING: &[&str] = &[
            "gpt-5.5",
            "gpt-5.6-terra",
            "gpt-5.3-codex-spark",
            "gpt-5.6-luna",
            "gpt-5.6-sol",
        ];
        const REJECTED: &[&str] = &[
            "gpt-5",
            "gpt-5-codex",
            "gpt-5.1-codex",
            "gpt-5.1-codex-mini",
            "codex-mini-latest",
        ];
        let view = ErrorView::from_error(&rejected_codex_binding(WORKING));
        let Some(ErrorAction::UseModel { model, .. }) = view.action else {
            panic!("{:?}", view.action);
        };
        assert!(WORKING.contains(&model.as_str()), "{model}");
        assert!(!REJECTED.contains(&model.as_str()), "{model}");
    }

    #[test]
    fn a_rejected_model_with_nothing_to_offer_still_never_says_copy_diagnostics() {
        i18n::set_language(i18n::Lang::En);
        let view = ErrorView::from_error(&rejected_codex_binding(&[]));
        assert_eq!(view.treatment, Treatment::Inline);
        assert!(view.headline.contains("gpt-5-codex"), "{}", view.headline);
        assert_eq!(view.action, Some(ErrorAction::OpenSettings));
    }

    #[test]
    fn a_rejected_model_reaches_the_panel_inline_and_keeps_a_partial_answer() {
        let mut state = panel();
        state.response = "half a rewr".to_owned();
        state.fail(&Arc::new(rejected_codex_binding(&["gpt-5.5"])));
        assert_eq!(state.phase, Phase::Failed);
        assert!(state.toast.is_none());
        assert!(state.can_copy(), "the partial must stay copyable (§13)");
        assert!(matches!(
            state.error.as_ref().and_then(|e| e.action.clone()),
            Some(ErrorAction::UseModel { .. })
        ));
    }

    #[test]
    fn auth_failures_offer_sign_in_for_the_right_provider() {
        let view = ErrorView::from_error(&AiboError::Auth {
            provider: ProviderId::CEREBRAS,
            kind: AuthKind::Expired,
        });
        assert_eq!(view.action, Some(ErrorAction::SignIn(ProviderId::CEREBRAS)));
    }

    #[test]
    fn reset_discards_the_previous_session_entirely() {
        let mut state = panel();
        state.response = "old answer".to_owned();
        state.input = "old instruction".to_owned();
        state.reset(SessionId::from_u128(2));
        assert!(state.response.is_empty());
        assert!(state.input.is_empty());
        assert_eq!(state.session, SessionId::from_u128(2));
        // Already warm: it must not warm up again, which would flash a frame.
        assert_eq!(state.phase, Phase::Idle);
    }

    #[test]
    fn the_answer_box_grows_in_steps_not_per_token() {
        let mut state = panel();
        let first = state.reserved_answer_height;
        assert!(!state.reserve_for(first - 1.0), "must not shrink or churn");
        assert!(state.reserve_for(first + 1.0));
        let grown = state.reserved_answer_height;
        assert!(
            !state.reserve_for(grown - 4.0),
            "small growth must not reflow"
        );
        assert!(state.reserved_answer_height <= theme::PANEL_HEIGHT_MAX);
    }
}
