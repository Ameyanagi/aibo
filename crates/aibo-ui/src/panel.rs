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

use aibo_core::context::Turn;
use aibo_core::error::{AttachmentRejection, CaptureFailure, InsertFailure, Treatment};
use aibo_core::types::{
    AppInfo, Attachment, AttachmentSource, PermissionStatus, ProviderId, Rect, Role, StopReason,
    Surface, validate_attachments,
};
use aibo_core::{AiboError, types::Usage};
use iced::widget::{
    Space, column, container, image, markdown, pick_list, row, rule, scrollable, text, text_editor,
    text_input,
};
use iced::{Alignment, Element, Length};
use uuid::Uuid;

use crate::bridge::{ModelOption, SessionId};
use crate::i18n::{self, Key};
use crate::theme::{self, Severity, space, type_scale};
use crate::widgets::{self, Action, RailState};

/// The id of the panel's text input, so focus can be requested on show.
pub const INPUT_ID: &str = "aibo.panel.input";

/// The id of the model quick-pick's search field.
pub const PICKER_ID: &str = "aibo.panel.picker";

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
        /// The full captured selection shown inside the composer.
        ///
        /// It remains distinct from [`PanelState::input`]: captured content is
        /// untrusted context and must not become a tool-authorizing instruction
        /// merely because the UI makes it visible beside the question.
        selection: Option<String>,
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

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

/// The shortcut that attaches the clipboard image.
///
/// `text_input` claims plain `⌘V` to paste text and captures the event, so the
/// attach handler cannot live on the panel's ordinary
/// [`iced::keyboard::listen`] subscription — see [`crate::app`] for how it is
/// claimed instead. Declared here because this is the module that shows it.
pub const ATTACH_KEY: &str = widgets::primary_shortcut("⌘V", "Ctrl+V");

/// The shortcut that removes the most recently attached image: backspace with
/// nothing left in the instruction to delete.
///
/// The convention every composer with chips already uses — mail recipients,
/// chat attachments — so it needs no learning. It is also the only binding here
/// that is *safe*, and the alternatives are worth recording because each looks
/// fine until it is tried:
///
/// * `⌘⌫` — on macOS `text_input` reads it as "delete to the start of the line"
///   and captures it, so removal would silently eat the typed instruction.
/// * `⌘⇧V`, the tidy inverse of [`ATTACH_KEY`] — `text_input` matches shortcuts
///   on `to_latin`, which yields `'V'` and misses its `'v'` arm, so the chord
///   falls through to the widget's plain text insertion. That insertion has no
///   modifier guard, and `iced_winit` feeds it winit's
///   `text_with_all_modifiers()`, which on macOS is `NSEvent.characters` — `"V"`.
///   The chord would remove the image *and* type a `V` into the instruction.
///
/// Backspace reaches the widget's own handler, which on an empty value moves a
/// cursor that is already at zero and inserts nothing. The panel therefore only
/// claims it while [`PanelState::input`] is empty; with text in the field the
/// key belongs to the text, which is what the user expects and what every other
/// composer does.
pub const DETACH_KEY: &str = "⌫";

/// An attachment plus the render-side handle used to draw its chip.
///
/// The handle is built **once, here, at attach time**. `Handle::from_bytes`
/// assigns a fresh id on every call, so a handle built inside `view` would miss
/// the renderer's cache on every frame and re-upload the whole image to the GPU
/// at frame rate — on the one surface in the product with a millisecond budget
/// (§15).
#[derive(Debug, Clone)]
pub struct Attached {
    /// The attachment itself: what actually goes on the wire.
    pub attachment: Attachment,
    /// Its thumbnail, decoded once.
    pub thumbnail: image::Handle,
}

impl Attached {
    /// Build the chip-side view of `attachment`.
    fn new(attachment: Attachment) -> Self {
        // One copy of the pixels, once, at attach time: `image::Handle` needs
        // `bytes::Bytes` and `Attachment::bytes` is an `Arc<[u8]>`, which it
        // cannot adopt. The `Arc` exists so that cloning a `ChatRequest` for a
        // fallback entry, a persistence task and the UI does not copy megabytes
        // three times per *request*; this is one copy per *attachment*, and it
        // buys the user a picture of what they attached.
        let thumbnail = image::Handle::from_bytes(attachment.bytes.to_vec());
        Self {
            attachment,
            thumbnail,
        }
    }
}

/// What the panel knows is sitting on the clipboard (§5 budget priority 4).
///
/// **This is ambient state and it decides nothing.** It exists so the attach
/// action can be shown enabled or disabled honestly, and so ⌘V has something to
/// attach. It never routes: `RouteInput::has_image` is a function of what the
/// user *attached*, and deriving it from what merely happened to be on the
/// pasteboard is the 2026-07-26 defect this whole feature exists to retire.
#[derive(Debug, Clone, Default)]
pub enum ClipboardOffer {
    /// Capture has not reported yet. Indistinguishable from `Nothing` for the
    /// user, but distinct in the state machine (§8: context arrives late).
    #[default]
    Unknown,

    /// Nothing attachable — text, files, empty, concealed, or a type aibo does
    /// not send.
    Nothing,

    /// An image, which ⌘V will attach.
    Image {
        /// What the chip will be labelled. Display text only.
        label: String,

        /// The pixels, when capture carried them.
        ///
        /// `None` means the pasteboard advertised an image it would not hand
        /// over. That is a real macOS state — promised/deferred pasteboard
        /// types are resolved lazily by the source app and the resolution can
        /// fail, which is the same hazard [`aibo_core::types::ClipboardItem`]
        /// tracks as `restorable` — and it is also, today, *every* image:
        /// `UiEvent::Context` carries a `ClipboardItem`, and a `ClipboardItem`
        /// describes the clipboard without inlining it (`ClipboardKind::ImageRef`
        /// is documented as "referenced rather than inlined").
        ///
        /// Filling it needs a byte-bearing request/event pair in
        /// `crate::bridge`, which this change does not own. Until then ⌘V is
        /// honest rather than silent: it reports that the image could not be
        /// read (§13 toast) instead of doing nothing, and it never invents a
        /// routing decision out of the clipboard's mere presence.
        image: Option<Box<Attachment>>,
    },
}

impl ClipboardOffer {
    /// Whether the clipboard advertises an image.
    ///
    /// This is deliberately weaker than [`ClipboardOffer::is_attachable`]: the
    /// capture bridge can identify an image before it can provide the bytes.
    pub const fn is_image(&self) -> bool {
        matches!(self, ClipboardOffer::Image { .. })
    }

    /// Whether activating Attach can actually add an image right now.
    pub const fn is_attachable(&self) -> bool {
        matches!(self, ClipboardOffer::Image { image: Some(_), .. })
    }
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
    /// Take an image back off the request (§13's one action for
    /// [`AiboError::AttachmentRejected`], and the fallback for
    /// [`AiboError::VisionUnsupported`] when there is no model to switch to).
    ///
    /// Carries the chip label the error named so the panel removes *that* one
    /// rather than guessing. Empty means "the set is the problem" — too many,
    /// or too many bytes in total — and the panel takes the most recent, which
    /// is the one the user just added and the only one they can have meant.
    RemoveAttachment {
        /// The chip label from the error, or empty for the whole-set case.
        label: String,
    },
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
    /// The second sentence: what to do about it (`design.md` §6).
    ///
    /// `headline` says what happened. Errors that can name a next step put it
    /// here, and `state_block` renders the pair. Most cannot — the action button
    /// is the next step — so this stays `None` for them rather than padding the
    /// state with a restatement.
    pub body: Option<String>,
    /// The single offered action, if any.
    pub action: Option<ErrorAction>,
    /// This error is a complaint about the attachments.
    ///
    /// Kept so the panel can retire it once the attachments are gone — an
    /// error that outlives the thing it complained about is a dead end.
    /// Derived from the variant here, in the one place the §13 table is
    /// applied, rather than re-inferred from the headline or the action at each
    /// call site.
    pub about_attachments: bool,
}

impl ErrorView {
    /// Whether this error is an authentication complaint about `provider`.
    ///
    /// Follows the same principle as `about_attachments`: an error that
    /// outlives the thing it complained about is a dead end. When a provider
    /// signs back in, an auth error naming it is no longer true and must go.
    ///
    /// Keyed on the offered action rather than the headline, because the
    /// headline is localised and the action is the structured fact.
    pub fn is_auth_for(&self, provider: &ProviderId) -> bool {
        matches!(&self.action, Some(ErrorAction::SignIn(p)) if p == provider)
    }

