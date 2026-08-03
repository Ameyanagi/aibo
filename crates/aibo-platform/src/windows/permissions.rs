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

use aibo_core::types::{Permission, PermissionStatus};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_ELEVATION,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenElevation, TokenIntegrityLevel, TokenUIAccess,
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

/// A kernel handle whose ownership belongs to this module.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the constructors below only wrap handles returned by APIs
        // that transfer ownership to the caller.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct OwnedRegKey(HKEY);

impl Drop for OwnedRegKey {
    fn drop(&mut self) {
        // SAFETY: only keys returned by RegOpenKeyExW are wrapped.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// Read a boolean-ish token information class for the current process.
///
/// Isolated into one `unsafe fn` because both call sites want the same shape:
/// open our own token and ask for a 4-byte class.
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
        let token = OwnedHandle(token);
        let mut value = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let result = GetTokenInformation(
            token.0,
            class,
            Some(std::ptr::from_mut(&mut value).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
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

/// Read the mandatory integrity RID from a token.
fn token_integrity_rid(token: HANDLE) -> Option<u32> {
    let mut size = 0u32;
    // SAFETY: the first call intentionally supplies no buffer to learn its
    // required size. The return value is expected to be an insufficient-buffer
    // error; a non-zero size is the useful result.
    let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut size) };
    if size < std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 {
        return None;
    }
    // `Vec<u8>` is not guaranteed to be aligned for TOKEN_MANDATORY_LABEL.
    // Machine-word storage is sufficiently aligned for the structure and SID
    // pointer while still providing a contiguous byte buffer to Win32.
    let word = std::mem::size_of::<usize>();
    let words = (size as usize).div_ceil(word);
    let mut storage = vec![0usize; words];
    // SAFETY: `storage` is live and exactly as large as the API requested.
    unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(storage.as_mut_ptr().cast()),
            size,
            &mut size,
        )
        .ok()?;
        let label = &*storage.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let count = GetSidSubAuthorityCount(label.Label.Sid);
        if count.is_null() || *count == 0 {
            return None;
        }
        let rid = GetSidSubAuthority(label.Label.Sid, u32::from(*count) - 1);
        (!rid.is_null()).then(|| *rid)
    }
}

fn current_integrity_rid() -> Option<u32> {
    // SAFETY: the pseudo process handle remains valid; OpenProcessToken returns
    // an owned real handle which the guard closes.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let token = OwnedHandle(token);
        token_integrity_rid(token.0)
    }
}

fn process_integrity_rid(pid: u32) -> Option<u32> {
    // SAFETY: pid is a plain integer. Both returned handles are owned and
    // closed by their guards.
    unsafe {
        let process = OwnedHandle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?);
        let mut token = HANDLE::default();
        OpenProcessToken(process.0, TOKEN_QUERY, &mut token).ok()?;
        let token = OwnedHandle(token);
        token_integrity_rid(token.0)
    }
}

const fn integrity_is_out_of_reach(current: u32, target: u32, ui_access: bool) -> bool {
    !ui_access && target > current
}

/// Is this process out of reach because its mandatory integrity level is above
/// ours? `OpenProcess` failure alone is not evidence of UIPI: protected
/// processes, ACLs, and exited processes can all produce the same error.
pub(crate) fn process_is_out_of_reach(pid: u32) -> bool {
    if pid == 0 || has_ui_access() {
        return false;
    }
    current_integrity_rid()
        .zip(process_integrity_rid(pid))
        .is_some_and(|(current, target)| integrity_is_out_of_reach(current, target, false))
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
fn open_run_key(write: bool) -> WinResult<OwnedRegKey> {
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
    Ok(OwnedRegKey(key))
}

/// Is aibo registered to launch at login?
pub(crate) fn autostart_enabled() -> bool {
    let Ok(key) = open_run_key(false) else {
        return false;
    };
    let mut size = 0u32;
    let mut kind = REG_SZ;
    // SAFETY: asking only for the value's size, so no data pointer is passed.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            RUN_VALUE_NAME,
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        )
    };
    if status.is_err() || kind != REG_SZ || size < 2 || !size.is_multiple_of(2) {
        return false;
    }
    let mut bytes = vec![0u8; size as usize];
    // SAFETY: `bytes` is live for the exact byte count returned above.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            RUN_VALUE_NAME,
            None,
            Some(&mut kind),
            Some(bytes.as_mut_ptr()),
            Some(&mut size),
        )
    };
    if status.is_err() || kind != REG_SZ || size < 2 || !size.is_multiple_of(2) {
        return false;
    }
    bytes.truncate(size as usize);
    let mut wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if wide.last() == Some(&0) {
        wide.pop();
    }
    let Ok(stored) = String::from_utf16(&wide) else {
        return false;
    };
    executable_path().is_ok_and(|path| stored == quote_run_command(&path))
}

/// Register (or unregister) aibo for launch at login.
pub(crate) fn set_autostart(enabled: bool) -> WinResult<()> {
    let key = open_run_key(true)?;
    let status = if enabled {
        let path = executable_path()?;
        // Run-key values are command lines, not paths. Quoting is mandatory for
        // the normal Program Files installation and prevents Windows from
        // interpreting a space-delimited prefix as the executable.
        let command = quote_run_command(&path);
        let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes =
            unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
        // SAFETY: `bytes` aliases `wide`, which outlives the call, and its
        // length is the exact byte length of the NUL-terminated wide string.
        unsafe { RegSetValueExW(key.0, RUN_VALUE_NAME, None, REG_SZ, Some(bytes)) }
    } else {
        // SAFETY: `key` is open for writing and the value name is static.
        unsafe { RegDeleteValueW(key.0, RUN_VALUE_NAME) }
    };
    status
        .ok()
        .map_err(|e| WindowsPlatformError::win32("RegSetValueExW", e))
}

fn quote_run_command(path: &str) -> String {
    format!("\"{path}\"")
}

/// Give the process an explicit AppUserModelID so toasts are attributed to
/// aibo rather than to the host process (§8).
pub(crate) fn register_app_user_model_id() -> WinResult<()> {
    // SAFETY: takes a static wide string.
    unsafe { SetCurrentProcessExplicitAppUserModelID(APP_USER_MODEL_ID) }
        .map_err(|e| WindowsPlatformError::win32("SetCurrentProcessExplicitAppUserModelID", e))?;
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
        // Registering an AppUserModelID gives a toast an identity; it does not
        // prove Windows notifications are enabled for that identity. Until a
        // real notification-settings query exists, do not claim `Granted`.
        Permission::Notifications => PermissionStatus::NotDetermined,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_key_command_quotes_paths_with_spaces() {
        assert_eq!(
            quote_run_command(r"C:\Program Files\aibo\aibo.exe"),
            r#""C:\Program Files\aibo\aibo.exe""#
        );
    }

    #[test]
    fn integrity_comparison_only_blocks_higher_targets_without_ui_access() {
        assert!(integrity_is_out_of_reach(0x2000, 0x3000, false));
        assert!(!integrity_is_out_of_reach(0x3000, 0x3000, false));
        assert!(!integrity_is_out_of_reach(0x4000, 0x3000, false));
        assert!(!integrity_is_out_of_reach(0x2000, 0x3000, true));
    }

    #[test]
    fn registering_identity_is_not_reported_as_notification_permission() {
        assert_eq!(
            status(Permission::Notifications),
            PermissionStatus::NotDetermined
        );
    }
}
