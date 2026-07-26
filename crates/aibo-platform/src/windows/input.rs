//! Synthetic input and focus restoration (§8).
//!
//! Two jobs:
//!
//! * **Insert.** §8's Windows column is "clipboard + `Ctrl+V`; `SendInput` per
//!   UTF-16 unit for short". Both live here. Note the unit: `KEYEVENTF_UNICODE`
//!   carries a single UTF-16 code unit, so an astral-plane character (emoji) is
//!   *two* events, and splitting a surrogate pair across a batch boundary emits
//!   garbage.
//! * **Focus.** §8 requires `restore_focus` to **confirm** the target regained
//!   focus before pasting, with a bounded retry. This module supplies one
//!   attempt; the retry loop and its sleeps live on the async side so nothing
//!   blocks.
//!
//! UIPI shows up here as `SendInput` returning fewer events than it was given
//! with `ERROR_ACCESS_DENIED`. That is reported, never ignored — see
//! [`WindowsPlatformError::UipiBlocked`].

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, GetLastError, HWND};
use windows::Win32::System::Threading::AttachThreadInput;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput, SetFocus, VIRTUAL_KEY, VK_CONTROL, VK_LWIN,
    VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
    SetForegroundWindow,
};

use super::error::{WinResult, WindowsPlatformError};

/// Virtual key for `V`. `VK_V` has no named constant in the Win32 metadata.
pub(crate) const VK_V: VIRTUAL_KEY = VIRTUAL_KEY(0x56);
/// Virtual key for `C`.
pub(crate) const VK_C: VIRTUAL_KEY = VIRTUAL_KEY(0x43);

/// Maximum number of characters worth sending as keystrokes rather than as a
/// paste. §8 calls `SendInput` the "short insert" path only; beyond this the
/// event storm is slow and far more likely to interleave with real typing.
pub(crate) const KEYSTROKE_MAX_CHARS: usize = 64;

fn key_event(vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_event(unit: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Push a batch of synthesised events, turning the two silent failure modes
/// into errors.
///
/// `SendInput` returning a short count with `ERROR_ACCESS_DENIED` is UIPI: the
/// foreground window is owned by a higher-integrity process and a
/// non-`uiAccess` build cannot type into it (§8).
fn send(inputs: &[INPUT]) -> WinResult<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: `inputs` is a live slice of correctly initialised `INPUT` values
    // and `cbSize` is the exact size of one element, as the API requires.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        return Ok(());
    }
    // SAFETY: reading the calling thread's last-error value; no pointers.
    let last = unsafe { GetLastError() };
    if last == ERROR_ACCESS_DENIED {
        Err(WindowsPlatformError::UipiBlocked {
            pid: foreground_pid().unwrap_or(0),
        })
    } else {
        Err(WindowsPlatformError::win32_bare(
            "SendInput",
            format!("inserted {sent} of {} events ({last:?})", inputs.len()),
        ))
    }
}

/// Process id owning the foreground window, if there is one.
pub(crate) fn foreground_pid() -> Option<u32> {
    // SAFETY: both calls take no pointers except `&mut pid`, which is valid.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (pid != 0).then_some(pid)
    }
}

/// The foreground window handle, or `None` when the desktop has no foreground
/// window (screen locked, a UAC prompt on the secure desktop, …).
pub(crate) fn foreground_window() -> Option<HWND> {
    // SAFETY: no arguments, no pointers.
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.0.is_null()).then_some(hwnd)
}

/// The window holding keyboard focus **inside another application's UI thread**.
///
/// §7's deferred capture reads the app snapshotted on hotkey-down, and by then
/// aibo's panel owns the foreground — so `GetFocus` (which only ever answers
/// about the calling thread) and UIA's global `GetFocusedElement` both describe
/// aibo. `GetGUIThreadInfo` is the one Win32 API that answers "which window has
/// focus *in that thread*", which is what the parameter is for.
///
/// `hwndCaret` and `hwndActive` are consulted in that order as fallbacks: a
/// thread whose focus window is null may still have a caret, and an active
/// window is a better subject than nothing.
pub(crate) fn focus_window_for(hwnd: HWND) -> Option<HWND> {
    if hwnd.0.is_null() {
        return None;
    }
    // SAFETY: `hwnd` was handed to us by the OS. `info` is a live, correctly
    // sized out-parameter, which `cbSize` declares as the API requires.
    unsafe {
        let thread = GetWindowThreadProcessId(hwnd, None);
        if thread == 0 {
            return None;
        }
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(thread, &mut info).ok()?;
        [info.hwndFocus, info.hwndCaret, info.hwndActive]
            .into_iter()
            .find(|h| !h.0.is_null())
    }
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: `GetAsyncKeyState` takes a plain integer and returns a bitfield.
    let state = unsafe { GetAsyncKeyState(vk.0 as i32) };
    (state as u16 & 0x8000) != 0
}

/// Release any modifier the user is still physically holding.
///
/// aibo is driven by a global hotkey, so at the moment of an insert the user is
/// very often still holding `Ctrl+Shift` from `Ctrl+Shift+Space` (§9). Sending
/// a paste chord underneath a held modifier produces `Ctrl+Shift+V` — "paste
/// without formatting", or nothing at all, depending on the app.
pub(crate) fn release_held_modifiers() -> WinResult<()> {
    let mut events = Vec::new();
    for vk in [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN] {
        if key_down(vk) {
            events.push(key_event(vk, KEYEVENTF_KEYUP));
        }
    }
    send(&events)
}

/// Synthesise `modifier + key` (used for `Ctrl+V` and the `Ctrl+C` selection
/// fallback).
pub(crate) fn send_chord(modifier: VIRTUAL_KEY, key: VIRTUAL_KEY) -> WinResult<()> {
    send(&[
        key_event(modifier, KEYBD_EVENT_FLAGS(0)),
        key_event(key, KEYBD_EVENT_FLAGS(0)),
        key_event(key, KEYEVENTF_KEYUP),
        key_event(modifier, KEYEVENTF_KEYUP),
    ])
}

/// Type `text` directly, one event pair per UTF-16 code unit.
///
/// Short inserts only — see [`KEYSTROKE_MAX_CHARS`]. Surrogate pairs are kept
/// together in one batch so a split can never emit a lone surrogate.
pub(crate) fn send_unicode(text: &str) -> WinResult<()> {
    let mut events = Vec::with_capacity(text.len() * 2);
    let mut buf = [0u16; 2];
    for ch in text.chars() {
        for unit in ch.encode_utf16(&mut buf) {
            events.push(unicode_event(*unit, KEYBD_EVENT_FLAGS(0)));
            events.push(unicode_event(*unit, KEYEVENTF_KEYUP));
        }
    }
    send(&events)
}

/// One attempt at giving focus back to `hwnd`; returns whether it landed.
///
/// The `AttachThreadInput` dance is the standard remedy for Windows' foreground
/// lock: `SetForegroundWindow` is refused unless the calling thread shares an
/// input queue with the current foreground thread. It is best-effort by design
/// — which is exactly why §8 demands the caller *confirm* rather than assume.
pub(crate) fn try_restore_focus(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    // SAFETY: every call takes an HWND we were handed by the OS plus plain
    // integers. `AttachThreadInput` is paired with its detach on both paths.
    unsafe {
        let current = GetCurrentThreadId();
        let target = GetWindowThreadProcessId(hwnd, None);
        let attached =
            target != 0 && target != current && AttachThreadInput(current, target, true).as_bool();

        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));

        if attached {
            let _ = AttachThreadInput(current, target, false);
        }

        GetForegroundWindow() == hwnd
    }
}
