//! AppKit implementation of the transient panel-window boundary.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::{
    NSAccessibilityAnnouncementKey, NSAccessibilityAnnouncementRequestedNotification,
    NSAccessibilityPostNotificationWithUserInfo, NSApplication, NSAutoresizingMaskOptions, NSColor,
    NSFloatingWindowLevel, NSScreen, NSUserInterfaceItemIdentification, NSView,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindowCollectionBehavior, NSWindowOrderingMode, NSWindowStyleMask, NSWindowTitleVisibility,
    NSWorkspace,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};
use raw_window_handle::AppKitWindowHandle;

use crate::overlay::{BackdropStatus, OverlayWindowConfiguration, OverlayWindowError};

const EFFECT_IDENTIFIER: &str = "com.aibo.panel.backdrop";

/// Bits to add to a borderless winit window without turning it into an
/// `NSPanel` (which it is not).
fn overlay_style_mask(mut current: NSWindowStyleMask) -> NSWindowStyleMask {
    current.insert(
        NSWindowStyleMask::UtilityWindow
            | NSWindowStyleMask::Titled
            | NSWindowStyleMask::FullSizeContentView,
    );
    current
}

/// Resolve mutually-exclusive collection groups before adding overlay policy.
fn overlay_collection_behavior(
    mut current: NSWindowCollectionBehavior,
) -> NSWindowCollectionBehavior {
    current.remove(
        NSWindowCollectionBehavior::MoveToActiveSpace
            | NSWindowCollectionBehavior::Managed
            | NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::ParticipatesInCycle
            | NSWindowCollectionBehavior::FullScreenPrimary
            | NSWindowCollectionBehavior::FullScreenNone,
    );
    current.insert(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    current
}

pub(crate) fn configure_panel_window(
    handle: AppKitWindowHandle,
) -> Result<OverlayWindowConfiguration, OverlayWindowError> {
    let mtm = MainThreadMarker::new().ok_or(OverlayWindowError::MainThreadRequired)?;
    with_native_view(handle.ns_view, |view| {
        let window = view
            .window()
            .ok_or(OverlayWindowError::DetachedNativeView)?;

        window.setStyleMask(overlay_style_mask(window.styleMask()));
        window.setCollectionBehavior(overlay_collection_behavior(window.collectionBehavior()));
        window.setExcludedFromWindowsMenu(true);
        window.setLevel(NSFloatingWindowLevel);
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        // Give the visually borderless panel a real, transparent AppKit title
        // region. `movableByWindowBackground` alone is ignored by winit's
        // renderer view; the title region provides a dependable drag target in
        // the unused outer surface while FullSizeContentView preserves the
        // existing edge-to-edge layout.
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        window.setTitlebarAppearsTransparent(true);
        window.setMovableByWindowBackground(true);

        pin_content_gravity(view);

        let backdrop = install_backdrop(mtm, view);
        Ok(OverlayWindowConfiguration { backdrop })
    })
}

/// Stop the renderer's layer from **stretching** while the window resizes.
///
/// A Core Animation layer's default `contentsGravity` is `resize`: during an
/// animated `setFrame`, AppKit scales the last rendered frame to each
/// intermediate size, so a panel growing by 200 pt smears its text vertically
/// for the length of the animation (owner report, 2026-08-04: "the animation
/// is stretching the text … very, very hard to read").
///
/// `topLeft` pins those pixels at their natural size against the panel's own
/// anchor instead. Growth then reveals space the next frame paints into, which
/// is what "only the outer size changes" means; shrinking crops rather than
/// squeezes. `setNeedsDisplayOnBoundsChange` asks for that next frame as early
/// as the bounds move, so the revealed strip is empty for as little time as
/// possible.
fn pin_content_gravity(view: &NSView) {
    let Some(layer) = view.layer() else {
        return;
    };
    // The gravity constants are plain strings; naming the value avoids
    // linking against `kCAGravityTopLeft` for one assignment.
    layer.setContentsGravity(&NSString::from_str("topLeft"));
    layer.setNeedsDisplayOnBoundsChange(true);
}

pub(crate) fn present_panel_without_activation(
    handle: AppKitWindowHandle,
) -> Result<(), OverlayWindowError> {
    let _mtm = MainThreadMarker::new().ok_or(OverlayWindowError::MainThreadRequired)?;
    with_native_view(handle.ns_view, |view| {
        let window = view
            .window()
            .ok_or(OverlayWindowError::DetachedNativeView)?;

        // `orderFrontRegardless` orders the window even while aibo is inactive,
        // but unlike `makeKeyAndOrderFront` it does not activate the application
        // or replace the previously focused app.
        window.orderFrontRegardless();
        Ok(())
    })
}

/// Make aibo the active application, so its key window can take keystrokes.
///
/// Deliberately **not** part of [`present`]. On the hotkey path the panel must
/// appear *without* replacing the previously focused app — §8's insert sequence
/// depends on knowing which app to give focus back to, and activating aibo on
/// every show would make "restore focus to the target" mean "restore focus to
/// aibo".
///
/// It is needed after an out-of-process capture. `/usr/sbin/screencapture` is a
/// separate application; when it exits, macOS re-activates whatever was
/// frontmost before it, and that activation lands *after* the panel has asked
/// for focus. The panel is then visible, on top, and unable to receive a
/// keystroke — which is the one state a text box must never be in.
pub(crate) fn activate_self() -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    NSApplication::sharedApplication(mtm).activate();
    true
}

