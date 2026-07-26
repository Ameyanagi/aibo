//! Display enumeration and the frontmost-window lookup (§9).
//!
//! Two rules from §9 shape this module:
//!
//! * "Fall back to the display containing the focused window's centre; **never
//!   the mouse**, never the 'main' display."
//! * "Recompute scale factor on every show, not just at creation."
//!
//! Everything here works in CoreGraphics' global coordinate space, whose origin
//! is the **top-left** of the primary display and whose y grows downwards —
//! matching [`aibo_core::types::Rect`], where `y` is the top edge. `NSScreen`
//! uses a bottom-left origin, so the one place this module touches AppKit
//! ([`visible_frame_for`]) flips explicitly.

use aibo_core::types::{DisplayInfo, Rect};
use core_foundation::base::{CFTypeRef, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_graphics::display::{CGDisplay, CGRect};
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowBounds, kCGWindowLayer,
    kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly, kCGWindowNumber,
    kCGWindowOwnerPID,
};
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_foundation::{NSNumber, NSString};

// `CGRectMakeWithDictionaryRepresentation` is the documented decoder for the
// `kCGWindowBounds` dictionary. `core-graphics` 0.24 does not bind it.
#[allow(unsafe_code)]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect) -> bool;
}

fn to_rect(r: CGRect) -> Rect {
    Rect {
        x: r.origin.x,
        y: r.origin.y,
        width: r.size.width,
        height: r.size.height,
    }
}

fn contains(outer: &Rect, x: f64, y: f64) -> bool {
    x >= outer.x && x < outer.x + outer.width && y >= outer.y && y < outer.y + outer.height
}

/// A window as reported by the window server.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowSummary {
    /// `CGWindowID`, stored in [`aibo_core::types::AppRef::window`].
    pub id: u64,
    /// Bounds in the CoreGraphics global (top-left origin) space.
    pub bounds: Rect,
}

/// The frontmost normal window belonging to `pid`.
///
/// `CGWindowListCopyWindowInfo` answers from the window server's own state, so
/// it cannot block on a hung application — unlike `kAXFocusedWindowAttribute`,
/// which is why §8 step 1 can take a window id synchronously. The on-screen
/// list is ordered front-to-back, so the first layer-0 match is the one the
/// user is looking at.
#[allow(unsafe_code)]
pub(crate) fn frontmost_window_for_pid(pid: i32) -> Option<WindowSummary> {
    let options = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
    let infos = copy_window_info(options, kCGNullWindowID)?;

    for raw in infos.iter() {
        let ptr = *raw as CFDictionaryRef;
        if ptr.is_null() {
            continue;
        }
        // SAFETY: the array is a live CFArray of CFDictionary; `wrap_under_get_rule`
        // takes its own reference and the array outlives this borrow.
        let dict: CFDictionary<CFString, CFTypeRef> =
            unsafe { CFDictionary::wrap_under_get_rule(ptr) };

        // SAFETY: the `kCGWindow*` statics are framework-owned constants valid
        // for the process lifetime.
        let (key_pid, key_layer, key_number, key_bounds) = unsafe {
            (
                CFString::wrap_under_get_rule(kCGWindowOwnerPID),
                CFString::wrap_under_get_rule(kCGWindowLayer),
                CFString::wrap_under_get_rule(kCGWindowNumber),
                CFString::wrap_under_get_rule(kCGWindowBounds),
            )
        };

        if number_value(&dict, &key_pid) != Some(i64::from(pid)) {
            continue;
        }
        // Layer 0 is the ordinary window layer; menus, tooltips and the dock
        // live above it and must not be mistaken for the user's window.
        if number_value(&dict, &key_layer) != Some(0) {
            continue;
        }
        let id = number_value(&dict, &key_number)? as u64;

        let bounds_ptr = *dict.find(&key_bounds)? as CFDictionaryRef;
        let mut rect = CGRect::new(
            &core_graphics::geometry::CGPoint::new(0.0, 0.0),
            &core_graphics::geometry::CGSize::new(0.0, 0.0),
        );
        // SAFETY: `bounds_ptr` is a live CFDictionary borrowed from `dict`;
        // `rect` is a valid out-pointer.
        let ok = unsafe { CGRectMakeWithDictionaryRepresentation(bounds_ptr, &raw mut rect) };
        if !ok {
            continue;
        }
        return Some(WindowSummary {
            id,
            bounds: to_rect(rect),
        });
    }
    None
}

