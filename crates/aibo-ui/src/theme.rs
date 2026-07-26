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
use iced::theme::Base as _;
use iced::widget::{button, container, rule, scrollable, text, text_input};
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

/// Height of the action row — `⏎ Replace`, `⌘C Copy`, `esc Dismiss`.
///
/// **Must be added to the panel height whenever an answer is shown.**
/// [`PANEL_HEIGHT_COLLAPSED`] covers the input and its chrome only, so a panel
/// sized `COLLAPSED + ANSWER_BOX_MIN_HEIGHT` is too short: the action row
/// renders, is pushed past the window's bottom edge, and is clipped. The result
/// is an answer with no visible way to accept, copy or dismiss it — §16's
/// "every action has a key, shown" failing at the one moment the actions
/// matter.
pub const ACTION_ROW_HEIGHT: f32 = 48.0;

/// Height of one metadata line below the answer.
///
/// Two of these can appear, and **both sit between the answer box and the
/// action row**, so each one pushes the actions further toward the edge:
/// the attribution line (`codex · gpt-5.5 · 435ms · ¢0.02`) and up to two
/// footnotes (fallback substitution, context truncation). Counting the action
/// row alone still clipped it — the rows above it have to be counted too.
pub const META_LINE_HEIGHT: f32 = 22.0;

// ---------------------------------------------------------------------------
// Type (§16)
// ---------------------------------------------------------------------------

/// Type sizes. Three text weights, so three roles — not a continuum.
pub mod type_scale {
    /// Response body and input.
    pub const BODY: f32 = 15.0;
    /// Metadata: model, latency, cost, key hints.
    pub const META: f32 = 12.0;
    /// Section headings in settings and the task window.
    pub const HEADING: f32 = 17.0;
    /// The context chip excerpt.
    pub const CHIP: f32 = 12.0;
}

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

