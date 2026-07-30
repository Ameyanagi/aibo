//! Windows backend: UIA on a dedicated MTA thread, clipboard, SendInput, IMM32 (§8, §9).
//!
//! # Shape
//!
//! [`WindowsBackend`] owns no platform objects. It owns two channel handles —
//! one to the UI Automation MTA thread ([`uia`]), one to the clipboard worker
//! ([`clipboard`]) — plus a hidden notification window ([`msgwin`]). §7 requires
//! this: `uiautomation`'s types are `!Send` and `!Sync`, so the trait
//! implementation can only ever be a handle, and every deadline in §8 is
//! enforced on the async side because a blocked UIA call cannot be cancelled.
//!
//! # What is honest and what is not
//!
//! Windows fails *silently* in more places than macOS does, and every one of
//! them is a place where aibo would look broken rather than blocked:
//!
//! * **UIPI.** A non-elevated process cannot read from, or `SendInput` into, a
//!   window owned by an elevated process — Task Manager, admin consoles,
//!   installers, IT tooling. Win32 reports this as an empty tree and a
//!   zero-length `SendInput`. Here it is [`WindowsPlatformError::UipiBlocked`],
//!   checked *before* the work where possible, and surfaced as a typed error
//!   all the way to `AiboError::InsertFailed`/`AiboError::CaptureFailed`. §8:
//!   `uiAccess=true` needs Authenticode signing *and* an install under
//!   `%ProgramFiles%`, so for most builds this is permanent, not a prompt.
//! * **`GetSelection` on a control with no selection support** returns success
//!   with NULL ranges. Gated on `SupportedTextSelection` in [`uia`].
//! * **`ITextProvider2::GetCaretRange`** does not exist in Chromium, and so not
//!   in Chrome, Edge, Electron, Slack or VS Code. Enhancement only.
//! * **IME.** `ImmGetCompositionString` on the foreground window; while a
//!   composition is live aibo neither reads nor inserts (§9).
//!
//! # Unverified
//!
//! aibo is developed on macOS. **None of this module has ever been compiled** —
//! `cargo check` on a Mac does not reach it. The behaviour encoded here comes
//! from §8/§9 and is deliberate; the `windows` 0.62 and `uiautomation` 0.25 API
//! spellings are what to verify on the first Windows build. Two whole-file
//! risks, on top of the per-call `// SPIKE:` markers:
//!
//! * `windows-rs` has moved several `BOOL` parameters between `bool` and `BOOL`,
//!   and several nullable handle parameters between `T` and `Option<T>`, across
//!   releases. Expect a first-build pass fixing argument types; none of that
//!   changes behaviour.
//! * The `Win32_*` cargo features must be enabled on the `windows` dependency
//!   for any of these namespaces to exist at all.

mod clipboard;
mod display;
mod dpapi;
pub mod error;
mod ime;
mod input;
mod msgwin;
pub(crate) mod overlay;
mod permissions;
pub(crate) mod proxy;
mod uia;

use std::time::{Duration, Instant};

use aibo_core::error::{AiboError, InsertFailure, Result};
use aibo_core::traits::PlatformBackend;
use aibo_core::types::{
    AppInfo, AppRef, BoxStream, ClipboardItem, DisplayInfo, DocumentBudget, DocumentText,
    FieldContext, InsertMode, InsertTarget, Permission, PermissionStatus, PowerEvent,
};
use async_trait::async_trait;
use futures::StreamExt;
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL;
use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextW, GetWindowThreadProcessId};
use windows_core::PWSTR;

use self::clipboard::ClipboardHandle;
pub use self::dpapi::DpapiProtector;
pub use self::error::{WinResult, WindowsPlatformError};
use self::uia::UiaHandle;

/// How long to let the target app settle after a synthetic paste before
/// restoring the previous clipboard. Too short and the app has not read the
/// clipboard yet; too long and the user's next copy loses the race (§12).
const PASTE_SETTLE: Duration = Duration::from_millis(80);

