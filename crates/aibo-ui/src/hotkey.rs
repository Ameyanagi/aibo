//! The global hotkey: per-platform defaults, registration, failure handling (§8, §9).
//!
//! §9 resolves open question 1 and the resolution is not cosmetic:
//!
//! * **macOS default `⌥Space`.**
//! * **Windows default `Ctrl+Shift+Space`.** `Alt+Space` is *not* viable there:
//!   it opens the Win32 system menu in every window, and `RegisterHotKey` will
//!   happily take it and globally break that shortcut.
//!
//! §9 also scopes the spec down, and the picker must not imply otherwise:
//! `RegisterHotKey` supports **one key plus modifiers** — no sequences, no
//! double-taps, no left/right modifier distinction. macOS 15 is additionally
//! *reported* to refuse shift/option-only combinations (§8) — a caution the
//! picker shows as a soft warning, including on the shipped `⌥Space` default,
//! which §8 measured registering successfully on macOS 26.2.
//!
//! Registration failure is a first-class state, not an `unwrap`. On a machine
//! with Raycast, Alfred or PowerToys Run already bound to the same combination
//! the register call fails and the app is otherwise unreachable — so the
//! failure surfaces in the panel and in settings (§9 "conflict detection at
//! first run"), and it surfaces *classified* ([`FailureReason`]): §8 names
//! "choose different modifiers" and "another app owns this shortcut" as the two
//! messages a user actually needs, and a raw platform string is neither.
//!
//! # Conflict detection is mandatory, not a nicety
//!
//! §9 resolves the default as `⌥Space` "with Raycast/Alfred conflict detection
//! at first run". Measurement on a Japanese-configured Mac (2026-07-26) says
//! that phrasing understates it — **every obvious default is already taken**:
//!
//! | Combination | Owner |
//! |---|---|
//! | `⌥Space` | Raycast is running and the user reported interference — but see below |
//! | `⌃Space` | macOS symbolic hotkey **60**, *Select the previous input source*, enabled |
//! | `⌃⌥Space` | macOS symbolic hotkey **61**, *Select the next source in the Input menu*, enabled |
//!
//! On a CJK-configured Mac the input-source switcher owns the control-space
//! family and launchers own option-space. So "pick a good uncontested default"
//! is not a thing this module can do by choosing a constant — it has to *look*.
//!
//! **What this code measured when it was pointed at that machine**, which is
//! not identical to the report it was written from and the difference matters:
//!
//! * `⌃Space` and `⌃⌥Space` are owned by symbolic hotkeys 60 and 61, exactly as
//!   reported. Found through the preference file.
//! * `⌥Space` **probes free**, and Raycast's own preferences give its global
//!   hotkey as `Shift-Command-36` — `⇧⌘↩`, not `⌥Space`. Either the
//!   interference the user hit came from something other than Raycast's main
//!   hotkey, or Raycast takes the key by an `NSEvent` monitor or `CGEventTap`
//!   rather than `RegisterEventHotKey`. **A tap is invisible to a probe**: it
//!   sees the key first and never contends for the registration. So a `Free`
//!   probe is "no Carbon registration and no known system shortcut", not "this
//!   key will reach aibo" — [`ProbeOutcome::Free`] is deliberately not called
//!   `Available`.
//! * A duplicate `RegisterEventHotKey` does fail, so the probe mechanism itself
//!   is real. It fails as `Unclassified`, not `AlreadyOwned`: §8's documented
//!   `global-hotkey` 0.8 gap is the *ordinary* macOS path, not an edge case,
//!   which is why [`ConflictReport::summary`] refuses to guess between
//!   `-9878` and `-9868` there.
//!
//! Two mechanisms, because neither alone is sufficient:
//!
//! 1. **The `com.apple.symbolichotkeys` preference domain**
//!    ([`SymbolicHotkeyMap`]). System shortcuts are consumed by the
//!    WindowServer *before* Carbon dispatches to `RegisterEventHotKey`
//!    handlers, so a probe registration of `⌃Space` **succeeds** and the
//!    hotkey then never fires. Only reading the preference finds these.
//! 2. **A probe registration that is immediately released** ([`ProbeOutcome`]).
//!    Another *application* holding the combination is invisible in any
//!    preference file; it shows up as `-9878 eventHotKeyExistsErr` and nowhere
//!    else.
//!
//! [`check_candidate`] runs both and returns a typed [`ConflictReport`];
//! [`suggest_free_binding`] walks a ranked list of candidates and returns the
//! first genuinely-free one *plus the reason every rejected one was rejected*,
//! which is the part the settings pane needs in order to say something more
//! useful than "try another shortcut".

use std::collections::BTreeMap;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::error::{Result, UiError};

/// What a registered hotkey does.
///
/// One action per binding. The panel hotkey is the only one that is
/// mandatory; the rest are opt-in from settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    /// Show the panel, capturing context for the frontmost app.
    ///
    /// §13: pressing this during an agent run does **not** interrupt it — the
    /// run continues in the task window and a fresh panel opens.
    TogglePanel,
    /// Interactively crop a screen region and open it as a deliberate image
    /// attachment in a fresh panel.
    CaptureScreenRegion,
    /// Bring the task window forward.
    ShowTasks,
    /// Re-insert the pre-transform original ("revert last transform", §13).
    RevertLastTransform,
}

/// A parsed, displayable binding.
#[derive(Debug, Clone)]
pub struct Binding {
    /// What it does.
    pub action: HotkeyAction,
    /// The combination itself.
    pub hotkey: HotKey,
    /// Platform-idiomatic label for the UI: `⌥Space`, `Ctrl+Shift+Space`.
    pub display: String,
}

impl Binding {
    /// Build a binding, deriving the display label from the combination.
    pub fn new(action: HotkeyAction, hotkey: HotKey) -> Self {
        let display = describe(&hotkey);
        Self {
            action,
            hotkey,
            display,
        }
    }

    /// The platform default for `action`, or `None` if the action has no
    /// default binding.
    pub fn default_for(action: HotkeyAction) -> Option<Self> {
        match action {
            HotkeyAction::TogglePanel => Some(Self::new(action, default_panel_hotkey())),
            HotkeyAction::CaptureScreenRegion => {
                default_screen_capture_hotkey().map(|hotkey| Self::new(action, hotkey))
            }
            // No defaults: both would collide with common editor bindings, and
            // §9 forbids shipping a picker that implies more than
            // `RegisterHotKey` can express.
            HotkeyAction::ShowTasks | HotkeyAction::RevertLastTransform => None,
        }
    }
}

/// The crop-and-ask shortcut.
///
/// On macOS this extends the panel's `⌥Space` with Shift: `⌥⇧Space`. Other
/// platforms keep it unbound until their native picker path is implemented.
pub fn default_screen_capture_hotkey() -> Option<HotKey> {
    #[cfg(target_os = "macos")]
    {
        Some(HotKey::new(
            Some(Modifiers::ALT | Modifiers::SHIFT),
            Code::Space,
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// The default panel hotkey for the current platform (§9).
///
/// `⌥Space` on macOS; `Ctrl+Shift+Space` on Windows, because `Alt+Space` is the
/// Win32 system menu. Other platforms follow the Windows default — they are not
/// v1 targets, and it is the more conservative of the two.
pub fn default_panel_hotkey() -> HotKey {
    #[cfg(target_os = "macos")]
    {
        HotKey::new(Some(Modifiers::ALT), Code::Space)
    }
    #[cfg(not(target_os = "macos"))]
    {
        HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space)
    }
}

/// Parse a stored combination such as `alt+Space` or `control+shift+Space`.
///
/// §9: key **codes are keyboard-layout dependent** — a combination bound on a
/// JIS layout may land elsewhere on US-QWERTY. Storing the `Code` (physical
/// position) rather than the produced character is the lesser of the two evils,
/// but it is not free, and the settings picker must show the resolved label.
pub fn parse(spec: &str) -> Result<HotKey> {
    spec.parse::<HotKey>()
        .map_err(|_| UiError::HotkeyParse(spec.to_owned()))
}

/// A platform-idiomatic label for a combination.
pub fn describe(hotkey: &HotKey) -> String {
    let mods = hotkey.mods;
    let mut out = String::new();

    #[cfg(target_os = "macos")]
    {
        if mods.contains(Modifiers::CONTROL) {
            out.push('⌃');
        }
        if mods.contains(Modifiers::ALT) {
            out.push('⌥');
        }
        if mods.contains(Modifiers::SHIFT) {
            out.push('⇧');
        }
        if mods.contains(Modifiers::SUPER) {
            out.push('⌘');
        }
        out.push_str(&key_label(hotkey.key));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut push = |part: &str| {
            if !out.is_empty() {
                out.push('+');
            }
            out.push_str(part);
        };
        if mods.contains(Modifiers::CONTROL) {
            push("Ctrl");
        }
        if mods.contains(Modifiers::ALT) {
            push("Alt");
        }
        if mods.contains(Modifiers::SHIFT) {
            push("Shift");
        }
        if mods.contains(Modifiers::SUPER) {
            push("Win");
        }
        let key = key_label(hotkey.key);
        push(&key);
    }

    out
}

fn key_label(code: Code) -> String {
    match code {
        Code::Space => "Space".to_owned(),
        Code::Enter => "↩".to_owned(),
        Code::Escape => "Esc".to_owned(),
        other => {
            let raw = format!("{other:?}");
            // `KeyA` -> `A`, `Digit1` -> `1`; anything else is shown verbatim.
            raw.strip_prefix("Key")
                .or_else(|| raw.strip_prefix("Digit"))
                .unwrap_or(&raw)
                .to_owned()
        }
    }
}

/// Whether a combination falls under the macOS shift/option-only caution.
///
/// The rule as §8 and §9 actually state it: an anti-keylogger change is
/// *reported* to make macOS 15+ refuse registrations whose **only** modifiers
/// are shift and/or option, with `-9868`. Nothing in the plan restricts that to
/// combinations whose key is itself a modifier — that extra conjunct was an
/// invention, and since `RegisterEventHotKey` cannot bind a bare modifier
/// anyway it narrowed the rule to a set nobody can register, i.e. to nothing.
///
/// So this returns `true` for `⌥Space` — **the shipped macOS default** (§9) —
/// and that is correct rather than a bug to design around. §8 measured `⌥Space`
/// registering successfully on macOS 26.2 (first call `0`, duplicate `-9878`),
/// so the rule is a **caution, not a certainty**: the picker shows it as a soft
/// warning ([`Caution::ShiftOrOptionOnly`]) and registration is still attempted.
/// A hard block here, or a rule bent until the default passes, is the failure
/// mode §9 calls out by name.
///
/// Pure and platform-independent so it is testable everywhere; whether the
/// caution is *shown* is the macOS-only decision made by [`caution_for`].
pub fn is_risky_on_macos(hotkey: &HotKey) -> bool {
    let mods = hotkey.mods;
    let has_shift_or_alt = mods.contains(Modifiers::SHIFT) || mods.contains(Modifiers::ALT);
    let has_control_or_super = mods.contains(Modifiers::CONTROL) || mods.contains(Modifiers::SUPER);
    has_shift_or_alt && !has_control_or_super
}

/// A registration that succeeded but is worth warning about (§9 "soft
/// warning ... rather than a hard rejection").
///
/// Distinct from [`FailureReason`]: the hotkey **is** registered and working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caution {
    /// Only shift and/or option are held down. Reported to be refused by some
    /// macOS 15+ releases, and observed to work on 26.2 (§8).
    ShiftOrOptionOnly,
}

impl Caution {
    /// One line of explanatory copy.
    ///
    /// TODO(§9 i18n): this is the one user-visible literal left in the crate.
    /// `i18n::Key` has no variant for a *soft* hotkey caution — `HotkeyRejectedByOs`
    /// asserts outright refusal, which is false for the shipped default and so
    /// cannot be reused here. Adding `Key::HotkeyOptionOnlyCaution` to
    /// `i18n.rs` (en + ja) replaces this method body.
    pub const fn explanation(self) -> &'static str {
        match self {
            Caution::ShiftOrOptionOnly => {
                "Some macOS releases refuse shortcuts that use only shift or option. \
                 This one registered; if it ever stops working, add ⌃ or ⌘."
            }
        }
    }
}

/// Whether a *successful* registration should still carry a caution.
///
/// macOS-only: the shift/option rule is a macOS anti-keylogger behaviour, and
/// showing it on Windows would be noise.
pub fn caution_for(hotkey: &HotKey) -> Option<Caution> {
    #[cfg(target_os = "macos")]
    {
        is_risky_on_macos(hotkey).then_some(Caution::ShiftOrOptionOnly)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = hotkey;
        None
    }
}

/// Whether a combination would break a documented OS shortcut if taken.
///
/// The entries are the ones where **succeeding is worse than failing**:
/// `RegisterHotKey` accepts them and globally breaks a shortcut every window
/// relies on, with no error to report. So they are checked *before* registering
/// rather than classified afterwards.
///
/// * `Alt+Space` — the Win32 system menu, called out by name in §9.
/// * `Win+Space` — the Windows input-source switcher. This is the exact
///   counterpart of the macOS measurement in the module docs: symbolic hotkeys
///   60/61 own `⌃Space`/`⌃⌥Space` there for the same reason. Taking it on a
///   Japanese-configured PC removes the user's only way to switch between kana
///   and direct input, which is worse for that user than aibo having no
///   shortcut at all.
///
/// macOS system shortcuts are *not* listed here. They are per-machine and
/// user-editable, so they are discovered at runtime from
/// `com.apple.symbolichotkeys` ([`SymbolicHotkeyMap`]) rather than hardcoded.
pub fn breaks_os_shortcut(hotkey: &HotKey) -> Option<&'static str> {
    #[cfg(target_os = "windows")]
    {
        let mods = hotkey.mods;
        if hotkey.key == Code::Space
            && mods.contains(Modifiers::ALT)
            && !mods.contains(Modifiers::CONTROL)
            && !mods.contains(Modifiers::SHIFT)
            && !mods.contains(Modifiers::SUPER)
        {
            return Some("Alt+Space opens the window system menu on Windows");
        }
        if hotkey.key == Code::Space
            && mods.contains(Modifiers::SUPER)
            && !mods.contains(Modifiers::CONTROL)
            && !mods.contains(Modifiers::ALT)
            && !mods.contains(Modifiers::SHIFT)
        {
            return Some("Win+Space switches the input source on Windows");
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = hotkey;
        None
    }
}

/// macOS `-9868`: the modifier combination itself was refused.
const OS_STATUS_MODIFIERS_REJECTED: i32 = -9868;
/// macOS `-9878 eventHotKeyExistsErr`: the combination is already held.
const OS_STATUS_HOT_KEY_EXISTS: i32 = -9878;
/// Win32 `ERROR_HOTKEY_ALREADY_REGISTERED`.
const WIN32_HOTKEY_ALREADY_REGISTERED: i32 = 1409;

/// Why a registration was refused — typed, because the two cases need
/// *different* instructions and a raw platform string gives neither (§8).
///
/// §8: "`global-hotkey` 0.8 gives only `Error::FailedToRegister(String)`, so
/// `-9868` ('choose different modifiers') is indistinguishable from `-9878`
/// ('another app owns this shortcut'), which are the two messages a user
/// actually needs. Parse the string or patch the crate."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureReason {
    /// `-9868`. The *modifiers* are the problem. Ask for different modifiers.
    ModifiersRejected,
    /// `-9878` / `ERROR_HOTKEY_ALREADY_REGISTERED`. The combination is the
    /// problem. Another app already owns it; quit it or pick another.
    AlreadyOwned,
    /// aibo refused to take it: registering would break a documented OS
    /// shortcut ([`breaks_os_shortcut`]).
    BreaksOsShortcut(&'static str),
    /// A failure that carries no code we can classify — including, today, every
    /// macOS `RegisterEventHotKey` failure (see [`FailureReason::from_register_error`]).
    /// Already redacted for display.
    Unclassified(String),
}

impl FailureReason {
    /// Classify a `global-hotkey` registration error.
    ///
    /// **Known gap, and it is upstream, not here.** `global-hotkey` 0.8.0's
    /// macOS backend throws the `OSStatus` away before building the error:
    ///
    /// ```text
    /// if result != noErr as _ {
    ///     return Err(Error::FailedToRegister(format!(
    ///         "RegisterEventHotKey failed for {}", hotkey.key)));
    /// }
    /// ```
    ///
    /// There is no number in that string, so on macOS the parse below cannot
    /// separate `-9868` from `-9878` no matter how it is written, and every
    /// macOS registration failure lands in [`FailureReason::Unclassified`].
    /// §8 offers two routes and only the second closes this on macOS: "parse
    /// the string **or patch the crate**". Patching means a `[patch.crates-io]`
    /// fork carrying the `OSStatus` through — a workspace `Cargo.toml` change,
    /// outside this module. The parse is implemented and correct for any
    /// backend that does include the code (and for the Windows paths, which
    /// surface theirs as typed variants); it is not a claim that macOS is
    /// covered.
    pub fn from_register_error(error: &global_hotkey::Error) -> Self {
        use global_hotkey::Error as E;
        match error {
            // Windows maps ERROR_HOTKEY_ALREADY_REGISTERED to this variant, and
            // macOS uses it for a media key aibo already holds. Either way the
            // combination is taken.
            E::AlreadyRegistered(_) => FailureReason::AlreadyOwned,
            E::OsError(io) if io.raw_os_error() == Some(WIN32_HOTKEY_ALREADY_REGISTERED) => {
                FailureReason::AlreadyOwned
            }
            other => {
                let message = other.to_string();
                match parse_os_status(&message) {
                    Some(OS_STATUS_MODIFIERS_REJECTED) => FailureReason::ModifiersRejected,
                    Some(OS_STATUS_HOT_KEY_EXISTS) => FailureReason::AlreadyOwned,
                    _ => FailureReason::Unclassified(message),
                }
            }
        }
    }
}

/// Pull a negative status code out of a platform error string.
///
/// Only *negative* runs of digits are considered: `OSStatus` values are
/// negative, while positive numbers in these messages are key names
/// (`Digit1`, `F13`) and would classify garbage.
fn parse_os_status(message: &str) -> Option<i32> {
    let bytes = message.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'-' {
            let start = at;
            let mut end = at + 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > start + 1
                && let Ok(code) = message[start..end].parse::<i32>()
            {
                return Some(code);
            }
            at = end.max(at + 1);
        } else {
            at += 1;
        }
    }
    None
}