    /// Reduce an [`AiboError`] to its §13 treatment.
    ///
    /// The mapping lives here rather than in `view` so it is testable and so
    /// the "`Internal` is never shown raw" rule is enforced in exactly one
    /// place: the source is deliberately dropped on the floor for display and
    /// survives only in the log and the diagnostics bundle.
    pub fn from_error(error: &AiboError) -> Self {
        let treatment = error.treatment();
        let (severity, headline, action) = match error {
            // `design.md` §6: the button names the outcome, not the window it
            // lives in — "the button that says Publish produces Published".
            // "Open settings" described the mechanism and left the user to find
            // the next step themselves; "Sign in" is the step.
            AiboError::NoProviderConfigured => (
                Severity::Danger,
                i18n::t(Key::ErrNoProvider).to_owned(),
                Some(ErrorAction::SignIn(ProviderId::CODEX)),
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
            // A 4xx is not "not responding" — the provider answered, and said
            // why. §13 wants errors that state what happened, and the provider's
            // own sentence is more use than anything aibo can invent: it is the
            // difference between "openai is not responding" and "temperature is
            // not supported with this model".
            AiboError::ProviderUnavailable {
                provider,
                status,
                detail: Some(detail),
            } if *status < 500 => (
                Severity::Warning,
                i18n::t2(Key::ErrProviderRejected, provider.as_str(), detail),
                Some(ErrorAction::CopyDiagnostics),
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
            // Secure input is not a generic capture failure and must not read
            // as one. §8: any process holding the global secure-event-input
            // flag blocks every AX read — a password field, a password manager,
            // or an app that set it and never cleared it. There is no user
            // action, so the copy names the cause and offers nothing rather
            // than implying something is theirs to fix.
            AiboError::CaptureFailed {
                reason: CaptureFailure::SecureInput,
                ..
            } => (
                Severity::Info,
                i18n::t(Key::ErrSecureInput).to_owned(),
                None,
            ),
            AiboError::CaptureFailed { app, .. } => {
                (Severity::Info, i18n::t1(Key::ErrCaptureFailed, app), None)
            }
            AiboError::InsertFailed {
                reason: InsertFailure::SecureInput,
            } => (
                Severity::Info,
                i18n::t(Key::ErrSecureInput).to_owned(),
                None,
            ),
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
            // §13 inline, and the reason `VisionUnsupported` is a typed variant
            // rather than an `Internal`: the user has a working text setup and
            // one attachment too many, so the sentence names the model that
            // cannot see and the action is a model that can. Falling through to
            // the generic arm below would render "Something went wrong inside
            // aibo." plus "Copy diagnostics" — the same unactionable dead end,
            // in nicer prose, that this feature was built to retire.
            AiboError::VisionUnsupported {
                binding,
                alternatives,
                ..
            } => match (binding, alternatives.first()) {
                // A bound model that cannot see, and one that can: switch.
                (Some(bound), Some(alternative)) => (
                    Severity::Warning,
                    i18n::t2(Key::ErrVisionUnsupportedUse, &bound.model, alternative),
                    Some(use_model_action(alternative, &bound.provider)),
                ),
                // A bound model that cannot see and nothing to move to. The
                // image is the only thing left the user can change, and they
                // must be told that rather than left with a dead panel.
                (Some(bound), None) => (
                    Severity::Warning,
                    i18n::t1(Key::ErrVisionUnsupported, &bound.model),
                    Some(ErrorAction::RemoveAttachment {
                        label: String::new(),
                    }),
                ),
                // No `Vision` chain at all. Deliberately *not*
                // `NoProviderConfigured`: that one is §13's only blocking
                // treatment and it interrupts, which is right for "aibo cannot
                // do anything" and wrong here.
                (None, Some(provider)) => (
                    Severity::Warning,
                    i18n::t1(Key::ErrVisionNoProviderUse, provider),
                    Some(ErrorAction::OpenSettings),
                ),
                (None, None) => (
                    Severity::Warning,
                    i18n::t(Key::ErrVisionNoProvider).to_owned(),
                    Some(ErrorAction::OpenSettings),
                ),
            },
            // §13 inline, one action: take the image off. Caught before
            // dispatch rather than as a provider 400, because §4 does not fall
            // back on a 400 and discovering a cap that way costs a round trip
            // and then dead-ends.
            AiboError::AttachmentRejected {
                label,
                reason,
                media_type: _,
            } => {
                let headline = match reason {
                    AttachmentRejection::TooMany { limit, .. } => {
                        i18n::t1(Key::ErrAttachmentTooMany, &limit.to_string())
                    }
                    AttachmentRejection::TooLarge { .. }
                    | AttachmentRejection::TotalTooLarge { .. } => {
                        i18n::t1(Key::ErrAttachmentTooLarge, label)
                    }
                    AttachmentRejection::UnsupportedMediaType | AttachmentRejection::Empty => {
                        i18n::t1(Key::ErrAttachmentUnusable, label)
                    }
                };
                (
                    Severity::Warning,
                    headline,
                    Some(ErrorAction::RemoveAttachment {
                        label: label.clone(),
                    }),
                )
            }
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
            // §6 wants "one sentence, one action" — where a second sentence
            // genuinely tells the user something the button does not, it goes
            // here. Only the no-provider state has one today: it is the only
            // error with two real routes out (sign in, or bring a key).
            body: match error {
                AiboError::NoProviderConfigured => Some(i18n::t(Key::ErrNoProviderBody).to_owned()),
                _ => None,
            },
            action,
            about_attachments: matches!(
                error,
                AiboError::VisionUnsupported { .. } | AiboError::AttachmentRejected { .. }
            ),
        }
    }
}

/// Turn one of [`AiboError::VisionUnsupported`]'s alternatives into an action.
///
/// The contract spells alternatives `provider/model` when a binding is known,
/// because a vision-capable replacement often lives on a *different* provider
/// than the one that could not see. `fallback` covers the bare-model spelling,
/// where staying on the failed binding's provider is the only reading.
fn use_model_action(alternative: &str, fallback: &ProviderId) -> ErrorAction {
    match alternative.split_once('/') {
        Some((provider, model)) => ErrorAction::UseModel {
            provider: ProviderId::new(provider),
            model: model.to_owned(),
        },
        None => ErrorAction::UseModel {
            provider: fallback.clone(),
            model: alternative.to_owned(),
        },
    }
}

/// A non-blocking toast (§13: `InsertFailed`, `CaptureFailed`).
#[derive(Debug, Clone)]
pub struct ToastView {
    /// Severity for the theme.
    pub severity: Severity,
    /// The message.
    pub body: String,
    /// Offer the redacted diagnostics action beside this message.
    pub offer_diagnostics: bool,
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

/// One completed exchange retained in the visible panel transcript.
#[derive(Debug, Clone)]
pub struct ConversationTurn {
    /// The instruction the user submitted.
    pub user: String,
    /// The assistant response.
    pub assistant: String,
    /// Parsed assistant Markdown retained for zero-copy rendering.
    assistant_markdown: Vec<markdown::Item>,
}

/// Everything the panel renders from.
#[derive(Debug)]
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
    /// Selectable rendering state for [`Self::response`].
    response_editor: text_editor::Content,
    /// Incrementally parsed Markdown for the active assistant response.
    response_markdown: markdown::Content,
    /// Usable height of the display the panel is on, in logical points.
    ///
    /// `design.md` §4 sizes the answer area as a fraction of the display rather
    /// than to a constant, so the panel has to know how big the screen is. It
    /// arrives from `ObservedGeometry::monitor_size` once the window server has
    /// answered; until then [`PanelState::max_panel_height`] falls back to
    /// [`theme::PANEL_HEIGHT_MAX`], which is the behaviour this replaced.
    ///
    /// Deliberately **not** reset by [`PanelState::reset`]: it describes the
    /// hardware, not the session.
    pub display_height: Option<f32>,
    /// Completed exchanges above the active turn.
    pub turns: Vec<ConversationTurn>,
    /// User message currently being answered or reviewed.
    pub active_user: Option<String>,
    /// Whether the pinned selected-text card is expanded.
    pub context_expanded: bool,
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
    /// Images the user **deliberately attached**, in attach order.
    ///
    /// Populated only by [`PanelState::attach`], which only ever runs from a
    /// user gesture. This is the field `RouteInput::has_image` is a function of;
    /// nothing ambient may write to it. §5: the pixels are attacker-controlled
    /// input and are context, never authority — a screenshot of a web page can
    /// contain rendered text reading "ignore your instructions", and text in
    /// pixels defeats every textual filter.
    attachments: Vec<Attached>,
    /// What is on the clipboard right now. Ambient; decides nothing.
    pub clipboard: ClipboardOffer,
    /// Backend-validated models offered by the popup selector.
    pub model_options: Vec<ModelOption>,
    /// Model currently persisted in configuration.
    pub selected_model: Option<ModelOption>,
    /// The quick-pick's own state (§4).
    pub picker: crate::model_picker::ModelPicker,
    /// Models the user has pinned, newest first.
    ///
    /// Persisted, unlike [`PanelState::recent_models`]: a pin is a deliberate
    /// statement about what the user works with, and losing it on quit would
    /// make pinning pointless.
    pub favourite_models: Vec<aibo_core::types::ModelBinding>,
    /// Models used this session, most recent first.
    ///
    /// Session-scoped on purpose. Recency is an observation, not a preference,
    /// and one restored from last week describes a task that is over.
    pub recent_models: Vec<aibo_core::types::ModelBinding>,
    /// `↑`/`↓` recall over previous submissions.
    ///
    /// Session-local: §12's `messages` table is the eventual home, but the
    /// database is not created until onboarding produces a key.
    pub history: crate::history_ring::HistoryRing,
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
            response_editor: text_editor::Content::new(),
            response_markdown: markdown::Content::new(),
            turns: Vec::new(),
            display_height: None,
            active_user: None,
            context_expanded: false,
            reasoning: String::new(),
            reserved_answer_height: theme::ANSWER_BOX_MIN_HEIGHT,
            attribution: Attribution::default(),
            usage: Usage::default(),
            error: None,
            toast: None,
            handed_off_to_task: false,
            attachments: Vec::new(),
            clipboard: ClipboardOffer::Unknown,
            model_options: Vec::new(),
            selected_model: None,
            picker: crate::model_picker::ModelPicker::default(),
            favourite_models: Vec::new(),
            recent_models: Vec::new(),
            // Constructed once at boot, not per session, so recall survives
            // closing and reopening the panel — which is the whole point.
            history: crate::history_ring::HistoryRing::new(),
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
        let model_options = std::mem::take(&mut self.model_options);
        let selected_model = self.selected_model.take();
        // The display is hardware, not session state. Dropping it here would
        // collapse the panel back to the unknown-display fallback on every new
        // invocation and only recover once the window server answered again —
        // a visible shrink between the hotkey and the first frame.
        let display_height = self.display_height;
        // Neither favourites nor recents belong to a panel session: a pin is a
        // durable preference, and recency describes the user's day rather than
        // one invocation.
        let favourite_models = std::mem::take(&mut self.favourite_models);
        let recent_models = std::mem::take(&mut self.recent_models);
        *self = Self::new(session);
        self.favourite_models = favourite_models;
        self.recent_models = recent_models;
        self.model_options = model_options;
        self.selected_model = selected_model;
        self.display_height = display_height;
        if warm {
            self.phase = Phase::Idle;
        }
    }

    /// Replace the response and its selectable rendering state together.
    pub fn set_response(&mut self, response: impl Into<String>) {
        self.response = response.into();
        self.response_editor = text_editor::Content::with_text(&self.response);
        self.response_markdown = markdown::Content::parse(&self.response);
    }

    /// Append a streaming chunk to both response representations.
    pub fn append_response(&mut self, chunk: &str) {
        self.response.push_str(chunk);
        // Rebuilding `Content` from the complete transcript on every provider
        // delta makes an n-byte answer O(n²). Append through the editor's
        // native paste action so only the new coalesced chunk is inserted.
        self.response_editor
            .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
        self.response_editor
            .perform(text_editor::Action::Edit(text_editor::Edit::Paste(
                Arc::new(chunk.to_owned()),
            )));
        self.response_markdown.push_str(chunk);
    }

    /// Clear both response representations.
    pub fn clear_response(&mut self) {
        self.response.clear();
        self.response_editor = text_editor::Content::new();
        self.response_markdown = markdown::Content::new();
    }

    /// Completed turns to include before the next user message.
    pub fn history_for_next_turn(&self) -> Vec<Turn> {
        let mut history: Vec<Turn> = self
            .turns
            .iter()
            .map(|turn| Turn::pair(turn.user.clone(), turn.assistant.clone()))
            .collect();
        if matches!(
            self.phase,
            Phase::Finished {
                reason: StopReason::EndTurn
            }
        ) && let Some(user) = &self.active_user
            && !self.response.is_empty()
        {
            history.push(Turn::pair(user.clone(), self.response.clone()));
        }
        history
    }

    /// Start a new visible chat turn, retaining the completed active exchange.
    pub fn begin_turn(&mut self, user: String) {
        if matches!(
            self.phase,
            Phase::Finished {
                reason: StopReason::EndTurn
            }
        ) && let Some(previous_user) = self.active_user.take()
            && !self.response.is_empty()
        {
            self.turns.push(ConversationTurn {
                user: previous_user,
                assistant: self.response.clone(),
                assistant_markdown: self.response_markdown.items().to_vec(),
            });
        }
        self.active_user = Some(user);
        self.input.clear();
        self.phase = Phase::Loading;
        self.clear_response();
        self.reasoning.clear();
        self.attribution = Attribution::default();
        self.usage = Usage::default();
        self.error = None;
        self.reserved_answer_height = theme::ANSWER_BOX_MIN_HEIGHT;
    }

    /// Prepare the active turn to be generated again.
    pub fn begin_retry(&mut self) {
        self.phase = Phase::Loading;
        self.clear_response();
        self.reasoning.clear();
        self.attribution = Attribution::default();
        self.usage = Usage::default();
        self.error = None;
        self.reserved_answer_height = theme::ANSWER_BOX_MIN_HEIGHT;
    }

    /// Whether a selected-text card is currently pinned to the conversation.
    pub fn includes_selection(&self) -> bool {
        matches!(
            self.context,
            ContextState::Available {
                selection: Some(_),
                ..
            }
        )
    }

    /// Whether the compact composer has expanded into a chat transcript.
    pub fn has_conversation(&self) -> bool {
        self.active_user.is_some() || !self.turns.is_empty()
    }

    /// Remove the selected text from the visible card and future requests.
    pub fn remove_selection(&mut self) {
        if let ContextState::Available {
            excerpt,
            selection,
            truncated,
            ..
        } = &mut self.context
        {
            *excerpt = None;
            *selection = None;
            *truncated = false;
        }
        self.context_expanded = false;
    }

    /// Apply cursor and selection actions while keeping the answer read-only.
    pub fn perform_response_action(&mut self, action: text_editor::Action) {
        if !action.is_edit() {
            self.response_editor.perform(action);
        }
    }

    // -----------------------------------------------------------------------
    // Attachments (§2, §5, §14)
    // -----------------------------------------------------------------------

    /// The attached images, in attach order.
    ///
    /// Read-only on purpose. The field is private and every write goes through
    /// [`PanelState::attach`] or [`PanelState::detach`], so "an attachment is a
    /// deliberate act" is a property of the type rather than of a comment: there
    /// is no way to put an image here that is not a user gesture, and therefore
    /// no way for ambient clipboard content to reach routing.
    pub fn attachments(&self) -> &[Attached] {
        &self.attachments
    }

    /// Whether this request carries at least one image.
    ///
    /// This — and nothing about the clipboard — is what `RouteInput::has_image`
    /// must be built from.
    pub fn has_attachments(&self) -> bool {
        !self.attachments.is_empty()
    }

    /// Attach `attachment`, refusing anything a provider would refuse (§14).
    ///
    /// Validation runs against the set the attach would *produce*, not against
    /// the item alone, so the per-request count and total-bytes ceilings are
    /// caught here rather than as a 400. §4 does not fall back on a 400, so
    /// discovering a cap at the provider costs a round trip and then dead-ends;
    /// discovering it here costs nothing and leaves the user one click from a
    /// request that works.
    ///
    /// On rejection the attachment is **not** added and the error is returned
    /// for the caller to render inline (§13). Nothing is silently dropped.
    ///
    /// Idempotent by [`Attachment::id`]. Holding ⌘V, or pressing it twice
    /// because the first press was not obviously acknowledged, attaches the one
    /// clipboard image once: ids are minted at capture, so a repeat carries the
    /// id it already has. Appending it again would bill the user twice for the
    /// same pixels (§14) and would put two chips behind one id, which is
    /// precisely the ambiguity `detach` resolves by id to avoid.
    pub fn attach(&mut self, attachment: Attachment) -> Result<Uuid, AiboError> {
        let id = attachment.id;
        if self.attachments.iter().any(|a| a.attachment.id == id) {
            return Ok(id);
        }
        let candidate: Vec<Attachment> = self
            .attachments
            .iter()
            .map(|a| a.attachment.clone())
            .chain(std::iter::once(attachment.clone()))
            .collect();
        validate_attachments(&candidate)?;
        self.attachments.push(Attached::new(attachment));
        Ok(id)
    }

    /// Remove the attachment with `id`. Returns whether one was there.
    pub fn detach(&mut self, id: Uuid) -> bool {
        let before = self.attachments.len();
        self.attachments.retain(|a| a.attachment.id != id);
        let removed = self.attachments.len() != before;
        if removed {
            self.clear_attachment_error();
        }
        removed
    }

    /// Remove the most recently attached image — what [`DETACH_KEY`] does.
    pub fn detach_last(&mut self) -> bool {
        match self.attachments.last().map(|a| a.attachment.id) {
            Some(id) => self.detach(id),
            None => false,
        }
    }

    /// Remove the attachment the error named, or the most recent one.
    ///
    /// An empty `label` is the whole-set rejection — too many images, or too
    /// many bytes across them — where the item to drop is the one just added.
    pub fn detach_labelled(&mut self, label: &str) -> bool {
        let target = if label.is_empty() {
            self.attachments.last()
        } else {
            self.attachments
                .iter()
                .rev()
                .find(|a| a.attachment.label == label)
                .or_else(|| self.attachments.last())
        };
        match target.map(|a| a.attachment.id) {
            Some(id) => self.detach(id),
            None => false,
        }
    }

    /// Estimated image input tokens across every attachment (§14).
    ///
    /// Shown as a footnote from the moment an image is attached, not after the
    /// bill arrives: BYOK means the user pays for it, and an attachment is the
    /// one thing that can multiply a turn's cost without changing a visible
    /// word of the instruction.
    pub fn estimated_image_tokens(&self) -> usize {
        self.attachments
            .iter()
            .map(|a| a.attachment.estimated_image_tokens())
            .sum()
    }

    /// Retire an error that was *about* the attachments once none are left.
    ///
    /// §13 promises one action that resolves the state, and an error that
    /// outlives the thing it complained about is a dead end: the panel would
    /// sit in [`Phase::Failed`], holding an instruction the user can no longer
    /// submit, over an image that is no longer there. Scoped by
    /// [`ErrorView::about_attachments`] rather than by which action was offered,
    /// because removing the last image also settles a `VisionUnsupported` whose
    /// offered action was "switch model".
    fn clear_attachment_error(&mut self) {
        if self.attachments.is_empty()
            && self.phase == Phase::Failed
            && self.error.as_ref().is_some_and(|e| e.about_attachments)
        {
            self.error = None;
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
        let target = (needed / ANSWER_HEIGHT_STEP).ceil() * ANSWER_HEIGHT_STEP;
        let clamped = target.clamp(theme::ANSWER_BOX_MIN_HEIGHT, self.max_answer_height());
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
                    offer_diagnostics: false,
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
        // The quick-pick replaces the body, so it sets its own height. Falling
        // through to the transcript arithmetic sized the window for content that
        // is not on screen, and clipped the list after its first row.
        if self.picker.open {
            return PICKER_PANEL_HEIGHT.min(self.max_panel_height());
        }
        // The chips are their own row above the input, so they add height in
        // every phase — including the collapsed one, which is otherwise a
        // constant. Attaching an image into a fixed-height panel would push the
        // input out from under the caret.
        let attachments = self.attachment_block_height();
        let selection = self.selection_preview_height();
        if self.has_conversation() {
            return (theme::PANEL_HEIGHT_COLLAPSED
                + attachments
                + selection
                + self.transcript_height()
                + self.footer_height())
            .clamp(CHAT_PANEL_MIN_HEIGHT, self.max_panel_height());
        }

        match self.phase {
            Phase::Hidden | Phase::WarmingUp { .. } | Phase::Idle => {
                (theme::PANEL_HEIGHT_COLLAPSED + attachments + selection)
                    .min(self.max_panel_height())
            }
            // `COLLAPSED` is input-plus-chrome only. Everything `footer()`
            // renders — the attribution line, any footnotes, and the action row
            // — sits *below* the answer box and must be counted, or the bottom
            // of the stack is clipped by the window edge. Counting only the
            // action row still cut it off; the rows above it push it down.
            _ => (theme::PANEL_HEIGHT_COLLAPSED
                + attachments
                + selection
                + self.answer_height()
                + self.footer_height())
            .min(self.max_panel_height()),
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
    /// Persist and activate a model chosen in the popup.
    SelectModel(ModelOption),
    /// Insert the response into the source app.
    Accept,
    /// Copy the response.
    Copy,
    /// Generate the active assistant response again.
    Retry,
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
    /// Copy the redacted diagnostics bundle offered by a recovery toast.
    CopyDiagnostics,
    /// Bring the task window forward (§6).
    ShowTask,
    /// `↑` — recall an older submission.
    HistoryOlder,
    /// `↓` — walk back toward the newest, then the draft.
    HistoryNewer,
    /// [`ATTACH_KEY`], or the attach entry in the action list: attach the image
    /// on the clipboard. A deliberate act — see [`ClipboardOffer`].
    Attach,
    /// [`DETACH_KEY`]: remove the most recently attached image.
    DetachLast,
    /// The `×` on a chip: remove that image.
    Detach(Uuid),
    /// Expand or collapse the pinned selected-text card.
    ToggleContext,
    /// Remove selected text from this conversation.
    RemoveSelection,
    /// Copy a model-supplied Markdown link instead of launching it.
    CopyLink(String),
    /// Selection, cursor, or an ignored edit attempt in the read-only answer.
    ResponseAction(text_editor::Action),
    /// Open the model quick-pick.
    OpenPicker,
    /// Close it without choosing.
    ClosePicker,
    /// The quick-pick's search query changed.
    PickerQuery(String),
    /// Move the quick-pick highlight.
    PickerMove(isize),
    /// Choose the highlighted model.
    PickerCommit,
    /// Pin or unpin the highlighted model.
    PickerToggleFavourite,
}

/// Render the panel.
///
/// Dispatches on [`Phase`] and [`ContextState`] so every §16 state has exactly
/// one code path; nothing here reaches into a fallback "and otherwise show the
/// happy path" branch.
pub fn view(state: &PanelState, appearance: theme::Appearance) -> Element<'_, Message> {
    if let Phase::WarmingUp { .. } = state.phase {
        return warm_up_view();
    }

    // Row order: source line, answer, one hairline, footer, and the **input
    // last**.
    //
    // `design.md` §3's mock draws the input second, directly under the source
    // line, and that was built and tried. It is reverted deliberately: with the
    // composer at the bottom the panel reads as a conversation you are adding
    // to, which is what a multi-turn transcript actually is, and the eye ends
    // where the next keystroke goes. The spec's argument for the other order —
    // the caret "continuing into a second place" — is about the *first*
    // interaction, and it stops describing the panel the moment there is a
    // transcript above the input.
    //
    // The rest of §3 is unaffected: the rail still runs the full height and is
    // amber on whichever row holds attention, which is the part of the spec
    // doing the real work.
    // While the quick-pick is open it *is* the panel. Rendering it alongside the
    // transcript would put two focusable text fields on screen at once, and the
    // one that answers the keyboard would be whichever iced focused last.
    if state.picker.open {
        return container(widgets::railed(RailState::Active, picker_overlay(state)))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(space(4.0))
            .style(theme::panel_surface)
            .into();
    }

    let has_result =
        state.has_conversation() || !matches!(state.phase, Phase::Hidden | Phase::Idle);
    let shows_content = has_result
        || matches!(
            state.context,
            ContextState::PermissionDenied { .. } | ContextState::ImeActive
        );

    // Zero spacing, deliberately: `widgets::railed` owns the vertical rhythm so
    // the rail segments abut into one continuous line. Spacing here would show
    // as gaps in the rail, and a rail with gaps reads as a rail that stopped.
    let mut body = column![widgets::railed(RailState::Inactive, chip_row(state))];
    if let Some(card) = selection_card(state) {
        body = body.push(widgets::railed(RailState::Inactive, card));
    }
    if let Some(row) = attachment_row(state) {
        body = body.push(widgets::railed(RailState::Inactive, row));
    }

    if shows_content {
        body = body.push(widgets::railed(
            content_rail_state(state),
            content(state, appearance),
        ));
    }
    if has_result {
        // The one hairline `design.md` §9 leaves standing, and it sits in the
        // rail's gutter rather than crossing it — the rail is continuous, and a
        // separator that cut through it would break the element the whole
        // design hangs on.
        body = body.push(row![
            Space::new().width(theme::RAIL_WIDTH + theme::RAIL_GUTTER),
            rule::horizontal(1).style(theme::separator),
        ]);
        body = body.push(widgets::railed(RailState::Inactive, footer(state)));
    }

    body = body.push(widgets::railed(input_rail_state(state), input_row(state)));

    let mut stack = column![body].spacing(space(2.0));
    if let Some(toast) = &state.toast {
        let action = if toast.offer_diagnostics {
            Action::new(
                Key::ActionCopyDiagnostics,
                widgets::primary_shortcut("⌘C", "Ctrl+C"),
                Message::CopyDiagnostics,
            )
        } else {
            Action::new(Key::ActionDismiss, "esc", Message::DismissToast)
        };
        stack = stack.push(widgets::toast(toast.severity, &toast.body, Some(action)));
    }

    container(stack)
        .width(Length::Fill)
        // The native visual-effect view fills the window. Filling the same
        // bounds here prevents a transparent strip of blur below the rounded
        // panel when the window is resized for a chat transcript.
        .height(Length::Fill)
        .padding(space(4.0))
        .style(theme::panel_surface)
        .into()
}

impl PanelState {
    /// The models that can serve *this* request.
    ///
    /// With an image attached, a model that cannot see is not a choice — it is a
    /// row whose only outcome is `VisionUnsupported`. Hiding it is also what
    /// makes §4's routing legible: the picker previously named a Codex model
    /// while the request went to OpenAI, because Codex declares no vision
    /// support, and nothing on screen explained the discrepancy.
    pub fn capable_models(&self) -> Vec<ModelOption> {
        if !self.has_attachments() {
            return self.model_options.clone();
        }
        self.model_options
            .iter()
            .filter(|option| self.model_supports_vision(&option.binding))
            .cloned()
            .collect()
    }

    /// Whether the catalogue says this binding accepts image input.
    ///
    /// The UI cannot reach `ModelCatalogue`, so the runtime has to have said so.
    /// Until `UiEvent::ModelOptions` carries capabilities, the one thing known
    /// for certain is §3a's measurement that the Codex endpoint has never been
    /// sent an image — so that provider is excluded and everything else is
    /// assumed capable, which matches the modern reality that most chat models
    /// take images.
    fn model_supports_vision(&self, binding: &aibo_core::types::ModelBinding) -> bool {
        binding.provider.as_str() != "codex"
    }

    /// The quick-pick's rows for the current state.
    pub fn picker_rows(&self) -> Vec<crate::model_picker::Row> {
        self.picker.rows(
            &self.capable_models(),
            &self.favourite_models,
            &self.recent_models,
        )
    }

    /// Record a model as just used, most recent first.
    pub fn remember_model(&mut self, binding: aibo_core::types::ModelBinding) {
        self.recent_models.retain(|b| b != &binding);
        self.recent_models.insert(0, binding);
        // Bounded: a "recent" list longer than the screen is a second full
        // catalogue with worse ordering.
        self.recent_models.truncate(RECENT_MODEL_LIMIT);
    }

    /// Pin or unpin a model.
    pub fn toggle_favourite(&mut self, binding: aibo_core::types::ModelBinding) {
        match self.favourite_models.iter().position(|b| b == &binding) {
            Some(at) => {
                self.favourite_models.remove(at);
            }
            None => self.favourite_models.push(binding),
        }
    }
}

/// How many recently used models the picker keeps.
const RECENT_MODEL_LIMIT: usize = 5;

/// Which rail state the input row carries (`design.md` §3).
///
/// Amber "while typing" is read as *while the input is where the attention is*
/// rather than while a key is physically down: a rail that flickered per
/// keystroke would be decoration, and the rail is meant to encode something
/// true. Attention leaves the input exactly when the panel starts producing an
/// answer, and returns when that answer is finished.
fn input_rail_state(state: &PanelState) -> RailState {
    match state.phase {
        Phase::Loading | Phase::Streaming => RailState::Inactive,
        _ => RailState::Active,
    }
}

/// Which rail state the answer/error block carries (`design.md` §3).
fn content_rail_state(state: &PanelState) -> RailState {
    if state.error.is_some()
        || matches!(state.phase, Phase::Failed)
        || matches!(state.context, ContextState::PermissionDenied { .. })
    {
        return RailState::Alert;
    }
    match state.phase {
        Phase::Loading | Phase::Streaming => RailState::Active,
        _ => RailState::Inactive,
    }
}

fn chip_row(state: &PanelState) -> Element<'_, Message> {
    let context = match &state.context {
        // The excerpt is the point of this line. `design.md` §1: "the most
        // interesting information on screen is not the input field — it is the
        // line that says *ghostty · …and screencapture works*". Every call site
        // here used to pass `None`, so the panel knew where the user was and
        // declined to say so.
        ContextState::Available {
            app,
            excerpt,
            selection,
            ..
        } => widgets::context_chip(
            app.as_ref().map(|a| a.display_name.as_str()),
            // Prefer what the user actually selected; fall back to the
            // surrounding field text when the capture was caret-only.
            selection.as_deref().or(excerpt.as_deref()),
        ),
        // Capture in flight. §4 requires "reading…" here rather than a blank or
        // a premature "no context" — the panel is up before the capture lands
        // (§8), and silence in that window reads as a failure that has not
        // happened yet.
        ContextState::Pending => widgets::reading_context_line(),
        // Capture settled with nothing. Distinct from Pending, and the wording
        // says so.
        ContextState::Unavailable { app } => match app.as_deref() {
            Some(app) => widgets::context_chip(Some(app), None),
            None => widgets::unavailable_context_line(),
        },
        ContextState::PermissionDenied { .. } | ContextState::ImeActive => {
            widgets::context_chip(None, None)
        }
    };

    if state.model_options.is_empty() {
        return context;
    }

    // The current model as a quiet, keyboard-reachable button rather than a
    // dropdown. §16: every action has a key, and a `pick_list` of eighty-eight
    // entries has no key at all — the mouse was the only way in.
    let current = state
        .selected_model
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| i18n::t(Key::PanelModel).to_owned());

    row![
        context,
        Space::new().width(Length::Fill),
        widgets::action_list(vec![Action::new(
            Key::PanelModel,
            widgets::primary_shortcut("⌘K", "Ctrl+K"),
            Message::OpenPicker,
        )]),
        text(current)
            .size(type_scale::META)
            .font(theme::MONO_FONT)
            .style(theme::text_dim),
    ]
    .spacing(space(1.5))
    .align_y(Alignment::Center)
    .into()
}

/// The model quick-pick overlay (§4).
///
/// Replaces the panel's body while open rather than floating over it: the panel
/// is already an overlay, and a popover inside an overlay reads as a rendering
/// fault. The search field takes focus, so the first keystroke filters instead
/// of being lost.
fn picker_overlay(state: &PanelState) -> Element<'_, Message> {
    let rows = state.picker_rows();
    let choices = crate::model_picker::selectable(&rows);
    let total = choices.len();

