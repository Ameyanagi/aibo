//! Read the target field back over AX so a run produces evidence, not a vibe.
//!
//! Without this the operator has to eyeball a 5 KB paste, which is exactly how
//! a truncation at character 4096 gets recorded as "worked". With it, S4 can say
//! *"diverged at character 1240, expected '\n' got 'r'"* — which points straight
//! at §8's claim that enigo silently drops chunks starting with a newline.
//!
//! This is a **deliberately minimal copy** of the read path S2 exercises fully.
//! A spike must not depend on another spike, and `aibo-platform` will own the
//! real thing.
//!
//! It only works where S2 says the app is AX-readable. Everywhere else the
//! operator reads the screen, which is why every command still prints what to
//! look for.

#![cfg(target_os = "macos")]

use accessibility_sys::{
    AXUIElementCopyAttributeValue, AXUIElementCreateApplication, AXUIElementRef,
    AXUIElementSetMessagingTimeout, kAXErrorSuccess,
};
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::string::CFString;
use core_foundation_sys::base::CFRelease;

/// An owned application `AXUIElementRef`.
struct AppElement(AXUIElementRef);

impl Drop for AppElement {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: created +1 by `AXUIElementCreateApplication`, released once.
            unsafe { CFRelease(self.0.cast()) };
        }
    }
}

/// The value of the focused text field in `pid`'s frontmost window.
///
/// `None` means AX could not read it — which for S4's purposes is "verification
/// unavailable, fall back to the operator's eyes", not "the insert failed".
pub fn focused_value(pid: i32, timeout_secs: f32) -> Option<String> {
    let app = AppElement(
        // SAFETY: returns +1 or null; a bad pid yields an element whose reads fail.
        unsafe { AXUIElementCreateApplication(pid) },
    );
    if app.0.is_null() {
        return None;
    }
    // SAFETY: `app.0` is live.
    unsafe { AXUIElementSetMessagingTimeout(app.0, timeout_secs) };

    let focused = copy_element(app.0, accessibility_sys::kAXFocusedUIElementAttribute)?;
    let value = copy_value(focused.0, accessibility_sys::kAXValueAttribute)?;
    value.downcast::<CFString>().map(|s| s.to_string())
}

fn copy_value(element: AXUIElementRef, attribute: &str) -> Option<CFType> {
    let key = CFString::new(attribute);
    let mut out: CFTypeRef = std::ptr::null();
    // SAFETY: `element` is live, `key` outlives the call, `out` is a valid
    // out-pointer. On success the value is +1 and wrapped under the create rule.
    let status =
        unsafe { AXUIElementCopyAttributeValue(element, key.as_concrete_TypeRef(), &mut out) };
    if status != kAXErrorSuccess || out.is_null() {
        return None;
    }
    // SAFETY: +1 value from a Copy-rule call.
    Some(unsafe { CFType::wrap_under_create_rule(out) })
}

fn copy_element(element: AXUIElementRef, attribute: &str) -> Option<AppElement> {
    let value = copy_value(element, attribute)?;
    // SAFETY: no arguments.
    let is_element = unsafe { core_foundation_sys::base::CFGetTypeID(value.as_CFTypeRef()) }
        == unsafe { accessibility_sys::AXUIElementGetTypeID() };
    if !is_element {
        return None;
    }
    let raw = value.as_CFTypeRef() as AXUIElementRef;
    // Move the +1 out of the `CFType` rather than letting both release it.
    std::mem::forget(value);
    Some(AppElement(raw))
}
