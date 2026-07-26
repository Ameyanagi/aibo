//! The only `unsafe` in this spike, and the reason it is one file.
//!
//! Everything below wraps `accessibility-sys` — a bare `extern "C"` block over
//! `ApplicationServices` with no Rust-side logic (§20 notes the crate is single
//! maintainer with one release in four years; it was read before being used,
//! and vendoring it later is a copy-paste).
//!
//! Two invariants make the unsafety tractable:
//!
//! 1. **Every `AXUIElementRef` that crosses out of this module is owned by
//!    [`AxElement`], which `CFRelease`s on drop.** The AX API is all
//!    Copy-rule — `AXUIElementCopy*` returns +1 — so wrapping under the *create*
//!    rule is correct and double-freeing is the bug to look for.
//! 2. **Nothing here recurses.** The tree walk lives in `main.rs` with an
//!    explicit depth and node budget, so a pathological tree (Chrome's web area
//!    is tens of thousands of nodes) cannot blow the stack from inside `unsafe`.

#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use accessibility_sys::{
    AXError, AXUIElementCopyAttributeNames, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementCreateSystemWide, AXUIElementGetPid,
    AXUIElementGetTypeID, AXUIElementRef, AXUIElementSetAttributeValue,
    AXUIElementSetMessagingTimeout, AXValueGetTypeID, AXValueGetValue, AXValueRef, error_string,
    kAXErrorSuccess, kAXValueTypeCFRange,
};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeID};
use core_foundation_sys::string::CFStringRef;

/// An owned `AXUIElementRef`.
///
/// Not `Send`: AX calls are IPC to the target process and the plan pins them to
/// a dedicated capture thread (§8). Keeping the type thread-bound here means the
/// spike cannot accidentally prove something the product cannot do.
pub struct AxElement {
    raw: AXUIElementRef,
}

impl Drop for AxElement {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: `raw` came from an `AXUIElementCreate*`/`Copy*` call
            // (+1 retain) and is released exactly once, here.
            unsafe { CFRelease(self.raw.cast()) };
        }
    }
}

/// What one AX read produced, plus how long it took.
///
/// The duration is not decoration. §8 gives context capture a hard **120 ms**
/// deadline for AX/UIA, and the failure this spike is looking for is not only
/// "unsupported" but "supported and far too slow".
#[derive(Debug)]
pub struct Timed<T> {
    /// The value, or the raw `AXError`.
    pub result: Result<T, AxFailure>,
    /// Wall time of the single AX call.
    pub elapsed: Duration,
}

impl<T> Timed<T> {
    /// Did the read beat §8's 120 ms AX deadline?
    pub fn within_deadline(&self) -> bool {
        self.elapsed <= Duration::from_millis(120)
    }
}

/// An AX error, kept as the raw code because the code is the diagnosis.
///
/// `kAXErrorAttributeUnsupported` on `AXValue` means the app does not expose the
/// text; `kAXErrorCannotComplete` usually means the target is busy or the
/// process is not trusted; `kAXErrorAPIDisabled` means Accessibility was never
/// granted. Collapsing them into one "failed" loses the whole point of S2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxFailure(pub AXError);

impl std::fmt::Display for AxFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", error_string(self.0), self.0)
    }
}

impl std::error::Error for AxFailure {}

/// Is this process trusted for Accessibility?
///
/// §8: AX *reads* require the Accessibility TCC grant; `CGEventPost` does not.
/// They are two different services behind one System Settings pane, and this
/// answers only the first.
pub fn process_is_trusted() -> bool {
    // SAFETY: no arguments, no ownership.
    unsafe { accessibility_sys::AXIsProcessTrusted() }
}

fn cfstring(name: &str) -> CFString {
    CFString::new(name)
}

#[allow(
    dead_code,
    reason = "`system_wide` and `pid` are the shape aibo-platform \
    needs; the spike reaches the app element via NSWorkspace instead because it \
    also wants the bundle id for the matrix"
)]
impl AxElement {
    /// The system-wide element — the root used to reach the focused application.
    pub fn system_wide() -> Self {
        // SAFETY: returns a +1 reference or null.
        Self {
            raw: unsafe { AXUIElementCreateSystemWide() },
        }
    }

    /// The application element for a process id.
    pub fn application(pid: i32) -> Self {
        // SAFETY: returns a +1 reference; a bogus pid yields an element whose
        // reads fail with `kAXErrorInvalidUIElement`, not UB.
        Self {
            raw: unsafe { AXUIElementCreateApplication(pid) },
        }
    }

