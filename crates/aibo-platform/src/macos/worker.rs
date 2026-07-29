//! The state that lives on the dedicated platform thread.
//!
//! Nothing in this module is `Send`: `AxElement` holds a raw `AXUIElementRef`
//! and the type system is what keeps §7's "one dedicated AX thread" rule
//! honest. [`super::thread::PlatformThread`] owns the only instance.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

use aibo_core::types::{
    AppInfo, AppRef, ClipboardItem, FieldContext, InsertMode, InsertTarget, Rect,
};
use core_foundation::base::{CFIndex, CFRange};

use super::apps::{self, AxActivation};
use super::ax::AxElement;
use super::clipboard;
use super::error::{MacosError, MacosResult};
use super::keys;
use super::permissions;

/// §5: "the last ~800 characters before the caret". Measured in UTF-16 code
/// units because that is the unit `AXStringForRange` speaks.
const PREFIX_WINDOW_UTF16: CFIndex = 800;

/// Text after the caret, kept separate and labelled — §5 calls completing
/// without it "the single most common autocomplete failure". Half the prefix
/// window is enough to detect a duplicate continuation.
const SUFFIX_WINDOW_UTF16: CFIndex = 400;

/// Upper bound on a whole-value read.
///
/// `AXStringForRange` is a *parameterized* attribute and plenty of apps do not
/// implement it. The fallback is `kAXValueAttribute`, which returns the entire
/// field — §5 forbids doing that to a document. This cap keeps the fallback to
/// the "one text box" case it is meant for.
const WHOLE_VALUE_CAP_UTF16: CFIndex = 20_000;

/// How long to let the target app act on a synthetic `⌘V` before restoring the
/// clipboard.
///
/// SPIKE: S4 — this is a guess with no delivery confirmation behind it.
/// `CGEventPost` returns void; the only honest measurement is the spike.
const PASTE_SETTLE: Duration = Duration::from_millis(40);

/// Non-standard AX attributes some apps expose for an in-progress IME
/// composition.
///
/// SPIKE: S7 — §9 states plainly that **macOS has no clean cross-process API**
/// for composition state. These two keys are the only signal aibo has found;
/// when neither is present the answer below is `false`, which is a *guess*, not
/// a reading. If S7 cannot do better, the fallback in §20 applies: block insert
/// whenever the source app is in a known-IME state and document the limitation.
const MARKED_RANGE_ATTRIBUTES: [&str; 2] = ["AXMarkedRange", "AXTextInputMarkedRange"];

/// Configuration the worker needs at runtime.
#[derive(Debug, Clone)]
pub struct MacosConfig {
    /// Whether aibo may set `AXEnhancedUserInterface` / `AXManualAccessibility`
    /// on a target app to make its accessibility tree appear (§8).
    ///
    /// Default `false`. The Chrome flag "breaks window positioning and makes
    /// resizing sluggish, which is why Electron invented the alternative.
    /// Setting it from a tray utility is user-hostile; consider asking first."
    pub allow_ax_tree_activation: bool,

    /// AX messaging timeout applied to every application element, in seconds.
    ///
    /// The second half of the §8 deadline: the caller's `tokio` timeout bounds
    /// the *reply*, this bounds the *worker* so one hung app cannot block the
    /// next request.
    pub ax_messaging_timeout: Duration,
}

impl Default for MacosConfig {
    fn default() -> Self {
        Self {
            allow_ax_tree_activation: false,
            // Just inside §8's 120 ms AX budget.
            ax_messaging_timeout: Duration::from_millis(100),
        }
    }
}

/// Stable-within-a-process hash of captured content, for [`InsertTarget`].
///
/// `DefaultHasher` is not stable across Rust releases, which is fine and
/// deliberate: capture and validation always happen in the same process, and
/// [`InsertTarget`] is never persisted.
pub fn content_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Everything the dedicated thread owns.
pub(crate) struct Worker {
    config: MacosConfig,
    /// Apps whose AX tree aibo has already tried to activate, so the flag is
    /// written once per process rather than on every capture.
    activated: HashSet<i32>,
    /// `IsSecureEventInputEnabled()`, behind a function pointer.
    ///
    /// Indirection rather than a direct call so the §5 refusal path can be
    /// exercised in a unit test without a password manager, Terminal, or a
    /// stuck global flag on the developer's machine. Production always gets
    /// [`permissions::secure_input_active`]; nothing else can set this.
    secure_input: fn() -> bool,
}

impl Worker {
    pub(crate) fn new(config: MacosConfig) -> Self {
        Self::with_secure_input_probe(config, permissions::secure_input_active)
    }