/// Poll interval for the bounded focus-restore retry (§8).
const FOCUS_RETRY_INTERVAL: Duration = Duration::from_millis(15);

/// Wall-clock ceiling for one write-back, including clipboard round trips.
const INSERT_BUDGET: Duration = Duration::from_millis(500);

/// Executable stems that make [`AppInfo::is_code_app`] true, feeding
/// `RouteInput::has_code` (§4).
///
// TODO: this list belongs in `aibo-core` beside `RouteInput` so both platforms
// share one definition; it is here only because `aibo-core` does not expose one
// yet. Compared lowercase, without the `.exe` suffix.
const CODE_APPS: &[&str] = &[
    "code",
    "code - insiders",
    "cursor",
    "zed",
    "devenv",
    "rider64",
    "idea64",
    "clion64",
    "pycharm64",
    "webstorm64",
    "goland64",
    "rustrover64",
    "sublime_text",
    "nvim",
    "neovide",
    "vim",
    "windowsterminal",
    "wt",
    "pwsh",
    "powershell",
    "cmd",
    "alacritty",
    "wezterm-gui",
];

/// Executable stems whose clipboard payload is treated as concealed even when
/// the application does not publish Windows' opt-out formats.
const CLIPBOARD_DENYLIST: &[&str] = &[
    "1password",
    "bitwarden",
    "dashlane",
    "enpass",
    "keepass",
    "keepassxc",
    "lastpass",
    "nordpass",
    "proton pass",
    "protonpass",
    "roboform",
];

fn is_clipboard_denylisted(executable_stem: &str) -> bool {
    let normalized = executable_stem.trim().to_ascii_lowercase();
    CLIPBOARD_DENYLIST.iter().any(|candidate| {
        normalized == *candidate || normalized.starts_with(&format!("{candidate}-"))
    })
}

/// The Windows [`PlatformBackend`].
///
/// Construct once and share: it is `Send + Sync` because it is only channels.
#[derive(Debug, Clone)]
pub struct WindowsBackend {
    uia: UiaHandle,
    clipboard: ClipboardHandle,
}

impl WindowsBackend {
    /// Start the platform threads and the notification window.
    ///
    /// Call before creating any window — the Per-Monitor-V2 opt-in must run
    /// before the first window exists to be effective, and a manifest that
    /// already declares it makes the call fail harmlessly (§9).
    pub fn new() -> WinResult<Self> {
        if let Err(e) = display::enable_per_monitor_v2() {
            // Expected when the manifest already declared awareness, which is
            // the configuration §9 actually wants.
            tracing::debug!(error = %e, "DPI awareness was already set");
        }
        msgwin::ensure_started();
        Ok(Self {
            uia: UiaHandle::spawn()?,
            clipboard: ClipboardHandle::spawn()?,
        })
    }

    /// Best-effort app name for the `app` field of a capture error.
    ///
    /// Takes the capture's subject, not the foreground window: after the panel
    /// is up the foreground app is aibo, and a `CaptureFailed { app: "aibo" }`
    /// tells the user the wrong application is broken.
    fn app_label(of: &AppRef) -> String {
        process_image_name(of.pid as u32).unwrap_or_else(|| "the focused application".to_owned())
    }

