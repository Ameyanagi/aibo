//! Display-equation rendering (owner, 2026-08-02: "iced cannot render
//! LaTeX?" — it cannot, so aibo typesets it).
//!
//! iced's markdown widget is CommonMark with no math extension, and its text
//! stack shapes text, it does not typeset formulas. This module fills the
//! gap with [RaTeX](https://github.com/erweixin/RaTeX): a finished response
//! is split into markdown segments and **display math** segments
//! (`$$…$$` / `\[…\]`), and each formula is laid out and rendered to a
//! self-contained SVG (glyphs as paths — the embedded KaTeX fonts never
//! reach iced, so text glyphs would render as nothing).
//!
//! Inline math stays out of scope on purpose: iced cannot embed an image
//! inside a shaped text run, so `$x^2$` mid-sentence cannot sit on the
//! baseline. The prompts steer models toward Unicode for inline maths
//! instead, which reads well at panel sizes.
//!
//! Segmentation happens **once, at completion** — never per view and never
//! during streaming. A formula streams in as its raw TeX and snaps to the
//! rendered form when the turn finishes.

use iced::widget::{markdown, svg};

/// The panel's `text` colour (`design.md` §2, `#E6E9EF`), as RaTeX's colour.
const TEXT_COLOR: ratex_types::color::Color = ratex_types::color::Color {
    r: 0.902,
    g: 0.914,
    b: 0.937,
    a: 1.0,
};

/// Formula font size, slightly above the 15 pt body so subscripts stay
/// legible.
const MATH_FONT_SIZE: f64 = 17.0;

/// One renderable piece of a finished response.
#[derive(Debug, Clone)]
pub enum Segment {
    /// Ordinary prose, pre-parsed for the markdown widget.
    Markdown(Vec<markdown::Item>),
    /// A rendered display equation.
    Math {
        /// Self-contained SVG, glyphs as paths.
        handle: svg::Handle,
        /// Logical width in points, from the SVG's own header.
        width: f32,
        /// Logical height in points.
        height: f32,
    },
}

/// Split a finished response into markdown and rendered display math.
///
/// A formula that fails to parse or render stays in the text as the author
/// wrote it — losing content to a typesetting error is never acceptable.
pub fn segments(text: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut prose = String::new();
    for piece in split_display_math(text) {
        match piece {
            Piece::Text(t) => prose.push_str(t),
            Piece::Math(tex) => match render(tex) {
                Some(segment) => {
                    if !prose.trim().is_empty() {
                        out.push(Segment::Markdown(markdown::parse(&prose).collect()));
                    }
                    prose.clear();
                    out.push(segment);
                }
                // Unrenderable: keep the source text in place.
                None => prose.push_str(tex),
            },
        }
    }
    if !prose.trim().is_empty() {
        out.push(Segment::Markdown(markdown::parse(&prose).collect()));
    }
    out
}

/// The estimated height contribution of rendered segments beyond the plain
/// text estimate, so the window can size itself (§16's lockstep — though the
/// composer is layout-pinned, a good estimate keeps the panel from opening
/// short and scrolling immediately).
pub fn extra_height(segments: &[Segment]) -> f32 {
    segments
        .iter()
        .map(|segment| match segment {
            Segment::Markdown(_) => 0.0,
            Segment::Math { height, .. } => height + 8.0,
        })
        .sum()
}

