//! The dedicated UI Automation thread (§6, §7, §8).
//!
//! **Why a thread.** `uiautomation`'s types are `!Send` and `!Sync` — UI
//! Automation is apartment-threaded COM. §7 makes this a shape requirement, not
//! a suggestion: the real UIA objects live on exactly one thread, which
//! initialises COM as **MTA**, and [`UiaHandle`] is only a channel to it. MTA
//! rather than STA because an STA client needs a message pump to service COM
//! calls, and a thread that is both pumping messages and blocking on UIA
//! deadlocks in the interesting cases.
//!
//! **Why deadlines are enforced by the caller.** A UIA call into an
//! unresponsive provider blocks for as long as the provider takes; there is no
//! cancel. The async side therefore times out on the reply channel (§8: 120 ms
//! for UIA, 250 ms including the clipboard fallback) and the worker discards
//! jobs whose deadline has already passed. A hung provider parks this thread and
//! later jobs are dropped as stale — the honest behaviour, and far better than
//! the earlier design where pressing the hotkey froze the target app.
//!
//! **What is primary and what is not.** §8: `GetSelection` is the primary read;
//! `ITextProvider2::GetCaretRange` is an *enhancement only*. Chromium declares
//! `ITextProvider`/`ITextEditProvider` but **not** `ITextProvider2`, so Chrome,
//! Edge, Electron, Slack and VS Code have no `GetCaretRange` — plausibly the
//! majority of the target surface. `uiautomation` 0.25 hides this: its
//! `UITextPattern::get_caret_range` does the `QueryInterface` to
//! `IUIAutomationTextPattern2` internally and fails at *runtime*, not at compile
//! time. Every use of it here degrades instead of propagating.
//!
//! **The `GetSelection` trap.** On a control whose `SupportedTextSelection` is
//! `None`, `GetSelection` returns **success with NULL ranges** — not an error.
//! Treating "no error" as "got a selection" produces confident empty captures,
//! so the gate is [`UITextPattern::get_supported_text_selection`], checked
//! first.
//!
// SPIKE: S2/S4 — the API spellings below are checked against `uiautomation`
// 0.25's source and this file cross-compiles for x86_64-pc-windows-msvc, but
// **nothing here has been run**. What a cross-compile cannot tell us, and what
// the spike must answer on real hardware:
//   * whether Chromium-based apps return usable ranges from `GetSelection` at
//     all, or only from `ITextEditProvider`;
//   * whether `MoveEndpointByUnit(Character, -800)` is bounded-cost in a large
//     document or walks the whole buffer (a latency cliff, §15);
//   * whether `get_active_composition` actually reports a live IME composition
//     in Chromium and in native Win32 controls (§9, S7).

use std::time::Instant;

use aibo_core::types::{AppRef, FieldContext, InsertTarget, Rect};
use tokio::sync::{mpsc, oneshot};
use uiautomation::UIAutomation;
use uiautomation::UIElement;
use uiautomation::patterns::{UITextEditPattern, UITextPattern, UITextRange};
use uiautomation::types::{Handle, SupportedTextSelection, TextPatternRangeEndpoint, TextUnit};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

use super::error::{WinResult, WindowsPlatformError};
use super::{input, permissions, text_hash};

/// §5 targets roughly 800 characters of pre-caret context.
const PREFIX_CHARS: i32 = 800;
/// Post-caret context is smaller: it disambiguates a completion, it is not the
/// subject of one.
const SUFFIX_CHARS: i32 = 400;
/// Upper bound for a single selection read. §5 forbids pulling a whole document
/// out of the target app before deciding it is too long.
const SELECTION_MAX_CHARS: i32 = 200_000;

/// Maximum UIA operations waiting behind an in-flight provider call.
///
/// A single capture can request selection, context, and focused-element
/// identity. Eight slots allow two overlapping captures plus validation while
/// bounding retained context if a third-party UIA provider hangs.
const UIA_QUEUE_CAPACITY: usize = 8;