    /// Bound how long a single AX call may block.
    ///
    /// §8 corrects an earlier draft that had capture running synchronously:
    /// *"a synchronous `AXUIElementCopyAttributeValue` against a busy or
    /// unresponsive app blocks on the AX timeout — seconds, not milliseconds."*
    /// Setting this is how the spike measures a real app instead of hanging on
    /// one.
    pub fn set_messaging_timeout(&self, seconds: f32) -> Result<(), AxFailure> {
        // SAFETY: `raw` is a live element; the call takes a plain f32.
        check(unsafe { AXUIElementSetMessagingTimeout(self.raw, seconds) })
    }

    /// The process id this element belongs to.
    pub fn pid(&self) -> Result<i32, AxFailure> {
        let mut pid: i32 = 0;
        // SAFETY: `raw` is live; `pid` is a valid out-pointer for the call.
        check(unsafe { AXUIElementGetPid(self.raw, &mut pid) })?;
        Ok(pid)
    }

    /// Every attribute name the element advertises.
    ///
    /// This is the honest inventory: what an app *claims* to support before any
    /// of it is read. The gap between this list and what actually returns a
    /// value is one of the findings S2 exists to produce.
    pub fn attribute_names(&self) -> Timed<Vec<String>> {
        let started = Instant::now();
        let mut names: core_foundation_sys::array::CFArrayRef = std::ptr::null();
        // SAFETY: `raw` is live; `names` is a valid out-pointer. On success the
        // array is +1 and is wrapped under the create rule below.
        let status = unsafe { AXUIElementCopyAttributeNames(self.raw, &mut names) };
        let elapsed = started.elapsed();

        if status != kAXErrorSuccess || names.is_null() {
            return Timed {
                result: Err(AxFailure(status)),
                elapsed,
            };
        }
        // SAFETY: +1 array of CFStringRef.
        let array: CFArray<*const std::ffi::c_void> =
            unsafe { CFArray::wrap_under_create_rule(names) };
        let mut out = Vec::with_capacity(array.len() as usize);
        for index in 0..array.len() {
            if let Some(item) = array.get(index) {
                let ptr = *item as CFStringRef;
                if !ptr.is_null() {
                    // SAFETY: array elements are +0 borrows; get-rule wrapping
                    // retains so the CFString outlives the array.
                    out.push(unsafe { CFString::wrap_under_get_rule(ptr) }.to_string());
                }
            }
        }
        Timed {
            result: Ok(out),
            elapsed,
        }
    }

    /// Read one attribute as an untyped Core Foundation value.
    pub fn attribute(&self, name: &str) -> Timed<CFType> {
        let key = cfstring(name);
        let started = Instant::now();
        let mut value: CFTypeRef = std::ptr::null();
        // SAFETY: `raw` is live, `key` outlives the call, `value` is a valid
        // out-pointer. On success the value is +1.
        let status = unsafe {
            AXUIElementCopyAttributeValue(self.raw, key.as_concrete_TypeRef(), &mut value)
        };
        let elapsed = started.elapsed();

        if status != kAXErrorSuccess || value.is_null() {
            return Timed {
                result: Err(AxFailure(if value.is_null() && status == kAXErrorSuccess {
                    accessibility_sys::kAXErrorNoValue
                } else {
                    status
                })),
                elapsed,
            };
        }
        Timed {
            // SAFETY: +1 value from a Copy-rule call.
            result: Ok(unsafe { CFType::wrap_under_create_rule(value) }),
            elapsed,
        }
    }

    /// Read one attribute that is expected to be another element.
    pub fn element_attribute(&self, name: &str) -> Timed<AxElement> {
        let timed = self.attribute(name);
        let elapsed = timed.elapsed;
        let result = timed.result.and_then(|value| {
            if is_ax_element(&value) {
                // Take ownership of the +1 reference without letting `CFType`
                // also release it.
                let raw = value.as_CFTypeRef() as AXUIElementRef;
                std::mem::forget(value);
                Ok(AxElement { raw })
            } else {
                Err(AxFailure(accessibility_sys::kAXErrorIllegalArgument))
            }
        });
        Timed { result, elapsed }
    }

    /// Children of this element, as owned elements.
    pub fn children(&self) -> Timed<Vec<AxElement>> {
        let timed = self.attribute(accessibility_sys::kAXChildrenAttribute);
        let elapsed = timed.elapsed;
        let result = timed.result.and_then(|value| {
            let Some(array) = value.downcast::<CFArray<*const std::ffi::c_void>>() else {
                return Err(AxFailure(accessibility_sys::kAXErrorIllegalArgument));
            };
            let mut out = Vec::with_capacity(array.len() as usize);
            for index in 0..array.len() {
                if let Some(item) = array.get(index) {
                    let raw = *item as AXUIElementRef;
                    if !raw.is_null() {
                        // SAFETY: array elements are +0; retain so the child
                        // outlives the array, matching `AxElement`'s +1 contract.
                        unsafe { core_foundation_sys::base::CFRetain(raw.cast()) };
                        out.push(AxElement { raw });
                    }
                }
            }
            Ok(out)
        });
        Timed { result, elapsed }
    }

