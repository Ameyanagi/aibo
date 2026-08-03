//! Win32 implementation of the transient panel-window boundary.

use std::ffi::c_void;
use std::mem::size_of;

use raw_window_handle::Win32WindowHandle;
use windows::Win32::Foundation::{
    ERROR_SUCCESS, GetLastError, HWND, LPARAM, LRESULT, POINT, SetLastError, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::Accessibility::{
    NotificationKind_Other, NotificationProcessing_MostRecent, UiaHostProviderFromHwnd,
    UiaRaiseNotificationEvent,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, HTCAPTION, HTCLIENT, HWND_TOPMOST, IsWindow,
    SET_WINDOW_POS_FLAGS, SPI_GETCLIENTAREAANIMATION, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowLongPtrW,
    SetWindowPos, SystemParametersInfoW, WINDOW_EX_STYLE, WM_NCDESTROY, WM_NCHITTEST,
    WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows_core::{BOOL, BSTR, Error, HRESULT};

use crate::overlay::{BackdropStatus, OverlayWindowConfiguration, OverlayWindowError};

/// The panel already reserves this much empty chrome above its first row.
/// Treating it as a caption matches macOS's movable-by-background panel while
/// leaving every actual control in the header interactive.
const DRAG_BAND_LOGICAL_PX: u32 = 16;
const PANEL_SUBCLASS_ID: usize = 0xA1B0_0001;

fn overlay_extended_style(current: WINDOW_EX_STYLE) -> WINDOW_EX_STYLE {
    let mut bits = current.0;
    bits |= WS_EX_TOOLWINDOW.0;
    bits &= !WS_EX_APPWINDOW.0;
    // Do not set WS_EX_NOACTIVATE. Initial presentation uses SWP_NOACTIVATE,
    // while an explicit user click must still be able to focus the text field.
    bits &= !WS_EX_NOACTIVATE.0;
    WINDOW_EX_STYLE(bits)
}

fn presentation_flags() -> SET_WINDOW_POS_FLAGS {
    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW
}

pub(crate) fn configure_panel_window(
    handle: Win32WindowHandle,
) -> Result<OverlayWindowConfiguration, OverlayWindowError> {
    let hwnd = hwnd(handle);
    validate_window(hwnd)?;
    install_panel_subclass(hwnd)?;
    let current = get_extended_style(hwnd)?;
    set_extended_style(hwnd, overlay_extended_style(current))?;

    // Make the style change visible to the shell without moving, resizing, or
    // activating the panel.
    set_window_position(
        hwnd,
        None,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        "refresh overlay window style",
    )?;

    // DWM must clip the window to rounded corners, or every square corner of
    // the rectangular HWND renders opaque — the backdrop below fills the
    // whole rect, and the chrome's own radius only paints *inside* it (owner
    // report, 2026-08-03: "the corners are not transparent"). Best-effort:
    // Windows 10 has no corner preference and degrades to square corners.
    let _ = set_dwm_attribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_ROUND);

    // Acrylic is available as DWMSBT_TRANSIENTWINDOW on current Windows 11.
    // Older builds return an unsupported-attribute HRESULT; that is cosmetic
    // and intentionally degrades to the transparent renderer background.
    let backdrop =
        if set_dwm_attribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &DWMSBT_TRANSIENTWINDOW).is_ok() {
            let dark = BOOL(1);
            let _ = set_dwm_attribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark);
            BackdropStatus::Applied
        } else {
            BackdropStatus::Unavailable
        };

    Ok(OverlayWindowConfiguration { backdrop })
}

pub(crate) fn present_panel_without_activation(
    handle: Win32WindowHandle,
) -> Result<(), OverlayWindowError> {
    let hwnd = hwnd(handle);
    validate_window(hwnd)?;
    set_window_position(
        hwnd,
        Some(HWND_TOPMOST),
        presentation_flags(),
        "present overlay without activation",
    )
}

