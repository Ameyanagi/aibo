//! Where the panel goes: caret anchoring, display selection, clamping (§9).
//!
//! §9 calls this "cheap to specify now, expensive to retrofit". It is therefore
//! a **pure function over plain data** — [`DisplayInfo`] and [`Rect`] snapshots
//! from `aibo-platform` plus an [`ObservedGeometry`] read back from the window
//! server in, a [`Placement`] out — so every rule in §9 is unit testable without
//! a window server:
//!
//! * anchor to the caret or selection bounds when AX/UIA provides them;
//! * otherwise the display containing the focused window's centre — **never the
//!   mouse, never "the main display"**;
//! * on that fallback, horizontally centred and 28 % from the top of the
//!   *visible* frame (below the menu bar, above the taskbar);
//! * clamp fully inside one visible frame, never straddling two displays;
//! * negative coordinates are normal (displays left of or above the primary);
//! * width grows within bounds for localisation and shrinks on small displays.
//!
//! Everything here is in **logical points**. The physical conversion happens
//! once, at the winit boundary, using the scale factor of the chosen display —
//! which §9 requires be recomputed on every show, not cached from creation.
//! That is why [`Placement`] carries `scale_factor` rather than assuming one,
//! and why [`PlacementRequest::observed`] exists: the value read back from the
//! window server on *this* show beats a cached snapshot.
//!
//! # The top-left corner is always wrong
//!
//! The first build of this module answered an empty display list with
//! `position: (0.0, 0.0)`, and because nothing was feeding it a display list the
//! panel appeared pinned to the top-left corner of the primary display on every
//! show. An origin is not a placement: it is the one coordinate that is wrong on
//! every topology, and it is indistinguishable from "the code never ran".
//!
//! So [`place`] has **no** origin path. When nothing is known about the displays
//! it invents one — see [`assumed_display`] — and runs the ordinary §9 rules
//! over it. A panel that is centred on a guessed frame is recoverable; a panel
//! in the corner reads as broken.

use std::borrow::Cow;
use std::cmp::Ordering;

use aibo_core::types::{DisplayInfo, Rect};

use crate::theme::{
    PANEL_HEIGHT_COLLAPSED, PANEL_HEIGHT_MAX, PANEL_WIDTH_DEFAULT, PANEL_WIDTH_MAX, PANEL_WIDTH_MIN,
};

/// Gap between the caret and the panel edge, in logical points.
const CARET_GAP: f32 = 10.0;

/// Minimum breathing room between the panel and the edge of the visible frame.
const EDGE_MARGIN: f32 = 16.0;

/// Vertical position of the panel on the fallback path: 28 % from the top of
/// the visible frame (§9).
const FALLBACK_TOP_FRACTION: f32 = 0.28;

/// The frame assumed when neither the platform layer nor the window server has
/// said anything about the attached displays.
///
/// Deliberately *smaller* than any display aibo will realistically run on: a
/// position centred inside a small frame is still inside a large one, whereas a
/// guess that is too large puts the panel off-screen entirely. It is a guess,
/// but it is a guess that is visible, which the origin never is.
const NOMINAL_FRAME: (f64, f64) = (1024.0, 640.0);

/// Identifier of [`assumed_display`].
///
/// `u64::MAX` so it cannot collide with a real `CGDirectDisplayID` or `HMONITOR`
/// hash, and so a remembered id of this value never matches a real display and
/// therefore never survives into a show that has real geometry.
const ASSUMED_DISPLAY_ID: u64 = u64::MAX;

/// Geometry read back from the window server for *this* show (§9).
///
/// §9: "recompute scale factor on every show, not just at creation — the panel
/// moves between displays constantly and a stale factor renders blurry or
/// wrong-sized." iced exposes `window::scale_factor` and `window::monitor_size`
/// for the window's *current* monitor and nothing else, so this is a narrow
/// view: enough to keep the scale factor honest and enough to place the panel
/// sanely when the platform layer has not reported a display list at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObservedGeometry {
    /// Logical size of the monitor the panel window is currently on, if the
    /// window server answered.
    pub monitor_size: Option<(f64, f64)>,
    /// The window's scale factor, re-read on this show.
    pub scale_factor: f64,
}