/// The outcome of trying to install the bindings, kept in app state so the UI
/// can show it (§9 conflict detection).
#[derive(Debug, Clone)]
pub enum HotkeyStatus {
    /// Registered and listening. Carries the display label for the tray
    /// tooltip and the onboarding copy.
    Registered {
        /// e.g. `⌥Space`.
        combo: String,
        /// A soft warning about a combination that nonetheless registered
        /// (§9). `Some` does **not** mean anything is broken.
        caution: Option<Caution>,
    },
    /// The OS refused it, or aibo refused to take it. Both render the same
    /// inline treatment: which shortcut, why, and a way to change it — but the
    /// "why" is typed so the copy can differ where it matters (§8).
    Failed {
        /// e.g. `⌥Space`.
        combo: String,
        /// Classified so the UI can say "choose different modifiers" rather
        /// than "another app owns this", or vice versa.
        reason: FailureReason,
    },
}

/// The subset of `GlobalHotKeyManager` [`Hotkeys`] needs.
///
/// It exists so the rebind/restore path can be exercised without a live Carbon
/// or Win32 registration: `GlobalHotKeyManager::new()` must run on the main
/// thread and talks to the real OS, so a test that used it would either not run
/// under `cargo test` or would depend on which other apps are installed. The
/// failure this seam covers — the previous binding refusing to come back —
/// cannot be provoked on real hardware on demand at all.
pub trait Registrar {
    /// Take the combination.
    fn register(&self, hotkey: HotKey) -> std::result::Result<(), global_hotkey::Error>;
    /// Release it.
    fn unregister(&self, hotkey: HotKey) -> std::result::Result<(), global_hotkey::Error>;
}

impl Registrar for GlobalHotKeyManager {
    fn register(&self, hotkey: HotKey) -> std::result::Result<(), global_hotkey::Error> {
        GlobalHotKeyManager::register(self, hotkey)
    }

    fn unregister(&self, hotkey: HotKey) -> std::result::Result<(), global_hotkey::Error> {
        GlobalHotKeyManager::unregister(self, hotkey)
    }
}

/// Owns the `GlobalHotKeyManager` and the set of live registrations.
///
/// Dropping this unregisters everything. It is **not** `Send`: on macOS the
/// manager talks to Carbon's event target on the main thread, which is where
/// the iced event loop already lives.
pub struct Hotkeys<R: Registrar = GlobalHotKeyManager> {
    manager: R,
    bindings: Vec<Binding>,
}

impl<R: Registrar> std::fmt::Debug for Hotkeys<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hotkeys")
            .field("bindings", &self.bindings)
            .finish_non_exhaustive()
    }
}

impl Hotkeys<GlobalHotKeyManager> {
    /// Create the manager without registering anything.
    ///
    /// Must be called on the main thread. Unlike the tray (§6) the manager does
    /// **not** need the event loop to be running first, so this can happen in
    /// `boot` — but the delivery of events still goes through the subscription.
    pub fn new() -> Result<Self> {
        let manager = GlobalHotKeyManager::new()
            .map_err(|e| UiError::Runtime(format!("hotkey manager: {e}")))?;
        Ok(Self {
            manager,
            bindings: Vec::new(),
        })
    }
}

impl<R: Registrar> Hotkeys<R> {
    /// Wrap an existing registrar.
    pub fn with_registrar(manager: R) -> Self {
        Self {
            manager,
            bindings: Vec::new(),
        }
    }

    /// Register one binding.
    ///
    /// Returns [`HotkeyStatus`] rather than an error because a refused hotkey
    /// is a *state the UI renders*, not a startup abort: aibo with a broken
    /// shortcut must still start, show its tray, and let the user pick another.
    pub fn register(&mut self, binding: Binding) -> HotkeyStatus {
        if let Some(why) = breaks_os_shortcut(&binding.hotkey) {
            return HotkeyStatus::Failed {
                combo: binding.display,
                reason: FailureReason::BreaksOsShortcut(why),
            };
        }
        match self.manager.register(binding.hotkey) {
            Ok(()) => {
                let combo = binding.display.clone();
                // §9: a shift/option-only combination is a *caution*, not a
                // rejection — the registration above just succeeded.
                let caution = caution_for(&binding.hotkey);
                self.bindings.push(binding);
                HotkeyStatus::Registered { combo, caution }
            }
            Err(e) => HotkeyStatus::Failed {
                combo: binding.display,
                reason: FailureReason::from_register_error(&e),
            },
        }
    }

    /// Replace the binding for an action, restoring **the previous binding** if
    /// the new one is refused. Losing the working shortcut to a failed rebind is
    /// not acceptable.
    ///
    /// Two things this must not do, both of which it used to. It must not fall
    /// back to `Binding::default_for(action)` — that is the *platform* default,
    /// so a user who had moved off `⌥Space` and then tried a combination the OS
    /// refused would silently be put back on `⌥Space` rather than on the
    /// shortcut they were actually using. And it must not discard the result of
    /// the restoring `register` call: if the old combination no longer
    /// registers either (another app grabbed it in the gap), pushing it back
    /// into `bindings` makes [`Hotkeys::bindings`] report a live registration
    /// that does not exist, and [`Hotkeys::action_for`] then matches an id the
    /// OS will never deliver.
    pub fn rebind(&mut self, action: HotkeyAction, hotkey: HotKey) -> HotkeyStatus {
        let previous = self
            .bindings
            .iter()
            .position(|b| b.action == action)
            .map(|index| self.bindings.remove(index));
        if let Some(previous) = &previous {
            let _ = self.manager.unregister(previous.hotkey);
        }

        let status = self.register(Binding::new(action, hotkey));

        if let HotkeyStatus::Failed { .. } = &status
            && let Some(previous) = previous
        {
            match self.manager.register(previous.hotkey) {
                Ok(()) => self.bindings.push(previous),
                // Neither combination is live now. Report *that*, not the
                // rejection of the new one: the user has no working shortcut
                // and the failure they need to see is the one about the
                // shortcut they still think they have.
                Err(e) => {
                    return HotkeyStatus::Failed {
                        combo: previous.display,
                        reason: FailureReason::from_register_error(&e),
                    };
                }
            }
        }

        status
    }

    /// Examine one candidate with the process's registrar (§9).
    ///
    /// **This is how the settings picker must probe.** It cannot build its own
    /// registrar: `GlobalHotKeyManager::new()` fails the second time in a
    /// process — measured 2026-07-26, `OsError(35, WouldBlock)` — so the one
    /// [`Hotkeys`] owns is the only one there is.
    ///
    /// A combination aibo *itself* already holds is reported free without
    /// probing. Probing it would refuse against aibo's own registration and
    /// tell the user another app owns the shortcut they are currently using.
    pub fn check_candidate(&self, map: &SymbolicHotkeyMap, hotkey: HotKey) -> ConflictReport {
        if self.bindings.iter().any(|b| b.hotkey == hotkey) {
            return ConflictReport {
                hotkey,
                display: describe(&hotkey),
                conflict: None,
                caution: caution_for(&hotkey),
                probe: ProbeOutcome::Skipped,
            };
        }
        check_candidate(map, &self.manager, hotkey)
    }

    /// Walk [`default_candidates`] with the process's registrar.
    ///
    /// Same ownership reason as [`Hotkeys::check_candidate`].
    pub fn suggest_free_binding(&self, map: &SymbolicHotkeyMap) -> Suggestion {
        self.suggest_free_binding_among(map, &default_candidates())
    }

    /// Walk an explicit ranked list with the process's registrar.
    pub fn suggest_free_binding_among(
        &self,
        map: &SymbolicHotkeyMap,
        candidates: &[HotKey],
    ) -> Suggestion {
        walk_candidates(candidates, |candidate| self.check_candidate(map, candidate))
    }

    /// The action a `GlobalHotKeyEvent` id maps to, if any.
    pub fn action_for(&self, id: u32) -> Option<HotkeyAction> {
        self.bindings
            .iter()
            .find(|b| b.hotkey.id() == id)
            .map(|b| b.action)
    }

    /// Every live binding.
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }
}

impl<R: Registrar> Drop for Hotkeys<R> {
    fn drop(&mut self) {
        for binding in &self.bindings {
            let _ = self.manager.unregister(binding.hotkey);
        }
    }
}