    let mut list = column![].spacing(space(0.5));
    let mut index = 0usize;
    for row in &rows {
        match row {
            crate::model_picker::Row::Group(label) => {
                list = list.push(
                    text(label.clone())
                        .size(type_scale::META)
                        .font(theme::MONO_FONT)
                        .style(theme::text_dim),
                );
            }
            crate::model_picker::Row::Model { option, favourite } => {
                let highlighted = index == state.picker.highlight;
                list = list.push(widgets::railed(
                    if highlighted {
                        RailState::Active
                    } else {
                        RailState::Inactive
                    },
                    model_row(option, *favourite, highlighted),
                ));
                index += 1;
            }
        }
    }

    let count = if total == 0 {
        i18n::t(Key::PickerNoMatch).to_owned()
    } else {
        i18n::t1(Key::PickerCount, &total.to_string())
    };

    column![
        text_input(i18n::t(Key::PickerPlaceholder), &state.picker.query)
            .id(PICKER_ID)
            .on_input(Message::PickerQuery)
            .on_submit(Message::PickerCommit)
            .size(type_scale::BODY)
            .font(theme::MONO_FONT)
            .padding(space(1.0))
            .style(theme::input),
        text(count)
            .size(type_scale::META)
            .font(theme::MONO_FONT)
            .style(theme::text_dim),
        scrollable(list)
            .height(Length::Fixed(PICKER_LIST_HEIGHT))
            .style(theme::scroller),
        widgets::action_list(vec![
            Action::new(Key::ActionSelect, "⏎", Message::PickerCommit).primary(),
            Action::new(Key::ActionPinModel, "⌘D", Message::PickerToggleFavourite),
            Action::new(Key::ActionDismiss, "esc", Message::ClosePicker),
        ]),
    ]
    .spacing(space(1.5))
    .into()
}