    /// Set a boolean attribute.
    ///
    /// The only reason this exists is §8's "enabling the AX tree" row:
    /// Chrome/Chromium honours `AXEnhancedUserInterface`, **Electron honours
    /// `AXManualAccessibility`**, and setting the wrong one returns
    /// `kAXErrorAttributeUnsupported`. Which flag each app wants is a matrix
    /// column, and this is how the operator fills it in.
    pub fn set_bool_attribute(&self, name: &str, value: bool) -> Result<(), AxFailure> {
        let key = cfstring(name);
        let flag = CFBoolean::from(value);
        // SAFETY: `raw` is live; both CF objects outlive the call, which does
        // not take ownership of them.
        check(unsafe {
            AXUIElementSetAttributeValue(self.raw, key.as_concrete_TypeRef(), flag.as_CFTypeRef())
        })
    }

    /// Convenience: `AXRole`, or `"?"`.
    pub fn role(&self) -> String {
        self.attribute(accessibility_sys::kAXRoleAttribute)
            .result
            .ok()
            .and_then(|v| v.downcast::<CFString>())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".to_owned())
    }
}

fn check(status: AXError) -> Result<(), AxFailure> {
    if status == kAXErrorSuccess {
        Ok(())
    } else {
        Err(AxFailure(status))
    }
}

fn type_id(value: &CFType) -> CFTypeID {
    // SAFETY: `value` holds a live CF object.
    unsafe { CFGetTypeID(value.as_CFTypeRef()) }
}

fn is_ax_element(value: &CFType) -> bool {
    // SAFETY: no arguments.
    type_id(value) == unsafe { AXUIElementGetTypeID() }
}

/// A `CFRange` unwrapped from an `AXValue`.
///
/// §8 names this specifically: `kAXSelectedTextRangeAttribute` is *"an `AXValue`
/// wrapping `CFRange` — unwrap via `AXValueGetValue`"*. Reading it as a number or
/// a string silently yields nothing, which is a very easy way to conclude an app
/// is unsupported when it is not.
///
/// The units are **UTF-16 code units**, not bytes and not graphemes. Anything
/// built on this must convert before indexing Rust strings.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct AxRange {
    /// Offset in UTF-16 code units.
    pub location: isize,
    /// Length in UTF-16 code units.
    pub length: isize,
}

/// Try to unwrap a `CFRange` out of an `AXValue`.
pub fn as_range(value: &CFType) -> Option<AxRange> {
    // SAFETY: no arguments.
    if type_id(value) != unsafe { AXValueGetTypeID() } {
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
    // SAFETY: the type id was just confirmed to be AXValue, `kAXValueTypeCFRange`
    // matches the `CFRange` layout of `RawRange`, and the out-pointer is valid
    // for that size. A type mismatch returns false rather than writing.
    let ok = unsafe {
        AXValueGetValue(
            value.as_CFTypeRef() as AXValueRef,
            kAXValueTypeCFRange,
            (&raw mut raw).cast(),
        )
    };
    ok.then_some(AxRange {
        location: raw.location,
        length: raw.length,
    })
}

/// Render any attribute value for the dump.
///
/// Long strings are truncated to `limit` **characters** with the true length
/// reported alongside — a 4 MB `AXValue` from a code editor should not end up in
/// the report, but the fact that it was 4 MB should.
pub fn describe(value: &CFType, limit: usize) -> serde_json::Value {
    use serde_json::json;

    if let Some(text) = value.downcast::<CFString>() {
        let text = text.to_string();
        let chars = text.chars().count();
        let shown: String = text.chars().take(limit).collect();
        return json!({
            "kind": "string",
            "chars": chars,
            "truncated": chars > limit,
            "value": shown,
        });
    }
    if let Some(range) = as_range(value) {
        return json!({ "kind": "range_utf16", "location": range.location, "length": range.length });
    }
    if let Some(number) = value.downcast::<CFNumber>() {
        return json!({
            "kind": "number",
            "value": number.to_f64().unwrap_or(f64::NAN),
        });
    }
    if let Some(flag) = value.downcast::<CFBoolean>() {
        return json!({ "kind": "bool", "value": bool::from(flag) });
    }
    if is_ax_element(value) {
        return json!({ "kind": "element" });
    }
    if let Some(array) = value.downcast::<CFArray<*const std::ffi::c_void>>() {
        return json!({ "kind": "array", "count": array.len() });
    }
    // AXValue that is not a CFRange (point, size, rect) lands here.
    json!({ "kind": "other" })
}