/// Install a process-wide handler that forwards **key-down only** hotkey events
/// to `sink`.
///
/// `global-hotkey` reports press and release; acting on both would open the
/// panel and immediately toggle it shut. §1's latency budget also starts at
/// key-*down*, so that is the only edge worth measuring from.
pub fn forward_events(sink: impl Fn(u32) + Send + Sync + 'static) {
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state() == HotKeyState::Pressed {
            sink(event.id());
        }
    }));
}

// ---------------------------------------------------------------------------
// §9 conflict detection: `com.apple.symbolichotkeys`
// ---------------------------------------------------------------------------

/// `NSEventModifierFlagShift`.
pub const COCOA_SHIFT: u64 = 131_072;
/// `NSEventModifierFlagControl`.
pub const COCOA_CONTROL: u64 = 262_144;
/// `NSEventModifierFlagOption`.
pub const COCOA_OPTION: u64 = 524_288;
/// `NSEventModifierFlagCommand`.
pub const COCOA_COMMAND: u64 = 1_048_576;

/// The four bits a user can actually hold down.
///
/// Everything else in a stored mask is a device-independent marker Cocoa sets
/// for the *key*, not for a modifier — `NSEventModifierFlagFunction`
/// (`8388608`) on every arrow-key shortcut, `NSEventModifierFlagNumericPad`
/// (`2097152`), `NSEventModifierFlagCapsLock` (`65536`). Comparing raw masks
/// without this makes `⌃←` (stored as `8650752`) fail to match `⌃←`, which is
/// the whole bug this constant exists to prevent.
const COCOA_USER_MODIFIERS: u64 = COCOA_SHIFT | COCOA_CONTROL | COCOA_OPTION | COCOA_COMMAND;

/// The `parameters` sentinel meaning "the user cleared this shortcut".
///
/// Appears as both the character and the virtual key code — symbolic hotkey 164
/// is stored as `[65535, 65535, 0]` on the machine measured for the module
/// docs. An entry carrying it is **dropped**, factory default and all, because
/// a cleared system shortcut is precisely a combination that has become free
/// and the stock table would otherwise keep claiming it.
const NO_KEY: u16 = 65_535;

/// Decode a Cocoa modifier mask into `global-hotkey`'s modifiers.
///
/// Ignores every bit outside the four user modifiers.
pub fn cocoa_modifiers(mask: u64) -> Modifiers {
    let mask = mask & COCOA_USER_MODIFIERS;
    let mut mods = Modifiers::empty();
    if mask & COCOA_SHIFT != 0 {
        mods |= Modifiers::SHIFT;
    }
    if mask & COCOA_CONTROL != 0 {
        mods |= Modifiers::CONTROL;
    }
    if mask & COCOA_OPTION != 0 {
        mods |= Modifiers::ALT;
    }
    if mask & COCOA_COMMAND != 0 {
        mods |= Modifiers::SUPER;
    }
    mods
}

/// Encode modifiers as a Cocoa mask. The inverse of [`cocoa_modifiers`].
pub fn cocoa_mask(mods: Modifiers) -> u64 {
    let mut mask = 0;
    if mods.contains(Modifiers::SHIFT) {
        mask |= COCOA_SHIFT;
    }
    if mods.contains(Modifiers::CONTROL) {
        mask |= COCOA_CONTROL;
    }
    if mods.contains(Modifiers::ALT) {
        mask |= COCOA_OPTION;
    }
    if mods.contains(Modifiers::SUPER) {
        mask |= COCOA_COMMAND;
    }
    mask
}

/// The macOS virtual key code for a physical key, or `None` if this table does
/// not cover it.
///
/// `AppleSymbolicHotKeys` stores `parameters[1]` as a virtual key code, so a
/// candidate cannot be compared against a system shortcut without this. The
/// codes are positional (`ANSI_A` is `0` wherever the letter A is printed),
/// which is the same physical-position model `Code` uses — the two agree, and
/// §9's warning that "key codes are keyboard-layout dependent" applies equally
/// to both sides of the comparison rather than making it wrong.
///
/// `None` is an honest "cannot compare", not "free": [`SymbolicHotkeyMap::owner_of`]
/// reports no owner for such a key and the probe still runs.
pub fn macos_virtual_key_code(code: Code) -> Option<u16> {
    let vk = match code {
        Code::KeyA => 0,
        Code::KeyS => 1,
        Code::KeyD => 2,
        Code::KeyF => 3,
        Code::KeyH => 4,
        Code::KeyG => 5,
        Code::KeyZ => 6,
        Code::KeyX => 7,
        Code::KeyC => 8,
        Code::KeyV => 9,
        Code::KeyB => 11,
        Code::KeyQ => 12,
        Code::KeyW => 13,
        Code::KeyE => 14,
        Code::KeyR => 15,
        Code::KeyY => 16,
        Code::KeyT => 17,
        Code::Digit1 => 18,
        Code::Digit2 => 19,
        Code::Digit3 => 20,
        Code::Digit4 => 21,
        Code::Digit6 => 22,
        Code::Digit5 => 23,
        Code::Equal => 24,
        Code::Digit9 => 25,
        Code::Digit7 => 26,
        Code::Minus => 27,
        Code::Digit8 => 28,
        Code::Digit0 => 29,
        Code::BracketRight => 30,
        Code::KeyO => 31,
        Code::KeyU => 32,
        Code::BracketLeft => 33,
        Code::KeyI => 34,
        Code::KeyP => 35,
        Code::Enter => 36,
        Code::KeyL => 37,
        Code::KeyJ => 38,
        Code::Quote => 39,
        Code::KeyK => 40,
        Code::Semicolon => 41,
        Code::Backslash => 42,
        Code::Comma => 43,
        Code::Slash => 44,
        Code::KeyN => 45,
        Code::KeyM => 46,
        Code::Period => 47,
        Code::Tab => 48,
        Code::Space => 49,
        Code::Backquote => 50,
        Code::Backspace => 51,
        Code::Escape => 53,
        Code::F17 => 64,
        Code::F18 => 79,
        Code::F19 => 80,
        Code::F20 => 90,
        Code::F5 => 96,
        Code::F6 => 97,
        Code::F7 => 98,
        Code::F3 => 99,
        Code::F8 => 100,
        Code::F9 => 101,
        Code::F11 => 103,
        Code::F13 => 105,
        Code::F16 => 106,
        Code::F14 => 107,
        Code::F10 => 109,
        Code::F12 => 111,
        Code::F15 => 113,
        Code::Home => 115,
        Code::PageUp => 116,
        Code::Delete => 117,
        Code::F4 => 118,
        Code::End => 119,
        Code::F2 => 120,
        Code::PageDown => 121,
        Code::F1 => 122,
        Code::ArrowLeft => 123,
        Code::ArrowRight => 124,
        Code::ArrowDown => 125,
        Code::ArrowUp => 126,
        _ => return None,
    };
    Some(vk)
}

/// Where the knowledge that a system shortcut exists came from.
///
/// The distinction is load-bearing and must reach the UI. `com.apple.symbolichotkeys`
/// only records entries the user has **changed**: on the machine measured for
/// the module docs the file holds 23 entries and `⌘Space` (Spotlight) is not
/// one of them, yet Spotlight plainly owns it. Absence from the plist is not
/// evidence of freedom, so a table of factory defaults is overlaid underneath
/// — and a conflict sourced from that table is an *assumption about a stock
/// macOS install*, which the copy should say, while a conflict sourced from the
/// plist is a *fact about this machine*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutSource {
    /// Read from this machine's `com.apple.symbolichotkeys`. Authoritative.
    UserPreference,
    /// Not present in the preference file; taken from the stock-macOS table.
    /// True unless the user has changed it in a way macOS did not record.
    FactoryDefault,
}

/// One entry of `AppleSymbolicHotKeys`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolicHotkey {
    /// The symbolic hotkey id — the dictionary key, e.g. `60`.
    pub id: u32,
    /// Whether macOS will act on it. A disabled entry is not a conflict.
    pub enabled: bool,
    /// `parameters[1]`, the macOS virtual key code.
    pub key_code: u16,
    /// `parameters[2]`, decoded through [`cocoa_modifiers`].
    pub modifiers: Modifiers,
    /// Preference file or factory table.
    pub source: ShortcutSource,
}

impl SymbolicHotkey {
    /// The human-readable name of the system action, or a fallback naming the
    /// id.
    pub fn action(&self) -> String {
        system_action_name(self.id)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("a macOS system shortcut (#{})", self.id))
    }

    /// Whether this entry would swallow `hotkey`.
    ///
    /// Cleared entries never reach here: [`SymbolicHotkeyMap::parse_defaults_export`]
    /// removes them rather than storing a `NO_KEY` binding, so there is no
    /// second place that has to remember the sentinel.
    fn owns(&self, hotkey: &HotKey) -> bool {
        if !self.enabled {
            return false;
        }
        match macos_virtual_key_code(hotkey.key) {
            Some(vk) => vk == self.key_code && self.modifiers == hotkey.mods,
            None => false,
        }
    }
}

/// The name macOS's Keyboard settings pane gives a symbolic hotkey id.
///
/// **Deliberately not exhaustive.** The id list is undocumented, and inventing
/// a label for an id whose meaning is guessed would put a confident wrong
/// sentence in front of the user. Ids that are not here render through
/// [`SymbolicHotkey::action`]'s fallback, which names the number and claims
/// nothing else.
pub fn system_action_name(id: u32) -> Option<&'static str> {
    let name = match id {
        7 => "Move focus to the menu bar",
        8 => "Move focus to the Dock",
        15 => "Zoom: toggle",
        17 => "Zoom: zoom in",
        19 => "Zoom: zoom out",
        21 => "Invert colours",
        23 => "Increase contrast",
        25 => "Decrease contrast",
        27 => "Move focus to the next window",
        28 => "Save a picture of the screen as a file",
        29 => "Copy a picture of the screen to the clipboard",
        30 => "Save a picture of the selected area as a file",
        31 => "Copy a picture of the selected area to the clipboard",
        32 => "Mission Control",
        52 => "Turn Dock hiding on or off",
        60 => "Select the previous input source",
        61 => "Select the next source in the Input menu",
        64 => "Show Spotlight search",
        65 => "Show Finder search window",
        79 | 80 => "Move left a space",
        81 | 82 => "Move right a space",
        98 => "Show Help menu",
        118..=123 => "Switch to another Desktop",
        160 => "Show Launchpad",
        162 => "Show Notification Centre",
        164 => "Turn Do Not Disturb on or off",
        175 => "Show Accessibility Shortcuts",
        179 => "Show Emoji & Symbols",
        _ => return None,
    };
    Some(name)
}

/// Stock-macOS bindings for ids the preference file omits until they are
/// changed: `(id, virtual key code, cocoa mask)`.
///
/// Same honesty rule as [`system_action_name`]: only entries that are stable
/// and widely documented. A missing entry means "no owner *known*", never "no
/// owner".
const FACTORY_BINDINGS: &[(u32, u16, u64)] = &[
    // ⌘`
    (27, 50, COCOA_COMMAND),
    // ⇧⌘3 / ⌃⇧⌘3
    (28, 20, COCOA_COMMAND | COCOA_SHIFT),
    (29, 20, COCOA_COMMAND | COCOA_SHIFT | COCOA_CONTROL),
    // ⇧⌘4 / ⌃⇧⌘4
    (30, 21, COCOA_COMMAND | COCOA_SHIFT),
    (31, 21, COCOA_COMMAND | COCOA_SHIFT | COCOA_CONTROL),
    // ⌃↑
    (32, 126, COCOA_CONTROL),
    // ⌥⌘D
    (52, 2, COCOA_OPTION | COCOA_COMMAND),
    // ⌃Space, ⌃⌥Space — measured enabled on the machine in the module docs.
    (60, 49, COCOA_CONTROL),
    (61, 49, COCOA_CONTROL | COCOA_OPTION),
    // ⌘Space, ⌥⌘Space — absent from that machine's plist and nonetheless live.
    (64, 49, COCOA_COMMAND),
    (65, 49, COCOA_OPTION | COCOA_COMMAND),
    // ⌃← / ⌃⇧← / ⌃→ / ⌃⇧→
    (79, 123, COCOA_CONTROL),
    (80, 123, COCOA_CONTROL | COCOA_SHIFT),
    (81, 124, COCOA_CONTROL),
    (82, 124, COCOA_CONTROL | COCOA_SHIFT),
    // ⇧⌘/
    (98, 44, COCOA_COMMAND | COCOA_SHIFT),
    // ⌃1 … ⌃6
    (118, 18, COCOA_CONTROL),
    (119, 19, COCOA_CONTROL),
    (120, 20, COCOA_CONTROL),
    (121, 21, COCOA_CONTROL),
    (122, 23, COCOA_CONTROL),
    (123, 22, COCOA_CONTROL),
    // ⌃⌘Space
    (179, 49, COCOA_CONTROL | COCOA_COMMAND),
];

/// Reading `com.apple.symbolichotkeys` failed.
///
/// Local to this module rather than a new [`UiError`] variant: `error.rs` is
/// owned elsewhere, and a failure to *inspect* system preferences is not a
/// shell-level startup failure — it degrades conflict detection to the probe
/// alone and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SymbolicHotkeyError {
    /// The `defaults` process could not be run or exited non-zero.
    #[error("could not read com.apple.symbolichotkeys: {0}")]
    Read(String),
    /// The output was not a property list this parser understands.
    #[error("com.apple.symbolichotkeys is not a property list we understand: {0}")]
    Malformed(String),
}