/// Everything the placement rule needs, gathered at hotkey time.
///
/// All fields are optional except the display list, because §8 captures the
/// cheap things synchronously and everything else with a deadline — the panel
/// must be placeable before the AX read comes back, and again after.
#[derive(Debug, Clone, Default)]
pub struct PlacementRequest {
    /// Caret or selection bounds from AX/UIA, in display coordinates.
    ///
    /// SPIKE: S1 — whether these bounds are obtainable, correct across
    /// displays, and correct under mixed DPI is exactly what S1 measures. Until
    /// it reports, treat `Some(_)` as best-effort and keep the fallback path
    /// first-class.
    pub caret_bounds: Option<Rect>,
    /// Centre of the focused window, used when there is no caret.
    pub focused_window_centre: Option<(f64, f64)>,
    /// Identifier of the display the panel was last shown on. Used only if
    /// nothing better is known, and ignored if that display is gone (§9).
    pub remembered_display: Option<u64>,
    /// Every display currently attached. May be empty if the platform layer has
    /// not reported yet; [`place`] then falls back to [`assumed_display`]
    /// rather than to the origin.
    pub displays: Vec<DisplayInfo>,
    /// What the window server says about the window right now (§9).
    pub observed: Option<ObservedGeometry>,
    /// Width the content would like, before clamping. `None` uses the default.
    pub preferred_width: Option<f32>,
    /// Height the content currently needs; the panel height animates to this.
    pub content_height: f32,
    /// The two sizes above came from the user dragging the corner grip, not
    /// from the content.
    ///
    /// Only the display bounds then apply. [`PANEL_WIDTH_MAX`] and
    /// [`PANEL_HEIGHT_MAX`] exist to stop *content* from running away — a
    /// long answer must not grow the panel to fill the screen — and applying
    /// them to a deliberate gesture would silently refuse it, which reads as
    /// the grip being broken. Staying on one display still holds: that rule is
    /// about not losing the panel, and the user cannot drag it back from
    /// off-screen.
    pub user_sized: bool,
}

/// The resolved geometry for one `show`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Display the panel will be shown on.
    pub display_id: u64,
    /// Top-left corner in logical display coordinates. May be negative.
    pub position: (f32, f32),
    /// Logical size.
    pub size: (f32, f32),
    /// Scale factor of the chosen display, recomputed on every show (§9).
    pub scale_factor: f64,
    /// The panel is attached to a caret rather than centred. Purely
    /// informational — the "attached to what you are doing" feel is the point
    /// of §9, and telemetry on how often it is achieved is worth having.
    pub anchored: bool,
    /// The display was invented by [`assumed_display`] because nothing was
    /// known. The placement is still usable; the caller may want to re-place
    /// once real geometry arrives.
    pub assumed: bool,
}

/// The display to use when the platform layer has reported nothing.
///
/// Uses the window server's monitor size when there is one and
/// [`NOMINAL_FRAME`] when there is not. The whole bounds is treated as the
/// visible frame: no menu-bar or taskbar inset is knowable here, which is the
/// same shape an auto-hiding dock or taskbar produces anyway (§9).
fn assumed_display(observed: Option<ObservedGeometry>) -> DisplayInfo {
    let (width, height) = observed
        .and_then(|o| o.monitor_size)
        .filter(|(w, h)| *w > 0.0 && *h > 0.0)
        .unwrap_or(NOMINAL_FRAME);
    let bounds = Rect {
        x: 0.0,
        y: 0.0,
        width,
        height,
    };
    DisplayInfo {
        id: ASSUMED_DISPLAY_ID,
        bounds,
        visible_frame: bounds,
        scale_factor: observed.map_or(1.0, |o| o.scale_factor),
        is_primary: true,
    }
}

/// Whether a rectangle can be used as geometry at all.
///
/// Caret bounds cross a process boundary: they are a `CGFloat` quadruple out of
/// another application's AX tree (or a UIA `BoundingRectangle`), and S1 has not
/// yet reported that they are even obtainable, let alone sane. A `NaN` that
/// reaches [`f32::clamp`] trips its `min <= max` assertion, so an unusable
/// rectangle is discarded here and the centred fallback takes over — which is
/// the correct answer for "aibo does not know where the caret is" anyway.
fn is_usable(rect: &Rect) -> bool {
    rect.x.is_finite()
        && rect.y.is_finite()
        && rect.width.is_finite()
        && rect.height.is_finite()
        && rect.width >= 0.0
        && rect.height >= 0.0
}

/// `value` if it is a real number, `fallback` otherwise.
fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

fn contains(frame: &Rect, x: f64, y: f64) -> bool {
    x >= frame.x && x < frame.x + frame.width && y >= frame.y && y < frame.y + frame.height
}

fn centre_of(rect: &Rect) -> (f64, f64) {
    (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0)
}

/// Squared distance from a point to the nearest edge of `frame`, or 0 inside.
fn distance_sq(frame: &Rect, x: f64, y: f64) -> f64 {
    let dx = (frame.x - x).max(0.0).max(x - (frame.x + frame.width));
    let dy = (frame.y - y).max(0.0).max(y - (frame.y + frame.height));
    dx * dx + dy * dy
}

