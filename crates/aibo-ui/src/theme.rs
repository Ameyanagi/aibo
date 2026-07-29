//! Dark-first palette, one accent, spring motion (§16).
//!
//! "Iced gives you nothing by default, so the theme is a real artefact built
//! once in `aibo-ui/theme.rs`" (§16). This module is that artefact: the whole
//! palette (one accent hue, two surface elevations, three text weights, plus
//! semantic red/amber), the 4 px / 8 px scale, the type ramp, the motion
//! constants, and the style functions every view uses.
//!
//! Two constraints from §16 are encoded here rather than left to discipline:
//!
//! * **The palette is closed.** Views may not invent colours; they pick from
//!   [`Palette`]. That is what keeps contrast auditable against WCAG AA.
//! * **The panel width is a range, not a number.** §9 requires it to grow for
//!   localisation and shrink on small displays, so [`PANEL_WIDTH_DEFAULT`] sits
//!   between [`PANEL_WIDTH_MIN`] and [`PANEL_WIDTH_MAX`].

use iced::border::Radius;
use iced::overlay;
use iced::theme::Base as _;
use iced::widget::{button, container, pick_list, rule, scrollable, text, text_editor, text_input};
use iced::{Background, Border, Color, Font, Shadow, Theme, Vector};

// ---------------------------------------------------------------------------
// Scale — 4 px base, 8 px rhythm (§16)
// ---------------------------------------------------------------------------

/// The base unit. Every dimension in the UI is a multiple of this.
pub const BASE: f32 = 4.0;

/// The vertical rhythm. Stack spacing is a multiple of this.
pub const RHYTHM: f32 = 8.0;

/// `n` base units.
pub const fn space(n: f32) -> f32 {
    BASE * n
}

/// Corner radius of the panel window itself.
///
/// §16 said 12, which is what a *card* wants. At 680 pt wide against a
/// desktop backdrop it read as very nearly square — the reference points are
/// Spotlight and Raycast, which sit around 16–20. A floating overlay needs
/// more radius than an inline card to read as detached from the screen.
pub const RADIUS: f32 = 18.0;

/// Corner radius of inline chrome — chips, buttons, key hints.
///
/// Deliberately much smaller than [`RADIUS`]: nesting near-equal radii makes
/// the inner element look like it is bulging out of the outer one.
pub const RADIUS_SMALL: f32 = 6.0;

/// Default panel width in logical points (§16).
pub const PANEL_WIDTH_DEFAULT: f32 = 680.0;

/// Narrowest the panel may get, for small and portrait displays (§9).
pub const PANEL_WIDTH_MIN: f32 = 420.0;

/// Widest the panel may get, so long translations do not force a redesign (§9).
pub const PANEL_WIDTH_MAX: f32 = 920.0;

/// Height the panel collapses to with no response — input plus chrome.
pub const PANEL_HEIGHT_COLLAPSED: f32 = 132.0;

/// Ceiling on panel height; beyond this the answer area scrolls (§16
/// "long-output-with-scroll").
pub const PANEL_HEIGHT_MAX: f32 = 520.0;

/// Height reserved for the answer box on the first chunk.
///
/// §16: "streaming must not reflow". The box is sized once and grown in
/// discrete steps, never per token.
pub const ANSWER_BOX_MIN_HEIGHT: f32 = 96.0;

/// Minimum selectable answer height inside a chat bubble.
///
/// The standalone rewrite surface needs a generous editing region, while a
/// one-line chat reply should remain one line. Keeping these as separate
/// tokens prevents a short reply from turning into a large empty card.
pub const CHAT_ANSWER_MIN_HEIGHT: f32 = 28.0;

/// Shared height for composer and action controls.
pub const CONTROL_HEIGHT: f32 = 36.0;

/// Height of the action row — `⏎ Replace`, `⌘C Copy`, `esc Dismiss`.
///
/// **Must be added to the panel height whenever an answer is shown.**
/// [`PANEL_HEIGHT_COLLAPSED`] covers the input and its chrome only, so a panel
/// sized `COLLAPSED + ANSWER_BOX_MIN_HEIGHT` is too short: the action row
/// renders, is pushed past the window's bottom edge, and is clipped. The result
/// is an answer with no visible way to accept, copy or dismiss it — §16's
/// "every action has a key, shown" failing at the one moment the actions
/// matter.
pub const ACTION_ROW_HEIGHT: f32 = 36.0;

/// Height of one metadata line below the answer.
///
/// Two of these can appear, and **both sit between the answer box and the
/// action row**, so each one pushes the actions further toward the edge:
/// the attribution line (`codex · gpt-5.5 · 435ms · ¢0.02`) and up to two
/// footnotes (fallback substitution, context truncation). Counting the action
/// row alone still clipped it — the rows above it have to be counted too.
pub const META_LINE_HEIGHT: f32 = 22.0;