/// Every system shortcut aibo knows about, keyed by symbolic hotkey id.
///
/// Built as the factory table with the preference file overlaid on top, so a
/// user who has disabled `⌃Space` gets `⌃Space` offered back to them and a user
/// who has never opened Keyboard settings still has `⌘Space` recognised as
/// Spotlight.
#[derive(Debug, Clone, Default)]
pub struct SymbolicHotkeyMap {
    entries: BTreeMap<u32, SymbolicHotkey>,
}

impl SymbolicHotkeyMap {
    /// No system shortcuts at all.
    ///
    /// What non-macOS platforms get: `AppleSymbolicHotKeys` is a macOS concept,
    /// and Windows' equivalents are handled by [`breaks_os_shortcut`] plus the
    /// probe.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The stock-macOS table with no preference file applied.
    pub fn factory_defaults() -> Self {
        let entries = FACTORY_BINDINGS
            .iter()
            .map(|&(id, key_code, mask)| {
                (
                    id,
                    SymbolicHotkey {
                        id,
                        enabled: true,
                        key_code,
                        modifiers: cocoa_modifiers(mask),
                        source: ShortcutSource::FactoryDefault,
                    },
                )
            })
            .collect();
        Self { entries }
    }

    /// Read this machine's settings (macOS only; [`Self::empty`] elsewhere).
    ///
    /// Shells out to `defaults export com.apple.symbolichotkeys -` rather than
    /// reading `~/Library/Preferences/com.apple.symbolichotkeys.plist`
    /// directly, for two reasons: the file on disk is a **binary** plist, and
    /// `cfprefsd` may be holding newer values that have not been flushed to it.
    /// `defaults` is the supported way to ask, and it emits XML.
    ///
    /// Blocking, and intended for first run and for the settings pane — not for
    /// the hotkey path, which has a latency budget (§15).
    pub fn from_system() -> std::result::Result<Self, SymbolicHotkeyError> {
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("defaults")
                .args(["export", "com.apple.symbolichotkeys", "-"])
                .output()
                .map_err(|e| SymbolicHotkeyError::Read(e.to_string()))?;
            if !output.status.success() {
                return Err(SymbolicHotkeyError::Read(format!(
                    "`defaults export` exited with {}",
                    output.status
                )));
            }
            let xml = String::from_utf8(output.stdout)
                .map_err(|e| SymbolicHotkeyError::Malformed(e.to_string()))?;
            Self::parse_defaults_export(&xml)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(Self::empty())
        }
    }

    /// Parse the XML property list `defaults export` writes, overlaying it on
    /// [`Self::factory_defaults`].
    ///
    /// This is the seam the tests use: the whole point is that conflict
    /// detection is asserted against synthetic fixtures rather than against
    /// whatever the developer's own Mac happens to be configured with.
    pub fn parse_defaults_export(xml: &str) -> std::result::Result<Self, SymbolicHotkeyError> {
        let root = plist::parse(xml)?;
        let mut map = Self::factory_defaults();

        // A domain with no shortcuts at all exports as an empty dict, which is
        // valid and means "everything is at its factory default".
        let Some(table) = root
            .get("AppleSymbolicHotKeys")
            .and_then(plist::Value::as_dict)
        else {
            return Ok(map);
        };

        for (key, entry) in table {
            let Ok(id) = key.parse::<u32>() else {
                // Not an error: the domain is Apple's and may grow non-numeric
                // keys. Skipping one is strictly better than refusing to
                // detect any conflict at all.
                continue;
            };
            let enabled = entry.get("enabled").and_then(plist::Value::as_bool);
            let parameters = entry
                .get("value")
                .and_then(|value| value.get("parameters"))
                .and_then(plist::Value::as_array);

            match (parameters, map.entries.get(&id).copied()) {
                // The common shape: an explicit key and mask.
                (Some(parameters), _) => {
                    // `[character, virtual key code, cocoa mask]`. The
                    // character is ignored — it is layout-dependent and 65535
                    // whenever the key produces none.
                    let (Some(key_code), Some(mask)) = (
                        parameters.get(1).and_then(plist::Value::as_i64),
                        parameters.get(2).and_then(plist::Value::as_i64),
                    ) else {
                        continue;
                    };
                    let key_code = u16::try_from(key_code).unwrap_or(NO_KEY);
                    if key_code == NO_KEY {
                        // Cleared. Drop any factory default too — this is the
                        // one signal that a stock combination has been freed.
                        map.entries.remove(&id);
                        continue;
                    }
                    map.entries.insert(
                        id,
                        SymbolicHotkey {
                            id,
                            enabled: enabled.unwrap_or(true),
                            key_code,
                            modifiers: cocoa_modifiers(mask.max(0) as u64),
                            source: ShortcutSource::UserPreference,
                        },
                    );
                }
                // `{ enabled = 0; }` with no value: macOS records only that the
                // user switched the factory shortcut off. Keep the factory key
                // and take the flag.
                (None, Some(known)) => {
                    map.entries.insert(
                        id,
                        SymbolicHotkey {
                            enabled: enabled.unwrap_or(known.enabled),
                            source: ShortcutSource::UserPreference,
                            ..known
                        },
                    );
                }
                // Neither a binding nor anything to inherit one from.
                (None, None) => {}
            }
        }

        Ok(map)
    }

    /// The enabled system shortcut that would swallow `hotkey`, if any.
    ///
    /// `None` means *no known owner*. It is not a guarantee of freedom — see
    /// [`system_action_name`] and the factory table on why both are
    /// deliberately incomplete — which is exactly why [`check_candidate`] also
    /// probes.
    pub fn owner_of(&self, hotkey: &HotKey) -> Option<&SymbolicHotkey> {
        self.entries.values().find(|entry| entry.owns(hotkey))
    }

    /// Every entry, factory and user, in id order.
    pub fn entries(&self) -> impl Iterator<Item = &SymbolicHotkey> {
        self.entries.values()
    }
}

// ---------------------------------------------------------------------------
// §9 conflict detection: the typed result
// ---------------------------------------------------------------------------

/// What happened when a candidate was registered and immediately released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Never attempted: a static check already rejected the candidate, so the
    /// OS was not touched. Registering a combination aibo has decided it must
    /// not take, even for a moment, is not acceptable.
    Skipped,
    /// Registered and released cleanly: no other Carbon/Win32 registration
    /// holds it.
    ///
    /// Not a promise that the key will reach aibo. An app that installs a
    /// `CGEventTap` or an `NSEvent` global monitor — which is how several
    /// launchers work — sees the key first and never contends for the
    /// registration, so it probes as free and still swallows the press. That
    /// residue is not detectable from this process, and calling this variant
    /// `Available` would claim otherwise.
    Free,
    /// Registered, but the release failed — **aibo now holds it**. The
    /// combination is free of *other* owners, and a later
    /// [`Hotkeys::register`] of it may report [`FailureReason::AlreadyOwned`]
    /// against aibo's own leaked registration. Surfaced rather than swallowed
    /// so that misdiagnosis is impossible.
    HeldByProbe(String),
    /// Registration was refused, classified per §8.
    Refused(FailureReason),
}

/// Why a candidate combination cannot be used.
///
/// Typed rather than a string because the three variants lead to three
/// different actions: change a macOS setting, quit another app, or pick
/// something else entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// A macOS system shortcut owns it. It would register successfully and
    /// then never fire, because the WindowServer consumes the key first —
    /// the failure mode that makes preference inspection necessary at all.
    SystemShortcut {
        /// Symbolic hotkey id, e.g. `60`.
        id: u32,
        /// Human-readable action, e.g. `Select the previous input source`.
        action: String,
        /// Preference file (fact) or factory table (assumption).
        source: ShortcutSource,
    },
    /// A probe registration was refused: another application holds it.
    /// `-9878 eventHotKeyExistsErr` where the platform gives us the code.
    AlreadyHeld {
        /// Classified reason, so "choose different modifiers" and "another app
        /// owns this" stay distinguishable (§8).
        reason: FailureReason,
    },
    /// aibo refuses to take it: [`breaks_os_shortcut`].
    BreaksOsShortcut(&'static str),
}

/// The verdict on one candidate combination.
///
/// This is the type the settings pane renders. See the module docs for the
/// render contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictReport {
    /// The combination examined.
    pub hotkey: HotKey,
    /// Its platform-idiomatic label, e.g. `⌥Space`.
    pub display: String,
    /// `None` if no conflict was **detected**. Not a proof of freedom; the
    /// tables are incomplete by design and a probe cannot see a shortcut the
    /// WindowServer eats.
    pub conflict: Option<Conflict>,
    /// The §9 soft warning, independent of the conflict. A combination can be
    /// perfectly free and still carry this.
    pub caution: Option<Caution>,
    /// What the probe did, including "did not run".
    pub probe: ProbeOutcome,
}

impl ConflictReport {
    /// Whether no conflict was detected.
    pub fn is_free(&self) -> bool {
        self.conflict.is_none()
    }

    /// One sentence explaining the verdict.
    ///
    /// TODO(§9 i18n): like [`Caution::explanation`], this is English-only
    /// because `i18n::Key` has no variants for it yet and `i18n.rs` is owned
    /// elsewhere. The three arms map to three future keys —
    /// `HotkeyConflictSystem`, `HotkeyConflictApp`, `HotkeyConflictRefused` —
    /// and the `{combo}` / `{action}` substitutions are `t1`/`t2` shaped.
    pub fn summary(&self) -> String {
        match &self.conflict {
            None => format!("{} is available.", self.display),
            Some(Conflict::SystemShortcut { action, source, .. }) => match source {
                ShortcutSource::UserPreference => format!(
                    "macOS uses {} for “{action}”. Change it in System Settings ▸ Keyboard ▸ \
                     Keyboard Shortcuts, or pick another shortcut.",
                    self.display
                ),
                ShortcutSource::FactoryDefault => format!(
                    "macOS normally uses {} for “{action}”. Pick another shortcut unless you \
                     have already changed that.",
                    self.display
                ),
            },
            Some(Conflict::AlreadyHeld { reason }) => match reason {
                FailureReason::ModifiersRejected => format!(
                    "macOS will not accept {} as a global shortcut. Add ⌃ or ⌘.",
                    self.display
                ),
                FailureReason::AlreadyOwned => format!(
                    "Another app already owns {}. Quit it or pick another shortcut.",
                    self.display
                ),
                // Measured 2026-07-26: a duplicate `RegisterEventHotKey` on
                // this machine really does fail, and it really does arrive as
                // `Unclassified("… failed for Space")` — §8's documented gap,
                // `global-hotkey` 0.8 having dropped the `OSStatus`. So the
                // *common* macOS case cannot tell `-9878` from `-9868`, and
                // picking either message would be a guess presented as fact.
                // Say both, and give the action that helps under either.
                _ => format!(
                    "{} was refused. Either another app already owns it or macOS will not accept \
                     these modifiers — it does not report which. Try a combination that includes \
                     ⌃ or ⌘, or quit the app you think is holding it.",
                    self.display
                ),
            },
            Some(Conflict::BreaksOsShortcut(why)) => format!("{}: {why}.", self.display),
        }
    }
}

/// The result of walking a ranked candidate list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The first genuinely-free candidate, or `None` when **every** candidate
    /// was taken.
    ///
    /// `None` is a real outcome on the machine in the module docs, not a
    /// defensive branch, and the UI must render it: the honest message is
    /// "aibo could not find a free shortcut — here is what is in the way", with
    /// [`Suggestion::rejected`] listed. Falling back to a hardcoded combination
    /// at that point would ship a shortcut that is known not to work.
    pub chosen: Option<ConflictReport>,
    /// Every candidate rejected before [`Suggestion::chosen`], in rank order,
    /// each carrying the reason. Candidates *after* the chosen one are not
    /// probed and do not appear.
    pub rejected: Vec<ConflictReport>,
}

/// Examine one candidate: static rules, then system shortcuts, then a probe.
///
/// The order matters. A combination [`breaks_os_shortcut`] rejects is never
/// registered, not even for the microsecond a probe would hold it. A
/// combination a system shortcut owns is not probed either — the probe would
/// *succeed* and say nothing, because the WindowServer intercepts the key
/// before Carbon ever dispatches it.
pub fn check_candidate<R: Registrar>(
    map: &SymbolicHotkeyMap,
    registrar: &R,
    hotkey: HotKey,
) -> ConflictReport {
    let display = describe(&hotkey);
    let caution = caution_for(&hotkey);

    if let Some(why) = breaks_os_shortcut(&hotkey) {
        return ConflictReport {
            hotkey,
            display,
            conflict: Some(Conflict::BreaksOsShortcut(why)),
            caution,
            probe: ProbeOutcome::Skipped,
        };
    }

    if let Some(owner) = map.owner_of(&hotkey) {
        return ConflictReport {
            hotkey,
            display,
            conflict: Some(Conflict::SystemShortcut {
                id: owner.id,
                action: owner.action(),
                source: owner.source,
            }),
            caution,
            probe: ProbeOutcome::Skipped,
        };
    }

    let (conflict, probe) = match registrar.register(hotkey) {
        Ok(()) => match registrar.unregister(hotkey) {
            Ok(()) => (None, ProbeOutcome::Free),
            Err(e) => {
                let message = e.to_string();
                // `label` rather than `display`: `tracing`'s macros pull
                // `tracing::field::display` into scope, which shadows a local
                // of that name and turns the field into a function item.
                let label: &str = &display;
                tracing::warn!(
                    combo = label,
                    error = message.as_str(),
                    "hotkey probe could not be released; aibo now holds this combination"
                );
                (None, ProbeOutcome::HeldByProbe(message))
            }
        },
        Err(e) => {
            let reason = FailureReason::from_register_error(&e);
            (
                Some(Conflict::AlreadyHeld {
                    reason: reason.clone(),
                }),
                ProbeOutcome::Refused(reason),
            )
        }
    };

    ConflictReport {
        hotkey,
        display,
        conflict,
        caution,
        probe,
    }
}

