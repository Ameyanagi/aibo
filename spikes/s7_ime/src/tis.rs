//! Which input source is selected — the *coarse* half of the answer.
//!
//! §9 is blunt: **"macOS has no clean cross-process API"** for composition
//! state. What macOS does have is Text Input Services, which reports which input
//! source is currently selected. That is a strictly weaker signal:
//!
//! - "Japanese-Romaji is selected" does **not** mean a composition is active.
//! - But "US ABC is selected" **does** mean one is not.
//!
//! §20's stated fallback for S7 is *"block insert whenever the source app is in
//! a known-IME state; document the limitation."* This module is exactly that
//! fallback's implementation, and the point of the spike is to find out whether
//! anything better exists before settling for it.
//!
//! `objc2` has no binding for TIS, so the four symbols are declared here. They
//! are stable public Carbon API, not SPI.

#![cfg(target_os = "macos")]

use std::ffi::c_void;

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::string::CFStringRef;

type TISInputSourceRef = *mut c_void;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    /// +1 reference to the currently selected keyboard input source.
    fn TISCopyCurrentKeyboardInputSource() -> TISInputSourceRef;
    /// +0 borrow of a property value; NULL when the property is absent.
    fn TISGetInputSourceProperty(
        input_source: TISInputSourceRef,
        property_key: CFStringRef,
    ) -> *mut c_void;

    static kTISPropertyInputSourceID: CFStringRef;
    static kTISPropertyLocalizedName: CFStringRef;
    static kTISPropertyInputSourceType: CFStringRef;
}

/// The selected input source.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InputSource {
    /// Reverse-DNS id, e.g. `com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese`.
    pub id: String,
    /// Localised name shown in the menu bar.
    pub name: String,
    /// `TISTypeKeyboardLayout` for a plain layout, `TISTypeKeyboardInputMode`
    /// or `…InputMethod` for an IME. **This distinction is the signal.**
    pub kind: String,
}

impl InputSource {
    /// Is this an input *method* rather than a plain keyboard layout?
    ///
    /// A layout (US, JIS, Dvorak) cannot compose. An input mode or input method
    /// can, so from a plain layout aibo may insert freely, and from an IME it
    /// must apply §9's rules.
    pub fn can_compose(&self) -> bool {
        !self.kind.contains("KeyboardLayout")
    }

    /// Is this one of the CJK input methods §9 calls a *daily-use hazard*?
    ///
    /// The id prefixes are matched rather than the name, because the name is
    /// localised and the id is not.
    pub fn is_cjk(&self) -> bool {
        const CJK: &[&str] = &[
            "com.apple.inputmethod.Kotoeri",  // Japanese
            "com.apple.inputmethod.Japanese", // Japanese, older ids
            "com.apple.inputmethod.SCIM",     // Simplified Chinese
            "com.apple.inputmethod.TCIM",     // Traditional Chinese
            "com.apple.inputmethod.TYIM",     // Cangjie etc.
            "com.apple.inputmethod.Korean",   // Korean
            "com.google.inputmethod",         // Google Japanese Input
            "org.pqrs",                       // third-party
            "com.justsystems",                // ATOK
        ];
        CJK.iter().any(|prefix| self.id.starts_with(prefix))
    }
}

/// Read the currently selected keyboard input source.
///
/// This is a **process-global** query, not a per-application one — it reports
/// what the *system* has selected, which follows the frontmost app. That is
/// convenient here (the spike is never frontmost when it matters) and is a real
/// limitation for the product: it tells you nothing about a background app.
pub fn current() -> Option<InputSource> {
    // SAFETY: no arguments; returns +1 or null.
    let source = unsafe { TISCopyCurrentKeyboardInputSource() };
    if source.is_null() {
        return None;
    }
    // SAFETY: `source` is a +1 CFType; released once at the end of this scope.
    let _guard = Releaser(source);

    // SAFETY: `source` is live and the three keys are the framework's own
    // statics. `TISGetInputSourceProperty` returns a +0 borrow valid while
    // `source` is alive, so each is wrapped under the GET rule.
    let read = |key: CFStringRef| -> Option<String> {
        let raw = unsafe { TISGetInputSourceProperty(source, key) };
        if raw.is_null() {
            return None;
        }
        Some(unsafe { CFString::wrap_under_get_rule(raw as CFStringRef) }.to_string())
    };

    // SAFETY: reading framework statics.
    let (id_key, name_key, type_key) = unsafe {
        (
            kTISPropertyInputSourceID,
            kTISPropertyLocalizedName,
            kTISPropertyInputSourceType,
        )
    };

    Some(InputSource {
        id: read(id_key)?,
        name: read(name_key).unwrap_or_else(|| "?".to_owned()),
        kind: read(type_key).unwrap_or_else(|| "?".to_owned()),
    })
}

/// Releases a +1 CFType on drop.
struct Releaser(*mut c_void);

impl Drop for Releaser {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: held a +1 reference, released exactly once.
            unsafe { core_foundation_sys::base::CFRelease(self.0.cast()) };
        }
    }
}
