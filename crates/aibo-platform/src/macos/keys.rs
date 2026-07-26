//! Synthetic keyboard events (§8 "Insert text").
//!
//! `CGEventPost` is a **different TCC service** from Accessibility (§8): a
//! paste-only build could ship sandboxed and it is the AX read that forfeits
//! that. Consequently nothing in this module calls `AXIsProcessTrusted`; it
//! checks [`super::permissions::secure_input_active`] instead, because secure
//! event input silently swallows synthesised keystrokes with no error.
//!
//! This is deliberately *not* `enigo`. §8 lists why: `set_string` chunked at 20
//! characters silently fails on chunks starting with a newline, events are
//! keydown-only with no delivery confirmation, the inter-event delay is only
//! applied on `Drop` so a long-lived instance drops characters, and emoji type
//! the wrong character on macOS. The crate self-describes as early alpha.

use std::time::Duration;

use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

use super::error::{MacosError, MacosResult};

/// `kVK_ANSI_C`.
pub(crate) const KEY_C: CGKeyCode = 8;
/// `kVK_ANSI_V`.
pub(crate) const KEY_V: CGKeyCode = 9;

/// Gap between the key-down and key-up of a synthetic chord.
///
/// Posting both in the same run-loop turn makes some Electron apps miss the
/// event entirely. This is inside the §8 budget: two chords cost ~8 ms.
const CHORD_GAP: Duration = Duration::from_millis(4);

fn source() -> MacosResult<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
        .map_err(|()| MacosError::Platform("CGEventSourceCreate failed".into()))
}

/// Post `⌘<key>` to the session event tap.
///
/// SPIKE: S4 — `CGEventPost` gives **no delivery confirmation**. Whether the
/// session tap or the HID tap reaches Chromium, Electron and Terminal reliably
/// is exactly what the insert-reliability spike has to measure; the caller must
/// treat a successful return as "posted", never as "applied".
pub(crate) fn post_command_chord(key: CGKeyCode) -> MacosResult<()> {
    if super::permissions::secure_input_active() {
        return Err(MacosError::SecureInput);
    }
    let src = source()?;
    let down = CGEvent::new_keyboard_event(src.clone(), key, true)
        .map_err(|()| MacosError::Platform("CGEventCreateKeyboardEvent(down) failed".into()))?;
    down.set_flags(CGEventFlags::CGEventFlagCommand);
    down.post(CGEventTapLocation::Session);

    std::thread::sleep(CHORD_GAP);

    let up = CGEvent::new_keyboard_event(src, key, false)
        .map_err(|()| MacosError::Platform("CGEventCreateKeyboardEvent(up) failed".into()))?;
    up.set_flags(CGEventFlags::CGEventFlagCommand);
    up.post(CGEventTapLocation::Session);
    Ok(())
}

/// Synthetic `⌘C`, the selection fallback of §8.
pub(crate) fn press_copy() -> MacosResult<()> {
    post_command_chord(KEY_C)
}

/// Synthetic `⌘V`, the default insert path of §8.
pub(crate) fn press_paste() -> MacosResult<()> {
    post_command_chord(KEY_V)
}

/// Type a string directly with `CGEventKeyboardSetUnicodeString`.
///
/// Used only for [`InsertMode::Keystroke`] and only for short inserts. Unlike
/// `enigo` this sends the whole string as one keydown/keyup pair rather than
/// 20-character chunks, which sidesteps the newline-leading-chunk bug — but it
/// inherits the deeper problem that the receiving app decides how to interpret
/// a unicode payload attached to keycode 0.
///
/// SPIKE: S4 — Unicode fidelity (emoji, combining marks, CJK) and the 5 KB
/// upper bound are unvalidated.
///
/// [`InsertMode::Keystroke`]: aibo_core::types::InsertMode::Keystroke
pub(crate) fn type_string(text: &str) -> MacosResult<()> {
    if super::permissions::secure_input_active() {
        return Err(MacosError::SecureInput);
    }
    let src = source()?;
    let down = CGEvent::new_keyboard_event(src.clone(), 0, true)
        .map_err(|()| MacosError::Platform("CGEventCreateKeyboardEvent(down) failed".into()))?;
    down.set_string(text);
    down.post(CGEventTapLocation::Session);

    std::thread::sleep(CHORD_GAP);

    let up = CGEvent::new_keyboard_event(src, 0, false)
        .map_err(|()| MacosError::Platform("CGEventCreateKeyboardEvent(up) failed".into()))?;
    up.set_string(text);
    up.post(CGEventTapLocation::Session);
    Ok(())
}
