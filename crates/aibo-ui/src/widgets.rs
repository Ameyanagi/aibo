//! The component inventory from §16 — "mostly hand-built, each a real unit of work".
//!
//! §16 lists them: context chip, action list with key hints, streaming markdown
//! viewer, diff viewer, approval prompt, provider picker, spend meter,
//! permission-state banner, settings forms, toast, agent step list, capture
//! inspector. Everything here is a **composition function**, not an iced
//! `Widget` impl: none of these needs custom layout or its own event handling,
//! and a function that returns an `Element` is far cheaper to keep consistent
//! with [`crate::theme`].
//!
//! Two rules hold across all of them, both from §16:
//!
//! * **Every action has a key, and the key is shown.** The mouse is optional —
//!   that is the real differentiator for a hotkey tool — so no component
//!   renders a clickable affordance without its shortcut beside it.
//! * **Streaming must not reflow.** [`answer`] reserves its height on the first
//!   chunk instead of growing per token.
//!
//! No user-visible string literal appears here; everything routes through
//! [`crate::i18n`].

use aibo_core::types::{Attachment, AttachmentKind};
use iced::widget::{
    Space, button, column, container, image, row, rule, scrollable, text, text_editor,
};
use iced::{Alignment, Background, ContentFit, Element, Length};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::i18n::{self, Key};
use crate::theme::{self, Severity, space, type_scale};

/// Render a shortcut using the platform's primary-modifier convention.
pub const fn primary_shortcut(macos: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        macos
    } else {
        other
    }
}

/// One action offered on a surface, with the key that triggers it (§16).
#[derive(Debug, Clone)]
pub struct Action<Message> {
    /// Catalogue key for the label.
    pub label: Key,
    /// The shortcut, already rendered for display: `⏎`, `⌘C`, `esc`.
    pub key: &'static str,
    /// Emitted on activation. `None` renders the action disabled — used while
    /// a stream is still running and "Replace" would insert a partial result,
    /// which §13 forbids.
    pub on_press: Option<Message>,
    /// Draw with the emphasis style. At most one per surface.
    pub primary: bool,
    /// Draw with the destructive style.
    pub destructive: bool,
}

impl<Message> Action<Message> {
    /// A quiet action.
    pub fn new(label: Key, key: &'static str, on_press: Message) -> Self {
        Self {
            label,
            key,
            on_press: Some(on_press),
            primary: false,
            destructive: false,
        }
    }

    /// Mark this as the emphasised action.
    pub fn primary(mut self) -> Self {
        self.primary = true;
        self
    }

    /// Mark this as destructive.
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    /// Disable it, keeping it visible so the layout does not jump.
    pub fn disabled(mut self) -> Self {
        self.on_press = None;
        self
    }
}

/// The context chip: source app plus a one-line excerpt (§16).
///
/// Renders the "no context" state rather than disappearing. §8 captures context
/// asynchronously with a deadline, so the panel is often shown *before* the
/// chip has anything to say — a chip that pops into existence mid-interaction
/// would move the input under the user's cursor.
pub fn context_chip<'a, Message: 'a>(
    source_app: Option<&str>,
    excerpt: Option<&str>,
) -> Element<'a, Message> {
    let label = match source_app {
        Some(app) => i18n::t1(Key::ContextChipFrom, app),
        None => i18n::t(Key::ContextChipNone).to_owned(),
    };

    let mut line = row![
        // The accent leading rule from the §16 mock.
        container(Space::new().width(2.0).height(20.0)).style(|t: &iced::Theme| {
            container::Style {
                background: Some(Background::Color(theme::palette_of(t).accent)),
                ..Default::default()
            }
        }),
        text(label)
            .size(type_scale::CHIP)
            .style(theme::text_primary),
    ]
    .spacing(space(2.0))
    .align_y(Alignment::Center);

    if let Some(excerpt) = excerpt {
        line = line.push(
            text(elide(excerpt, 96))
                .size(type_scale::CHIP)
                .style(theme::text_dim),
        );
    }

    container(line)
        .padding([space(1.5), space(2.5)])
        .style(theme::chip)
        .into()
}

/// Edge of the square thumbnail on an attachment chip, in points.
///
/// Small enough to remain a thumbnail inside the chip's 44 pt remove target.
const THUMBNAIL_EDGE: f32 = 20.0;