/// Every capture op carries the [`AppRef`] snapshotted on hotkey-down (§7).
/// The UIA thread reads from *that* application; "the focused element" without
/// a subject means aibo's own panel by the time these jobs run.
enum UiaOp {
    SelectedText {
        of: AppRef,
        reply: oneshot::Sender<WinResult<Option<String>>>,
    },
    FieldContext {
        of: AppRef,
        reply: oneshot::Sender<WinResult<Option<FieldContext>>>,
    },
    /// Opaque identity of the element focused inside `of`, captured alongside
    /// the context so [`InsertTarget::focused_element`] has something to
    /// compare.
    FocusedElementId {
        of: AppRef,
        reply: oneshot::Sender<WinResult<Option<String>>>,
    },
    ValidateTarget {
        target: Box<InsertTarget>,
        reply: oneshot::Sender<WinResult<bool>>,
    },
}

struct UiaJob {
    deadline: Instant,
    op: UiaOp,
}

/// A channel handle to the UIA thread. Cheap to clone, `Send + Sync`.
#[derive(Clone, Debug)]
pub(crate) struct UiaHandle {
    tx: mpsc::Sender<UiaJob>,
}

impl UiaHandle {
    /// Start the UIA thread.
    pub(crate) fn spawn() -> WinResult<Self> {
        let (tx, rx) = mpsc::channel(UIA_QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("aibo-win-uia".into())
            .spawn(move || worker(rx))
            .map_err(|e| {
                WindowsPlatformError::win32_bare("CreateThread", format!("UIA worker: {e}"))
            })?;
        Ok(Self { tx })
    }

    fn submit(&self, deadline: Instant, op: UiaOp) -> WinResult<()> {
        match self.tx.try_send(UiaJob { deadline, op }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(WindowsPlatformError::WorkerBusy {
                worker: "UI Automation",
            }),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(WindowsPlatformError::UiaThreadGone),
        }
    }

    /// Read the selection inside `of` (§8 primary path).
    pub(crate) async fn selected_text(
        &self,
        of: &AppRef,
        deadline: Instant,
    ) -> WinResult<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.submit(
            deadline,
            UiaOp::SelectedText {
                of: of.clone(),
                reply,
            },
        )?;
        rx.await.map_err(|_| WindowsPlatformError::UiaThreadGone)?
    }

    /// Read a bounded window of the text field focused inside `of`.
    pub(crate) async fn field_context(
        &self,
        of: &AppRef,
        deadline: Instant,
    ) -> WinResult<Option<FieldContext>> {
        let (reply, rx) = oneshot::channel();
        self.submit(
            deadline,
            UiaOp::FieldContext {
                of: of.clone(),
                reply,
            },
        )?;
        rx.await.map_err(|_| WindowsPlatformError::UiaThreadGone)?
    }

    /// Identity of the element focused inside `of`, for [`InsertTarget`].
    #[allow(dead_code, reason = "consumed by the capture pipeline in P1 (§8)")]
    pub(crate) async fn focused_element_id(
        &self,
        of: &AppRef,
        deadline: Instant,
    ) -> WinResult<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.submit(
            deadline,
            UiaOp::FocusedElementId {
                of: of.clone(),
                reply,
            },
        )?;
        rx.await.map_err(|_| WindowsPlatformError::UiaThreadGone)?
    }

    /// Re-check everything that was true at capture time (§8).
    pub(crate) async fn validate_target(
        &self,
        target: &InsertTarget,
        deadline: Instant,
    ) -> WinResult<bool> {
        let (reply, rx) = oneshot::channel();
        self.submit(
            deadline,
            UiaOp::ValidateTarget {
                target: Box::new(target.clone()),
                reply,
            },
        )?;
        rx.await.map_err(|_| WindowsPlatformError::UiaThreadGone)?
    }
}