/// Whether the OS is currently in a dark appearance.
///
/// `None` off the main thread or before AppKit is up — the caller keeps its
/// previous answer rather than guessing. Matching on the name string covers
/// both `DarkAqua` and the vibrant variants.
pub(crate) fn system_prefers_dark() -> Option<bool> {
    let mtm = MainThreadMarker::new()?;
    let appearance = NSApplication::sharedApplication(mtm).effectiveAppearance();
    Some(appearance.name().to_string().contains("Dark"))
}

pub(crate) fn reduced_motion_preferred() -> bool {
    // This AppKit preference is main-thread-only. An off-main query cannot
    // safely hop synchronously to the UI thread, so prefer reduced motion.
    MainThreadMarker::new()
        .map(|_| NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion())
        .unwrap_or(true)
}

pub(crate) fn announce_accessibility(message: &str) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let application = NSApplication::sharedApplication(mtm);
    let announcement = NSString::from_str(message);
    let values: [&AnyObject; 1] = [as_any_object(&*announcement)];
    let (key, notification) = accessibility_constants();
    let user_info = NSDictionary::from_slices(&[key], &values);

    post_accessibility_announcement(as_any_object(&*application), notification, &user_info);
}

#[allow(unsafe_code)]
fn install_backdrop(mtm: MainThreadMarker, host: &NSView) -> BackdropStatus {
    // The winit view is backed directly by the renderer's CAMetalLayer.
    // Installing a visual-effect *subview* on it covers that layer even when
    // the subview is ordered below every sibling, leaving an otherwise
    // functional panel as a featureless grey rectangle. Install the effect as
    // a sibling behind the render view in AppKit's frame view instead.
    // SAFETY: `host` is a live AppKit view borrowed from the window handle,
    // and `configure_panel_window` established that this code is running on
    // the main thread. Reading its retained superview does not mutate either.
    let Some(frame_view) = (unsafe { host.superview() }) else {
        return BackdropStatus::Unavailable;
    };
    if frame_view.subviews().iter().any(|view| {
        view.identifier()
            .as_deref()
            .is_some_and(|identifier| identifier.to_string() == EFFECT_IDENTIFIER)
    }) {
        return BackdropStatus::Applied;
    }

    let effect = NSVisualEffectView::initWithFrame(mtm.alloc(), host.frame());
    effect.setIdentifier(Some(&NSString::from_str(EFFECT_IDENTIFIER)));
    effect.setMaterial(NSVisualEffectMaterial::HUDWindow);
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    // The panel is intentionally shown while aibo is inactive. Following the
    // window-active state would turn the HUD material into a flat grey surface.
    effect.setState(NSVisualEffectState::Active);
    // Top-anchored with a *fixed* height, not height-sizable: the window is
    // sometimes taller than the visible panel (a floating menu needs room
    // below the chrome, §9), and a backdrop that stretched with the window
    // would render the slack as a frosted strip. The shell keeps the height
    // in step with the chrome via [`set_panel_backdrop_height`].
    effect.setAutoresizingMask(top_anchored_mask(frame_view.isFlipped()));
    frame_view.addSubview_positioned_relativeTo(&effect, NSWindowOrderingMode::Below, Some(host));
    BackdropStatus::Applied
}