/// One attached image, as a chip beside the context chip (§16, §2).
///
/// Everything the user needs in order to explain a surprising answer or a
/// surprising bill is on this chip: *what* is attached (the thumbnail, so it is
/// recognisable at a glance rather than by a label they have to trust), how big
/// it is in pixels and in bytes, whether aibo resampled it, and how to take it
/// off again. An attachment the user cannot see is an attachment they cannot
/// reason about.
///
/// `thumbnail` is an already-built handle rather than raw bytes on purpose:
/// [`iced::widget::image::Handle::from_bytes`] mints a **fresh id on every
/// call**, so building one here would defeat the renderer's cache and re-upload
/// megabytes of pixels on every frame the panel draws (§15). It is built once,
/// at attach time.
///
/// `key` is the shortcut that removes *this* chip, or `None` when the chip is
/// only removable by click. §16 wants every action to show its key; with more
/// than one image attached only the most recent has an unambiguous one.
pub fn attachment_chip<'a, Message: Clone + 'a>(
    thumbnail: &image::Handle,
    attachment: &Attachment,
    key: Option<&'static str>,
    on_remove: Message,
) -> Element<'a, Message> {
    let mut line = row![
        // `ContentFit::Cover` crops rather than letterboxes: a chip is 20 pt of
        // recognition, and a letterboxed wide screenshot spends most of that on
        // empty bars.
        image(thumbnail.clone())
            .width(Length::Fixed(THUMBNAIL_EDGE))
            .height(Length::Fixed(THUMBNAIL_EDGE))
            .content_fit(ContentFit::Cover),
        // The label is *display text the attachment carried in* — from the
        // clipboard's source app or a file name — so it is elided rather than
        // trusted to be short, and it is never rendered as anything but text.
        text(elide(&attachment.label, 24))
            .size(type_scale::CHIP)
            .style(theme::text_primary),
        text(format!(
            "{}×{} · {}",
            attachment.width,
            attachment.height,
            format_bytes(attachment.byte_len())
        ))
        .size(type_scale::CHIP)
        .font(theme::MONO_FONT)
        .style(theme::text_dim),
    ]
    .spacing(space(1.5))
    .align_y(Alignment::Center);

    // §14: the bytes on the wire are a re-encoding, and the original resolution
    // is not recoverable from them. Saying so here is the answer to "why is my
    // screenshot blurry".
    if matches!(attachment.kind, AttachmentKind::Image { downscaled: true }) {
        line = line.push(
            text(i18n::t(Key::AttachmentDownscaled))
                .size(type_scale::CHIP)
                .style(theme::text_faint),
        );
    }

    let mut remove = row![].spacing(space(1.0)).align_y(Alignment::Center);
    if let Some(key) = key {
        remove = remove.push(
            text(key)
                .size(type_scale::META)
                .font(theme::MONO_FONT)
                .style(theme::text_accent),
        );
    }
    remove = remove.push(
        text(format!("× {}", i18n::t(Key::ActionRemoveImage)))
            .size(type_scale::CHIP)
            .style(theme::text_dim),
    );

    line = line.push(
        button(remove)
            .height(Length::Fixed(theme::MIN_HIT_TARGET))
            .padding([0.0, space(1.5)])
            .style(theme::action_button)
            .on_press(on_remove),
    );

    container(line)
        .padding([space(1.0), space(2.0)])
        .style(theme::chip)
        .into()
}

/// A byte count in the largest unit that keeps it under four digits.
///
/// Display-only, and deliberately decimal rather than binary: providers meter
/// their per-image ceilings in decimal megabytes, so a chip that reads `3.6 MB`
/// against a documented `5 MB` limit has to mean the same MB the limit does.
pub fn format_bytes(n: usize) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "display rounding; the value is capped at a few megabytes"
    )]
    let bytes = n as f64;
    if n < 1_000 {
        format!("{n} B")
    } else if n < 1_000_000 {
        format!("{:.0} KB", bytes / 1_000.0)
    } else {
        format!("{:.1} MB", bytes / 1_000_000.0)
    }
}

/// The metadata line: model, latency, cost (§16, §14).
///
/// Cost is shown from the first token because BYOK means the user pays for
/// every mistake aibo makes (§14) — hiding it until the request completes is
/// the wrong default.
pub fn meta_line<'a, Message: 'a>(
    provider: &str,
    model: &str,
    latency_ms: Option<u64>,
    cost_label: Option<&str>,
) -> Element<'a, Message> {
    let mut parts = vec![provider.to_owned(), model.to_owned()];
    if let Some(ms) = latency_ms {
        parts.push(format!("{ms}ms"));
    }
    if let Some(cost) = cost_label {
        parts.push(cost.to_owned());
    }
    text(parts.join(" · "))
        .size(type_scale::META)
        .style(theme::text_dim)
        .into()
}