fn worker(mut rx: mpsc::Receiver<UiaJob>) {
    // SAFETY: initialising COM for this thread as MTA, exactly once, and
    // uninitialising it when the loop ends. No pointers are passed in.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() {
        tracing::error!(hresult = ?hr, "CoInitializeEx(MTA) failed; UI Automation is unavailable");
        drain_with_error(rx, || {
            WindowsPlatformError::Uia("COM is not initialised".into())
        });
        return;
    }

    // `new_direct` is the constructor that does *not* call `CoInitializeEx`
    // itself, which is what this thread wants: it has already chosen MTA.
    let automation = match UIAutomation::new_direct() {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "could not create the UIAutomation client");
            drain_with_error(rx, || {
                WindowsPlatformError::Uia("the UIAutomation client is unavailable".into())
            });
            // SAFETY: paired with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
            return;
        }
    };

    while let Some(job) = rx.blocking_recv() {
        if Instant::now() > job.deadline {
            tracing::debug!("UIA job dropped: past deadline before it started");
            continue;
        }
        match job.op {
            UiaOp::SelectedText { of, reply } => {
                let _ = reply.send(selected_text(&automation, &of));
            }
            UiaOp::FieldContext { of, reply } => {
                let _ = reply.send(field_context(&automation, &of));
            }
            UiaOp::FocusedElementId { of, reply } => {
                let _ = reply.send(focused_element_id(&automation, &of));
            }
            UiaOp::ValidateTarget { target, reply } => {
                let _ = reply.send(validate_target(&automation, &target));
            }
        }
    }

    // SAFETY: paired with the successful CoInitializeEx above; the automation
    // client has been dropped by now.
    unsafe { CoUninitialize() };
}

/// Answer every queued and future job with an error rather than leaving callers
/// to time out on a dead channel.
fn drain_with_error(mut rx: mpsc::Receiver<UiaJob>, error: impl Fn() -> WindowsPlatformError) {
    while let Some(job) = rx.blocking_recv() {
        match job.op {
            UiaOp::SelectedText { reply, .. } => {
                let _ = reply.send(Err(error()));
            }
            UiaOp::FieldContext { reply, .. } => {
                let _ = reply.send(Err(error()));
            }
            UiaOp::FocusedElementId { reply, .. } => {
                let _ = reply.send(Err(error()));
            }
            UiaOp::ValidateTarget { reply, .. } => {
                let _ = reply.send(Err(error()));
            }
        }
    }
}

fn uia_err(e: uiautomation::Error) -> WindowsPlatformError {
    WindowsPlatformError::Uia(e.to_string())
}

/// Refuse before doing any work if the capture target is out of reach.
///
/// §8: UIPI returns empty results rather than errors, so this check is what
/// turns "aibo does nothing in Task Manager" into a message the user can act on.
/// It is keyed on the snapshotted app rather than on the foreground window for
/// the same reason everything else here is: the foreground window is aibo's own
/// panel, which is always in reach and therefore always answers "fine".
fn guard_uipi(of: &AppRef) -> WinResult<()> {
    let pid = u32::try_from(of.pid).unwrap_or(0);
    if pid != 0 && permissions::process_is_out_of_reach(pid) {
        return Err(WindowsPlatformError::UipiBlocked { pid });
    }
    Ok(())
}

/// The focused element **inside `of`** (§7).
///
/// `IUIAutomation::GetFocusedElement` is global: it answers with whatever has
/// keyboard focus process-wide, which during §8's deferred capture is aibo's own
/// panel text box. Using it unqualified is the defect this parameter exists to
/// close — the captured `field.prefix` becomes the query the user just typed
/// into aibo instead of the document they were writing.
///
/// So the global answer is *accepted only if it belongs to `of`* (the case where
/// the panel has not taken focus — a genuinely non-activating overlay, S1), and
/// otherwise the target's own focus window is resolved through
/// `GetGUIThreadInfo` and turned into an element with `ElementFromHandle`.
///
// SPIKE: S2 — `ElementFromHandle` yields the element *for that HWND*. For a
// native Win32 control that is the focused control itself; for a Chromium or
// Electron window the whole UI is one HWND, so it is the window element and the
// text patterns below are queried on it rather than on the inner control. Which
// of the two the target surface actually needs is a spike question, not a
// guess to make here.
fn focused_in(automation: &UIAutomation, of: &AppRef) -> WinResult<UIElement> {
    if let Ok(element) = automation.get_focused_element()
        && element
            .get_process_id()
            .is_ok_and(|pid| i64::from(pid) == i64::from(of.pid))
    {
        return Ok(element);
    }

    let Some(window) = of.window.map(super::hwnd_from_u64) else {
        return Err(WindowsPlatformError::TargetChanged);
    };
    let focus = input::focus_window_for(window).unwrap_or(window);
    let element = automation
        .element_from_handle(Handle::from(focus))
        .map_err(uia_err)?;

    // A recycled HWND belongs to a different process now. Reading it would be
    // the same category of error as reading aibo's own panel.
    if element
        .get_process_id()
        .is_ok_and(|pid| i64::from(pid) == i64::from(of.pid))
    {
        Ok(element)
    } else {
        Err(WindowsPlatformError::TargetChanged)
    }
}

