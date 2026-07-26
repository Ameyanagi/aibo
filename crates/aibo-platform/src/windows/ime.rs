//! IME composition detection (§9).
//!
//! §9 treats this as first-class, not an edge case: while a CJK composition is
//! active, a UIA read returns either the pre-composition text or the
//! uncommitted reading — neither is what the user sees — and a synthetic paste
//! interleaves with the pending composition and corrupts the buffer. So
//! [`FieldContext::ime_active`] gates both reads and inserts.
//!
//! The Windows detection §9 names is `ImmGetContext` + `ImmGetCompositionString`
//! on the foreground window.
//!
//! [`FieldContext::ime_active`]: aibo_core::types::FieldContext::ime_active

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::Ime::{
    GCS_COMPSTR, ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext,
};

/// Is an IME composition currently active on `hwnd`?
///
/// Returns `false` when the state cannot be determined — the caller must treat
/// that as "no composition" and rely on the other guards, because refusing to
/// work whenever IMM32 is unhelpful would break every non-CJK user.
///
// SPIKE: S7 — `ImmGetContext` is documented against windows owned by the
// *calling* thread. Against a foreign-process HWND it may simply return NULL,
// in which case this reports `false` for every real target and the §9 guarantee
// is worthless. The spike must establish which of these actually works
// cross-process, in Chromium/Electron as well as native controls:
//   a) `ImmGetContext` on the foreground HWND directly (this code),
//   b) `AttachThreadInput` to the foreground thread first, then (a),
//   c) `GetGUIThreadInfo(fg_thread)` → `GUITHREADINFO::hwndCaret` /
//      `flags & GUI_INMENUMODE`, which is explicitly cross-thread,
//   d) UIA: no composition property exists, so there is no UIA answer.
// Do not promote this to "verified" without evidence from a real IME.
pub(crate) fn composition_active(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }

    // SAFETY: `hwnd` is a window handle obtained from `GetForegroundWindow`.
    // `ImmGetContext` returns a null `HIMC` when the window has no input
    // context, which is checked before use. The matching `ImmReleaseContext`
    // runs on every non-null path.
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc.0.is_null() {
            return false;
        }
        // Passing a null buffer with length 0 asks only for the byte length of
        // the composition string; a positive length means a composition exists.
        let len = ImmGetCompositionStringW(himc, GCS_COMPSTR, None, 0);
        let _ = ImmReleaseContext(hwnd, himc);
        len > 0
    }
}
