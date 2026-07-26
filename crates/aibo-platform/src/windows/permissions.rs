//! Windows permissions, integrity levels and autostart (§8, §17).
//!
//! §8: the Windows permission story is **not "none"**. There is no TCC prompt,
//! but UIPI silently blocks reads and synthetic input against windows owned by
//! a higher-integrity process, and `uiAccess=true` — the only sanctioned way
//! around it — requires Authenticode signing *and* installation under
//! `%ProgramFiles%`. That combination cannot be granted at runtime, which is
//! why [`Permission::ElevatedWindowAccess`] reports
//! [`PermissionStatus::Restricted`] rather than `NotDetermined`: it is not a
//! prompt the user has yet to see, it is a property of how aibo was built and
//! installed.

use std::sync::atomic::{AtomicBool, Ordering};

use aibo_core::types::{Permission, PermissionStatus};
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE};
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation, TokenUIAccess,
};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows_core::{PCWSTR, w};

use super::error::{WinResult, WindowsPlatformError};

/// Registered so Windows toasts have an identity to attribute (§8: "needs
/// registered AppUserModelID"). Also the Run-key value name.
const APP_USER_MODEL_ID: PCWSTR = w!("com.aibo.Aibo");
/// Value name under the Run key.
const RUN_VALUE_NAME: PCWSTR = w!("aibo");
/// The per-user autostart key (§8: "Run registry key").
const RUN_KEY_PATH: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

static AUMID_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Read a boolean-ish token information class for the current process.
///
/// Isolated into one `unsafe fn` because both call sites want the same shape:
/// open our own token, ask for a 4-byte class, close the token.
///
/// # Safety
/// `class` must be a `TOKEN_INFORMATION_CLASS` whose payload is a struct of
/// exactly `size_of::<TOKEN_ELEVATION>()` bytes whose first field is a `u32`
/// flag — true for `TokenElevation` and `TokenUIAccess`.
unsafe fn current_token_flag(
    class: windows::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> Option<bool> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut value = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let result = GetTokenInformation(
            token,
            class,
            Some(std::ptr::from_mut(&mut value).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        let _ = CloseHandle(token);
        result.ok()?;
        Some(value.TokenIsElevated != 0)
    }
}

/// Is aibo itself running elevated?
pub(crate) fn is_elevated() -> bool {
    // SAFETY: `TokenElevation`'s payload is `TOKEN_ELEVATION`.
    unsafe { current_token_flag(TokenElevation) }.unwrap_or(false)
}

/// Does aibo's token carry `uiAccess`? Only true for a signed build installed
/// under `%ProgramFiles%` with `uiAccess=true` in its manifest (§8).
pub(crate) fn has_ui_access() -> bool {
    // SAFETY: `TokenUIAccess`'s payload is a single `DWORD`, which
    // `TOKEN_ELEVATION` matches in size and layout.
    unsafe { current_token_flag(TokenUIAccess) }.unwrap_or(false)
}

/// Can aibo drive windows owned by elevated processes at all?
pub(crate) fn can_cross_uipi() -> bool {
    has_ui_access() || is_elevated()
}

/// Cheap probe for "is this process out of reach because of UIPI?".
///
/// A non-elevated process is refused even `PROCESS_QUERY_LIMITED_INFORMATION`
/// on a higher-integrity target, so `ERROR_ACCESS_DENIED` here is a reliable
/// signal — and much cheaper than discovering it after a `SendInput` silently
/// did nothing.
pub(crate) fn process_is_out_of_reach(pid: u32) -> bool {
    if pid == 0 || can_cross_uipi() {
        return false;
    }
    // SAFETY: `OpenProcess` takes plain integers; the handle is closed on the
    // success path.
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => {
                let _ = CloseHandle(handle);
                false
            }
            Err(e) => e.code() == ERROR_ACCESS_DENIED.to_hresult(),
        }
    }
}

/// Full path to aibo's own executable, for the Run key.
fn executable_path() -> WinResult<String> {
    // Heap, not stack: extended-length paths run to 32 767 wide characters and
    // this is called from the UI thread.
    let mut buf = vec![0u16; 4096];
    // SAFETY: `buf` is a live, correctly sized UTF-16 buffer.
    let len = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    if len == 0 || len >= buf.len() {
        return Err(WindowsPlatformError::win32_bare(
            "GetModuleFileNameW",
            "could not resolve the executable path",
        ));
    }
    Ok(String::from_utf16_lossy(&buf[..len]))
}