#[allow(unsafe_code)]
pub(crate) fn reduced_motion_preferred() -> bool {
    // The setting answers whether client-area animation is enabled. A failed
    // query conservatively means "reduce motion".
    let mut animation_enabled = BOOL(1);
    // SAFETY: `animation_enabled` is a live, correctly sized BOOL output
    // buffer for SPI_GETCLIENTAREAANIMATION.
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut animation_enabled as *mut BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    result.is_err() || !animation_enabled.as_bool()
}

#[allow(unsafe_code)]
pub(crate) fn announce_accessibility(message: &str) {
    let Some(hwnd) = super::msgwin::notification_window() else {
        return;
    };
    let display = BSTR::from(message);
    let activity = BSTR::from("aibo");

    // SAFETY: the HWND belongs to aibo's process-wide notification window,
    // both BSTR values stay live through the call, and failures are explicitly
    // best-effort. Neither the content nor the error is logged.
    unsafe {
        let Ok(provider) = UiaHostProviderFromHwnd(hwnd) else {
            return;
        };
        let _ = UiaRaiseNotificationEvent(
            &provider,
            NotificationKind_Other,
            NotificationProcessing_MostRecent,
            &display,
            &activity,
        );
    }
}

fn hwnd(handle: Win32WindowHandle) -> HWND {
    HWND(handle.hwnd.get() as *mut c_void)
}

fn drag_band_height(dpi: u32) -> i32 {
    let dpi = dpi.max(96);
    (u64::from(DRAG_BAND_LOGICAL_PX) * u64::from(dpi)).div_ceil(96) as i32
}

fn is_drag_band_y(client_y: i32, dpi: u32) -> bool {
    (0..drag_band_height(dpi)).contains(&client_y)
}

#[allow(unsafe_code)]
fn install_panel_subclass(hwnd: HWND) -> Result<(), OverlayWindowError> {
    // SAFETY: `hwnd` was validated by the caller. The callback is a static
    // function, its subclass id is process-local and stable, and comctl32
    // automatically confines callback execution to the window's UI thread.
    unsafe { SetLastError(ERROR_SUCCESS) };
    if unsafe { SetWindowSubclass(hwnd, Some(panel_subclass_proc), PANEL_SUBCLASS_ID, 0) }.as_bool()
    {
        Ok(())
    } else {
        let error = unsafe { GetLastError() };
        Err(native_error(
            "install panel drag region",
            if error == ERROR_SUCCESS {
                "SetWindowSubclass returned false".to_owned()
            } else {
                Error::from_hresult(HRESULT::from_win32(error.0)).to_string()
            },
        ))
    }
}

#[allow(unsafe_code)]
unsafe extern "system" fn panel_subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _reference_data: usize,
) -> LRESULT {
    if message == WM_NCHITTEST {
        // Preserve any non-client answer supplied by winit first. For an
        // ordinary client hit, promote only the panel's otherwise-unused top
        // band to HTCAPTION so Windows owns dragging and snap gestures.
        let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
        if result.0 == HTCLIENT as isize {
            let packed = lparam.0 as u32;
            let mut point = POINT {
                x: (packed as u16 as i16) as i32,
                y: ((packed >> 16) as u16 as i16) as i32,
            };
            if unsafe { ScreenToClient(hwnd, &mut point) }.as_bool()
                && is_drag_band_y(point.y, unsafe { GetDpiForWindow(hwnd) })
            {
                return LRESULT(HTCAPTION as isize);
            }
        }
        return result;
    }

    if message == WM_NCDESTROY {
        // SAFETY: this exact callback/id pair was installed on `hwnd` above.
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(panel_subclass_proc), PANEL_SUBCLASS_ID) };
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[allow(unsafe_code)]
fn validate_window(hwnd: HWND) -> Result<(), OverlayWindowError> {
    // SAFETY: `IsWindow` only samples whether this opaque value currently
    // identifies a live window; it does not dereference caller memory.
    if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        Ok(())
    } else {
        Err(native_error(
            "validate overlay window",
            "the HWND is no longer valid".to_owned(),
        ))
    }
}

