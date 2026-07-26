//! The AX half: watch what the focused field reports *while* a composition is
//! being typed.
//!
//! §9 rule 3 makes a specific claim that this module exists to verify or refute:
//!
//! > AX/UIA field reads during composition return either the pre-composition
//! > text or the uncommitted reading, and **neither is what the user sees.**
//!
//! There is no public AX attribute for marked text, so the method is
//! observational: sample `AXValue` and `AXSelectedTextRange` at a fixed interval
//! while the operator types Japanese, and print every change. The shape of the
//! transcript is the finding.
//!
//! A **deliberately minimal copy** of the read path S2 exercises fully — a spike
//! must not depend on another spike.

#![cfg(target_os = "macos")]

use accessibility_sys::{
    AXUIElementCopyAttributeNames, AXUIElementCopyAttributeValue, AXUIElementCreateApplication,
    AXUIElementRef, AXUIElementSetMessagingTimeout, AXValueGetTypeID, AXValueGetValue, AXValueRef,
    kAXErrorSuccess, kAXValueTypeCFRange,
};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFGetTypeID, CFRelease};
use core_foundation_sys::string::CFStringRef;

/// An owned `AXUIElementRef`.
pub struct Element(AXUIElementRef);

impl Drop for Element {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: +1 on creation, released exactly once.
            unsafe { CFRelease(self.0.cast()) };
        }
    }
}

/// The application element for a pid, with a bounded messaging timeout.
///
/// §8: an unbounded AX call against a busy app blocks for *seconds*. During
/// composition the target app is by definition busy talking to the input method,
/// which makes the bound matter more here than anywhere else.
pub fn application(pid: i32, timeout_secs: f32) -> Option<Element> {
    // SAFETY: returns +1 or null.
    let raw = unsafe { AXUIElementCreateApplication(pid) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` is live.
    unsafe { AXUIElementSetMessagingTimeout(raw, timeout_secs) };
    Some(Element(raw))
}

impl Element {
    /// Read one attribute as an untyped CF value.
    pub fn attribute(&self, name: &str) -> Option<CFType> {
        let key = CFString::new(name);
        let mut out: CFTypeRef = std::ptr::null();
        // SAFETY: element live, key outlives the call, `out` is a valid
        // out-pointer; on success the value is +1.
        let status =
            unsafe { AXUIElementCopyAttributeValue(self.0, key.as_concrete_TypeRef(), &mut out) };
        if status != kAXErrorSuccess || out.is_null() {
            return None;
        }
        // SAFETY: +1 from a Copy-rule call.
        Some(unsafe { CFType::wrap_under_create_rule(out) })
    }

    /// Read one attribute expected to be another element.
    pub fn element_attribute(&self, name: &str) -> Option<Element> {
        let value = self.attribute(name)?;
        // SAFETY: no arguments.
        let is_element = unsafe { CFGetTypeID(value.as_CFTypeRef()) }
            == unsafe { accessibility_sys::AXUIElementGetTypeID() };
        if !is_element {
            return None;
        }
        let raw = value.as_CFTypeRef() as AXUIElementRef;
        // Move the +1 out rather than letting both release it.
        std::mem::forget(value);
        Some(Element(raw))
    }

    /// The focused element in this application.
    pub fn focused(&self) -> Option<Element> {
        self.element_attribute(accessibility_sys::kAXFocusedUIElementAttribute)
    }

    /// `AXValue` as a string.
    pub fn text_value(&self) -> Option<String> {
        self.attribute(accessibility_sys::kAXValueAttribute)?
            .downcast::<CFString>()
            .map(|s| s.to_string())
    }

    /// `AXSelectedText` as a string.
    pub fn selected_text(&self) -> Option<String> {
        self.attribute(accessibility_sys::kAXSelectedTextAttribute)?
            .downcast::<CFString>()
            .map(|s| s.to_string())
    }

    /// `AXSelectedTextRange`, in **UTF-16 code units**.
    pub fn selected_range(&self) -> Option<(isize, isize)> {
        let value = self.attribute(accessibility_sys::kAXSelectedTextRangeAttribute)?;
        // SAFETY: no arguments.
        if unsafe { CFGetTypeID(value.as_CFTypeRef()) } != unsafe { AXValueGetTypeID() } {
            return None;
        }
        #[repr(C)]
        struct RawRange {
            location: isize,
            length: isize,
        }
        let mut raw = RawRange {
            location: 0,
            length: 0,
        };
        // SAFETY: type id confirmed AXValue; `kAXValueTypeCFRange` matches the
        // layout of `RawRange`; the out-pointer is valid for that size.
        let ok = unsafe {
            AXValueGetValue(
                value.as_CFTypeRef() as AXValueRef,
                kAXValueTypeCFRange,
                (&raw mut raw).cast(),
            )
        };
        ok.then_some((raw.location, raw.length))
    }

    /// Every attribute name this element advertises.
    ///
    /// Used by the `attributes` subcommand to hunt for anything composition
    /// related — `AXMarked*`, `AXTextMarker*`, `AX*Composition*`. If a
    /// widely-used app publishes such an attribute, that is a better answer than
    /// §20's fallback and is the single most valuable thing S7 can find.
    pub fn attribute_names(&self) -> Vec<String> {
        let mut names: core_foundation_sys::array::CFArrayRef = std::ptr::null();
        // SAFETY: element live, `names` a valid out-pointer; +1 array on success.
        let status = unsafe { AXUIElementCopyAttributeNames(self.0, &mut names) };
        if status != kAXErrorSuccess || names.is_null() {
            return Vec::new();
        }
        // SAFETY: +1 array of CFStringRef.
        let array: CFArray<*const std::ffi::c_void> =
            unsafe { CFArray::wrap_under_create_rule(names) };
        (0..array.len())
            .filter_map(|index| array.get(index))
            .filter_map(|item| {
                let ptr = *item as CFStringRef;
                (!ptr.is_null())
                    // SAFETY: +0 borrow from the array; get-rule wrapping retains.
                    .then(|| unsafe { CFString::wrap_under_get_rule(ptr) }.to_string())
            })
            .collect()
    }
}