fn text_pattern(element: &UIElement) -> WinResult<UITextPattern> {
    element
        .get_pattern::<UITextPattern>()
        .map_err(|_| WindowsPlatformError::NoTextPattern)
}

/// The §8 gate.
///
/// Returns `Err(NoTextSelectionSupport)` when the control reports `None` — the
/// case where `GetSelection` would otherwise succeed with NULL ranges and be
/// mistaken for an empty selection. An unreadable property is allowed through:
/// the empty-ranges check downstream still stands between us and a confident
/// wrong answer.
fn require_text_selection(pattern: &UITextPattern) -> WinResult<()> {
    match pattern.get_supported_text_selection() {
        Ok(SupportedTextSelection::None) => Err(WindowsPlatformError::NoTextSelectionSupport),
        _ => Ok(()),
    }
}

/// Is an IME composition live in the focused element (§9)?
///
/// Two signals, in order of reliability:
///
/// 1. `ITextEditProvider::GetActiveComposition`, exposed here as
///    [`UITextEditPattern::get_active_composition`]. §8 records that Chromium
///    *does* implement `ITextEditProvider` — unlike `ITextProvider2` — so this
///    is expected to work across the Electron surface, which is exactly where
///    the IMM32 route is least trustworthy.
/// 2. `ImmGetCompositionString` on the foreground window, which is the API §9
///    names but which is documented against windows owned by the calling
///    thread (see [`super::ime`], SPIKE S7).
fn composition_active(element: &UIElement, of: &AppRef) -> bool {
    if let Ok(edit) = element.get_pattern::<UITextEditPattern>()
        && let Ok(range) = edit.get_active_composition()
        && range.get_text(1).is_ok_and(|t| !t.is_empty())
    {
        return true;
    }
    // The IMM32 half must also ask about the *target's* window: the foreground
    // window during deferred capture is aibo's panel, whose composition state
    // says nothing about the document being captured (§7, §9).
    of.window
        .map(super::hwnd_from_u64)
        .and_then(input::focus_window_for)
        .is_some_and(super::ime::composition_active)
}

fn selected_text(automation: &UIAutomation, of: &AppRef) -> WinResult<Option<String>> {
    // `guard_uipi` is the Windows half of §5's "check
    // `IsSecureEventInputEnabled()` before reading". Windows has no global
    // secure-input flag; the ambient condition that behaves the same way is
    // UIPI, and it is what `WindowsBackend::secure_input_active` reports.
    guard_uipi(of)?;
    let element = focused_in(automation, of)?;
    // §5: never capture from a secure field. A *typed refusal*, not `Ok(None)`
    // — see `WindowsPlatformError::SecureField`. `Ok(None)` here would send the
    // caller into the synthetic-Ctrl+C fallback and put the password on the
    // clipboard.
    if element.is_password().unwrap_or(false) {
        return Err(WindowsPlatformError::SecureField);
    }

    let pattern = text_pattern(&element)?;
    require_text_selection(&pattern)?;

    let ranges = pattern.get_selection().map_err(uia_err)?;
    if ranges.is_empty() {
        // The NULL-ranges case §8 warns about, reachable even past the gate. An
        // empty selection is a legitimate answer, not a failure.
        return Ok(None);
    }

    let mut out = String::new();
    for range in &ranges {
        match range.get_text(SELECTION_MAX_CHARS) {
            Ok(text) => out.push_str(&text),
            Err(e) => tracing::debug!(error = %e, "a selection range refused get_text"),
        }
    }
    Ok((!out.is_empty()).then_some(out))
}