    /// Same, but with the secure-input probe supplied. Test seam.
    pub(crate) fn with_secure_input_probe(config: MacosConfig, secure_input: fn() -> bool) -> Self {
        Self {
            config,
            activated: HashSet::new(),
            secure_input,
        }
    }

    // -- app identity --------------------------------------------------------

    /// The frontmost app and its window, without touching AX (§8 step 1).
    pub(crate) fn focused_app_ref() -> MacosResult<AppRef> {
        let (pid, _, _) = apps::frontmost_application()
            .ok_or_else(|| MacosError::Platform("no frontmost application".into()))?;
        Ok(AppRef {
            pid,
            window: super::display::frontmost_window_for_pid(pid).map(|w| w.id),
        })
    }

    /// Resolve full identity for the app snapshotted on hotkey-down.
    ///
    /// `of` is the subject, not a hint. §7: by the time this deferred call runs
    /// the panel is frontmost, so resolving `frontmost_application()` here would
    /// return aibo's own bundle id — which is what `is_code_app` routing (§4),
    /// §5's source-app prompt line and `Exchange::source_app` are all keyed on.
    pub(crate) fn focused_app(&mut self, of: &AppRef) -> MacosResult<AppInfo> {
        let (identifier, display_name) = Self::identity_of(of)?;
        Ok(AppInfo {
            app_ref: AppRef {
                pid: of.pid,
                // Prefer the id snapshotted in step 1; only look one up when the
                // snapshot had none. The lookup is per-pid, so it still cannot
                // wander onto aibo's panel.
                window: of
                    .window
                    .or_else(|| super::display::frontmost_window_for_pid(of.pid).map(|w| w.id)),
            },
            is_code_app: apps::is_code_app(&identifier),
            identifier,
            display_name,
        })
    }

    /// `(bundle identifier, localised name)` for the snapshotted app, or
    /// [`MacosError::TargetChanged`] when the process is gone.
    fn identity_of(of: &AppRef) -> MacosResult<(String, String)> {
        apps::application_identity(of.pid).ok_or(MacosError::TargetChanged)
    }

    // -- AX plumbing ---------------------------------------------------------

    /// The application element for the **snapshotted** process, with the
    /// AX-tree activation flag applied if policy allows it.
    fn focused_application_element(&mut self, of: &AppRef) -> MacosResult<(AxElement, String)> {
        permissions::require_accessibility()?;
        let (identifier, _) = Self::identity_of(of)?;
        let app = AxElement::application(of.pid)?;
        app.set_messaging_timeout(self.config.ax_messaging_timeout.as_secs_f32());
        self.activate_ax_tree(of.pid, &identifier, &app);
        Ok((app, identifier))
    }

    /// Apply the correct activation attribute for this app, once.
    ///
    /// §8: Chrome/Chromium honours `AXEnhancedUserInterface`, Electron honours
    /// `AXManualAccessibility`, and setting the wrong one returns
    /// `kAXErrorAttributeUnsupported`. Chrome's activation is **asynchronous**
    /// — the tree is empty for a while after the write — so the first capture
    /// after activation is expected to come back empty and the panel must
    /// tolerate that (§8 step 4). aibo does not sleep here: blocking the
    /// capture thread waiting for Chrome would blow the 120 ms budget.
    fn activate_ax_tree(&mut self, pid: i32, identifier: &str, app: &AxElement) {
        if !self.config.allow_ax_tree_activation || self.activated.contains(&pid) {
            return;
        }
        self.activated.insert(pid);
        let attribute = match apps::ax_activation_for(identifier) {
            AxActivation::EnhancedUserInterface => apps::AX_ENHANCED_USER_INTERFACE,
            AxActivation::ManualAccessibility => apps::AX_MANUAL_ACCESSIBILITY,
            AxActivation::None => return,
        };
        if let Err(err) = app.set_bool_attribute(attribute, true) {
            tracing::debug!(
                target: "aibo::platform::macos",
                %identifier, attribute, %err,
                "AX tree activation was rejected"
            );
        }
    }