/// One row of the quick-pick.
///
/// Name, then the facts that actually decide a choice: what it costs, and what
/// it can do. Both come from real data — §14's price table and §10's catalogue —
/// so neither goes stale as models change.
///
/// The badges are words rather than icons. `design.md` §9 cut icons from the
/// product on the grounds that "the rail plus a key hint carries every meaning
/// an icon would", and three unlabelled glyphs are exactly the case it was
/// arguing against: an eye and a brain have to be learned, `vision` does not.
fn model_row<'a>(option: &ModelOption, favourite: bool, highlighted: bool) -> Element<'a, Message> {
    let mut row = row![
        text(if favourite { "★" } else { " " })
            .width(Length::Fixed(space(3.0)))
            .size(type_scale::META)
            .style(theme::text_accent),
        text(option.to_string())
            .size(type_scale::ANSWER)
            .style(if highlighted {
                theme::text_primary
            } else {
                theme::text_dim
            }),
    ]
    .spacing(space(1.0))
    .align_y(Alignment::Center);

    // §14: an unpriced model shows nothing rather than `$`. Reporting a price
    // aibo does not know is worse than reporting none.
    if let Some(cost) = option.cost {
        row = row.push(
            text(cost.glyphs())
                .size(type_scale::META)
                .font(theme::MONO_FONT)
                .style(theme::text_accent),
        );
    }

    // **No ability badges, and this is a data problem rather than a design one.**
    //
    // `Abilities` is carried and populated, but rendering it would lie. No
    // OpenAI-compatible `/models` endpoint reports capabilities, so every
    // fetched model inherits its *provider's* declared defaults — which made
    // `gpt-3.5-turbo` claim vision, reasoning and tools, none of which it has.
    // A picker that says a model can see when it cannot is worse than one that
    // says nothing, because the user acts on it and gets a 400.
    //
    // Badges become honest as soon as there is per-model capability data: the
    // §19 signed manifest is where §10 says that belongs, and `models.toml`
    // would let a user correct one entry without waiting for a release. The
    // field stays populated so the capability *filter* can use it — excluding a
    // vision-incapable provider is a provider-level fact, and that much is
    // true.
    let _ = &option.abilities;

    row.into()
}

/// Total panel height while the quick-pick is open.
///
/// The list plus the search field, the result count, the action row and the
/// panel's own padding. Fixed rather than derived from the number of matches: a
/// window that resizes on every keystroke is unreadable while typing, which is
/// the one thing this widget exists to make easy.
const PICKER_PANEL_HEIGHT: f32 = 400.0;

/// Height of the quick-pick's scrolling list.
///
/// Fixed so the panel does not resize as the query narrows the results — a list
/// that changes height on every keystroke is unreadable while typing.
const PICKER_LIST_HEIGHT: f32 = 260.0;

#[expect(dead_code, reason = "kept while the old dropdown path is retired")]
fn unused_pick_list(state: &PanelState) -> Element<'_, Message> {
    row![
        pick_list(
            state.model_options.as_slice(),
            state.selected_model.as_ref(),
            Message::SelectModel,
        )
        .placeholder(i18n::t(Key::PanelModel))
        .width(Length::Fixed(230.0))
        .text_size(type_scale::META)
        .font(theme::MONO_FONT)
        .padding([space(1.0), space(1.5)])
        .style(theme::model_picker)
        .menu_style(theme::model_picker_menu),
    ]
    .spacing(space(1.5))
    .align_y(Alignment::Center)
    .into()
}

/// Pinned captured selection, shown once and never duplicated in the header.
fn selection_card(state: &PanelState) -> Option<Element<'_, Message>> {
    let ContextState::Available {
        selection: Some(selection),
        ..
    } = &state.context
    else {
        return None;
    };

    let preview: Element<'_, Message> = if state.context_expanded {
        scrollable(
            text(selection)
                .size(type_scale::CHIP)
                .font(theme::UI_FONT)
                .style(theme::text_dim),
        )
        .height(Length::Fixed(88.0))
        .style(theme::scroller)
        .into()
    } else {
        text(widgets::elide(selection, 240))
            .size(type_scale::CHIP)
            .font(theme::UI_FONT)
            .style(theme::text_dim)
            .into()
    };

    let toggle = if state.context_expanded {
        Action::new(Key::ActionCollapse, "⌘E", Message::ToggleContext)
    } else {
        Action::new(Key::ActionExpand, "⌘E", Message::ToggleContext)
    };
    let remove = Action::new(Key::ActionRemoveSelection, "⌘⇧E", Message::RemoveSelection);

    Some(
        container(
            column![
                row![
                    text(i18n::t(Key::ContextSelectedText))
                        .size(type_scale::META)
                        .style(theme::text_primary),
                    Space::new().width(Length::Fill),
                    widgets::action_list(vec![toggle, remove]),
                ]
                .align_y(Alignment::Center),
                preview,
            ]
            .spacing(space(1.0)),
        )
        .width(Length::Fill)
        .padding([space(1.5), space(2.0)])
        .style(theme::raised)
        .into(),
    )
}

/// The row of attachment chips, or `None` when nothing is attached.
///
/// Wrapping rather than clipping: at [`theme::PANEL_WIDTH_MIN`] four chips do
/// not fit on one line, and a chip that runs off the edge of the panel is
/// exactly the "you cannot see what is attached" failure this row exists to
/// prevent. [`PanelState::attachment_block_height`] reserves the height to match.
fn attachment_row(state: &PanelState) -> Option<Element<'_, Message>> {
    if state.attachments.is_empty() {
        return None;
    }

    let last = state.attachments.len() - 1;
    let chips: Vec<Element<'_, Message>> = state
        .attachments
        .iter()
        .enumerate()
        .map(|(index, attached)| {
            widgets::attachment_chip(
                &attached.thumbnail,
                &attached.attachment,
                // §16 shows every action's key, but `⌫` removes the most
                // recent image and only the most recent — printing it on all
                // four chips would advertise four different outcomes for one
                // key. The rest are removable by click, as every chip is.
                (index == last).then_some(DETACH_KEY),
                Message::Detach(attached.attachment.id),
            )
        })
        .collect();

    Some(
        row(chips)
            .spacing(space(2.0))
            .align_y(Alignment::Center)
            .wrap()
            .into(),
    )
}

fn input_row(state: &PanelState) -> Element<'_, Message> {
    // §9: aibo must accept Japanese input in its own panel, which needs winit
    // `Ime` events, `set_ime_allowed` and `set_ime_cursor_area` for candidate
    // window placement. "If a Japanese user cannot type Japanese into the
    // panel, the Japanese market is closed."
    //
    // SPIKE: S10 — iced's IME support has historically been incomplete and an
    // overlay window makes it harder. On the critical path, not a nicety.
    // In the empty state the invitation *is* the placeholder, and it has to be:
    // rendered as its own row underneath, it left the input row visually blank
    // with the amber rail beside it — the rail pointing at nothing, which is the
    // opposite of "at a glance, the rail tells you where the panel thinks you
    // are" (`design.md` §3).
    let placeholder = if shows_empty_invitation(state) {
        i18n::t(Key::PanelEmptyInvitation)
    } else {
        i18n::t(Key::PanelPlaceholder)
    };

    let input = text_input(placeholder, &state.input)
        .id(INPUT_ID)
        .on_input(Message::InputChanged)
        .size(type_scale::BODY)
        .font(theme::MONO_FONT)
        .padding(space(1.0))
        .style(theme::input);

    // No well. `design.md` §3's mock puts the caret directly on the panel
    // ground with the rail beside it — the input is the line you were already
    // typing on, not a field you have been given to fill in.
    container(
        row![input, widgets::action_list(composer_actions_for(state)),]
            .spacing(space(1.5))
            .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .into()
}