/// The closed colour set: one accent hue, two surface elevations, three text
/// weights, plus semantic red/amber. That is the whole palette (§16).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Elevation 0 — the panel body.
    pub surface: Color,
    /// Elevation 1 — chips, code blocks, the input well, list rows.
    pub surface_raised: Color,
    /// Hairline separators and the panel edge.
    pub border: Color,
    /// The single accent hue. The caret, focus rings, the active key hint.
    pub accent: Color,
    /// Accent at low alpha, for selection and hover fills.
    pub accent_muted: Color,
    /// Primary text.
    pub text: Color,
    /// Secondary text — metadata, key hints.
    pub text_dim: Color,
    /// Tertiary text — placeholders, disabled.
    pub text_faint: Color,
    /// Semantic red: destructive approvals, hard failures.
    pub danger: Color,
    /// Semantic amber: budget warnings, degraded providers, truncation.
    pub warning: Color,
    /// Semantic green, used only for a granted-permission tick.
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
    /// The dark palette. Dark-first is the product default (§16).
    pub const DARK: Self = Self {
        surface: rgb(0x12, 0x13, 0x18),
        surface_raised: rgb(0x1B, 0x1D, 0x24),
        border: rgb(0x2A, 0x2D, 0x36),
        accent: rgb(0x7C, 0x93, 0xFF),
        accent_muted: rgba(0x7C, 0x93, 0xFF, 0.16),
        text: rgb(0xEC, 0xEE, 0xF4),
        text_dim: rgb(0x9A, 0xA1, 0xB2),
        text_faint: rgb(0x66, 0x6D, 0x7E),
        danger: rgb(0xFF, 0x6B, 0x6B),
        warning: rgb(0xF2, 0xB0, 0x4A),
        success: rgb(0x5A, 0xD1, 0x9A),
    };

    /// The light palette. Same hues, inverted elevations; contrast ratios are
    /// held at AA or better against their own surface.
    pub const LIGHT: Self = Self {
        surface: rgb(0xFB, 0xFB, 0xFD),
        surface_raised: rgb(0xF0, 0xF1, 0xF5),
        border: rgb(0xD8, 0xDA, 0xE2),
        accent: rgb(0x3B, 0x54, 0xE0),
        accent_muted: rgba(0x3B, 0x54, 0xE0, 0.12),
        text: rgb(0x16, 0x18, 0x1F),
        text_dim: rgb(0x51, 0x57, 0x66),
        text_faint: rgb(0x84, 0x8A, 0x99),
        danger: rgb(0xC4, 0x2B, 0x2B),
        warning: rgb(0x9A, 0x64, 0x0C),
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

/// The panel body: elevation 0, 12 px radius, hairline border, soft shadow.
///
/// The shadow matters because the panel floats over an arbitrary desktop; the
/// border alone does not separate it from a light background.
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

/// Elevation 1: the input well, code blocks, list rows.
pub fn raised(theme: &Theme) -> container::Style {
    let p = palette_of(theme);
    container::Style {
        text_color: Some(p.text),
        background: Some(Background::Color(p.surface_raised)),
        border: Border {
            color: p.border,
            width: 1.0,
            radius: Radius::new(RADIUS_SMALL),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// The context chip (§16): a raised pill with an accent leading rule.
pub fn chip(theme: &Theme) -> container::Style {
    let p = palette_of(theme);
    container::Style {
        text_color: Some(p.text_dim),
        background: Some(Background::Color(p.surface_raised)),
        border: Border {
            color: p.border,
            width: 1.0,
            radius: Radius::new(RADIUS_SMALL),
        },
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
pub fn banner(severity: Severity) -> impl Fn(&Theme) -> container::Style {
    move |theme: &Theme| {
        let p = palette_of(theme);
        let accent = severity.color(&p);
        container::Style {
            text_color: Some(p.text),
            background: Some(Background::Color(Color { a: 0.10, ..accent })),
            border: Border {
                color: Color { a: 0.45, ..accent },
                width: 1.0,
                radius: Radius::new(RADIUS_SMALL),
            },
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// A toast: elevation 1 with a shadow, never blocking (§13).
pub fn toast(theme: &Theme) -> container::Style {
    let p = palette_of(theme);
    container::Style {
        text_color: Some(p.text),
        background: Some(Background::Color(p.surface_raised)),
        border: Border {
            color: p.border,
            width: 1.0,
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

/// Text in a severity colour.
pub fn text_severity(severity: Severity) -> impl Fn(&Theme) -> text::Style {
    move |theme: &Theme| text::Style {
        color: Some(severity.color(&palette_of(theme))),
    }
}

/// The panel input. The caret is the accent (§16).
pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let p = palette_of(theme);
    let border_color = match status {
        text_input::Status::Focused { .. } => p.accent,
        _ => p.border,
    };
    text_input::Style {
        background: Background::Color(p.surface_raised),
        border: Border {
            color: border_color,
            width: 1.0,
            radius: Radius::new(RADIUS_SMALL),
        },
        icon: p.text_faint,
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
        button::Status::Hovered => (p.accent_muted, p.text),
        button::Status::Pressed => (p.accent_muted, p.accent),
        button::Status::Disabled => (Color::TRANSPARENT, p.text_faint),
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color,
        border: Border {
            color: p.border,
            width: 1.0,
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
            a: 0.88,
            ..p.accent
        },
        button::Status::Pressed => Color {
            a: 0.76,
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
pub fn danger_button(theme: &Theme, status: button::Status) -> button::Style {
    let p = palette_of(theme);
    let tint = match status {
        button::Status::Active => 0.14,
        button::Status::Hovered => 0.24,
        button::Status::Pressed => 0.34,
        button::Status::Disabled => 0.06,
    };
    button::Style {
        background: Some(Background::Color(Color {
            a: tint,
            ..p.danger
        })),
        text_color: p.danger,
        border: Border {
            color: Color { a: 0.5, ..p.danger },
            width: 1.0,
            radius: Radius::new(RADIUS_SMALL),
        },
        shadow: Shadow::default(),
        snap: true,
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

    #[test]
    fn body_text_passes_aa_on_both_surfaces() {
        for p in [Palette::DARK, Palette::LIGHT] {
            assert!(
                contrast_ratio(p.text, p.surface) >= 4.5,
                "primary text fails AA"
            );
            assert!(
                contrast_ratio(p.text, p.surface_raised) >= 4.5,
                "primary text fails AA on elevation 1"
            );
            assert!(
                contrast_ratio(p.text_dim, p.surface) >= 4.5,
                "secondary text fails AA"
            );
        }
    }

    #[test]
    fn semantic_colours_pass_aa_large_on_their_surface() {
        // Severity colours are only ever used at >= 15 pt or as a fill tint,
        // so AA-large (3.0) is the correct bar for them.
        for p in [Palette::DARK, Palette::LIGHT] {
            for c in [p.danger, p.warning, p.success, p.accent] {
                assert!(contrast_ratio(c, p.surface) >= 3.0);
            }
        }
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
}