/// Minimum interactive target edge in logical points.
///
/// 44 pt is the platform-independent floor used for mouse, touchpad and touch
/// accessibility. Visual chrome may remain smaller inside the target.
pub const MIN_HIT_TARGET: f32 = 44.0;

// ---------------------------------------------------------------------------
// Type (§16)
// ---------------------------------------------------------------------------

/// Type sizes, from the `design.md` §2 ramp.
///
/// The ramp is deliberately short. Each size is a role, not a step on a
/// continuum, and the roles are: what you type, what you read, what tells you
/// where the answer came from, and what labels a group.
pub mod type_scale {
    /// The input. Mono — the caret is the accent, so the field it sits in is
    /// monospaced to match (`design.md` §2: "Input | Plex Mono | 15 / 400").
    pub const BODY: f32 = 15.0;
    /// Answer body. Sans, one step down from the input, set at
    /// [`ANSWER_LINE_HEIGHT`].
    pub const ANSWER: f32 = 14.0;
    /// Source line, provenance, key hints. Mono, dim.
    ///
    /// 11 rather than 12: this line is read at a glance, not scanned, and
    /// dropping it a point is what stops the provenance row competing with the
    /// answer it annotates.
    pub const META: f32 = 11.0;
    /// Section headings in settings and the task window. Sans, weight 600.
    pub const HEADING: f32 = 13.0;
    /// The context chip excerpt, which shares the source line's treatment.
    pub const CHIP: f32 = 11.0;
    /// Display scale, used in exactly one place: the device-code sign-in screen.
    ///
    /// A deliberate exception to "three text roles, not a continuum". That code
    /// has to be read character by character and typed into a browser, and a
    /// mistake costs a full 15-minute retry cycle — the only screen in the
    /// product where transcription accuracy is the whole job.
    pub const DISPLAY: f32 = 28.0;

    /// Line height for the answer body, as a multiple of [`ANSWER`].
    ///
    /// `design.md` §2 sets 1.55. Prose read in a floating overlay needs more
    /// leading than prose in a document, because there is no page margin to
    /// rest against.
    pub const ANSWER_LINE_HEIGHT: f32 = 1.55;
}

// ---------------------------------------------------------------------------
// The rail (design.md §3)
// ---------------------------------------------------------------------------

/// Width of the state-carrying rail that runs the full height of the panel.
///
/// `design.md` §3 makes this the product's one signature element, and it earns
/// the place by encoding something true rather than decorating: the rail is
/// [`Palette::border`] except on the row that currently has the user's
/// attention, where it is [`Palette::accent`] — or [`Palette::danger`] on a
/// permission or error row. It replaces every inner border in the design.
pub const RAIL_WIDTH: f32 = 3.0;

/// Gap between the rail and the content it annotates (`design.md` §2).
pub const RAIL_GUTTER: f32 = 16.0;

/// The interface face.
///
/// §16 records the unresolved contradiction honestly: no bundled variable sans
/// has CJK coverage, and §15 budgets ≤ 25 MB for the whole binary. The
/// resolution is to bundle Latin faces for identity and declare an explicit
/// CJK fallback chain, accepting mixed rendering in Japanese.
///
/// `Font::DEFAULT` until the licensed faces are vendored; swapping this
/// constant is the only change required then.
pub const UI_FONT: Font = Font::DEFAULT;

/// The face for the input and for code in responses. The caret is the accent.
pub const MONO_FONT: Font = Font::MONOSPACE;

/// The CJK fallback chain (§16).
///
/// SPIKE: S10 — verify that a mixed Latin/CJK line looks deliberate rather than
/// broken once the Latin faces are bundled, on both platforms and at both
/// 1× and 2× scale. For this author the mixed case is the common one.
pub const CJK_FALLBACKS: &[&str] = &[
    "Hiragino Sans",
    "Yu Gothic UI",
    "Noto Sans CJK JP",
    "Meiryo",
];

// ---------------------------------------------------------------------------
// Motion (§16)
// ---------------------------------------------------------------------------

/// Motion constants. §16 allows animation on exactly three things: panel
/// in/out, height change, streaming reveal.
pub mod motion {
    use std::time::Duration;

    /// Fast end of the spring range.
    pub const FAST: Duration = Duration::from_millis(180);
    /// Slow end of the spring range.
    pub const SLOW: Duration = Duration::from_millis(220);