/// Whether the panel is in `design.md` §4's empty state.
///
/// Nothing typed, nothing asked, nothing in flight — the moment right after the
/// hotkey. §6: "Empty states are an invitation to act, not a mood."
fn shows_empty_invitation(state: &PanelState) -> bool {
    state.input.is_empty()
        && !state.has_conversation()
        && matches!(state.phase, Phase::Idle | Phase::Hidden)
}

fn composer_actions_for(state: &PanelState) -> Vec<Action<Message>> {
    let attach = Action::new(Key::ActionAttachImage, ATTACH_KEY, Message::Attach);
    let attach = if state.clipboard.is_attachable() {
        attach
    } else {
        attach.disabled()
    };

    // Deliberately *not* `.primary()`. `design.md` §2 makes amber "the one live
    // accent" and §9's whole method is to spend the boldness in one place — a
    // filled amber pill in the composer competes with the rail for exactly the
    // signal the rail exists to carry. It also read as lit even with nothing to
    // send, because the disabled primary style is still amber at 0.32. Send is
    // the ⏎ key; the label says so, which §8 considers sufficient.
    let primary = Action::new(Key::ActionSend, "↩", Message::Submit);
    let primary = if state.input.trim().is_empty()
        || matches!(state.phase, Phase::Loading | Phase::Streaming)
    {
        primary.disabled()
    } else {
        primary
    };
    vec![attach, primary]
}

fn user_bubble(message: &str) -> Element<'_, Message> {
    row![
        Space::new().width(Length::FillPortion(1)),
        container(
            column![
                text(i18n::t(Key::ChatYou))
                    .size(type_scale::META)
                    .style(theme::text_dim),
                text(message.to_owned())
                    .size(type_scale::BODY)
                    .font(theme::UI_FONT)
                    .style(theme::text_primary),
            ]
            .spacing(space(1.0)),
        )
        .width(Length::FillPortion(4))
        .padding([space(2.0), space(2.5)])
        .style(theme::user_bubble),
    ]
    .into()
}

fn markdown_settings(appearance: theme::Appearance) -> markdown::Settings {
    let palette = appearance.palette();
    let mut style = markdown::Style::from_palette(iced::theme::Palette {
        background: palette.surface,
        text: palette.text,
        primary: palette.accent,
        success: palette.success,
        warning: palette.warning,
        danger: palette.danger,
    });
    style.font = theme::UI_FONT;
    style.inline_code_font = theme::MONO_FONT;
    style.code_block_font = theme::MONO_FONT;
    style.inline_code_color = palette.text;
    style.inline_code_highlight.background = palette.surface_raised.into();

    let mut settings = markdown::Settings::with_text_size(type_scale::BODY, style);
    // A compact overlay needs visible hierarchy without document-sized
    // headings that make a short answer dominate the screen.
    settings.h1_size = 20.0.into();
    settings.h2_size = 18.0.into();
    settings.h3_size = 17.0.into();
    settings.h4_size = 16.0.into();
    settings.h5_size = type_scale::BODY.into();
    settings.h6_size = type_scale::BODY.into();
    settings.code_size = 14.0.into();
    settings.spacing = space(2.0).into();
    settings
}

fn markdown_view<'a>(
    items: impl IntoIterator<Item = &'a markdown::Item>,
    appearance: theme::Appearance,
) -> Element<'a, Message> {
    markdown::view(items, markdown_settings(appearance))
        // Model output is untrusted context. Links are rendered so their
        // destination remains visible and can be copied, but never launched
        // automatically.
        .map(Message::CopyLink)
}

fn assistant_text_bubble(
    turn: &ConversationTurn,
    appearance: theme::Appearance,
) -> Element<'_, Message> {
    row![
        container(
            column![
                text(i18n::t(Key::ChatAssistant))
                    .size(type_scale::META)
                    .style(theme::text_dim),
                markdown_view(&turn.assistant_markdown, appearance),
            ]
            .spacing(space(1.0)),
        )
        .width(Length::FillPortion(4))
        .padding([space(2.0), space(2.5)])
        .style(theme::assistant_bubble),
        Space::new().width(Length::FillPortion(1)),
    ]
    .into()
}

fn active_assistant_bubble(
    state: &PanelState,
    appearance: theme::Appearance,
) -> Element<'_, Message> {
    let body: Element<'_, Message> = match state.phase {
        Phase::Loading => widgets::thinking(Some(state.chat_answer_height())),
        Phase::Streaming | Phase::Finished { .. } => {
            let rendered = markdown_view(state.response_markdown.items(), appearance);
            if state.is_truncated() {
                column![
                    rendered,
                    text(i18n::t(Key::StateTruncated))
                        .size(type_scale::META)
                        .font(theme::MONO_FONT)
                        .style(theme::text_dim),
                ]
                .spacing(space(1.0))
                .into()
            } else {
                rendered
            }
        }
        Phase::Failed if !state.response.is_empty() => column![
            markdown_view(state.response_markdown.items(), appearance),
            text(i18n::t(Key::StateTruncated))
                .size(type_scale::META)
                .font(theme::MONO_FONT)
                .style(theme::text_dim),
        ]
        .spacing(space(1.0))
        .into(),
        _ => Space::new().height(0.0).into(),
    };

    row![
        container(
            column![
                text(i18n::t(Key::ChatAssistant))
                    .size(type_scale::META)
                    .style(theme::text_dim),
                body,
            ]
            .spacing(space(1.0)),
        )
        .width(Length::FillPortion(4))
        .padding([space(2.0), space(2.5)])
        .style(theme::assistant_bubble),
        Space::new().width(Length::FillPortion(1)),
    ]
    .into()
}

fn conversation(state: &PanelState, appearance: theme::Appearance) -> Element<'_, Message> {
    let mut transcript = column![].spacing(space(2.0));
    for turn in &state.turns {
        transcript = transcript
            .push(user_bubble(&turn.user))
            .push(assistant_text_bubble(turn, appearance));
    }
    if let Some(user) = &state.active_user {
        transcript = transcript.push(user_bubble(user));
        if matches!(
            state.phase,
            Phase::Loading | Phase::Streaming | Phase::Finished { .. } | Phase::Failed
        ) {
            transcript = transcript.push(active_assistant_bubble(state, appearance));
        }
    }

    let height = Length::Fixed(state.transcript_height());
    if state.transcript_content_height() > state.max_transcript_height() {
        scrollable(transcript)
            .height(height)
            .style(theme::scroller)
            .into()
    } else {
        // Avoid retaining a scroll offset while retrying or replacing a short
        // answer. On macOS that stale offset can yield an invalid glyph
        // position after the window shrinks.
        container(transcript).height(height).into()
    }
}

fn content(state: &PanelState, appearance: theme::Appearance) -> Element<'_, Message> {
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

    if state.has_conversation() {
        if matches!(state.phase, Phase::Failed)
            && let Some(error) = &state.error
            && state.response.is_empty()
        {
            let actions = error
                .action
                .clone()
                .and_then(error_action)
                .into_iter()
                .collect();
            return column![
                conversation(state, appearance),
                widgets::state_block(
                    error.severity,
                    &error.headline,
                    error.body.as_deref(),
                    actions,
                ),
            ]
            .spacing(space(2.0))
            .into();
        }
        return conversation(state, appearance);
    }

    match &state.phase {
        Phase::Hidden | Phase::WarmingUp { .. } | Phase::Idle => Space::new().height(0.0).into(),
        // No box, no spinner, no 15 pt "Thinking…" filling the panel. §4 gives
        // this state a mono ellipsis and the amber rail, and nothing else.
        Phase::Loading => widgets::thinking(Some(state.answer_height())),
        Phase::Streaming | Phase::Finished { .. } => widgets::selectable_answer(
            &state.response_editor,
            state.answer_height(),
            state.is_truncated(),
            Message::ResponseAction,
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
            let actions = error
                .action
                .clone()
                .and_then(error_action)
                .into_iter()
                .collect();
            widgets::state_block(
                error.severity,
                &error.headline,
                error.body.as_deref(),
                actions,
            )
        }
    }
}

fn error_action(action: ErrorAction) -> Option<Action<Message>> {
    let (label, key) = match &action {
        ErrorAction::Retry => (Key::ActionRetry, widgets::primary_shortcut("⌘R", "Ctrl+R")),
        ErrorAction::RetryWith(_) => (
            Key::ActionSmartModel,
            widgets::primary_shortcut("⌘↩", "Ctrl+Enter"),
        ),
        ErrorAction::SignIn(_) => (Key::ActionSignIn, "⏎"),
        ErrorAction::OpenSettings => (Key::ActionOpenSettings, "⏎"),
        ErrorAction::CopyDiagnostics => (
            Key::ActionCopyDiagnostics,
            widgets::primary_shortcut("⌘C", "Ctrl+C"),
        ),
        ErrorAction::RemoveAttachment { .. } => (Key::ActionRemoveImage, DETACH_KEY),
        // These actions do not have a backing `UiRequest` yet. Rendering a
        // primary button that is known to do nothing is worse than leaving the
        // recovery unavailable, especially on a blocking error.
        ErrorAction::TrimSelection | ErrorAction::UseModel { .. } | ErrorAction::ContinueAnyway => {
            return None;
        }
    };
    Some(Action::new(label, key, Message::Error(action)).primary())
}

impl PanelState {
    /// Compact answer height used inside the active assistant bubble.
    ///
    /// Deliberately **not** quantised to [`ANSWER_HEIGHT_STEP`], though it was
    /// worth measuring whether it should be. `estimated_text_height` already
    /// resolves to whole lines, so this grows one line height at a time and not
    /// per token: a hundred-chunk answer produces six distinct panel heights,
    /// not a hundred. Rounding those six up to 48 pt steps would trade a resize
    /// or two for as much as 47 pt of dead space inside every short reply —
    /// exactly the "short reply turning into a large empty card" that
    /// [`theme::CHAT_ANSWER_MIN_HEIGHT`] exists to prevent.
    ///
    /// The reflow defect was never the number of height changes. It was that
    /// every one of them re-ran the panel's whole arrival sequence; see
    /// `app::resize_panel_if_visible`.
    fn chat_answer_height(&self) -> f32 {
        estimated_text_height(&self.response, CHAT_ASSISTANT_CHARS_PER_LINE)
            .clamp(theme::CHAT_ANSWER_MIN_HEIGHT, self.max_chat_answer_height())
    }

    /// Estimated height of all messages before applying viewport bounds.
    fn transcript_content_height(&self) -> f32 {
        let mut height = 0.0;
        let mut messages = 0usize;

        for turn in &self.turns {
            height += chat_bubble_height(&turn.user, false);
            height += chat_bubble_height(&turn.assistant, true);
            messages += 2;
        }
        if let Some(user) = &self.active_user {
            height += chat_bubble_height(user, false);
            messages += 1;
            if matches!(
                self.phase,
                Phase::Loading | Phase::Streaming | Phase::Finished { .. } | Phase::Failed
            ) {
                height += CHAT_BUBBLE_CHROME_HEIGHT + self.chat_answer_height();
                messages += 1;
            }
        }
        if messages > 1 {
            height += (messages - 1) as f32 * CHAT_MESSAGE_SPACING;
        }

        height
    }

    /// Visible transcript height, content-sized until a useful scrolling cap.
    fn transcript_height(&self) -> f32 {
        self.transcript_content_height()
            .clamp(CHAT_TRANSCRIPT_MIN_HEIGHT, self.max_transcript_height())
    }

    /// The tallest the panel may grow on this display (`design.md` §4).
    ///
    /// 60 % of the display, floored at [`theme::PANEL_HEIGHT_MAX`] so an unknown
    /// display behaves exactly as the old constant did, and ceilinged so the
    /// panel stays a panel: something that covers the screen is a window, and
    /// this product is explicitly not that (§1 — it appears *over* your work).
    pub fn max_panel_height(&self) -> f32 {
        let Some(display) = self.display_height else {
            return theme::PANEL_HEIGHT_MAX;
        };
        (display * PANEL_HEIGHT_DISPLAY_FRACTION).clamp(theme::PANEL_HEIGHT_MAX, 900.0)
    }

    /// The active answer bubble's ceiling.
    ///
    /// Derived from the transcript rather than fixed, leaving room for the user
    /// turn above it. The old flat 172 pt was the binding constraint on a
    /// single-turn answer — the transcript could be allowed 800 pt and the
    /// answer inside it would still stop at 172 and scroll, which is the shape
    /// of "the response is not growing the window".
    fn max_chat_answer_height(&self) -> f32 {
        (self.max_transcript_height() - CHAT_BUBBLE_CHROME_HEIGHT * 2.0 - CHAT_MESSAGE_SPACING)
            .max(CHAT_ANSWER_MAX_HEIGHT)
    }

