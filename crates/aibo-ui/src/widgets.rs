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

/// What a rail segment says about the row beside it (`design.md` §3).
///
/// The rail is the product's one signature element, and it earns the place by
/// encoding something true rather than decorating: at a glance it tells you
/// where the panel thinks you are. It also replaces every border in the design —
/// nothing else gets a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RailState {
    /// The row is present but not where the attention is. Drawn in `rule`.
    #[default]
    Inactive,
    /// The row that currently has the user's attention: the input while typing,
    /// the answer while streaming. Drawn in `amber`.
    Active,
    /// A permission or error row. Drawn in `danger`.
    ///
    /// Quiet by construction — `design.md` §4 treats a denied permission as a
    /// state, not an alarm — but it is a second channel beside the message's own
    /// colour, so severity never depends on reading one coloured word.
    Alert,
}

impl RailState {
    fn color(self, p: &theme::Palette) -> iced::Color {
        match self {
            RailState::Inactive => p.border,
            RailState::Active => p.accent,
            RailState::Alert => p.danger,
        }
    }
}

/// Attach a rail segment to a row (`design.md` §3).
///
/// Segments are stacked per row rather than drawn as one full-height bar with a
/// computed offset. The result is the same continuous 3 pt rail down the left
/// gutter, amber only beside the row that has attention, but it needs no layout
/// measurement, so it cannot drift out of alignment when a row changes height
/// mid-stream.
///
/// **The row's vertical rhythm lives inside this function, not between calls to
/// it.** §3 says the rail "runs the full height of the panel", and a parent
/// `column` with `spacing` set would break it into floating stubs with gaps of
/// background showing through — which is what a rail must never look like,
/// because a gap reads as the rail *ending*. So the padding that separates rows
/// is applied to the content here, inside the segment, and callers stack these
/// with **zero** spacing.
pub fn railed<'a, Message: 'a>(
    state: RailState,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    // Drawn as a **background the content is painted over**, not as a
    // `Length::Fill` sibling in a row.
    //
    // The sibling version is the obvious construction and it is wrong: a
    // `Fill`-height child makes the *row* report "I want to fill", the parent
    // `column` then sees five such rows and divides the panel between them, and
    // the result is a source line inflated into a band of empty space and an
    // answer squashed to a few points. Pinning the row to `Shrink` fixes the
    // squashing and kills the rail instead, because `Fill` inside `Shrink`
    // resolves to nothing.
    //
    // So: the outer container is painted the rail colour and inset from the
    // left by exactly `RAIL_WIDTH`; the inner container repaints the panel
    // ground over everything but that strip. Both are content-sized, so the
    // rail is always precisely as tall as the row beside it and can never
    // dictate the row's height.
    railed_with(state, theme::ground, content)
}

/// [`railed`], with the fill the content is painted in chosen by the caller.
///
/// Split out for the quick-pick, where the highlighted row wants the palette's
/// one elevation (`ink-raised`) rather than the ground: a 3pt rail alone is
/// legible on a source line, where it is the only marked row on screen, but
/// inside a list of ninety it is a detail the eye has to hunt for. Fill plus
/// rail reads instantly and adds no new colour.
pub fn railed_with<'a, Message: 'a>(
    state: RailState,
    fill: fn(&iced::Theme) -> container::Style,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    container(
        container(content.into())
            .padding([space(1.0), 0.0])
            .width(Length::Fill)
            .style(fill),
    )
    .padding(iced::Padding {
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
        left: theme::RAIL_WIDTH,
    })
    .width(Length::Fill)
    .style(move |t: &iced::Theme| container::Style {
        background: Some(Background::Color(state.color(&theme::palette_of(t)))),
        ..Default::default()
    })
    .into()
}