    /// Whether animation should run at all.
    ///
    /// §16 requires respecting the OS reduced-motion preference. The platform
    /// layer reports it; this is the shell-side gate so no view has to ask.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum Motion {
        /// Animate within the 180–220 ms spring range.
        #[default]
        Full,
        /// Snap. No panel fade, no height tween, no reveal.
        Reduced,
    }

    impl Motion {
        /// The duration to use for a transition, honouring the preference.
        pub const fn duration(self, requested: Duration) -> Duration {
            match self {
                Motion::Full => requested,
                Motion::Reduced => Duration::ZERO,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Palette (§16)
// ---------------------------------------------------------------------------

/// The closed colour set of `design.md` §2: an ink ground, one elevation, a
/// hairline, three text weights and a single amber accent.
///
/// Seven values carry the design. If a view needs an eighth, something else is
/// wrong — views pick from here and may not invent colours, which is what keeps
/// contrast auditable against WCAG AA in this module's tests.
///
/// Amber is not a house style. It is chosen for a reason specific to this
/// product: it is the colour of a text caret, it signals attention without
/// alarm, and it leaves red free for the permission and danger states §16
/// reserves.
///
/// Three fields below are **not** among the seven, and each is here for a
/// stated reason rather than by drift:
///
/// * [`Palette::text_faint`] is an alias of [`Palette::text_dim`]. `design.md`
///   collapses the two — a placeholder and a piece of metadata are the same
///   weight — but four call sites still name the tertiary role, so the alias
///   stays until they are folded. It must never diverge.
/// * [`Palette::warning`] is an alias of [`Palette::accent`]. Amber *is* the
///   warning colour; a budget notice and an active rail are the same hue by
///   design, not by coincidence.
/// * [`Palette::success`] is the one genuine addition. It exists for the diff
///   viewer, where added and removed lines are green and red by a convention
///   older than this product, and for the granted-permission tick. It is
///   deliberately absent from the panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// `ink` — the panel ground. Near-black with a blue cast, so amber reads
    /// warm against it.
    pub surface: Color,
    /// `ink-raised` — the one elevation: settings sidebar, hovered rows.
    pub surface_raised: Color,
    /// `rule` — hairlines and the inactive rail.
    ///
    /// This is a decorative separator, never the sole identifier of a control.
    /// The rail and the caret carry that job, which is why this value is
    /// allowed to sit below the 3:1 that WCAG SC 1.4.11 requires of a control
    /// boundary.
    pub border: Color,
    /// `amber` — the caret, focus, the active rail segment, the one live
    /// accent.
    pub accent: Color,
    /// Accent at low alpha, for selection and hover fills.
    pub accent_muted: Color,
    /// `text` — primary.
    pub text: Color,
    /// `text-dim` — source line, provenance, key hints.
    pub text_dim: Color,
    /// Tertiary text. An alias of [`Palette::text_dim`]; see the type docs.
    pub text_faint: Color,
    /// `danger` — permission prompts and destructive confirms **only**.
    pub danger: Color,
    /// Budget warnings, degraded providers, truncation. An alias of
    /// [`Palette::accent`]; see the type docs.
    pub warning: Color,
    /// Diff additions and the granted-permission tick. Outside the seven; see
    /// the type docs.
    pub success: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

impl Palette {
    /// The dark palette — `design.md` §2 verbatim. Dark-first is the product
    /// default (§16).
    pub const DARK: Self = Self {
        surface: rgb(0x0E, 0x11, 0x16),
        surface_raised: rgb(0x16, 0x1A, 0x21),
        border: rgb(0x26, 0x2C, 0x36),
        accent: rgb(0xF0, 0xA7, 0x42),
        accent_muted: rgba(0xF0, 0xA7, 0x42, 0.12),
        text: rgb(0xE6, 0xE9, 0xEF),
        text_dim: rgb(0x8B, 0x94, 0xA3),
        // Alias of `text_dim`. See the `Palette` docs.
        text_faint: rgb(0x8B, 0x94, 0xA3),
        danger: rgb(0xE5, 0x53, 0x4B),
        // Alias of `accent`. See the `Palette` docs.
        warning: rgb(0xF0, 0xA7, 0x42),
        success: rgb(0x5A, 0xD1, 0x9A),
    };

    /// The light palette.
    ///
    /// `design.md` specifies the dark ground only, so this is derived rather
    /// than quoted: the same relationships — one elevation, a hairline, three
    /// text weights, one accent — inverted, with the accent darkened until it
    /// carries AA as *text* on a light ground. Amber at `#F0A742` is a 1.9:1
    /// foreground on white and cannot be reused unchanged; the hue is kept and
    /// the value dropped, so the two appearances still read as one product.
    pub const LIGHT: Self = Self {
        surface: rgb(0xF7, 0xF8, 0xFA),
        surface_raised: rgb(0xED, 0xEF, 0xF3),
        border: rgb(0xD5, 0xD9, 0xE0),
        accent: rgb(0x8A, 0x53, 0x00),
        accent_muted: rgba(0x8A, 0x53, 0x00, 0.10),
        text: rgb(0x12, 0x15, 0x1A),
        text_dim: rgb(0x56, 0x5E, 0x6D),
        // Alias of `text_dim`. See the `Palette` docs.
        text_faint: rgb(0x56, 0x5E, 0x6D),
        danger: rgb(0xB3, 0x22, 0x1B),
        // Alias of `accent`. See the `Palette` docs.
        warning: rgb(0x8A, 0x53, 0x00),
        success: rgb(0x14, 0x6E, 0x50),
    };
}

/// Which palette a window is rendering with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Appearance {
    /// Dark-first, the product default (§16).
    #[default]
    Dark,
    /// Light.
    Light,
}

impl Appearance {
    /// The palette for this appearance.
    pub const fn palette(self) -> Palette {
        match self {
            Appearance::Dark => Palette::DARK,
            Appearance::Light => Palette::LIGHT,
        }
    }

    /// The `iced::Theme` to hand the runtime.
    ///
    /// The built-in `Theme` is kept as the widget theme so every stock widget
    /// still has a `Catalog` impl; aibo's own colours arrive through the style
    /// functions in this module, which take precedence.
    pub fn iced_theme(self) -> Theme {
        match self {
            Appearance::Dark => Theme::custom(
                "aibo dark".to_owned(),
                iced::theme::Palette {
                    background: Palette::DARK.surface,
                    text: Palette::DARK.text,
                    primary: Palette::DARK.accent,
                    success: Palette::DARK.success,
                    warning: Palette::DARK.warning,
                    danger: Palette::DARK.danger,
                },
            ),
            Appearance::Light => Theme::custom(
                "aibo light".to_owned(),
                iced::theme::Palette {
                    background: Palette::LIGHT.surface,
                    text: Palette::LIGHT.text,
                    primary: Palette::LIGHT.accent,
                    success: Palette::LIGHT.success,
                    warning: Palette::LIGHT.warning,
                    danger: Palette::LIGHT.danger,
                },
            ),
        }
    }
}

/// Recover aibo's palette from the `iced::Theme` a style function is handed.
///
/// Style callbacks only receive the runtime theme, so this maps back. It keys
/// off the theme's own mode rather than its name, which survives the user
/// switching to a stock iced theme in a debug build.
pub fn palette_of(theme: &Theme) -> Palette {
    match theme.base().background_color {
        c if luminance(c) < 0.5 => Palette::DARK,
        _ => Palette::LIGHT,
    }
}

/// Relative luminance, used both for [`palette_of`] and for contrast checks.
pub fn luminance(color: Color) -> f32 {
    fn channel(c: f32) -> f32 {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

/// WCAG contrast ratio between two opaque colours.
///
/// §16 requires contrast ratios that pass AA. Exposed so the palette can be
/// asserted in tests rather than eyeballed.
pub fn contrast_ratio(a: Color, b: Color) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

// ---------------------------------------------------------------------------
// Style functions
// ---------------------------------------------------------------------------

/// The panel body: elevation 0, soft shadow, and the one border that survives.
///
/// `design.md` §9 removes every border in the design bar one hairline. This is
/// not that hairline — it is the panel's own edge against an arbitrary desktop,
/// and it stays because the shadow alone does not separate the panel from a
/// light background. Every *inner* border is gone; see [`raised`].
pub fn panel_surface(theme: &Theme) -> container::Style {
    let p = palette_of(theme);
    container::Style {
        text_color: Some(p.text),
        background: Some(Background::Color(p.surface)),
        border: Border {
            color: p.border,
            width: 1.0,
            radius: Radius::new(RADIUS),
        },
        shadow: Shadow {
            color: Color {
                a: 0.45,
                ..Color::BLACK
            },
            offset: Vector::new(0.0, 8.0),
            blur_radius: 32.0,
        },
        snap: true,
    }
}

/// Elevation 1: code blocks, list rows, hovered rows.
///
/// Fill only. `design.md` §9: group with space and a change of ground, never
/// with a box — a bordered container inside a bordered panel is the exact
/// nesting the redesign exists to remove.
pub fn raised(theme: &Theme) -> container::Style {
    let p = palette_of(theme);
    container::Style {
        text_color: Some(p.text),
        background: Some(Background::Color(p.surface_raised)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: Radius::new(RADIUS_SMALL),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// The source line — `ghostty · "…and screencapture works"`.
///
/// No fill and no border. `design.md` §1 calls this "the most interesting
/// information on screen … the whole pitch: it knows where you were", and §9
/// records that rendering it as a fourth bordered box is the failure mode. It
/// is dim mono text next to the rail, and nothing else.
pub fn chip(theme: &Theme) -> container::Style {
    let p = palette_of(theme);
    container::Style {
        text_color: Some(p.text_dim),
        background: None,
        border: Border::default(),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A user-authored turn in the transcript.
///
/// Keeps a quiet accent fill — in a multi-turn transcript, authorship is the
/// one distinction the rail cannot carry, because both turns sit on the same
/// rail. The border is gone; the fill alone is the signal.
pub fn user_bubble(theme: &Theme) -> container::Style {
    let p = palette_of(theme);
    container::Style {
        text_color: Some(p.text),
        background: Some(Background::Color(Color {
            a: 0.14,
            ..p.accent
        })),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: Radius::new(12.0),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// An assistant-authored turn in the transcript.
///
/// Neither fill nor border: the answer is the panel's primary content and sits
/// directly on the ground, the way body text sits on a page. Boxing it is what
/// made the first render "look like a form" (`design.md` preamble).
pub fn assistant_bubble(theme: &Theme) -> container::Style {
    let p = palette_of(theme);
    container::Style {
        text_color: Some(p.text),
        background: None,
        border: Border::default(),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Severity of a banner or inline treatment, mapped from §13.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Severity {
    /// Neutral information — a fallback footnote, a truncation marker.
    #[default]
    Info,
    /// Amber: budget warnings, degraded providers, truncated output.
    Warning,
    /// Red: auth failures, denied permissions, destructive approvals.
    Danger,
    /// Green: a granted permission.
    Success,
}

impl Severity {
    /// The foreground colour for this severity.
    pub const fn color(self, p: &Palette) -> Color {
        match self {
            Severity::Info => p.text_dim,
            Severity::Warning => p.warning,
            Severity::Danger => p.danger,
            Severity::Success => p.success,
        }
    }
}

/// A banner tinted for `severity` — the permission-state banner, the inline
/// error treatment, the budget notice.
/// A severity treatment: text colour and the rail, with no box of its own.
///
/// `design.md` §3 shows the error state carried entirely by the rail — "Error
/// state — the rail carries it, no box required" — against a current build that
/// renders the same state as a red-bordered rectangle containing a bordered
/// button, two more boxes in a design that already had three.
///
/// The tint is gone for a second, measurable reason: `danger` is 4.71:1 on
/// `ink-raised`, so *any* danger-tinted fill behind danger-coloured text drops
/// it below WCAG AA. The fill was buying nothing and costing the contrast
/// budget. Severity now reaches the eye through [`Severity::color`] on the text
/// and through the rail segment beside it — two channels, neither of them a
/// border.
pub fn banner(severity: Severity) -> impl Fn(&Theme) -> container::Style {
    move |theme: &Theme| {
        let _ = severity;
        let p = palette_of(theme);
        container::Style {
            text_color: Some(p.text),
            background: None,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// A toast: elevation 1 with a shadow, never blocking (§13).
///
/// Keeps its fill and shadow — a toast floats over the panel's own content, so
/// it is the one place where separation has to be drawn rather than implied.
/// The border still goes; the shadow does that job.
pub fn toast(theme: &Theme) -> container::Style {
    let p = palette_of(theme);
    container::Style {
        text_color: Some(p.text),
        background: Some(Background::Color(p.surface_raised)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: Radius::new(RADIUS_SMALL),
        },
        shadow: Shadow {
            color: Color {
                a: 0.35,
                ..Color::BLACK
            },
            offset: Vector::new(0.0, 4.0),
            blur_radius: 16.0,
        },
        snap: true,
    }
}

/// Primary text.
pub fn text_primary(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(palette_of(theme).text),
    }
}

/// Secondary text: model, latency, cost, key hints.
pub fn text_dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(palette_of(theme).text_dim),
    }
}

/// Tertiary text: placeholders and disabled labels.
pub fn text_faint(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(palette_of(theme).text_faint),
    }
}

/// Accent text: the active key hint, a selected row.
pub fn text_accent(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(palette_of(theme).accent),
    }
}

/// Text placed on the solid accent fill of a primary button.
pub fn text_on_primary(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(palette_of(theme).surface),
    }
}

/// Destructive-button text, including its key hint.
pub fn text_danger(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(palette_of(theme).danger),
    }
}

/// Text in a severity colour.
pub fn text_severity(severity: Severity) -> impl Fn(&Theme) -> text::Style {
    move |theme: &Theme| text::Style {
        color: Some(severity.color(&palette_of(theme))),
    }
}

/// The panel input — no well, no border. The caret is the accent (§16).
///
/// `design.md` §3's thesis is that the panel should read as "the caret
/// continuing into a second place", so the input is not a field you fill in; it
/// is the line you were already typing on. Focus is shown by the amber rail
/// segment beside the row and by the amber caret in it, which is why removing
/// the focus border here loses no focus indication — see `rail` in `widgets`.
pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let _ = status;
    let p = palette_of(theme);
    text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        icon: p.text_dim,
        placeholder: p.text_dim,
        value: p.text,
        selection: p.accent_muted,
    }
}

/// A selectable, read-only answer surface.
///
/// Transparent for the same reason as [`assistant_bubble`]: the answer is the
/// content, not a control. Selection remains visible through `accent_muted`.
pub fn answer_editor(theme: &Theme, status: text_editor::Status) -> text_editor::Style {
    let _ = status;
    let p = palette_of(theme);
    text_editor::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        placeholder: p.text_dim,
        value: p.text,
        selection: p.accent_muted,
    }
}

/// Read-only assistant text inside an already-bordered chat bubble.
pub fn chat_answer_editor(theme: &Theme, _status: text_editor::Status) -> text_editor::Style {
    let p = palette_of(theme);
    text_editor::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: Radius::new(0.0),
        },
        placeholder: p.text_faint,
        value: p.text,
        selection: p.accent_muted,
    }
}

/// A quiet action button. Every action is keyboard-reachable, so the button is
/// an affordance rather than the primary path (§16).
pub fn action_button(theme: &Theme, status: button::Status) -> button::Style {
    let p = palette_of(theme);
    let (background, text_color) = match status {
        button::Status::Active => (Color::TRANSPARENT, p.text_dim),
        // Hover is a change of ground, not a tint. `design.md` §2 gives the
        // palette exactly one elevation and names "hovered rows" as its use.
        button::Status::Hovered => (p.surface_raised, p.text),
        button::Status::Pressed => (p.accent_muted, p.accent),
        button::Status::Disabled => (Color::TRANSPARENT, p.text_dim),
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: Radius::new(RADIUS_SMALL),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A selected navigation or choice row.
///
/// Views also include a checkmark in their label, so selection never depends on
/// perceiving colour. The 2 pt accent border is gone: `design.md` §7 marks the
/// active settings row with an amber rail segment instead, reusing the panel's
/// identity element so the two windows read as one product.
pub fn selected_button(theme: &Theme, status: button::Status) -> button::Style {
    let p = palette_of(theme);
    let background = match status {
        button::Status::Active => p.accent_muted,
        button::Status::Hovered => Color {
            a: 0.20,
            ..p.accent
        },
        button::Status::Pressed => Color {
            a: 0.26,
            ..p.accent
        },
        button::Status::Disabled => Color {
            a: 0.08,
            ..p.accent
        },
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: p.text,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: Radius::new(RADIUS_SMALL),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// The one emphasised button per surface — "Sign in", "Approve", "Retry".
pub fn primary_button(theme: &Theme, status: button::Status) -> button::Style {
    let p = palette_of(theme);
    let background = match status {
        button::Status::Active => p.accent,
        button::Status::Hovered => Color {
            a: 0.92,
            ..p.accent
        },
        button::Status::Pressed => Color {
            a: 0.88,
            ..p.accent
        },
        button::Status::Disabled => Color {
            a: 0.32,
            ..p.accent
        },
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: p.surface,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: Radius::new(RADIUS_SMALL),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A destructive button — "Deny", and anything behind a typed confirmation
/// (§11).
/// A destructive button — "Deny", and anything behind a typed confirmation
/// (§11).
///
/// Deliberately has **no danger tint**. `danger` measures 4.71:1 against
/// `ink-raised`, so tinting the ground behind danger-coloured text with the
/// same hue drops the label below WCAG AA — the old 0.06/0.12/0.16 ramp put
/// every interaction state under 4.5:1. Hover and press use the palette's one
/// elevation instead, which leaves the label's contrast untouched and keeps the
/// red for the word itself.
pub fn danger_button(theme: &Theme, status: button::Status) -> button::Style {
    let p = palette_of(theme);
    let background = match status {
        button::Status::Active | button::Status::Disabled => Color::TRANSPARENT,
        button::Status::Hovered | button::Status::Pressed => p.surface_raised,
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: p.danger,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: Radius::new(RADIUS_SMALL),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// The model picker.
///
/// iced's stock `pick_list` catalogue draws a bordered well, which made it the
/// last visible box in the panel after §9's border removal — and the most
/// conspicuous one, since it sits on the source line where the design wants
/// nothing but dim mono text. Fill and border both go; the disclosure caret is
/// the affordance, and hover raises the ground the way every other control here
/// does.
pub fn model_picker(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let p = palette_of(theme);
    let background = match status {
        pick_list::Status::Hovered | pick_list::Status::Opened { .. } => p.surface_raised,
        pick_list::Status::Active => Color::TRANSPARENT,
    };
    pick_list::Style {
        text_color: p.text_dim,
        placeholder_color: p.text_dim,
        handle_color: p.text_dim,
        background: Background::Color(background),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: Radius::new(RADIUS_SMALL),
        },
    }
}

/// The model picker's dropped menu. Same ground rules: fill, no border.
pub fn model_picker_menu(theme: &Theme) -> overlay::menu::Style {
    let p = palette_of(theme);
    overlay::menu::Style {
        background: Background::Color(p.surface_raised),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: Radius::new(RADIUS_SMALL),
        },
        text_color: p.text,
        selected_background: Background::Color(p.accent_muted),
        selected_text_color: p.text,
        shadow: Shadow {
            color: Color {
                a: 0.35,
                ..Color::BLACK
            },
            offset: Vector::new(0.0, 4.0),
            blur_radius: 16.0,
        },
    }
}

/// The panel's own ground, for painting over a rail strip.
///
/// Used by `widgets::railed` to mask everything except the 3 pt bar; see the
/// comment there for why the rail is drawn as a background rather than as a
/// `Fill`-height sibling.
pub fn ground(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(palette_of(theme).surface)),
        ..container::Style::default()
    }
}

/// A hairline separator.
pub fn separator(theme: &Theme) -> rule::Style {
    let p = palette_of(theme);
    rule::Style {
        color: p.border,
        radius: Radius::new(0.0),
        fill_mode: rule::FillMode::Full,
        snap: true,
    }
}

/// Scrollbars: present but recessive; the keyboard is the primary path.
pub fn scroller(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let p = palette_of(theme);
    let hovered = matches!(
        status,
        scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. }
    );
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: Background::Color(if hovered { p.text_faint } else { p.border }),
            border: Border {
                radius: Radius::new(RADIUS_SMALL),
                ..Border::default()
            },
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        ..scrollable::default(theme, status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composite_over(foreground: Color, background: Color) -> Color {
        let alpha = foreground.a;
        Color {
            r: foreground.r * alpha + background.r * (1.0 - alpha),
            g: foreground.g * alpha + background.g * (1.0 - alpha),
            b: foreground.b * alpha + background.b * (1.0 - alpha),
            a: 1.0,
        }
    }

    fn style_background(background: Option<Background>, surface: Color) -> Color {
        match background {
            Some(Background::Color(color)) => composite_over(color, surface),
            _ => surface,
        }
    }

    #[test]
    fn every_meaningful_text_colour_passes_aa_on_both_surfaces() {
        for p in [Palette::DARK, Palette::LIGHT] {
            for foreground in [
                p.text,
                p.text_dim,
                p.text_faint,
                p.accent,
                p.danger,
                p.warning,
                p.success,
            ] {
                for background in [p.surface, p.surface_raised] {
                    assert!(
                        contrast_ratio(foreground, background) >= 4.5,
                        "{foreground:?} fails AA on {background:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn banner_text_passes_aa_on_the_actual_tinted_background() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = appearance.iced_theme();
            let p = appearance.palette();
            for surface in [p.surface, p.surface_raised] {
                for severity in [
                    Severity::Info,
                    Severity::Warning,
                    Severity::Danger,
                    Severity::Success,
                ] {
                    let style = banner(severity)(&theme);
                    let background = style_background(style.background, surface);
                    for foreground in [p.text, p.text_dim, severity.color(&p)] {
                        assert!(
                            contrast_ratio(foreground, background) >= 4.5,
                            "{appearance:?}/{severity:?}: {foreground:?} fails on {background:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn enabled_button_text_passes_aa_in_every_interaction_state() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = appearance.iced_theme();
            let p = appearance.palette();
            for surface in [p.surface, p.surface_raised] {
                for style in [
                    action_button,
                    selected_button,
                    primary_button,
                    danger_button,
                ] {
                    for status in [
                        button::Status::Active,
                        button::Status::Hovered,
                        button::Status::Pressed,
                    ] {
                        let style = style(&theme, status);
                        let background = style_background(style.background, surface);
                        assert!(
                            contrast_ratio(style.text_color, background) >= 4.5,
                            "{appearance:?}/{status:?}: {:?} fails on {background:?}",
                            style.text_color
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn accent_action_labels_pass_aa_inside_every_banner() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = appearance.iced_theme();
            let p = appearance.palette();
            for severity in [
                Severity::Info,
                Severity::Warning,
                Severity::Danger,
                Severity::Success,
            ] {
                let banner = banner(severity)(&theme);
                let banner_background = style_background(banner.background, p.surface);
                let action = action_button(&theme, button::Status::Pressed);
                let action_background = style_background(action.background, banner_background);
                assert!(
                    contrast_ratio(p.accent, action_background) >= 4.5,
                    "{appearance:?}/{severity:?}: accent action text fails on nested background"
                );
            }
        }
    }

    /// `design.md` §2 is a table of seven hexes. Quoting them back is the only
    /// way an edit to the palette stays an edit to the *design* rather than a
    /// drift away from it — the previous palette was indigo `#7C93FF` against a
    /// spec that says amber `#F0A742`, and nothing caught it.
    #[test]
    fn the_dark_palette_is_design_md_verbatim() {
        let p = Palette::DARK;
        assert_eq!(p.surface, rgb(0x0E, 0x11, 0x16), "ink");
        assert_eq!(p.surface_raised, rgb(0x16, 0x1A, 0x21), "ink-raised");
        assert_eq!(p.border, rgb(0x26, 0x2C, 0x36), "rule");
        assert_eq!(p.text, rgb(0xE6, 0xE9, 0xEF), "text");
        assert_eq!(p.text_dim, rgb(0x8B, 0x94, 0xA3), "text-dim");
        assert_eq!(p.accent, rgb(0xF0, 0xA7, 0x42), "amber");
        assert_eq!(p.danger, rgb(0xE5, 0x53, 0x4B), "danger");
    }

    /// Two fields are aliases and the type docs say so. An alias that silently
    /// diverges is worse than no alias, because the divergence is invisible at
    /// every call site.
    #[test]
    fn aliased_tokens_never_diverge() {
        for p in [Palette::DARK, Palette::LIGHT] {
            assert_eq!(p.text_faint, p.text_dim, "text_faint aliases text_dim");
            assert_eq!(p.warning, p.accent, "warning aliases accent");
        }
    }

    /// `design.md` §9: "All borders except one hairline. This is the single
    /// biggest change from the current build, and the one that will make it
    /// stop looking generic."
    ///
    /// The exception is [`panel_surface`], which draws the panel's own edge
    /// against an arbitrary desktop. Everything else is grouped by space, by
    /// ground, or by the rail — never by a box.
    #[test]
    fn no_inner_surface_draws_a_border() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = appearance.iced_theme();

            for (name, style) in [
                ("raised", raised(&theme)),
                ("chip", chip(&theme)),
                ("user_bubble", user_bubble(&theme)),
                ("assistant_bubble", assistant_bubble(&theme)),
                ("toast", toast(&theme)),
                ("banner/info", banner(Severity::Info)(&theme)),
                ("banner/warning", banner(Severity::Warning)(&theme)),
                ("banner/danger", banner(Severity::Danger)(&theme)),
                ("banner/success", banner(Severity::Success)(&theme)),
            ] {
                assert_eq!(
                    style.border.width, 0.0,
                    "{appearance:?}/{name} draws a border"
                );
            }

            for status in [
                button::Status::Active,
                button::Status::Hovered,
                button::Status::Pressed,
                button::Status::Disabled,
            ] {
                for (name, style) in [
                    ("action_button", action_button(&theme, status)),
                    ("selected_button", selected_button(&theme, status)),
                    ("primary_button", primary_button(&theme, status)),
                    ("danger_button", danger_button(&theme, status)),
                ] {
                    assert_eq!(
                        style.border.width, 0.0,
                        "{appearance:?}/{name}/{status:?} draws a border"
                    );
                }
            }

            assert_eq!(
                input(&theme, text_input::Status::Active).border.width,
                0.0,
                "{appearance:?}/input draws a border"
            );
            assert_eq!(
                answer_editor(&theme, text_editor::Status::Active)
                    .border
                    .width,
                0.0,
                "{appearance:?}/answer_editor draws a border"
            );

            assert!(
                panel_surface(&theme).border.width > 0.0,
                "{appearance:?}: the panel edge is the one border that stays"
            );
        }
    }

    /// WCAG SC 1.4.11 wants 3:1 for anything that identifies a control or a
    /// state without text.
    ///
    /// With the borders gone, exactly two things do that job: the rail's active
    /// segment and the caret, both drawn in `accent`. [`Palette::border`] is
    /// explicitly *not* in this test — it is a decorative hairline now, and the
    /// 1.35:1 it measures against `ink` was only a defect while it was the sole
    /// signal identifying the input well.
    #[test]
    fn the_non_text_signals_pass_wcag_1_4_11() {
        for p in [Palette::DARK, Palette::LIGHT] {
            for background in [p.surface, p.surface_raised] {
                assert!(
                    contrast_ratio(p.accent, background) >= 3.0,
                    "the rail and caret must stay distinguishable: {:?} on {background:?} is {:.2}:1",
                    p.accent,
                    contrast_ratio(p.accent, background)
                );
                assert!(
                    contrast_ratio(p.danger, background) >= 3.0,
                    "a danger rail must stay distinguishable: {:?} on {background:?} is {:.2}:1",
                    p.danger,
                    contrast_ratio(p.danger, background)
                );
            }
        }
    }

    /// `design.md` §8 states the ratios it expects. Asserting the floors keeps
    /// the claim honest after a palette edit — the spec quoted ~14:1, ~5.2:1
    /// and ~8:1, and a spec that describes numbers no code produces is worse
    /// than no spec.
    #[test]
    fn the_documented_contrast_ratios_hold() {
        let p = Palette::DARK;
        assert!(contrast_ratio(p.text, p.surface) >= 14.0);
        assert!(contrast_ratio(p.text_dim, p.surface) >= 5.2);
        assert!(contrast_ratio(p.accent, p.surface) >= 8.0);
    }

    #[test]
    fn palette_is_recovered_from_the_runtime_theme() {
        assert_eq!(
            palette_of(&Appearance::Dark.iced_theme()).surface,
            Palette::DARK.surface
        );
        assert_eq!(
            palette_of(&Appearance::Light.iced_theme()).surface,
            Palette::LIGHT.surface
        );
    }

    /// §9 lets the panel grow for localisation and shrink on small displays, so
    /// the three widths must stay ordered no matter who edits them.
    #[test]
    fn panel_width_range_is_ordered() {
        const _: () = assert!(PANEL_WIDTH_MIN < PANEL_WIDTH_DEFAULT);
        const _: () = assert!(PANEL_WIDTH_DEFAULT < PANEL_WIDTH_MAX);
    }

    #[test]
    fn interactive_targets_meet_the_platform_floor() {
        const _: () = assert!(MIN_HIT_TARGET >= 44.0);
    }
}
