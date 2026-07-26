//! Displays and DPI (§9).
//!
//! §9 is blunt about the failure this prevents: "declare **Per-Monitor-V2** DPI
//! awareness in the manifest and handle the logical/physical conversion
//! explicitly. Getting this wrong is the classic *blurry on the second monitor*
//! bug." The manifest is the real declaration — [`enable_per_monitor_v2`] is
//! only the belt-and-braces path for a build that somehow ships without one,
//! and it must run before the first window exists.
//!
//! Two §9 rules are load-bearing for callers of [`display_for_window`]:
//!
//! * Coordinates may be **negative** — a display left of or above the primary
//!   is ordinary, and clamping code that assumes non-negative origins puts the
//!   panel off-screen for those users.
//! * The scale factor must be recomputed on **every show**, never cached at
//!   creation, because the panel moves between displays constantly.

use aibo_core::types::{DisplayInfo, Rect};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};

use super::error::{WinResult, WindowsPlatformError};

/// `MONITORINFOF_PRIMARY`, declared locally to keep the import surface small.
const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;

/// The reference DPI Windows scales against.
const USER_DEFAULT_SCREEN_DPI: f64 = 96.0;

/// Opt the process into Per-Monitor-V2 DPI awareness.
///
/// Call once, before any window is created. Prefer the application manifest —
/// this API returns `ERROR_ACCESS_DENIED` if awareness was already set (which
/// is exactly what a correct manifest does), so an error here is usually good
/// news and is downgraded to a debug log by the caller.
pub(crate) fn enable_per_monitor_v2() -> WinResult<()> {
    // SAFETY: takes an opaque context value defined by the OS; no pointers of
    // ours are involved.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        .map_err(|e| WindowsPlatformError::win32("SetProcessDpiAwarenessContext", e))
}

/// The display containing `hwnd`, or the primary display when `hwnd` is null.
///
/// §9: never the mouse's display, never "the main one" — the display holding
/// the focused window.
pub(crate) fn display_for_window(hwnd: Option<HWND>) -> WinResult<DisplayInfo> {
    // SAFETY: `MonitorFromWindow`/`MonitorFromPoint` take a handle or a POINT
    // by value and always return a monitor because of the DEFAULTTO* flags.
    let monitor = unsafe {
        match hwnd {
            Some(h) if !h.0.is_null() => MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST),
            _ => MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY),
        }
    };
    describe(monitor)
}

/// Describe a monitor as a [`DisplayInfo`].
fn describe(monitor: HMONITOR) -> WinResult<DisplayInfo> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a live, correctly sized MONITORINFO with `cbSize` set,
    // which is the contract `GetMonitorInfoW` documents.
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info) };
    if !ok.as_bool() {
        return Err(WindowsPlatformError::win32_bare(
            "GetMonitorInfoW",
            "the monitor handle was not accepted",
        ));
    }

    let mut dpi_x = USER_DEFAULT_SCREEN_DPI as u32;
    let mut dpi_y = USER_DEFAULT_SCREEN_DPI as u32;
    // SAFETY: both out-parameters are live `u32`s for the duration of the call.
    if let Err(e) = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
    {
        // Not fatal: a 1.0 scale factor renders correctly on a 96-DPI display
        // and merely wrongly-sized elsewhere, which beats no panel at all.
        tracing::debug!(error = %e, "GetDpiForMonitor failed; assuming 96 DPI");
    }
    let scale_factor = f64::from(dpi_x) / USER_DEFAULT_SCREEN_DPI;

    Ok(DisplayInfo {
        // `HMONITOR` is stable for as long as the monitor is attached, which is
        // exactly the lifetime over which §9 wants to remember a placement.
        id: monitor.0 as usize as u64,
        bounds: rect(
            info.rcMonitor.left,
            info.rcMonitor.top,
            info.rcMonitor.right,
            info.rcMonitor.bottom,
        ),
        // `rcWork` is already the taskbar-excluded area, including auto-hiding
        // taskbars, which is the "visible frame" §9 clamps inside.
        visible_frame: rect(
            info.rcWork.left,
            info.rcWork.top,
            info.rcWork.right,
            info.rcWork.bottom,
        ),
        scale_factor,
        is_primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

// SPIKE: S1 — the coordinate space of `DisplayInfo::bounds`.
// These rectangles are **physical** pixels in the virtual-screen coordinate
// space, which is what Win32 reports to a Per-Monitor-V2 process. They are
// deliberately not divided by `scale_factor`: in a mixed-DPI layout the virtual
// desktop is not uniformly scalable, so dividing the origin by *this* monitor's
// factor produces an origin that is wrong for every monitor but one. The panel
// layer must convert per-monitor using `scale_factor`. Confirm against what
// iced 0.14 / winit actually expect from a window position before wiring
// placement, because the two conventions disagree and the symptom is a panel
// that lands on the correct monitor at the wrong place.
fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
    Rect {
        x: f64::from(left),
        y: f64::from(top),
        width: f64::from(right - left),
        height: f64::from(bottom - top),
    }
}