#[allow(unsafe_code)]
fn number_value(dict: &CFDictionary<CFString, CFTypeRef>, key: &CFString) -> Option<i64> {
    let value = *dict.find(key)?;
    if value.is_null() {
        return None;
    }
    // SAFETY: the value is a live CF object borrowed from `dict`.
    let number = unsafe { CFNumber::wrap_under_get_rule(value.cast()) };
    number.to_i64()
}

/// Build a [`DisplayInfo`] for one `CGDirectDisplayID`.
fn display_info(id: u32) -> DisplayInfo {
    let display = CGDisplay::new(id);
    let bounds = to_rect(display.bounds());
    // Backing scale is `pixels / points`, recomputed on every call because §9
    // requires it fresh on every show rather than cached at creation.
    let scale_factor = if bounds.width > 0.0 {
        display.pixels_wide() as f64 / bounds.width
    } else {
        1.0
    };
    DisplayInfo {
        id: u64::from(id),
        visible_frame: visible_frame_for(id).unwrap_or(bounds),
        bounds,
        scale_factor,
        is_primary: display.is_main(),
    }
}

/// `NSScreen.visibleFrame` for a display id, converted to the CoreGraphics
/// top-left space.
///
/// `NSScreen` is main-thread-only in `objc2` 0.3, and the §8 capture thread is
/// not the main thread. When called from anywhere else this returns `None` and
/// the caller falls back to the full bounds, which over-estimates the usable
/// area by the height of the menu bar. Callers on the UI thread — which is
/// where §9's "recompute on every show" placement runs — get the exact value.
fn visible_frame_for(id: u32) -> Option<Rect> {
    let mtm = MainThreadMarker::new()?;
    let screens = NSScreen::screens(mtm);
    // The primary screen is the one whose AppKit frame origin is (0, 0); its
    // height defines the flip.
    let primary_height = screens
        .iter()
        .find(|s| s.frame().origin.x == 0.0 && s.frame().origin.y == 0.0)
        .map(|s| s.frame().size.height)?;

    let key = NSString::from_str("NSScreenNumber");
    for screen in screens.iter() {
        let Some(value) = screen.deviceDescription().objectForKey(&key) else {
            continue;
        };
        let Ok(number) = value.downcast::<NSNumber>() else {
            continue;
        };
        if number.as_u32() != id {
            continue;
        }
        let vf = screen.visibleFrame();
        return Some(Rect {
            x: vf.origin.x,
            // AppKit y grows upwards from the bottom of the primary screen.
            y: primary_height - (vf.origin.y + vf.size.height),
            width: vf.size.width,
            height: vf.size.height,
        });
    }
    None
}

/// Every active display, primary first.
pub(crate) fn all_displays() -> Vec<DisplayInfo> {
    let mut displays: Vec<DisplayInfo> = CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(display_info)
        .collect();
    displays.sort_by_key(|d| !d.is_primary);
    displays
}

/// The display the panel should open on (§9).
///
/// Resolution order: the display containing the centre of the focused window,
/// then the primary display. Never the display under the mouse.
pub(crate) fn active_display(focused_window: Option<WindowSummary>) -> DisplayInfo {
    let displays = all_displays();
    if let Some(window) = focused_window {
        let cx = window.bounds.x + window.bounds.width / 2.0;
        let cy = window.bounds.y + window.bounds.height / 2.0;
        if let Some(hit) = displays.iter().find(|d| contains(&d.bounds, cx, cy)) {
            return hit.clone();
        }
    }
    displays
        .into_iter()
        .find(|d| d.is_primary)
        .unwrap_or_else(|| display_info(CGDisplay::main().id))
}