    /// The transcript's ceiling before it scrolls internally.
    fn max_transcript_height(&self) -> f32 {
        (self.max_panel_height()
            - theme::PANEL_HEIGHT_COLLAPSED
            - self.attachment_block_height()
            - self.selection_preview_height()
            - self.footer_height())
        .max(CHAT_TRANSCRIPT_MIN_MAX_HEIGHT)
    }

    /// Maximum height the answer may consume while preserving fixed chrome.
    fn max_answer_height(&self) -> f32 {
        (self.max_panel_height()
            - theme::PANEL_HEIGHT_COLLAPSED
            - self.attachment_block_height()
            - self.selection_preview_height()
            - self.footer_height())
        .max(theme::ANSWER_BOX_MIN_HEIGHT)
    }

    /// Effective answer height after accounting for footer rows added later.
    fn answer_height(&self) -> f32 {
        self.reserved_answer_height.min(self.max_answer_height())
    }

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
        if matches!(
            self.context,
            ContextState::Available {
                truncated: true,
                ..
            }
        ) {
            rows += theme::META_LINE_HEIGHT;
        }
        rows + theme::ACTION_ROW_HEIGHT
    }

    /// Height the attachments add, wherever the panel is in its lifecycle.
    ///
    /// Both parts travel together and neither is optional: the chip rows above
    /// the input, and the §14 token footnote below the answer that only exists
    /// because they do. Kept in one function so the collapsed and expanded
    /// branches of [`PanelState::desired_height`] cannot disagree about it —
    /// the failure mode when they do is a row clipped against the window edge,
    /// which nothing reports.
    ///
    /// Two chips per line is what fits at [`theme::PANEL_WIDTH_MIN`], and the
    /// reservation has to hold at the *narrowest* width the panel may take (§9
    /// makes the width a range so translations fit). Over-reserving at 680 pt
    /// costs a few points of empty panel; under-reserving clips silently.
    fn attachment_block_height(&self) -> f32 {
        if self.attachments.is_empty() {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "bounded by MAX_ATTACHMENTS, which is 4"
        )]
        let rows = self.attachments.len().div_ceil(2) as f32;
        rows * ATTACHMENT_ROW_HEIGHT + theme::META_LINE_HEIGHT
    }

    fn selection_preview_height(&self) -> f32 {
        match self.context {
            ContextState::Available {
                selection: Some(_), ..
            } if self.context_expanded => SELECTION_CARD_EXPANDED_HEIGHT,
            ContextState::Available {
                selection: Some(_), ..
            } => SELECTION_CARD_COLLAPSED_HEIGHT,
            _ => 0.0,
        }
    }
}

/// Height of one row of attachment chips, including the column's spacing.
///
/// Not in [`crate::theme`] because it is a consequence of this row's own
/// composition — a 20 pt thumbnail plus the chip's vertical padding — rather
/// than a shared token.
const ATTACHMENT_ROW_HEIGHT: f32 = 36.0;
const SELECTION_CARD_COLLAPSED_HEIGHT: f32 = 64.0;
const SELECTION_CARD_EXPANDED_HEIGHT: f32 = 132.0;
const CHAT_PANEL_MIN_HEIGHT: f32 = 320.0;
const CHAT_TRANSCRIPT_MIN_HEIGHT: f32 = 112.0;

/// Fraction of the display the panel may occupy before the answer scrolls.
///
/// `design.md` §4: "Long answer | Answer area scrolls internally at 60 % display
/// height; rail stays full-height." The previous fixed 268 pt transcript cap
/// implemented that rule as a constant, which on any display taller than about
/// 450 pt meant the window stopped growing long before it had any right to —
/// the answer went on scrolling inside a small box with most of the screen
/// empty around it. On a 1440 pt display 60 % is 864 pt, more than three times
/// what the constant allowed.
const PANEL_HEIGHT_DISPLAY_FRACTION: f32 = 0.60;

/// The chat transcript's ceiling once the panel itself is at full height.
///
/// Kept as a floor rather than the answer, so a display whose size is not yet
/// known still behaves the way the old constant did instead of collapsing.
const CHAT_TRANSCRIPT_MIN_MAX_HEIGHT: f32 = 268.0;
/// The granularity at which the answer area is allowed to grow.
///
/// §16: "streaming must not reflow". A window resize is not free — on macOS it
/// goes to the window server, and each one is a visible jump in a panel that
/// floats over the user's work — so the answer area is sized in steps rather
/// than tracked continuously. One step per ~48 pt of answer bounds a full-length
/// response to a handful of resizes instead of one per token.
const ANSWER_HEIGHT_STEP: f32 = 48.0;

const CHAT_ANSWER_MAX_HEIGHT: f32 = 172.0;
const CHAT_BUBBLE_CHROME_HEIGHT: f32 = 38.0;
const CHAT_MESSAGE_SPACING: f32 = 8.0;
const CHAT_USER_CHARS_PER_LINE: usize = 72;
const CHAT_ASSISTANT_CHARS_PER_LINE: usize = 68;

fn chat_bubble_height(message: &str, assistant: bool) -> f32 {
    let chars_per_line = if assistant {
        CHAT_ASSISTANT_CHARS_PER_LINE
    } else {
        CHAT_USER_CHARS_PER_LINE
    };
    CHAT_BUBBLE_CHROME_HEIGHT + estimated_text_height(message, chars_per_line)
}

