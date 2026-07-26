//! Thin CoreFoundation helpers shared by the macOS backend.
//!
//! `accessibility-sys` 0.2 is a raw `extern "C"` surface over
//! `core-foundation-sys` 0.8 (§20 notes it is a single-maintainer crate with
//! one release in four years — expect to vendor it). Everything in this module
//! exists so that the rest of the backend never has to hold a raw
//! `CFTypeRef` for longer than one statement.

use core_foundation::base::{CFRelease, CFType, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};

/// An owned `CFTypeRef` obtained from a `Copy`-rule CoreFoundation call.
///
/// AX attribute reads hand back +1 references. Wrapping them means an early
/// return or a `?` cannot leak.
pub(crate) struct OwnedCfType(CFTypeRef);

impl OwnedCfType {
    /// Take ownership of a `CFTypeRef` returned under the *create/copy* rule.
    ///
    /// # Safety
    /// `ptr` must be a non-null, +1-retained CoreFoundation object, and must
    /// not be released anywhere else.
    #[allow(unsafe_code)]
    pub(crate) unsafe fn from_create_rule(ptr: CFTypeRef) -> Option<Self> {
        if ptr.is_null() { None } else { Some(Self(ptr)) }
    }

    /// The borrowed raw pointer. Valid for as long as `self` is alive.
    pub(crate) fn as_ptr(&self) -> CFTypeRef {
        self.0
    }

    /// Borrow as a typed `CFType` so `downcast` can be used.
    ///
    /// The returned value follows the *get* rule: it does not take ownership
    /// away from `self`.
    #[allow(unsafe_code)]
    pub(crate) fn as_cf_type(&self) -> CFType {
        // SAFETY: `self.0` is a live CF object owned by `self`; `wrap_under_get_rule`
        // retains it, so the returned `CFType` has an independent reference.
        unsafe { CFType::wrap_under_get_rule(self.0) }
    }

    /// Interpret the value as a `CFString` and copy it into a Rust `String`.
    pub(crate) fn to_string_value(&self) -> Option<String> {
        self.as_cf_type()
            .downcast::<CFString>()
            .map(|s| s.to_string())
    }
}

impl Drop for OwnedCfType {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: `self.0` was obtained under the create/copy rule and is
        // released exactly once, here.
        unsafe { CFRelease(self.0) }
    }
}

/// Build a `CFString` from a Rust string slice.
///
/// AX attribute names are ASCII constants from `accessibility-sys`, so this is
/// on the hot path of every capture — but `CFString::new` is a cheap
/// short-string allocation and caching it would need a per-thread interner
/// that is not worth the complexity yet.
pub(crate) fn cf_string(s: &str) -> CFString {
    CFString::new(s)
}

/// Borrow a `CFString`'s raw pointer for an FFI call.
pub(crate) fn cf_string_ref(s: &CFString) -> CFStringRef {
    s.as_concrete_TypeRef()
}