/// Pick the display a point belongs to.
///
/// A point can fall in a gap between non-contiguous displays (a common laptop +
/// external arrangement), so containment is tried first and nearest-edge second.
/// Neither ever returns "the main display" as a shortcut — §9 forbids it.
fn display_for_point(displays: &[DisplayInfo], x: f64, y: f64) -> Option<&DisplayInfo> {
    displays
        .iter()
        .find(|d| contains(&d.bounds, x, y))
        .or_else(|| {
            displays.iter().min_by(|a, b| {
                distance_sq(&a.bounds, x, y)
                    .partial_cmp(&distance_sq(&b.bounds, x, y))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        })
}

fn primary(displays: &[DisplayInfo]) -> Option<&DisplayInfo> {
    displays
        .iter()
        .find(|d| d.is_primary)
        .or_else(|| displays.first())
}

/// Choose the display and note whether a caret drove the choice.
fn choose_display<'a>(
    request: &PlacementRequest,
    caret_bounds: Option<Rect>,
    displays: &'a [DisplayInfo],
) -> Option<(&'a DisplayInfo, bool)> {
    if let Some(caret) = caret_bounds {
        let (cx, cy) = centre_of(&caret);
        if let Some(display) = display_for_point(displays, cx, cy) {
            return Some((display, true));
        }
    }
    if let Some((wx, wy)) = request.focused_window_centre
        && let Some(display) = display_for_point(displays, wx, wy)
    {
        return Some((display, false));
    }
    // §9: if the remembered display is gone, fall back to the primary.
    if let Some(id) = request.remembered_display
        && let Some(display) = displays.iter().find(|d| d.id == id)
    {
        return Some((display, false));
    }
    primary(displays).map(|d| (d, false))
}

/// The display list to actually place against.
///
/// Never empty. That is the whole contract: every rule below this point may
/// assume a frame exists, so none of them has an origin-shaped escape hatch.
fn effective_displays(request: &PlacementRequest) -> Cow<'_, [DisplayInfo]> {
    if request.displays.is_empty() {
        Cow::Owned(vec![assumed_display(request.observed)])
    } else {
        Cow::Borrowed(&request.displays)
    }
}

/// Fit one dimension inside `available`.
///
/// Keeps [`EDGE_MARGIN`] on both sides when there is room for it and otherwise
/// simply fills the frame: §9 lists "small or portrait displays" as a case to
/// handle, and a margin rule that produces a negative width handles nothing.
fn fit(desired: f32, min: f32, max: f32, available: f32) -> f32 {
    let inset = available - 2.0 * EDGE_MARGIN;
    let ceiling = if inset >= min {
        inset.min(max)
    } else {
        available.max(1.0).min(max)
    };
    desired.clamp(min.min(ceiling), ceiling)
}

/// Clamp a `size`-long span into `[origin, origin + extent]`, keeping
/// [`EDGE_MARGIN`] at both ends.
///
/// When the span cannot satisfy both margins the answer is the *centre* of the
/// frame, not one of the two edges. Picking an edge is how a panel that is one
/// point too wide ends up flush against the corner of a small display; centring
/// degrades gracefully and keeps the overflow symmetric.
fn clamp_span(value: f32, origin: f32, extent: f32, size: f32) -> f32 {
    let lo = origin + EDGE_MARGIN;
    let hi = origin + extent - size - EDGE_MARGIN;
    // `partial_cmp` rather than `lo > hi`: the two can also be *incomparable*,
    // and a `NaN` bound reaching `f32::clamp` trips its `min <= max` assertion.
    if matches!(lo.partial_cmp(&hi), Some(Ordering::Less | Ordering::Equal)) {
        value.clamp(lo, hi)
    } else {
        origin + (extent - size) / 2.0
    }
}