fn field_context(automation: &UIAutomation, of: &AppRef) -> WinResult<Option<FieldContext>> {
    // See `selected_text`: on Windows this is §5's "before reading" gate.
    guard_uipi(of)?;
    let element = focused_in(automation, of)?;

    // §9: while composing, a read returns the pre-composition text or the
    // uncommitted reading — neither is what the user sees — so aibo declines
    // and the panel says "finish typing to continue".
    if composition_active(&element, of) {
        return Err(WindowsPlatformError::ImeActive);
    }

    // §5: never capture from a secure field. This mirrors the macOS
    // `AXSecureTextField` branch exactly — empty `prefix`/`suffix` plus
    // `is_secure`, which `FieldContext` documents as mandatory and which
    // `aibo_core::prompts::assemble` uses as the second line of the same
    // defence. `Ok(None)` (the previous answer) carried no such signal: the
    // panel could not distinguish "password field" from "not a text field".
    if element.is_password().unwrap_or(false) {
        return Ok(Some(FieldContext {
            prefix: String::new(),
            suffix: String::new(),
            caret: None,
            label: element.get_name().ok().filter(|n| !n.is_empty()),
            is_secure: true,
            ime_active: false,
            truncated: false,
            caret_bounds: None,
        }));
    }

    let pattern = text_pattern(&element)?;
    require_text_selection(&pattern)?;

    // Two *separate* caret ranges, deliberately. `UITextRange` derives `Clone`,
    // but that clones the COM interface pointer — both handles then refer to the
    // same underlying range object, so moving one endpoint would corrupt the
    // other read. Fetching twice is the only way to get independent ranges
    // through this crate's surface.
    let prefix = caret_range(&pattern).and_then(|r| {
        expand(
            &r,
            TextPatternRangeEndpoint::Start,
            -PREFIX_CHARS,
            PREFIX_CHARS,
        )
    });
    let suffix = caret_range(&pattern).and_then(|r| {
        expand(
            &r,
            TextPatternRangeEndpoint::End,
            SUFFIX_CHARS,
            SUFFIX_CHARS,
        )
    });

    if prefix.is_none() && suffix.is_none() {
        return Ok(None);
    }

    let truncated = prefix
        .as_ref()
        .is_some_and(|p| p.chars().count() >= PREFIX_CHARS as usize)
        || suffix
            .as_ref()
            .is_some_and(|s| s.chars().count() >= SUFFIX_CHARS as usize);

    Ok(Some(FieldContext {
        prefix: prefix.unwrap_or_default(),
        suffix: suffix.unwrap_or_default(),
        // UIA expresses the caret as a degenerate range, never as an offset into
        // the field's value, and materialising one would mean reading the whole
        // document — which §5 forbids.
        caret: None,
        label: element.get_name().ok().filter(|n| !n.is_empty()),
        is_secure: false,
        ime_active: false,
        truncated,
        // SPIKE: S2 — this is the *element's* rectangle, not the caret's.
        // `IUIAutomationTextRange::GetBoundingRectangles` exists in COM but
        // `uiautomation` 0.25 does not wrap it, so §9's "anchor to the caret"
        // degrades to "anchor to the field" until that is either wrapped
        // upstream or reached through the raw interface.
        caret_bounds: element_bounds(&element),
    }))
}

/// A range at the caret, or `None`.
///
/// `GetSelection` is primary; `GetCaretRange` is consulted only when the
/// selection is unusable, because §8 records that Chromium — and so Chrome,
/// Edge, Electron, Slack and VS Code — has no `ITextProvider2` at all.
fn caret_range(pattern: &UITextPattern) -> Option<UITextRange> {
    if let Ok(ranges) = pattern.get_selection()
        && let Some(first) = ranges.into_iter().next()
    {
        return Some(first);
    }

    // Enhancement path. A QueryInterface failure here is expected across most
    // of the target surface and must never propagate as an error.
    match pattern.get_caret_range() {
        Ok((true, range)) => Some(range),
        Ok((false, _)) => None,
        Err(e) => {
            tracing::debug!(error = %e, "GetCaretRange unavailable (no ITextProvider2)");
            None
        }
    }
}

/// Walk one endpoint of `range` by `count` characters and read what now lies
/// between the endpoints.
///
/// Mutates `range` in place — see the note at the call site about why each call
/// gets its own freshly fetched range.
fn expand(
    range: &UITextRange,
    endpoint: TextPatternRangeEndpoint,
    count: i32,
    max_chars: i32,
) -> Option<String> {
    range
        .move_endpoint_by_unit(endpoint, TextUnit::Character, count)
        .ok()?;
    range.get_text(max_chars).ok().filter(|t| !t.is_empty())
}