/// The action list with key hints (§16).
pub fn action_list<'a, Message: Clone + 'a>(actions: Vec<Action<Message>>) -> Element<'a, Message> {
    let mut line = row![].spacing(space(2.0)).align_y(Alignment::Center);
    for action in actions {
        let enabled = action.on_press.is_some();
        let key_style = if !enabled {
            theme::text_faint
        } else if action.primary {
            theme::text_on_primary
        } else if action.destructive {
            theme::text_danger
        } else {
            theme::text_accent
        };
        let label = row![
            text(action.key).size(type_scale::META).style(key_style),
            text(i18n::t(action.label)).size(type_scale::META),
        ]
        .spacing(space(1.5))
        .align_y(Alignment::Center);

        // The overlay's footer is a compact keyboard legend, not a toolbar.
        // The audit pass forced these to 44 pt, which becomes an 88 px wall on
        // Retina displays and visually outweighs the answer. Restore the
        // original compact vertical rhythm while keeping generous horizontal
        // click area.
        let label = container(label)
            .height(Length::Fill)
            .align_y(iced::alignment::Vertical::Center);
        let mut widget = button(label)
            .height(Length::Fixed(theme::CONTROL_HEIGHT))
            .padding([space(1.0), space(2.0)]);
        widget = if action.destructive {
            widget.style(theme::danger_button)
        } else if action.primary {
            widget.style(theme::primary_button)
        } else {
            widget.style(theme::action_button)
        };
        if let Some(message) = action.on_press {
            widget = widget.on_press(message);
        }
        line = line.push(widget);
    }
    line.into()
}

/// A titled state block — the shape every non-happy-path state takes (§16).
///
/// Empty, context-unavailable, permission-denied and the inline error treatment
/// all render through this, which is what stops them drifting apart visually.
pub fn state_block<'a, Message: Clone + 'a>(
    severity: Severity,
    title: &str,
    body: Option<&str>,
    actions: Vec<Action<Message>>,
) -> Element<'a, Message> {
    let mut stack = column![
        text(title.to_owned())
            .size(type_scale::BODY)
            .style(theme::text_severity(severity)),
    ]
    .spacing(space(1.5));

    if let Some(body) = body {
        stack = stack.push(
            text(body.to_owned())
                .size(type_scale::META)
                .style(theme::text_dim),
        );
    }
    if !actions.is_empty() {
        stack = stack.push(action_list(actions));
    }

    container(stack)
        .width(Length::Fill)
        .padding(space(3.0))
        .style(theme::banner(severity))
        .into()
}

/// The answer area: streaming text that does not reflow (§16).
///
/// `reserved_height` is fixed on the first chunk and only ever grown in
/// discrete steps by the caller. Growing it per token is the reflow §16
/// forbids, and it is also the single easiest way to blow the frame budget in
/// §15.
///
/// The `markdown` widget iced ships is feature-gated and not enabled for this
/// crate, so this renders plain text for now.
/// TODO: enable `iced/markdown` and swap the body for the incremental markdown
/// viewer — §16 calls that "a customisation job rather than a from-scratch one",
/// but selection, link handling and code-block actions remain product work.
pub fn answer<'a, Message: 'a>(
    body: &str,
    reserved_height: f32,
    truncated: bool,
) -> Element<'a, Message> {
    let mut stack = column![
        scrollable(
            container(
                text(body.to_owned())
                    .size(type_scale::BODY)
                    .font(theme::UI_FONT)
            )
            .width(Length::Fill)
            .padding(space(1.0))
        )
        .style(theme::scroller)
        .height(Length::Fixed(
            reserved_height.max(theme::ANSWER_BOX_MIN_HEIGHT)
        )),
    ]
    .spacing(space(1.5));

    if truncated {
        // §13: a partial stream is never auto-inserted; it stays in the panel
        // marked truncated with retry and copy actions.
        stack = stack.push(
            text(i18n::t(Key::StateTruncated))
                .size(type_scale::META)
                .style(theme::text_severity(Severity::Warning)),
        );
    }

    stack.into()
}

/// A selectable, read-only answer area.
///
/// The caller receives selection/cursor actions so the editor remains fully
/// keyboard- and mouse-selectable, but must reject editing actions.
pub fn selectable_answer<'a, Message: Clone + 'a>(
    content: &'a text_editor::Content,
    reserved_height: f32,
    truncated: bool,
    on_action: impl Fn(text_editor::Action) -> Message + 'a,
) -> Element<'a, Message> {
    let mut stack = column![
        text_editor(content)
            .on_action(on_action)
            .height(Length::Fixed(
                reserved_height.max(theme::ANSWER_BOX_MIN_HEIGHT),
            ))
            .padding(space(2.0))
            .size(type_scale::BODY)
            .font(theme::UI_FONT)
            .style(theme::answer_editor),
    ]
    .spacing(space(1.5));

    if truncated {
        stack = stack.push(
            text(i18n::t(Key::StateTruncated))
                .size(type_scale::META)
                .style(theme::text_severity(Severity::Warning)),
        );
    }

    stack.into()
}