/// Resolve the panel geometry for one `show` (§9).
///
/// Never fails, never panics, and **never returns the origin as a placement**:
/// an empty display list is answered with [`assumed_display`] rather than
/// `(0, 0)`.
pub fn place(request: &PlacementRequest) -> Placement {
    let displays = effective_displays(request);
    let desired_width = request.preferred_width.unwrap_or(PANEL_WIDTH_DEFAULT);
    let caret_bounds = request.caret_bounds.filter(is_usable);

    let (display, anchored) = choose_display(request, caret_bounds, &displays)
        .expect("effective_displays is never empty");

    // A display whose visible frame is unusable degrades to its bounds and then
    // to the assumed frame, rather than poisoning every coordinate below.
    let frame = if is_usable(&display.visible_frame) {
        display.visible_frame
    } else if is_usable(&display.bounds) {
        display.bounds
    } else {
        assumed_display(request.observed).visible_frame
    };
    let (fx, fy) = (frame.x as f32, frame.y as f32);
    let (fw, fh) = (frame.width as f32, frame.height as f32);

    // Width grows within bounds for localisation, shrinks on small displays (§9).
    // A hand-set size answers to the display and nothing else; see
    // `PlacementRequest::user_sized`.
    let (width_max, height_max) = if request.user_sized {
        (fw, fh)
    } else {
        (PANEL_WIDTH_MAX, PANEL_HEIGHT_MAX)
    };
    let width = fit(
        finite_or(desired_width, PANEL_WIDTH_DEFAULT),
        PANEL_WIDTH_MIN,
        width_max,
        fw,
    );
    let height = fit(
        finite_or(request.content_height, PANEL_HEIGHT_COLLAPSED),
        PANEL_HEIGHT_COLLAPSED,
        height_max,
        fh,
    );

    let (x, y) = if anchored {
        let caret = caret_bounds.expect("anchored implies usable caret bounds");
        let below = (caret.y + caret.height) as f32 + CARET_GAP;
        let above = caret.y as f32 - CARET_GAP - height;
        // Prefer below the caret; flip above only if below would be clipped and
        // above genuinely fits. Otherwise clamping below is the lesser evil —
        // it keeps the panel on the same side the user is looking.
        let y = if below + height <= fy + fh - EDGE_MARGIN {
            below
        } else if above >= fy + EDGE_MARGIN {
            above
        } else {
            below
        };
        (caret.x as f32, y)
    } else {
        (fx + (fw - width) / 2.0, fy + fh * FALLBACK_TOP_FRACTION)
    };

    // Clamp fully inside this one visible frame. Never straddle two displays (§9).
    let x = clamp_span(x, fx, fw, width);
    let y = clamp_span(y, fy, fh, height);

    // §9: recompute the scale factor on every show. With a single display there
    // is no ambiguity about which monitor the window is on, so the value read
    // back from the window server beats a snapshot that may predate a
    // resolution change. With several, only the per-display snapshot knows
    // which factor belongs to the display the panel is moving *to*.
    let scale_factor = match (displays.len(), request.observed) {
        (1, Some(observed)) => observed.scale_factor,
        _ => display.scale_factor,
    };
    // A `NaN` here would be worse than wrong: `Placement`'s `PartialEq` is what
    // stops the re-place loop in `app`, and `NaN != NaN` would make every probe
    // look like a change and re-show the panel forever.
    let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };

    Placement {
        display_id: display.id,
        position: (x, y),
        size: (width, height),
        scale_factor,
        anchored,
        assumed: display.id == ASSUMED_DISPLAY_ID && request.displays.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn display(id: u64, bounds: Rect, is_primary: bool) -> DisplayInfo {
        DisplayInfo {
            id,
            bounds,
            // Menu bar / taskbar inset, so the two frames are never identical.
            visible_frame: rect(
                bounds.x,
                bounds.y + 25.0,
                bounds.width,
                bounds.height - 25.0,
            ),
            scale_factor: 2.0,
            is_primary,
        }
    }

    /// A display whose dock or taskbar auto-hides: no inset at all (§9).
    fn undecorated(id: u64, bounds: Rect, is_primary: bool) -> DisplayInfo {
        DisplayInfo {
            id,
            bounds,
            visible_frame: bounds,
            scale_factor: 1.0,
            is_primary,
        }
    }

    fn primary_only() -> Vec<DisplayInfo> {
        vec![display(1, rect(0.0, 0.0, 1920.0, 1080.0), true)]
    }

    /// A laptop with an external display placed to its left and above it —
    /// negative coordinates, which §9 names explicitly.
    fn negative_arrangement() -> Vec<DisplayInfo> {
        vec![
            display(1, rect(0.0, 0.0, 1440.0, 900.0), true),
            display(2, rect(-2560.0, -400.0, 2560.0, 1440.0), false),
        ]
    }

    /// A tall, narrow secondary display, and a genuinely small primary.
    fn portrait_arrangement() -> Vec<DisplayInfo> {
        vec![
            undecorated(7, rect(0.0, 0.0, 1280.0, 800.0), true),
            display(8, rect(1280.0, -600.0, 1080.0, 1920.0), false),
        ]
    }

    /// Smaller than the panel's minimum width, so every margin rule inverts.
    fn tiny_display() -> Vec<DisplayInfo> {
        vec![undecorated(11, rect(-300.0, -200.0, 320.0, 240.0), true)]
    }

    /// The visible frame the placement claims to be inside.
    fn frame_of(request: &PlacementRequest, id: u64) -> Rect {
        request
            .displays
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.visible_frame)
            .unwrap_or_else(|| assumed_display(request.observed).visible_frame)
    }

    /// Every §9 rule that must hold for *every* input, asserted in one place.
    ///
    /// This is the guard the original bug walked straight past: the top-left
    /// corner was not an edge case of the rules, it was a code path that skipped
    /// them entirely.
    #[track_caller]
    fn assert_section_9_invariants(request: &PlacementRequest) -> Placement {
        let p = place(request);
        let frame = frame_of(request, p.display_id);
        let (fx, fy) = (frame.x as f32, frame.y as f32);
        let (fw, fh) = (frame.width as f32, frame.height as f32);
        const EPS: f32 = 0.01;

        assert!(
            p.size.0 > 0.0 && p.size.1 > 0.0,
            "a panel with no area is not a panel: {p:?}"
        );
        // Never wider or taller than the frame it is on.
        assert!(p.size.0 <= fw + EPS, "wider than the frame: {p:?}");
        assert!(p.size.1 <= fh + EPS, "taller than the frame: {p:?}");

        // Clamped fully inside one visible frame; never straddling two (§9).
        assert!(p.position.0 + EPS >= fx, "off the left edge: {p:?}");
        assert!(p.position.1 + EPS >= fy, "off the top edge: {p:?}");
        assert!(
            p.position.0 + p.size.0 <= fx + fw + EPS,
            "off the right edge: {p:?}"
        );
        assert!(
            p.position.1 + p.size.1 <= fy + fh + EPS,
            "off the bottom edge: {p:?}"
        );

        // The margin is kept whenever the frame is big enough to allow it.
        if fw >= p.size.0 + 2.0 * EDGE_MARGIN {
            assert!(
                p.position.0 + EPS >= fx + EDGE_MARGIN,
                "no left margin: {p:?}"
            );
            assert!(
                p.position.0 + p.size.0 <= fx + fw - EDGE_MARGIN + EPS,
                "no right margin: {p:?}"
            );
        }
        if fh >= p.size.1 + 2.0 * EDGE_MARGIN {
            assert!(
                p.position.1 + EPS >= fy + EDGE_MARGIN,
                "no top margin: {p:?}"
            );
            assert!(
                p.position.1 + p.size.1 <= fy + fh - EDGE_MARGIN + EPS,
                "no bottom margin: {p:?}"
            );
        }

        // The regression itself. Every frame used in these tests has its
        // top-left strictly outside the margin band around the origin, so the
        // origin can only be produced by a code path that skipped the rules.
        assert_ne!(
            p.position,
            (0.0, 0.0),
            "§9: the top-left corner of the coordinate space is never a placement"
        );
        p
    }

    fn request(displays: Vec<DisplayInfo>, caret: Option<Rect>) -> PlacementRequest {
        PlacementRequest {
            caret_bounds: caret,
            displays,
            content_height: 200.0,
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // The regression: the panel appeared in the top-left corner of the display
    // -----------------------------------------------------------------------

    /// Observed on screen: with nothing feeding the display list, `place`
    /// returned `(0.0, 0.0)` and the panel sat in the corner of the primary
    /// display on every show.
    #[test]
    fn an_unknown_topology_never_places_the_panel_in_the_corner() {
        let p = place(&PlacementRequest::default());
        assert_ne!(p.position, (0.0, 0.0), "the original bug, exactly");
        assert!(p.assumed, "the caller must be able to tell it was a guess");

        // It is placed by the ordinary §9 rules over the assumed frame: centred
        // horizontally, 28 % down.
        let (fw, fh) = NOMINAL_FRAME;
        assert!((p.position.0 - (fw as f32 - p.size.0) / 2.0).abs() < 0.5);
        assert!((p.position.1 - fh as f32 * FALLBACK_TOP_FRACTION).abs() < 0.5);
        assert!(p.position.0 > EDGE_MARGIN && p.position.1 > EDGE_MARGIN);
        assert!(p.position.0 + p.size.0 <= fw as f32);
        assert!(p.position.1 + p.size.1 <= fh as f32);
    }

    /// The same, but the window server answered even though the platform layer
    /// did not: the guess is then the real monitor.
    #[test]
    fn an_unknown_topology_uses_the_window_servers_monitor_size() {
        let p = place(&PlacementRequest {
            observed: Some(ObservedGeometry {
                monitor_size: Some((3440.0, 1440.0)),
                scale_factor: 1.0,
            }),
            content_height: 200.0,
            ..Default::default()
        });
        assert_ne!(p.position, (0.0, 0.0));
        assert!((p.position.0 - (3440.0 - p.size.0) / 2.0).abs() < 0.5);
        assert!((p.position.1 - 1440.0 * FALLBACK_TOP_FRACTION).abs() < 0.5);
    }

    /// The corner must be unreachable across the whole input space, not just on
    /// the arrangements someone thought to write a test for.
    #[test]
    fn no_topology_and_no_caret_position_can_produce_the_corner() {
        let topologies = [
            Vec::new(),
            primary_only(),
            negative_arrangement(),
            portrait_arrangement(),
            tiny_display(),
        ];
        // Corners, edge midpoints and centres of every frame in play, plus a
        // few points in the gaps between displays and far outside them all.
        let carets = [
            None,
            Some(rect(0.0, 0.0, 2.0, 18.0)),
            Some(rect(-2560.0, -400.0, 2.0, 18.0)),
            Some(rect(-1.0, -1.0, 2.0, 18.0)),
            Some(rect(1919.0, 1079.0, 2.0, 18.0)),
            Some(rect(1439.0, 899.0, 2.0, 18.0)),
            Some(rect(1280.0, -600.0, 2.0, 18.0)),
            Some(rect(2359.0, 1319.0, 2.0, 18.0)),
            Some(rect(-300.0, -200.0, 2.0, 18.0)),
            Some(rect(19.0, 39.0, 2.0, 18.0)),
            Some(rect(100_000.0, 100_000.0, 2.0, 18.0)),
            Some(rect(-100_000.0, -100_000.0, 2.0, 18.0)),
        ];
        let heights = [0.0, 200.0, 10_000.0];

        for topology in &topologies {
            for caret in &carets {
                for height in heights {
                    let request = PlacementRequest {
                        content_height: height,
                        ..request(topology.clone(), *caret)
                    };
                    assert_section_9_invariants(&request);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // The §9 rules themselves
    // -----------------------------------------------------------------------

    #[test]
    fn falls_back_to_28_percent_from_the_top_and_centres_horizontally() {
        let request = request(primary_only(), None);
        let p = assert_section_9_invariants(&request);
        assert!(!p.anchored);
        let frame = primary_only()[0].visible_frame;
        let expected_x = frame.x as f32 + (frame.width as f32 - p.size.0) / 2.0;
        let expected_y = frame.y as f32 + frame.height as f32 * FALLBACK_TOP_FRACTION;
        assert!((p.position.0 - expected_x).abs() < 0.5);
        assert!((p.position.1 - expected_y).abs() < 0.5);
    }

    /// The fallback is measured against the *visible* frame, not the bounds —
    /// otherwise the 28 % sits under the menu bar on every macOS display.
    #[test]
    fn the_fallback_measures_from_the_visible_frame_not_the_bounds() {
        let displays = primary_only();
        let p = assert_section_9_invariants(&request(displays.clone(), None));
        let bounds = displays[0].bounds;
        let naive = bounds.y as f32 + bounds.height as f32 * FALLBACK_TOP_FRACTION;
        assert!(
            (p.position.1 - naive).abs() > 1.0,
            "the menu-bar inset must be part of the calculation"
        );
    }

    #[test]
    fn anchors_below_the_caret_when_bounds_are_available() {
        let request = request(primary_only(), Some(rect(400.0, 300.0, 2.0, 18.0)));
        let p = assert_section_9_invariants(&request);
        assert!(p.anchored);
        assert_eq!(p.position.0, 400.0);
        assert_eq!(p.position.1, 300.0 + 18.0 + CARET_GAP);
    }

    #[test]
    fn flips_above_the_caret_when_below_would_be_clipped() {
        let request = PlacementRequest {
            content_height: 300.0,
            ..request(primary_only(), Some(rect(400.0, 1000.0, 2.0, 18.0)))
        };
        let p = assert_section_9_invariants(&request);
        assert!(p.position.1 < 1000.0, "should have flipped above the caret");
    }

    /// A caret at each of the four corners of the visible frame. None of them
    /// may push the panel outside it, and none of them may produce the origin.
    #[test]
    fn a_caret_at_every_screen_edge_stays_inside_the_frame() {
        let displays = primary_only();
        let frame = displays[0].visible_frame;
        let (l, t) = (frame.x, frame.y);
        let (r, b) = (frame.x + frame.width, frame.y + frame.height);
        for (x, y) in [
            (l, t),
            (r - 2.0, t),
            (l, b - 18.0),
            (r - 2.0, b - 18.0),
            ((l + r) / 2.0, t),
            ((l + r) / 2.0, b - 18.0),
        ] {
            let request = request(displays.clone(), Some(rect(x, y, 2.0, 18.0)));
            let p = assert_section_9_invariants(&request);
            assert!(p.anchored, "the caret is on the display: {p:?}");
        }
    }

    #[test]
    fn never_straddles_two_displays() {
        // Caret near the right edge of the primary; the panel must stay on it.
        let displays = vec![
            display(1, rect(0.0, 0.0, 1440.0, 900.0), true),
            display(2, rect(1440.0, 0.0, 1920.0, 1080.0), false),
        ];
        let request = request(displays.clone(), Some(rect(1400.0, 400.0, 2.0, 18.0)));
        let p = assert_section_9_invariants(&request);
        assert_eq!(p.display_id, 1);
        let frame = displays[0].visible_frame;
        assert!(p.position.0 >= frame.x as f32);
        assert!(p.position.0 + p.size.0 <= (frame.x + frame.width) as f32);
    }

    #[test]
    fn handles_displays_at_negative_coordinates() {
        let displays = negative_arrangement();
        let request = request(displays.clone(), Some(rect(-1200.0, -100.0, 2.0, 18.0)));
        let p = assert_section_9_invariants(&request);
        assert_eq!(p.display_id, 2);
        assert!(p.position.0 < 0.0);
        assert!(p.position.1 < 0.0, "the display starts above the primary");
    }

    /// A display entirely above and left of the primary, with no caret at all:
    /// the remembered display is the only thing pointing at it.
    #[test]
    fn a_remembered_display_at_negative_coordinates_is_honoured() {
        let request = PlacementRequest {
            remembered_display: Some(2),
            ..request(negative_arrangement(), None)
        };
        let p = assert_section_9_invariants(&request);
        assert_eq!(p.display_id, 2);
        // Horizontally centred on a display whose whole width is negative; 28 %
        // down a frame that starts above the primary, which lands just past
        // `y == 0` — the arithmetic has to work in both signs.
        assert!(p.position.0 < 0.0, "{p:?}");
        assert!(p.position.1 < 1040.0, "{p:?}");
    }

    #[test]
    fn shrinks_on_a_small_or_portrait_display() {
        let displays = vec![display(9, rect(0.0, 0.0, 480.0, 1024.0), true)];
        let p = assert_section_9_invariants(&request(displays, None));
        assert!(p.size.0 < PANEL_WIDTH_DEFAULT);
        assert!(p.size.0 <= 480.0 - 2.0 * EDGE_MARGIN);
    }

    /// A portrait secondary at negative `y`, with the caret on it.
    #[test]
    fn a_portrait_display_gets_a_narrow_panel_28_percent_down() {
        let displays = portrait_arrangement();
        let request = request(displays.clone(), Some(rect(1500.0, 200.0, 2.0, 18.0)));
        let p = assert_section_9_invariants(&request);
        assert_eq!(p.display_id, 8);
        assert!(p.size.0 <= 1080.0 - 2.0 * EDGE_MARGIN);
        assert!(p.size.1 <= PANEL_HEIGHT_MAX);
    }

    /// Narrower than [`PANEL_WIDTH_MIN`]: the margins cannot be honoured, so the
    /// panel fills the frame and is centred rather than shoved into a corner.
    #[test]
    fn a_display_smaller_than_the_minimum_panel_still_gets_a_centred_panel() {
        let displays = tiny_display();
        let p = assert_section_9_invariants(&request(displays.clone(), None));
        let frame = displays[0].visible_frame;
        assert!(p.size.0 <= frame.width as f32);
        assert!(p.size.1 <= frame.height as f32);
        assert!((p.position.0 - frame.x as f32).abs() < 1.0, "{p:?}");
    }

    /// An auto-hiding dock or taskbar means `visible_frame == bounds`; the
    /// margin still has to be kept, or the panel sits flush against the edge.
    #[test]
    fn an_auto_hiding_dock_still_leaves_a_margin() {
        let displays = vec![undecorated(3, rect(0.0, 0.0, 1440.0, 900.0), true)];
        let p = assert_section_9_invariants(&request(displays, None));
        assert!(p.position.1 >= EDGE_MARGIN);
        assert!(p.position.1 + p.size.1 <= 900.0 - EDGE_MARGIN);
    }

    #[test]
    fn grows_within_bounds_for_a_longer_localisation() {
        let request = PlacementRequest {
            preferred_width: Some(2000.0),
            ..request(primary_only(), None)
        };
        assert_eq!(
            assert_section_9_invariants(&request).size.0,
            PANEL_WIDTH_MAX
        );
    }

    /// The content ceilings are there to stop an answer from growing the panel
    /// to fill the screen. A drag is not an answer.
    #[test]
    fn a_hand_set_size_is_bounded_by_the_display_and_not_by_the_content_ceiling() {
        let request = PlacementRequest {
            preferred_width: Some(PANEL_WIDTH_MAX + 200.0),
            content_height: PANEL_HEIGHT_MAX + 200.0,
            user_sized: true,
            ..request(primary_only(), None)
        };
        let p = assert_section_9_invariants(&request);
        assert!(
            p.size.0 > PANEL_WIDTH_MAX,
            "a hand-set width must pass the localisation ceiling, got {}",
            p.size.0
        );
        assert!(
            p.size.1 > PANEL_HEIGHT_MAX,
            "a hand-set height must pass the content ceiling, got {}",
            p.size.1
        );
    }

    /// A size dragged on a large display, restored on a small one.
    #[test]
    fn a_hand_set_size_still_fits_on_the_display_it_is_restored_to() {
        let displays = vec![display(1, rect(0.0, 0.0, 1280.0, 800.0), true)];
        let request = PlacementRequest {
            preferred_width: Some(2400.0),
            content_height: 1600.0,
            user_sized: true,
            ..request(displays, None)
        };
        let p = assert_section_9_invariants(&request);
        assert!(p.size.0 <= 1280.0 && p.size.1 <= 800.0, "got {:?}", p.size);
    }

    #[test]
    fn forgets_a_display_that_is_gone() {
        let request = PlacementRequest {
            remembered_display: Some(404),
            ..request(primary_only(), None)
        };
        assert_eq!(assert_section_9_invariants(&request).display_id, 1);
    }

    /// §9's disconnect case: the remembered display is gone *and* the one that
    /// remains is not the primary flag holder either.
    #[test]
    fn a_disconnected_remembered_display_falls_back_to_the_primary() {
        let displays = vec![
            display(1, rect(0.0, 0.0, 1440.0, 900.0), true),
            display(5, rect(1440.0, 0.0, 1920.0, 1080.0), false),
        ];
        let request = PlacementRequest {
            remembered_display: Some(2),
            ..request(displays, None)
        };
        assert_eq!(assert_section_9_invariants(&request).display_id, 1);
    }

    // -----------------------------------------------------------------------
    // Scale factor (§9: recompute on every show)
    // -----------------------------------------------------------------------

    #[test]
    fn scale_factor_comes_from_the_chosen_display() {
        let mut displays = negative_arrangement();
        displays[1].scale_factor = 1.0;
        let request = request(displays, Some(rect(-1200.0, -100.0, 2.0, 18.0)));
        assert_eq!(assert_section_9_invariants(&request).scale_factor, 1.0);
    }

    /// With one display there is no ambiguity, so the freshly read factor wins
    /// over the snapshot — that is what "recompute on every show" buys.
    #[test]
    fn a_freshly_read_scale_factor_beats_a_stale_snapshot() {
        let request = PlacementRequest {
            observed: Some(ObservedGeometry {
                monitor_size: Some((1920.0, 1080.0)),
                scale_factor: 1.0,
            }),
            ..request(primary_only(), None)
        };
        assert_eq!(primary_only()[0].scale_factor, 2.0, "the stale snapshot");
        assert_eq!(assert_section_9_invariants(&request).scale_factor, 1.0);
    }

    #[test]
    fn an_assumed_display_carries_the_observed_scale_factor() {
        let p = place(&PlacementRequest {
            observed: Some(ObservedGeometry {
                monitor_size: None,
                scale_factor: 2.0,
            }),
            ..Default::default()
        });
        assert!(p.assumed);
        assert_eq!(p.scale_factor, 2.0);
    }

    /// A degenerate answer from the window server must not become a degenerate
    /// frame; `NOMINAL_FRAME` takes over.
    #[test]
    fn a_zero_sized_monitor_report_is_ignored() {
        let p = place(&PlacementRequest {
            observed: Some(ObservedGeometry {
                monitor_size: Some((0.0, 0.0)),
                scale_factor: 1.0,
            }),
            ..Default::default()
        });
        assert_ne!(p.position, (0.0, 0.0));
        assert!(p.size.0 > 0.0 && p.size.1 > 0.0);
    }

    // -----------------------------------------------------------------------
    // Untrusted geometry
    // -----------------------------------------------------------------------

    /// Caret bounds come out of another process's AX tree. Garbage must fall
    /// back to the centred placement, not panic inside `f32::clamp` and not
    /// reach the window server as a `NaN` position.
    #[test]
    fn unusable_caret_bounds_fall_back_to_the_centred_placement() {
        for caret in [
            rect(f64::NAN, 300.0, 2.0, 18.0),
            rect(400.0, f64::NAN, 2.0, 18.0),
            rect(400.0, 300.0, f64::NAN, 18.0),
            rect(400.0, 300.0, 2.0, f64::INFINITY),
            rect(400.0, 300.0, -2.0, 18.0),
        ] {
            let p = assert_section_9_invariants(&request(primary_only(), Some(caret)));
            assert!(!p.anchored, "garbage must not anchor: {caret:?}");
            assert!(p.position.0.is_finite() && p.position.1.is_finite());
        }
    }

    /// The same for a display the platform layer described badly.
    #[test]
    fn an_unusable_visible_frame_degrades_to_the_bounds() {
        let mut displays = primary_only();
        displays[0].visible_frame = rect(f64::NAN, f64::NAN, f64::NAN, f64::NAN);
        let p = place(&request(displays, None));
        assert!(
            p.position.0.is_finite() && p.position.1.is_finite(),
            "{p:?}"
        );
        assert!(p.position.0 > 0.0 && p.position.1 > 0.0, "{p:?}");
        assert!(p.position.0 + p.size.0 <= 1920.0);
    }

    /// `app` stops its re-place loop by comparing the new [`Placement`] with the
    /// last one, so `place` must be a *function*: same input, equal output. A
    /// single `NaN` anywhere in the result would make every comparison unequal
    /// and re-show the panel on every frame.
    #[test]
    fn a_placement_always_equals_itself() {
        let degenerate = [
            PlacementRequest::default(),
            PlacementRequest {
                content_height: f32::NAN,
                preferred_width: Some(f32::NAN),
                ..request(primary_only(), Some(rect(f64::NAN, 0.0, 2.0, 18.0)))
            },
            PlacementRequest {
                observed: Some(ObservedGeometry {
                    monitor_size: Some((f64::NAN, f64::NAN)),
                    scale_factor: f64::NAN,
                }),
                ..Default::default()
            },
            PlacementRequest {
                displays: vec![undecorated(
                    1,
                    rect(f64::NAN, f64::NAN, f64::NAN, f64::NAN),
                    true,
                )],
                ..Default::default()
            },
        ];
        for request in degenerate {
            let p = place(&request);
            assert_eq!(p, place(&request), "{request:?} produced {p:?}");
            assert!(
                p.position.0.is_finite() && p.position.1.is_finite(),
                "{p:?}"
            );
            assert!(p.size.0.is_finite() && p.size.1.is_finite(), "{p:?}");
            assert!(p.scale_factor.is_finite() && p.scale_factor > 0.0, "{p:?}");
        }
    }

    /// Once a real display list arrives, the assumed display is gone — its id
    /// must never survive as a "remembered" choice.
    #[test]
    fn the_assumed_display_is_never_remembered() {
        let assumed = place(&PlacementRequest::default());
        assert_eq!(assumed.display_id, ASSUMED_DISPLAY_ID);
        let request = PlacementRequest {
            remembered_display: Some(assumed.display_id),
            ..request(primary_only(), None)
        };
        let p = assert_section_9_invariants(&request);
        assert_eq!(p.display_id, 1);
        assert!(!p.assumed);
    }
}
