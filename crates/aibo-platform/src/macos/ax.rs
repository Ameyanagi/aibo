//! `AXUIElement` wrapper.
//!
//! These objects are **not** `Send` and **not** `Sync` and they must never be
//! touched from the UI event loop: a synchronous
//! `AXUIElementCopyAttributeValue` against a busy app blocks for *seconds*
//! (§8). Everything here therefore lives on the dedicated thread in
//! [`super::thread`], and every element gets an explicit AX messaging timeout
//! so the thread itself cannot be wedged by one unresponsive app.

use std::ffi::c_void;

use accessibility_sys::{
    AXUIElementCopyAttributeValue, AXUIElementCopyParameterizedAttributeValue,
    AXUIElementCreateApplication, AXUIElementCreateSystemWide, AXUIElementGetPid, AXUIElementRef,
    AXUIElementSetAttributeValue, AXUIElementSetMessagingTimeout, AXValueCreate, AXValueGetType,
    AXValueGetValue, AXValueRef, kAXValueTypeCFRange, kAXValueTypeCGRect,
};
use core_foundation::base::{CFHash, CFRange, CFRelease, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_graphics::geometry::CGRect;

use super::cf::{OwnedCfType, cf_string, cf_string_ref};
use super::error::{MacosError, MacosResult, ax_result};

/// An owned reference to an accessibility element.
///
/// Deliberately neither `Send` nor `Sync` (the raw pointer makes it so): the
/// type system enforces the "one dedicated AX thread" rule from §7.
pub(crate) struct AxElement {
    raw: AXUIElementRef,
}

impl AxElement {
    /// Wrap a raw element obtained under the create/copy rule.
    ///
    /// # Safety
    /// `raw` must be non-null and +1-retained.
    #[allow(unsafe_code)]
    unsafe fn from_create_rule(raw: AXUIElementRef) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Self { raw })
        }
    }

    /// The system-wide element — the entry point for
    /// `kAXFocusedApplicationAttribute` and `kAXFocusedUIElementAttribute`.
    #[allow(unsafe_code)]
    pub(crate) fn system_wide() -> MacosResult<Self> {
        // SAFETY: no arguments; returns a +1 element or null.
        let raw = unsafe { AXUIElementCreateSystemWide() };
        // SAFETY: `raw` is +1-retained per the Copy rule.
        unsafe { Self::from_create_rule(raw) }
            .ok_or_else(|| MacosError::Platform("AXUIElementCreateSystemWide returned null".into()))
    }

    /// The application element for a process id.
    #[allow(unsafe_code)]
    pub(crate) fn application(pid: i32) -> MacosResult<Self> {
        // SAFETY: `pid` is a plain integer; returns a +1 element or null.
        let raw = unsafe { AXUIElementCreateApplication(pid) };
        // SAFETY: `raw` is +1-retained per the Copy rule.
        unsafe { Self::from_create_rule(raw) }.ok_or_else(|| {
            MacosError::Platform(format!("AXUIElementCreateApplication({pid}) returned null"))
        })
    }

    /// Bound how long a single AX message may block.
    ///
    /// This is the *first* half of the §8 deadline: the caller's
    /// `tokio::time::timeout` bounds the reply, and this bounds the worker
    /// thread so the next request is not stuck behind a hung app.
    #[allow(unsafe_code)]
    pub(crate) fn set_messaging_timeout(&self, seconds: f32) {
        // SAFETY: `self.raw` is a live element; the call only stores a float.
        let _ = unsafe { AXUIElementSetMessagingTimeout(self.raw, seconds) };
    }

    /// The owning process id.
    #[allow(unsafe_code)]
    pub(crate) fn pid(&self) -> MacosResult<i32> {
        let mut pid: i32 = 0;
        // SAFETY: `self.raw` is live; `pid` is a valid out-pointer.
        let code = unsafe { AXUIElementGetPid(self.raw, &mut pid) };
        ax_result(code)?;
        Ok(pid)
    }

    /// A stable opaque identity for this element, used by `validate_target`.
    ///
    /// `AXUIElement` implements `CFEqual`/`CFHash` meaningfully, so two copies
    /// of the same element hash equally. That is exactly what
    /// [`InsertTarget::focused_element`] needs.
    ///
    /// [`InsertTarget::focused_element`]: aibo_core::types::InsertTarget::focused_element
    #[allow(unsafe_code)]
    pub(crate) fn identity(&self) -> String {
        // SAFETY: `self.raw` is a live CF object.
        let hash = unsafe { CFHash(self.raw.cast::<c_void>()) };
        format!("ax:{hash:016x}")
    }

    /// Copy an attribute value, transferring ownership to the caller.
    #[allow(unsafe_code)]
    pub(crate) fn attribute(&self, name: &str) -> MacosResult<OwnedCfType> {
        let key = cf_string(name);
        let mut value: CFTypeRef = std::ptr::null();
        // SAFETY: `self.raw` and the key are live; `value` is a valid out-pointer
        // written only when the call returns success.
        let code =
            unsafe { AXUIElementCopyAttributeValue(self.raw, cf_string_ref(&key), &mut value) };
        ax_result(code)?;
        // SAFETY: on success `value` is +1-retained per the Copy rule.
        unsafe { OwnedCfType::from_create_rule(value) }
            .ok_or(MacosError::Ax("attribute returned a null value"))
    }

    /// Copy an attribute and interpret it as a string.
    pub(crate) fn string_attribute(&self, name: &str) -> MacosResult<String> {
        self.attribute(name)?
            .to_string_value()
            .ok_or(MacosError::Ax("attribute was not a CFString"))
    }

    /// Copy an attribute and interpret it as a boolean.
    pub(crate) fn bool_attribute(&self, name: &str) -> MacosResult<bool> {
        let value = self.attribute(name)?;
        value
            .as_cf_type()
            .downcast::<CFBoolean>()
            .map(Into::into)
            .ok_or(MacosError::Ax("attribute was not a CFBoolean"))
    }

    /// Copy an attribute and interpret it as an integer.
    pub(crate) fn int_attribute(&self, name: &str) -> MacosResult<i64> {
        let value = self.attribute(name)?;
        value
            .as_cf_type()
            .downcast::<CFNumber>()
            .and_then(|n| n.to_i64())
            .ok_or(MacosError::Ax("attribute was not a CFNumber"))
    }

    /// Copy an attribute that is itself an element.
    #[allow(unsafe_code)]
    pub(crate) fn element_attribute(&self, name: &str) -> MacosResult<AxElement> {
        let key = cf_string(name);
        let mut value: CFTypeRef = std::ptr::null();
        // SAFETY: as in `attribute`.
        let code =
            unsafe { AXUIElementCopyAttributeValue(self.raw, cf_string_ref(&key), &mut value) };
        ax_result(code)?;
        // SAFETY: on success `value` is a +1-retained AXUIElement.
        unsafe { Self::from_create_rule(value as AXUIElementRef) }
            .ok_or(MacosError::Ax("attribute was not an AXUIElement"))
    }

    /// `kAXSelectedTextRangeAttribute`, unwrapped from its `AXValue` box.
    ///
    /// The attribute is **not** a `CFRange` — it is an `AXValue` *wrapping* a
    /// `CFRange`, and the only way out is `AXValueGetValue`. §8 calls this out
    /// explicitly because reading it as a number silently yields garbage.
    ///
    /// The returned offsets are in UTF-16 code units, which is what every
    /// caller must convert from before indexing a Rust `String`.
    #[allow(unsafe_code)]
    pub(crate) fn selected_range(&self) -> MacosResult<CFRange> {
        let value = self.attribute(accessibility_sys::kAXSelectedTextRangeAttribute)?;
        let ax_value = value.as_ptr() as AXValueRef;
        // SAFETY: `ax_value` is the live object owned by `value`.
        if unsafe { AXValueGetType(ax_value) } != kAXValueTypeCFRange {
            return Err(MacosError::Ax(
                "AXSelectedTextRange was not a CFRange AXValue",
            ));
        }
        let mut range = CFRange {
            location: 0,
            length: 0,
        };
        // SAFETY: the type tag was checked above, so writing a `CFRange` through
        // the out-pointer is the documented contract of `AXValueGetValue`.
        let ok = unsafe {
            AXValueGetValue(
                ax_value,
                kAXValueTypeCFRange,
                (&raw mut range).cast::<c_void>(),
            )
        };
        if ok {
            Ok(range)
        } else {
            Err(MacosError::Ax("AXValueGetValue(CFRange) failed"))
        }
    }

    /// `AXBoundsForRange`, used to anchor the panel to the caret (§9).
    ///
    /// Returns screen coordinates with a top-left origin, matching
    /// [`aibo_core::types::Rect`].
    #[allow(unsafe_code)]
    pub(crate) fn bounds_for_range(&self, range: CFRange) -> MacosResult<CGRect> {
        // SAFETY: `&range` points at a valid `CFRange` for the duration of the call.
        let param =
            unsafe { AXValueCreate(kAXValueTypeCFRange, (&raw const range).cast::<c_void>()) };
        if param.is_null() {
            return Err(MacosError::Ax("AXValueCreate(CFRange) returned null"));
        }
        let key = cf_string(accessibility_sys::kAXBoundsForRangeParameterizedAttribute);
        let mut out: CFTypeRef = std::ptr::null();
        // SAFETY: all three inputs are live; `out` is a valid out-pointer.
        let code = unsafe {
            AXUIElementCopyParameterizedAttributeValue(
                self.raw,
                cf_string_ref(&key),
                param.cast::<c_void>(),
                &mut out,
            )
        };
        // SAFETY: `param` was created +1 by `AXValueCreate` and is released once.
        unsafe { CFRelease(param.cast::<c_void>()) };
        ax_result(code)?;
        // SAFETY: on success `out` is +1-retained.
        let owned = unsafe { OwnedCfType::from_create_rule(out) }
            .ok_or(MacosError::Ax("AXBoundsForRange returned null"))?;

        let ax_value = owned.as_ptr() as AXValueRef;
        // SAFETY: `ax_value` is the live object owned by `owned`.
        if unsafe { AXValueGetType(ax_value) } != kAXValueTypeCGRect {
            return Err(MacosError::Ax("AXBoundsForRange was not a CGRect AXValue"));
        }
        let mut rect = CGRect::new(
            &core_graphics::geometry::CGPoint::new(0.0, 0.0),
            &core_graphics::geometry::CGSize::new(0.0, 0.0),
        );
        // SAFETY: the type tag was checked above.
        let ok = unsafe {
            AXValueGetValue(
                ax_value,
                kAXValueTypeCGRect,
                (&raw mut rect).cast::<c_void>(),
            )
        };
        if ok {
            Ok(rect)
        } else {
            Err(MacosError::Ax("AXValueGetValue(CGRect) failed"))
        }
    }

    /// `AXStringForRange` — a *bounded* window of the field's text.
    ///
    /// §5 forbids pulling a whole document out of the target app before
    /// deciding it is too long, so the prefix/suffix capture asks for a range
    /// around the caret rather than reading `kAXValueAttribute`.
    #[allow(unsafe_code)]
    pub(crate) fn string_for_range(&self, range: CFRange) -> MacosResult<String> {
        // SAFETY: `&range` points at a valid `CFRange` for the duration of the call.
        let param =
            unsafe { AXValueCreate(kAXValueTypeCFRange, (&raw const range).cast::<c_void>()) };
        if param.is_null() {
            return Err(MacosError::Ax("AXValueCreate(CFRange) returned null"));
        }
        let key = cf_string(accessibility_sys::kAXStringForRangeParameterizedAttribute);
        let mut out: CFTypeRef = std::ptr::null();
        // SAFETY: all three inputs are live; `out` is a valid out-pointer.
        let code = unsafe {
            AXUIElementCopyParameterizedAttributeValue(
                self.raw,
                cf_string_ref(&key),
                param.cast::<c_void>(),
                &mut out,
            )
        };
        // SAFETY: `param` was created +1 and is released once.
        unsafe { CFRelease(param.cast::<c_void>()) };
        ax_result(code)?;
        // SAFETY: on success `out` is +1-retained.
        unsafe { OwnedCfType::from_create_rule(out) }
            .and_then(|v| v.to_string_value())
            .ok_or(MacosError::Ax("AXStringForRange was not a CFString"))
    }

    /// Set a boolean attribute — used only for the two AX-tree activation
    /// flags in [`super::apps`].
    #[allow(unsafe_code)]
    pub(crate) fn set_bool_attribute(&self, name: &str, value: bool) -> MacosResult<()> {
        let key = cf_string(name);
        let boxed = CFBoolean::from(value);
        // SAFETY: element, key and value are all live for the call.
        let code = unsafe {
            AXUIElementSetAttributeValue(self.raw, cf_string_ref(&key), boxed.as_CFTypeRef())
        };
        ax_result(code)
    }

    /// Does this element advertise the attribute at all?
    ///
    /// Cheaper and less noisy than reading it and discarding
    /// `kAXErrorAttributeUnsupported`.
    pub(crate) fn has_attribute(&self, name: &str) -> bool {
        self.attribute(name).is_ok()
    }

    /// The element's `AXRole`, when it has one.
    pub(crate) fn role(&self) -> Option<String> {
        self.string_attribute(accessibility_sys::kAXRoleAttribute)
            .ok()
    }

    /// The best available human label for a field: `AXTitle`, then
    /// `AXDescription`, then `AXPlaceholderValue` (§5 puts this in the Complete
    /// prompt).
    pub(crate) fn label(&self) -> Option<String> {
        for key in [
            accessibility_sys::kAXTitleAttribute,
            accessibility_sys::kAXDescriptionAttribute,
            accessibility_sys::kAXPlaceholderValueAttribute,
        ] {
            if let Ok(s) = self.string_attribute(key)
                && !s.trim().is_empty()
            {
                return Some(s);
            }
        }
        // `AXTitleUIElement` points at a separate label element (the common
        // shape for a form field next to its `<label>`).
        self.element_attribute(accessibility_sys::kAXTitleUIElementAttribute)
            .ok()
            .and_then(|e| {
                e.string_attribute(accessibility_sys::kAXValueAttribute)
                    .ok()
            })
            .filter(|s| !s.trim().is_empty())
    }
}

impl Drop for AxElement {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: `self.raw` was obtained under the create/copy rule and is
        // released exactly once, here.
        unsafe { CFRelease(self.raw.cast::<c_void>()) }
    }
}