/// Open `HKCU\...\Run` with the requested access.
fn open_run_key(write: bool) -> WinResult<HKEY> {
    let mut key = HKEY::default();
    let access = if write {
        KEY_READ | KEY_WRITE
    } else {
        KEY_READ
    };
    // SAFETY: `RUN_KEY_PATH` is a static wide string; `key` is a live out-param.
    // SPIKE: the `windows` crate has changed `RegOpenKeyExW`'s `uloptions`
    // between `u32` and `Option<u32>` across releases; adjust on first build.
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY_PATH, None, access, &mut key) };
    status
        .ok()
        .map_err(|e| WindowsPlatformError::win32("RegOpenKeyExW", e))?;
    Ok(key)
}

/// Is aibo registered to launch at login?
pub(crate) fn autostart_enabled() -> bool {
    let Ok(key) = open_run_key(false) else {
        return false;
    };
    let mut size = 0u32;
    // SAFETY: asking only for the value's size, so no data pointer is passed.
    let status =
        unsafe { RegQueryValueExW(key, RUN_VALUE_NAME, None, None, None, Some(&mut size)) };
    // SAFETY: `key` came from `RegOpenKeyExW` and is not used afterwards.
    let _ = unsafe { RegCloseKey(key) };
    status.is_ok()
}

/// Register (or unregister) aibo for launch at login.
pub(crate) fn set_autostart(enabled: bool) -> WinResult<()> {
    let key = open_run_key(true)?;
    let status = if enabled {
        let path = executable_path()?;
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes =
            unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
        // SAFETY: `bytes` aliases `wide`, which outlives the call, and its
        // length is the exact byte length of the NUL-terminated wide string.
        unsafe { RegSetValueExW(key, RUN_VALUE_NAME, None, REG_SZ, Some(bytes)) }
    } else {
        // SAFETY: `key` is open for writing and the value name is static.
        unsafe { RegDeleteValueW(key, RUN_VALUE_NAME) }
    };
    // SAFETY: `key` came from `RegOpenKeyExW` and is not used afterwards.
    let _ = unsafe { RegCloseKey(key) };
    status
        .ok()
        .map_err(|e| WindowsPlatformError::win32("RegSetValueExW", e))
}

/// Give the process an explicit AppUserModelID so toasts are attributed to
/// aibo rather than to the host process (§8).
pub(crate) fn register_app_user_model_id() -> WinResult<()> {
    // SAFETY: takes a static wide string.
    unsafe { SetCurrentProcessExplicitAppUserModelID(APP_USER_MODEL_ID) }
        .map_err(|e| WindowsPlatformError::win32("SetCurrentProcessExplicitAppUserModelID", e))?;
    AUMID_REGISTERED.store(true, Ordering::Release);
    Ok(())
}

/// [`PlatformBackend::permission_status`] for Windows.
///
/// [`PlatformBackend::permission_status`]: aibo_core::traits::PlatformBackend::permission_status
pub(crate) fn status(p: Permission) -> PermissionStatus {
    match p {
        // Both are macOS TCC services (§8) and mean nothing here.
        Permission::Accessibility | Permission::PostEvents => PermissionStatus::NotApplicable,
        Permission::ElevatedWindowAccess => {
            if can_cross_uipi() {
                PermissionStatus::Granted
            } else {
                // Not `Denied`: the user never refused anything. It is a
                // property of the build and install location (§8), so the UI
                // must explain rather than offer a retry.
                PermissionStatus::Restricted
            }
        }
        Permission::Notifications => {
            if AUMID_REGISTERED.load(Ordering::Acquire) {
                PermissionStatus::Granted
            } else {
                PermissionStatus::NotDetermined
            }
        }
        Permission::Autostart => {
            if autostart_enabled() {
                PermissionStatus::Granted
            } else {
                PermissionStatus::NotDetermined
            }
        }
    }
}

/// [`PlatformBackend::request_permission`] for Windows.
///
/// [`PlatformBackend::request_permission`]: aibo_core::traits::PlatformBackend::request_permission
pub(crate) fn request(p: Permission) -> WinResult<()> {
    match p {
        Permission::Accessibility | Permission::PostEvents => Ok(()),
        Permission::Notifications => register_app_user_model_id(),
        Permission::Autostart => set_autostart(true),
        // Deliberately an error: there is no runtime path to `uiAccess`, and
        // silently returning `Ok` would let the onboarding flow (§17) claim it
        // asked for something it cannot obtain.
        Permission::ElevatedWindowAccess => Err(WindowsPlatformError::UipiBlocked { pid: 0 }),
    }
}