    /// The focused UI element inside the **snapshotted** app.
    ///
    /// Asks that app's own application element first — it is per-pid, so its
    /// `kAXFocusedUIElementAttribute` is the element focused *within that app*
    /// regardless of who is frontmost, and it is also the element the
    /// activation flag was written to.
    ///
    /// The system-wide fallback exists because some apps answer it when their
    /// own application element does not — but `AXUIElementCreateSystemWide`'s
    /// focused element is the *globally* focused one, which during deferred
    /// capture is aibo's own panel field. So its answer is accepted only when it
    /// still belongs to `of`; otherwise it is exactly the wrong-application read
    /// this parameter exists to prevent (§7).
    fn focused_element(&mut self, of: &AppRef) -> MacosResult<(AxElement, String)> {
        let (app, identifier) = self.focused_application_element(of)?;
        if let Ok(element) = app.element_attribute(accessibility_sys::kAXFocusedUIElementAttribute)
        {
            return Ok((element, identifier));
        }
        let system_wide = AxElement::system_wide()?;
        system_wide.set_messaging_timeout(self.config.ax_messaging_timeout.as_secs_f32());
        let element =
            system_wide.element_attribute(accessibility_sys::kAXFocusedUIElementAttribute)?;
        if element.pid().ok() != Some(of.pid) {
            return Err(MacosError::Ax(
                "the system-wide focused element belongs to another application",
            ));
        }
        Ok((element, identifier))
    }

    /// AX roles that can hold editable text worth capturing.
    ///
    /// `AXWebArea` is included because Chromium and Safari report a focused
    /// contenteditable that way; §5's Complete surface is useless without it.
    ///
    /// SPIKE: S2 — the real list comes out of the app matrix.
    fn is_text_role(role: &str) -> bool {
        role == accessibility_sys::kAXTextFieldRole
            || role == accessibility_sys::kAXTextAreaRole
            || role == accessibility_sys::kAXComboBoxRole
            // Not in `accessibility-sys`, which stops at the roles Apple
            // documents; Chromium and WebKit both use it.
            || role == "AXWebArea"
    }

    /// Is this element a password field?
    ///
    /// A password field is `AXTextField` with the *subrole*
    /// `AXSecureTextField` — the role alone does not distinguish it, which is
    /// why §5 says "check the field's **role**", not "check that it is a text
    /// field".
    fn is_secure_field(element: &AxElement) -> bool {
        element
            .string_attribute(accessibility_sys::kAXSubroleAttribute)
            .is_ok_and(|subrole| subrole == accessibility_sys::kAXSecureTextFieldSubrole)
    }

    /// §5's capture gate: refuse before reading anything, not after.
    ///
    /// > Never capture from a secure or password field. Check the field's role
    /// > **and** `IsSecureEventInputEnabled()` before reading (§8); a password
    /// > that reaches prompt assembly has already left the machine by the time
    /// > anyone notices.
    ///
    /// The global flag is the half that catches what a subrole check cannot:
    /// Terminal in `read -s`, 1Password's unlock sheet, an app that took secure
    /// input and never gave it back. In every one of those the frontmost
    /// element is an ordinary `AXTextArea`/`AXTextField` — or aibo cannot see
    /// the tree at all — so the subrole says "safe" while the user is typing a
    /// credential.
    ///
    /// It is checked *first*, before any AX call and before the synthetic-`⌘C`
    /// fallback, because the fallback would copy the password onto the
    /// pasteboard, which is strictly worse than reading it.
    fn refuse_if_secure_input(&self) -> MacosResult<()> {
        if (self.secure_input)() {
            tracing::debug!(
                target: "aibo::platform::macos",
                "refusing: secure event input is enabled (§5, §8)"
            );
            return Err(MacosError::SecureInput);
        }
        Ok(())
    }

    // -- capture -------------------------------------------------------------