/// aibo's preference order for the panel shortcut.
///
/// Not "combinations that are probably free" — the measurement in the module
/// docs is that no such list exists on a real Japanese-configured Mac. It is
/// the order in which a user would want them, *including* the ones most likely
/// to be rejected, because the point of [`suggest_free_binding`] is to explain
/// why the obvious choices are unavailable rather than to silently arrive at an
/// obscure one.
pub fn default_candidates() -> Vec<HotKey> {
    #[cfg(target_os = "macos")]
    {
        vec![
            // §9's default. Contended by Raycast on the measured machine, which
            // the probe finds; it stays first because §9 says it stands and a
            // machine without a launcher should still get it.
            HotKey::new(Some(Modifiers::ALT), Code::Space),
            // The two the input-source switcher owns (symbolic 60 / 61). Listed
            // so the report names them instead of leaving the user guessing.
            HotKey::new(Some(Modifiers::CONTROL), Code::Space),
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space),
            // ⌥⌘Space is the Finder search window (65) and ⌃⌘Space is Emoji &
            // Symbols (179); both are absent from a stock plist, which is what
            // the factory table is for.
            HotKey::new(Some(Modifiers::ALT | Modifiers::SUPER), Code::Space),
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::SUPER), Code::Space),
            // From here on: no known system owner.
            //
            // These two come before `⌥⇧Space` on purpose. Reaching this point
            // means the option-space family already failed once, and the two
            // things that would have caused that — a launcher's event tap on
            // option-space, or §8's reported macOS 15+ refusal of
            // shift/option-only combinations — both apply just as much to
            // `⌥⇧Space`. Adding ⌃ or ⌘ is the documented way out of the second
            // and empirically dodges the first.
            HotKey::new(
                Some(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER),
                Code::Space,
            ),
            HotKey::new(
                Some(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER),
                Code::KeyA,
            ),
            // Still offered, because the caution is soft and §9 keeps an
            // option-only combination as the shipped default.
            HotKey::new(Some(Modifiers::SHIFT | Modifiers::ALT), Code::Space),
            // Last resort: a bare function key. Uncontested almost everywhere,
            // and absent from a lot of keyboards — hence last, not first.
            HotKey::new(None, Code::F13),
        ]
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![
            // §9's Windows default.
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space),
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space),
            // Both are asked for by name and both are refused by
            // `breaks_os_shortcut`; listing them is how the report gets to say
            // so rather than the user wondering why they were skipped.
            HotKey::new(Some(Modifiers::ALT), Code::Space),
            HotKey::new(Some(Modifiers::SUPER), Code::Space),
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Backquote),
            HotKey::new(None, Code::F13),
        ]
    }
}

/// Probe [`default_candidates`] and return the first genuinely-free one.
pub fn suggest_free_binding<R: Registrar>(map: &SymbolicHotkeyMap, registrar: &R) -> Suggestion {
    suggest_free_binding_among(map, registrar, &default_candidates())
}

/// Probe an explicit ranked list and return the first genuinely-free one,
/// together with the reason each earlier candidate was rejected.
///
/// Stops at the first free candidate: the remaining ones are not probed, so
/// nothing is registered and released needlessly.
pub fn suggest_free_binding_among<R: Registrar>(
    map: &SymbolicHotkeyMap,
    registrar: &R,
    candidates: &[HotKey],
) -> Suggestion {
    walk_candidates(candidates, |candidate| {
        check_candidate(map, registrar, candidate)
    })
}

/// The ranked walk itself, shared by the free functions and by [`Hotkeys`]'s
/// methods so the two cannot drift on "stops at the first free candidate".
fn walk_candidates(
    candidates: &[HotKey],
    mut check: impl FnMut(HotKey) -> ConflictReport,
) -> Suggestion {
    let mut rejected = Vec::new();
    for &candidate in candidates {
        let report = check(candidate);
        if report.is_free() {
            return Suggestion {
                chosen: Some(report),
                rejected,
            };
        }
        rejected.push(report);
    }
    Suggestion {
        chosen: None,
        rejected,
    }
}

/// A deliberately small XML property list reader.
///
/// `defaults export` emits XML, and this module needs exactly one shape out of
/// it. A full plist crate would be a workspace dependency change for four value
/// kinds, so this reads those four and treats everything else as opaque rather
/// than failing on it.
mod plist {
    use super::SymbolicHotkeyError;