    /// §8's synthetic-copy fallback: `Ctrl+C`, wait for the clipboard to
    /// change, read it, put the old contents back.
    ///
    /// Mutates the clipboard, so it is only reached when UIA has nothing, and
    /// it restores under the §12 race rule — if the sequence number moved for a
    /// reason that was not aibo, the user's copy wins and the restore is
    /// abandoned rather than forced.
    ///
    /// `SendInput` cannot be aimed: the chord goes to whatever owns the
    /// foreground. The caller must therefore only reach this while the capture
    /// target is still frontmost — see [`PlatformBackend::selected_text`].
    async fn selected_text_via_copy(&self, deadline: Instant) -> WinResult<Option<String>> {
        let saved = self.clipboard.read(deadline, false).await.ok();
        let before = ClipboardHandle::sequence();
        // Subscribe *before* sending the chord, so the notification cannot be
        // missed in the gap.
        let mut updates = msgwin::clipboard_updates();

        input::release_held_modifiers()?;
        input::send_chord(VK_CONTROL, input::VK_C)?;

        // Wait for the copy to land rather than sleeping a guessed interval.
        // Some apps swallow the shortcut entirely — §8 — which is why this is
        // bounded and why expiry is not an error, it is "no selection".
        let budget = deadline.saturating_duration_since(Instant::now());
        let landed = tokio::time::timeout(budget, async {
            loop {
                match updates.recv().await {
                    Ok(seq) if seq != before => return true,
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false);

        if !landed && ClipboardHandle::sequence() == before {
            return Ok(None);
        }

        let copied = self.clipboard.read(deadline, false).await?;
        let after = ClipboardHandle::sequence();

        if let Some(previous) = saved {
            // Only restore what can be restored faithfully (§12): putting plain
            // text back over HTML or RTF is data loss, so decline instead.
            let text = previous.restorable.then(|| previous.text.clone()).flatten();
            let _ = self.clipboard.restore(text, after, deadline).await;
        }

        Ok(copied.text.filter(|t| !t.is_empty()))
    }

    /// The shared paste path.
    ///
    /// One paste, never chunked: §13 depends on the insert being atomic from the
    /// user's point of view so that undo, cancellation and partial failure stay
    /// tractable.
    async fn paste(&self, text: &str, restore_previous: bool) -> WinResult<()> {
        let deadline = Instant::now() + INSERT_BUDGET;

        let saved = if restore_previous {
            self.clipboard.read(deadline, false).await.ok()
        } else {
            None
        };

        let sequence = self.clipboard.write(text.to_owned(), deadline).await?;

        input::release_held_modifiers()?;
        input::send_chord(VK_CONTROL, input::VK_V)?;

        if let Some(previous) = saved {
            tokio::time::sleep(PASTE_SETTLE).await;
            let restorable = previous.restorable && !previous.concealed;
            let text = restorable.then(|| previous.text.clone()).flatten();
            let _ = self
                .clipboard
                .restore(text, sequence, Instant::now() + INSERT_BUDGET)
                .await;
        }
        Ok(())
    }

    /// Guard every write-back on §9's IME rule.
    fn refuse_if_composing(&self) -> WinResult<()> {
        if let Some(hwnd) = input::foreground_window()
            && ime::composition_active(hwnd)
        {
            return Err(WindowsPlatformError::ImeActive);
        }
        Ok(())
    }
}

#[async_trait]
impl PlatformBackend for WindowsBackend {
    fn focused_app_ref(&self) -> Result<AppRef> {
        Ok(foreground_app_ref()?)
    }

    fn active_display(&self) -> Result<DisplayInfo> {
        Ok(display::display_for_window(input::foreground_window())?)
    }

    fn secure_input_active(&self) -> bool {
        // Windows has no global secure-input flag; §8's note is that "password
        // fields behave similarly under UIPI". The condition that generalises
        // is reachability: if the foreground process is out of UIPI reach, both
        // reads and synthetic input fail silently — precisely what this
        // predicate exists to warn about before the user blames aibo.
        //
        // A password *field* inside a reachable process is not covered here.
        // That is detected per-element during capture, because answering it
        // requires UIA and this method must not block.
        input::foreground_pid().is_some_and(permissions::process_is_out_of_reach)
    }

    /// Identity of the app snapshotted on hotkey-down (§7).
    ///
    /// `of` is the subject: resolving `foreground_app_ref()` here would answer
    /// "aibo" once the panel is up, which is what `is_code_app` routing (§4),
    /// §5's source-app prompt line and `Exchange::source_app` all read.
    async fn focused_app(&self, of: &AppRef, _timeout: Duration) -> Result<AppInfo> {
        let app_ref = of.clone();

        // Deliberately not routed through the UIA thread: the identity comes
        // from Win32 calls that cannot block, and queueing it behind a
        // possibly-stuck UIA job would cost the panel its first chip (§8).
        let identifier = process_image_name(app_ref.pid as u32).unwrap_or_else(|| {
            // §8: `QueryFullProcessImageName` is itself refused across a UIPI
            // boundary, so an unknown identifier usually means an elevated
            // target rather than a missing process.
            "unknown".to_owned()
        });
        let stem = identifier.to_ascii_lowercase();
        let display_name = app_ref
            .window
            .and_then(|w| window_title(hwnd_from_u64(w)))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| identifier.clone());

        Ok(AppInfo {
            app_ref,
            identifier,
            display_name,
            is_code_app: CODE_APPS.contains(&stem.as_str()),
        })
    }

    async fn read_document(
        &self,
        of: &AppRef,
        budget: DocumentBudget,
        timeout: Duration,
    ) -> Result<Option<DocumentText>> {
        let deadline = Instant::now() + timeout;
        match tokio::time::timeout(timeout, self.uia.read_document(of, budget, deadline)).await {
            Ok(Ok(document)) => Ok(document),
            Ok(Err(e)) if e.is_uipi() => Err(e.into_capture_error(Self::app_label(of))),
            // Out of time, or an app with no text provider. Neither is aibo
            // failing, and §13 renders "nothing to read" without an error.
            Ok(Err(_)) | Err(_) => Ok(None),
        }
    }

    async fn focused_element_id(&self, of: &AppRef, timeout: Duration) -> Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        // §8's third insert-validation comparison. A UIA runtime id identifies
        // the control within this session; failing to get one weakens the check
        // to pid-plus-window rather than blocking the insert, which is what §13
        // prefers on an app with no usable UIA tree.
        match tokio::time::timeout(timeout, self.uia.focused_element_id(of, deadline)).await {
            Ok(Ok(id)) => Ok(id),
            Ok(Err(e)) if e.is_uipi() => Err(e.into_capture_error(Self::app_label(of))),
            Ok(Err(_)) | Err(_) => Ok(None),
        }
    }

    async fn selected_text(&self, of: &AppRef, timeout: Duration) -> Result<Option<String>> {
        let deadline = Instant::now() + timeout;

        // §8: UIA `GetSelection` is the primary read — of `of`, not of whatever
        // owns the foreground once the panel is up.
        match tokio::time::timeout(timeout, self.uia.selected_text(of, deadline)).await {
            Ok(Ok(Some(text))) => return Ok(Some(text)),
            // UIPI is permanent for this build; a synthetic Ctrl+C would fail
            // the same way and mutate the clipboard for nothing.
            Ok(Err(e)) if e.is_uipi() => return Err(e.into_capture_error(Self::app_label(of))),
            // §5: the focused control is a password field. The fallback below
            // is *more* dangerous than the primary read it replaces — it
            // synthesises Ctrl+C, so falling through would copy the password
            // onto the clipboard and then "restore" over it. Refuse here.
            Ok(Err(e)) if e.is_secure_field() => {
                return Err(e.into_capture_error(Self::app_label(of)));
            }
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {}
        }

        // Fallback: synthetic copy — but only if the caller's deadline actually
        // covers it (§8 budgets 250 ms including this path, 120 ms without).
        if Instant::now() >= deadline {
            return Ok(None);
        }
        // …and only while `of` still owns the foreground. `SendInput` cannot be
        // aimed at a background window, so once the panel has activation the
        // chord copies out of aibo's own text box: it would attribute the user's
        // typed query to the target app *and* clobber their clipboard doing it.
        // Declining is the only correct answer; §8 already requires every view
        // to tolerate a capture that comes back empty.
        if input::foreground_pid() != Some(of.pid as u32) {
            tracing::debug!(
                pid = of.pid,
                "skipping the synthetic-Ctrl+C fallback: the capture target is not frontmost (§7)"
            );
            return Ok(None);
        }
        match self.selected_text_via_copy(deadline).await {
            Ok(text) => Ok(text),
            Err(e) if e.is_uipi() => Err(e.into_capture_error(Self::app_label(of))),
            // The app swallowed the shortcut, or the clipboard was busy. "No
            // selection" is the truthful answer, not an error worth a panel
            // treatment (§13).
            Err(e) => {
                tracing::debug!(error = %e, "synthetic-copy selection fallback failed");
                Ok(None)
            }
        }
    }

    async fn text_field_context(
        &self,
        of: &AppRef,
        timeout: Duration,
    ) -> Result<Option<FieldContext>> {
        let deadline = Instant::now() + timeout;
        match tokio::time::timeout(timeout, self.uia.field_context(of, deadline)).await {
            Ok(Ok(ctx)) => Ok(ctx),
            // §9: a live IME composition is a real, user-visible state with its
            // own "finish typing to continue" message, so it propagates instead
            // of being flattened into "no context".
            Ok(Err(e @ WindowsPlatformError::ImeActive)) => {
                Err(e.into_capture_error(Self::app_label(of)))
            }
            Ok(Err(e)) if e.is_uipi() || e.is_secure_field() => {
                Err(e.into_capture_error(Self::app_label(of)))
            }
            // Everything else — no text pattern, no selection support, a
            // provider that refused — means "no field context", which §8
            // requires every view to tolerate anyway.
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "no usable field context");
                Ok(None)
            }
            Err(_) => Ok(None),
        }
    }

    async fn clipboard(&self, owner_hint: &AppRef, timeout: Duration) -> Result<ClipboardItem> {
        let deadline = Instant::now() + timeout;
        let source_app = process_image_name(owner_hint.pid as u32);
        let conceal_source = source_app.as_deref().is_some_and(is_clipboard_denylisted);
        let mut item = tokio::time::timeout(timeout, self.clipboard.read(deadline, conceal_source))
            .await
            .map_err(|_| WindowsPlatformError::Deadline(timeout))??;
        // §12 attribution. `GetClipboardOwner` is frequently NULL (delayed
        // rendering, or an owner that has already exited), so the app that had
        // focus when the hotkey fired is the best available answer — and it is
        // the *only* one that can ever match a denylist, because by capture time
        // the frontmost app is aibo.
        //
        item.source_app = source_app;
        Ok(item)
    }

    async fn insert_text(&self, text: &str, mode: InsertMode) -> Result<()> {
        self.refuse_if_composing()
            .map_err(WindowsPlatformError::into_insert_error)?;

        let result = match mode {
            InsertMode::PasteAndRestore => self.paste(text, true).await,
            InsertMode::PasteKeepNew => self.paste(text, false).await,
            InsertMode::Keystroke if text.chars().count() <= input::KEYSTROKE_MAX_CHARS => {
                input::release_held_modifiers().and_then(|()| input::send_unicode(text))
            }
            // §8 calls `SendInput` the short-insert path only. Sending thousands
            // of key events instead would take seconds and interleave with real
            // typing; the paste path is the correct answer, not a refusal.
            InsertMode::Keystroke => self.paste(text, true).await,
        };
        result.map_err(WindowsPlatformError::into_insert_error)
    }

    async fn replace_selection(&self, text: &str) -> Result<()> {
        self.refuse_if_composing()
            .map_err(WindowsPlatformError::into_insert_error)?;
        // A paste over a live selection replaces it. There is no separate
        // Windows API for this, and doing it as delete-then-insert would cost
        // the single-undo-step property (§13).
        self.paste(text, true)
            .await
            .map_err(WindowsPlatformError::into_insert_error)
    }

    async fn validate_target(&self, target: &InsertTarget) -> Result<bool> {
        let timeout = Duration::from_millis(120);
        let deadline = Instant::now() + timeout;
        match tokio::time::timeout(timeout, self.uia.validate_target(target, deadline)).await {
            Ok(Ok(valid)) => Ok(valid),
            // Fail closed. §8: pasting a rewrite over the wrong content is
            // unrecoverable, so "could not check" must mean "do not insert".
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "target validation failed; refusing the insert");
                Ok(false)
            }
            Err(_) => Ok(false),
        }
    }

    async fn restore_focus(&self, prev: &AppRef, timeout: Duration) -> Result<()> {
        let Some(handle) = prev.window else {
            return Err(AiboError::InsertFailed {
                reason: InsertFailure::Cancelled,
            });
        };
        let deadline = Instant::now() + timeout;

        // §8: confirm, with a bounded retry. `SetForegroundWindow` is
        // best-effort by design — Windows refuses it outright when the calling
        // thread does not own the foreground — so assuming success races and
        // pastes into the wrong window.
        //
        // The `HWND` is rebuilt each iteration rather than held: it wraps a raw
        // pointer and so is `!Send`, and this future has to cross threads.
        loop {
            if input::try_restore_focus(hwnd_from_u64(handle)) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(WindowsPlatformError::FocusNotRestored.into_insert_error());
            }
            tokio::time::sleep(FOCUS_RETRY_INTERVAL).await;
        }
    }

    fn permission_status(&self, p: Permission) -> PermissionStatus {
        permissions::status(p)
    }

    fn request_permission(&self, p: Permission) -> Result<()> {
        Ok(permissions::request(p)?)
    }

    fn power_events(&self) -> Result<BoxStream<'static, PowerEvent>> {
        let rx = msgwin::power_events();
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(event) => return Some((event, rx)),
                    // A lagging subscriber missed transitions; the next one is
                    // still worth delivering — §13 only needs "something
                    // happened, re-warm and re-probe".
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(missed = n, "power event subscriber lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        Ok(stream.boxed())
    }
}