enum Piece<'a> {
    Text(&'a str),
    Math(&'a str),
}

/// Split on `$$…$$` and `\[…\]` display-math fences. Unterminated fences are
/// text — a lone `$$` in prose must not swallow the rest of the message.
fn split_display_math(text: &str) -> Vec<Piece<'_>> {
    let mut pieces = Vec::new();
    let mut rest = text;
    loop {
        let dollars = rest.find("$$");
        let bracket = rest.find("\\[");
        let (start, open_len, close): (usize, usize, &str) = match (dollars, bracket) {
            (Some(d), Some(b)) if d <= b => (d, 2, "$$"),
            (Some(_), Some(b)) => (b, 2, "\\]"),
            (Some(d), None) => (d, 2, "$$"),
            (None, Some(b)) => (b, 2, "\\]"),
            (None, None) => break,
        };
        let after_open = &rest[start + open_len..];
        let Some(end) = after_open.find(close) else {
            break;
        };
        if start > 0 {
            pieces.push(Piece::Text(&rest[..start]));
        }
        pieces.push(Piece::Math(after_open[..end].trim()));
        rest = &after_open[end + close.len()..];
    }
    if !rest.is_empty() {
        pieces.push(Piece::Text(rest));
    }
    pieces
}

/// Typeset one formula. `None` on any failure; the caller keeps the source.
fn render(tex: &str) -> Option<Segment> {
    if tex.is_empty() {
        return None;
    }
    let ast = ratex_parser::parser::parse(tex).ok()?;
    let options = ratex_layout::LayoutOptions::default()
        .with_style(ratex_types::math_style::MathStyle::Display)
        .with_color(TEXT_COLOR);
    let laid_out = ratex_layout::layout(&ast, &options);
    let display_list = ratex_layout::to_display_list(&laid_out);
    let svg_text = ratex_svg::render_to_svg_with_color_syntax(
        &display_list,
        &ratex_svg::SvgOptions {
            font_size: MATH_FONT_SIZE,
            padding: 2.0,
            stroke_width: 1.0,
            // Paths, not text: iced's SVG renderer has no KaTeX fonts.
            embed_glyphs: true,
            font_dir: String::new(),
        },
        ratex_svg::SvgColorSyntax::Rgba,
    );
    let (width, height) = svg_dimensions(&svg_text)?;
    Some(Segment::Math {
        handle: svg::Handle::from_memory(svg_text.into_bytes()),
        width,
        height,
    })
}

/// The `width="…pt" height="…pt"` pair from the SVG header.
fn svg_dimensions(svg_text: &str) -> Option<(f32, f32)> {
    let attr = |name: &str| -> Option<f32> {
        let marker = format!("{name}=\"");
        let start = svg_text.find(&marker)? + marker.len();
        let end = svg_text[start..].find('"')? + start;
        svg_text[start..end].trim_end_matches("pt").parse().ok()
    };
    let (width, height) = (attr("width")?, attr("height")?);
    (width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0)
        .then_some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_without_math_is_one_markdown_segment() {
        let segments = segments("just **words**, and even a $ sign alone");
        assert_eq!(segments.len(), 1);
        assert!(matches!(segments[0], Segment::Markdown(_)));
    }

    /// The end-to-end pipeline: a fraction renders to a real SVG with real
    /// dimensions, prose on both sides survives.
    #[test]
    fn display_math_renders_between_markdown() {
        let segments = segments("The ratio is\n$$\\frac{a}{b^2}$$\nwhich is small.");
        assert_eq!(segments.len(), 3, "prose, math, prose");
        let Segment::Math { width, height, .. } = &segments[1] else {
            panic!("the middle segment must be rendered math");
        };
        assert!(*width > 0.0 && *height > 0.0);
        assert!(extra_height(&segments) > *height);
    }

    #[test]
    fn bracket_fences_render_too() {
        let segments = segments("\\[e^{i\\pi} + 1 = 0\\]");
        assert!(matches!(segments[0], Segment::Math { .. }));
    }

    /// A lone `$$` must not swallow the rest of the message, and a formula
    /// RaTeX cannot parse stays in the text as written.
    #[test]
    fn broken_fences_and_bad_tex_lose_nothing() {
        let broken = segments("price is $$ negotiable, honestly");
        assert_eq!(broken.len(), 1);
        assert!(matches!(broken[0], Segment::Markdown(_)));

        let bad_tex = segments("$$\\undefinedmacro{x}$$ tail");
        assert_eq!(bad_tex.len(), 1, "unrenderable math folds back to text");
    }
}