/// A small square identity mark — the picker's provider column (§16).
///
/// **Why letterforms and not vendor logos.** `design.md` §9 cut icons on the
/// grounds that "the rail plus a key hint carries every meaning an icon would",
/// and for *state* that holds: an amber rail says "here" better than any glyph.
/// Identity is the case it does not cover. Thirteen providers down a list are a
/// thing you recognise, not a thing you read, and a column of dim lowercase
/// words is the slowest possible way to present it.
///
/// Real logos would be faster still, and are deliberately not used:
///
/// * They are other people's trademarks, and aibo would be redistributing the
///   artwork in a signed binary (§19) — a licence question per vendor, for a
///   decoration.
/// * Drawn from memory they come out subtly wrong, and a mangled logo reads as
///   a broken build.
/// * A logo carries brand colour, and the palette has seven values on purpose.
///
/// So: two letters, mono, in a filled tile. Nothing to learn — `OR` beside
/// `OA` is unambiguous at a glance in a way an unlabelled pair of glyphs is
/// not — and it stays inside the palette. If licensed marks ever ship, they
/// drop in behind this same call.
///
/// `on_raised` inverts the fill. The tile is one elevation step from whatever
/// it sits on: raised against the ground, inset against a highlighted row.
/// Without the inversion the mark disappears into exactly the row the user is
/// looking at.
pub fn mark<'a, Message: 'a>(label: impl Into<String>, on_raised: bool) -> Element<'a, Message> {
    container(
        iced::widget::text(label.into())
            .size(theme::type_scale::CHIP)
            .font(theme::MONO_FONT)
            .style(if on_raised {
                theme::text_primary
            } else {
                theme::text_dim
            })
            .align_x(Alignment::Center),
    )
    .width(Length::Fixed(MARK_SIZE))
    .height(Length::Fixed(MARK_SIZE))
    .align_x(Alignment::Center)
    .align_y(Alignment::Center)
    .style(move |t: &iced::Theme| {
        if on_raised {
            theme::inset(t)
        } else {
            theme::raised(t)
        }
    })
    .into()
}

/// The provider's official mark, or the [`mark`] tile when none is bundled.
///
/// This is the "if licensed marks ever ship, they drop in behind this same
/// call" the [`mark`] doc promised. The owner asked for the real logos
/// (2026-08-01); the bundled sources are the monochrome SVGs the icon sets
/// distribute, tinted into the text ramp so they stay inside the palette —
/// which retires the brand-colour objection, and the redistribution question
/// is the owner's to make for their own build.
///
/// `fallback` is the caller's two-letter tile label, so a custom endpoint
/// (§10) still shows something in the column.
pub fn provider_logo<'a, Message: 'a>(
    provider: &str,
    fallback: impl Into<String>,
    on_raised: bool,
) -> Element<'a, Message> {
    let Some(handle) = crate::icons::provider_icon(provider) else {
        return mark(fallback, on_raised);
    };
    container(
        iced::widget::svg(handle)
            .width(Length::Fixed(MARK_SIZE - 6.0))
            .height(Length::Fixed(MARK_SIZE - 6.0))
            .style(if on_raised {
                theme::icon_primary
            } else {
                theme::icon_dim
            }),
    )
    .width(Length::Fixed(MARK_SIZE))
    .height(Length::Fixed(MARK_SIZE))
    .align_x(Alignment::Center)
    .align_y(Alignment::Center)
    .into()
}

/// Side of a [`mark`]'s tile.
///
/// Square, and fixed rather than derived from the label, so the marks form a
/// column that the eye can run down — the only reason to have them.
pub const MARK_SIZE: f32 = 22.0;

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

/// The source line: where you were, and what you had selected.
///
/// `design.md` §1 calls this the most interesting information on screen — "that
/// line is the whole pitch: it knows where you were" — and records that the
/// first build rendered it as the smallest, dullest element on the panel.
/// Correcting that inversion is mostly a matter of *showing the excerpt*, which
/// is why `excerpt` is no longer optional in practice: `ghostty` alone says far
/// less than `ghostty · "…and screencapture works"`.
///
/// Two things it deliberately no longer draws. The 2 pt accent leading rule is
/// gone, superseded by the rail that now runs down the whole gutter — two
/// vertical accent marks 16 pt apart read as a rendering bug. So is the chip's
/// box: §9 counts it among the borders being removed, and a pill around the one
/// line that is meant to feel ambient is the wrong container.
///
/// Renders the "no context" state rather than disappearing. §8 captures context
/// asynchronously with a deadline, so the panel is often shown *before* the
/// line has anything to say — text that pops into existence mid-interaction
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
        text(label)
            .size(type_scale::META)
            .font(theme::MONO_FONT)
            .style(theme::text_dim),
    ]
    .spacing(space(1.5))
    .align_y(Alignment::Center);

    // An empty excerpt is not an excerpt. A caret-only capture yields
    // `Some("")`, and quoting it renders `Ghostty · ""` — a pair of empty
    // quotes that says the panel read something and it was nothing.
    if let Some(excerpt) = excerpt.map(str::trim).filter(|e| !e.is_empty()) {
        line = line.push(
            text(format!("· \u{201c}{}\u{201d}", elide(excerpt, 96)))
                .size(type_scale::META)
                .font(theme::MONO_FONT)
                .style(theme::text_dim),
        );
    }

    container(line).style(theme::chip).into()
}