#[allow(unsafe_code)]
fn get_extended_style(hwnd: HWND) -> Result<WINDOW_EX_STYLE, OverlayWindowError> {
    // SAFETY: `hwnd` was validated immediately before this operation. Clearing
    // last-error disambiguates a valid zero style from API failure.
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let value = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let error = GetLastError();
        if value == 0 && error != ERROR_SUCCESS {
            Err(native_error(
                "read overlay window style",
                Error::from_hresult(HRESULT::from_win32(error.0)).to_string(),
            ))
        } else {
            Ok(WINDOW_EX_STYLE(value as u32))
        }
    }
}

#[allow(unsafe_code)]
fn set_extended_style(hwnd: HWND, style: WINDOW_EX_STYLE) -> Result<(), OverlayWindowError> {
    // SAFETY: `hwnd` was validated and GWL_EXSTYLE accepts an integer bitset.
    // Clearing last-error disambiguates a previous zero style from failure.
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let previous = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style.0 as isize);
        let error = GetLastError();
        if previous == 0 && error != ERROR_SUCCESS {
            Err(native_error(
                "set overlay window style",
                Error::from_hresult(HRESULT::from_win32(error.0)).to_string(),
            ))
        } else {
            Ok(())
        }
    }
}

#[allow(unsafe_code)]
fn set_window_position(
    hwnd: HWND,
    insert_after: Option<HWND>,
    flags: SET_WINDOW_POS_FLAGS,
    operation: &'static str,
) -> Result<(), OverlayWindowError> {
    // SAFETY: `hwnd` was validated, no geometry is consumed under NOMOVE and
    // NOSIZE, and `insert_after` is either a Win32 sentinel or null.
    unsafe { SetWindowPos(hwnd, insert_after, 0, 0, 0, 0, flags) }
        .map_err(|error| native_error(operation, error.to_string()))
}

#[allow(unsafe_code)]
fn set_dwm_attribute<T>(
    hwnd: HWND,
    attribute: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE,
    value: &T,
) -> windows_core::Result<()> {
    // SAFETY: DWM reads exactly `size_of::<T>()` bytes synchronously and does
    // not retain the pointer. Call sites pass the documented attribute type.
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attribute,
            (value as *const T).cast(),
            size_of::<T>() as u32,
        )
    }
}

fn native_error(operation: &'static str, reason: String) -> OverlayWindowError {
    OverlayWindowError::Native { operation, reason }
}

#[cfg(test)]
mod tests {
    use super::{drag_band_height, is_drag_band_y, overlay_extended_style, presentation_flags};
    use windows::Win32::UI::WindowsAndMessaging::{
        SWP_NOACTIVATE, SWP_SHOWWINDOW, WINDOW_EX_STYLE, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW,
    };

    #[test]
    fn utility_style_hides_from_shell_without_permanently_blocking_input() {
        let existing = WINDOW_EX_STYLE(WS_EX_APPWINDOW.0 | WS_EX_NOACTIVATE.0 | 0x0010_0000);
        let result = overlay_extended_style(existing);

        assert_ne!(result.0 & WS_EX_TOOLWINDOW.0, 0);
        assert_eq!(result.0 & WS_EX_APPWINDOW.0, 0);
        assert_eq!(result.0 & WS_EX_NOACTIVATE.0, 0);
        assert_ne!(result.0 & 0x0010_0000, 0);
    }

    #[test]
    fn presentation_is_visible_but_does_not_activate() {
        let flags = presentation_flags();
        assert!(flags.contains(SWP_SHOWWINDOW));
        assert!(flags.contains(SWP_NOACTIVATE));
    }

    #[test]
    fn drag_band_tracks_the_windows_scale_factor() {
        assert_eq!(drag_band_height(96), 16);
        assert_eq!(drag_band_height(144), 24);
        assert_eq!(drag_band_height(192), 32);
        assert!(is_drag_band_y(15, 96));
        assert!(!is_drag_band_y(16, 96));
        assert!(!is_drag_band_y(-1, 192));
    }
}