// ---------------------------------------------------------------------------
// Small Win32 helpers shared across the module
// ---------------------------------------------------------------------------

/// Rebuild an `HWND` from the opaque `u64` [`AppRef`] carries.
fn hwnd_from_u64(handle: u64) -> HWND {
    HWND(handle as usize as *mut std::ffi::c_void)
}

/// The foreground app and window (§8 step 1: instant, and cannot fail slowly).
pub(crate) fn foreground_app_ref() -> WinResult<AppRef> {
    let Some(hwnd) = input::foreground_window() else {
        return Err(WindowsPlatformError::win32_bare(
            "GetForegroundWindow",
            "no foreground window (locked session, or a secure-desktop prompt)",
        ));
    };
    let mut pid = 0u32;
    // SAFETY: `hwnd` came from `GetForegroundWindow`; `pid` is a live out-param.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    Ok(AppRef {
        pid: pid as i32,
        window: Some(hwnd.0 as usize as u64),
    })
}

/// Executable name, without extension, for a pid.
///
/// `None` when the process is out of UIPI reach — §8's "silently fails in
/// exactly the power-user contexts" case, which callers surface rather than
/// paper over with a placeholder that reads like a real app name.
pub(crate) fn process_image_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    // SAFETY: the handle is opened with the minimum access right, used for one
    // query, and closed on both paths. `buf` and `len` are live for the duration
    // of the call, and `len` is the buffer capacity in wide characters, which is
    // what the API documents.
    let path = unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        String::from_utf16_lossy(&buf[..len as usize])
    };
    std::path::Path::new(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
}

/// The window's title-bar text. Display only — never an identity.
pub(crate) fn window_title(hwnd: HWND) -> Option<String> {
    if hwnd.0.is_null() {
        return None;
    }
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is live and `GetWindowTextW` writes at most its length,
    // returning the number of characters written.
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    (len > 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// Stable content hash for [`InsertTarget`] (§8).
///
/// Only ever compared against another hash produced in the same process, so it
/// does not need to be portable — only deterministic.
pub(crate) fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::is_clipboard_denylisted;

    #[test]
    fn password_manager_executables_are_clipboard_denylisted() {
        for executable in [
            "1Password",
            "Bitwarden",
            "KeePassXC",
            "ProtonPass",
            "RoboForm",
        ] {
            assert!(is_clipboard_denylisted(executable), "{executable}");
        }
        assert!(!is_clipboard_denylisted("Code"));
        assert!(!is_clipboard_denylisted("Safari"));
    }
}