    /// `kAXSelectedTextAttribute`, with the synthetic-`⌘C` fallback.
    ///
    /// Note the attribute name: §8's table writes `kAXSelectedTextAttribute`,
    /// **not** `kAXSelectedText` — the `Attribute` suffix is the real constant
    /// and getting it wrong yields `kAXErrorAttributeUnsupported` at runtime.
    ///
    /// `budget` is the whole §8 allowance for this call (250 ms when the
    /// clipboard fallback is permitted). The AX attempt is bounded by the
    /// element's own messaging timeout; whatever is left goes to the poll.
    ///
    /// Both halves of §5's secure-field rule apply here — a selection inside a
    /// password field is a password, and the synthetic-`⌘C` fallback would put
    /// it on the pasteboard. Refusal is [`MacosError::SecureInput`], which
    /// reaches the caller as `CaptureFailed { reason: Denied }`; `Ok(None)`
    /// would be indistinguishable from "nothing was selected" and would let the
    /// panel invite a retry that must never succeed.
    pub(crate) fn selected_text(
        &mut self,
        of: &AppRef,
        budget: Duration,
        allow_clipboard_fallback: bool,
    ) -> MacosResult<Option<String>> {
        self.refuse_if_secure_input()?;

        let started = Instant::now();
        let element = match self.focused_element(of) {
            Ok((element, _)) => Some(element),
            // A missing AX tree is not fatal — the clipboard fallback may still
            // work, and §17's degraded mode depends on this not erroring out.
            Err(MacosError::NotTrusted) if !allow_clipboard_fallback => {
                return Err(MacosError::NotTrusted);
            }
            Err(_) => None,
        };

        // The role half of the rule. Checked even when the selection read below
        // would have come back empty, because the ⌘C fallback runs in exactly
        // that case and does not consult the element at all.
        if let Some(element) = &element
            && Self::is_secure_field(element)
        {
            tracing::debug!(
                target: "aibo::platform::macos",
                "refusing capture: the focused element is AXSecureTextField (§5)"
            );
            return Err(MacosError::SecureInput);
        }

        if let Some(element) = &element
            && let Ok(text) = element.string_attribute(accessibility_sys::kAXSelectedTextAttribute)
            && !text.is_empty()
        {
            return Ok(Some(text));
        }

        if !allow_clipboard_fallback {
            return Ok(None);
        }
        // A synthetic `⌘C` is delivered to whichever application is frontmost,
        // not to `of`. §8's deferred capture runs *after* the panel is up, so
        // unless the panel is genuinely non-activating (S1) the frontmost app is
        // aibo — and copying from there would attribute the user's typed panel
        // query to the target app *and* clobber their clipboard for it. There is
        // no way to aim the chord, so the only correct answer is to decline.
        if apps::frontmost_application().map(|(pid, _, _)| pid) != Some(of.pid) {
            tracing::debug!(
                target: "aibo::platform::macos",
                pid = of.pid,
                "skipping the synthetic-⌘C fallback: the snapshotted app is no longer frontmost, \
                 so the chord would read whatever is (§7, §8)"
            );
            return Ok(None);
        }
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(None);
        }
        self.selected_text_via_copy(remaining)
    }

    /// Synthesise `⌘C` and watch `changeCount`.
    ///
    /// macOS has no clipboard notification, so this is a poll. It "fails
    /// silently in apps that swallow the shortcut" (§8), which is why the
    /// return is `Ok(None)` rather than an error.
    fn selected_text_via_copy(&mut self, budget: Duration) -> MacosResult<Option<String>> {
        let saved = clipboard::snapshot();
        let baseline = saved.change_count;
        keys::press_copy()?;

        let Some(after_copy) = clipboard::wait_for_change(baseline, budget) else {
            // The app swallowed the chord. Nothing was written, so nothing to
            // restore.
            return Ok(None);
        };
        let text = clipboard::read_string();
        // §12: restore only if aibo is still looking at what the copy produced.
        let outcome = clipboard::restore(&saved, after_copy);
        tracing::trace!(
            target: "aibo::platform::macos",
            ?outcome,
            "clipboard restore after synthetic copy"
        );
        Ok(text.filter(|t| !t.is_empty()))
    }

    /// A bounded window of the focused text field (§5, §9).
    ///
    /// Two secure-field refusals, deliberately shaped differently:
    ///
    /// * **Global secure event input** — [`MacosError::SecureInput`], i.e.
    ///   `CaptureFailed { reason: Denied }`. aibo does not know what the
    ///   focused element is and must not claim to: the flag can be held by
    ///   Terminal, by a password manager, or by an app that leaked it, and
    ///   returning a `FieldContext { is_secure: true }` would assert "the
    ///   focused field is a password field", which is frequently false. The
    ///   honest answer is "aibo is not allowed to read right now".
    /// * **`AXSecureTextField`** — a [`FieldContext`] with empty
    ///   `prefix`/`suffix` and `is_secure` set. Here aibo *does* know, and §5's
    ///   second line of defence (`aibo_core::prompts::assemble`) is keyed on
    ///   that flag. The panel can say "this is a password field" instead of
    ///   showing an unexplained blank.
    ///
    /// Either way nothing is read.
    pub(crate) fn text_field_context(&mut self, of: &AppRef) -> MacosResult<Option<FieldContext>> {
        self.refuse_if_secure_input()?;

        let Ok((element, _)) = self.focused_element(of) else {
            return Ok(None);
        };

        // §5 forbids capturing from secure fields at all; `FieldContext`
        // documents that `prefix`/`suffix` MUST then be empty.
        if Self::is_secure_field(&element) {
            return Ok(Some(FieldContext {
                prefix: String::new(),
                suffix: String::new(),
                caret: None,
                label: element.label(),
                is_secure: true,
                ime_active: false,
                truncated: false,
                caret_bounds: None,
            }));
        }

        // A focused button or list is not a field. Bail before spending the
        // budget on parameterized attribute reads it will not answer.
        if !Self::is_text_role(&element.role().unwrap_or_default()) {
            return Ok(None);
        }

        // §9: during composition an AX read returns either the pre-composition
        // text or the uncommitted reading, and neither is what the user sees.
        if Self::ime_composition_active(&element) {
            return Ok(Some(FieldContext {
                prefix: String::new(),
                suffix: String::new(),
                caret: None,
                label: element.label(),
                is_secure: false,
                ime_active: true,
                truncated: false,
                caret_bounds: None,
            }));
        }

        let Ok(range) = element.selected_range() else {
            return Ok(None);
        };
        let caret = range.location.max(0);
        let selection_end = caret + range.length.max(0);
        let total = element
            .int_attribute(accessibility_sys::kAXNumberOfCharactersAttribute)
            .map(|n| n as CFIndex)
            .unwrap_or(selection_end);

        let prefix_start = (caret - PREFIX_WINDOW_UTF16).max(0);
        let prefix = self
            .read_range(&element, prefix_start, caret - prefix_start, total)
            .unwrap_or_default();
        let suffix_len = SUFFIX_WINDOW_UTF16.min((total - selection_end).max(0));
        let suffix = self
            .read_range(&element, selection_end, suffix_len, total)
            .unwrap_or_default();

        let truncated = prefix_start > 0 || selection_end + suffix_len < total;
        // `FieldContext::caret` is a *byte* offset into the field's full value.
        // That is only knowable when the prefix window started at 0; otherwise
        // the documented answer is `None` ("only a selection range was
        // available").
        let caret_byte = (prefix_start == 0).then_some(prefix.len());

        let caret_bounds = element
            .bounds_for_range(CFRange {
                location: caret,
                length: range.length.max(1),
            })
            .ok()
            .map(|r| Rect {
                x: r.origin.x,
                y: r.origin.y,
                width: r.size.width,
                height: r.size.height,
            });

        Ok(Some(FieldContext {
            prefix,
            suffix,
            caret: caret_byte,
            label: element.label(),
            is_secure: false,
            ime_active: false,
            truncated,
            caret_bounds,
        }))
    }

    /// Read `length` UTF-16 units starting at `location`.
    ///
    /// Prefers the parameterized `AXStringForRange`. Falls back to slicing
    /// `kAXValueAttribute` only when the field is small enough that reading it
    /// whole is not the document-scale read §5 prohibits.
    fn read_range(
        &self,
        element: &AxElement,
        location: CFIndex,
        length: CFIndex,
        total: CFIndex,
    ) -> Option<String> {
        if length <= 0 {
            return Some(String::new());
        }
        if let Ok(s) = element.string_for_range(CFRange { location, length }) {
            return Some(s);
        }
        if total > WHOLE_VALUE_CAP_UTF16 {
            return None;
        }
        let value = element
            .string_attribute(accessibility_sys::kAXValueAttribute)
            .ok()?;
        let units: Vec<u16> = value.encode_utf16().collect();
        let start = usize::try_from(location).ok()?.min(units.len());
        let end = start
            .saturating_add(usize::try_from(length).ok()?)
            .min(units.len());
        Some(String::from_utf16_lossy(&units[start..end]))
    }

    /// Best-effort IME composition detection.
    ///
    /// SPIKE: S7 — see [`MARKED_RANGE_ATTRIBUTES`]. A `false` here means "no
    /// evidence of composition", not "no composition".
    fn ime_composition_active(element: &AxElement) -> bool {
        MARKED_RANGE_ATTRIBUTES
            .iter()
            .any(|attr| element.has_attribute(attr))
    }

    /// The clipboard, with §12 hygiene applied.
    ///
    /// macOS does not report the pasteboard's owner, so the best available
    /// attribution is the app that had focus when the hotkey fired — which is
    /// `owner_hint`, **not** the frontmost app. Attributing it to the frontmost
    /// app made §12's denylist inert: at capture time the frontmost app is
    /// aibo, and aibo is on nobody's password-manager denylist, so a copy taken
    /// out of 1Password was recorded as ordinary text.
    pub(crate) fn clipboard(&mut self, owner_hint: &AppRef) -> MacosResult<ClipboardItem> {
        let source = apps::application_identity(owner_hint.pid).map(|(identifier, _)| identifier);
        Ok(clipboard::read(source.as_deref()))
    }

    // -- write-back ----------------------------------------------------------

    /// Insert `text`, atomically from the user's point of view (§13).
    pub(crate) fn insert_text(&mut self, text: &str, mode: InsertMode) -> MacosResult<()> {
        // Same gate as capture, same probe — §8: while the flag is held, both
        // keystroke synthesis and paste are dropped silently.
        self.refuse_if_secure_input()?;
        match mode {
            InsertMode::Keystroke => keys::type_string(text),
            InsertMode::PasteAndRestore | InsertMode::PasteKeepNew => {
                let saved = clipboard::snapshot();
                let ours = clipboard::write_transient_text(text);
                keys::press_paste()?;
                // No delivery confirmation exists; give the target a moment
                // before touching the clipboard again.
                std::thread::sleep(PASTE_SETTLE);
                if matches!(mode, InsertMode::PasteAndRestore) {
                    let outcome = clipboard::restore(&saved, ours);
                    tracing::trace!(
                        target: "aibo::platform::macos",
                        ?outcome,
                        "clipboard restore after paste"
                    );
                }
                Ok(())
            }
        }
    }

    /// Replace the selection. On macOS this is the same synthetic paste — the
    /// target app deletes the selection for us.
    pub(crate) fn replace_selection(&mut self, text: &str) -> MacosResult<()> {
        self.insert_text(text, InsertMode::PasteAndRestore)
    }

    /// Confirm everything captured is still true (§8).
    ///
    /// Returns `false`, never an error, for a *changed* target: the caller's
    /// response is "copy instead", not a failure dialogue.
    ///
    /// **This runs after `restore_focus` has confirmed the target is frontmost
    /// again**, which is why comparing against `frontmost_application()` is
    /// correct here and wrong in the capture methods above. Called before the
    /// restore it would compare aibo's pid against the target's and answer
    /// `false` every time, turning every insert into "target changed, copy
    /// instead" (§8).
    pub(crate) fn validate_target(&mut self, target: &InsertTarget) -> MacosResult<bool> {
        let Some((pid, _, _)) = apps::frontmost_application() else {
            return Ok(false);
        };
        if pid != target.app_ref.pid {
            return Ok(false);
        }
        if let Some(expected_window) = target.app_ref.window {
            let live = super::display::frontmost_window_for_pid(pid).map(|w| w.id);
            if live != Some(expected_window) {
                return Ok(false);
            }
        }

        // Element and content checks need AX. Without the grant aibo cannot
        // insert anyway, so a missing tree is a "do not insert" answer.
        let Ok((element, _)) = self.focused_element(&target.app_ref) else {
            return Ok(target.focused_element.is_none()
                && target.selection_hash.is_none()
                && target.prefix_hash.is_none());
        };

        // The focused element must still belong to the captured process. A pid
        // can be recycled and `AXUIElement` identity is not globally unique, so
        // this is checked directly rather than inferred.
        if element.pid().ok() != Some(target.app_ref.pid) {
            return Ok(false);
        }
        // An element that no longer reports itself focused cannot receive the
        // paste, whatever the frontmost app says.
        if element
            .bool_attribute(accessibility_sys::kAXFocusedAttribute)
            .is_ok_and(|focused| !focused)
        {
            return Ok(false);
        }
        if let Some(expected) = &target.focused_element
            && element.identity() != *expected
        {
            return Ok(false);
        }
        if let Some(expected) = target.selection_hash {
            let live = element
                .string_attribute(accessibility_sys::kAXSelectedTextAttribute)
                .unwrap_or_default();
            if content_hash(&live) != expected {
                return Ok(false);
            }
        }
        if let Some(expected) = target.prefix_hash {
            let Ok(range) = element.selected_range() else {
                return Ok(false);
            };
            let caret = range.location.max(0);
            let total = element
                .int_attribute(accessibility_sys::kAXNumberOfCharactersAttribute)
                .map(|n| n as CFIndex)
                .unwrap_or(caret);
            let start = (caret - PREFIX_WINDOW_UTF16).max(0);
            let live = self
                .read_range(&element, start, caret - start, total)
                .unwrap_or_default();
            if content_hash(&live) != expected {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Give focus back to `prev` and **confirm** it landed (§8).
    ///
    /// "An unconfirmed restore races and pastes into the wrong window, which is
    /// the most damaging bug this product can ship." The loop below re-issues
    /// the activation periodically and only returns `Ok` once `NSWorkspace`
    /// agrees the pid is frontmost.
    pub(crate) fn restore_focus(&mut self, prev: &AppRef, budget: Duration) -> MacosResult<()> {
        const POLL: Duration = Duration::from_millis(8);
        const REACTIVATE_EVERY: Duration = Duration::from_millis(60);

        let app = apps::running_application_for_pid(prev.pid).ok_or(MacosError::TargetChanged)?;
        let started = Instant::now();
        let mut last_activate = Instant::now();
        apps::activate(&app);

        loop {
            if let Some((pid, _, _)) = apps::frontmost_application()
                && pid == prev.pid
            {
                return Ok(());
            }
            if started.elapsed() >= budget {
                return Err(MacosError::AppRejected);
            }
            if last_activate.elapsed() >= REACTIVATE_EVERY {
                apps::activate(&app);
                last_activate = Instant::now();
            }
            std::thread::sleep(POLL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aibo_core::error::{AiboError, CaptureFailure, InsertFailure};

    /// Stands in for `IsSecureEventInputEnabled() == true`: a password field is
    /// focused, or Terminal / a password manager is holding the flag.
    fn secure_input_held() -> bool {
        true
    }

    /// The ordinary case.
    fn secure_input_clear() -> bool {
        false
    }

    fn worker(probe: fn() -> bool) -> Worker {
        Worker::with_secure_input_probe(MacosConfig::default(), probe)
    }

    /// The app that had focus when the hotkey fired. Not this process — that is
    /// the whole point of §7's `of` parameter.
    fn snapshot() -> AppRef {
        AppRef {
            // Deliberately a pid that is not aibo's and, on any sane machine,
            // not a running app either: every read attributed to it must fail
            // or come back empty rather than quietly answering about aibo.
            pid: 999_999,
            window: Some(4242),
        }
    }

    /// §5: the capture path must consult `IsSecureEventInputEnabled()`, not just
    /// the AX subrole. This is the regression this test exists for — the gate
    /// used to live only on the insert path.
    #[test]
    fn selected_text_refuses_while_secure_input_is_held() {
        let mut w = worker(secure_input_held);
        // Both budgets: 120 ms is "AX only", 250 ms additionally licenses the
        // synthetic-⌘C fallback. Neither may read.
        assert!(matches!(
            w.selected_text(&snapshot(), super::super::AX_DEADLINE, false),
            Err(MacosError::SecureInput)
        ));
        assert!(matches!(
            w.selected_text(&snapshot(), super::super::CAPTURE_DEADLINE, true),
            Err(MacosError::SecureInput)
        ));
    }

    #[test]
    fn text_field_context_refuses_while_secure_input_is_held() {
        let mut w = worker(secure_input_held);
        assert!(matches!(
            w.text_field_context(&snapshot()),
            Err(MacosError::SecureInput)
        ));
    }

    #[test]
    fn insert_refuses_while_secure_input_is_held() {
        let mut w = worker(secure_input_held);
        assert!(matches!(
            w.insert_text("x", InsertMode::PasteAndRestore),
            Err(MacosError::SecureInput)
        ));
        assert!(matches!(
            w.replace_selection("x"),
            Err(MacosError::SecureInput)
        ));
    }

    /// The refusal must be *typed*, not an empty success: §5's whole point is
    /// that the panel says why rather than showing a silent blank, and a
    /// `CaptureFailed` is what the failure model (§13) renders.
    ///
    /// It must also be typed **as secure input rather than as denial**. Both
    /// end in "aibo could not read that", but the recoveries are opposite:
    /// `Denied` renders a banner pointing at the Accessibility pane, and when
    /// secure input is the cause that checkbox is already ticked — so the user
    /// is sent to fix a setting that is not broken, with no way to tell the app
    /// is wrong rather than themselves. Secure input has no user action at all.
    #[test]
    fn secure_input_is_reported_as_itself_not_as_a_permission_denial() {
        let err = MacosError::SecureInput.into_capture_error("com.apple.Terminal");
        assert!(matches!(
            err,
            AiboError::CaptureFailed {
                reason: CaptureFailure::SecureInput,
                ..
            }
        ));
        assert!(matches!(
            MacosError::SecureInput.into_insert_error(),
            AiboError::InsertFailed {
                reason: InsertFailure::SecureInput,
            }
        ));

        // The missing-permission case keeps pointing at the pane, because there
        // it is the correct advice.
        assert!(matches!(
            MacosError::NotTrusted.into_capture_error("com.apple.Terminal"),
            AiboError::CaptureFailed {
                reason: CaptureFailure::Denied,
                ..
            }
        ));
        assert!(matches!(
            MacosError::NotTrusted.into_insert_error(),
            AiboError::InsertFailed {
                reason: InsertFailure::PermissionDenied,
            }
        ));
    }

    /// The gate must be conditional. With the flag clear the call proceeds to
    /// the ordinary AX path, whose answer on an untrusted test binary is
    /// `Ok(None)` — the one thing it must never be is a secure-input refusal.
    ///
    /// `selected_text` is exercised with the fallback *disabled* on purpose: an
    /// enabled fallback would synthesise ⌘C and clobber the developer's
    /// pasteboard from a unit test.
    #[test]
    fn a_clear_flag_does_not_refuse() {
        let mut w = worker(secure_input_clear);
        assert!(!matches!(
            w.text_field_context(&snapshot()),
            Err(MacosError::SecureInput)
        ));
        assert!(!matches!(
            w.selected_text(&snapshot(), super::super::AX_DEADLINE, false),
            Err(MacosError::SecureInput)
        ));
    }

    /// Production wiring: the default constructor must use the real Carbon
    /// probe. A test seam that the product does not actually take is worthless.
    #[test]
    fn the_default_probe_is_the_real_one() {
        let w = Worker::new(MacosConfig::default());
        let real: fn() -> bool = permissions::secure_input_active;
        assert!(std::ptr::fn_addr_eq(w.secure_input, real));
    }

    // -- §7: deferred capture reads the snapshotted app, not the frontmost one -

    /// **F1 regression.** `focused_app` must describe `of`.
    ///
    /// The test binary *is* the frontmost-ish process here, standing in for
    /// aibo's panel. The old implementation called `frontmost_application()` and
    /// happily returned this process's identity for any `of` at all; the fixed
    /// one refuses, because pid 999_999 is not running. What must never happen
    /// is an `Ok(AppInfo)` describing somebody other than `of` — that is the
    /// value that reaches `is_code_app` routing (§4), §5's source-app prompt
    /// line and `Exchange::source_app`.
    #[test]
    fn focused_app_describes_the_snapshotted_app_not_the_frontmost_one() {
        let mut w = worker(secure_input_clear);
        let of = snapshot();
        match w.focused_app(&of) {
            Ok(info) => {
                assert_eq!(
                    info.app_ref.pid, of.pid,
                    "focused_app answered about a different application than the one it was \
                     handed — this is the deferred-capture bug in §7"
                );
                assert_ne!(
                    info.app_ref.pid,
                    std::process::id() as i32,
                    "focused_app attributed the capture to aibo itself"
                );
            }
            Err(MacosError::TargetChanged) => {}
            Err(other) => panic!("unexpected failure: {other}"),
        }
    }

    /// **F1 regression, the live half.** Even for a pid that *is* running, the
    /// answer is about that pid — never about this process.
    ///
    /// aibo's own pid is used as the snapshot here precisely because it is the
    /// one identity a frontmost-based implementation would also produce; the
    /// assertion is on the pid actually reported back, so an implementation that
    /// re-resolves "frontmost" fails the moment the frontmost app is anything
    /// else.
    #[test]
    fn focused_app_reports_the_pid_it_was_given() {
        let mut w = worker(secure_input_clear);
        let of = AppRef {
            pid: std::process::id() as i32,
            window: None,
        };
        if let Ok(info) = w.focused_app(&of) {
            assert_eq!(info.app_ref.pid, of.pid);
        }
    }

    /// **F3 regression.** §12's clipboard denylist is matched against the app
    /// that had focus on hotkey-down, so the read must be attributed to
    /// `owner_hint`.
    ///
    /// Attributing it to the frontmost app is what made the denylist inert: at
    /// capture time the frontmost app is aibo, and aibo matches no
    /// password-manager prefix, so a 1Password copy was recorded as plain text.
    /// A dead pid yields `None` — "unknown owner" — which is honest; what it
    /// must never be is *this* process's identity.
    #[test]
    fn the_clipboard_is_attributed_to_the_owner_hint() {
        let mut w = worker(secure_input_clear);
        let item = w.clipboard(&snapshot()).expect("clipboard read");
        assert_eq!(
            item.source_app, None,
            "the clipboard was attributed to an app other than the owner hint; §12's denylist \
             matches on this field and can never fire if it names aibo"
        );
    }

    /// The denylist has to be reachable *through* that attribution, or F3 is
    /// only half fixed: a hint naming a password manager must conceal.
    #[test]
    fn a_denylisted_owner_hint_would_conceal() {
        assert!(
            apps::is_clipboard_denylisted("com.1password.1password"),
            "the denylist must match the identifier the owner hint resolves to"
        );
        assert!(!apps::is_clipboard_denylisted("com.aibo.aibo"));
    }
}
