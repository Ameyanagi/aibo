//! TCC, the settings deep link, and secure input detection (§8, §17).
//!
//! Two facts drive everything here:
//!
//! * **AX reads require Accessibility; `CGEventPost` does not.** They are two
//!   different TCC services that share one System Settings pane, so
//!   [`Permission::Accessibility`] and [`Permission::PostEvents`] are queried
//!   through completely different APIs.
//! * **`AXIsProcessTrustedWithOptions` prompts once.** After a denial the OS
//!   will not prompt again — the only remaining route is deep-linking to the
//!   pane and polling `AXIsProcessTrusted()` for the change (§17).

use std::sync::atomic::{AtomicBool, Ordering};

use accessibility_sys::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt,
};
use aibo_core::types::{Permission, PermissionStatus};
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSString, NSURL};

use super::error::{MacosError, MacosResult};

/// The System Settings pane for Accessibility (§17).
pub const ACCESSIBILITY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Remembers whether aibo has ever been trusted in this process lifetime, so a
/// later `false` can be reported as [`PermissionStatus::Revoked`] rather than
/// [`PermissionStatus::Denied`]. §17 wants those distinguished because the
/// former gets a recovery screen — a TCC reset after an update looks exactly
/// like this.
static WAS_TRUSTED: AtomicBool = AtomicBool::new(false);

/// Remembers that the one-shot TCC prompt has already been fired.
static PROMPTED: AtomicBool = AtomicBool::new(false);

// CoreGraphics 10.15+ event-tap access checks. `core-graphics` 0.24 does not
// bind these, and they are the only supported way to ask about the *PostEvents*
// TCC service without actually posting an event.
#[allow(unsafe_code)]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
}

// `IsSecureEventInputEnabled` lives in Carbon's HIToolbox. There is no modern
// replacement; it is still the documented way to learn that keystroke
// synthesis will be silently dropped (§8).
#[allow(unsafe_code)]
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

/// Is any process holding secure event input?
///
/// When true, both keystroke synthesis and AX reads fail **silently**, and
/// another app can leave the flag stuck globally — §8 requires this to be
/// checked and explained rather than discovered.
#[allow(unsafe_code)]
pub fn secure_input_active() -> bool {
    // SAFETY: no arguments, no pointers; the call only reads a global flag.
    unsafe { IsSecureEventInputEnabled() }
}

/// `AXIsProcessTrusted()` — cheap, non-prompting, safe to call on every panel
/// show as §17 requires.
#[allow(unsafe_code)]
pub fn is_trusted() -> bool {
    // SAFETY: no arguments.
    let trusted = unsafe { AXIsProcessTrusted() };
    if trusted {
        WAS_TRUSTED.store(true, Ordering::Relaxed);
    }
    trusted
}

/// Fire the one-shot TCC prompt.
///
/// Returns the trust state *at the moment of the call* — the user's answer
/// arrives later and must be picked up by polling [`is_trusted`].
#[allow(unsafe_code)]
pub fn prompt_for_accessibility() -> bool {
    let key = unsafe {
        // SAFETY: `kAXTrustedCheckOptionPrompt` is a framework-owned constant
        // that is valid for the process lifetime.
        CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt)
    };
    let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
    PROMPTED.store(true, Ordering::Relaxed);
    // SAFETY: `options` is a live CFDictionary for the duration of the call.
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) }
}

/// Open the Accessibility pane in System Settings.
///
/// This is the *only* remaining path once the user has denied once, because
/// the OS will not prompt a second time (§17).
pub fn open_accessibility_settings() -> MacosResult<()> {
    let url = NSURL::URLWithString(&NSString::from_str(ACCESSIBILITY_SETTINGS_URL))
        .ok_or_else(|| MacosError::Platform("could not build the settings URL".into()))?;
    if NSWorkspace::sharedWorkspace().openURL(&url) {
        Ok(())
    } else {
        Err(MacosError::Platform(
            "System Settings refused to open the Accessibility pane".into(),
        ))
    }
}

/// Status of one [`Permission`] (§8, §17).
pub fn status(permission: Permission) -> PermissionStatus {
    match permission {
        Permission::Accessibility => {
            if is_trusted() {
                PermissionStatus::Granted
            } else if WAS_TRUSTED.load(Ordering::Relaxed) {
                // Granted earlier in this process, gone now: a TCC reset, an
                // updater that changed signing identity, or a manual revoke.
                PermissionStatus::Revoked
            } else if PROMPTED.load(Ordering::Relaxed) {
                PermissionStatus::Denied
            } else {
                PermissionStatus::NotDetermined
            }
        }
        Permission::PostEvents => {
            // SAFETY: no arguments; the call only inspects TCC state.
            #[allow(unsafe_code)]
            let ok = unsafe { CGPreflightPostEventAccess() };
            if ok {
                PermissionStatus::Granted
            } else {
                // CoreGraphics does not distinguish "never asked" from
                // "denied"; treat it as not-determined so the UI offers the
                // request rather than a dead end.
                PermissionStatus::NotDetermined
            }
        }
        // Windows-only (UIPI / `uiAccess=true`), §8.
        Permission::ElevatedWindowAccess => PermissionStatus::NotApplicable,
        // SPIKE: S8 — `UNUserNotificationCenter` authorisation requires a
        // bundled, signed app; unbundled `cargo run` reports nothing useful.
        // Until the packaging chain exists this cannot be answered honestly.
        Permission::Notifications => PermissionStatus::NotDetermined,
        // SPIKE: S8 — `SMAppService.mainApp.status` likewise needs a real
        // bundle. Wiring it before packaging would return a value that is
        // wrong in development and right in release, which is worse than
        // admitting we do not know.
        Permission::Autostart => PermissionStatus::NotDetermined,
    }
}

/// Ask for a permission, or open the pane when asking is no longer possible.
pub fn request(permission: Permission) -> MacosResult<()> {
    match permission {
        Permission::Accessibility => {
            if is_trusted() {
                return Ok(());
            }
            if PROMPTED.load(Ordering::Relaxed) {
                // The OS will not prompt twice (§17).
                open_accessibility_settings()
            } else {
                prompt_for_accessibility();
                Ok(())
            }
        }
        Permission::PostEvents => {
            // SAFETY: no arguments; prompts at most once, like the AX variant.
            #[allow(unsafe_code)]
            let granted = unsafe { CGRequestPostEventAccess() };
            if granted {
                Ok(())
            } else {
                open_accessibility_settings()
            }
        }
        Permission::ElevatedWindowAccess => Ok(()),
        // SPIKE: S8 — see `status`.
        Permission::Notifications | Permission::Autostart => Ok(()),
    }
}

/// Guard used by every capture and insert path.
///
/// Returns [`MacosError::NotTrusted`] when the grant is missing, which the
/// caller maps to `CaptureFailure::Denied` / `InsertFailure::PermissionDenied`.
/// §17's degraded-mode matrix depends on this being an ordinary error rather
/// than a panic: without Accessibility, Ask/Compute/clipboard Transform must
/// keep working.
pub(crate) fn require_accessibility() -> MacosResult<()> {
    if is_trusted() {
        Ok(())
    } else {
        Err(MacosError::NotTrusted)
    }
}