    /// A parsed property list value.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        /// `<dict>`, in document order — the ids matter and sorting them as
        /// strings would reorder `9` after `118`.
        Dict(Vec<(String, Value)>),
        /// `<array>`.
        Array(Vec<Value>),
        /// `<integer>` or a `<real>` that is exactly an integer.
        Integer(i64),
        /// `<string>`.
        Str(String),
        /// `<true/>` / `<false/>`.
        Bool(bool),
        /// `<data>`, `<date>`, a fractional `<real>` — present but unused.
        Opaque,
    }

    impl Value {
        /// Look a key up in a dict.
        pub fn get(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Dict(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        /// The pairs of a dict.
        pub fn as_dict(&self) -> Option<&[(String, Value)]> {
            match self {
                Value::Dict(pairs) => Some(pairs),
                _ => None,
            }
        }

        /// The elements of an array.
        pub fn as_array(&self) -> Option<&[Value]> {
            match self {
                Value::Array(items) => Some(items),
                _ => None,
            }
        }

        /// An integer.
        pub fn as_i64(&self) -> Option<i64> {
            match self {
                Value::Integer(n) => Some(*n),
                _ => None,
            }
        }

        /// A boolean. `<integer>0</integer>` also appears in this domain and
        /// means the same thing, so it is accepted.
        pub fn as_bool(&self) -> Option<bool> {
            match self {
                Value::Bool(b) => Some(*b),
                Value::Integer(n) => Some(*n != 0),
                _ => None,
            }
        }
    }

    struct Tag {
        name: String,
        closing: bool,
        self_closing: bool,
    }

    struct Reader<'a> {
        src: &'a str,
        at: usize,
    }

    impl<'a> Reader<'a> {
        fn malformed(what: &str) -> SymbolicHotkeyError {
            SymbolicHotkeyError::Malformed(what.to_owned())
        }

        /// The next tag, skipping `<?…?>` declarations and `<!…>` doctypes.
        fn next_tag(&mut self) -> Result<Option<Tag>, SymbolicHotkeyError> {
            loop {
                let Some(open) = self.src[self.at..].find('<') else {
                    return Ok(None);
                };
                let open = self.at + open;
                let Some(close) = self.src[open..].find('>') else {
                    return Err(Self::malformed("unterminated tag"));
                };
                let close = open + close;
                let body = &self.src[open + 1..close];
                self.at = close + 1;

                if body.starts_with('?') || body.starts_with('!') {
                    continue;
                }
                let closing = body.starts_with('/');
                let self_closing = body.ends_with('/');
                let name = body
                    .trim_start_matches('/')
                    .trim_end_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                if name.is_empty() {
                    return Err(Self::malformed("tag with no name"));
                }
                return Ok(Some(Tag {
                    name,
                    closing,
                    self_closing,
                }));
            }
        }

        /// The text up to `</name>`, consuming the closing tag.
        fn text_until_close(&mut self, name: &str) -> Result<String, SymbolicHotkeyError> {
            let needle = format!("</{name}>");
            let Some(end) = self.src[self.at..].find(&needle) else {
                return Err(Self::malformed(&format!("unterminated <{name}>")));
            };
            let text = &self.src[self.at..self.at + end];
            self.at += end + needle.len();
            Ok(unescape(text))
        }

        fn value(&mut self, tag: &Tag) -> Result<Value, SymbolicHotkeyError> {
            if tag.self_closing {
                return Ok(match tag.name.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    "dict" => Value::Dict(Vec::new()),
                    "array" => Value::Array(Vec::new()),
                    "string" => Value::Str(String::new()),
                    _ => Value::Opaque,
                });
            }

            match tag.name.as_str() {
                "dict" => {
                    let mut pairs = Vec::new();
                    loop {
                        let Some(tag) = self.next_tag()? else {
                            return Err(Self::malformed("unterminated <dict>"));
                        };
                        if tag.closing && tag.name == "dict" {
                            return Ok(Value::Dict(pairs));
                        }
                        if tag.closing || tag.name != "key" {
                            return Err(Self::malformed("expected <key> inside <dict>"));
                        }
                        let key = if tag.self_closing {
                            String::new()
                        } else {
                            self.text_until_close("key")?
                        };
                        let Some(value_tag) = self.next_tag()? else {
                            return Err(Self::malformed("<key> with no value"));
                        };
                        if value_tag.closing {
                            return Err(Self::malformed("<key> with no value"));
                        }
                        pairs.push((key, self.value(&value_tag)?));
                    }
                }
                "array" => {
                    let mut items = Vec::new();
                    loop {
                        let Some(tag) = self.next_tag()? else {
                            return Err(Self::malformed("unterminated <array>"));
                        };
                        if tag.closing && tag.name == "array" {
                            return Ok(Value::Array(items));
                        }
                        if tag.closing {
                            return Err(Self::malformed("unbalanced tag inside <array>"));
                        }
                        items.push(self.value(&tag)?);
                    }
                }
                "true" | "false" => {
                    let is_true = tag.name == "true";
                    self.text_until_close(&tag.name)?;
                    Ok(Value::Bool(is_true))
                }
                "integer" => {
                    let text = self.text_until_close("integer")?;
                    text.trim()
                        .parse::<i64>()
                        .map(Value::Integer)
                        .map_err(|_| Self::malformed(&format!("`{text}` is not an integer")))
                }
                "real" => {
                    let text = self.text_until_close("real")?;
                    // Masks and key codes are whole numbers even when macOS
                    // writes them as reals; anything fractional is not one of
                    // ours and stays opaque.
                    Ok(match text.trim().parse::<f64>() {
                        Ok(n) if n.fract() == 0.0 && n.abs() < 9e18 => Value::Integer(n as i64),
                        _ => Value::Opaque,
                    })
                }
                "string" => Ok(Value::Str(self.text_until_close("string")?)),
                other => {
                    self.text_until_close(other)?;
                    Ok(Value::Opaque)
                }
            }
        }
    }

    fn unescape(text: &str) -> String {
        if !text.contains('&') {
            return text.to_owned();
        }
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    /// Parse an XML property list document and return its root value.
    pub fn parse(xml: &str) -> Result<Value, SymbolicHotkeyError> {
        let mut reader = Reader { src: xml, at: 0 };
        while let Some(tag) = reader.next_tag()? {
            if tag.closing {
                continue;
            }
            if tag.name == "plist" {
                if tag.self_closing {
                    return Ok(Value::Dict(Vec::new()));
                }
                continue;
            }
            return reader.value(&tag);
        }
        Err(SymbolicHotkeyError::Malformed(
            "no property list value in the document".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_default_matches_section_9() {
        let hk = default_panel_hotkey();
        assert_eq!(hk.key, Code::Space);
        if cfg!(target_os = "macos") {
            assert!(hk.mods.contains(Modifiers::ALT));
            assert!(!hk.mods.contains(Modifiers::CONTROL));
        } else {
            assert!(hk.mods.contains(Modifiers::CONTROL));
            assert!(hk.mods.contains(Modifiers::SHIFT));
            assert!(!hk.mods.contains(Modifiers::ALT));
        }
    }

    #[test]
    fn the_default_is_never_one_aibo_refuses_to_take() {
        assert!(breaks_os_shortcut(&default_panel_hotkey()).is_none());
    }

    #[test]
    fn alt_space_is_rejected_on_windows() {
        let alt_space = HotKey::new(Some(Modifiers::ALT), Code::Space);
        #[cfg(target_os = "windows")]
        assert!(breaks_os_shortcut(&alt_space).is_some());
        #[cfg(not(target_os = "windows"))]
        assert!(breaks_os_shortcut(&alt_space).is_none());
    }

    #[test]
    fn labels_are_platform_idiomatic() {
        let label = describe(&default_panel_hotkey());
        assert!(label.contains("Space"));
        if cfg!(target_os = "macos") {
            assert!(label.starts_with('⌥'));
        } else {
            assert!(label.starts_with("Ctrl+"));
        }
    }

    /// Regression, F5. The rule was previously `only_shift_or_alt && the key is
    /// itself a modifier` — a combination `RegisterEventHotKey` cannot express,
    /// so the rule matched nothing and `⌥Space` was excluded by construction.
    /// The documented rule is shift/option-**only**, full stop.
    #[test]
    fn shift_or_option_only_combinations_are_flagged() {
        // Option-only, ordinary key: the case the narrowed rule missed.
        assert!(is_risky_on_macos(&HotKey::new(
            Some(Modifiers::ALT),
            Code::Space
        )));
        assert!(is_risky_on_macos(&HotKey::new(
            Some(Modifiers::SHIFT),
            Code::KeyK
        )));
        assert!(is_risky_on_macos(&HotKey::new(
            Some(Modifiers::SHIFT | Modifiers::ALT),
            Code::Space
        )));

        // Adding control or command takes it out of the rule.
        assert!(!is_risky_on_macos(&HotKey::new(
            Some(Modifiers::CONTROL | Modifiers::SHIFT),
            Code::Space
        )));
        assert!(!is_risky_on_macos(&HotKey::new(
            Some(Modifiers::SUPER | Modifiers::ALT),
            Code::Space
        )));
        assert!(!is_risky_on_macos(&HotKey::new(None, Code::F13)));
    }

    /// Regression, F5. The old test asserted
    /// `!is_risky_on_macos(&default_panel_hotkey())`, which pinned the rule to
    /// whatever let the shipped default through. §9 forbids exactly that: the
    /// default is shift/option-only, so it *is* flagged, and the answer is a
    /// soft warning rather than narrowing the rule or dropping the default.
    #[test]
    fn the_macos_default_is_flagged_and_still_shipped() {
        if cfg!(target_os = "macos") {
            assert!(is_risky_on_macos(&default_panel_hotkey()));
            assert_eq!(
                caution_for(&default_panel_hotkey()),
                Some(Caution::ShiftOrOptionOnly)
            );
            // Soft, not hard: nothing refuses to take it.
            assert!(breaks_os_shortcut(&default_panel_hotkey()).is_none());
            assert!(!Caution::ShiftOrOptionOnly.explanation().is_empty());
        } else {
            // Ctrl+Shift+Space is not shift/option-only, and the caution is
            // macOS-only regardless.
            assert!(!is_risky_on_macos(&default_panel_hotkey()));
            assert_eq!(caution_for(&default_panel_hotkey()), None);
        }
    }

    /// Regression, F6. `-9868` and `-9878` must not collapse into one message.
    #[test]
    fn os_status_codes_map_to_distinct_reasons() {
        assert_eq!(
            parse_os_status("RegisterEventHotKey failed: -9868"),
            Some(OS_STATUS_MODIFIERS_REJECTED)
        );
        assert_eq!(
            parse_os_status("RegisterEventHotKey failed: -9878"),
            Some(OS_STATUS_HOT_KEY_EXISTS)
        );

        let modifiers = FailureReason::from_register_error(
            &global_hotkey::Error::FailedToRegister("RegisterEventHotKey failed: -9868".into()),
        );
        let owned = FailureReason::from_register_error(&global_hotkey::Error::FailedToRegister(
            "RegisterEventHotKey failed: -9878".into(),
        ));
        assert_eq!(modifiers, FailureReason::ModifiersRejected);
        assert_eq!(owned, FailureReason::AlreadyOwned);
        assert_ne!(modifiers, owned);

        // `AlreadyRegistered` is the same user-facing situation.
        assert_eq!(
            FailureReason::from_register_error(&global_hotkey::Error::AlreadyRegistered(
                default_panel_hotkey()
            )),
            FailureReason::AlreadyOwned
        );
    }

    /// Positive integers in these messages are key names (`Digit1`, `F13`), not
    /// status codes, and must not be classified.
    #[test]
    fn only_negative_status_codes_are_parsed() {
        assert_eq!(parse_os_status("Unknown VKCode for F13"), None);
        assert_eq!(parse_os_status("Unknown scancode for Digit1"), None);
        assert_eq!(parse_os_status("nothing here"), None);
        assert_eq!(parse_os_status("dash-then-letters"), None);
    }

    /// Regression, F6. `global-hotkey` 0.8.0's macOS backend drops the
    /// `OSStatus` before constructing the error, so the string it produces
    /// carries nothing to classify. This pins the *known gap* rather than
    /// pretending it is covered: if a future release starts including the code,
    /// this test fails and the doc comment on
    /// [`FailureReason::from_register_error`] gets to be deleted.
    #[test]
    fn the_macos_backend_string_carries_no_code_to_parse() {
        let as_upstream_writes_it = "RegisterEventHotKey failed for Space";
        assert_eq!(parse_os_status(as_upstream_writes_it), None);
        assert!(matches!(
            FailureReason::from_register_error(&global_hotkey::Error::FailedToRegister(
                as_upstream_writes_it.into()
            )),
            FailureReason::Unclassified(_)
        ));
    }

    /// A [`Registrar`] that refuses the combinations it is told to refuse.
    #[derive(Default)]
    struct FakeRegistrar {
        refuse: std::cell::RefCell<Vec<HotKey>>,
        /// Combinations whose *release* fails — the leaked-probe case.
        refuse_release: std::cell::RefCell<Vec<HotKey>>,
        live: std::cell::RefCell<Vec<HotKey>>,
        /// Every `register` attempt, in order. Lets a test assert that a
        /// candidate was rejected *without* the OS being touched.
        attempts: std::cell::RefCell<Vec<HotKey>>,
    }

    impl FakeRegistrar {
        fn refusing(refuse: &[HotKey]) -> Self {
            Self {
                refuse: std::cell::RefCell::new(refuse.to_vec()),
                ..Self::default()
            }
        }
    }

    impl Registrar for FakeRegistrar {
        fn register(&self, hotkey: HotKey) -> std::result::Result<(), global_hotkey::Error> {
            self.attempts.borrow_mut().push(hotkey);
            if self.refuse.borrow().contains(&hotkey) {
                return Err(global_hotkey::Error::FailedToRegister(
                    "RegisterEventHotKey failed: -9878".into(),
                ));
            }
            self.live.borrow_mut().push(hotkey);
            Ok(())
        }

        fn unregister(&self, hotkey: HotKey) -> std::result::Result<(), global_hotkey::Error> {
            if self.refuse_release.borrow().contains(&hotkey) {
                return Err(global_hotkey::Error::FailedToUnRegister(hotkey));
            }
            self.live.borrow_mut().retain(|live| *live != hotkey);
            Ok(())
        }
    }

    /// A combination that is neither the platform default nor the one we try to
    /// rebind to, so "restored the previous binding" and "fell back to the
    /// platform default" cannot be confused.
    fn user_chosen() -> HotKey {
        HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyJ)
    }

    /// Regression, F6. A refused rebind restored `Binding::default_for(action)`
    /// — the *platform* default — not the binding the user actually had.
    #[test]
    fn a_refused_rebind_restores_the_previous_binding_not_the_default() {
        let refused = HotKey::new(Some(Modifiers::CONTROL), Code::KeyQ);
        let registrar = FakeRegistrar::refusing(&[refused]);
        let mut hotkeys = Hotkeys::with_registrar(registrar);

        let installed = hotkeys.register(Binding::new(HotkeyAction::TogglePanel, user_chosen()));
        assert!(matches!(installed, HotkeyStatus::Registered { .. }));

        let status = hotkeys.rebind(HotkeyAction::TogglePanel, refused);
        assert!(matches!(status, HotkeyStatus::Failed { .. }));

        let live: Vec<HotKey> = hotkeys.bindings().iter().map(|b| b.hotkey).collect();
        assert_eq!(live, vec![user_chosen()]);
        assert_ne!(live, vec![default_panel_hotkey()]);
        assert_eq!(
            hotkeys.action_for(user_chosen().id()),
            Some(HotkeyAction::TogglePanel)
        );
        // And it is registered with the OS, not merely recorded.
        assert_eq!(*hotkeys.manager.live.borrow(), vec![user_chosen()]);
    }

    /// Regression, F6. The restoring `register` was `let _`-discarded, so if the
    /// previous combination could no longer be taken, `bindings()` still
    /// reported it as live and `action_for` matched an id the OS would never
    /// deliver.
    #[test]
    fn a_failed_restore_is_propagated_and_not_recorded_as_live() {
        let refused = HotKey::new(Some(Modifiers::CONTROL), Code::KeyQ);
        let registrar = FakeRegistrar::default();
        let mut hotkeys = Hotkeys::with_registrar(registrar);

        assert!(matches!(
            hotkeys.register(Binding::new(HotkeyAction::TogglePanel, user_chosen())),
            HotkeyStatus::Registered { .. }
        ));

        // Another app grabs the old combination while the picker is open, so
        // neither the new nor the old one can be taken.
        hotkeys
            .manager
            .refuse
            .borrow_mut()
            .extend([refused, user_chosen()]);

        let status = hotkeys.rebind(HotkeyAction::TogglePanel, refused);
        match status {
            HotkeyStatus::Failed { combo, reason } => {
                assert_eq!(combo, describe(&user_chosen()));
                assert_eq!(reason, FailureReason::AlreadyOwned);
            }
            HotkeyStatus::Registered { .. } => panic!("nothing is registered"),
        }
        assert!(hotkeys.bindings().is_empty());
        assert_eq!(hotkeys.action_for(user_chosen().id()), None);
        assert!(hotkeys.manager.live.borrow().is_empty());
    }

    /// A rebind that succeeds replaces the binding and leaves exactly one live.
    #[test]
    fn a_successful_rebind_replaces_the_previous_binding() {
        let mut hotkeys = Hotkeys::with_registrar(FakeRegistrar::default());
        hotkeys.register(Binding::new(HotkeyAction::TogglePanel, user_chosen()));

        let next = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyK);
        assert!(matches!(
            hotkeys.rebind(HotkeyAction::TogglePanel, next),
            HotkeyStatus::Registered { .. }
        ));
        assert_eq!(*hotkeys.manager.live.borrow(), vec![next]);
        assert_eq!(hotkeys.bindings().len(), 1);
        assert_eq!(
            hotkeys.action_for(next.id()),
            Some(HotkeyAction::TogglePanel)
        );
    }

    #[test]
    fn parse_round_trips_a_stored_spec() {
        let hk = parse("control+shift+Space").expect("valid spec");
        assert_eq!(hk.key, Code::Space);
        assert!(parse("definitely not a hotkey").is_err());
    }

    /// Regression. `Win+Space` is the Windows input-source switcher — the exact
    /// counterpart of symbolic hotkeys 60/61 on macOS. Taking it on a Japanese
    /// PC removes the user's only kana/direct-input toggle, and `RegisterHotKey`
    /// gives no error while doing it.
    #[test]
    fn win_space_is_refused_on_windows() {
        let win_space = HotKey::new(Some(Modifiers::SUPER), Code::Space);
        #[cfg(target_os = "windows")]
        assert!(breaks_os_shortcut(&win_space).is_some());
        #[cfg(not(target_os = "windows"))]
        assert!(breaks_os_shortcut(&win_space).is_none());
        // Still not the same rule as Alt+Space: adding shift takes it out.
        assert!(
            breaks_os_shortcut(&HotKey::new(
                Some(Modifiers::SUPER | Modifiers::SHIFT),
                Code::Space
            ))
            .is_none()
        );
    }

    // -----------------------------------------------------------------------
    // §9 conflict detection
    //
    // Every fixture below is synthetic. Nothing here reads this machine's
    // `com.apple.symbolichotkeys`: the tests must pass on a US-QWERTY Mac with
    // factory settings, on the Japanese-configured Mac these numbers were
    // measured on, and on the Windows CI runner.
    // -----------------------------------------------------------------------

    /// Wrap `entries` (already-formatted `<key>…</key><dict>…</dict>` pairs) in
    /// the envelope `defaults export com.apple.symbolichotkeys -` produces.
    fn fixture(entries: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AppleSymbolicHotKeys</key>
	<dict>
{entries}
	</dict>
</dict>
</plist>
"#
        )
    }

    /// One `standard` entry.
    fn entry(id: u32, enabled: bool, character: i64, key_code: u16, mask: u64) -> String {
        let flag = if enabled { "<true/>" } else { "<false/>" };
        format!(
            "\t\t<key>{id}</key>
\t\t<dict>
\t\t\t<key>enabled</key>
\t\t\t{flag}
\t\t\t<key>value</key>
\t\t\t<dict>
\t\t\t\t<key>parameters</key>
\t\t\t\t<array>
\t\t\t\t\t<integer>{character}</integer>
\t\t\t\t\t<integer>{key_code}</integer>
\t\t\t\t\t<integer>{mask}</integer>
\t\t\t\t</array>
\t\t\t\t<key>type</key>
\t\t\t\t<string>standard</string>
\t\t\t</dict>
\t\t</dict>"
        )
    }

    /// The `{ enabled = 0; }` shape macOS writes when a factory shortcut is
    /// switched off without being rebound. No `value` key at all.
    fn flag_only(id: u32, enabled: bool) -> String {
        let flag = if enabled { "<true/>" } else { "<false/>" };
        format!(
            "\t\t<key>{id}</key>
\t\t<dict>
\t\t\t<key>enabled</key>
\t\t\t{flag}
\t\t</dict>"
        )
    }

    /// Verbatim transcription of the machine measured on 2026-07-26: symbolic
    /// 60 and 61 enabled on `⌃Space` and `⌃⌥Space`, the space-switching pair
    /// carrying `NSEventModifierFlagFunction`, and 164 cleared.
    fn measured_japanese_mac() -> SymbolicHotkeyMap {
        let xml = fixture(
            &[
                flag_only(15, false),
                entry(60, true, 32, 49, 262_144),
                entry(61, true, 32, 49, 786_432),
                entry(79, true, 65_535, 123, 8_650_752),
                entry(80, true, 65_535, 123, 8_781_824),
                entry(118, true, 49, 18, 262_144),
                entry(164, false, 65_535, 65_535, 0),
            ]
            .join("\n"),
        );
        SymbolicHotkeyMap::parse_defaults_export(&xml).expect("fixture parses")
    }

    fn combo(mods: Modifiers, key: Code) -> HotKey {
        HotKey::new(Some(mods), key)
    }

    /// The measurement this whole module exists for. `⌃Space` and `⌃⌥Space` are
    /// owned by the input-source switcher on a Japanese-configured Mac, and the
    /// owner is reported by name.
    #[test]
    fn the_input_source_switcher_is_detected_as_the_owner() {
        let map = measured_japanese_mac();

        let previous = map
            .owner_of(&combo(Modifiers::CONTROL, Code::Space))
            .expect("⌃Space is taken");
        assert_eq!(previous.id, 60);
        assert_eq!(previous.action(), "Select the previous input source");
        assert_eq!(previous.source, ShortcutSource::UserPreference);

        let next = map
            .owner_of(&combo(Modifiers::CONTROL | Modifiers::ALT, Code::Space))
            .expect("⌃⌥Space is taken");
        assert_eq!(next.id, 61);
        assert_eq!(next.action(), "Select the next source in the Input menu");
    }

    /// Regression. A stored mask carries device-independent bits for the *key*:
    /// `⌃←` is `8650752` (`Function | Control`) and `⌃⇧←` is `8781824`.
    /// Comparing raw masks makes both fail to match, so the arrow-key system
    /// shortcuts silently become "free".
    #[test]
    fn device_independent_mask_bits_are_ignored() {
        let map = measured_japanese_mac();

        let left = map
            .owner_of(&combo(Modifiers::CONTROL, Code::ArrowLeft))
            .expect("⌃← is Move left a space");
        assert_eq!(left.id, 79);
        assert_eq!(left.modifiers, Modifiers::CONTROL);

        let shift_left = map
            .owner_of(&combo(
                Modifiers::CONTROL | Modifiers::SHIFT,
                Code::ArrowLeft,
            ))
            .expect("⌃⇧← is taken too");
        assert_eq!(shift_left.id, 80);

        // And the two do not collapse into each other.
        assert_ne!(left.modifiers, shift_left.modifiers);
    }

    /// Regression, and the reason a factory table exists at all. The measured
    /// machine's preference file has 23 entries and `⌘Space` is not among them,
    /// yet Spotlight plainly owns it. Absence from the plist is not evidence of
    /// freedom.
    #[test]
    fn absence_from_the_preference_file_is_not_freedom() {
        let map = measured_japanese_mac();

        let spotlight = map
            .owner_of(&combo(Modifiers::SUPER, Code::Space))
            .expect("⌘Space is Spotlight even when unmentioned");
        assert_eq!(spotlight.id, 64);
        assert_eq!(spotlight.action(), "Show Spotlight search");
        assert_eq!(spotlight.source, ShortcutSource::FactoryDefault);

        // Same for the two other space-family combinations a picker reaches for.
        assert_eq!(
            map.owner_of(&combo(Modifiers::ALT | Modifiers::SUPER, Code::Space))
                .map(|o| o.id),
            Some(65)
        );
        assert_eq!(
            map.owner_of(&combo(Modifiers::CONTROL | Modifiers::SUPER, Code::Space))
                .map(|o| o.id),
            Some(179)
        );
    }

    /// The preference file wins over the factory table in both directions:
    /// disabling frees a combination, rebinding moves it.
    #[test]
    fn the_preference_file_overrides_the_factory_table() {
        // 60 switched off with the `{ enabled = 0; }` shape — no `value` key,
        // so the factory binding has to be remembered to know *what* was
        // switched off.
        let disabled = SymbolicHotkeyMap::parse_defaults_export(&fixture(&flag_only(60, false)))
            .expect("fixture parses");
        assert!(
            disabled
                .owner_of(&combo(Modifiers::CONTROL, Code::Space))
                .is_none(),
            "a disabled system shortcut is not a conflict"
        );
        let entry = disabled
            .entries()
            .find(|e| e.id == 60)
            .expect("still known");
        assert!(!entry.enabled);
        assert_eq!(
            entry.key_code, 49,
            "factory key kept so it can be re-enabled"
        );
        assert_eq!(entry.source, ShortcutSource::UserPreference);

        // 64 rebound off Space and onto ⌘F1: ⌘Space frees up, ⌘F1 does not.
        let moved = SymbolicHotkeyMap::parse_defaults_export(&fixture(&entry_moved()))
            .expect("fixture parses");
        assert!(
            moved
                .owner_of(&combo(Modifiers::SUPER, Code::Space))
                .is_none()
        );
        assert_eq!(
            moved
                .owner_of(&combo(Modifiers::SUPER, Code::F1))
                .map(|o| o.id),
            Some(64)
        );
    }

    fn entry_moved() -> String {
        entry(64, true, 65_535, 122, COCOA_COMMAND)
    }

    /// A cleared shortcut (`[65535, 65535, 0]`) must drop the factory default
    /// too, otherwise the stock table keeps claiming a combination the user has
    /// deliberately freed.
    #[test]
    fn a_cleared_shortcut_releases_its_factory_binding() {
        let xml = fixture(&entry(64, false, 65_535, 65_535, 0));
        let map = SymbolicHotkeyMap::parse_defaults_export(&xml).expect("fixture parses");
        assert!(map.entries().all(|e| e.id != 64));
        assert!(
            map.owner_of(&combo(Modifiers::SUPER, Code::Space))
                .is_none(),
            "⌘Space is genuinely free once Spotlight's shortcut is cleared"
        );
    }

    /// The four Cocoa modifier bits, decoded and re-encoded.
    #[test]
    fn cocoa_modifier_bits_decode() {
        assert_eq!(cocoa_modifiers(COCOA_SHIFT), Modifiers::SHIFT);
        assert_eq!(cocoa_modifiers(COCOA_CONTROL), Modifiers::CONTROL);
        assert_eq!(cocoa_modifiers(COCOA_OPTION), Modifiers::ALT);
        assert_eq!(cocoa_modifiers(COCOA_COMMAND), Modifiers::SUPER);
        assert_eq!(cocoa_modifiers(131_072), Modifiers::SHIFT);
        assert_eq!(cocoa_modifiers(262_144), Modifiers::CONTROL);
        assert_eq!(cocoa_modifiers(524_288), Modifiers::ALT);
        assert_eq!(cocoa_modifiers(1_048_576), Modifiers::SUPER);
        assert_eq!(
            cocoa_modifiers(786_432),
            Modifiers::CONTROL | Modifiers::ALT
        );
        // Function (8388608), numeric pad (2097152) and caps lock (65536) are
        // not modifiers a user holds.
        assert_eq!(
            cocoa_modifiers(8_388_608 | 2_097_152 | 65_536),
            Modifiers::empty()
        );

        for mods in [
            Modifiers::empty(),
            Modifiers::SHIFT,
            Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER,
            Modifiers::SHIFT | Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER,
        ] {
            assert_eq!(cocoa_modifiers(cocoa_mask(mods)), mods);
        }
    }

    /// The virtual key codes the factory table is written in terms of. If these
    /// drift, every `FACTORY_BINDINGS` row silently stops matching.
    #[test]
    fn virtual_key_codes_match_the_factory_table() {
        assert_eq!(macos_virtual_key_code(Code::Space), Some(49));
        assert_eq!(macos_virtual_key_code(Code::ArrowLeft), Some(123));
        assert_eq!(macos_virtual_key_code(Code::ArrowUp), Some(126));
        assert_eq!(macos_virtual_key_code(Code::Digit3), Some(20));
        assert_eq!(macos_virtual_key_code(Code::KeyD), Some(2));
        assert_eq!(macos_virtual_key_code(Code::Slash), Some(44));
        assert_eq!(macos_virtual_key_code(Code::F1), Some(122));
        assert_eq!(macos_virtual_key_code(Code::F13), Some(105));
        // Unmapped is "cannot compare", never "free".
        assert_eq!(macos_virtual_key_code(Code::Fn), None);

        // No factory row may be unbound, and no id may appear twice — a
        // duplicate would make `owner_of` report whichever `BTreeMap` insertion
        // happened to win.
        let mut seen = std::collections::BTreeSet::new();
        for &(id, key_code, mask) in FACTORY_BINDINGS {
            assert_ne!(key_code, NO_KEY, "factory entry {id} is unbound");
            assert_ne!(
                mask & COCOA_USER_MODIFIERS,
                0,
                "factory entry {id} has no modifiers"
            );
            assert!(seen.insert(id), "factory entry {id} is listed twice");
        }
    }

    /// A system shortcut must be found **without** touching the OS: a probe
    /// registration of `⌃Space` succeeds, because the WindowServer eats the key
    /// before Carbon dispatches it. Probing would report "free" and be wrong.
    #[test]
    fn a_system_shortcut_is_detected_without_probing() {
        let map = measured_japanese_mac();
        let registrar = FakeRegistrar::default();
        let report = check_candidate(&map, &registrar, combo(Modifiers::CONTROL, Code::Space));

        assert!(!report.is_free());
        assert_eq!(report.probe, ProbeOutcome::Skipped);
        assert!(
            registrar.attempts.borrow().is_empty(),
            "the OS must not be touched for a combination we already know is taken"
        );
        match &report.conflict {
            Some(Conflict::SystemShortcut { id, action, source }) => {
                assert_eq!(*id, 60);
                assert_eq!(action, "Select the previous input source");
                assert_eq!(*source, ShortcutSource::UserPreference);
            }
            other => panic!("expected a system-shortcut conflict, got {other:?}"),
        }
        assert!(
            report
                .summary()
                .contains("Select the previous input source")
        );
    }

    /// A free candidate is registered and **released**. Leaving the probe
    /// registered would make the subsequent real registration fail against
    /// aibo's own leftover.
    #[test]
    fn a_free_candidate_is_probed_and_released() {
        let map = measured_japanese_mac();
        let registrar = FakeRegistrar::default();
        let candidate = combo(
            Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER,
            Code::Space,
        );

        let report = check_candidate(&map, &registrar, candidate);
        assert!(report.is_free());
        assert_eq!(report.probe, ProbeOutcome::Free);
        assert_eq!(*registrar.attempts.borrow(), vec![candidate]);
        assert!(
            registrar.live.borrow().is_empty(),
            "the probe must not stay registered"
        );
    }

    /// Raycast holding `⌥Space` is invisible in every preference file; it shows
    /// up only as a refused registration, and it must be classified rather than
    /// reported as a raw string.
    #[test]
    fn another_app_holding_the_combination_is_found_by_the_probe() {
        let map = measured_japanese_mac();
        let raycast = combo(Modifiers::ALT, Code::Space);
        let registrar = FakeRegistrar::refusing(&[raycast]);

        let report = check_candidate(&map, &registrar, raycast);
        assert!(!report.is_free());
        assert_eq!(
            report.conflict,
            Some(Conflict::AlreadyHeld {
                reason: FailureReason::AlreadyOwned
            })
        );
        assert_eq!(
            report.probe,
            ProbeOutcome::Refused(FailureReason::AlreadyOwned)
        );
        assert!(registrar.live.borrow().is_empty());
    }

    /// A probe that registers but cannot be released has left aibo holding the
    /// combination. That is not a conflict — but it must be visible, because a
    /// later `AlreadyOwned` on this combination would otherwise be blamed on
    /// another app.
    #[test]
    fn a_probe_that_cannot_be_released_is_reported_not_swallowed() {
        let map = SymbolicHotkeyMap::empty();
        let candidate = combo(Modifiers::CONTROL | Modifiers::SHIFT, Code::KeyK);
        let registrar = FakeRegistrar::default();
        registrar.refuse_release.borrow_mut().push(candidate);

        let report = check_candidate(&map, &registrar, candidate);
        assert!(report.is_free(), "nothing else owns it");
        assert!(matches!(report.probe, ProbeOutcome::HeldByProbe(_)));
        assert_eq!(*registrar.live.borrow(), vec![candidate]);
    }

    /// The end-to-end measurement of 2026-07-26, as a fixture: Raycast on
    /// `⌥Space`, the input-source switcher on `⌃Space` and `⌃⌥Space`, Finder
    /// search on `⌥⌘Space`, Emoji & Symbols on `⌃⌘Space`. Every obvious default
    /// is taken and the suggestion has to walk past all five, saying why.
    #[test]
    fn suggest_free_binding_walks_past_every_taken_default() {
        let map = measured_japanese_mac();
        let raycast = combo(Modifiers::ALT, Code::Space);
        let registrar = FakeRegistrar::refusing(&[raycast]);

        let candidates = vec![
            raycast,
            combo(Modifiers::CONTROL, Code::Space),
            combo(Modifiers::CONTROL | Modifiers::ALT, Code::Space),
            combo(Modifiers::ALT | Modifiers::SUPER, Code::Space),
            combo(Modifiers::CONTROL | Modifiers::SUPER, Code::Space),
            combo(
                Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER,
                Code::Space,
            ),
        ];
        let suggestion = suggest_free_binding_among(&map, &registrar, &candidates);

        let chosen = suggestion.chosen.expect("one candidate survives");
        assert_eq!(chosen.hotkey, candidates[5]);
        assert_eq!(chosen.probe, ProbeOutcome::Free);

        // Every rejection, in rank order, with its own reason.
        let reasons: Vec<Option<Conflict>> = suggestion
            .rejected
            .iter()
            .map(|r| r.conflict.clone())
            .collect();
        assert_eq!(reasons.len(), 5);
        for (index, expected_id) in [(1, 60), (2, 61), (3, 65), (4, 179)] {
            match &reasons[index] {
                Some(Conflict::SystemShortcut { id, .. }) => assert_eq!(*id, expected_id),
                other => panic!("candidate {index} should be a system shortcut, got {other:?}"),
            }
        }

        // `⌥Space` is rejected on both platforms, for two different reasons:
        // on macOS Raycast holds it and only the probe can see that, while on
        // Windows aibo refuses it outright (§9's system menu) and never probes.
        if cfg!(target_os = "windows") {
            assert!(matches!(reasons[0], Some(Conflict::BreaksOsShortcut(_))));
            assert_eq!(*registrar.attempts.borrow(), vec![candidates[5]]);
        } else {
            assert_eq!(
                reasons[0],
                Some(Conflict::AlreadyHeld {
                    reason: FailureReason::AlreadyOwned
                })
            );
            // Only the two combinations with no known system owner were ever
            // registered: ⌥Space (refused) and the winner (probed, released).
            assert_eq!(*registrar.attempts.borrow(), vec![raycast, candidates[5]]);
        }
        assert!(registrar.live.borrow().is_empty());
    }

    /// "Every candidate is taken" is a real outcome on the measured machine,
    /// not a defensive branch. It must not silently degrade into the platform
    /// default, which is one of the combinations already known not to work.
    #[test]
    fn no_free_candidate_reports_nothing_rather_than_the_default() {
        let map = measured_japanese_mac();
        let candidates = vec![
            combo(Modifiers::CONTROL, Code::Space),
            combo(Modifiers::CONTROL | Modifiers::ALT, Code::Space),
            combo(Modifiers::SUPER, Code::Space),
        ];
        let registrar = FakeRegistrar::default();
        let suggestion = suggest_free_binding_among(&map, &registrar, &candidates);

        assert!(suggestion.chosen.is_none());
        assert_eq!(suggestion.rejected.len(), 3);
        assert!(suggestion.rejected.iter().all(|r| !r.is_free()));
        assert!(registrar.attempts.borrow().is_empty());
    }

    /// The §9 caution is orthogonal to conflict detection: `⌥Space` on a Mac
    /// with no launcher installed is free *and* carries the shift/option-only
    /// warning. Neither one may suppress the other.
    #[test]
    fn a_free_candidate_can_still_carry_the_caution() {
        let map = SymbolicHotkeyMap::empty();
        let registrar = FakeRegistrar::default();
        // Shift/option-only, and refused by nothing on any platform.
        let candidate = combo(Modifiers::SHIFT | Modifiers::ALT, Code::KeyK);
        let report = check_candidate(&map, &registrar, candidate);

        assert!(report.is_free());
        assert_eq!(report.caution, caution_for(&candidate));
        if cfg!(target_os = "macos") {
            assert_eq!(report.caution, Some(Caution::ShiftOrOptionOnly));
            assert!(report.summary().contains("available"));
        }
    }

    /// A combination aibo refuses outright is never handed to the OS, not even
    /// for the instant a probe would hold it.
    #[test]
    fn a_refused_combination_is_never_probed() {
        let map = SymbolicHotkeyMap::empty();
        let registrar = FakeRegistrar::default();
        let report = check_candidate(&map, &registrar, combo(Modifiers::ALT, Code::Space));

        if cfg!(target_os = "windows") {
            assert!(matches!(
                report.conflict,
                Some(Conflict::BreaksOsShortcut(_))
            ));
            assert_eq!(report.probe, ProbeOutcome::Skipped);
            assert!(registrar.attempts.borrow().is_empty());
        } else {
            assert_eq!(report.probe, ProbeOutcome::Free);
        }
    }

    /// The ranked list starts at §9's platform default and holds no duplicates.
    #[test]
    fn the_candidate_list_is_ranked_and_distinct() {
        let candidates = default_candidates();
        assert_eq!(candidates[0], default_panel_hotkey());
        for (index, candidate) in candidates.iter().enumerate() {
            assert!(
                !candidates[..index].contains(candidate),
                "{} appears twice",
                describe(candidate)
            );
        }
        // Whatever the platform, a candidate `breaks_os_shortcut` refuses is
        // present only so the report can explain it — never as a silent skip.
        let refused = candidates
            .iter()
            .filter(|c| breaks_os_shortcut(c).is_some())
            .count();
        if cfg!(target_os = "windows") {
            assert_eq!(refused, 2, "Alt+Space and Win+Space");
        } else {
            assert_eq!(refused, 0);
        }
    }

    /// The plist reader handles the shapes `defaults export` actually emits.
    #[test]
    fn the_plist_reader_handles_the_shapes_defaults_emits() {
        // An empty domain is valid and means "everything is factory".
        let empty = SymbolicHotkeyMap::parse_defaults_export(&fixture("")).expect("empty domain");
        assert_eq!(
            empty
                .owner_of(&combo(Modifiers::SUPER, Code::Space))
                .map(|o| o.id),
            Some(64)
        );

        // A domain that does not mention AppleSymbolicHotKeys at all.
        let other = SymbolicHotkeyMap::parse_defaults_export(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Something</key><string>else</string></dict></plist>"#,
        )
        .expect("unrelated domain");
        assert!(other.entries().count() > 0, "factory table still applies");

        // Non-numeric keys are skipped rather than failing the whole read.
        let odd = SymbolicHotkeyMap::parse_defaults_export(&fixture(
            "\t\t<key>notanid</key>\n\t\t<dict><key>enabled</key><true/></dict>",
        ))
        .expect("tolerates unknown keys");
        assert!(odd.entries().all(|e| e.id != 0));

        // Truncated input is an error, not a silent empty map.
        assert!(matches!(
            SymbolicHotkeyMap::parse_defaults_export("<plist><dict><key>a</key>"),
            Err(SymbolicHotkeyError::Malformed(_))
        ));
        assert!(matches!(
            SymbolicHotkeyMap::parse_defaults_export(""),
            Err(SymbolicHotkeyError::Malformed(_))
        ));
    }

    /// `<integer>0</integer>` for `enabled` and `<real>` parameters both occur
    /// in this domain and mean what the typed forms mean.
    #[test]
    fn the_plist_reader_accepts_integer_and_real_spellings() {
        let xml = fixture(
            "\t\t<key>60</key>
\t\t<dict>
\t\t\t<key>enabled</key>
\t\t\t<integer>0</integer>
\t\t\t<key>value</key>
\t\t\t<dict>
\t\t\t\t<key>parameters</key>
\t\t\t\t<array>
\t\t\t\t\t<integer>32</integer>
\t\t\t\t\t<real>49</real>
\t\t\t\t\t<real>262144</real>
\t\t\t\t</array>
\t\t\t</dict>
\t\t</dict>",
        );
        let map = SymbolicHotkeyMap::parse_defaults_export(&xml).expect("fixture parses");
        let entry = map.entries().find(|e| e.id == 60).expect("entry 60");
        assert_eq!(entry.key_code, 49);
        assert_eq!(entry.modifiers, Modifiers::CONTROL);
        assert!(!entry.enabled, "<integer>0</integer> means disabled");
    }

    /// Unknown ids get a fallback that names the number and claims nothing
    /// else — an invented label would be a confident wrong sentence.
    #[test]
    fn unknown_symbolic_ids_are_named_honestly() {
        assert_eq!(
            system_action_name(60),
            Some("Select the previous input source")
        );
        assert_eq!(system_action_name(9_999), None);

        let xml = fixture(&entry(9_999, true, 65_535, 105, COCOA_CONTROL));
        let map = SymbolicHotkeyMap::parse_defaults_export(&xml).expect("fixture parses");
        let owner = map
            .owner_of(&combo(Modifiers::CONTROL, Code::F13))
            .expect("still a conflict");
        assert_eq!(owner.action(), "a macOS system shortcut (#9999)");
        assert!(!owner.action().is_empty());
    }

    /// Every conflict renders one non-empty sentence, and the two sources say
    /// different things: the preference file is a fact about this machine, the
    /// factory table is an assumption about a stock install.
    #[test]
    fn every_conflict_has_distinct_renderable_copy() {
        let map = measured_japanese_mac();
        // A combination no platform rule refuses, so the probe is what rejects
        // it on macOS *and* on Windows.
        let held_by_an_app = combo(Modifiers::CONTROL | Modifiers::SHIFT, Code::KeyJ);
        let registrar = FakeRegistrar::refusing(&[held_by_an_app]);

        let from_preference =
            check_candidate(&map, &registrar, combo(Modifiers::CONTROL, Code::Space)).summary();
        // Symbolic 65 (⌥⌘Space), which no `breaks_os_shortcut` rule touches.
        let from_factory = check_candidate(
            &map,
            &registrar,
            combo(Modifiers::ALT | Modifiers::SUPER, Code::Space),
        )
        .summary();
        let from_probe = check_candidate(&map, &registrar, held_by_an_app).summary();
        let free = check_candidate(
            &map,
            &registrar,
            combo(Modifiers::CONTROL | Modifiers::SHIFT, Code::KeyK),
        )
        .summary();

        for text in [&from_preference, &from_factory, &from_probe, &free] {
            assert!(!text.is_empty());
        }
        assert_ne!(from_preference, from_factory);
        assert_ne!(from_factory, from_probe);
        assert_ne!(from_probe, free);
        assert!(from_preference.contains("System Settings"));
        assert!(from_factory.contains("normally"));
    }

    /// Regression. The settings picker has to probe through [`Hotkeys`] —
    /// `GlobalHotKeyManager::new()` fails the second time in a process — and
    /// that registrar is already holding aibo's own shortcut. Probing it would
    /// refuse against aibo's own registration and tell the user that another
    /// app owns the shortcut they are using right now.
    #[test]
    fn aibos_own_binding_is_not_reported_as_a_conflict_with_itself() {
        let mine = user_chosen();
        let mut hotkeys = Hotkeys::with_registrar(FakeRegistrar::default());
        assert!(matches!(
            hotkeys.register(Binding::new(HotkeyAction::TogglePanel, mine)),
            HotkeyStatus::Registered { .. }
        ));
        // Registered once and still live; a second `register` would refuse.
        hotkeys.manager.refuse.borrow_mut().push(mine);

        let map = SymbolicHotkeyMap::empty();
        let report = hotkeys.check_candidate(&map, mine);
        assert!(report.is_free(), "aibo already holds this and it works");
        assert_eq!(report.probe, ProbeOutcome::Skipped);
        assert_eq!(*hotkeys.manager.live.borrow(), vec![mine]);

        // And the walk picks it rather than stepping past it.
        let suggestion = hotkeys.suggest_free_binding_among(&map, &[mine, default_panel_hotkey()]);
        assert_eq!(suggestion.chosen.map(|c| c.hotkey), Some(mine));
        assert!(suggestion.rejected.is_empty());
    }

    /// Regression, measured 2026-07-26. A duplicate `RegisterEventHotKey` on
    /// macOS fails as
    /// `Unclassified("Unable to register hotkey: RegisterEventHotKey failed for Space")`,
    /// because `global-hotkey` 0.8 drops the `OSStatus` (§8). That is the
    /// *ordinary* macOS path, so the copy for it must not silently reuse the
    /// `-9878` sentence: "another app already owns this" is a claim the code
    /// cannot support, and it sends a user hunting for an app when macOS may
    /// simply have refused the modifiers.
    #[test]
    fn an_unclassifiable_refusal_does_not_claim_another_app_owns_it() {
        let combos = |reason: FailureReason| ConflictReport {
            hotkey: combo(Modifiers::ALT, Code::Space),
            display: "⌥Space".to_owned(),
            conflict: Some(Conflict::AlreadyHeld { reason }),
            caution: None,
            probe: ProbeOutcome::Skipped,
        };

        let upstream_macos_string =
            FailureReason::from_register_error(&global_hotkey::Error::FailedToRegister(
                "Unable to register hotkey: RegisterEventHotKey failed for Space".into(),
            ));
        assert!(matches!(
            upstream_macos_string,
            FailureReason::Unclassified(_)
        ));

        let ambiguous = combos(upstream_macos_string).summary();
        let owned = combos(FailureReason::AlreadyOwned).summary();
        let modifiers = combos(FailureReason::ModifiersRejected).summary();

        assert_ne!(ambiguous, owned);
        assert_ne!(ambiguous, modifiers);
        assert_ne!(owned, modifiers);
        assert!(
            !ambiguous.contains("Another app already owns"),
            "an unclassifiable refusal must not assert the -9878 reading: {ambiguous}"
        );
        // It must still name both readings and give an action.
        assert!(ambiguous.contains("another app"));
        assert!(ambiguous.contains("modifiers"));
    }
}