/// Selectable assistant text rendered inside a chat bubble.
///
/// The bubble already owns its background and border, so this editor keeps
/// only selection behavior and typography.
pub fn selectable_chat_answer<'a, Message: Clone + 'a>(
    content: &'a text_editor::Content,
    reserved_height: f32,
    truncated: bool,
    on_action: impl Fn(text_editor::Action) -> Message + 'a,
) -> Element<'a, Message> {
    let mut stack = column![
        text_editor(content)
            .on_action(on_action)
            .height(Length::Fixed(
                reserved_height.max(theme::CHAT_ANSWER_MIN_HEIGHT)
            ))
            .padding(0.0)
            .size(type_scale::BODY)
            .font(theme::UI_FONT)
            .style(theme::chat_answer_editor),
    ]
    .spacing(space(1.0));

    if truncated {
        stack = stack.push(
            text(i18n::t(Key::StateTruncated))
                .size(type_scale::META)
                .style(theme::text_severity(Severity::Warning)),
        );
    }

    stack.into()
}

/// A subtle footnote, e.g. the §13 silent-fallback substitute notice.
pub fn footnote<'a, Message: 'a>(body: String) -> Element<'a, Message> {
    text(body)
        .size(type_scale::META)
        .style(theme::text_faint)
        .into()
}

/// A non-blocking toast (§13: `InsertFailed`, `CaptureFailed`).
///
/// The result stays in the panel behind it so the user can copy manually — the
/// toast never replaces the content it is complaining about.
pub fn toast<'a, Message: Clone + 'a>(
    severity: Severity,
    body: &str,
    action: Option<Action<Message>>,
) -> Element<'a, Message> {
    let mut line = row![
        // The severity reads off a leading bar rather than the body colour: a
        // toast is deliberately quiet chrome (§13, non-blocking) and tinting a
        // whole sentence red would make `InsertFailed` louder than the answer
        // it is sitting beside.
        container(Space::new().width(2.0).height(Length::Fill)).style(move |t: &iced::Theme| {
            container::Style {
                background: Some(Background::Color(severity.color(&theme::palette_of(t)))),
                ..Default::default()
            }
        }),
        text(body.to_owned())
            .size(type_scale::META)
            .style(theme::text_primary),
    ]
    .spacing(space(2.0))
    .align_y(Alignment::Center);

    if let Some(action) = action {
        line = line.push(action_list(vec![action]));
    }

    container(line)
        .padding([space(2.0), space(3.0)])
        .style(theme::toast)
        .into()
}

/// The spend meter (§14): spent against a cap, with the cap's own colour.
///
/// Amber past 80 %, red at the ceiling — the same semantic pair used for
/// permission prompts, because both are "you are about to be charged for
/// something" moments.
pub fn spend_meter<'a, Message: 'a>(
    spent_label: &str,
    fraction_of_cap: Option<f32>,
) -> Element<'a, Message> {
    let severity = match fraction_of_cap {
        Some(f) if f >= 1.0 => Severity::Danger,
        Some(f) if f >= 0.8 => Severity::Warning,
        _ => Severity::Info,
    };
    let fill = fraction_of_cap.unwrap_or(0.0).clamp(0.0, 1.0);

    column![
        text(i18n::t1(Key::SpendThisMonth, spent_label))
            .size(type_scale::META)
            .style(theme::text_severity(severity)),
        container(
            container(Space::new().height(3.0))
                .width(Length::FillPortion(((fill * 100.0) as u16).max(1)))
                .style(move |t: &iced::Theme| container::Style {
                    background: Some(Background::Color(severity.color(&theme::palette_of(t)))),
                    ..Default::default()
                })
        )
        .width(Length::Fill)
        .style(theme::raised),
    ]
    .spacing(space(1.0))
    .into()
}

