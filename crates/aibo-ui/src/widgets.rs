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

use iced::widget::{Space, button, column, container, row, rule, scrollable, text};
use iced::{Alignment, Background, Element, Length};

use crate::i18n::{self, Key};
use crate::theme::{self, Severity, space, type_scale};

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
        container(Space::new().width(2.0).height(Length::Fill)).style(|t: &iced::Theme| {
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
        let label = row![
            text(action.key).size(type_scale::META).style(if enabled {
                theme::text_accent
            } else {
                theme::text_faint
            }),
            text(i18n::t(action.label)).size(type_scale::META),
        ]
        .spacing(space(1.5))
        .align_y(Alignment::Center);

        let mut widget = button(label).padding([space(1.0), space(2.0)]);
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

/// Shorten `s` to `max` characters with an ellipsis, on a character boundary.
///
/// Character-based rather than byte-based so a Japanese excerpt is not cut mid
/// code point. Grapheme-cluster-correct truncation lives in `aibo-core` (§5);
/// this is display-only chrome and does not need it.
pub fn elide(s: &str, max: usize) -> String {
    let flattened: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = flattened.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_owned();
    }
    let mut out: String = trimmed.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elide_is_char_safe_and_flattens_newlines() {
        assert_eq!(elide("hello", 10), "hello");
        assert_eq!(elide("a\nb", 10), "a b");
        let ja = "これは長い日本語のテキストです";
        let out = elide(ja, 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn actions_can_be_disabled_without_disappearing() {
        let action: Action<()> = Action::new(Key::ActionReplace, "⏎", ()).disabled();
        assert!(action.on_press.is_none());
    }
}