fn element_bounds(element: &UIElement) -> Option<Rect> {
    let r = element.get_bounding_rectangle().ok()?;
    Some(Rect {
        x: f64::from(r.get_left()),
        y: f64::from(r.get_top()),
        width: f64::from(r.get_right() - r.get_left()),
        height: f64::from(r.get_bottom() - r.get_top()),
    })
}

fn focused_element_id(automation: &UIAutomation, of: &AppRef) -> WinResult<Option<String>> {
    let element = focused_in(automation, of)?;
    Ok(runtime_id(&element))
}

/// Opaque per-element identity, stable for as long as the element exists.
fn runtime_id(element: &UIElement) -> Option<String> {
    let id = element.get_runtime_id().ok()?;
    (!id.is_empty()).then(|| {
        id.iter()
            .map(|part| part.to_string())
            .collect::<Vec<_>>()
            .join(".")
    })
}

/// §8: "Validate the target before every insert, not just at capture."
///
/// Unlike the capture reads above, this one *does* consult the foreground —
/// deliberately. §8 orders the insert sequence `restore_focus` → `validate` →
/// paste, so by the time this runs the target has been confirmed frontmost
/// again and "is the foreground still the target?" is exactly the question
/// worth asking. Run before the restore it compares aibo's pid against the
/// target's and answers `false` every time.
fn validate_target(automation: &UIAutomation, target: &InsertTarget) -> WinResult<bool> {
    // The window and process must still be the ones that were captured.
    let Some(hwnd) = input::foreground_window() else {
        return Ok(false);
    };
    if let Some(expected) = target.app_ref.window
        && hwnd.0 as usize as u64 != expected
    {
        return Ok(false);
    }
    if input::foreground_pid() != Some(target.app_ref.pid as u32) {
        return Ok(false);
    }

    let element = automation.get_focused_element().map_err(uia_err)?;
    // Fail closed rather than falling back to the target's own focus window the
    // way capture does: if focus is somewhere else at paste time, pasting is the
    // unrecoverable move (§8).
    if !element
        .get_process_id()
        .is_ok_and(|pid| i64::from(pid) == i64::from(target.app_ref.pid))
    {
        return Ok(false);
    }

    // The focused element must still be the same one.
    if let Some(expected) = target.focused_element.as_deref()
        && runtime_id(&element).as_deref() != Some(expected)
    {
        return Ok(false);
    }

    // And the text must not have moved underneath us. This is the case that
    // makes a paste unrecoverable: same window, same field, different content.
    if let Some(expected) = target.selection_hash
        && let Ok(pattern) = text_pattern(&element)
        && let Ok(ranges) = pattern.get_selection()
    {
        let mut current = String::new();
        for range in &ranges {
            if let Ok(text) = range.get_text(SELECTION_MAX_CHARS) {
                current.push_str(&text);
            }
        }
        if text_hash(&current) != expected {
            return Ok(false);
        }
    }

    Ok(true)
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    fn selected_text_op() -> UiaOp {
        let (reply, _reply_rx) = oneshot::channel();
        UiaOp::SelectedText {
            of: AppRef {
                pid: 1,
                window: None,
            },
            reply,
        }
    }

    #[test]
    fn saturated_queue_fails_immediately_with_sanitized_error() {
        let (tx, mut rx) = mpsc::channel(1);
        let handle = UiaHandle { tx };
        let deadline = Instant::now() + std::time::Duration::from_secs(1);

        handle.submit(deadline, selected_text_op()).unwrap();
        let error = handle.submit(deadline, selected_text_op()).unwrap_err();

        assert!(matches!(
            error,
            WindowsPlatformError::WorkerBusy {
                worker: "UI Automation"
            }
        ));
        assert_eq!(error.to_string(), "the UI Automation worker queue is busy");
        assert!(rx.try_recv().is_ok(), "the first queued job is preserved");
    }

    #[test]
    fn closed_queue_reports_the_dead_worker() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let handle = UiaHandle { tx };

        assert!(matches!(
            handle.submit(Instant::now(), selected_text_op()),
            Err(WindowsPlatformError::UiaThreadGone)
        ));
    }
}