/// The permission-state banner (§8, §17).
///
/// `Revoked` is deliberately distinct from `Denied`: §17 gives a TCC grant that
/// disappeared after an update its own recovery screen, because the user did
/// nothing wrong and the copy has to say so.
pub fn permission_banner<'a, Message: Clone + 'a>(
    status: aibo_core::types::PermissionStatus,
    explanation: &str,
    on_open_settings: Option<Message>,
) -> Element<'a, Message> {
    use aibo_core::types::PermissionStatus as S;

    let (severity, headline) = match status {
        S::Granted => (Severity::Success, Key::PermissionGranted),
        S::Denied | S::Restricted => (Severity::Danger, Key::PermissionDenied),
        S::NotDetermined => (Severity::Info, Key::PermissionNotDetermined),
        S::Revoked => (Severity::Warning, Key::PermissionRevoked),
        S::NotApplicable => (Severity::Info, Key::PermissionGranted),
    };

    let actions = match on_open_settings {
        Some(message) if status != S::Granted => {
            vec![Action::new(Key::ActionOpenSystemSettings, "⏎", message)]
        }
        _ => Vec::new(),
    };

    state_block(severity, i18n::t(headline), Some(explanation), actions)
}

/// A unified-diff viewer (§16, and §11's "approval happens before the write").
///
/// Line-level colouring only: this is a review affordance in a 680 pt panel,
/// not an editor. Word-level intra-line diffing is deferred.
pub fn diff_view<'a, Message: 'a>(path: &str, unified_diff: &str) -> Element<'a, Message> {
    let mut lines = column![].spacing(0);
    for line in unified_diff.lines() {
        let severity = match line.as_bytes().first() {
            Some(b'+') if !line.starts_with("+++") => Severity::Success,
            Some(b'-') if !line.starts_with("---") => Severity::Danger,
            _ => Severity::Info,
        };
        lines = lines.push(
            text(line.to_owned())
                .size(type_scale::META)
                .font(theme::MONO_FONT)
                .style(theme::text_severity(severity)),
        );
    }

    column![
        row![
            text(i18n::t(Key::TaskFileChanged))
                .size(type_scale::META)
                .style(theme::text_dim),
            text(path.to_owned())
                .size(type_scale::META)
                .font(theme::MONO_FONT)
                .style(theme::text_primary),
        ]
        .spacing(space(2.0)),
        container(scrollable(lines).style(theme::scroller))
            .width(Length::Fill)
            .padding(space(2.0))
            .style(theme::raised),
    ]
    .spacing(space(1.5))
    .into()
}

/// A section heading for settings and the task window.
pub fn section<'a, Message: 'a>(title: Key) -> Element<'a, Message> {
    column![
        text(i18n::t(title))
            .size(type_scale::HEADING)
            .style(theme::text_primary),
        rule::horizontal(1).style(theme::separator),
    ]
    .spacing(space(1.5))
    .into()
}

/// Shorten `s` to `max` grapheme clusters with an ellipsis.
///
/// A Unicode scalar boundary is not enough: emoji families use zero-width
/// joiners, flags use paired regional indicators and accented letters may use
/// combining marks. Every visible elision in the shell lands on the same
/// user-perceived-character boundaries as §5's context truncation.
pub fn elide(s: &str, max: usize) -> String {
    let flattened: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = flattened.trim();
    if max == 0 {
        return String::new();
    }
    let graphemes: Vec<&str> = trimmed.graphemes(true).collect();
    if graphemes.len() <= max {
        return trimmed.to_owned();
    }
    let mut out = graphemes[..max - 1].concat();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elide_is_grapheme_safe_and_flattens_newlines() {
        assert_eq!(elide("hello", 10), "hello");
        assert_eq!(elide("a\nb", 10), "a b");
        let ja = "これは長い日本語のテキストです";
        let out = elide(ja, 5);
        assert_eq!(out.graphemes(true).count(), 5);
        assert!(out.ends_with('…'));

        assert_eq!(elide("👨‍👩‍👧‍👦abcdef", 3), "👨‍👩‍👧‍👦a…");
        assert_eq!(elide("e\u{301}abcdef", 3), "e\u{301}a…");
        assert_eq!(elide("🇯🇵abcdef", 2), "🇯🇵…");
        assert_eq!(elide("anything", 0), "");
    }

    #[test]
    fn byte_counts_read_in_the_unit_providers_meter() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_000), "1 KB");
        assert_eq!(format_bytes(412_000), "412 KB");
        // The §10 per-image ceiling is documented as 5 decimal MB, so the chip
        // has to speak in the same MB or the comparison the user makes is wrong.
        assert_eq!(format_bytes(3_750_000), "3.8 MB");
    }

    #[test]
    fn actions_can_be_disabled_without_disappearing() {
        let action: Action<()> = Action::new(Key::ActionReplace, "⏎", ()).disabled();
        assert!(action.on_press.is_none());
    }
}