/// Set the panel's frame in one native call without scaling its contents.
///
/// `x`/`y` are top-left-origin global logical points — the shell's own
/// convention — flipped here against the primary screen into AppKit's
/// bottom-left frame. Applying the complete frame atomically preserves the
/// top edge without the transient intermediate sizes produced by separate
/// resize and move effects. Animation stays disabled: AppKit scales the last
/// Metal frame while interpolating the window bounds, which visibly changes
/// text size during every growth step.
pub(crate) fn set_panel_frame(
    handle: AppKitWindowHandle,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<(), OverlayWindowError> {
    let mtm = MainThreadMarker::new().ok_or(OverlayWindowError::MainThreadRequired)?;
    with_native_view(handle.ns_view, |host| {
        let Some(window) = host.window() else {
            return Ok(());
        };
        // AppKit's global space is anchored to the primary screen's
        // bottom-left; winit's to its top-left. The primary screen is the
        // first in `screens` and holds the origin in both conventions.
        let Some(primary) = NSScreen::screens(mtm).firstObject() else {
            return Ok(());
        };
        let frame = NSRect::new(
            NSPoint::new(x, primary.frame().size.height - y - height),
            NSSize::new(width, height),
        );
        window.setFrame_display_animate(frame, true, false);
        Ok(())
    })
}

/// Resize the installed backdrop to hug the top `height` points of the panel.
///
/// A no-op when no backdrop was installed (older OS, or configuration never
/// ran) — the backdrop is cosmetic and its absence must stay harmless.
#[allow(unsafe_code)]
pub(crate) fn set_panel_backdrop_height(
    handle: AppKitWindowHandle,
    height: f64,
) -> Result<(), OverlayWindowError> {
    let _mtm = MainThreadMarker::new().ok_or(OverlayWindowError::MainThreadRequired)?;
    with_native_view(handle.ns_view, |host| {
        // SAFETY: as in `install_backdrop` — `host` is a live AppKit view
        // borrowed from the window handle, on the main thread; reading its
        // retained superview mutates nothing.
        let Some(frame_view) = (unsafe { host.superview() }) else {
            return Ok(());
        };
        let flipped = frame_view.isFlipped();
        for view in frame_view.subviews().iter() {
            if view
                .identifier()
                .as_deref()
                .is_some_and(|identifier| identifier.to_string() == EFFECT_IDENTIFIER)
            {
                view.setFrame(top_anchored(host.frame(), height, flipped));
                view.setAutoresizingMask(top_anchored_mask(flipped));
            }
        }
        Ok(())
    })
}

/// The frame pinning a subview to the top of `host` at `height` points.
///
/// "Top" depends on the superview's coordinate orientation: AppKit frame views
/// may or may not be flipped, and guessing wrong anchors the blur to the
/// bottom — which is exactly the strip this exists to prevent.
fn top_anchored(host: NSRect, height: f64, flipped: bool) -> NSRect {
    let height = height.clamp(0.0, host.size.height);
    let y = if flipped {
        host.origin.y
    } else {
        host.origin.y + host.size.height - height
    };
    NSRect::new(
        NSPoint::new(host.origin.x, y),
        NSSize::new(host.size.width, height),
    )
}

/// The autoresizing mask that keeps a top-anchored, fixed-height subview
/// pinned through window resizes that land after an explicit [`top_anchored`]
/// placement.
fn top_anchored_mask(flipped: bool) -> NSAutoresizingMaskOptions {
    NSAutoresizingMaskOptions::ViewWidthSizable
        | if flipped {
            NSAutoresizingMaskOptions::ViewMaxYMargin
        } else {
            NSAutoresizingMaskOptions::ViewMinYMargin
        }
}

/// Reinterpret the AppKit handle only for the duration of one operation.
#[allow(unsafe_code)]
fn with_native_view<R>(pointer: NonNull<c_void>, operation: impl FnOnce(&NSView) -> R) -> R {
    // SAFETY: `AppKitWindowHandle::ns_view` is specified to point to the live
    // NSView represented by the borrowed raw-window-handle. Public entry points
    // accept `WindowHandle<'_>` rather than a freely constructible raw handle,
    // and AppKit access is gated to the main thread before reaching this helper.
    operation(unsafe { pointer.cast::<NSView>().as_ref() })
}

/// Erase a statically known Objective-C class for heterogeneous dictionaries.
#[allow(unsafe_code)]
fn as_any_object<T: Message>(object: &T) -> &AnyObject {
    // SAFETY: every `Message` implementor is an Objective-C object with the
    // same base address as `AnyObject`; no ownership or lifetime is changed.
    unsafe { &*(object as *const T).cast::<AnyObject>() }
}

/// Fetch AppKit's process-lifetime accessibility constants in one audited
/// unsafe boundary.
#[allow(unsafe_code)]
fn accessibility_constants() -> (
    &'static objc2_app_kit::NSAccessibilityNotificationUserInfoKey,
    &'static objc2_app_kit::NSAccessibilityNotificationName,
) {
    // SAFETY: both extern statics are AppKit-owned strings valid for the
    // lifetime of the process.
    unsafe {
        (
            NSAccessibilityAnnouncementKey,
            NSAccessibilityAnnouncementRequestedNotification,
        )
    }
}

/// Post one correctly typed AppKit accessibility notification.
#[allow(unsafe_code)]
fn post_accessibility_announcement(
    application: &AnyObject,
    notification: &objc2_app_kit::NSAccessibilityNotificationName,
    user_info: &NSDictionary<objc2_app_kit::NSAccessibilityNotificationUserInfoKey, AnyObject>,
) {
    // SAFETY: `application` is a live NSApplication, the dictionary has the
    // exact key/value generic types required by AppKit, and all objects remain
    // retained for the synchronous call.
    unsafe {
        NSAccessibilityPostNotificationWithUserInfo(application, notification, Some(user_info));
    }
}

#[cfg(test)]
mod tests {
    use super::{overlay_collection_behavior, overlay_style_mask, top_anchored, top_anchored_mask};
    use objc2_app_kit::{NSAutoresizingMaskOptions, NSWindowCollectionBehavior, NSWindowStyleMask};
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    /// The backdrop hugs the top of the panel in either coordinate
    /// orientation; anchoring to the wrong edge puts the blur under the
    /// transparent slack instead of the chrome.
    #[test]
    fn the_backdrop_pins_to_the_top_whatever_the_orientation() {
        let host = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(680.0, 500.0));

        let unflipped = top_anchored(host, 140.0, false);
        assert_eq!(unflipped.origin.y, 360.0, "top in bottom-left coordinates");
        assert_eq!(unflipped.size.height, 140.0);

        let flipped = top_anchored(host, 140.0, true);
        assert_eq!(flipped.origin.y, 0.0, "top in top-left coordinates");
        assert_eq!(flipped.size.height, 140.0);

        // A chrome estimate taller than the window must clamp, not overflow.
        let clamped = top_anchored(host, 900.0, false);
        assert_eq!(clamped.size.height, 500.0);
        assert_eq!(clamped.origin.y, 0.0);
    }

    /// Between explicit placements, autoresizing must flex the *bottom*
    /// margin so window growth leaves the backdrop's height alone.
    #[test]
    fn the_backdrop_mask_keeps_height_fixed_through_window_growth() {
        assert!(!top_anchored_mask(false).contains(NSAutoresizingMaskOptions::ViewHeightSizable));
        assert!(top_anchored_mask(false).contains(NSAutoresizingMaskOptions::ViewMinYMargin));
        assert!(top_anchored_mask(true).contains(NSAutoresizingMaskOptions::ViewMaxYMargin));
    }

    #[test]
    fn utility_style_preserves_the_renderers_existing_style() {
        let current = NSWindowStyleMask::Borderless | NSWindowStyleMask::Resizable;
        let result = overlay_style_mask(current);
        assert!(result.contains(NSWindowStyleMask::UtilityWindow));
        assert!(result.contains(NSWindowStyleMask::Titled));
        assert!(result.contains(NSWindowStyleMask::FullSizeContentView));
        assert!(result.contains(NSWindowStyleMask::Resizable));
        assert!(!result.contains(NSWindowStyleMask::NonactivatingPanel));
    }

    #[test]
    fn all_spaces_policy_removes_conflicting_collection_bits() {
        let current = NSWindowCollectionBehavior::MoveToActiveSpace
            | NSWindowCollectionBehavior::Managed
            | NSWindowCollectionBehavior::ParticipatesInCycle
            | NSWindowCollectionBehavior::FullScreenPrimary;
        let result = overlay_collection_behavior(current);

        assert!(result.contains(NSWindowCollectionBehavior::CanJoinAllSpaces));
        assert!(result.contains(NSWindowCollectionBehavior::Stationary));
        assert!(result.contains(NSWindowCollectionBehavior::IgnoresCycle));
        assert!(result.contains(NSWindowCollectionBehavior::FullScreenAuxiliary));
        assert!(!result.intersects(
            NSWindowCollectionBehavior::MoveToActiveSpace
                | NSWindowCollectionBehavior::Managed
                | NSWindowCollectionBehavior::ParticipatesInCycle
                | NSWindowCollectionBehavior::FullScreenPrimary
        ));
    }
}