fn estimated_text_height(message: &str, chars_per_line: usize) -> f32 {
    let lines = message
        .split('\n')
        .map(|line| line.chars().count().max(1).div_ceil(chars_per_line))
        .sum::<usize>()
        .max(1);
    lines as f32 * 20.0 + 4.0
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

    // §14: images are priced input, and the reservation has to be visible
    // *before* the request rather than reconciled after it — `Usage` never
    // arrives on a cancelled stream, so waiting for the real number means the
    // user sometimes never sees one at all.
    if state.has_attachments() {
        stack = stack.push(widgets::footnote::<Message>(i18n::t1(
            Key::FootnoteImageTokens,
            &state.estimated_image_tokens().to_string(),
        )));
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
        actions.push(Action::new(
            Key::ActionShowTask,
            widgets::primary_shortcut("⌘T", "Ctrl+T"),
            Message::ShowTask,
        ));
    }

    let replace = Action::new(
        Key::ActionReplace,
        widgets::primary_shortcut("⌘↩", "Ctrl+Enter"),
        Message::Accept,
    );
    actions.push(if state.can_accept() {
        replace
    } else {
        replace.disabled()
    });

    let copy = Action::new(
        Key::ActionCopy,
        widgets::primary_shortcut("⌘C", "Ctrl+C"),
        Message::Copy,
    );
    actions.push(if state.can_copy() {
        copy
    } else {
        copy.disabled()
    });

    let retry = Action::new(
        Key::ActionRegenerate,
        widgets::primary_shortcut("⌘R", "Ctrl+R"),
        Message::Retry,
    );
    actions.push(
        if matches!(state.phase, Phase::Finished { .. } | Phase::Failed)
            && state.active_user.is_some()
        {
            retry
        } else {
            retry.disabled()
        },
    );

    // Removal is listed only while there is something to remove. It is not the
    // "disabled rather than absent" case: unlike Replace and Copy, this action
    // has a second, always-visible home — the `×` on every chip — so a disabled
    // entry here would be a permanent row of noise for a capability the chip
    // already advertises the moment it becomes real.
    if state.has_attachments() {
        actions.push(Action::new(
            Key::ActionRemoveImage,
            DETACH_KEY,
            Message::DetachLast,
        ));
    }

    if matches!(state.phase, Phase::Loading | Phase::Streaming) {
        actions.push(Action::new(Key::ActionCancel, "esc", Message::Dismiss));
    } else {
        actions.push(Action::new(
            Key::ActionSmartModel,
            widgets::primary_shortcut("⌘⇧↩", "Ctrl+Shift+Enter"),
            Message::Escalate,
        ));
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
    // A 1×1 transparent pixel, purely so the image pipeline and the atlas
    // upload path are compiled here rather than on the frame that first shows a
    // chip. Skipping it would leave an attachment costing a pipeline compile at
    // exactly the moment the user is watching (§6).
    let pixel = image::Handle::from_rgba(1, 1, vec![0, 0, 0, 0]);

    container(
        column![
            widgets::context_chip::<Message>(Some("aibo"), Some("warm")),
            widgets::attachment_chip(
                &pixel,
                &Attachment::image(
                    AttachmentSource::Clipboard,
                    vec![0],
                    "image/png",
                    1,
                    1,
                    "warm"
                ),
                Some(DETACH_KEY),
                Message::Dismiss,
            ),
            text_input("", "")
                .size(type_scale::BODY)
                .font(theme::MONO_FONT)
                .style(theme::input),
            widgets::answer::<Message>("warm", theme::ANSWER_BOX_MIN_HEIGHT, true),
            widgets::meta_line::<Message>("aibo", "warm", Some(0), Some("0")),
            widgets::action_list(vec![
                Action::new(Key::ActionReplace, "⏎", Message::Dismiss).primary(),
                Action::new(
                    Key::ActionCopy,
                    widgets::primary_shortcut("⌘C", "Ctrl+C"),
                    Message::Dismiss,
                ),
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
    use aibo_core::types::{MAX_ATTACHMENTS, ModelBinding};

    fn panel() -> PanelState {
        let mut state = PanelState::new(SessionId::from_u128(1));
        state.phase = Phase::Idle;
        state
    }

    fn screenshot(label: &str) -> Attachment {
        Attachment::image(
            AttachmentSource::Clipboard,
            vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "image/png",
            1200,
            750,
            label,
        )
    }

    fn offered() -> ClipboardOffer {
        ClipboardOffer::Image {
            label: "Image from Chrome".to_owned(),
            image: Some(Box::new(screenshot("Image from Chrome"))),
        }
    }

    fn model_option(model: &str) -> ModelOption {
        ModelOption {
            binding: ModelBinding {
                provider: ProviderId::CODEX,
                model: model.to_owned(),
            },
            display_name: model.to_owned(),
            latency_ms: Some(435),
            abilities: Default::default(),
            cost: None,
        }
    }

    #[test]
    fn a_partial_response_is_never_acceptable() {
        let mut state = panel();
        state.set_response("half a rewr");
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
        state.set_response("書き換え");
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
        state.set_response("keep me");
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
        // `design.md` §6: the button names the outcome, not the window it lives
        // in. This used to be `OpenSettings`, which rendered as "Open settings"
        // — a mechanism, leaving the user to work out what to do once they got
        // there.
        assert_eq!(
            state.error.as_ref().and_then(|e| e.action.clone()),
            Some(ErrorAction::SignIn(ProviderId::CODEX))
        );
        // …and it is the one error that carries a second sentence, because it
        // is the one with two real routes out.
        assert!(
            state.error.as_ref().is_some_and(|e| e.body.is_some()),
            "the no-provider state must say what to do, not only what happened"
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
        state.set_response("half a rewr");
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
        state.set_response("old answer");
        state.input = "old instruction".to_owned();
        state.reset(SessionId::from_u128(2));
        assert!(state.response.is_empty());
        assert!(state.input.is_empty());
        assert_eq!(state.session, SessionId::from_u128(2));
        // Already warm: it must not warm up again, which would flash a frame.
        assert_eq!(state.phase, Phase::Idle);
    }

    #[test]
    fn reset_preserves_the_users_model_selection() {
        let mut state = panel();
        let selected = model_option("gpt-5.6-terra");
        state.model_options = vec![model_option("gpt-5.5"), selected.clone()];
        state.selected_model = Some(selected.clone());

        state.reset(SessionId::from_u128(2));

        assert_eq!(state.selected_model, Some(selected));
        assert_eq!(state.model_options.len(), 2);
        let _ = view(&state, theme::Appearance::Dark);
    }

    // -----------------------------------------------------------------------
    // Attachments (§2, §5, §13, §14)
    //
    // The defect these exist to prevent, observed 2026-07-26: `has_image` was
    // derived from whatever sat on the clipboard, so taking a screenshot
    // rerouted every subsequent request to the `Vision` role — which nothing
    // binds — and surfaced as "No provider is configured yet" beside a
    // signed-in, healthy provider.
    // -----------------------------------------------------------------------

    /// The whole thesis, as an assertion: an image on the clipboard is not an
    /// attachment, and only a gesture makes it one.
    #[test]
    fn an_image_on_the_clipboard_is_not_an_attachment_until_it_is_attached() {
        let mut state = panel();
        state.clipboard = offered();

        assert!(state.clipboard.is_image(), "the offer is there");
        assert!(
            !state.has_attachments(),
            "ambient clipboard content must never populate `attachments`"
        );

        state
            .attach(screenshot("Image from Chrome"))
            .expect("valid");
        assert!(state.has_attachments(), "the gesture is what attaches");
    }

    /// Every attachment is reversible, by chip and by key.
    #[test]
    fn an_attachment_is_removable_by_id_and_by_key() {
        let mut state = panel();
        let first = state.attach(screenshot("one")).expect("valid");
        let second = state.attach(screenshot("two")).expect("valid");
        assert_eq!(state.attachments().len(), 2);

        // The chip's own `×`.
        assert!(state.detach(first));
        assert!(!state.detach(first), "removing twice is not an error");
        assert_eq!(state.attachments().len(), 1);

        // `⌫` takes the most recent, which is now the only one.
        assert!(state.detach_last());
        assert!(!state.has_attachments());
        assert!(!state.detach_last(), "nothing left to take");
        let _ = second;
    }

    /// ⌘V pressed twice on one clipboard image attaches it once. §14: the
    /// second press would double the bill for identical pixels, and two chips
    /// sharing one id would make removal ambiguous.
    #[test]
    fn attaching_the_same_image_twice_attaches_it_once() {
        let mut state = panel();
        let shot = screenshot("Screenshot 14:32");

        let first = state.attach(shot.clone()).expect("valid");
        let second = state.attach(shot).expect("a repeat is not an error");
        assert_eq!(first, second);
        assert_eq!(state.attachments().len(), 1);

        // …and removal is unambiguous, which is what the idempotence buys.
        assert!(state.detach(first));
        assert!(!state.has_attachments());
    }

    /// §14: the per-request ceilings are enforced before dispatch, because §4
    /// does not fall back on a 400 — discovering a cap at the provider costs a
    /// round trip and then dead-ends.
    #[test]
    fn the_request_ceilings_are_enforced_at_attach_time() {
        let mut state = panel();
        for i in 0..MAX_ATTACHMENTS {
            state
                .attach(screenshot(&format!("shot {i}")))
                .expect("under the limit");
        }

        let error = state
            .attach(screenshot("one too many"))
            .expect_err("§14 caps the count per request");
        assert!(matches!(
            error,
            AiboError::AttachmentRejected {
                reason: AttachmentRejection::TooMany { .. },
                ..
            }
        ));
        assert_eq!(
            state.attachments().len(),
            MAX_ATTACHMENTS,
            "a refused attach must not half-apply"
        );
    }

    #[test]
    fn a_media_type_no_provider_accepts_is_refused_here_not_as_a_400() {
        let mut state = panel();
        let error = state
            .attach(Attachment::image(
                AttachmentSource::Clipboard,
                vec![0x47, 0x49, 0x46],
                "image/gif",
                8,
                8,
                "loop.gif",
            ))
            .expect_err("gif is not in the §10 matrix");
        assert!(matches!(
            error,
            AiboError::AttachmentRejected {
                reason: AttachmentRejection::UnsupportedMediaType,
                ..
            }
        ));
        assert!(!state.has_attachments());
    }

    fn vision_unsupported(binding: Option<ModelBinding>, alternatives: &[&str]) -> AiboError {
        let alternatives = alternatives.iter().map(|s| (*s).to_owned()).collect();
        match binding {
            Some(binding) => AiboError::vision_unsupported(binding, 1, alternatives),
            None => AiboError::no_vision_provider(1, alternatives),
        }
    }

    /// The regression, stated at the layer that showed it. Before the typed
    /// variant existed this landed on the generic arm — "Something went wrong
    /// inside aibo." plus "Copy diagnostics" — which discards the model id and
    /// the list of models that would work at the exact moment the user needs
    /// both.
    #[test]
    fn a_model_that_cannot_see_is_actionable_not_something_went_wrong() {
        i18n::set_language(i18n::Lang::En);
        let view = ErrorView::from_error(&vision_unsupported(
            Some(ModelBinding {
                provider: ProviderId::CODEX,
                model: "gpt-5.5".to_owned(),
            }),
            &["openai/gpt-5", "anthropic/claude-sonnet-4"],
        ));

        assert_eq!(view.treatment, Treatment::Inline);
        assert_ne!(view.headline, i18n::t(Key::ErrInternal));
        assert_ne!(view.action, Some(ErrorAction::CopyDiagnostics));

        // One sentence naming the model that cannot see and one that can.
        assert!(view.headline.contains("gpt-5.5"), "{}", view.headline);
        assert!(view.headline.contains("openai/gpt-5"), "{}", view.headline);

        // One action, carrying a binding rather than only describing it — and
        // on the alternative's *own* provider, which is usually not the one
        // that could not see.
        assert_eq!(
            view.action,
            Some(ErrorAction::UseModel {
                provider: ProviderId::OPENAI,
                model: "gpt-5".to_owned(),
            })
        );
    }

    /// A bare model id in `alternatives` means "same provider, different
    /// model". Offering it on the wrong provider would swap one refused
    /// binding for another.
    #[test]
    fn a_bare_alternative_stays_on_the_provider_that_could_not_see() {
        let view = ErrorView::from_error(&vision_unsupported(
            Some(ModelBinding {
                provider: ProviderId::ANTHROPIC,
                model: "claude-haiku-text".to_owned(),
            }),
            &["claude-sonnet-4"],
        ));
        assert_eq!(
            view.action,
            Some(ErrorAction::UseModel {
                provider: ProviderId::ANTHROPIC,
                model: "claude-sonnet-4".to_owned(),
            })
        );
    }

    /// The 2026-07-26 surface, exactly: no vision chain must NOT be reported as
    /// `NoProviderConfigured`. That variant is §13's only blocking treatment —
    /// it interrupts and opens settings — and the user here has a working text
    /// setup, a typed instruction and one attachment too many.
    #[test]
    fn no_vision_chain_is_inline_and_never_says_no_provider_is_configured() {
        i18n::set_language(i18n::Lang::En);
        let view = ErrorView::from_error(&vision_unsupported(None, &["openai"]));

        assert_eq!(view.treatment, Treatment::Inline);
        assert_ne!(view.treatment, Treatment::Blocking);
        assert_ne!(
            view.headline,
            i18n::t(Key::ErrNoProvider),
            "the contradiction this feature exists to retire"
        );
        assert!(view.headline.contains("openai"), "{}", view.headline);
        assert_eq!(view.action, Some(ErrorAction::OpenSettings));
    }

    /// The refusal reaches the panel inline, and the panel keeps the session:
    /// the instruction and the image both survive it (§13).
    #[test]
    fn a_vision_refusal_keeps_the_instruction_and_the_image() {
        let mut state = panel();
        state.input = "what is wrong with this chart".to_owned();
        state.attach(screenshot("chart")).expect("valid");

        state.fail(&Arc::new(vision_unsupported(
            Some(ModelBinding {
                provider: ProviderId::CEREBRAS,
                model: "gpt-oss-120b".to_owned(),
            }),
            &["openai/gpt-5"],
        )));

        assert_eq!(state.phase, Phase::Failed);
        assert!(state.toast.is_none(), "inline, not a toast");
        assert_eq!(state.input, "what is wrong with this chart");
        assert!(
            state.has_attachments(),
            "§13 never strips the attachment out from under the user"
        );
    }

    /// §13 promises one action that *resolves* the state. An error that
    /// outlives the image it complained about is a dead end.
    #[test]
    fn removing_the_image_retires_the_error_it_was_about() {
        let mut state = panel();
        state.attach(screenshot("chart")).expect("valid");
        state.fail(&Arc::new(vision_unsupported(
            Some(ModelBinding {
                provider: ProviderId::CEREBRAS,
                model: "gpt-oss-120b".to_owned(),
            }),
            &["openai/gpt-5"],
        )));
        assert_eq!(state.phase, Phase::Failed);

        assert!(state.detach_last());
        assert_eq!(state.phase, Phase::Idle, "the action settled the state");
        assert!(state.error.is_none());
    }

    /// A failure that has nothing to do with the images must survive them being
    /// removed — the recovery is scoped, not a blanket error clear.
    #[test]
    fn removing_an_image_does_not_clear_an_unrelated_failure() {
        let mut state = panel();
        state.attach(screenshot("chart")).expect("valid");
        state.fail(&Arc::new(AiboError::Timeout {
            phase: TimeoutPhase::FirstToken,
        }));
        assert_eq!(state.phase, Phase::Failed);

        assert!(state.detach_last());
        assert_eq!(state.phase, Phase::Failed);
        assert!(state.error.is_some());
    }

    #[test]
    fn a_rejected_attachment_offers_removal_of_the_chip_it_named() {
        i18n::set_language(i18n::Lang::En);
        let view = ErrorView::from_error(&AiboError::AttachmentRejected {
            label: "Screenshot 14:32".to_owned(),
            media_type: "image/png".to_owned(),
            reason: AttachmentRejection::TooLarge {
                bytes: 9_000_000,
                limit: 3_750_000,
            },
        });
        assert_eq!(view.treatment, Treatment::Inline);
        assert!(view.headline.contains("Screenshot 14:32"));
        assert_eq!(
            view.action,
            Some(ErrorAction::RemoveAttachment {
                label: "Screenshot 14:32".to_owned()
            })
        );
    }

    /// The whole-set rejections name no chip, so removal falls to the most
    /// recent — the one the user just added and the only one they can have
    /// meant.
    #[test]
    fn a_whole_set_rejection_removes_the_image_that_caused_it() {
        let mut state = panel();
        state.attach(screenshot("keep me")).expect("valid");
        state.attach(screenshot("the last straw")).expect("valid");
        assert!(state.detach_labelled(""));
        assert_eq!(state.attachments().len(), 1);
        assert_eq!(state.attachments()[0].attachment.label, "keep me");
    }

    /// §16: every action has a key, and it is shown. Attaching must be
    /// discoverable without already knowing the chord.
    #[test]
    fn the_attach_action_is_always_listed_with_its_key() {
        let mut state = panel();

        let listed = |state: &PanelState| {
            composer_actions_for(state)
                .into_iter()
                .find(|a| a.label == Key::ActionAttachImage)
        };

        let without = listed(&state).expect("§16: listed even with nothing to attach");
        assert_eq!(without.key, ATTACH_KEY);
        assert!(
            without.on_press.is_none(),
            "disabled rather than absent, so the row does not change length"
        );

        state.clipboard = offered();
        let with = listed(&state).expect("still listed");
        assert!(with.on_press.is_some(), "the offer enables it");

        // Removal appears with the thing it removes, and shows its own key.
        assert!(
            actions_for(&state)
                .iter()
                .all(|a| a.label != Key::ActionRemoveImage)
        );
        state.attach(screenshot("shot")).expect("valid");
        let remove = actions_for(&state)
            .into_iter()
            .find(|a| a.label == Key::ActionRemoveImage)
            .expect("removal is offered once there is something to remove");
        assert_eq!(remove.key, DETACH_KEY);
    }

    #[test]
    fn an_image_reference_without_pixels_keeps_attach_disabled() {
        let mut state = panel();
        state.clipboard = ClipboardOffer::Image {
            label: "Clipboard image".to_owned(),
            image: None,
        };
        assert!(state.clipboard.is_image());
        assert!(!state.clipboard.is_attachable());
        let attach = composer_actions_for(&state)
            .into_iter()
            .find(|action| action.label == Key::ActionAttachImage)
            .expect("the stable composer keeps the disabled entry");
        assert!(attach.on_press.is_none());
    }

    #[test]
    fn recovery_actions_without_backend_wiring_are_not_rendered() {
        assert!(error_action(ErrorAction::TrimSelection).is_none());
        assert!(
            error_action(ErrorAction::UseModel {
                provider: ProviderId::OPENAI,
                model: "gpt-5".to_owned(),
            })
            .is_none()
        );
        assert!(error_action(ErrorAction::ContinueAnyway).is_none());
        assert!(error_action(ErrorAction::Retry).is_some());
    }

    /// §14: the user pays for every image, so the estimate is on screen before
    /// the request rather than reconciled after it — `Usage` never arrives on a
    /// cancelled stream.
    #[test]
    fn attached_images_declare_their_cost_before_the_request() {
        let mut state = panel();
        assert_eq!(state.estimated_image_tokens(), 0);
        let before = state.desired_height();

        state.attach(screenshot("chart")).expect("valid");
        // 1200 × 750 / 750.
        assert_eq!(state.estimated_image_tokens(), 1200);

        // The footnote rides with the chips rather than with the footer, so it
        // is reserved in the collapsed phase too — where the footer's own rows
        // are not counted at all. Reserving only the chip row would clip it
        // against the window edge, and nothing reports that.
        assert!(
            state.desired_height() >= before + ATTACHMENT_ROW_HEIGHT + theme::META_LINE_HEIGHT,
            "{} vs {before}",
            state.desired_height()
        );
    }

    /// Attaching into a fixed-height panel would push the input out from under
    /// the caret. The chips have to be reserved in every phase.
    #[test]
    fn the_panel_grows_to_fit_its_attachment_chips() {
        let mut state = panel();
        let collapsed = state.desired_height();

        state.attach(screenshot("one")).expect("valid");
        let one = state.desired_height();
        assert!(one > collapsed, "{one} vs {collapsed}");

        // Four chips do not fit on one line at `PANEL_WIDTH_MIN`, so the
        // reservation has to grow again rather than clip the wrapped row.
        for i in 1..MAX_ATTACHMENTS {
            state
                .attach(screenshot(&format!("shot {i}")))
                .expect("valid");
        }
        assert!(state.desired_height() > one);
        assert!(state.desired_height() <= theme::PANEL_HEIGHT_MAX);
    }

    /// §13: one panel, one session, and a new invocation keeps nothing. An
    /// image carried into the next session is an image sent to a model the user
    /// never chose to show it to (§5).
    #[test]
    fn a_new_session_discards_the_attachments_too() {
        let mut state = panel();
        state.clipboard = offered();
        state.attach(screenshot("private")).expect("valid");

        state.reset(SessionId::from_u128(2));
        assert!(!state.has_attachments());
        assert!(!state.clipboard.is_image(), "the offer is re-captured too");
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

    /// §16's "streaming must not reflow", stated as the number that matters:
    /// how many times does the window geometry change while one answer streams?
    ///
    /// Measured at **6**, for chunk sizes from three characters to a full
    /// sentence — `estimated_text_height` resolves to whole lines, so growth is
    /// per line and not per token. The bound is a guard rather than a
    /// regression test: it fails if someone makes the panel track the response
    /// continuously, which is the shape of the bug it is here to prevent.
    ///
    /// It does **not** cover the defect that was actually fixed alongside it.
    /// Each of these six changes used to re-run the panel's full arrival
    /// sequence, including `gain_focus` and `operation::focus(INPUT_ID)`, so the
    /// caret was pulled back into the input six times per answer. That fix is
    /// in `app::resize_panel_if_visible`, and it is structural — the two
    /// operations no longer share a code path — but it lives in a `Task` that
    /// cannot be introspected from a unit test, so nothing here locks it.
    #[test]
    fn streaming_an_answer_changes_the_panel_height_a_handful_of_times() {
        for (label, chunk, chunks) in [
            ("tiny", "ab ", 400),
            ("word", "hello ", 200),
            (
                "sentence",
                "the quick brown fox jumps over the lazy dog. ",
                100,
            ),
        ] {
            let mut state = panel();
            state.begin_turn("Rewrite this as a changelog entry".to_owned());
            state.phase = Phase::Streaming;

            let mut heights = vec![state.desired_height()];
            for _ in 0..chunks {
                state.append_response(chunk);
                let height = state.desired_height();
                if (height - heights[heights.len() - 1]).abs() >= 1.0 {
                    heights.push(height);
                }
            }

            let resizes = heights.len() - 1;
            // Expressed as a ratio rather than a fixed count, because the
            // absolute number legitimately depends on how much room the display
            // allows the answer — a taller screen means more lines before the
            // area saturates, and pinning a constant here would fail the moment
            // §4's 60 %-of-display rule gave the answer the room it asked for.
            // The invariant that actually matters is unchanged: the panel grows
            // per *line*, never per token.
            assert!(
                resizes * 10 <= chunks,
                "{label}: {resizes} height changes across {chunks} chunks — \
                 the panel is tracking the response instead of its lines"
            );

            // Monotonic, because a panel that shrinks mid-answer is worse than
            // one that grows: the text being read moves out from under the eye.
            for pair in heights.windows(2) {
                assert!(
                    pair[1] >= pair[0],
                    "{label}: the panel shrank mid-answer, {} then {}",
                    pair[0],
                    pair[1]
                );
            }
        }
    }

    /// `design.md` §3: the rail is amber "on the row that currently has the
    /// user's attention — the input while typing, the answer while streaming"
    /// and danger on a permission or error row.
    ///
    /// Exactly one row may be amber at a time. Two amber segments would say the
    /// panel is in two places at once, which is precisely the claim the rail
    /// exists to make unambiguous.
    #[test]
    fn the_rail_marks_one_row_at_a_time() {
        let mut state = panel();

        // Idle: attention is the input.
        assert_eq!(input_rail_state(&state), RailState::Active);
        assert_eq!(content_rail_state(&state), RailState::Inactive);

        // Streaming: attention moves to the answer, and leaves the input.
        state.begin_turn("q".to_owned());
        state.phase = Phase::Streaming;
        assert_eq!(input_rail_state(&state), RailState::Inactive);
        assert_eq!(content_rail_state(&state), RailState::Active);

        // Finished: attention returns to the input, so the next thing typed is
        // obviously going to the same place.
        state.phase = Phase::Finished {
            reason: StopReason::EndTurn,
        };
        assert_eq!(input_rail_state(&state), RailState::Active);
        assert_eq!(content_rail_state(&state), RailState::Inactive);
    }

    /// An error takes the rail regardless of phase, and it takes it as `Alert`
    /// rather than `Active` — §13's treatment reaching the eye through a second
    /// channel, so severity never rests on reading one coloured word.
    #[test]
    fn an_error_row_turns_the_rail_danger() {
        let mut state = panel();
        state.phase = Phase::Streaming;
        assert_eq!(content_rail_state(&state), RailState::Active);

        state.phase = Phase::Failed;
        assert_eq!(content_rail_state(&state), RailState::Alert);
    }

    #[test]
    fn a_denied_permission_takes_the_rail_even_while_idle() {
        let mut state = panel();
        state.context = ContextState::PermissionDenied {
            status: PermissionStatus::Denied,
        };
        assert_eq!(content_rail_state(&state), RailState::Alert);
    }

    #[test]
    fn a_long_answer_cannot_push_the_footer_out_of_the_panel() {
        let mut state = panel();
        state.phase = Phase::Streaming;
        state.attribution.provider = Some(ProviderId::OPENAI);
        state.attribution.model = Some("gpt-5".to_owned());

        assert!(state.reserve_for(10_000.0));
        assert_eq!(state.answer_height(), state.max_answer_height());
        assert!(
            theme::PANEL_HEIGHT_COLLAPSED + state.answer_height() + state.footer_height()
                <= theme::PANEL_HEIGHT_MAX
        );
        assert_eq!(state.desired_height(), theme::PANEL_HEIGHT_MAX);
    }

    #[test]
    fn a_chat_window_grows_with_wrapped_lines_then_stops_at_its_maximum() {
        let mut state = panel();
        state.active_user = Some("Explain this".to_owned());
        state.phase = Phase::Streaming;
        state.set_response("Short answer");
        let short = state.desired_height();

        state.set_response("A long wrapped response. ".repeat(80));
        let long = state.desired_height();

        assert!(long > short, "{long} must grow beyond {short}");
        assert!(long <= state.max_panel_height());
        assert_eq!(state.transcript_height(), state.max_transcript_height());
    }

    /// `design.md` §4 sizes the answer area at 60 % of the display, not to a
    /// constant. The old fixed 268 pt transcript cap and 172 pt answer cap were
    /// the binding constraints on a long answer: the window stopped growing
    /// while most of the screen was still empty, and the response scrolled
    /// inside a small box — "the response is not growing the window".
    #[test]
    fn a_taller_display_gives_the_answer_more_room() {
        let mut small = panel();
        small.display_height = Some(800.0);
        let mut large = panel();
        large.display_height = Some(1440.0);

        assert!(
            large.max_panel_height() > small.max_panel_height(),
            "a 1440 pt display must allow a taller panel than an 800 pt one: {} vs {}",
            large.max_panel_height(),
            small.max_panel_height()
        );
        assert!(
            large.max_chat_answer_height() > small.max_chat_answer_height(),
            "the answer bubble must scale with the panel, not stay at 172 pt"
        );

        // 60 % of 1440 is 864, and the panel stays a panel rather than becoming
        // a window that covers the screen.
        assert!((large.max_panel_height() - 864.0).abs() < 0.01);
        assert!(large.max_panel_height() <= 900.0);
    }

    /// An unknown display must never be *worse* than the constants it replaced,
    /// because the window server has not answered when the panel first paints
    /// and a collapsed panel at that moment is a visible glitch.
    #[test]
    fn an_unknown_display_is_never_smaller_than_the_old_constants() {
        let state = panel();
        assert_eq!(state.display_height, None);
        assert_eq!(state.max_panel_height(), theme::PANEL_HEIGHT_MAX);
        assert!(
            state.max_chat_answer_height() >= CHAT_ANSWER_MAX_HEIGHT,
            "the fallback must not shrink the answer below the old 172 pt cap"
        );
    }

    /// The display is hardware, not session state.
    #[test]
    fn a_new_session_keeps_the_display_height() {
        let mut state = panel();
        state.display_height = Some(1440.0);
        state.reset(SessionId::from_u128(9));
        assert_eq!(state.display_height, Some(1440.0));
    }

    #[test]
    fn selectable_answers_reject_edit_actions() {
        let mut state = panel();
        state.set_response("server-owned answer");
        state.perform_response_action(text_editor::Action::SelectAll);
        assert_eq!(
            state.response_editor.selection().as_deref(),
            Some("server-owned answer")
        );
        state.perform_response_action(text_editor::Action::Edit(text_editor::Edit::Insert('x')));
        assert_eq!(state.response_editor.text(), "server-owned answer");
        assert_eq!(state.response, "server-owned answer");
    }

    #[test]
    fn streamed_chunks_append_to_the_selectable_answer_without_rebuilding_it() {
        let mut state = panel();
        for chunk in ["Hello", " ", "世界", " 👨‍👩‍👧‍👦"] {
            state.append_response(chunk);
        }

        assert_eq!(state.response, "Hello 世界 👨‍👩‍👧‍👦");
        assert_eq!(state.response_editor.text(), state.response);
    }

    #[test]
    fn streamed_chunks_build_markdown_incrementally() {
        let mut state = panel();
        for chunk in [
            "# Heading\n\n",
            "- **one**\n- two\n\n",
            "```rust\nfn main() {}\n",
            "```\n",
        ] {
            state.append_response(chunk);
        }

        let items = state.response_markdown.items();
        assert!(
            items
                .iter()
                .any(|item| matches!(item, markdown::Item::Heading(..)))
        );
        assert!(
            items
                .iter()
                .any(|item| matches!(item, markdown::Item::List { .. }))
        );
        assert!(
            items
                .iter()
                .any(|item| matches!(item, markdown::Item::CodeBlock { .. }))
        );
    }

    #[test]
    fn a_follow_up_keeps_complete_turns_and_starts_a_fresh_active_bubble() {
        let mut state = panel();
        state.begin_turn("first question".to_owned());
        state.append_response("first answer");
        state.phase = Phase::Finished {
            reason: StopReason::EndTurn,
        };

        assert_eq!(
            state.history_for_next_turn(),
            vec![Turn::pair("first question", "first answer")]
        );

        state.begin_turn("follow up".to_owned());
        assert_eq!(state.turns.len(), 1);
        assert_eq!(state.turns[0].user, "first question");
        assert_eq!(state.turns[0].assistant, "first answer");
        assert!(!state.turns[0].assistant_markdown.is_empty());
        assert_eq!(state.active_user.as_deref(), Some("follow up"));
        assert!(state.response.is_empty());
        assert!(state.input.is_empty());
        assert_eq!(state.phase, Phase::Loading);
    }

    #[test]
    fn removing_the_context_card_excludes_selection_from_future_turns() {
        let mut state = panel();
        state.context = ContextState::Available {
            app: None,
            excerpt: Some("selected".to_owned()),
            selection: Some("selected text".to_owned()),
            truncated: true,
            caret_bounds: None,
        };

        assert!(state.includes_selection());
        state.remove_selection();
        assert!(!state.includes_selection());
        assert!(matches!(
            state.context,
            ContextState::Available {
                excerpt: None,
                selection: None,
                truncated: false,
                ..
            }
        ));
    }
}