/// Pre-first-token: a three-dot mono ellipsis, and deliberately not a spinner.
///
/// `design.md` §4 is specific about the reason. A spinner means *indeterminate*
/// — it promises nothing about when this ends — and §15 budgets first token at
/// around 400 ms. Spinning for 400 ms reads as a stall the product does not
/// have, and it is the single most common way a fast thing is made to feel
/// slow. Three static dots say "in progress" without making a claim about
/// duration.
///
/// `reserve` holds the answer's height from this moment, so the first chunk
/// replaces the dots in place instead of growing the panel out from under the
/// eye (§16: "streaming must not reflow").
pub fn thinking<'a, Message: 'a>(reserve: Option<f32>) -> Element<'a, Message> {
    let dots = text("\u{00b7}\u{00b7}\u{00b7}")
        .size(type_scale::META)
        .font(theme::MONO_FONT)
        .style(theme::text_dim);

    match reserve {
        Some(height) => container(dots)
            .height(Length::Fixed(height))
            .align_y(Alignment::Start)
            .into(),
        None => dots.into(),
    }
}

/// The empty panel: an invitation, not a mood (`design.md` §4, §6).
///
/// One dim line under the source line, with no placeholder box around it.
/// `design.md` §6: "Empty states are an invitation to act, not a mood."
pub fn empty_invitation<'a, Message: 'a>() -> Element<'a, Message> {
    text(i18n::t(Key::PanelEmptyInvitation))
        .size(type_scale::META)
        .font(theme::MONO_FONT)
        .style(theme::text_dim)
        .into()
}

/// The source line while context capture is still in flight (`design.md` §4).
pub fn reading_context_line<'a, Message: 'a>() -> Element<'a, Message> {
    source_note(i18n::t(Key::ContextChipReading))
}

/// The source line once capture has settled with nothing (`design.md` §4).
pub fn unavailable_context_line<'a, Message: 'a>() -> Element<'a, Message> {
    source_note(i18n::t(Key::ContextChipUnavailable))
}

/// One dim mono line in the source-line slot, holding its height.
fn source_note<'a, Message: 'a>(body: &str) -> Element<'a, Message> {
    container(
        text(body.to_owned())
            .size(type_scale::META)
            .font(theme::MONO_FONT)
            .style(theme::text_dim),
    )
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
                .font(theme::MONO_FONT)
                .style(theme::text_dim),
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
                .font(theme::MONO_FONT)
                .style(theme::text_dim),
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
                .font(theme::MONO_FONT)
                .style(theme::text_dim),
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

/// The severity bar's height: one line of [`type_scale::META`] with its
/// leading. A constant rather than `Length::Fill` — see the note in [`toast`].
const TOAST_BAR_HEIGHT: f32 = 18.0;

/// A non-blocking notice (§13: `InsertFailed`, `CaptureFailed`).
///
/// Rendered as a row of the panel, above the composer — the result stays
/// visible beside it so the user can still copy manually, and a notice never
/// replaces the content it is complaining about.
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
        // Fixed height, NOT `Length::Fill`. A cross-axis `Fill` makes the row
        // claim every point the parent can spare, and the toast lives in a
        // column under a `Length::Fill` panel — so one line of text rendered
        // as a box the height of the whole window with the sentence stranded
        // at the bottom (owner screenshot, 2026-08-02).
        container(Space::new().width(2.0).height(TOAST_BAR_HEIGHT)).style(
            move |t: &iced::Theme| container::Style {
                background: Some(Background::Color(severity.color(&theme::palette_of(t)))),
                ..Default::default()
            },
        ),
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
