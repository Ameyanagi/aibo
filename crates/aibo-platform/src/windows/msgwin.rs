//! The hidden message window: clipboard-change and power/display notifications.
//!
//! One window, one thread, one message loop. It exists because three things
//! aibo needs are push-only:
//!
//! * `WM_CLIPBOARDUPDATE` via `AddClipboardFormatListener` — the supported
//!   replacement for the old clipboard-viewer chain, and what §12's
//!   save-and-restore race check keys off: a restore is only safe if nothing
//!   *else* wrote the clipboard in between.
//! * `WM_POWERBROADCAST` — §13 needs sleep/wake so the connection pool is
//!   re-warmed; after a lid-close the pooled HTTPS connections are dead and the
//!   first hotkey of the day misses the latency budget.
//! * `WM_DISPLAYCHANGE` — §9 needs to re-clamp the panel when a display goes
//!   away or is reconfigured.
//!
//! **The window is a hidden top-level window, not a message-only
//! (`HWND_MESSAGE`) window.** That is deliberate: message-only windows are
//! excluded from broadcast messages, so `WM_POWERBROADCAST` and
//! `WM_DISPLAYCHANGE` would never arrive. It is created `WS_POPUP`, 0×0, and
//! never shown.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};

use aibo_core::types::PowerEvent;
use tokio::sync::broadcast;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, GetClipboardSequenceNumber,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, RegisterClassW,
    TranslateMessage, WINDOW_EX_STYLE, WM_CLIPBOARDUPDATE, WM_DISPLAYCHANGE, WM_POWERBROADCAST,
    WNDCLASSW, WS_POPUP,
};
use windows_core::w;

/// `PBT_APMSUSPEND`. Declared locally rather than imported: the `PBT_*`
/// constants have moved between namespaces across `windows` releases and a
/// wrong import is a build break for a three-line value.
const PBT_APMSUSPEND: usize = 0x0004;
/// `PBT_APMRESUMESUSPEND`.
const PBT_APMRESUMESUSPEND: usize = 0x0007;
/// `PBT_APMRESUMEAUTOMATIC`.
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

/// Broadcast fan-out for the notification window.
struct Hub {
    power: broadcast::Sender<PowerEvent>,
    /// Payload is the OS clipboard sequence number observed at the time of the
    /// change, so a waiter can tell *which* change it saw (§12).
    clipboard: broadcast::Sender<u32>,
}

static HUB: OnceLock<Hub> = OnceLock::new();

/// Last clipboard sequence number seen by the listener. Read by the `Ctrl+C`
/// selection fallback to detect that the copy actually landed rather than
/// polling blindly (§8).
static LAST_SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// Native handle of the process-wide notification window.
///
/// Stored as an integer because `HWND` is an opaque pointer wrapper and is not
/// `Sync`; the window is never destroyed during normal process lifetime.
static WINDOW_HANDLE: AtomicIsize = AtomicIsize::new(0);

fn hub() -> &'static Hub {
    HUB.get_or_init(|| {
        let (power, _) = broadcast::channel(16);
        let (clipboard, _) = broadcast::channel(16);
        let hub = Hub { power, clipboard };
        std::thread::Builder::new()
            .name("aibo-win-msgwin".into())
            .spawn(message_loop)
            .expect("failed to spawn the aibo notification-window thread");
        hub
    })
}

/// Start the notification window if it is not already running.
///
/// Failures are logged rather than returned: none of the three notifications is
/// required for correctness, only for freshness, and refusing to start aibo
/// because a hidden window would not register would be a worse trade.
pub(crate) fn ensure_started() {
    let _ = hub();
}

/// Subscribe to sleep/wake/display transitions (§13).
pub(crate) fn power_events() -> broadcast::Receiver<PowerEvent> {
    hub().power.subscribe()
}

/// Subscribe to clipboard-change notifications, carrying the sequence number.
pub(crate) fn clipboard_updates() -> broadcast::Receiver<u32> {
    hub().clipboard.subscribe()
}

/// The most recent clipboard sequence number the listener saw.
///
/// Kept as the poll-free alternative to `GetClipboardSequenceNumber` for code
/// that must not take the clipboard lock; the `Ctrl+C` fallback in
/// [`super::WindowsBackend`] uses [`clipboard_updates`] instead, because it
/// needs to *wait* rather than sample.
#[allow(dead_code, reason = "sampling counterpart to clipboard_updates")]
pub(crate) fn last_clipboard_sequence() -> u32 {
    LAST_SEQUENCE.load(Ordering::Acquire)
}

/// The process-wide notification window, once its worker has created it.
///
/// Used as the provider identity for best-effort UI Automation announcements.
pub(crate) fn notification_window() -> Option<HWND> {
    let raw = WINDOW_HANDLE.load(Ordering::Acquire);
    (raw != 0).then_some(HWND(raw as *mut std::ffi::c_void))
}

fn message_loop() {
    let Some(hwnd) = create_window() else {
        tracing::error!(
            "aibo: notification window could not be created; clipboard-change, sleep/wake and display-change notifications are unavailable"
        );
        return;
    };
    WINDOW_HANDLE.store(hwnd.0 as isize, Ordering::Release);

    // SAFETY: `hwnd` was just created by this thread and is still alive.
    if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
        tracing::warn!(error = %e, "AddClipboardFormatListener failed; clipboard changes will not be observed");
    }

    let mut msg = MSG::default();
    // SAFETY: `msg` is a live, correctly sized message struct; the loop ends
    // when `GetMessageW` returns 0 (WM_QUIT) or -1 (error).
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn create_window() -> Option<HWND> {
    // SAFETY: registering a window class and creating a hidden 0x0 popup. All
    // pointers come from `w!` string literals with static lifetime.
    unsafe {
        let module = GetModuleHandleW(None).ok()?;
        let class = w!("AiboNotificationWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: HINSTANCE(module.0),
            lpszClassName: class,
            ..Default::default()
        };
        // A non-zero atom, or a class that is already registered from a
        // previous run of this thread; both are fine.
        let _ = RegisterClassW(&wc);

        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("aibo"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(HINSTANCE(module.0)),
            None,
        )
        .ok()
    }
}

/// Window procedure. Runs on the notification thread only.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            // SAFETY: no arguments, no pointers.
            let seq = unsafe { GetClipboardSequenceNumber() };
            LAST_SEQUENCE.store(seq, Ordering::Release);
            if let Some(h) = HUB.get() {
                let _ = h.clipboard.send(seq);
            }
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            let event = match wparam.0 {
                PBT_APMSUSPEND => Some(PowerEvent::WillSleep),
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => Some(PowerEvent::DidWake),
                _ => None,
            };
            if let (Some(event), Some(h)) = (event, HUB.get()) {
                let _ = h.power.send(event);
            }
            LRESULT(1)
        }
        WM_DISPLAYCHANGE => {
            if let Some(h) = HUB.get() {
                let _ = h.power.send(PowerEvent::DisplaysChanged);
            }
            LRESULT(0)
        }
        // SAFETY: forwarding the message untouched to the default handler.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
