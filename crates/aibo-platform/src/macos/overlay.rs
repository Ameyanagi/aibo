//! AppKit implementation of the transient panel-window boundary.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::{
    NSAccessibilityAnnouncementKey, NSAccessibilityAnnouncementRequestedNotification,
    NSAccessibilityPostNotificationWithUserInfo, NSApplication, NSAutoresizingMaskOptions, NSColor,
    NSFloatingWindowLevel, NSUserInterfaceItemIdentification, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindowCollectionBehavior,
    NSWindowOrderingMode, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSDictionary, NSString};
use raw_window_handle::AppKitWindowHandle;

use crate::overlay::{BackdropStatus, OverlayWindowConfiguration, OverlayWindowError};

const EFFECT_IDENTIFIER: &str = "com.aibo.panel.backdrop";

/// Bits to add to a borderless winit window without turning it into an
/// `NSPanel` (which it is not).
fn overlay_style_mask(mut current: NSWindowStyleMask) -> NSWindowStyleMask {
    current.insert(NSWindowStyleMask::UtilityWindow);
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

        let backdrop = install_backdrop(mtm, view);
        Ok(OverlayWindowConfiguration { backdrop })
    })
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
    effect.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    frame_view.addSubview_positioned_relativeTo(&effect, NSWindowOrderingMode::Below, Some(host));
    BackdropStatus::Applied
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
    use super::{overlay_collection_behavior, overlay_style_mask};
    use objc2_app_kit::{NSWindowCollectionBehavior, NSWindowStyleMask};

    #[test]
    fn utility_style_preserves_the_renderers_existing_style() {
        let current = NSWindowStyleMask::Borderless | NSWindowStyleMask::Resizable;
        let result = overlay_style_mask(current);
        assert!(result.contains(NSWindowStyleMask::UtilityWindow));
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
