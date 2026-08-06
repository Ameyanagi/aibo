//! The iced 0.14 daemon: zero windows, a warm hidden panel, a late-built tray (§6).
//!
//! This is the module the whole crate exists to support, and three details in
//! it are non-obvious enough that getting them wrong looks like a working app
//! until it isn't:
//!
//! 1. **`daemon`, not `application`.** iced 0.14's `daemon(boot, update, view)`
//!    runs with *zero windows open* and does not exit when the last one closes.
//!    §6: "the shape a tray app needs, and the reason 0.14 is the right target."
//!
//! 2. **The panel is created hidden in `boot` and painted before it is ever
//!    shown.** A window created on hotkey press costs surface creation plus
//!    first-frame pipeline compile and misses the budget (§6). `iced_winit`
//!    always creates windows with `with_visible(false)` and then flips them, so
//!    `window::Settings { visible: false }` yields a window whose wgpu surface
//!    exists and whose UI tree is built; `set_mode(Mode::Windowed)` maps to
//!    `set_visible(true)`. Showing is then position + show + focus.
//!
//! 3. **The tray is created from the first `update` tick, never from `boot`.**
//!    §6: `tray-icon` requires the event loop to be *already running* — not
//!    merely created — and on macOS the tray must be created on the main
//!    thread. `iced_winit` runs `boot` **before** `event_loop.run_app`, so a
//!    tray built there does not work. `boot` therefore returns a
//!    `Task::done(Message::Ready)` purely to guarantee there *is* a first
//!    update tick, and [`Aibo::update`] does the shell wiring on whichever
//!    message arrives first.
//!
//! iced's own tray-icon integration PR is still open and unmerged (§6), so the
//! event plumbing below is integration work rather than a drop-in.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use accesskit::{NodeId, TreeUpdate};
use aibo_core::types::{AppRef, DisplayInfo, StreamEvent, Surface};
use aibo_platform::{AccessibilityEvent, AccessibilitySurface};
use iced::widget::operation;
use iced::window::{self, Mode};
use iced::{Element, Point, Size, Subscription, Task, Theme};
use secrecy::ExposeSecret as _;
use tokio::sync::mpsc::{Receiver, Sender, error::TrySendError};
use uuid::Uuid;

use crate::a11y;
#[cfg(test)]
use crate::bridge::UI_REQUEST_CHANNEL_CAPACITY;
use crate::bridge::{PersistenceScope, SessionId, UiEvent, UiRequest};
use crate::error::{Result, UiError};
use crate::hotkey::{self, HotkeyAction, HotkeyStatus, Hotkeys};
use crate::i18n::{self, Lang};
use crate::panel::{self, ContextState, PanelState, Phase};
use crate::placement::{self, ObservedGeometry, Placement, PlacementRequest};
use crate::settings::{self, SettingsState};
use crate::tasks::TaskState;
use crate::theme::{self as ui_theme, Appearance, motion::Motion};
use crate::tray::{self, Tray, TrayCommand, TrayState};

enum PersistenceSnapshot {
    Language(Lang),
    Appearance(ui_theme::AppearancePreference),
    Model {
        selected: Option<crate::bridge::ModelOption>,
        recents: Vec<aibo_core::types::ModelBinding>,
    },
    ProviderSave(settings::ProviderDraft),
    ProviderRemove(Option<aibo_core::types::ProviderId>),
    SttEndOnSend(bool),
    SttBackend(settings::SttChoice),
    PinnedModels {
        pins: Vec<aibo_core::types::ModelBinding>,
        customised: bool,
    },
    Hotkey {
        action: HotkeyAction,
        live: Option<hotkey::HotKey>,
        status: Option<HotkeyStatus>,
        draft: String,
    },
    AxTreeActivation(bool),
    FileRoots(Option<Vec<String>>),
    MonthlyBudget {
        configured: bool,
        hard_stop: bool,
        spend_fraction: Option<f32>,
    },
    Updates {
        enabled: bool,
        channel: crate::bridge::UpdateChannel,
    },
    PanelSize(Option<(f32, f32)>),
}

struct PendingPersistence {
    generation: u64,
    rollback: PersistenceSnapshot,
}

// ---------------------------------------------------------------------------
// Configuration and handles
// ---------------------------------------------------------------------------

/// Shell configuration, resolved from the user's config before the loop starts.
#[derive(Debug, Clone)]
pub struct UiConfig {
    /// UI language (§9).
    pub language: Lang,
    /// Light or dark. Dark-first is the product default (§16).
    pub appearance: Appearance,
    /// What the user asked for; `appearance` above is its current resolution.
    /// Kept so "system" can be re-resolved on every panel or settings open.
    pub appearance_preference: ui_theme::AppearancePreference,
    /// Whether animation runs at all (§16 reduced-motion).
    pub motion: Motion,
    /// The panel hotkey. `None` uses the platform default from §9 — `⌥Space`
    /// on macOS, `Ctrl+Shift+Space` on Windows.
    pub panel_hotkey: Option<global_hotkey::hotkey::HotKey>,
    /// Optional override for the screen-region shortcut. `None` uses its
    /// platform default when one exists.
    pub screen_capture_hotkey: Option<global_hotkey::hotkey::HotKey>,
    /// Optional task-window shortcut. This action is unbound by default.
    pub show_tasks_hotkey: Option<global_hotkey::hotkey::HotKey>,
    /// The panel size the user last dragged to (`ui.panel_width` /
    /// `ui.panel_height`). `None` sizes the panel from its content.
    pub panel_size: Option<(f32, f32)>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            language: Lang::default(),
            appearance: Appearance::Dark,
            appearance_preference: ui_theme::AppearancePreference::Dark,
            motion: Motion::Full,
            panel_hotkey: None,
            screen_capture_hotkey: None,
            show_tasks_hotkey: None,
            panel_size: None,
        }
    }
}

/// A corner-grip drag in flight.
///
/// The anchor is taken from the *first* motion rather than from the press:
/// `mouse_area` reports a press without a position, and asking the window
/// server where the cursor is would be a second source of truth for something
/// the next event already carries. Sizes are therefore relative from the first
/// move, which also makes the drag immune to where inside the grip the press
/// landed.
#[derive(Debug, Clone, Copy)]
struct ResizeDrag {
    /// Panel size when the drag began, in logical points.
    origin: (f32, f32),
    /// Cursor position at the first motion of this drag, window-local.
    anchor: Option<Point>,
}

/// The two ends of the §6 bridge, handed to [`run`] by the binary.
pub struct UiHandles {
    /// UI → tokio. Human-scale and bounded, with capacity reserved for
    /// cancellation and lifecycle signals so the UI thread never blocks.
    pub requests: Sender<UiRequest>,
    /// tokio → UI. Bounded and drained by a `Subscription`.
    pub events: Receiver<UiEvent>,
}

// ---------------------------------------------------------------------------
// Process-global plumbing
// ---------------------------------------------------------------------------

/// `Subscription::run` takes a bare `fn` pointer, so the receivers cannot be
/// captured in a closure — they are parked here and taken exactly once by the
/// subscription that owns them.
static BACKEND_EVENTS: Mutex<Option<Receiver<UiEvent>>> = Mutex::new(None);
static SHELL_EVENTS: Mutex<Option<Receiver<ShellEvent>>> = Mutex::new(None);
static ACCESSIBILITY_EVENTS: Mutex<Option<Receiver<AccessibilityEvent>>> = Mutex::new(None);

/// The sender half of the shell channel, held for the process lifetime because
/// `global-hotkey` and `tray-icon` install *global* handlers.
static SHELL_SENDER: OnceLock<Sender<ShellEvent>> = OnceLock::new();

/// Shell events are human/OS lifecycle signals, not model output. A modest
/// bounded queue absorbs normal bursts and coalesces overload by dropping only
/// events that arrive after the queue is already full.
const SHELL_EVENT_CHANNEL_CAPACITY: usize = 32;
/// Assistive-technology input is human-scale and must not create an unbounded
/// queue if a native client repeats an action while the renderer is busy.
const ACCESSIBILITY_EVENT_CHANNEL_CAPACITY: usize = 32;
/// A tray Quit must remain deliverable even if a hotkey source is noisy.
const SHELL_EVENT_CRITICAL_RESERVE: usize = 1;

/// Keep a tail of the UI request queue available for actions whose loss could
/// leave work running or an approval unresolved. Ordinary clicks are declined
/// before they can consume these slots; the queue itself remains the sole
/// backlog, so overload cannot create an unbounded collection of retry tasks.
const UI_REQUEST_CRITICAL_RESERVE: usize = 8;

const PANEL_ACCESSIBILITY: AccessibilitySurface = AccessibilitySurface(1);
const SETTINGS_ACCESSIBILITY: AccessibilitySurface = AccessibilitySurface(2);

/// §6 requires single-instance behaviour across the machine; this only guards
/// the far cheaper in-process case, which the global handlers make unavoidable.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// An event from one of the two OS-level shell sources.
#[derive(Debug, Clone)]
enum ShellEvent {
    /// A registered hotkey fired (key-down only, see [`hotkey::forward_events`]).
    Hotkey(u32),
    /// A tray menu item was chosen.
    Tray(TrayCommand),
}

fn send_shell_event(sender: &Sender<ShellEvent>, event: ShellEvent) {
    let critical = matches!(event, ShellEvent::Tray(TrayCommand::Quit));
    if !critical && sender.capacity() <= SHELL_EVENT_CRITICAL_RESERVE {
        tracing::debug!("shell queue busy; duplicate human input coalesced");
        return;
    }

    match sender.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Closed(_)) => {
            tracing::debug!("shell channel closed; event ignored");
        }
        Err(TrySendError::Full(event)) => {
            tracing::warn!(critical, ?event, "shell event queue saturated");
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Everything the daemon reacts to.
#[derive(Debug, Clone)]
pub enum Message {
    /// The first update tick. Exists so the tray has a legal place to be built.
    Ready,
    /// A window finished opening.
    WindowOpened(window::Id),
    /// A window closed.
    WindowClosed(window::Id),
    /// A frame was painted; drives the warm-up counter (§6).
    FramePainted,
    /// A registered hotkey fired.
    Hotkey(u32),
    /// The OS screen-region picker finished. `None` means the user cancelled.
    ScreenRegionCaptured(std::result::Result<Option<aibo_core::types::Attachment>, String>),
    /// A tray command.
    Tray(TrayCommand),
    /// Show the panel at an already-resolved placement (§9).
    ///
    /// The panel is only ever made visible from a resolved [`Placement`] —
    /// there is no "show it now and position it in a moment" path, because that
    /// is what put it in the top-left corner.
    Place(Placement),
    /// Ask the window server for the panel's monitor size and scale factor.
    ///
    /// §9: recompute on every show, and re-layout on scale-factor and size
    /// changes — not once at creation.
    ProbeGeometry,
    /// The window server answered [`Message::ProbeGeometry`].
    Observed {
        /// Logical size of the monitor the panel window is currently on.
        monitor: Option<Size>,
        /// The panel window's scale factor, as of now.
        scale: f32,
    },
    /// Attached displays changed; re-clamp (§9).
    Displays(Vec<DisplayInfo>),
    /// A keyboard action plus the window that received it (§16).
    ///
    /// The id is load-bearing in a multi-window daemon: `esc` in a task must
    /// deny/close that task, never cancel a hidden panel session.
    WindowKey(window::Id, WindowChord),
    /// Whether this window currently holds non-empty IME preedit text.
    ImePreedit(window::Id, bool),
    /// Whether the platform's command modifier is currently held.
    ///
    /// Tracked because iced's `text_input` on macOS inserts the raw character
    /// of any ⌘-shortcut it does not itself recognise — ⌘L toggled dictation
    /// *and* typed an `l` (owner report, 2026-08-01). The input handlers drop
    /// single-character insertions that arrive while ⌘ is down.
    CommandHeld(bool),
    /// A message from the panel.
    Panel(panel::Message),
    /// A message from a task window.
    /// A message from the settings window.
    Settings(settings::Message),
    /// A semantic action from VoiceOver, Narrator, or another native client.
    Accessibility(AccessibilityEvent),
    /// The native adapter finished attaching to a still-hidden window.
    AccessibilityAttached {
        /// Window receiving the adapter.
        window: window::Id,
        /// Stable semantic surface identity.
        surface: AccessibilitySurface,
        /// Empty on success; contains no user content on failure.
        result: std::result::Result<(), String>,
    },
    /// Initial native scale is known, so the already-adapted window may show.
    AccessibilityReadyToReveal {
        /// Window to reveal or finish configuring.
        window: window::Id,
        /// Surface installed on that window.
        surface: AccessibilitySurface,
        /// Current native scale factor.
        scale: f32,
    },
    /// A window's logical client size changed.
    AccessibilityResize(window::Id, Size),
    /// A window crossed to a display with another scale factor.
    AccessibilityScale(window::Id, f32),
    /// A native host window gained or lost focus.
    AccessibilityFocus(window::Id, bool),
    /// An event from the runtime.
    Backend(Box<UiEvent>),
    /// The native animated resize was unavailable; apply this placement with
    /// iced's instant resize/move instead.
    PanelFrameFallback(Placement),
    /// Pointer motion while the corner grip is held, in window-local points.
    PanelResizeMoved(Point),
    /// The pointer was released, ending a corner-grip drag.
    PanelResizeEnded,
    /// Nothing. Returned where a branch has no work, so `update` stays total.
    Ignored,
}

/// A keyboard action routed using the window that received it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowChord {
    /// Dismiss the current transient state or window.
    Escape,
    /// Activate the state-dependent default action.
    Enter {
        /// Command/Control was held.
        command: bool,
        /// Shift was held.
        shift: bool,
    },
    /// Copy the current surface's content.
    Copy,
    /// Open the model quick-pick.
    PickModel,
    /// Move to the next quick-pick lane. `⇥`, as the placeholder promises.
    NextLane,
    /// Start or finish push-to-talk dictation (`⌘L`, §P9+).
    Dictate,
    /// Flip agent mode for the session (`⌘J`, §1 Do).
    AgentMode,
    /// Pin or unpin the highlighted model. Only meaningful while the quick-pick
    /// is open; the subscription cannot see panel state, so the meaning is
    /// decided where the chord is handled.
    PinModel,
    /// Begin a new item on the current surface.
    ///
    /// Settings' "Add a provider" advertises `⌘N`. It was a label with no
    /// binding — §16 requires every action to be reachable by its key, and a
    /// key hint that does nothing is worse than none, because it teaches the
    /// user the keyboard route is broken.
    New,
    /// Open the settings window.
    ///
    /// `design.md` §3's error footer shows `⌘, Settings`, and §8's quality
    /// floor makes the mouse optional — but there was no keyboard route to
    /// Settings from anywhere, so the one window that fixes a misconfiguration
    /// could only be reached through the tray.
    OpenSettings,
    /// Retry the current panel request.
    Retry,
    /// Bring an agent task forward.
    ShowTask,
    /// Cancel an agent task.
    CancelTask,
    /// Recall an older panel instruction.
    HistoryOlder,
    /// Recall a newer panel instruction.
    HistoryNewer,
    /// [`panel::ATTACH_KEY`] — attach the image on the clipboard.
    Attach,
    /// [`panel::DETACH_KEY`] — remove the most recent attachment.
    DetachLast,
    /// Expand or collapse the pinned selected-text card.
    ToggleContext,
    /// Remove the pinned selected text from future turns.
    RemoveSelection,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Which window a given [`window::Id`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Panel,
    Settings,
}

/// The daemon's state.
///
/// Not `Send` and not required to be: it holds the tray (an `NSStatusItem` on
/// macOS) and the hotkey manager, both main-thread-affine, and iced's `daemon`
/// imposes no `Send` bound on `State`.
pub struct Aibo {
    config: UiConfig,
    requests: Sender<UiRequest>,

    /// The pre-created hidden panel window (§6).
    panel_window: window::Id,
    panel: PanelState,
    /// Whether the panel is currently on screen.
    panel_visible: bool,
    /// Whether the panel window holds native keyboard focus.
    ///
    /// Set optimistically on show (the show sequence asks for focus) and
    /// corrected by the window server's `Focused`/`Unfocused` events. The
    /// toggle hotkey reads it to tell "dismiss me" from "bring me back":
    /// pressing the hotkey while another app has the keyboard summons the
    /// visible panel instead of destroying the session it still displays.
    panel_focused: bool,
    /// A summon is capturing from the previous foreground application.
    ///
    /// The panel may be visible during this short window, but it must remain
    /// non-activating so Windows can use its safe Ctrl+C selection fallback.
    /// The matching context event clears the flag and focuses the composer.
    context_capture_pending: bool,
    /// The command modifier is currently down; see [`Message::CommandHeld`].
    command_held: bool,
    /// The in-flight region capture joins the session in progress — the
    /// transcript, composer text and attachments survive it and the crop is
    /// appended. Set by the attach menu, and by the capture hotkey pressed
    /// while the panel is open; only the hotkey over a closed panel keeps the
    /// crop-*then*-ask shape and starts fresh.
    region_capture_keeps_session: bool,
    /// Placement of the last show, so a display change can re-clamp (§9).
    last_placement: Option<Placement>,
    /// The window server's last answer about the panel's monitor (§9).
    ///
    /// Refreshed on every show and on every resize/scale-factor change, never
    /// cached from window creation. It is also the only display information
    /// aibo can obtain without the platform layer, so it is what keeps the
    /// panel off the corner before the first `DisplaysChanged` arrives.
    observed: Option<ObservedGeometry>,
    /// A show is waiting on [`Message::Observed`].
    ///
    /// Set only on the cold path where *nothing* is known about the displays.
    /// §8 wants the panel up immediately, so a show with usable geometry never
    /// waits — but a show with no geometry at all must, because the alternative
    /// is putting the panel somewhere and moving it, and "somewhere" was the
    /// top-left corner.
    pending_show: bool,
    /// The user moved the borderless panel during this showing.
    ///
    /// Content growth must resize a hand-placed panel without snapping it back
    /// to the computed caret/fallback position. Cleared on every new summon.
    panel_dragged: bool,
    /// A corner-grip drag in progress.
    ///
    /// winit 0.30 implements `drag_resize_window` on Windows and answers
    /// `NotSupported` on macOS, so the OS cannot be asked to do this and the
    /// drag is tracked here instead — one implementation, identical on both
    /// platforms. Both backends deliver pointer motion to the window that
    /// received the press even once the cursor leaves it (AppKit routes
    /// dragged events to that window; winit calls `SetCapture` on Win32), so
    /// growing the panel does not lose the pointer.
    panel_resize: Option<ResizeDrag>,

    settings_window: Option<window::Id>,
    settings: SettingsState,
    /// Last issued async file-list generation.
    file_list_generation: u64,
    /// Last issued Files-settings directory-list generation.
    directory_list_generation: u64,
    /// Last issued async workdir-list generation.
    workdir_list_generation: u64,
    /// Current dictation turn id; terminal events from older turns are ignored.
    dictation_turn: u64,
    /// A stop was requested and the final transcript is still draining.
    dictation_stopping: bool,
    /// Monotonic id shared by independently versioned persistence lanes.
    persistence_generation: u64,
    /// Only the newest optimistic mutation in each lane may roll back UI state.
    pending_persistence: HashMap<PersistenceScope, PendingPersistence>,

    /// Open task windows. §6: an agent run outlives the panel and lives here.
    tasks: Vec<(window::Id, TaskState)>,
    /// Runs whose window has not been opened yet.
    tray: Option<Tray>,
    hotkeys: Option<Hotkeys>,
    hotkey_status: Option<HotkeyStatus>,

    displays: Vec<DisplayInfo>,
    /// Set once the first `update` tick has run the shell wiring.
    shell_started: bool,
    /// The backend receives [`UiRequest::UiReady`] once the warm panel exists.
    ui_ready_sent: bool,
    /// Windows with an active uncommitted composition.
    ime_preedit: HashSet<window::Id>,
    /// Bounded native-action sink cloned into each AccessKit adapter.
    accessibility_events: Sender<AccessibilityEvent>,
    /// Surfaces whose native adapters have completed installation.
    accessibility_attached: HashSet<AccessibilitySurface>,
    /// Last semantic focus reported to each surface.
    accessibility_focus: HashMap<AccessibilitySurface, NodeId>,
    /// Last observed logical client size for semantic bounds.
    accessibility_sizes: HashMap<AccessibilitySurface, (f32, f32)>,
    /// Last observed native scale factor for semantic transforms.
    accessibility_scales: HashMap<AccessibilitySurface, f32>,
}

impl std::fmt::Debug for Aibo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Aibo")
            .field("panel_visible", &self.panel_visible)
            .field("tasks", &self.tasks.len())
            .field("hotkey_status", &self.hotkey_status)
            .finish_non_exhaustive()
    }
}

/// Requests that must still have room after ordinary UI input is throttled.
fn is_critical_request(request: &UiRequest) -> bool {
    matches!(
        request,
        UiRequest::Cancel { .. }
            | UiRequest::DiscardSession { .. }
            | UiRequest::Approve { .. }
            | UiRequest::CancelTask { .. }
            | UiRequest::SetLanguage { .. }
            | UiRequest::SetAppearance { .. }
            | UiRequest::SetModel { .. }
            | UiRequest::SetProviderKey { .. }
            | UiRequest::RemoveProvider { .. }
            | UiRequest::SetSttEndOnSend { .. }
            | UiRequest::SetSttBackend { .. }
            | UiRequest::SetPinnedModels { .. }
            | UiRequest::SetHotkey { .. }
            | UiRequest::SetAxTreeActivation { .. }
            | UiRequest::SetFileRoots { .. }
            | UiRequest::SetMonthlyBudget { .. }
            | UiRequest::SetUpdatePreferences { .. }
            | UiRequest::InstallUpdate
            | UiRequest::Quit
    )
}

impl Aibo {
    fn begin_persistence(&mut self, scope: PersistenceScope, rollback: PersistenceSnapshot) -> u64 {
        self.persistence_generation = self.persistence_generation.wrapping_add(1);
        let generation = self.persistence_generation;
        self.pending_persistence.insert(
            scope,
            PendingPersistence {
                generation,
                rollback,
            },
        );
        generation
    }

    /// Write the hand-set panel size, or clear it and return to automatic.
    ///
    /// The rollback restores the size that was in force before this write, so
    /// a config file that cannot be written leaves the panel where the file
    /// says it is rather than where the last gesture put it.
    fn persist_panel_size(&mut self, size: Option<(f32, f32)>) -> Task<Message> {
        let generation = self.begin_persistence(
            PersistenceScope::PanelSize,
            PersistenceSnapshot::PanelSize(self.config.panel_size),
        );
        self.config.panel_size = size;
        self.send(UiRequest::SetPanelSize { generation, size });
        Task::none()
    }

    fn role_of(&self, id: window::Id) -> Option<Role> {
        if id == self.panel_window {
            return Some(Role::Panel);
        }
        if Some(id) == self.settings_window {
            return Some(Role::Settings);
        }
        None
    }

    fn accessibility_surface(role: Role) -> AccessibilitySurface {
        match role {
            Role::Panel => PANEL_ACCESSIBILITY,
            Role::Settings => SETTINGS_ACCESSIBILITY,
        }
    }

    fn role_for_accessibility_surface(&self, surface: AccessibilitySurface) -> Option<Role> {
        if surface == PANEL_ACCESSIBILITY {
            return Some(Role::Panel);
        }
        if surface == SETTINGS_ACCESSIBILITY && self.settings_window.is_some() {
            return Some(Role::Settings);
        }
        None
    }

    fn accessibility_tree(&self, surface: AccessibilitySurface) -> Option<TreeUpdate> {
        let role = self.role_for_accessibility_surface(surface)?;
        let size = self
            .accessibility_sizes
            .get(&surface)
            .copied()
            .unwrap_or_else(|| default_accessibility_size(role, &self.panel));
        let scale = self
            .accessibility_scales
            .get(&surface)
            .copied()
            .unwrap_or(1.0);
        let focus = self
            .accessibility_focus
            .get(&surface)
            .copied()
            .unwrap_or_else(|| accessibility_root(role));

        Some(match role {
            Role::Panel => a11y::panel_tree(&self.panel, size, scale, focus),
            Role::Settings => a11y::settings_tree(&self.settings, size, scale, focus),
        })
    }

    fn sync_accessibility(&self) {
        for surface in &self.accessibility_attached {
            if let Some(tree) = self.accessibility_tree(*surface) {
                aibo_platform::update_accessibility(*surface, tree);
            }
        }
    }

    fn send(&self, request: UiRequest) {
        let critical = is_critical_request(&request);
        if !critical && self.requests.capacity() <= UI_REQUEST_CRITICAL_RESERVE {
            tracing::debug!("runtime queue busy; noncritical request coalesced");
            return;
        }

        match self.requests.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Closed(_)) => {
                // A closed channel means the runtime is already shutting down.
                tracing::warn!("runtime channel closed; request dropped");
            }
            Err(TrySendError::Full(_)) => {
                // Critical requests may consume the reserved tail, but there
                // is deliberately no second, hidden overflow queue. Reaching
                // this branch requires the reserve itself to be saturated.
                tracing::warn!(
                    critical,
                    "runtime request queue saturated; request coalesced"
                );
            }
        }
    }

    /// The same dispatch as [`Aibo::send`], but as a [`Task`] so it can be
    /// *sequenced after* another task rather than racing it.
    ///
    /// `send` fires the instant it is called, which is fine for requests with
    /// no ordering constraint. `Insert` has one — §8 requires the panel to be
    /// hidden first — and only `chain` can express it.
    fn deferred_send(&self, pending: Vec<UiRequest>) -> Task<Message> {
        let requests = self.requests.clone();
        Task::future(async move {
            for request in pending {
                if requests.send(request).await.is_err() {
                    tracing::warn!("runtime channel closed; request dropped");
                    break;
                }
            }
            Message::Ignored
        })
    }

    /// Wire up the OS-level shell. **Runs on the first `update` tick** (§6).
    fn start_shell(&mut self) {
        if self.shell_started {
            return;
        }
        self.shell_started = true;

        // Unit tests drive `update` from the harness's own threads. `tray-icon`
        // needs the main thread and a running event loop, and
        // `GlobalHotKeyManager` installs a process-wide Carbon handler — neither
        // is safe or meaningful here, so the shell is not started under test.
        #[cfg(test)]
        return;

        #[cfg(not(test))]
        self.start_shell_inner();
    }

    fn notify_ui_ready(&mut self) {
        if !self.ui_ready_sent {
            self.send(UiRequest::UiReady);
            self.ui_ready_sent = true;
        }
    }

    /// The real shell wiring. Split out only so [`Aibo::start_shell`] can skip
    /// it under `cfg(test)` without duplicating the guard.
    #[cfg(not(test))]
    fn start_shell_inner(&mut self) {
        // The tray must be created here, inside the running event loop and on
        // the main thread — not in `boot`, where `tray-icon` cannot work (§6).
        match tray::create() {
            Ok(tray) => self.tray = Some(tray),
            // A missing tray is bad but not fatal; the hotkey still works and
            // §6 would rather have a degraded app than no app.
            Err(error) => tracing::error!(%error, "tray icon unavailable"),
        }

        match Hotkeys::new() {
            Ok(mut hotkeys) => {
                let configured = [
                    (HotkeyAction::TogglePanel, self.config.panel_hotkey, true),
                    (
                        HotkeyAction::CaptureScreenRegion,
                        self.config.screen_capture_hotkey,
                        true,
                    ),
                    (
                        HotkeyAction::ShowTasks,
                        self.config.show_tasks_hotkey,
                        false,
                    ),
                ];
                for (action, override_hotkey, use_default) in configured {
                    let binding = override_hotkey
                        .map(|combo| hotkey::Binding::new(action, combo))
                        .or_else(|| {
                            use_default
                                .then(|| hotkey::Binding::default_for(action))
                                .flatten()
                        });
                    let Some(binding) = binding else {
                        continue;
                    };
                    let status = hotkeys.register(binding);
                    match &status {
                        HotkeyStatus::Failed { combo, reason } => {
                            tracing::warn!(?action, %combo, ?reason, "global hotkey unavailable");
                        }
                        HotkeyStatus::Registered {
                            combo,
                            caution: Some(caution),
                        } => {
                            tracing::warn!(?action, %combo, ?caution, "global hotkey registered with a caution");
                        }
                        HotkeyStatus::Registered { combo, .. } => {
                            tracing::info!(?action, %combo, "global hotkey registered");
                        }
                    }
                    self.settings.hotkeys.insert(action, status.clone());
                    if action == HotkeyAction::TogglePanel {
                        self.hotkey_status = Some(status.clone());
                        self.settings.hotkey = Some(status);
                    }
                }
                self.hotkeys = Some(hotkeys);
            }
            Err(error) => {
                tracing::error!(%error, "hotkey manager unavailable");
                let status = HotkeyStatus::Failed {
                    combo: String::new(),
                    reason: hotkey::FailureReason::Unclassified(error.to_string()),
                };
                self.hotkey_status = Some(status.clone());
                // `update` opens settings on this state; without the mirror the
                // window opens showing nothing at all.
                self.settings.hotkey = Some(status);
            }
        }
    }

    /// Show the panel: position, show, focus — and nothing else (§6).
    ///
    /// **The order is load-bearing and therefore a `chain`, not a `batch`.**
    /// `Task::batch` merges its streams with `SelectAll`, which makes no
    /// promise about which action reaches the window server first; `resize` and
    /// `move_to` landing after `set_mode(Windowed)` is a visible jump from
    /// wherever the window happened to be — and while the window happened to be
    /// at the origin, an indistinguishable one from the placement bug itself.
    fn show_panel(&mut self, placement: Placement) -> Task<Message> {
        self.last_placement = Some(placement);
        self.panel_visible = true;
        // A context capture keeps the source app active until its selection is
        // safely read. Every other presentation focuses immediately.
        let focus_now = !self.context_capture_pending;
        self.panel_focused = focus_now;
        self.pending_show = false;
        self.panel_dragged = false;

        let position = Point::new(placement.position.0, placement.position.1);
        let size = Size::new(placement.size.0, placement.size.1);

        let shown = window::resize(self.panel_window, size)
            .chain(sync_backdrop_to_chrome(
                self.panel_window,
                self.panel.chrome_height(),
            ))
            .chain(window::move_to(self.panel_window, position))
            .chain(window::set_mode(self.panel_window, Mode::Windowed))
            // winit rewrites the Win32 style words when changing mode. Reapply
            // WS_EX_TOOLWINDOW after the show so the panel stays out of the
            // taskbar and Alt-Tab, matching AppKit's utility-window policy.
            .chain(configure_or_present_panel(self.panel_window, true))
            .chain(configure_or_present_panel(self.panel_window, false));
        if focus_now {
            shown
                .chain(window::gain_focus(self.panel_window))
                .chain(operation::focus(panel::INPUT_ID))
        } else {
            shown
        }
    }

    /// Hide the panel. Never cancels an agent run (§6).
    fn hide_panel(&mut self) -> Task<Message> {
        // A hidden composer must never leave an invisible microphone running.
        // Keep the turn id so late terminal events cannot affect a later turn.
        if self.panel.dictating && !self.dictation_stopping {
            self.dictation_stopping = true;
            self.send(UiRequest::StopDictation {
                turn: self.dictation_turn,
            });
        }
        self.panel_visible = false;
        self.panel_focused = false;
        self.context_capture_pending = false;
        // A drag cannot survive the panel it was sizing: the release would
        // land on whatever the user is looking at instead.
        self.panel_resize = None;
        // A show that was still waiting on the window server must not land
        // after the user has already dismissed the panel.
        self.pending_show = false;
        window::set_mode(self.panel_window, Mode::Hidden)
    }

    /// Compute the placement for the current state (§9).
    fn placement(&self) -> Placement {
        // SPIKE: S1 — caret/selection bounds come from AX (`kAXBoundsForRange`)
        // on macOS and UIA `BoundingRectangle` on Windows. The anchored path is
        // wired end to end, but until S1 confirms the bounds are obtainable and
        // correct under mixed DPI they arrive as `None` and the panel uses the
        // §9 fallback: the display containing the focused window's centre, 28 %
        // from the top.
        placement::place(&PlacementRequest {
            caret_bounds: self.panel.caret_bounds(),
            // The focused window's bounds are not on the §7 capture surface, so
            // the UI cannot know its centre. `caret_bounds` covers the anchored
            // case and `remembered_display` the returning one; this stays
            // `None` rather than being faked from the panel's own position,
            // which would make the panel choose the display it is already on
            // and never follow the user to another.
            focused_window_centre: None,
            remembered_display: self.last_placement.map(|p| p.display_id),
            displays: self.displays.clone(),
            observed: self.observed,
            // §9's width range, finally given a number: 45 % of the display,
            // clamped. A fixed 680 is a column down the middle of a 5K screen
            // and wider than the window it describes on a small one.
            preferred_width: Some(self.panel.user_size.map_or_else(
                || ui_theme::panel_width_for(self.panel.display_width),
                |(width, _)| width,
            )),
            content_height: self.panel.desired_height(),
            user_sized: self.panel.user_size.is_some(),
        })
    }

    /// Whether anything at all is known about where the displays are.
    ///
    /// False only before the first `DisplaysChanged` *and* the first answer
    /// from the window server — in practice, before the panel window has
    /// finished opening.
    fn geometry_is_known(&self) -> bool {
        !self.displays.is_empty() || self.observed.is_some()
    }

    /// Begin a new panel invocation.
    ///
    /// §13: pressing the hotkey while a Complete is streaming cancels the
    /// in-flight request and discards the old session; pressing it during an
    /// **agent run** does not interrupt — the run continues in its task window
    /// and a fresh panel opens.
    fn begin_panel_session(&mut self) {
        self.begin_panel_session_for(None);
    }

    /// Begin a panel invocation while retaining a focus snapshot captured
    /// before the panel activated.
    fn begin_panel_session_for(&mut self, app_ref: Option<AppRef>) {
        let old_session = self.panel.session;
        if matches!(self.panel.phase, Phase::Loading | Phase::Streaming) {
            self.send(UiRequest::Cancel {
                session: old_session,
            });
        }
        self.send(UiRequest::DiscardSession {
            session: old_session,
        });

        let session: SessionId = Uuid::now_v7();
        self.panel.reset(session);
        self.panel.phase = Phase::Idle;
        self.send(UiRequest::CaptureContext { session, app_ref });
    }

    fn present_panel(&mut self) -> Task<Message> {
        if self.geometry_is_known() {
            // §8: show immediately. The cached geometry is a frame old at
            // worst, and §9's re-probe below corrects it if the window server
            // disagrees — that is cheaper than making every hotkey press wait
            // for a round trip it will almost always agree with.
            let placement = self.placement();
            self.show_panel(placement)
                .chain(probe_geometry(self.panel_window))
        } else {
            // Nothing is known: resolve the placement *first*. Showing here and
            // correcting afterwards is exactly the bug — the panel appears in
            // the corner and then jumps.
            self.pending_show = true;
            probe_geometry(self.panel_window)
        }
    }

    /// Reopen the panel on the conversation it was last showing.
    ///
    /// The hotkey used to discard the session every time, so dismissing the
    /// panel to look something up lost the thread — the one thing a person
    /// reliably wants back. Continuing is now the default and the session id is
    /// kept, which is what makes it work: the backend holds the history against
    /// that id, so reusing it *is* the continuation.
    ///
    /// A fresh start still happens whenever new context arrives, because that
    /// is a different question rather than a follow-up: a selection (see the
    /// `UiEvent::Context` arm) or a screen capture. `⌘N` forces one on demand.
    fn resume_panel_session_for(&mut self, app_ref: Option<AppRef>) {
        // §13 unchanged: reopening mid-stream cancels the request. What is
        // dropped is the in-flight answer, not the conversation above it.
        if matches!(self.panel.phase, Phase::Loading | Phase::Streaming) {
            self.send(UiRequest::Cancel {
                session: self.panel.session,
            });
        }
        self.panel.phase = Phase::Idle;
        let session = self.panel.session;
        self.send(UiRequest::CaptureContext { session, app_ref });
    }

    fn open_panel(&mut self) -> Task<Message> {
        self.refresh_appearance();
        // `present_panel` ultimately asks the OS for keyboard focus. Snapshot
        // its current owner first so Windows UIA and the safe Ctrl+C fallback
        // still target the application where the summon gesture occurred.
        // Windows' last-resort selection read is an ordinary Ctrl+C and can
        // only target the foreground app. macOS AX reads are addressable and
        // do not need to delay activation.
        self.context_capture_pending = cfg!(target_os = "windows");
        self.resume_panel_session_for(aibo_platform::focused_app_ref());
        self.present_panel()
    }

    /// Re-resolve "system" against the OS's current answer.
    ///
    /// Called on every panel and settings open rather than subscribed to a
    /// notification: appearance changes are rare, the read is one main-thread
    /// AppKit call, and the next open is exactly when a stale palette would
    /// first be seen.
    fn refresh_appearance(&mut self) {
        self.config.appearance = self
            .config
            .appearance_preference
            .resolve(aibo_platform::system_prefers_dark());
    }

    fn refresh_tray(&mut self) {
        let Some(tray) = self.tray.as_mut() else {
            return;
        };
        let state = if self.panel.tasks.iter().any(TaskState::is_blocked) {
            TrayState::Attention
        } else if self.panel.tasks.iter().any(TaskState::is_running) {
            TrayState::Busy
        } else {
            TrayState::Idle
        };
        tray.set_state(state);
    }
}

fn accessibility_root(role: Role) -> NodeId {
    match role {
        Role::Panel => a11y::PANEL_ROOT,
        Role::Settings => a11y::SETTINGS_ROOT,
    }
}

fn default_accessibility_size(role: Role, panel: &PanelState) -> (f32, f32) {
    match role {
        Role::Panel => (ui_theme::PANEL_WIDTH_DEFAULT, panel.desired_height()),
        Role::Settings => (880.0, 520.0),
    }
}

// ---------------------------------------------------------------------------
// Window settings
// ---------------------------------------------------------------------------

/// Settings for the pre-created hidden panel window (§6, §9).
fn panel_window_settings() -> window::Settings {
    window::Settings {
        size: Size::new(
            ui_theme::PANEL_WIDTH_DEFAULT,
            ui_theme::PANEL_HEIGHT_COLLAPSED,
        ),
        // §6, the cold-start trick: created hidden so its wgpu surface exists
        // and its first frame is compiled before the first hotkey.
        visible: false,
        decorations: false,
        transparent: true,
        resizable: false,
        minimizable: false,
        closeable: false,
        level: window::Level::AlwaysOnTop,
        // The panel must never take over the app's lifetime; §6 keeps it alive
        // hidden rather than destroying and recreating it.
        exit_on_close_request: false,
        min_size: Some(Size::new(
            ui_theme::PANEL_WIDTH_MIN,
            ui_theme::PANEL_HEIGHT_COLLAPSED,
        )),
        max_size: Some(Size::new(
            ui_theme::PANEL_WIDTH_MAX,
            ui_theme::PANEL_HEIGHT_MAX,
        )),
        platform_specific: panel_platform_settings(),
        ..Default::default()
    }
}

#[cfg(target_os = "macos")]
fn panel_platform_settings() -> window::settings::PlatformSpecific {
    // All-Spaces, fullscreen-auxiliary, non-activating presentation and HUD
    // vibrancy are applied after `WindowOpened` through aibo-platform's native
    // handle boundary. Nothing macOS-specific remains for iced to express.
    window::settings::PlatformSpecific::default()
}

#[cfg(not(target_os = "macos"))]
fn panel_platform_settings() -> window::settings::PlatformSpecific {
    #[cfg(target_os = "windows")]
    {
        window::settings::PlatformSpecific {
            // A hotkey overlay has no business in the taskbar or Alt-Tab.
            skip_taskbar: true,
            // Acrylic and non-activating presentation are applied after
            // `WindowOpened` through aibo-platform. Per-Monitor-V2 DPI
            // awareness is configured before the first window is created.
            ..Default::default()
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        window::settings::PlatformSpecific::default()
    }
}

/// Settings for the settings window.
fn settings_window_settings() -> window::Settings {
    window::Settings {
        // Tall enough for the whole section list at the 44 pt accessibility
        // floor: eleven sections plus the sidebar's padding need 516 pt, and
        // 520 did not clear the title bar — About sat under the window edge
        // with no way to scroll to it. The list scrolls now as well, so this
        // is comfort rather than the fix.
        size: Size::new(880.0, 620.0),
        min_size: Some(Size::new(640.0, 420.0)),
        // Keep the first frame private until the native semantic tree is
        // attached; see the task-window setting above.
        visible: false,
        exit_on_close_request: true,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// boot / update / view / subscription
// ---------------------------------------------------------------------------

fn boot(config: UiConfig, requests: Sender<UiRequest>) -> (Aibo, Task<Message>) {
    i18n::set_language(config.language);

    let (accessibility_events, accessibility_receiver) =
        tokio::sync::mpsc::channel(ACCESSIBILITY_EVENT_CHANNEL_CAPACITY);
    if let Ok(mut receiver) = ACCESSIBILITY_EVENTS.lock() {
        *receiver = Some(accessibility_receiver);
    }

    // §6: pre-create the panel hidden. This is the only window opened at boot;
    // the daemon otherwise runs with none.
    let (panel_window, opened) = window::open(panel_window_settings());

    let mut panel = PanelState::new(Uuid::now_v7());
    // A size the user dragged to in an earlier run. Clamping to the display
    // happens in `placement`, which knows the display this show lands on; a
    // saved size from a monitor that is no longer attached therefore comes
    // back as the nearest size that fits rather than as an off-screen panel.
    panel.user_size = config.panel_size;

    let state = Aibo {
        config,
        requests,
        panel_window,
        panel,
        panel_visible: false,
        panel_focused: false,
        context_capture_pending: false,
        command_held: false,
        region_capture_keeps_session: false,
        last_placement: None,
        observed: None,
        pending_show: false,
        panel_dragged: false,
        panel_resize: None,
        settings_window: None,
        settings: SettingsState::default(),
        file_list_generation: 0,
        directory_list_generation: 0,
        workdir_list_generation: 0,
        dictation_turn: 0,
        dictation_stopping: false,
        persistence_generation: 0,
        pending_persistence: HashMap::new(),
        tasks: Vec::new(),
        tray: None,
        hotkeys: None,
        hotkey_status: None,
        displays: Vec::new(),
        shell_started: false,
        ui_ready_sent: false,
        ime_preedit: HashSet::new(),
        accessibility_events,
        accessibility_attached: HashSet::new(),
        accessibility_focus: HashMap::from([(PANEL_ACCESSIBILITY, a11y::PANEL_ROOT)]),
        accessibility_sizes: HashMap::from([(
            PANEL_ACCESSIBILITY,
            (
                ui_theme::PANEL_WIDTH_DEFAULT,
                ui_theme::PANEL_HEIGHT_COLLAPSED,
            ),
        )]),
        accessibility_scales: HashMap::from([(PANEL_ACCESSIBILITY, 1.0)]),
    };
    let mut state = state;
    // The selector shows the *preference* (System/Dark/Light), not its
    // current resolution — the config carries both.
    state.settings.appearance = state.config.appearance_preference;
    // True until `SettingsLoaded` says otherwise: ending dictation on ⏎ is
    // the default, and the brief pre-hydration window must agree with it.
    state.settings.stt_end_on_send = true;
    state.settings.auto_update = true;

    (
        state,
        Task::batch([
            opened.map(Message::WindowOpened),
            // Guarantees a first `update` tick even if nothing else happens,
            // which is where the tray gets built (§6).
            Task::done(Message::Ready),
        ]),
    )
}

fn update(state: &mut Aibo, message: Message) -> Task<Message> {
    // The tray cannot be created in `boot`; this is the first moment inside the
    // running event loop, on the main thread (§6).
    let first_tick = !state.shell_started;
    state.start_shell();

    // §9 wants conflict detection at first run, and a refused shortcut is the
    // one startup problem the user cannot discover on their own: the app looks
    // installed and does nothing. Logging it is not enough, so settings opens
    // once, on the tick that discovered it, showing which combination failed.
    let shell_task =
        if first_tick && matches!(state.hotkey_status, Some(HotkeyStatus::Failed { .. })) {
            // Permissions, not Actions: the hotkey block lives in the
            // permissions section (§9 "this is where a user who has lost
            // ⌥Space to Raycast will come looking"). Opening on Actions
            // showed an empty state and no explanation.
            state.settings.section = settings::Section::Permissions;
            open_settings(state)
        } else {
            Task::none()
        };

    let task = match message {
        Message::Ready | Message::Ignored => Task::none(),

        Message::PanelFrameFallback(placement) => {
            let size = Size::new(placement.size.0, placement.size.1);
            let position = Point::new(placement.position.0, placement.position.1);
            window::resize(state.panel_window, size)
                .chain(window::move_to(state.panel_window, position))
        }

        Message::PanelResizeMoved(position) => resize_drag_moved(state, position),

        Message::PanelResizeEnded => resize_drag_ended(state),

        Message::WindowOpened(id) => {
            let Some(role) = state.role_of(id) else {
                return Task::batch([shell_task, Task::none()]);
            };
            let surface = Aibo::accessibility_surface(role);
            state
                .accessibility_focus
                .entry(surface)
                .or_insert_with(|| accessibility_root(role));
            state
                .accessibility_sizes
                .entry(surface)
                .or_insert_with(|| default_accessibility_size(role, &state.panel));
            state.accessibility_scales.entry(surface).or_insert(1.0);
            tracing::debug!(?role, ?surface, "native window opened");

            if role == Role::Panel {
                // The hidden window is now real: paint the throwaway frames…
                state.panel.phase = Phase::WarmingUp { frames_left: 2 };
                state.notify_ui_ready();
            }
            // Every aibo surface is born hidden. Install the native semantic
            // adapter first; `AccessibilityAttached` performs the role-specific
            // reveal/configuration only after this task completes.
            attach_accessibility_window(state, id, role)
        }

        Message::WindowClosed(id) => {
            state.ime_preedit.remove(&id);
            let role = state.role_of(id);
            if let Some(role) = role {
                let surface = Aibo::accessibility_surface(role);
                aibo_platform::detach_accessibility(surface);
                state.accessibility_attached.remove(&surface);
                state.accessibility_focus.remove(&surface);
                state.accessibility_sizes.remove(&surface);
                state.accessibility_scales.remove(&surface);
            }
            match role {
                // §6: the panel is never destroyed, only hidden. If the OS
                // closed it anyway, the warm surface is gone and the next show
                // will be slow — worth knowing about.
                Some(Role::Panel) => tracing::warn!("the panel window was closed"),
                Some(Role::Settings) => {
                    state.settings_window = None;
                    state.settings.hotkey_recording = false;
                    // A newly generated history recovery code is a one-time
                    // setup disclosure, not a value retained for later visits.
                    state.settings.recovery_code = None;
                }
                None => {}
            }
            Task::none()
        }

        Message::AccessibilityAttached {
            window,
            surface,
            result,
        } => {
            if result.is_ok() {
                state.accessibility_attached.insert(surface);
                tracing::debug!(?surface, "native accessibility adapter attached");
            } else if let Err(error) = result {
                tracing::warn!(%error, ?surface, "native accessibility adapter unavailable");
            }

            let Some(role) = state.role_of(window) else {
                if state.accessibility_attached.remove(&surface) {
                    aibo_platform::detach_accessibility(surface);
                }
                return Task::batch([shell_task, Task::none()]);
            };
            if Aibo::accessibility_surface(role) != surface {
                tracing::warn!(
                    ?surface,
                    "accessibility surface no longer matches its window"
                );
                aibo_platform::detach_accessibility(surface);
                state.accessibility_attached.remove(&surface);
                Task::none()
            } else {
                // Resolve DPI while the window is still hidden. The next
                // message stores it and synchronously refreshes the tree before
                // any reveal action reaches the window server.
                window::scale_factor(window).map(move |scale| Message::AccessibilityReadyToReveal {
                    window,
                    surface,
                    scale,
                })
            }
        }

        Message::AccessibilityReadyToReveal {
            window,
            surface,
            scale,
        } => {
            let Some(role) = state.role_of(window) else {
                if state.accessibility_attached.remove(&surface) {
                    aibo_platform::detach_accessibility(surface);
                }
                return Task::batch([shell_task, Task::none()]);
            };
            if Aibo::accessibility_surface(role) != surface {
                aibo_platform::detach_accessibility(surface);
                state.accessibility_attached.remove(&surface);
                Task::none()
            } else {
                state.accessibility_scales.insert(surface, scale);
                match role {
                    Role::Panel => {
                        configure_or_present_panel(window, true).chain(probe_geometry(window))
                    }
                    Role::Settings => {
                        window::set_mode(window, Mode::Windowed).chain(window::gain_focus(window))
                    }
                }
            }
        }

        Message::AccessibilityResize(id, size) => {
            if let Some(role) = state.role_of(id) {
                state
                    .accessibility_sizes
                    .insert(Aibo::accessibility_surface(role), (size.width, size.height));
                if role == Role::Panel {
                    probe_geometry(state.panel_window)
                } else {
                    Task::none()
                }
            } else {
                Task::none()
            }
        }

        Message::AccessibilityScale(id, scale) => {
            if let Some(role) = state.role_of(id) {
                state
                    .accessibility_scales
                    .insert(Aibo::accessibility_surface(role), scale);
                if role == Role::Panel {
                    probe_geometry(state.panel_window)
                } else {
                    Task::none()
                }
            } else {
                Task::none()
            }
        }

        Message::AccessibilityFocus(id, focused) => {
            if let Some(role) = state.role_of(id) {
                aibo_platform::set_accessibility_focus(Aibo::accessibility_surface(role), focused);
            }
            if id == state.panel_window {
                state.panel_focused = focused;
            }
            // Focusing the panel window is intent to type (owner, 2026-08-01:
            // "when the focus is on aibo, it should immediately be able to
            // start typing" — clicking the panel or ⌘-tabbing back left the
            // caret nowhere until the text box was clicked). The composer
            // takes the caret unless a menu's query field owns it.
            if focused
                && id == state.panel_window
                && state.panel_visible
                && !state.panel.picker.open
                && !state.panel.file_finder.open
            {
                return operation::focus(panel::INPUT_ID);
            }
            Task::none()
        }

        Message::Accessibility(event) => {
            // Native clients may hold a node reference across a repaint. Only
            // honor actions whose target is still present in this snapshot;
            // this prevents a stale permission or approval control from
            // activating after its UI has disappeared.
            let target_exists = state.accessibility_tree(event.surface).is_some_and(|tree| {
                tree.nodes
                    .iter()
                    .any(|(node_id, _)| *node_id == event.request.target_node)
            });
            if !target_exists {
                return Task::batch([shell_task, Task::none()]);
            }
            if let Some(focus) = a11y::requested_focus(&event.request) {
                state.accessibility_focus.insert(event.surface, focus);
                Task::none()
            } else {
                match state.role_for_accessibility_surface(event.surface) {
                    Some(Role::Panel) => a11y::panel_message(&state.panel, &event.request)
                        .map_or_else(Task::none, |message| panel_update(state, message)),
                    Some(Role::Settings) => a11y::settings_message(&state.settings, &event.request)
                        .map_or_else(Task::none, |message| settings_update(state, message)),
                    None => Task::none(),
                }
            }
        }

        Message::FramePainted => {
            if let Phase::WarmingUp { frames_left } = state.panel.phase {
                state.panel.phase = match frames_left.saturating_sub(1) {
                    0 => Phase::Hidden,
                    left => Phase::WarmingUp { frames_left: left },
                };
            }
            Task::none()
        }

        Message::Hotkey(id) => {
            let action = state
                .hotkeys
                .as_ref()
                .and_then(|hotkeys| hotkeys.action_for(id));
            match action {
                Some(HotkeyAction::TogglePanel) => {
                    if state.panel_visible && !state.panel_focused {
                        // The panel is on screen but another app holds the
                        // keyboard. From there the hotkey means "get me back
                        // to aibo" — hiding would destroy a session the user
                        // can still see, which is an implicit destructive
                        // gesture. Dismissal stays one press away: the next
                        // toggle finds the panel focused and hides it.
                        //
                        // The full `open_panel`, not a bare focus: the press
                        // carries the other app's selection with it (§7), and
                        // only `resume_panel_session`'s CaptureContext reads
                        // it. A bare `gain_focus` summoned a panel that had
                        // not looked at what the user was pointing at.
                        state.open_panel()
                    } else if state.panel_visible {
                        discard_panel_session(state, true);
                        state.hide_panel()
                    } else {
                        // The final onboarding step is an actual hotkey
                        // invocation. Provider health alone used to dismiss the
                        // guide before the user ever learned how to reopen the
                        // panel.
                        state.settings.onboarding = false;
                        state.open_panel()
                    }
                }
                Some(HotkeyAction::CaptureScreenRegion) => {
                    // Keep the panel out of the pixels. `chain` begins the picker
                    // only after the window server has hidden the overlay.
                    let hidden = if state.panel_visible {
                        // An open panel means a conversation in progress, and
                        // the hotkey joins it exactly like the attach menu's
                        // screenshot row: composer text, transcript and
                        // attachments survive, and the crop lands beside them.
                        // Discarding here threw away what the user had typed —
                        // an implicit destructive gesture. Crop-*then*-ask
                        // starts fresh only from a closed panel.
                        state.region_capture_keeps_session = true;
                        state.hide_panel()
                    } else {
                        Task::none()
                    };
                    hidden.chain(capture_screen_region_task())
                }
                Some(HotkeyAction::ShowTasks) => focus_first_task(state),
                // TODO(§13): the revert buffer holds the pre-transform original
                // for the session and is owned by the runtime, not the UI.
                Some(HotkeyAction::RevertLastTransform) | None => Task::none(),
            }
        }

        Message::ScreenRegionCaptured(result) => match result {
            Ok(None) => {
                // A cancelled crop from the attach menu must bring the
                // conversation back; the hotkey path had nothing to return to.
                if std::mem::take(&mut state.region_capture_keeps_session) {
                    aibo_platform::activate_self();
                    return state.present_panel();
                }
                Task::none()
            }
            Ok(Some(mut attachment)) => {
                attachment.label = i18n::t(crate::i18n::Key::AttachmentScreenRegion).to_owned();
                if !std::mem::take(&mut state.region_capture_keeps_session) {
                    state.begin_panel_session();
                }
                if let Err(error) = state.panel.attach(attachment) {
                    state.panel.fail(&std::sync::Arc::new(error));
                }
                // `/usr/sbin/screencapture` is a separate application. When it
                // exits, macOS re-activates whatever was frontmost before it,
                // and that lands *after* `present_panel` asks for focus — so
                // the panel appeared on top and could not take a keystroke.
                // Claiming activation explicitly is the only thing that beats
                // the system's own restore.
                aibo_platform::activate_self();
                state.present_panel()
            }
            Err(error) => {
                tracing::warn!(%error, "screen-region capture failed");
                if !std::mem::take(&mut state.region_capture_keeps_session) {
                    state.begin_panel_session();
                }
                state.panel.toast = Some(panel::ToastView {
                    severity: ui_theme::Severity::Warning,
                    body: i18n::t(crate::i18n::Key::ToastScreenCaptureFailed).to_owned(),
                    offer_diagnostics: false,
                });
                aibo_platform::activate_self();
                state.present_panel()
            }
        },

        Message::Tray(command) => match command {
            TrayCommand::OpenPanel => state.open_panel(),
            TrayCommand::ShowTasks => focus_first_task(state),
            TrayCommand::OpenSettings => open_settings(state),
            TrayCommand::Quit => {
                // §6: child processes must not outlive aibo. The runtime reaps
                // them; the UI only asks and then exits.
                state.send(UiRequest::Quit);
                iced::exit()
            }
        },

        Message::Place(placement) => state.show_panel(placement),

        Message::ProbeGeometry => probe_geometry(state.panel_window),

        Message::Observed { monitor, scale } => {
            state.observed = Some(ObservedGeometry {
                monitor_size: monitor.map(|s| (f64::from(s.width), f64::from(s.height))),
                scale_factor: f64::from(scale),
            });
            // §4 sizes the answer area as a fraction of the display, so the
            // panel needs the display's height in the same logical points its
            // own layout is measured in — which is what `monitor_size` already
            // is. iced converts before handing it over (`iced_winit`'s
            // `GetMonitorSize` does `monitor.size().to_logical(scale)`), so this
            // used to divide by the scale factor a *second* time: a 1920×1080
            // display at 150 % reported 853×480 instead of 1280×720. Height
            // survived by luck, because `max_panel_height`'s floor absorbed the
            // error; width did not, and every Retina Mac asked for the 420 pt
            // minimum where 680 was intended.
            state.panel.display_height = monitor.map(|s| s.height);
            state.panel.display_width = monitor.map(|s| s.width);

            let pending = std::mem::take(&mut state.pending_show);
            if pending {
                let placement = state.placement();
                return Task::batch([shell_task, update(state, Message::Place(placement))]);
            }
            if !state.panel_visible {
                return Task::batch([shell_task, Task::none()]);
            }
            // §9's "re-layout on scale-factor and size changes" — but only
            // when something actually changed. `show_panel` issues a resize,
            // which produces another resize event, which probes again; without
            // this guard that is a loop, not a correction.
            let fresh = state.placement();
            let settled = state.last_placement.is_some_and(|previous| {
                previous.size == fresh.size && previous.scale_factor == fresh.scale_factor
            });
            if settled {
                return Task::batch([shell_task, Task::none()]);
            }
            // **A visible panel follows the new geometry in size, never in
            // position.** This used to re-`Place` it, which recomputes the
            // §9 position from scratch — and since a resize is what triggers
            // this probe, dragging the corner grip re-centred the panel on
            // every pointer motion (owner report, 2026-08-05). Content growth
            // hid the bug: it changes only the height, and the fallback's
            // 28 %-from-the-top is a constant, so the recomputed position came
            // back identical. Changing the *width* is what made it visible.
            //
            // `Place` is also the wrong instrument for a second reason: it
            // runs `show_panel`, which re-asserts focus on the composer. Focus
            // must not move on a resize, or a mid-answer selection is yanked.
            resize_panel_if_visible(state)
        }

        Message::Displays(displays) => {
            state.displays = displays;
            // §9: if the remembered display is gone it must not steer the next
            // show — dropping it here is what makes `place` fall back to the
            // primary instead of to a display id nothing matches.
            if let Some(previous) = state.last_placement
                && !state.displays.iter().any(|d| d.id == previous.display_id)
            {
                state.last_placement = None;
            }
            // §9: on disconnect or resolution change, re-clamp.
            if state.panel_visible {
                let placement = state.placement();
                update(state, Message::Place(placement))
            } else {
                Task::none()
            }
        }

        Message::WindowKey(id, chord) => window_shortcut(state, id, chord),

        Message::ImePreedit(id, active) => {
            if active {
                state.ime_preedit.insert(id);
            } else {
                state.ime_preedit.remove(&id);
            }
            Task::none()
        }

        Message::CommandHeld(held) => {
            state.command_held = held;
            Task::none()
        }

        Message::Panel(message) => panel_update(state, message),
        Message::Settings(message) => settings_update(state, message),
        Message::Backend(event) => backend_update(state, *event),
    };

    state.sync_accessibility();
    Task::batch([shell_task, task])
}

/// Ask the window server for the panel's monitor size and scale factor (§9).
///
/// These are the only two facts about the displays iced 0.14 exposes — there is
/// no monitor enumeration, so origins and multi-display topology still have to
/// come from `aibo-platform` via [`UiEvent::DisplaysChanged`]. What they *do*
/// give is a fresh scale factor on every show and a real frame to centre in
/// before the platform layer has reported anything, which is the difference
/// between a centred panel and one in the corner.
fn probe_geometry(window: window::Id) -> Task<Message> {
    window::monitor_size(window).then(move |monitor| {
        window::scale_factor(window).map(move |scale| Message::Observed { monitor, scale })
    })
}

fn attach_accessibility_window(state: &Aibo, id: window::Id, role: Role) -> Task<Message> {
    let surface = Aibo::accessibility_surface(role);
    let Some(tree) = state.accessibility_tree(surface) else {
        return Task::done(Message::AccessibilityAttached {
            window: id,
            surface,
            result: Err("semantic tree was unavailable".to_owned()),
        });
    };
    let events = state.accessibility_events.clone();

    window::run(id, move |window| {
        let result = window
            .window_handle()
            .map_err(|error| error.to_string())
            .and_then(|handle| {
                aibo_platform::attach_accessibility(handle, surface, tree, events)
                    .map_err(|error| error.to_string())
            });
        Message::AccessibilityAttached {
            window: id,
            surface,
            result,
        }
    })
}

/// Bring the tasks overlay up, on the blocked run first (owner redesign,
/// 2026-08-02: no task windows — the overlay is how any run is reached).
fn focus_first_task(state: &mut Aibo) -> Task<Message> {
    let task = state
        .panel
        .tasks
        .iter()
        .find(|task| task.is_blocked())
        .or_else(|| state.panel.tasks.first())
        .map(|task| task.id);
    state.panel.selected_task = task;
    state.panel.tasks_open = task.is_some();
    if state.panel_visible {
        resize_panel_if_visible(state)
    } else {
        // The overlay needs the panel on screen: the hotkey's own show path,
        // with the overlay pre-armed above.
        state.open_panel()
    }
}

fn open_settings(state: &mut Aibo) -> Task<Message> {
    state.refresh_appearance();
    // Ask what the OS currently thinks. Until this existed, the permission
    // rows had no producer at all: `UiEvent::PermissionChanged` was only ever
    // going to arrive from a capture that had already failed, so a user whose
    // permissions were fine saw an empty space where their status should be,
    // and a user whose permissions were broken had to break something again to
    // find out. TCC state also changes behind the app's back — an OS update
    // resets it — so this is asked on every open, not cached from startup.
    state.send(UiRequest::RefreshPermissions);
    // The panel floats at `Level::AlwaysOnTop`; a normal-level settings window
    // can only ever open *behind* it. Opening settings is a statement about
    // where the user is going next, so the panel steps aside. Nothing is lost:
    // `hide_panel` never cancels a run (§6) and the hotkey resumes the
    // conversation.
    let step_aside = if state.panel_visible {
        state.hide_panel()
    } else {
        Task::none()
    };
    if let Some(id) = state.settings_window {
        return step_aside.chain(window::gain_focus(id));
    }
    let (id, opened) = window::open(settings_window_settings());
    state.settings_window = Some(id);
    step_aside.chain(opened.map(Message::WindowOpened))
}

/// Whether `new` is exactly `old` with one extra character inserted.
///
/// The shape of iced's ⌘-shortcut fallout: the shortcut's own letter, inserted
/// at the cursor. Anything else — deletion, paste, IME commit of a word —
/// passes through untouched.
fn is_single_char_insertion(old: &str, new: &str) -> bool {
    if new.chars().count() != old.chars().count() + 1 {
        return false;
    }
    let prefix = old
        .char_indices()
        .zip(new.char_indices())
        .take_while(|((_, a), (_, b))| a == b)
        .last()
        .map_or(0, |((at, ch), _)| at + ch.len_utf8());
    let Some(inserted) = new[prefix..].chars().next() else {
        return false;
    };
    new[prefix + inserted.len_utf8()..] == old[prefix..]
}

/// Wind down a live microphone before the panel state that owns it moves on.
fn stop_dictation_if_active(state: &mut Aibo) {
    if state.panel.dictating && !state.dictation_stopping {
        state.dictation_stopping = true;
        state.send(UiRequest::StopDictation {
            turn: state.dictation_turn,
        });
    }
}

fn request_file_list(state: &mut Aibo) {
    state.file_list_generation = state.file_list_generation.wrapping_add(1);
    state.send(UiRequest::ListFiles {
        session: state.panel.session,
        generation: state.file_list_generation,
    });
}

fn request_directory_list(state: &mut Aibo, parent: String) {
    state.directory_list_generation = state.directory_list_generation.wrapping_add(1);
    state.send(UiRequest::ListDirectories {
        generation: state.directory_list_generation,
        parent,
    });
}

fn request_workdir_list(state: &mut Aibo) {
    state.workdir_list_generation = state.workdir_list_generation.wrapping_add(1);
    state.send(UiRequest::ListWorkdirs {
        generation: state.workdir_list_generation,
    });
}

fn discard_panel_session(state: &Aibo, cancel: bool) {
    let session = state.panel.session;
    if cancel {
        state.send(UiRequest::Cancel { session });
    }
    state.send(UiRequest::DiscardSession { session });
}

fn window_shortcut(state: &mut Aibo, window: window::Id, chord: WindowChord) -> Task<Message> {
    if state.ime_preedit.contains(&window) && matches!(chord, WindowChord::Enter { .. }) {
        return Task::none();
    }

    // Available from every window, and from the panel in particular: §17's
    // recovery from "no provider configured" is *open settings*, and until now
    // that had no key.
    if matches!(chord, WindowChord::OpenSettings) {
        return open_settings(state);
    }

    // The slash popup and the help overlay intercept only their own keys —
    // unlike the finder and quick-pick they hold no text field, the composer
    // keeps focus, and every other chord must keep meaning what it means.
    if matches!(state.role_of(window), Some(Role::Panel)) && state.panel_visible {
        if state.panel.slash.open {
            match chord {
                WindowChord::Escape => return panel_update(state, panel::Message::SlashClose),
                WindowChord::Enter {
                    command: false,
                    shift: false,
                } => return panel_update(state, panel::Message::SlashAccept),
                WindowChord::NextLane => return panel_update(state, panel::Message::SlashAccept),
                WindowChord::HistoryOlder => {
                    return panel_update(state, panel::Message::SlashMove(-1));
                }
                WindowChord::HistoryNewer => {
                    return panel_update(state, panel::Message::SlashMove(1));
                }
                _ => {}
            }
        }
        if state.panel.help_open && matches!(chord, WindowChord::Escape) {
            return panel_update(state, panel::Message::HelpClose);
        }
        if state.panel.attach_menu_open && matches!(chord, WindowChord::Escape) {
            return panel_update(state, panel::Message::AttachMenuClose);
        }
        if state.panel.skills_open && matches!(chord, WindowChord::Escape) {
            return panel_update(state, panel::Message::SkillsClose);
        }
    }

    match state.role_of(window) {
        Some(Role::Panel) if state.panel_visible => {
            let message = match chord {
                // The workdir picker owns the keyboard while it is open,
                // exactly like the finder and quick-pick below.
                _ if state.panel.workdir_picker.open => match chord {
                    WindowChord::Escape => panel::Message::WorkdirClose,
                    WindowChord::Enter { .. } => panel::Message::WorkdirCommit,
                    WindowChord::HistoryOlder => panel::Message::WorkdirMove(-1),
                    WindowChord::HistoryNewer => panel::Message::WorkdirMove(1),
                    _ => return Task::none(),
                },
                // The `@` finder owns the keyboard while it is open, exactly
                // like the quick-pick below.
                _ if state.panel.file_finder.open => match chord {
                    WindowChord::Escape => panel::Message::FinderClose,
                    WindowChord::Enter { .. } => panel::Message::FinderCommit,
                    WindowChord::HistoryOlder => panel::Message::FinderMove(-1),
                    WindowChord::HistoryNewer => panel::Message::FinderMove(1),
                    _ => return Task::none(),
                },
                // The quick-pick owns the keyboard while it is open, so its
                // keys are matched before the panel's own. Otherwise ↑/↓ would
                // recall history instead of moving the highlight, and ⏎ would
                // submit an empty instruction.
                _ if state.panel.picker.open => match chord {
                    WindowChord::Escape => panel::Message::ClosePicker,
                    WindowChord::Enter { .. } => panel::Message::PickerCommit,
                    WindowChord::HistoryOlder => panel::Message::PickerMove(-1),
                    WindowChord::HistoryNewer => panel::Message::PickerMove(1),
                    WindowChord::PinModel => panel::Message::PickerToggleFavourite,
                    WindowChord::NextLane => panel::Message::PickerCycleLane,
                    _ => return Task::none(),
                },
                WindowChord::PickModel => panel::Message::OpenPicker,
                // ⇥ opens the quick-pick. The empty state advertises "⇥ for
                // models" and the picker's own placeholder promises "⇥ to
                // browse", so a Tab that did nothing made the panel's first
                // suggestion to a new user a lie.
                WindowChord::NextLane => panel::Message::OpenPicker,
                // Outside the picker ⌘D means nothing, and inventing a meaning
                // for it would make the binding unpredictable.
                WindowChord::PinModel => return Task::none(),
                // Handled above, for every window.
                WindowChord::OpenSettings => unreachable!("intercepted before the role match"),
                // The fresh start `resume_panel_session`'s doc promises.
                WindowChord::New => panel::Message::NewChat,
                WindowChord::Dictate => panel::Message::ToggleDictation,
                WindowChord::AgentMode => panel::Message::ToggleAgentMode,
                WindowChord::Escape if state.panel.toast.is_some() => panel::Message::DismissToast,
                WindowChord::Escape => panel::Message::Dismiss,
                WindowChord::Enter {
                    command: true,
                    shift: true,
                } => panel::Message::Escalate,
                WindowChord::Enter {
                    command: true,
                    shift: false,
                } => {
                    // `design.md` §4: the truncated state swaps replace for
                    // retry. A partial answer can never be inserted, so the
                    // primary chord re-runs the turn instead of dead-ending
                    // on a disabled Replace.
                    if state.panel.is_truncated() && state.panel.active_user.is_some() {
                        panel::Message::Retry
                    } else {
                        panel::Message::Accept
                    }
                }
                // ⇧⏎ belongs to the composer: its key binding turns it into a
                // line break, so the chord must not also submit.
                WindowChord::Enter {
                    command: false,
                    shift: true,
                } => return Task::none(),
                WindowChord::Enter { command: false, .. } => panel::Message::Submit,
                WindowChord::Copy => {
                    if state.panel.can_copy() {
                        panel::Message::Copy
                    } else if let Some(panel::ErrorAction::CopyDiagnostics) = state
                        .panel
                        .error
                        .as_ref()
                        .and_then(|error| error.action.clone())
                    {
                        panel::Message::Error(panel::ErrorAction::CopyDiagnostics)
                    } else {
                        return Task::none();
                    }
                }
                WindowChord::Retry
                    if matches!(state.panel.phase, Phase::Finished { .. } | Phase::Failed)
                        && state.panel.active_user.is_some() =>
                {
                    panel::Message::Retry
                }
                WindowChord::Retry => return Task::none(),
                WindowChord::ShowTask => panel::Message::ShowTask,
                WindowChord::HistoryOlder => panel::Message::HistoryOlder,
                WindowChord::HistoryNewer => panel::Message::HistoryNewer,
                WindowChord::Attach if state.panel.clipboard.is_attachable() => {
                    panel::Message::Attach
                }
                WindowChord::Attach => return Task::none(),
                WindowChord::ToggleContext if state.panel.includes_selection() => {
                    panel::Message::ToggleContext
                }
                WindowChord::RemoveSelection if state.panel.includes_selection() => {
                    panel::Message::RemoveSelection
                }
                // `DetachLast` is deliberately dead in the panel. Backspace
                // with an empty input used to remove the newest image, and a
                // screen capture opens the panel with the image attached and
                // the input empty — so the first reflexive backspace threw the
                // screenshot away. Removal is now a pointer act on the chip or
                // the footer action; a fresh start is ⌘N.
                WindowChord::DetachLast
                | WindowChord::ToggleContext
                | WindowChord::RemoveSelection
                | WindowChord::CancelTask => return Task::none(),
            };
            panel_update(state, message)
        }
        Some(Role::Settings) => match chord {
            // Escape backs out of the draft before it closes the window: a
            // half-typed key is state the user can lose by reflex otherwise.
            WindowChord::Escape if state.settings.draft.is_some() => {
                settings_update(state, settings::Message::DraftCancel)
            }
            // Likewise an armed Forget: Escape is the reflex for "no, wait",
            // and it must disarm rather than close the window over it.
            WindowChord::Escape if state.settings.forget_armed.is_some() => {
                state.settings.forget_armed = None;
                Task::none()
            }
            WindowChord::Escape => settings_update(state, settings::Message::Close),
            WindowChord::New if state.settings.section == settings::Section::Providers => {
                settings_update(
                    state,
                    settings::Message::DraftBackend(settings::Backend::default()),
                )
            }
            WindowChord::Enter { command: false, .. } if state.settings.draft.is_some() => {
                settings_update(state, settings::Message::DraftSave)
            }
            // §6b: while a device code is waiting for approval, ⏎ opens the
            // verification page — exactly what the card's button advertises.
            WindowChord::Enter { .. }
                if state.settings.section == settings::Section::Providers
                    && state.settings.device_code().is_some() =>
            {
                settings_update(state, settings::Message::OpenDeviceUrl)
            }
            // The history block labels its enable action "⏎"; make that true.
            WindowChord::Enter { .. }
                if state.settings.section == settings::Section::History
                    && !state.settings.history_ready
                    && !state.settings.history_initializing
                    && state.settings.recovery_code.is_none() =>
            {
                settings_update(state, settings::Message::InitializeHistory)
            }
            WindowChord::Copy if state.settings.section == settings::Section::About => {
                settings_update(state, settings::Message::CopyDiagnostics)
            }
            // The recovery-code and device-code copy actions both label
            // themselves ⌘C; these arms are what make the labels honest.
            WindowChord::Copy
                if state.settings.section == settings::Section::History
                    && state.settings.recovery_code.is_some() =>
            {
                settings_update(state, settings::Message::CopyRecoveryCode)
            }
            WindowChord::Copy
                if state.settings.section == settings::Section::Providers
                    && state.settings.device_code().is_some() =>
            {
                let code = state.settings.device_code().unwrap_or_default().to_owned();
                settings_update(state, settings::Message::CopyDeviceCode(code))
            }
            // ⌫ forgets a provider only while that is unambiguous: exactly one
            // key-based row. With several, a global key cannot know which row
            // it means, and the rows drop their ⌫ hint to match.
            WindowChord::DetachLast
                if state.settings.section == settings::Section::Providers
                    && state.settings.draft.is_none() =>
            {
                let mut rows = state
                    .settings
                    .providers
                    .iter()
                    .filter(|row| row.id != aibo_core::types::ProviderId::CODEX);
                match (rows.next(), rows.next()) {
                    (Some(row), None) => {
                        let id = row.id.clone();
                        settings_update(state, settings::Message::ForgetProvider(id))
                    }
                    _ => Task::none(),
                }
            }
            _ => Task::none(),
        },
        _ => Task::none(),
    }
}

fn panel_update(state: &mut Aibo, message: panel::Message) -> Task<Message> {
    use panel::{ErrorAction, Message as M};

    match message {
        M::CopyLink(url) => iced::clipboard::write(url),

        M::InputChanged(input) => {
            // The accessibility tree's SetValue: a wholesale replacement.
            state.panel.set_input(&input);
            resize_panel_if_visible(state)
        }

        M::InputEdited(action) => {
            // The ⌘-fallout guard, editor edition: while ⌘ is down, a raw
            // character insertion can only be shortcut fallout (⌘L toggled
            // dictation *and* typed an `l`), never intended typing.
            if state.command_held
                && matches!(
                    &action,
                    iced::widget::text_editor::Action::Edit(
                        iced::widget::text_editor::Edit::Insert(_)
                    )
                )
            {
                return Task::none();
            }
            // `@` opens the file finder (§P9+): detected on the way in so the
            // character still lands in the text — commit strips it back out.
            let opens_finder = matches!(
                &action,
                iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Insert(
                    '@'
                ))
            );
            let height_before = state.panel.desired_height();
            let slash_open_before = state.panel.slash.open;
            state.panel.perform_input_action(action);
            if opens_finder && !state.panel.picker.open && !state.panel.file_finder.open {
                state.panel.file_finder.open();
                // The walk is re-requested on every open, so a file created
                // since the last one is findable.
                request_file_list(state);
                return resize_panel_if_visible(state).chain(operation::focus(panel::FINDER_ID));
            }
            // Opening or closing the slash popup re-roots the panel — the
            // view becomes a `stack!` with the menu over it, and back again —
            // and iced drops keyboard focus when a widget's position in the
            // tree changes. That left the user staring at a completion list
            // they could not type into (owner report, 2026-08-02). The popup
            // is deliberately focus-free: its query *is* the composer text,
            // so focus is put straight back on **both** transitions.
            let slash_toggled = state.panel.slash.open != slash_open_before;
            // Wrapping is why the composer is an editor at all: the window
            // has to follow the line count or the new lines paint past the
            // bottom edge.
            let resized = (state.panel.desired_height() - height_before).abs() >= 1.0;
            match (slash_toggled, resized) {
                (true, _) => {
                    return resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID));
                }
                (false, true) => return resize_panel_if_visible(state),
                (false, false) => {}
            }
            Task::none()
        }

        M::OpenPicker => {
            state.panel.picker.open();
            // The picker asks for a taller panel than the transcript does, and
            // `desired_height` alone changes nothing — a window is only resized
            // when something asks. Without this the overlay rendered into the
            // old height and the list was clipped after its first row.
            resize_panel_if_visible(state).chain(operation::focus(panel::PICKER_ID))
        }
        M::ClosePicker => {
            state.panel.picker.close();
            // Back to the transcript's height, and focus back to the input, or
            // the panel is unusable without the mouse after closing.
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }
        M::PickerQuery(query) => {
            // Same ⌘-fallout guard as the main input: ⌘D toggles a pin and
            // must not also type a `d` into the search query.
            if state.command_held && is_single_char_insertion(&state.panel.picker.query, &query) {
                return Task::none();
            }
            state.panel.picker.set_query(query);
            Task::none()
        }
        M::PickerMove(delta) => {
            let count = crate::model_picker::selectable(&state.panel.picker_rows()).len();
            state.panel.picker.move_highlight(delta, count);
            Task::none()
        }
        M::PickerCycleLane => {
            let capable = state.panel.capable_models();
            let lanes = crate::model_picker::lanes(&capable, &state.panel.pins(&capable));
            state.panel.picker.cycle_lane(&lanes);
            Task::none()
        }
        M::BeginDrag => {
            state.panel_dragged = true;
            window::drag(state.panel_window)
        }
        M::BeginResize => {
            // The size the drag grows from is whatever is on screen now — the
            // hand-set one if there is one, otherwise the size the content
            // arrived at, so the first drag continues from what the user can
            // see rather than jumping to a default.
            let origin = state.panel.user_size.unwrap_or_else(|| {
                state.last_placement.map_or(
                    (ui_theme::PANEL_WIDTH_DEFAULT, state.panel.desired_height()),
                    |placement| placement.size,
                )
            });
            state.panel_resize = Some(ResizeDrag {
                origin,
                anchor: None,
            });
            Task::none()
        }
        M::ResetSize => {
            // The press that opened this double-click already started a drag;
            // it moved nothing, and this ends it.
            state.panel_resize = None;
            if state.panel.user_size.take().is_none() {
                return Task::none();
            }
            let persist = state.persist_panel_size(None);
            resize_panel_if_visible(state).chain(persist)
        }
        M::PickerToggleFavourite => {
            let rollback = PersistenceSnapshot::PinnedModels {
                pins: state.panel.favourite_models.clone(),
                customised: state.panel.pins_customised,
            };
            let rows = state.panel.picker_rows();
            if let Some(option) =
                crate::model_picker::selectable(&rows).get(state.panel.picker.highlight)
            {
                let binding = option.binding.clone();
                state.panel.toggle_favourite(binding);
            }
            sync_settings_models(state);
            persist_pins(state, rollback);
            Task::none()
        }
        M::PickerChoose(index) => {
            let count = crate::model_picker::selectable(&state.panel.picker_rows()).len();
            if index >= count {
                return Task::none();
            }
            state.panel.picker.highlight = index;
            panel_update(state, M::PickerCommit)
        }
        M::PickerPin(index) => {
            let rollback = PersistenceSnapshot::PinnedModels {
                pins: state.panel.favourite_models.clone(),
                customised: state.panel.pins_customised,
            };
            let rows = state.panel.picker_rows();
            if let Some(option) = crate::model_picker::selectable(&rows).get(index) {
                let binding = option.binding.clone();
                state.panel.toggle_favourite(binding);
            }
            sync_settings_models(state);
            persist_pins(state, rollback);
            Task::none()
        }
        M::PickerLane(lane) => {
            state.panel.picker.lane = lane;
            // A new lane is a new result set; a stale highlight would commit
            // something never looked at.
            state.panel.picker.highlight = 0;
            Task::none()
        }

        M::FinderQuery(query) => {
            // The same ⌘-fallout guard as every other text field.
            if state.command_held
                && is_single_char_insertion(&state.panel.file_finder.query, &query)
            {
                return Task::none();
            }
            state.panel.file_finder.set_query(query);
            Task::none()
        }
        M::FinderMove(delta) => {
            state.panel.file_finder.move_highlight(delta);
            Task::none()
        }
        M::FinderChoose(index) => {
            state.panel.file_finder.highlight = index;
            panel_update(state, M::FinderCommit)
        }
        M::FinderCommit => {
            let Some(file) = state.panel.file_finder.highlighted() else {
                return Task::none();
            };
            // Strip the `@` that opened the finder; it was a trigger, not
            // text the user meant to send.
            if let Some(stripped) = state.panel.input.strip_suffix('@') {
                let stripped = stripped.to_owned();
                state.panel.set_input(&stripped);
            }
            // `@` is path completion in both chat and agent mode. Reading a
            // chosen file asynchronously made binary formats such as DOCX end
            // in "could not read", and made the same gesture mean content in
            // one mode but a path in the other. The path is useful visible
            // input in either mode and selecting it is never a submission.
            append_path_to_composer(&mut state.panel, &file.path);
            state.panel.file_finder.close();
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }
        M::SlashMove(delta) => {
            let count = crate::slash::matches(&state.panel.input).len();
            state.panel.slash.move_highlight(delta, count);
            // Moving the highlight must not cost the composer its keyboard;
            // ↑/↓ are for choosing while the hands stay on the text.
            operation::focus(panel::INPUT_ID)
        }
        M::SlashChoose(index) => {
            state.panel.slash.highlight = index;
            panel_update(state, M::SlashAccept)
        }
        M::SlashAccept => {
            let commands = crate::slash::matches(&state.panel.input);
            let Some(command) = commands.get(state.panel.slash.highlight).copied() else {
                let input = state.panel.input.clone();
                state.panel.slash.dismiss(&input);
                return resize_panel_if_visible(state);
            };
            if command.takes_args {
                // Complete the prefix and keep the user typing the arguments;
                // the trailing space is what closes the popup.
                state.panel.set_input(&format!("{} ", command.name));
                resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
            } else {
                // Argument-less commands run on accept — a second ⏎ to
                // confirm a completion the popup already showed is busywork.
                state.panel.set_input(command.name);
                panel_update(state, M::Submit)
            }
        }
        M::SlashClose => {
            let input = state.panel.input.clone();
            state.panel.slash.dismiss(&input);
            // The popup goes away; the caret stays where the user left it.
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }
        M::HelpOpen => {
            state.panel.help_open = true;
            resize_panel_if_visible(state)
        }
        M::HelpClose => {
            state.panel.help_open = false;
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }

        M::AttachMenuToggle => {
            state.panel.attach_menu_open = !state.panel.attach_menu_open;
            resize_panel_if_visible(state)
        }
        M::AttachMenuClose => {
            state.panel.attach_menu_open = false;
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }
        M::AttachScreenshot => {
            state.panel.attach_menu_open = false;
            // Unlike the ⌥⇧Space crop-then-ask, this joins the conversation
            // in progress: the panel hides for clean pixels but nothing is
            // discarded, and the capture re-presents it with the crop
            // attached.
            state.region_capture_keeps_session = true;
            let hidden = if state.panel_visible {
                state.hide_panel()
            } else {
                Task::none()
            };
            hidden.chain(capture_screen_region_task())
        }
        M::AttachPickFile => {
            state.panel.attach_menu_open = false;
            state.panel.file_finder.open();
            request_file_list(state);
            resize_panel_if_visible(state).chain(operation::focus(panel::FINDER_ID))
        }

        M::FinderClose => {
            state.panel.file_finder.close();
            // The trigger `@` stays if the user dismissed — they may have
            // meant to type a literal one.
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }
        M::PickerCommit => {
            let rows = state.panel.picker_rows();
            let choices = crate::model_picker::selectable(&rows);
            let Some(option) = choices
                .get(state.panel.picker.highlight)
                .map(|o| (*o).clone())
            else {
                return Task::none();
            };
            drop(choices);
            state.panel.picker.close();
            // Closing shrinks the panel back to the transcript's height, so the
            // resize belongs here too.
            panel_update(state, M::SelectModel(option))
                .chain(resize_panel_if_visible(state))
                .chain(operation::focus(panel::INPUT_ID))
        }

        M::SelectModel(option) => {
            // Rebuilding the engine cancels its in-flight request. Keep model
            // changes available while composing or reviewing an answer, but
            // never let a dropdown gesture silently terminate a stream.
            if matches!(state.panel.phase, Phase::Loading | Phase::Streaming) {
                return Task::none();
            }
            let generation = state.begin_persistence(
                PersistenceScope::Model,
                PersistenceSnapshot::Model {
                    selected: state.panel.selected_model.clone(),
                    recents: state.panel.recent_models.clone(),
                },
            );
            state.panel.remember_model(option.binding.clone());
            state.panel.selected_model = Some(option.clone());
            state.send(UiRequest::SetModel {
                generation,
                binding: option.binding,
            });
            Task::none()
        }

        M::Submit => {
            if matches!(state.panel.context, ContextState::PermissionDenied { .. }) {
                return panel_update(state, M::OpenSystemSettings);
            }
            if matches!(state.panel.phase, Phase::Failed)
                && let Some(action @ (ErrorAction::SignIn(_) | ErrorAction::OpenSettings)) = state
                    .panel
                    .error
                    .as_ref()
                    .and_then(|error| error.action.clone())
            {
                return panel_update(state, M::Error(action));
            }
            if matches!(state.panel.phase, Phase::Loading | Phase::Streaming) {
                return Task::none();
            }
            if state.panel.input.trim().is_empty() {
                return Task::none();
            }

            // ⏎ ends a live dictation turn (owner, 2026-08-03, and a
            // setting): the alternative is a microphone that keeps typing
            // into the next composer after the message left.
            if state.settings.stt_end_on_send {
                stop_dictation_if_active(state);
            }

            // Slash commands and their bare aliases act locally; they never
            // reach a model (owner redesign, 2026-08-02: help is "?", "help"
            // or "/help" typed into the composer).
            if let Some((command, command_args)) = crate::slash::parse(&state.panel.input) {
                use crate::slash::CommandAction;
                match command.action {
                    CommandAction::Help => {
                        state.panel.consume_input();
                        state.panel.help_open = true;
                        return resize_panel_if_visible(state);
                    }
                    CommandAction::NewChat => {
                        state.panel.consume_input();
                        return panel_update(state, M::NewChat);
                    }
                    CommandAction::Model => {
                        state.panel.consume_input();
                        return panel_update(state, M::OpenPicker);
                    }
                    CommandAction::Settings => {
                        state.panel.consume_input();
                        return open_settings(state);
                    }
                    CommandAction::Workdir => {
                        let args = command_args.trim().to_owned();
                        state.panel.consume_input();
                        if args.is_empty() {
                            return panel_update(state, M::WorkdirOpen);
                        }
                        // `~` expansion, then the filesystem's own verdict —
                        // the UI shares the process, so it can just ask.
                        let expanded = if let Some(rest) = args.strip_prefix("~/") {
                            std::env::var("HOME")
                                .map(|home| std::path::PathBuf::from(home).join(rest))
                                .unwrap_or_else(|_| std::path::PathBuf::from(&args))
                        } else {
                            std::path::PathBuf::from(&args)
                        };
                        match expanded.canonicalize() {
                            Ok(dir) if dir.is_dir() => {
                                state.panel.agent_workdir = Some(dir);
                                // A chosen workdir is agent intent; make the
                                // mode match so the chip shows the choice.
                                if !state.panel.agent_mode {
                                    state.panel.agent_mode = true;
                                    request_workdir_list(state);
                                }
                            }
                            _ => {
                                state.panel.toast = Some(panel::ToastView {
                                    severity: ui_theme::Severity::Warning,
                                    body: i18n::t1(crate::i18n::Key::ToastWorkdirInvalid, &args),
                                    offer_diagnostics: false,
                                });
                            }
                        }
                        return resize_panel_if_visible(state);
                    }
                    CommandAction::Skills => {
                        state.panel.consume_input();
                        return panel_update(state, M::SkillsOpen);
                    }
                    // `/agent …` and `/skill …` are the runtime's spellings
                    // and go through the ordinary submit path below.
                    CommandAction::Agent | CommandAction::Skill => {}
                }
            }

            let instruction = state.panel.input.clone();
            let history = state.panel.history_for_next_turn();
            let include_selection = state.panel.includes_selection();
            let attachments = state
                .panel
                .attachments()
                .iter()
                .map(|attached| attached.attachment.clone())
                .collect();
            state.panel.history.record(&instruction);
            // Steering (owner, 2026-08-02): typing while this session's run
            // is still going joins that run rather than starting a rival.
            if state.panel.agent_mode
                && let Some(running) = state
                    .panel
                    .session_tasks()
                    .filter(|task| task.is_running())
                    .last()
            {
                let task = running.id;
                state.panel.consume_input();
                state.send(UiRequest::SteerTask {
                    task,
                    text: instruction,
                });
                // The next steer starts here too; the caret has no reason to
                // leave the composer.
                return resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID));
            }
            if state.panel.agent_mode {
                // The task window owns this run's transcript. Beginning a
                // chat turn here too left the panel stuck on a spinner no
                // stream would ever resolve, with Send disabled — a soft
                // lock (owner screenshot, 2026-08-01). Just consume the
                // input; `TaskStarted` brings the ⌘T affordance.
                state.panel.consume_input();
            } else {
                state.panel.begin_turn(instruction.clone());
            }
            state.send(UiRequest::Submit {
                session: state.panel.session,
                instruction,
                // The toggle overrides the frozen surface: agent mode makes
                // every submission a Do until turned off or the session ends.
                surface: if state.panel.agent_mode {
                    Surface::Do
                } else {
                    state.panel.surface
                },
                role_override: None,
                attachments,
                history,
                include_selection,
                workdir: state.panel.agent_workdir.clone(),
            });
            // Keep the caret in the composer while the answer streams (owner,
            // 2026-08-02). `begin_turn` grew a transcript row above the input,
            // which re-roots the tree, and iced drops focus on a re-root — so
            // the next question could not be typed until completion's refocus.
            // Asserting it here, at the one re-rooting moment, is what lets
            // the caret survive the whole response without the resize path
            // ever touching focus (and yanking a mid-answer text selection).
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }

        // ↑ / ↓ recall. Only meaningful while the user is composing: once a
        // request is in flight the input is not what the keys should move.
        M::HistoryOlder => {
            if matches!(state.panel.phase, Phase::Idle)
                && let Some(text) = state.panel.history.older(&state.panel.input)
            {
                // `set_input` already leaves the caret at the end.
                state.panel.set_input(&text);
                return resize_panel_if_visible(state);
            }
            Task::none()
        }

        M::HistoryNewer => {
            if matches!(state.panel.phase, Phase::Idle)
                && let Some(text) = state.panel.history.newer()
            {
                state.panel.set_input(&text);
                return resize_panel_if_visible(state);
            }
            Task::none()
        }

        // ⌘V. An attachment is an act, and this is the act: nothing else in the
        // UI writes to `PanelState::attachments`, so nothing ambient can reach
        // routing through it.
        M::Attach => {
            // Reached from ⌘V or from the attach menu's clipboard row; either
            // way the menu's job is done.
            state.panel.attach_menu_open = false;
            let panel::ClipboardOffer::Image { image, .. } = &state.panel.clipboard else {
                // Nothing attachable. The action list already renders the entry
                // disabled in this state, so silence here is what the panel
                // promised rather than a dropped keypress.
                return Task::none();
            };

            let Some(attachment) = image.as_deref().cloned() else {
                // The pasteboard advertised an image it would not hand over.
                // §13 toast: non-blocking, nothing is lost, and it is the truth
                // — the alternative is a key that visibly does nothing.
                state.panel.toast = Some(panel::ToastView {
                    severity: ui_theme::Severity::Warning,
                    body: i18n::t(crate::i18n::Key::ToastClipboardImageUnreadable).to_owned(),
                    offer_diagnostics: false,
                });
                return resize_panel_if_visible(state);
            };

            // §14: the ceilings are enforced here, before dispatch. A rejection
            // is rendered inline with one action, never swallowed.
            if let Err(error) = state.panel.attach(attachment) {
                state.panel.fail(&std::sync::Arc::new(error));
            }
            resize_panel_if_visible(state)
        }

        M::DetachLast => {
            if state.panel.detach_last() {
                return resize_panel_if_visible(state);
            }
            Task::none()
        }

        M::Detach(id) => {
            if state.panel.detach(id) {
                return resize_panel_if_visible(state);
            }
            Task::none()
        }

        M::Accept => {
            // Defence in depth: `can_accept` already gates the button, but §13
            // makes a wrong insert the worst failure this product can have.
            if !state.panel.can_accept() {
                return Task::none();
            }
            // §8's insert sequence is ordered and the order is load-bearing:
            //   1. hide the panel
            //   2. restore_focus(target) — and CONFIRM it landed
            //   3. validate_target(target)
            //   4. one atomic paste
            // Steps 2-4 belong to the runtime, but step 1 is the UI's, and
            // dispatching `Insert` while the panel is still up inverts the
            // order: the panel holds focus, so `restore_focus` starts its
            // confirm-and-retry loop against a window aibo has not given up
            // yet. Chaining rather than calling `send` first is what makes the
            // hide actually precede the request instead of racing it.
            let Some(text) = state.panel.latest_answer().map(str::to_owned) else {
                return Task::none();
            };
            let request = UiRequest::Insert {
                session: state.panel.session,
                text,
            };
            let dispatch = state.deferred_send(vec![
                request,
                UiRequest::DiscardSession {
                    session: state.panel.session,
                },
            ]);
            state.hide_panel().chain(dispatch)
        }

        M::Copy => {
            if let Some(text) = state.panel.copyable_text().map(str::to_owned) {
                state.send(UiRequest::Copy { text });
            }
            Task::none()
        }

        M::Retry => {
            if !matches!(state.panel.phase, Phase::Finished { .. } | Phase::Failed)
                || state.panel.active_user.is_none()
            {
                return Task::none();
            }
            state.panel.begin_retry();
            state.send(UiRequest::Retry {
                session: state.panel.session,
                role: None,
            });
            resize_panel_if_visible(state)
        }

        M::Escalate => {
            if !matches!(state.panel.phase, Phase::Finished { .. } | Phase::Failed) {
                return Task::none();
            }
            state.panel.begin_retry();
            state.send(UiRequest::Retry {
                session: state.panel.session,
                role: Some(aibo_core::types::Role::Smart),
            });
            Task::none()
        }

        M::Dismiss => {
            stop_dictation_if_active(state);
            // `esc` cancels in-flight work and closes the panel (§13). It never
            // cancels an agent run — that lives in its own window (§6).
            discard_panel_session(state, true);
            state.hide_panel()
        }

        M::NewChat => {
            stop_dictation_if_active(state);
            // `begin_panel_session` is the hotkey's fresh-start path: cancel
            // and discard the old session, reset the panel, capture anew.
            // Reusing it is what keeps ⌘N and a context-driven fresh start
            // identical in behaviour.
            state.begin_panel_session();
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }

        M::ToggleAgentMode => {
            state.panel.agent_mode = !state.panel.agent_mode;
            if state.panel.agent_mode {
                // The chip needs candidates to name the default workdir, and
                // the picker should open warm.
                request_workdir_list(state);
            }
            resize_panel_if_visible(state)
        }

        M::WorkdirOpen => {
            state.panel.workdir_picker.open();
            // Re-listed on every open, so a directory created since the last
            // one is choosable.
            request_workdir_list(state);
            resize_panel_if_visible(state).chain(operation::focus(panel::WORKDIR_ID))
        }
        M::WorkdirClose => {
            state.panel.workdir_picker.close();
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }
        M::WorkdirQuery(query) => {
            // The same ⌘-fallout guard as every other text field.
            if state.command_held
                && is_single_char_insertion(&state.panel.workdir_picker.query, &query)
            {
                return Task::none();
            }
            state.panel.workdir_picker.query = query;
            state.panel.workdir_picker.highlight = 0;
            Task::none()
        }
        M::WorkdirMove(delta) => {
            state.panel.workdir_picker.move_highlight(delta);
            Task::none()
        }
        M::WorkdirChoose(index) => {
            state.panel.workdir_picker.highlight = index;
            panel_update(state, M::WorkdirCommit)
        }
        M::SkillsOpen => {
            state.panel.skills_open = true;
            // Re-listed on every open, so a skill the agent just wrote shows
            // without a restart.
            state.send(UiRequest::ListSkills);
            resize_panel_if_visible(state)
        }
        M::SkillsClose => {
            state.panel.skills_open = false;
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }

        M::WorkdirCommit => {
            let chosen = state
                .panel
                .workdir_picker
                .rows()
                .get(state.panel.workdir_picker.highlight)
                .map(|dir| dir.to_path_buf());
            let Some(dir) = chosen else {
                return Task::none();
            };
            state.panel.agent_workdir = Some(dir);
            state.panel.workdir_picker.close();
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }
        M::TasksToggle => {
            state.panel.tasks_open = !state.panel.tasks_open;
            if state.panel.tasks_open {
                state.panel.selected_task = None;
            }
            resize_panel_if_visible(state)
        }
        M::TasksClose => {
            state.panel.tasks_open = false;
            state.panel.selected_task = None;
            resize_panel_if_visible(state)
        }
        M::TasksSelect(task) => {
            state.panel.selected_task = task;
            Task::none()
        }
        M::TaskToggleEntry(task, index) => {
            if let Some(task) = state.panel.tasks.iter_mut().find(|t| t.id == task)
                && let Some(entry) = task.entries.get_mut(index)
            {
                entry.collapsed = !entry.collapsed;
            }
            Task::none()
        }
        M::TaskConfirmation(task, value) => {
            if let Some(task) = state.panel.tasks.iter_mut().find(|t| t.id == task) {
                task.typed_confirmation = value;
            }
            Task::none()
        }
        M::TaskDecide(id, decision) => {
            let Some(task) = state.panel.tasks.iter_mut().find(|t| t.id == id) else {
                return Task::none();
            };
            if !task.decision_is_ready(decision) {
                return Task::none();
            }
            let Some(approval) = task.pending_approval.take() else {
                return Task::none();
            };
            let typed_confirmation = approval
                .requires_typed_confirmation
                .then(|| task.typed_confirmation.clone());
            state.send(UiRequest::Approve {
                task: id,
                approval: approval.id,
                decision,
                typed_confirmation,
            });
            state.refresh_tray();
            Task::none()
        }
        M::TaskCancel(id) => {
            state.send(UiRequest::CancelTask { task: id });
            Task::none()
        }
        M::TaskCopy(id) => {
            if let Some(task) = state.panel.tasks.iter().find(|t| t.id == id) {
                let text = task.transcript();
                state.send(UiRequest::Copy { text });
            }
            Task::none()
        }
        M::ToggleDictation => {
            if state.dictation_stopping {
                return Task::none();
            }
            if state.panel.dictating {
                stop_dictation_if_active(state);
            } else {
                // Optimistic: the runtime confirms with `DictationStarted` or
                // corrects with `DictationFailed`. Waiting for the round-trip
                // would make the button feel dead for exactly the press that
                // starts the microphone.
                state.panel.dictating = true;
                state.dictation_turn = state.dictation_turn.wrapping_add(1);
                state.send(UiRequest::StartDictation {
                    turn: state.dictation_turn,
                });
            }
            Task::none()
        }

        M::DismissToast => {
            state.panel.toast = None;
            Task::none()
        }

        M::CopyDiagnostics => {
            state.send(UiRequest::CopyDiagnostics);
            Task::none()
        }
        M::ResponseAction(action) => {
            state.panel.perform_response_action(action);
            Task::none()
        }
        M::TurnUserAction(index, action) => {
            state.panel.perform_turn_user_action(index, action);
            Task::none()
        }
        M::TurnAssistantAction(index, action) => {
            state.panel.perform_turn_assistant_action(index, action);
            Task::none()
        }
        M::ActiveUserAction(action) => {
            state.panel.perform_active_user_action(action);
            Task::none()
        }

        M::ToggleContext => {
            if state.panel.includes_selection() {
                state.panel.context_expanded = !state.panel.context_expanded;
                return resize_panel_if_visible(state);
            }
            Task::none()
        }

        M::RemoveSelection => {
            if state.panel.includes_selection() {
                state.panel.remove_selection();
                return resize_panel_if_visible(state);
            }
            Task::none()
        }

        M::ShowTask => focus_first_task(state),

        M::OpenSystemSettings => {
            state.send(UiRequest::OpenSystemSettings {
                permission: aibo_core::types::Permission::Accessibility,
            });
            Task::none()
        }

        M::Error(action) => {
            let session = state.panel.session;
            match action {
                ErrorAction::Retry => {
                    state.panel.begin_retry();
                    state.send(UiRequest::Retry {
                        session,
                        role: None,
                    });
                    Task::none()
                }
                ErrorAction::RetryWith(role) => {
                    state.panel.begin_retry();
                    state.send(UiRequest::Retry {
                        session,
                        role: Some(role),
                    });
                    Task::none()
                }
                ErrorAction::SignIn(provider) => {
                    state.send(UiRequest::SignIn { provider });
                    open_settings(state)
                }
                ErrorAction::OpenSettings => open_settings(state),
                // §13's one action, and it has to actually resolve the state:
                // `detach_labelled` removes the image the error named and
                // `PanelState` retires the error with it, so the user is left
                // in a panel they can submit rather than staring at a complaint
                // about something that is no longer there.
                ErrorAction::RemoveAttachment { label } => {
                    state.panel.detach_labelled(&label);
                    resize_panel_if_visible(state)
                }
                ErrorAction::CopyDiagnostics => {
                    state.send(UiRequest::CopyDiagnostics);
                    Task::none()
                }
                // TODO(§5): trimming needs the capture the runtime holds; the
                // UI cannot shorten a selection it never received in full.
                ErrorAction::TrimSelection => Task::none(),
                // TODO(§14): raising a budget ceiling mid-run is a settings
                // write plus a resume, both runtime-side.
                ErrorAction::ContinueAnyway => Task::none(),
                // Rebinding the role chain to `model` is a settings write the
                // runtime owns; until `UiRequest` carries one, the quick-pick
                // is the honest version of this action — the error names a
                // model that works, and the picker is where one is chosen.
                ErrorAction::UseModel { .. } => panel_update(state, M::OpenPicker),
            }
        }
    }
}

/// Mirror the panel's model catalogue and pin set into the settings window.
///
/// The settings Models section curates the same favourites the quick-pick
/// shows; one owner (the panel), one syncing point, or the two lists drift.
fn sync_settings_models(state: &mut Aibo) {
    state.settings.models = state.panel.model_options.clone();
    let capable = state.panel.model_options.clone();
    state.settings.favourite_models = state.panel.pins(&capable);
}

fn apply_persistence_result(
    state: &mut Aibo,
    scope: PersistenceScope,
    generation: u64,
    succeeded: bool,
) -> Task<Message> {
    let Some(pending) = state.pending_persistence.get(&scope) else {
        return Task::none();
    };
    if pending.generation != generation {
        return Task::none();
    }
    let pending = state
        .pending_persistence
        .remove(&scope)
        .expect("generation checked above");
    if succeeded {
        return Task::none();
    }

    tracing::warn!(
        ?scope,
        generation,
        "settings write failed; rolling back optimistic state"
    );
    match pending.rollback {
        PersistenceSnapshot::Language(language) => {
            i18n::set_language(language);
            state.settings.language = language;
            state.config.language = language;
            if let Some(tray) = &state.tray {
                let _ = tray.relocalise();
            }
        }
        PersistenceSnapshot::Appearance(preference) => {
            state.settings.appearance = preference;
            state.config.appearance_preference = preference;
            state.config.appearance = preference.resolve(aibo_platform::system_prefers_dark());
        }
        PersistenceSnapshot::Model { selected, recents } => {
            state.panel.selected_model = selected;
            state.panel.recent_models = recents;
        }
        PersistenceSnapshot::ProviderSave(draft) => {
            if state.settings.draft.is_none() {
                state.settings.draft = Some(draft);
            }
        }
        PersistenceSnapshot::ProviderRemove(armed) => {
            if state.settings.forget_armed.is_none() {
                state.settings.forget_armed = armed;
            }
        }
        PersistenceSnapshot::SttEndOnSend(enabled) => {
            state.settings.stt_end_on_send = enabled;
        }
        PersistenceSnapshot::SttBackend(choice) => state.settings.stt_backend = choice,
        PersistenceSnapshot::PinnedModels { pins, customised } => {
            state.panel.favourite_models = pins;
            state.panel.pins_customised = customised;
            sync_settings_models(state);
        }
        PersistenceSnapshot::Hotkey {
            action,
            live,
            status,
            draft,
        } => {
            if let Some(hotkeys) = state.hotkeys.as_mut() {
                match live {
                    Some(previous) => {
                        let _ = hotkeys.rebind(action, previous);
                    }
                    None => hotkeys.unbind(action),
                }
            }
            match status.clone() {
                Some(status) => {
                    state.settings.hotkeys.insert(action, status);
                }
                None => {
                    state.settings.hotkeys.remove(&action);
                }
            }
            if action == HotkeyAction::TogglePanel {
                state.hotkey_status = status.clone();
                state.settings.hotkey = status;
            }
            if state.settings.hotkey_draft.is_empty() {
                state.settings.hotkey_draft = draft;
            }
        }
        PersistenceSnapshot::AxTreeActivation(enabled) => {
            state.settings.ax_tree_activation = enabled;
        }
        PersistenceSnapshot::FileRoots(roots) => state.settings.file_roots = roots,
        PersistenceSnapshot::MonthlyBudget {
            configured,
            hard_stop,
            spend_fraction,
        } => {
            state.settings.budget_configured = configured;
            state.settings.budget_hard_stop = hard_stop;
            state.settings.spend_fraction = spend_fraction;
        }
        PersistenceSnapshot::Updates { enabled, channel } => {
            state.settings.auto_update = enabled;
            state.settings.update_channel = channel;
        }
        PersistenceSnapshot::PanelSize(size) => {
            state.panel.user_size = size;
            return resize_panel_if_visible(state);
        }
    }
    Task::none()
}

/// Persist the pin set after a deliberate toggle.
///
/// The literal customised list, not the derived view: `favourite_models` is
/// what [`panel::PanelState::pins`] honours once `pins_customised` is set,
/// and it is exactly what must come back at the next launch.
fn persist_pins(state: &mut Aibo, rollback: PersistenceSnapshot) {
    let generation = state.begin_persistence(PersistenceScope::PinnedModels, rollback);
    state.send(UiRequest::SetPinnedModels {
        generation,
        pins: state.panel.favourite_models.clone(),
    });
}

/// Arm the settings `✓ copied` badge and schedule its expiry.
///
/// §6b: a copy affordance confirms for a moment — "silent copying leaves
/// people pressing it twice" — then reverts. The epoch keeps a stale expiry
/// task from clearing the badge a newer copy just set.
fn arm_copied_badge(state: &mut Aibo, badge: settings::CopiedBadge) -> Task<Message> {
    state.settings.copied_epoch += 1;
    state.settings.copied_badge = Some(badge);
    let epoch = state.settings.copied_epoch;
    Task::future(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        Message::Settings(settings::Message::CopiedBadgeExpired(epoch))
    })
}

fn settings_update(state: &mut Aibo, message: settings::Message) -> Task<Message> {
    use settings::Message as M;

    match message {
        M::Select(section) => {
            state.settings.hotkey_recording = false;
            state.settings.section = section;
            // Navigating away is a second thought: an armed Forget must not
            // survive it and go off when the user comes back.
            state.settings.forget_armed = None;
            Task::none()
        }
        M::ToggleCodexDetails => {
            state.settings.codex_details_expanded = !state.settings.codex_details_expanded;
            Task::none()
        }
        M::SignIn(provider) => {
            state.send(UiRequest::SignIn { provider });
            Task::none()
        }
        M::DraftBackend(backend) => {
            match &mut state.settings.draft {
                // Switching backend mid-draft keeps what was typed: the key is
                // usually the last thing entered and re-typing it because the
                // wrong row was picked first is a real annoyance.
                Some(draft) => {
                    draft.backend = backend;
                }
                None => state.settings.draft = Some(settings::ProviderDraft::new(backend)),
            }
            Task::none()
        }
        // The three draft fields share the panel input's ⌘-fallout guard: ⌘N
        // with a focused field otherwise types an `n` into it — worst of all
        // into the API key.
        M::DraftId(id) => {
            if let Some(draft) = &mut state.settings.draft
                && !(state.command_held && is_single_char_insertion(&draft.id, &id))
            {
                draft.id = id;
            }
            Task::none()
        }
        M::DraftBaseUrl(url) => {
            if let Some(draft) = &mut state.settings.draft
                && !(state.command_held && is_single_char_insertion(&draft.base_url, &url))
            {
                draft.base_url = url;
            }
            Task::none()
        }
        M::DraftModels(models) => {
            if let Some(draft) = &mut state.settings.draft
                && !(state.command_held && is_single_char_insertion(&draft.models, &models))
            {
                draft.models = models;
            }
            Task::none()
        }
        M::DraftKey(key) => {
            if let Some(draft) = &mut state.settings.draft
                && !(state.command_held && is_single_char_insertion(draft.key_field(), &key))
            {
                draft.set_key(key);
            }
            Task::none()
        }
        M::DraftCancel => {
            // Dropping the draft scrubs the key; that is `ProviderDraft::drop`.
            state.settings.draft = None;
            Task::none()
        }
        M::DraftSave => {
            let Some(draft) = state.settings.draft.take() else {
                return Task::none();
            };
            if !draft.is_saveable() {
                state.settings.draft = Some(draft);
                return Task::none();
            }
            let backend = draft.backend.config_value().to_owned();
            let id = (!draft.id.trim().is_empty()).then(|| draft.id.trim().to_owned());
            let base_url =
                (!draft.base_url.trim().is_empty()).then(|| draft.base_url.trim().to_owned());
            // Azure only: the comma-separated deployments become the picker
            // catalogue. Other backends never show the field, so it is empty.
            let models: Vec<String> = draft
                .models
                .split(',')
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_owned)
                .collect();
            let key = draft.key_for_persistence();
            let generation = state.begin_persistence(
                PersistenceScope::Provider,
                PersistenceSnapshot::ProviderSave(draft),
            );
            state.send(UiRequest::SetProviderKey {
                generation,
                backend,
                id,
                base_url,
                models,
                key,
            });
            Task::none()
        }
        M::ForgetProvider(provider) => {
            // Irreversible — the credential leaves the OS store — so the first
            // press arms and only a second press on the same provider sends.
            // The task-window equivalent gets a typed confirmation (§5); a
            // two-press with a relabelled button is the settings-scale version.
            if state.settings.forget_armed.as_ref() == Some(&provider) {
                let generation = state.begin_persistence(
                    PersistenceScope::Provider,
                    PersistenceSnapshot::ProviderRemove(Some(provider.clone())),
                );
                state.settings.forget_armed = None;
                state.send(UiRequest::RemoveProvider {
                    generation,
                    id: provider.as_str().to_owned(),
                });
            } else {
                state.settings.forget_armed = Some(provider);
            }
            Task::none()
        }
        M::ToggleFavourite(binding) => {
            // The panel owns the pin set; the settings list is a mirror.
            let rollback = PersistenceSnapshot::PinnedModels {
                pins: state.panel.favourite_models.clone(),
                customised: state.panel.pins_customised,
            };
            state.panel.toggle_favourite(binding);
            sync_settings_models(state);
            persist_pins(state, rollback);
            Task::none()
        }
        M::OpenSystemSettings(permission) => {
            state.send(UiRequest::OpenSystemSettings { permission });
            Task::none()
        }
        // Copy the code exactly as the server issued it, hyphen included — the
        // verification page expects that form, and a "helpfully" stripped or
        // re-spaced version silently fails to match.
        M::CopyDeviceCode(code) => {
            aibo_platform::announce_accessibility(i18n::t(crate::i18n::Key::ToastCopied));
            let expiry = arm_copied_badge(state, settings::CopiedBadge::DeviceCode);
            iced::clipboard::write(code).chain(expiry)
        }
        M::DeviceCodeAction(action) => {
            state.settings.perform_device_code_action(action);
            Task::none()
        }
        M::OpenDeviceUrl => {
            state.send(UiRequest::OpenUrl {
                url: settings::codex_text::VERIFICATION_URL.to_owned(),
            });
            Task::none()
        }
        M::AutoUpdateToggle(enabled) => {
            let generation = state.begin_persistence(
                PersistenceScope::Updates,
                PersistenceSnapshot::Updates {
                    enabled: state.settings.auto_update,
                    channel: state.settings.update_channel,
                },
            );
            state.settings.auto_update = enabled;
            state.send(UiRequest::SetUpdatePreferences {
                generation,
                enabled,
                channel: state.settings.update_channel,
            });
            if enabled {
                state.send(UiRequest::CheckForUpdates);
            }
            Task::none()
        }
        M::SetUpdateChannel(channel) => {
            let generation = state.begin_persistence(
                PersistenceScope::Updates,
                PersistenceSnapshot::Updates {
                    enabled: state.settings.auto_update,
                    channel: state.settings.update_channel,
                },
            );
            state.settings.update_channel = channel;
            state.settings.update_state = crate::bridge::UpdateState::Idle;
            state.send(UiRequest::SetUpdatePreferences {
                generation,
                enabled: state.settings.auto_update,
                channel,
            });
            state.send(UiRequest::CheckForUpdates);
            Task::none()
        }
        M::CheckForUpdates => {
            state.send(UiRequest::CheckForUpdates);
            Task::none()
        }
        M::InstallUpdate => {
            state.send(UiRequest::InstallUpdate);
            Task::none()
        }
        M::SetAppearance(preference) => {
            let generation = state.begin_persistence(
                PersistenceScope::Appearance,
                PersistenceSnapshot::Appearance(state.settings.appearance),
            );
            state.settings.appearance = preference;
            state.config.appearance_preference = preference;
            state.config.appearance = preference.resolve(aibo_platform::system_prefers_dark());
            state.send(UiRequest::SetAppearance {
                generation,
                preference,
            });
            Task::none()
        }
        M::SetLanguage(lang) => {
            let generation = state.begin_persistence(
                PersistenceScope::Language,
                PersistenceSnapshot::Language(state.settings.language),
            );
            i18n::set_language(lang);
            state.settings.language = lang;
            state.config.language = lang;
            if let Some(tray) = &state.tray
                && let Err(error) = tray.relocalise()
            {
                tracing::warn!(%error, "could not relocalise the tray menu");
            }
            state.send(UiRequest::SetLanguage {
                generation,
                language: lang,
            });
            Task::none()
        }
        M::AxTreeToggle(enabled) => {
            let generation = state.begin_persistence(
                PersistenceScope::AxTreeActivation,
                PersistenceSnapshot::AxTreeActivation(state.settings.ax_tree_activation),
            );
            state.settings.ax_tree_activation = enabled;
            state.send(UiRequest::SetAxTreeActivation {
                generation,
                enabled,
            });
            Task::none()
        }
        M::SttEndOnSendToggle(enabled) => {
            let generation = state.begin_persistence(
                PersistenceScope::SttEndOnSend,
                PersistenceSnapshot::SttEndOnSend(state.settings.stt_end_on_send),
            );
            state.settings.stt_end_on_send = enabled;
            state.send(UiRequest::SetSttEndOnSend {
                generation,
                enabled,
            });
            Task::none()
        }
        M::SetSttBackend(choice) => {
            let generation = state.begin_persistence(
                PersistenceScope::SttBackend,
                PersistenceSnapshot::SttBackend(state.settings.stt_backend),
            );
            state.settings.stt_backend = choice;
            state.send(UiRequest::SetSttBackend {
                generation,
                backend: choice.tag().map(str::to_owned),
            });
            Task::none()
        }
        M::RootDraft(draft) => {
            if state.command_held && is_single_char_insertion(&state.settings.root_draft, &draft) {
                return Task::none();
            }
            state.settings.root_draft = draft;
            if let Some(parent) = state
                .settings
                .root_completer
                .update_draft(&state.settings.root_draft)
            {
                request_directory_list(state, parent);
            }
            Task::none()
        }
        M::RootComplete(path) => {
            state.settings.root_draft = path;
            state.settings.root_completer.dismiss();
            Task::none()
        }
        M::RootAdd => {
            let root = state.settings.root_draft.trim().to_owned();
            if root.is_empty() {
                return Task::none();
            }
            // First edit materialises the defaults, so removing a default and
            // adding a folder compose the way the list on screen implies.
            let mut roots = state
                .settings
                .file_roots
                .clone()
                .unwrap_or_else(|| state.settings.default_file_roots.clone());
            if !roots.contains(&root) {
                roots.push(root);
            }
            let generation = state.begin_persistence(
                PersistenceScope::FileRoots,
                PersistenceSnapshot::FileRoots(state.settings.file_roots.clone()),
            );
            state.settings.root_draft.clear();
            let _ = state.settings.root_completer.update_draft("");
            state.settings.file_roots = Some(roots.clone());
            state.send(UiRequest::SetFileRoots {
                generation,
                roots: Some(roots),
            });
            Task::none()
        }
        M::RootRemove(index) => {
            let mut roots = state
                .settings
                .file_roots
                .clone()
                .unwrap_or_else(|| state.settings.default_file_roots.clone());
            if index < roots.len() {
                roots.remove(index);
            }
            let generation = state.begin_persistence(
                PersistenceScope::FileRoots,
                PersistenceSnapshot::FileRoots(state.settings.file_roots.clone()),
            );
            state.settings.file_roots = Some(roots.clone());
            state.send(UiRequest::SetFileRoots {
                generation,
                roots: Some(roots),
            });
            Task::none()
        }
        M::RootsReset => {
            let generation = state.begin_persistence(
                PersistenceScope::FileRoots,
                PersistenceSnapshot::FileRoots(state.settings.file_roots.clone()),
            );
            state.settings.file_roots = None;
            state.send(UiRequest::SetFileRoots {
                generation,
                roots: None,
            });
            Task::none()
        }
        M::HotkeyDraft(draft) => {
            if state.command_held && is_single_char_insertion(&state.settings.hotkey_draft, &draft)
            {
                return Task::none();
            }
            state.settings.hotkey_draft = draft;
            state.settings.hotkey_draft_invalid = false;
            Task::none()
        }
        M::HotkeySelect(action) => {
            state.settings.hotkey_action = action;
            state.settings.hotkey_recording = false;
            state.settings.hotkey_draft.clear();
            state.settings.hotkey_draft_invalid = false;
            Task::none()
        }
        M::HotkeyRecord(recording) => {
            state.settings.hotkey_recording = recording;
            state.settings.hotkey_draft_invalid = false;
            Task::none()
        }
        M::HotkeyCaptured(spec) => {
            // The recorder only ever offers combinations `hotkey::parse`
            // accepts, so this cannot arm Apply with something unusable.
            state.settings.hotkey_recording = false;
            state.settings.hotkey_draft = spec;
            state.settings.hotkey_draft_invalid = false;
            Task::none()
        }
        M::HotkeyApply => {
            let action = state.settings.hotkey_action;
            let spec = state.settings.hotkey_draft.trim().to_owned();
            if spec.is_empty() {
                return Task::none();
            }
            let Ok(parsed) = hotkey::parse(&spec) else {
                state.settings.hotkey_draft_invalid = true;
                return Task::none();
            };
            state.settings.hotkey_draft_invalid = false;
            let rollback = PersistenceSnapshot::Hotkey {
                action,
                live: state.hotkeys.as_ref().and_then(|hotkeys| {
                    hotkeys
                        .bindings()
                        .iter()
                        .find(|binding| binding.action == action)
                        .map(|binding| binding.hotkey)
                }),
                status: state.settings.hotkeys.get(&action).cloned().or_else(|| {
                    (action == HotkeyAction::TogglePanel)
                        .then(|| state.hotkey_status.clone())
                        .flatten()
                }),
                draft: state.settings.hotkey_draft.clone(),
            };
            let Some(hotkeys) = state.hotkeys.as_mut() else {
                // No registrar (it failed at startup): persist so the next
                // launch picks the combination up, and claim nothing more.
                let generation =
                    state.begin_persistence(PersistenceScope::Hotkey(action), rollback);
                state.send(UiRequest::SetHotkey {
                    generation,
                    action,
                    spec: Some(spec),
                });
                return Task::none();
            };
            // `rebind` restores the previous combination if the OS refuses
            // this one; either way its status is the truth the block renders.
            let status = hotkeys.rebind(action, parsed);
            let registered = matches!(status, hotkey::HotkeyStatus::Registered { .. });
            state.settings.hotkeys.insert(action, status.clone());
            if action == HotkeyAction::TogglePanel {
                state.hotkey_status = Some(status.clone());
                state.settings.hotkey = Some(status);
            }
            if registered {
                state.settings.hotkey_draft.clear();
                let generation =
                    state.begin_persistence(PersistenceScope::Hotkey(action), rollback);
                state.send(UiRequest::SetHotkey {
                    generation,
                    action,
                    spec: Some(spec),
                });
            }
            Task::none()
        }
        M::HotkeyReset => {
            let action = state.settings.hotkey_action;
            let rollback = PersistenceSnapshot::Hotkey {
                action,
                live: state.hotkeys.as_ref().and_then(|hotkeys| {
                    hotkeys
                        .bindings()
                        .iter()
                        .find(|binding| binding.action == action)
                        .map(|binding| binding.hotkey)
                }),
                status: state.settings.hotkeys.get(&action).cloned().or_else(|| {
                    (action == HotkeyAction::TogglePanel)
                        .then(|| state.hotkey_status.clone())
                        .flatten()
                }),
                draft: state.settings.hotkey_draft.clone(),
            };

            let reset_succeeded = match state.hotkeys.as_mut() {
                Some(hotkeys) => match hotkey::Binding::default_for(action) {
                    Some(default) => {
                        let status = hotkeys.rebind(action, default.hotkey);
                        let registered = matches!(status, hotkey::HotkeyStatus::Registered { .. });
                        state.settings.hotkeys.insert(action, status.clone());
                        if action == HotkeyAction::TogglePanel {
                            state.hotkey_status = Some(status.clone());
                            state.settings.hotkey = Some(status);
                        }
                        registered
                    }
                    None => {
                        hotkeys.unbind(action);
                        state.settings.hotkeys.remove(&action);
                        true
                    }
                },
                // With no registrar there is no live binding to reset. Removing
                // the override still restores the shipped state next launch.
                None => true,
            };

            if reset_succeeded {
                state.settings.hotkey_recording = false;
                state.settings.hotkey_draft.clear();
                state.settings.hotkey_draft_invalid = false;
                let generation =
                    state.begin_persistence(PersistenceScope::Hotkey(action), rollback);
                state.send(UiRequest::SetHotkey {
                    generation,
                    action,
                    spec: None,
                });
            }
            Task::none()
        }
        M::BudgetLimitDraft(draft) => {
            if state.command_held
                && is_single_char_insertion(&state.settings.budget_limit_draft, &draft)
            {
                return Task::none();
            }
            state.settings.budget_limit_draft = draft;
            Task::none()
        }
        M::BudgetWarnDraft(draft) => {
            if state.command_held
                && is_single_char_insertion(&state.settings.budget_warn_draft, &draft)
            {
                return Task::none();
            }
            state.settings.budget_warn_draft = draft;
            Task::none()
        }
        M::BudgetHardStop(hard_stop) => {
            let rollback = PersistenceSnapshot::MonthlyBudget {
                configured: state.settings.budget_configured,
                hard_stop: state.settings.budget_hard_stop,
                spend_fraction: state.settings.spend_fraction,
            };
            state.settings.budget_hard_stop = hard_stop;
            // Live only while a ceiling is in force; otherwise it rides the
            // next Apply.
            if state.settings.budget_configured
                && let Some((limit_micros, warn_at_percent)) =
                    settings::parsed_budget(&state.settings)
            {
                let generation = state.begin_persistence(PersistenceScope::MonthlyBudget, rollback);
                state.send(UiRequest::SetMonthlyBudget {
                    generation,
                    limit_micros: Some(limit_micros),
                    warn_at_percent,
                    hard_stop,
                });
            }
            Task::none()
        }
        M::BudgetApply => {
            let Some((limit_micros, warn_at_percent)) = settings::parsed_budget(&state.settings)
            else {
                return Task::none();
            };
            let rollback = PersistenceSnapshot::MonthlyBudget {
                configured: state.settings.budget_configured,
                hard_stop: state.settings.budget_hard_stop,
                spend_fraction: state.settings.spend_fraction,
            };
            let generation = state.begin_persistence(PersistenceScope::MonthlyBudget, rollback);
            state.settings.budget_configured = true;
            state.send(UiRequest::SetMonthlyBudget {
                generation,
                limit_micros: Some(limit_micros),
                warn_at_percent,
                hard_stop: state.settings.budget_hard_stop,
            });
            Task::none()
        }
        M::BudgetRemove => {
            let rollback = PersistenceSnapshot::MonthlyBudget {
                configured: state.settings.budget_configured,
                hard_stop: state.settings.budget_hard_stop,
                spend_fraction: state.settings.spend_fraction,
            };
            let generation = state.begin_persistence(PersistenceScope::MonthlyBudget, rollback);
            state.settings.budget_configured = false;
            state.settings.spend_fraction = None;
            state.send(UiRequest::SetMonthlyBudget {
                generation,
                limit_micros: None,
                warn_at_percent: 80,
                hard_stop: false,
            });
            Task::none()
        }
        M::CopyDiagnostics => {
            state.send(UiRequest::CopyDiagnostics);
            Task::none()
        }
        M::InitializeHistory => {
            state.settings.history_initializing = true;
            state.settings.history_failed = false;
            state.send(UiRequest::InitializeHistory);
            Task::none()
        }
        M::CopyRecoveryCode => {
            let Some(code) = state.settings.recovery_code.as_ref() else {
                return Task::none();
            };
            let code = code.expose_secret().to_owned();
            aibo_platform::announce_accessibility(i18n::t(crate::i18n::Key::ToastCopied));
            let expiry = arm_copied_badge(state, settings::CopiedBadge::RecoveryCode);
            iced::clipboard::write(code).chain(expiry)
        }
        M::CopiedBadgeExpired(epoch) => {
            if state.settings.copied_epoch == epoch {
                state.settings.copied_badge = None;
            }
            Task::none()
        }
        M::Close => {
            state.settings.hotkey_recording = false;
            state.settings.recovery_code = None;
            match state.settings_window {
                Some(id) => window::close(id),
                None => Task::none(),
            }
        }
    }
}

/// Finish a non-activating summon once the source application's context has
/// been captured. If cold-start geometry is still pending, `show_panel` will
/// perform the focus step when it eventually reveals the window.
fn finish_context_capture(state: &mut Aibo, task: Task<Message>) -> Task<Message> {
    if !std::mem::take(&mut state.context_capture_pending) || !state.panel_visible {
        return task;
    }
    state.panel_focused = true;
    task.chain(window::gain_focus(state.panel_window))
        .chain(operation::focus(panel::INPUT_ID))
}

fn backend_update(state: &mut Aibo, event: UiEvent) -> Task<Message> {
    match event {
        UiEvent::PersistenceResult {
            scope,
            generation,
            succeeded,
        } => apply_persistence_result(state, scope, generation, succeeded),

        UiEvent::Context {
            session,
            app,
            field,
            selection,
            clipboard,
        } => {
            // §13: one panel, one session. An answer for a session the user has
            // moved on from is dropped, not rendered.
            if session != state.panel.session {
                return Task::none();
            }
            let selection = selection.filter(|text| !text.is_empty());

            // A selection is a new question, not a follow-up to the last one.
            // Reopening the panel keeps the conversation
            // ([`Aibo::resume_panel_session`]); arriving with text selected
            // discards it, because the alternative is asking about this
            // selection with an unrelated exchange still above it and in the
            // model's history.
            // …unless a turn is already in flight: the capture was requested
            // at open, and a fast submit can beat a slow AX read (a terminal
            // scrollback takes a second or more). The late capture then
            // belongs to the *previous* gesture, and starting fresh here
            // destroys the turn the user just asked (observed live,
            // 2026-08-02: the submission vanished mid-request).
            if selection.is_some()
                && state.panel.has_conversation()
                && !matches!(state.panel.phase, Phase::Loading | Phase::Streaming)
            {
                state.begin_panel_session_for(app.as_ref().map(|app| app.app_ref.clone()));
                // `begin_panel_session` re-requests context against the new
                // id, so this event now belongs to a session that is gone.
                // Letting it fall through would file the selection under the
                // old conversation it just replaced.
                return resize_panel_if_visible(state);
            }
            state.panel.clipboard = clipboard_offer(clipboard.as_deref());
            state.panel.context = match field.as_deref() {
                // §9: while composing, aibo neither reads nor inserts.
                Some(field) if field.ime_active => ContextState::ImeActive,
                Some(field) => ContextState::Available {
                    app: app.clone(),
                    excerpt: selection
                        .as_deref()
                        .or(Some(field.prefix.as_str()))
                        .map(|text| crate::widgets::elide(text, 96)),
                    selection,
                    truncated: field.truncated,
                    caret_bounds: field.caret_bounds,
                },
                // A terminal or canvas may expose a selection through the
                // synthetic-copy fallback while exposing no readable text
                // field. That is usable context, not "unavailable".
                None if selection.is_some() => ContextState::Available {
                    app: app.clone(),
                    excerpt: selection
                        .as_deref()
                        .map(|text| crate::widgets::elide(text, 96)),
                    selection,
                    truncated: false,
                    caret_bounds: None,
                },
                None => match app.as_ref() {
                    Some(app) => ContextState::Unavailable {
                        app: Some(app.display_name.clone()),
                    },
                    None => ContextState::Unavailable { app: None },
                },
            };
            let resized = resize_panel_if_visible(state);
            finish_context_capture(state, resized)
        }

        UiEvent::ContextFailed { session, error } => {
            if session != state.panel.session {
                return Task::none();
            }
            if let aibo_core::AiboError::CaptureFailed {
                reason: aibo_core::error::CaptureFailure::Denied,
                ..
            } = error.as_ref()
            {
                state.panel.context = ContextState::PermissionDenied {
                    status: aibo_core::types::PermissionStatus::Denied,
                };
            }
            state.panel.fail(&error);
            // Same as `UiEvent::Failed`: the error ends the capture, so the
            // caret returns to the composer for the retry.
            let resized = resize_panel_if_visible(state);
            finish_context_capture(state, resized)
        }

        UiEvent::Dispatched {
            session,
            provider,
            model,
            substituted_for,
        } => {
            if session != state.panel.session {
                return Task::none();
            }
            let height_before = state.panel.desired_height();
            state.settings.active_model = Some(aibo_core::types::ModelBinding {
                provider: provider.clone(),
                model: model.clone(),
            });
            state.panel.attribution.provider = Some(provider);
            state.panel.attribution.model = Some(model);
            state.panel.attribution.substituted_for = substituted_for;
            state.panel.phase = Phase::Loading;
            // Attribution is a real footer row, and a fallback adds another.
            // The first-turn submit has already sized for the user/thinking
            // bubbles by the time this event arrives. Failing to grow again
            // makes the bottom-anchored transcript pay for these rows by
            // clipping the top of the user's first message.
            resize_panel_if_height_changed(state, height_before)
        }

        UiEvent::Stream { session, event } => {
            if session != state.panel.session {
                return Task::none();
            }
            match *event {
                StreamEvent::Text(chunk) => {
                    let height_before = state.panel.desired_height();
                    state.panel.phase = Phase::Streaming;
                    state.panel.append_response(&chunk);
                    // §16: reserve height in discrete steps so streaming never
                    // reflows. The estimate is deliberately coarse.
                    let estimated = 24.0 + (state.panel.response.len() as f32 / 64.0) * 20.0;
                    let reserve_changed = state.panel.reserve_for(estimated);
                    let content_height_changed =
                        (state.panel.desired_height() - height_before).abs() >= 1.0;
                    if reserve_changed || content_height_changed {
                        return resize_panel_if_visible(state);
                    }
                    Task::none()
                }
                StreamEvent::Reasoning(chunk) => {
                    // §7: its own channel, rendered collapsed, never inserted.
                    state.panel.reasoning.push_str(&chunk);
                    Task::none()
                }
                StreamEvent::Usage(usage) => {
                    let height_before = state.panel.desired_height();
                    state.panel.usage = usage;
                    // The first non-zero usage report inserts a footer row.
                    // Later reports normally only change its text, so the
                    // height comparison avoids redundant window-server work.
                    resize_panel_if_height_changed(state, height_before)
                }
                StreamEvent::Done(reason) => {
                    state.panel.phase = Phase::Finished { reason };
                    aibo_platform::announce_accessibility(i18n::t(crate::i18n::Key::TaskCompleted));
                    // Give the caret back, **once, on completion**.
                    //
                    // Stripping `operation::focus` out of the resize path was
                    // right — it was firing on every height change and yanking
                    // the caret mid-answer, including mid-selection. But
                    // removing it entirely left focus nowhere once the answer
                    // finished, so the next question could not be typed: the
                    // rail said attention had returned to the input
                    // (`input_rail_state`) while the keyboard disagreed.
                    //
                    // Completion is the honest moment for it. Exactly one focus
                    // per answer, at the point the user is free to type again.
                    resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
                }
                // Tool calls belong to an agent run and surface in the task
                // window, not the panel.
                StreamEvent::ToolCall { .. } => Task::none(),
            }
        }

        UiEvent::FirstToken {
            session,
            elapsed_ms,
        } => {
            if session == state.panel.session {
                state.panel.attribution.latency_ms = Some(elapsed_ms);
            }
            Task::none()
        }

        UiEvent::Cost {
            session,
            label,
            usage,
        } => {
            if session == state.panel.session {
                let height_before = state.panel.desired_height();
                state.panel.attribution.cost_label = Some(label);
                state.panel.usage = usage;
                return resize_panel_if_height_changed(state, height_before);
            }
            Task::none()
        }

        UiEvent::Failed { session, error } => {
            if session != state.panel.session {
                return Task::none();
            }
            state.panel.fail(&error);
            if let Some(error) = &state.panel.error {
                aibo_platform::announce_accessibility(&error.headline);
            }
            // §13: `NoProviderConfigured` is the only error allowed to
            // interrupt, and it opens settings.
            let opens_settings = matches!(
                state.panel.error.as_ref().map(|e| e.treatment),
                Some(aibo_core::error::Treatment::Blocking)
            );
            let resize = resize_panel_if_visible(state);
            if opens_settings {
                Task::batch([resize, open_settings(state)])
            } else {
                // A failure ends the run just as `StreamEvent::Done` ends a
                // success, and the caret comes back at the same moment: the
                // user's next move is to edit and retry, not to click the
                // input first.
                resize.chain(operation::focus(panel::INPUT_ID))
            }
        }

        UiEvent::Inserted { session } => {
            if session == state.panel.session {
                return state.hide_panel();
            }
            Task::none()
        }

        UiEvent::TaskStarted {
            task,
            session,
            instruction,
        } => {
            state.panel.handed_off_to_task = true;
            // A `/agent` submission looked like chat until this moment; take
            // back the spinner turn the activity card now narrates.
            state.panel.retract_handed_off_turn();
            match state.panel.tasks.iter_mut().find(|t| t.id == task) {
                Some(existing) => existing.instruction = instruction,
                None => state
                    .panel
                    .tasks
                    .push(TaskState::new(task, session, instruction)),
            }
            state.refresh_tray();
            // The card enters this session's transcript; the window makes room.
            resize_panel_if_visible(state)
        }

        UiEvent::TaskStep { task, step } => {
            let became_blocked;
            match state.panel.tasks.iter_mut().find(|t| t.id == task) {
                Some(existing) => {
                    let was_blocked = existing.is_blocked();
                    existing.push(*step);
                    became_blocked = existing.is_blocked() && !was_blocked;
                }
                None => {
                    // A step for a run that raced its Started event: keep it
                    // so scrollback and approvals survive.
                    let mut orphan = TaskState::new(task, state.panel.session, String::new());
                    orphan.push(*step);
                    became_blocked = orphan.is_blocked();
                    state.panel.tasks.push(orphan);
                }
            }
            state.refresh_tray();
            if became_blocked {
                // §11: nothing runs until the user answers. No window is
                // popped (owner redesign) — the chip and tray turn amber, and
                // the announcement tells a screen-reader user why.
                aibo_platform::announce_accessibility(i18n::t(
                    crate::i18n::Key::TaskAwaitingApproval,
                ));
            }
            // A finished run settles its card and appends the final message —
            // and gives the caret back, exactly like a finished chat answer:
            // "after I get the result I cannot keep on typing" (owner,
            // 2026-08-02). Once, on completion, current session only.
            let finished_here = state
                .panel
                .tasks
                .iter()
                .find(|t| t.id == task)
                .is_some_and(|t| !t.is_running() && t.session == state.panel.session);
            let resize = resize_panel_if_visible(state);
            if finished_here && state.panel_visible {
                resize.chain(operation::focus(panel::INPUT_ID))
            } else {
                resize
            }
        }

        UiEvent::DisplaysChanged { displays } => update(state, Message::Displays(displays)),

        UiEvent::PermissionChanged { permission, status } => {
            let height_before = state.panel.desired_height();
            match state
                .settings
                .permissions
                .iter_mut()
                .find(|row| row.permission == permission)
            {
                Some(row) => row.status = status,
                None => state
                    .settings
                    .permissions
                    .push(settings::PermissionRow { permission, status }),
            }
            if permission == aibo_core::types::Permission::Accessibility
                && matches!(
                    status,
                    aibo_core::types::PermissionStatus::Denied
                        | aibo_core::types::PermissionStatus::Revoked
                )
            {
                state.panel.context = ContextState::PermissionDenied { status };
            }
            resize_panel_if_height_changed(state, height_before)
        }

        UiEvent::ProviderRemoved { provider } => {
            state.settings.providers.retain(|row| row.id != provider);
            state.settings.sync_device_code();
            Task::none()
        }

        UiEvent::ProviderHealth { provider, health } => {
            let height_before = state.panel.desired_height();
            match state
                .settings
                .providers
                .iter_mut()
                .find(|row| row.id == provider)
            {
                Some(row) => row.health = health.clone(),
                None => state.settings.providers.push(settings::ProviderRow {
                    id: provider.clone(),
                    configured: true,
                    health: health.clone(),
                }),
            }
            state.settings.sync_device_code();
            // A provider coming back healthy has to clear the panel's stale
            // auth error, not just repaint the settings row.
            //
            // Observed 2026-07-26: after a successful sign-in the settings card
            // read "Signed in. Codex is bound to the Smart and Ask surfaces."
            // while the panel still showed "Your codex credentials are no longer
            // valid" with a Sign in button — two windows disagreeing about the
            // same fact, and the one the user works in was the wrong one.
            //
            // Scoped to auth errors on purpose: a `ContextTooLarge` or a
            // `Timeout` is still true regardless of what health now says, and
            // clearing those would hide a real failure behind an unrelated
            // recovery.
            if matches!(health, aibo_core::types::Health::Ok { .. })
                && let Some(error) = &state.panel.error
                && error.is_auth_for(&provider)
            {
                state.panel.error = None;
            }
            resize_panel_if_height_changed(state, height_before)
        }

        UiEvent::ModelOptions { options, selected } => {
            state.panel.selected_model = selected.and_then(|binding| {
                options
                    .iter()
                    .find(|option| option.binding == binding)
                    .cloned()
            });
            state.panel.model_options = options;
            sync_settings_models(state);
            if state.settings.active_model.is_none() {
                state.settings.active_model = state
                    .panel
                    .selected_model
                    .as_ref()
                    .map(|option| option.binding.clone());
            }
            Task::none()
        }

        UiEvent::LanguageChanged { language } => {
            let height_before = state.panel.desired_height();
            i18n::set_language(language);
            state.settings.language = language;
            state.config.language = language;
            if let Some(tray) = &state.tray
                && let Err(error) = tray.relocalise()
            {
                tracing::warn!(%error, "could not relocalise the tray menu");
            }
            // Localised footer and notice rows can wrap differently. Most
            // language changes retain the same height, in which case this is
            // a no-op; a real change must reach the window server.
            resize_panel_if_height_changed(state, height_before)
        }

        UiEvent::Spend {
            label,
            fraction_of_cap,
        } => {
            state.settings.spend_label = label;
            state.settings.spend_fraction = fraction_of_cap;
            Task::none()
        }

        UiEvent::RecoveredFromCrash => {
            aibo_platform::announce_accessibility(i18n::t(
                crate::i18n::Key::ToastRecoveredFromCrash,
            ));
            state.panel.toast = Some(panel::ToastView {
                severity: ui_theme::Severity::Warning,
                body: i18n::t(crate::i18n::Key::ToastRecoveredFromCrash).to_owned(),
                offer_diagnostics: true,
            });
            resize_panel_if_visible(state)
        }

        UiEvent::DiagnosticsCopied => {
            state.panel.toast = Some(panel::ToastView {
                severity: ui_theme::Severity::Success,
                body: i18n::t(crate::i18n::Key::ToastDiagnosticsCopied).to_owned(),
                offer_diagnostics: false,
            });
            resize_panel_if_visible(state)
        }

        UiEvent::Copied => {
            // The announcement is unconditional — a screen reader gets the
            // confirmation whichever window the copy came from — but the toast
            // only makes sense over a visible panel; parking it on a hidden one
            // would surface a stale "Copied." at the next open.
            aibo_platform::announce_accessibility(i18n::t(crate::i18n::Key::ToastCopied));
            if state.panel_visible {
                state.panel.toast = Some(panel::ToastView {
                    severity: ui_theme::Severity::Success,
                    body: i18n::t(crate::i18n::Key::ToastCopied).to_owned(),
                    offer_diagnostics: false,
                });
                return resize_panel_if_visible(state);
            }
            Task::none()
        }

        UiEvent::OnboardingRequired => {
            tracing::debug!("opening settings for first-run onboarding");
            aibo_platform::announce_accessibility(i18n::t(crate::i18n::Key::SettingsWelcomeTitle));
            state.settings.onboarding = true;
            state.settings.section = settings::Section::Providers;
            open_settings(state)
        }

        UiEvent::OpenPanel => state.open_panel(),

        UiEvent::HistoryReady { recovery_code } => {
            state.settings.history_initializing = false;
            state.settings.history_failed = false;
            state.settings.history_ready = true;
            state.settings.recovery_code = recovery_code;
            state.settings.section = settings::Section::History;
            aibo_platform::announce_accessibility(i18n::t(
                if state.settings.recovery_code.is_some() {
                    crate::i18n::Key::SettingsRecoveryTitle
                } else {
                    crate::i18n::Key::SettingsHistoryReady
                },
            ));
            Task::none()
        }

        UiEvent::HistorySetupFailed => {
            state.settings.history_initializing = false;
            state.settings.history_failed = true;
            state.settings.section = settings::Section::History;
            aibo_platform::announce_accessibility(i18n::t(crate::i18n::Key::SettingsHistoryFailed));
            Task::none()
        }

        UiEvent::DictationStarted { turn } => {
            if turn != state.dictation_turn {
                return Task::none();
            }
            state.panel.dictating = true;
            state.dictation_stopping = false;
            // If the input already ends mid-word, the first delta gets one
            // space in front of it so speech never welds onto typed text.
            state.panel.dictation_pad =
                !state.panel.input.is_empty() && !state.panel.input.ends_with(char::is_whitespace);
            aibo_platform::announce_accessibility(i18n::t(crate::i18n::Key::ActionDictate));
            Task::none()
        }

        UiEvent::DictationDelta { turn, text } => {
            if turn != state.dictation_turn {
                return Task::none();
            }
            // Appended even just after a stop: the final fragments of a
            // committed turn arrive between `StopDictation` and
            // `DictationEnded`, and dropping them would eat the last words.
            let mut updated = state.panel.input.clone();
            if state.panel.dictation_pad {
                updated.push(' ');
                state.panel.dictation_pad = false;
            }
            updated.push_str(&text);
            state.panel.set_input(&updated);
            // Speech wraps into new composer lines; the window follows.
            resize_panel_if_visible(state)
        }

        UiEvent::DictationEnded { turn } => {
            if turn != state.dictation_turn {
                return Task::none();
            }
            state.panel.dictating = false;
            state.dictation_stopping = false;
            operation::focus(panel::INPUT_ID)
        }

        UiEvent::FileCandidates {
            session,
            generation,
            files,
        } => {
            if session != state.panel.session || generation != state.file_list_generation {
                return Task::none();
            }
            state.panel.file_finder.set_candidates(files);
            Task::none()
        }

        UiEvent::DirectoryCandidates { generation, dirs } => {
            if generation != state.directory_list_generation {
                return Task::none();
            }
            state.settings.root_completer.set_candidates(dirs);
            Task::none()
        }

        UiEvent::WorkdirCandidates {
            generation,
            recents,
            dirs,
        } => {
            if generation != state.workdir_list_generation {
                return Task::none();
            }
            state.panel.workdir_picker.recents = recents;
            state.panel.workdir_picker.dirs = dirs;
            state.panel.workdir_picker.highlight = 0;
            Task::none()
        }

        UiEvent::SkillCatalog { skills, dir } => {
            state.panel.skill_catalog = skills;
            state.panel.skills_dir = Some(dir);
            Task::none()
        }

        UiEvent::FileAttached {
            session,
            name,
            content,
        } => {
            if session != state.panel.session {
                return Task::none();
            }
            state.panel.attach_file_selection(content);
            let body = i18n::t1(crate::i18n::Key::ToastFileAttached, &name);
            aibo_platform::announce_accessibility(&body);
            state.panel.toast = Some(panel::ToastView {
                severity: ui_theme::Severity::Success,
                body,
                offer_diagnostics: false,
            });
            resize_panel_if_visible(state)
        }

        UiEvent::FileAttachFailed { session, name } => {
            if session != state.panel.session {
                return Task::none();
            }
            let body = i18n::t1(crate::i18n::Key::ToastFileAttachFailed, &name);
            aibo_platform::announce_accessibility(&body);
            state.panel.toast = Some(panel::ToastView {
                severity: ui_theme::Severity::Warning,
                body,
                offer_diagnostics: false,
            });
            resize_panel_if_visible(state)
        }

        UiEvent::PinnedModelsLoaded { pins } => {
            // Receipt implies the user curated this set — including empty.
            state.panel.favourite_models = pins;
            state.panel.pins_customised = true;
            sync_settings_models(state);
            Task::none()
        }

        UiEvent::SettingsLoaded {
            ax_tree_activation,
            file_roots,
            default_file_roots,
            budget,
            stt_backend,
            stt_end_on_send,
            auto_update,
            update_channel,
        } => {
            state.settings.ax_tree_activation = ax_tree_activation;
            state.settings.stt_end_on_send = stt_end_on_send;
            state.settings.stt_backend =
                crate::settings::SttChoice::from_tag(stt_backend.as_deref());
            state.settings.file_roots = file_roots;
            state.settings.default_file_roots = default_file_roots;
            state.settings.budget_configured = budget.is_some();
            state.settings.auto_update = auto_update;
            state.settings.update_channel = update_channel;
            if let Some((limit_micros, warn_at_percent, hard_stop)) = budget {
                // Micros back to whole units for the draft; trailing zeros
                // trimmed so "20" round-trips as "20", not "20.000000".
                let limit = limit_micros as f64 / 1_000_000.0;
                let mut draft = format!("{limit:.6}");
                while draft.ends_with('0') {
                    draft.pop();
                }
                if draft.ends_with('.') {
                    draft.pop();
                }
                state.settings.budget_limit_draft = draft;
                state.settings.budget_warn_draft = warn_at_percent.to_string();
                state.settings.budget_hard_stop = hard_stop;
            }
            Task::none()
        }

        UiEvent::UpdateState(update_state) => {
            let installing = update_state == crate::bridge::UpdateState::Installing;
            state.settings.update_state = update_state;
            if installing {
                state.send(UiRequest::Quit);
                iced::exit()
            } else {
                Task::none()
            }
        }

        UiEvent::DictationFailed { turn, failure } => {
            if turn != state.dictation_turn {
                return Task::none();
            }
            state.panel.dictating = false;
            state.dictation_stopping = false;
            let key = match failure {
                crate::bridge::DictationFailure::NoOpenAiKey => {
                    crate::i18n::Key::ToastDictationNoKey
                }
                crate::bridge::DictationFailure::Microphone => {
                    crate::i18n::Key::ToastDictationMicrophone
                }
                crate::bridge::DictationFailure::Connection => {
                    crate::i18n::Key::ToastDictationConnection
                }
            };
            aibo_platform::announce_accessibility(i18n::t(key));
            state.panel.toast = Some(panel::ToastView {
                severity: ui_theme::Severity::Warning,
                body: i18n::t(key).to_owned(),
                offer_diagnostics: false,
            });
            // `DictationEnded` gives the caret back; ending in failure is
            // still ending, and the user types what they meant to dictate.
            resize_panel_if_visible(state).chain(operation::focus(panel::INPUT_ID))
        }
    }
}

/// Put a deliberately selected file path at the caret's logical end.
///
/// `@` completion uses paths in both chat and agent mode. Keeping this as one
/// route means file format and mode cannot silently change what the gesture
/// inserts.
fn append_path_to_composer(panel: &mut panel::PanelState, path: &str) {
    let mut input = panel.input.clone();
    if !input.is_empty() && !input.ends_with(char::is_whitespace) {
        input.push(' ');
    }
    input.push_str(path);
    input.push(' ');
    panel.set_input(&input);
}

/// What the panel may offer to attach, from the capture's clipboard snapshot.
///
/// **This is the one place ambient clipboard state enters the panel, and it
/// produces an *offer*, never an attachment.** The distinction is the whole
/// point of the feature: `RouteInput::has_image` used to be derived from
/// whatever sat on the pasteboard, so taking any screenshot silently rerouted
/// every request to the `Vision` role, and because nothing binds that role the
/// failure surfaced as "No provider is configured yet" beside a signed-in,
/// healthy provider. An offer only decides whether ⌘V is enabled; it takes a
/// keypress to become an attachment and an attachment to change routing.
fn clipboard_offer(item: Option<&aibo_core::types::ClipboardItem>) -> panel::ClipboardOffer {
    use aibo_core::types::ClipboardKind;

    let Some(item) = item else {
        return panel::ClipboardOffer::Unknown;
    };
    // §12: concealed content is never recorded and never sent. That it happens
    // to be an image does not exempt it, and offering it would invite the user
    // to send exactly what the marker exists to withhold.
    if item.concealed || item.kind != ClipboardKind::ImageRef {
        return panel::ClipboardOffer::Nothing;
    }

    let label = match &item.source_app {
        Some(app) => i18n::t1(crate::i18n::Key::AttachmentClipboardFrom, app),
        None => i18n::t(crate::i18n::Key::AttachmentClipboardLabel).to_owned(),
    };

    panel::ClipboardOffer::Image {
        label,
        // TODO(§2, bridge): `ClipboardItem` describes the clipboard without
        // inlining it — `ClipboardKind::ImageRef` is documented as "referenced
        // rather than inlined" — so the capture that reaches the UI carries no
        // pixels. Materialising them needs a request/event pair in
        // `crate::bridge` plus a downscaling read in `aibo-platform`, neither
        // of which this change owns. Until then ⌘V says so (§13 toast) instead
        // of failing silently, and no path invents an attachment the user did
        // not make.
        image: None,
    }
}

/// Re-run placement so the panel's height follows its content.
///
/// **Geometry only.** This deliberately does not go through
/// [`Aibo::show_panel`], and the distinction is load-bearing rather than
/// stylistic. `show_panel` is the *arrival* sequence: it resizes, moves, takes
/// the window out of `Mode::Hidden`, applies the native overlay policy, gains
/// focus, and focuses the input. Every one of those is correct exactly once,
/// when the panel appears.
///
/// This function runs while the panel is **already on screen and streaming an
/// answer**, several times per response. Routing it through `show_panel` meant
/// re-issuing `set_mode`, `orderFrontRegardless`, `gain_focus` and
/// `operation::focus(INPUT_ID)` mid-answer: the window jumped, and the caret was
/// yanked back into the input while the user was reading — or worse, while they
/// were selecting the text they wanted to copy. Growing a window and presenting
/// a window are two different operations that happened to share a code path.
fn resize_panel_if_height_changed(state: &mut Aibo, height_before: f32) -> Task<Message> {
    if (state.panel.desired_height() - height_before).abs() >= 1.0 {
        resize_panel_if_visible(state)
    } else {
        Task::none()
    }
}

fn resize_panel_if_visible(state: &mut Aibo) -> Task<Message> {
    if !state.panel_visible {
        return Task::none();
    }
    // A resize is not a re-placement. Recomputing `placement()` wholesale here
    // re-anchored the panel on every height change — and the anchor moves: by
    // the time an answer grows the transcript, the caret capture that placed
    // the panel is long gone, so every submit visibly snapped the window back
    // to the fallback position. The corner stays where the user last saw it;
    // only the size follows the content, with the top edge pushed up just
    // enough when growth would walk off the display's visible frame.
    let fresh = state.placement();
    let placement = match state.last_placement {
        Some(previous) => {
            let (width, height) = fresh.size;
            let (x, mut y) = previous.position;
            if let Some(display) = state
                .displays
                .iter()
                .find(|display| display.id == previous.display_id)
            {
                let frame = display.visible_frame;
                #[expect(clippy::cast_possible_truncation, reason = "logical points fit f32")]
                {
                    let top = frame.y as f32;
                    let bottom = (frame.y + frame.height) as f32;
                    y = y.min(bottom - height).max(top);
                }
            }
            Placement {
                position: (x, y),
                size: (width, height),
                // §9 wants the scale factor re-read on every show and every
                // display change, and this is now one of the paths a display
                // change arrives on. Everything else — which display the panel
                // is on, whether it is caret-anchored — describes where the
                // panel already is, and is preserved with the position.
                scale_factor: fresh.scale_factor,
                ..previous
            }
        }
        None => fresh,
    };
    let previous = state.last_placement;
    state.last_placement = Some(placement);

    // The backdrop is synced even when the window itself has not moved: with
    // a menu open the window height is pinned above the chrome, so the chrome
    // can grow (a wrapped composer line, a dictation delta) without any
    // window-server geometry changing at all.
    let backdrop = sync_backdrop_to_chrome(state.panel_window, state.panel.chrome_height());
    // Every window-server call is a visible flicker on a transparent overlay,
    // so none is sent redundantly: nothing at all when geometry is unchanged,
    // and no `move_to` when only the size moved.
    if previous == Some(placement) {
        return backdrop;
    }
    let size = Size::new(placement.size.0, placement.size.1);
    if previous.is_some_and(|previous| previous.position == placement.position) {
        // A size-only change should travel through iced/winit so the renderer's
        // client bounds and AppKit's frame advance together. Calling AppKit
        // directly here briefly stretched the previous Metal frame, which is
        // what made text appear to resize with the window. The native atomic
        // frame path below remains necessary when growth also moves the panel
        // upward to keep it on-screen.
        return window::resize(state.panel_window, size).chain(backdrop);
    }
    // One native, atomic frame change instead of separate resize/move effects.
    // It is deliberately not animated: AppKit scales the last Metal frame
    // while interpolating bounds, which makes the text visibly grow and shrink.
    // The existing 48 pt height steps keep these instant updates infrequent.
    set_panel_geometry(state.panel_window, placement).chain(backdrop)
}

/// Pointer motion during a corner-grip drag.
///
/// The panel's top-left corner is the anchor — the grip is diagonally opposite
/// it — so the new size is the old size plus the cursor's travel, and no
/// `move_to` is involved at any point.
fn resize_drag_moved(state: &mut Aibo, position: Point) -> Task<Message> {
    let Some(drag) = state.panel_resize.as_mut() else {
        return Task::none();
    };
    // The first motion only establishes where the drag started from; treating
    // it as a delta would jump the panel by the cursor's offset inside the
    // grip.
    let Some(anchor) = drag.anchor else {
        drag.anchor = Some(position);
        return Task::none();
    };
    // Only the floors are applied here. The display ceiling belongs to
    // `placement`, which is the one place that knows which display the panel
    // is on — and clamping the stored size to it here would make a drag past
    // the edge and back again lose the size it started from.
    let size = (
        (drag.origin.0 + (position.x - anchor.x)).max(ui_theme::PANEL_WIDTH_MIN),
        (drag.origin.1 + (position.y - anchor.y)).max(ui_theme::PANEL_HEIGHT_COLLAPSED),
    );
    if state.panel.user_size == Some(size) {
        return Task::none();
    }
    state.panel.user_size = Some(size);
    let before = state.last_placement.map(|placement| placement.position);
    let task = resize_panel_if_visible(state);
    // Growing past the bottom of the display makes `resize_panel_if_visible`
    // walk the panel *upwards* to keep it on screen. The cursor positions in
    // these events are window-local, so a window that moved under a stationary
    // pointer reports motion that never happened — and since that phantom
    // motion is downward, it grows the panel, which moves the window again.
    // Carrying the move into the anchor is what stops that loop.
    let after = state.last_placement.map(|placement| placement.position);
    if let (Some(before), Some(after), Some(drag)) = (before, after, state.panel_resize.as_mut())
        && let Some(anchor) = drag.anchor
        && before != after
    {
        // A window origin that moved up by `d` makes a stationary pointer read
        // `d` further down the window, so the anchor moves down by `d` too:
        // `before - after`, not the other way round.
        drag.anchor = Some(Point::new(
            anchor.x + (before.0 - after.0),
            anchor.y + (before.1 - after.1),
        ));
    }
    task
}

/// The pointer was released: settle the size and write it once.
///
/// Persisting on release rather than on motion is the whole reason the drag is
/// stateful — a write per pointer event would rewrite `config.toml` a hundred
/// times across one gesture.
fn resize_drag_ended(state: &mut Aibo) -> Task<Message> {
    let Some(drag) = state.panel_resize.take() else {
        return Task::none();
    };
    // A press that never moved is a click on the corner, not a resize; it
    // changed nothing, so it writes nothing.
    if drag.anchor.is_none() {
        return Task::none();
    }
    // What is written is the size actually on screen, which `placement` may
    // have trimmed to the display — so the next launch restores what the user
    // was last looking at rather than a size the display never allowed.
    let size = state
        .last_placement
        .map(|placement| placement.size)
        .or(state.panel.user_size);
    state.panel.user_size = size;
    state.persist_panel_size(size)
}

/// Apply `placement` through the platform's atomic frame call, falling back to
/// iced's instant resize/move when the native path is unavailable.
fn set_panel_geometry(id: window::Id, placement: Placement) -> Task<Message> {
    window::run(id, move |window| {
        let animated = window
            .window_handle()
            .map_err(|error| error.to_string())
            .and_then(|handle| {
                aibo_platform::set_panel_frame(
                    handle,
                    f64::from(placement.position.0),
                    f64::from(placement.position.1),
                    f64::from(placement.size.0),
                    f64::from(placement.size.1),
                )
                .map_err(|error| error.to_string())
            });
        match animated {
            Ok(()) => Message::Ignored,
            Err(error) => {
                tracing::debug!(%error, "native frame update unavailable; iced resize");
                Message::PanelFrameFallback(placement)
            }
        }
    })
}

fn capture_screen_region_task() -> Task<Message> {
    Task::future(async {
        Message::ScreenRegionCaptured(
            aibo_platform::capture_screen_region()
                .await
                .map_err(|error| error.to_string()),
        )
    })
}

/// Apply the durable native overlay policy, or present the already-configured
/// panel without activating the source application.
fn configure_or_present_panel(id: window::Id, configure: bool) -> Task<Message> {
    window::run(id, move |window| {
        let result = window
            .window_handle()
            .map_err(|error| error.to_string())
            .and_then(|handle| {
                if configure {
                    aibo_platform::configure_panel_window(handle)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                } else {
                    aibo_platform::present_panel_without_activation(handle)
                        .map_err(|error| error.to_string())
                }
            });
        if let Err(error) = result {
            tracing::warn!(%error, configure, "native panel overlay operation failed");
        }
        Message::Ignored
    })
}

/// Pin the native backdrop to the panel's visible chrome.
///
/// Chained wherever the panel's geometry is sent to the window server: the
/// window may be taller than the chrome while a floating menu is open (§9),
/// and the blur must track the chrome, not the window. Sending it when the
/// height is unchanged is a cheap native no-op, which keeps this a blanket
/// rule instead of a state machine.
fn sync_backdrop_to_chrome(id: window::Id, height: f32) -> Task<Message> {
    let height = f64::from(height);
    window::run(id, move |window| {
        let result = window
            .window_handle()
            .map_err(|error| error.to_string())
            .and_then(|handle| {
                aibo_platform::set_panel_backdrop_height(handle, height)
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = result {
            tracing::warn!(%error, "backdrop height update failed");
        }
        Message::Ignored
    })
}

fn view(state: &Aibo, window: window::Id) -> Element<'_, Message> {
    match state.role_of(window) {
        Some(Role::Panel) | None => {
            panel::view(&state.panel, state.config.appearance).map(Message::Panel)
        }
        Some(Role::Settings) => settings::view(&state.settings).map(Message::Settings),
    }
}

fn title(state: &Aibo, window: window::Id) -> String {
    use crate::i18n::Key;
    match state.role_of(window) {
        Some(Role::Settings) => i18n::t(Key::SettingsTitle).to_owned(),
        _ => i18n::t(Key::AppName).to_owned(),
    }
}

fn theme_of(state: &Aibo, _window: window::Id) -> Theme {
    state.config.appearance.iced_theme()
}

fn subscription(state: &Aibo) -> Subscription<Message> {
    let mut subscriptions = vec![
        Subscription::run(shell_event_stream),
        Subscription::run(backend_event_stream),
        Subscription::run(accessibility_event_stream),
        window::close_events().map(Message::WindowClosed),
        // §9: recompute on scale-factor and size changes rather than caching
        // the value from creation — that is the "blurry on the second monitor"
        // bug. Dropping these events on the floor, which is what this
        // subscription used to do, is the same as caching.
        window::resize_events().map(|(id, size)| Message::AccessibilityResize(id, size)),
        // Window-aware keyboard routing. Ordinary shortcuts are handled only
        // when the focused widget ignored them, preventing a focused button or
        // text selection from firing twice. Attach and empty-input backspace are
        // the two deliberate exceptions: the panel input consumes those before
        // a normal listener sees them.
        iced::event::listen_with(|event, status, window| {
            use iced::keyboard::Key;
            use iced::keyboard::key::{Code, Named, NativeCode, Physical};

            if let iced::Event::Window(event) = event {
                return match event {
                    iced::window::Event::Focused => Some(Message::AccessibilityFocus(window, true)),
                    iced::window::Event::Unfocused => {
                        Some(Message::AccessibilityFocus(window, false))
                    }
                    iced::window::Event::Rescaled(scale) => {
                        Some(Message::AccessibilityScale(window, scale))
                    }
                    _ => None,
                };
            }

            if let iced::Event::InputMethod(event) = event {
                let active = match event {
                    iced::advanced::input_method::Event::Preedit(text, _) => !text.is_empty(),
                    iced::advanced::input_method::Event::Commit(_)
                    | iced::advanced::input_method::Event::Closed => false,
                    iced::advanced::input_method::Event::Opened => return None,
                };
                return Some(Message::ImePreedit(window, active));
            }

            if let iced::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(modifiers)) = event
            {
                return Some(Message::CommandHeld(modifiers.command()));
            }

            let iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                ..
            }) = event
            else {
                return None;
            };

            if matches!(key.as_ref(), Key::Named(Named::Backspace)) && !modifiers.command() {
                return Some(Message::WindowKey(window, WindowChord::DetachLast));
            }
            if modifiers.command() && key.to_latin(physical_key) == Some('v') {
                return Some(Message::WindowKey(window, WindowChord::Attach));
            }
            if modifiers.command() && key.to_latin(physical_key) == Some('e') {
                return Some(Message::WindowKey(
                    window,
                    if modifiers.shift() {
                        WindowChord::RemoveSelection
                    } else {
                        WindowChord::ToggleContext
                    },
                ));
            }
            // A focused `text_input` captures every Enter variant. Route them
            // here before the captured-event gate so exactly one semantic
            // action fires: plain Enter submits, Command-Enter replaces, and
            // Command-Shift-Enter retries with Smart. `window_shortcut` blocks
            // these while an IME preedit is active.
            let is_enter = matches!(
                key.as_ref(),
                Key::Named(Named::Enter) | Key::Character("\r" | "\n")
            ) || matches!(
                physical_key,
                Physical::Code(Code::Enter | Code::NumpadEnter)
                    | Physical::Unidentified(NativeCode::MacOS(36 | 76))
            );
            if is_enter {
                tracing::debug!(
                    ?key,
                    ?physical_key,
                    ?modifiers,
                    ?status,
                    "routing panel Enter shortcut"
                );
                return Some(Message::WindowKey(
                    window,
                    WindowChord::Enter {
                        command: modifiers.command(),
                        shift: modifiers.shift(),
                    },
                ));
            }
            // **Command chords are extracted before the `Captured` check, and
            // only these.**
            //
            // A focused `text_input` captures every key event it sees, including
            // ⌘-modified ones. Bailing on `Captured` therefore meant ⌘K, ⌘R, ⌘T
            // and ⌘, were dead whenever the panel input had focus — which is
            // always, since the panel focuses its input on open. Pressing ⌘K
            // typed a literal "k".
            //
            // The list is deliberately short: ⌘C, ⌘V, ⌘X, ⌘A and ⌘Z are *text
            // editing* commands, and stealing them would break copy and paste
            // inside the field the user is typing in. These five have no meaning
            // in a text field, so intercepting them takes nothing away.
            if modifiers.command()
                && let Some(latin) = key.to_latin(physical_key)
            {
                let chord = match latin {
                    'k' => Some(WindowChord::PickModel),
                    'd' => Some(WindowChord::PinModel),
                    'r' => Some(WindowChord::Retry),
                    't' => Some(WindowChord::ShowTask),
                    'l' => Some(WindowChord::Dictate),
                    'j' => Some(WindowChord::AgentMode),
                    ',' => Some(WindowChord::OpenSettings),
                    _ => None,
                };
                if let Some(chord) = chord {
                    return Some(Message::WindowKey(window, chord));
                }
            }

            if status == iced::event::Status::Captured {
                return None;
            }

            let chord = match key.as_ref() {
                Key::Named(Named::Escape) => WindowChord::Escape,
                Key::Named(Named::Enter) => WindowChord::Enter {
                    command: modifiers.command(),
                    shift: modifiers.shift(),
                },
                Key::Named(Named::Tab) => WindowChord::NextLane,
                Key::Named(Named::ArrowUp) => WindowChord::HistoryOlder,
                Key::Named(Named::ArrowDown) => WindowChord::HistoryNewer,
                // ⌘C stays here rather than above: inside a text field it is
                // the field's copy, and only an *uncaptured* ⌘C means "copy the
                // answer".
                _ if modifiers.command() => match key.to_latin(physical_key) {
                    Some('c') => WindowChord::Copy,
                    Some('.') => WindowChord::CancelTask,
                    Some('n') => WindowChord::New,
                    _ => return None,
                },
                _ => return None,
            };
            Some(Message::WindowKey(window, chord))
        }),
    ];

    // Frames are only interesting while the panel is warming up; subscribing
    // permanently would wake the app 60 times a second for nothing (§15).
    if matches!(state.panel.phase, Phase::WarmingUp { .. }) {
        subscriptions.push(window::frames().map(|_| Message::FramePainted));
    }

    // The shortcut recorder, live only while it is listening: a raw key
    // subscription is exactly the thing that must not be permanently
    // installed, since it sees every keystroke in the app.
    if state.settings.hotkey_recording
        && state.settings_window.is_some()
        && state.settings.section == settings::Section::Permissions
    {
        subscriptions.push(iced::event::listen_with(|event, _status, _window| {
            let iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                physical_key,
                modifiers,
                ..
            }) = event
            else {
                return None;
            };
            if matches!(
                physical_key,
                iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Escape)
            ) {
                return Some(Message::Settings(settings::Message::HotkeyRecord(false)));
            }
            captured_hotkey(physical_key, modifiers)
                .map(|spec| Message::Settings(settings::Message::HotkeyCaptured(spec)))
        }));
    }

    // Pointer motion, live only while the corner grip is held — for the same
    // reason as the recorder above. A permanent cursor subscription turns
    // every mouse movement over the panel into an `update` and a re-render,
    // which on a panel rendering markdown is the most expensive thing the app
    // could do with an idle pointer.
    if state.panel_resize.is_some() {
        subscriptions.push(iced::event::listen_with(|event, _status, _window| {
            match event {
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Some(Message::PanelResizeMoved(position))
                }
                // Deliberately not `CursorLeft`: growing the panel outruns the
                // window it is growing, so the pointer leaves the old bounds
                // constantly during a normal drag. Both backends deliver the
                // release to the window that took the press, so the release is
                // the only end condition needed — and `hide_panel` covers the
                // panel going away mid-gesture.
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )) => Some(Message::PanelResizeEnded),
                _ => None,
            }
        }));
    }

    Subscription::batch(subscriptions)
}

/// One key press as a `hotkey::parse` spec, or `None` if it cannot be one.
///
/// Rejected without comment: a bare modifier (the user is still reaching for
/// the real key), and anything whose physical code aibo cannot name. Escape
/// is *accepted as a cancel* by the caller rather than bound, because a
/// shortcut of Escape alone would be unusable.
fn captured_hotkey(
    physical_key: iced::keyboard::key::Physical,
    modifiers: iced::keyboard::Modifiers,
) -> Option<String> {
    use iced::keyboard::key::{Code, Physical};

    let Physical::Code(code) = physical_key else {
        return None;
    };
    // Winit's `KeyCode` debug spelling is the UI Events code name — `KeyA`,
    // `Space`, `Digit1`, `F5` — which is what the hotkey parser reads.
    let key = format!("{code:?}");
    if matches!(
        code,
        Code::ControlLeft
            | Code::ControlRight
            | Code::AltLeft
            | Code::AltRight
            | Code::ShiftLeft
            | Code::ShiftRight
            | Code::SuperLeft
            | Code::SuperRight
    ) {
        return None;
    }
    if code == Code::Escape {
        // Cancels the recorder; `HotkeyRecord(false)` is the honest message.
        return None;
    }

    let mut parts: Vec<&str> = Vec::new();
    if modifiers.control() {
        parts.push("control");
    }
    if modifiers.alt() {
        parts.push("alt");
    }
    if modifiers.shift() {
        parts.push("shift");
    }
    if modifiers.logo() {
        parts.push("super");
    }
    // A global shortcut with no modifier would swallow that key everywhere.
    if parts.is_empty() {
        return None;
    }
    parts.push(&key);
    let spec = parts.join("+");
    // Only offer what the registrar can actually take.
    crate::hotkey::parse(&spec).ok().map(|_| spec)
}

fn shell_event_stream() -> impl iced::futures::Stream<Item = Message> {
    let receiver = SHELL_EVENTS.lock().ok().and_then(|mut slot| slot.take());
    iced::futures::stream::unfold(receiver, |mut receiver| async move {
        let channel = receiver.as_mut()?;
        let event = channel.recv().await?;
        let message = match event {
            ShellEvent::Hotkey(id) => Message::Hotkey(id),
            ShellEvent::Tray(command) => Message::Tray(command),
        };
        Some((message, receiver))
    })
}

fn backend_event_stream() -> impl iced::futures::Stream<Item = Message> {
    let receiver = BACKEND_EVENTS.lock().ok().and_then(|mut slot| slot.take());
    iced::futures::stream::unfold(receiver, |mut receiver| async move {
        let channel = receiver.as_mut()?;
        let event = channel.recv().await?;
        Some((Message::Backend(Box::new(event)), receiver))
    })
}

fn accessibility_event_stream() -> impl iced::futures::Stream<Item = Message> {
    let receiver = ACCESSIBILITY_EVENTS
        .lock()
        .ok()
        .and_then(|mut slot| slot.take());
    iced::futures::stream::unfold(receiver, |mut receiver| async move {
        let channel = receiver.as_mut()?;
        let event = channel.recv().await?;
        Some((Message::Accessibility(event), receiver))
    })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the aibo shell. Blocks until the daemon exits.
///
/// Must be called on the **main thread**: winit requires it, and so do
/// `tray-icon` and `global-hotkey` on macOS.
///
/// The daemon opens no window of its own beyond the hidden panel and does not
/// exit when windows close (§6) — only [`UiRequest::Quit`] ends it.
pub fn run(config: UiConfig, handles: UiHandles) -> Result<()> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err(UiError::AlreadyRunning);
    }

    // Park the backend receiver for the subscription to claim.
    *BACKEND_EVENTS
        .lock()
        .map_err(|_| UiError::Runtime("event channel poisoned".to_owned()))? = Some(handles.events);

    // The OS installs *global* handlers for hotkeys and tray menus, so their
    // sink lives for the process lifetime rather than in app state.
    let (shell_tx, shell_rx) = tokio::sync::mpsc::channel(SHELL_EVENT_CHANNEL_CAPACITY);
    *SHELL_EVENTS
        .lock()
        .map_err(|_| UiError::Runtime("shell channel poisoned".to_owned()))? = Some(shell_rx);
    let _ = SHELL_SENDER.set(shell_tx);

    if let Some(sender) = SHELL_SENDER.get() {
        let hotkey_sink = sender.clone();
        hotkey::forward_events(move |id| {
            send_shell_event(&hotkey_sink, ShellEvent::Hotkey(id));
        });

        let tray_sink = sender.clone();
        tray::forward_events(move |command| {
            send_shell_event(&tray_sink, ShellEvent::Tray(command));
        });
    }

    let requests = handles.requests;
    let boot_config = config.clone();

    /// The window fill, beneath every widget.
    ///
    /// **Must be fully transparent.** The panel window is `transparent: true`
    /// and undecorated (§9), and its rounded corners come from the
    /// `panel_surface` container drawn *inside* it. iced's default daemon style
    /// fills the whole window rect with `palette.background` — an opaque dark
    /// surface — so the container's 18 pt radius was drawn correctly and then
    /// framed by four black square corners. The border looked rounded; the
    /// panel looked rectangular.
    ///
    /// Leaving this to the default is also why a translucent or vibrant
    /// backdrop could never work: an opaque fill sits between the desktop and
    /// the panel.
    fn app_style(_state: &Aibo, theme: &iced::Theme) -> iced::theme::Style {
        iced::theme::Style {
            background_color: iced::Color::TRANSPARENT,
            text_color: theme.palette().text,
        }
    }

    iced::daemon(
        move || boot(boot_config.clone(), requests.clone()),
        update,
        view,
    )
    .title(title)
    .theme(theme_of)
    .style(app_style)
    // Make unannotated labels use the same JP-safe face as rich text and
    // editors. Without this, macOS can resolve Han glyphs through PingFang
    // even though Japanese is the active UI language.
    .default_font(ui_theme::UI_FONT)
    .subscription(subscription)
    .run()
    .map_err(|error| UiError::Runtime(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aibo_core::types::{
        AppInfo, AppRef, ClipboardKind, FieldContext, ModelBinding, ProviderId, StopReason, Usage,
    };

    fn app() -> Aibo {
        let (requests, _rx) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (state, _task) = boot(UiConfig::default(), requests);
        state
    }

    fn field(ime_active: bool) -> FieldContext {
        FieldContext {
            prefix: "the deployment should be".to_owned(),
            suffix: String::new(),
            caret: None,
            label: None,
            is_secure: false,
            ime_active,
            truncated: false,
            caret_bounds: None,
        }
    }

    fn app_info() -> AppInfo {
        AppInfo {
            app_ref: AppRef {
                pid: 1,
                window: None,
            },
            identifier: "com.google.Chrome".to_owned(),
            display_name: "Chrome".to_owned(),
            is_code_app: false,
        }
    }

    fn model_option(model: &str) -> crate::bridge::ModelOption {
        crate::bridge::ModelOption {
            binding: ModelBinding {
                provider: ProviderId::CODEX,
                model: model.to_owned(),
            },
            display_name: model.to_owned(),
            latency_ms: Some(435),
            released_at: None,
            abilities: Default::default(),
            cost: None,
        }
    }

    /// The slash popup is a *completion*, not a mode: typing `/` must open
    /// it and leave the composer holding the keyboard, and every further
    /// keystroke must keep filtering it (owner report, 2026-08-02: "the
    /// focus changes to the completion menu, which makes it impossible to
    /// input anything").
    #[test]
    fn typing_a_slash_command_never_leaves_the_composer() {
        use iced::widget::text_editor::{Action, Edit};

        let (requests, _received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        state.panel.phase = Phase::Idle;

        let _ = panel_update(
            &mut state,
            panel::Message::InputEdited(Action::Edit(Edit::Insert('/'))),
        );
        assert!(state.panel.slash.open, "`/` opens the completion");
        assert_eq!(state.panel.input, "/", "and the character is still typed");

        // Every following keystroke narrows the list rather than being eaten
        // by it.
        for c in ['h', 'e'] {
            let _ = panel_update(
                &mut state,
                panel::Message::InputEdited(Action::Edit(Edit::Insert(c))),
            );
        }
        assert_eq!(state.panel.input, "/he");
        assert!(state.panel.slash.open);
        assert_eq!(crate::slash::matches(&state.panel.input).len(), 1);

        // A space begins arguments: the popup closes and typing continues.
        let _ = panel_update(
            &mut state,
            panel::Message::InputEdited(Action::Edit(Edit::Insert(' '))),
        );
        assert!(!state.panel.slash.open, "arguments close the completion");
        assert_eq!(state.panel.input, "/he ");
    }

    /// The agent-mode toggle (owner, 2026-08-01): ⌘J makes submissions Do,
    /// the mode is visible in state, and a new chat clears it — a toggle
    /// forgotten yesterday must not make today's first question agentic.
    #[test]
    fn agent_mode_routes_submissions_to_do_and_dies_with_the_session() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        state.panel.phase = Phase::Idle;

        let _ = panel_update(&mut state, panel::Message::ToggleAgentMode);
        assert!(state.panel.agent_mode);
        assert!(
            matches!(received.try_recv(), Ok(UiRequest::ListWorkdirs { .. })),
            "agent mode warms the workdir candidates for the chip"
        );

        state.panel.set_input("tidy the downloads folder");
        let _ = panel_update(&mut state, panel::Message::Submit);
        let sent = received.try_recv().expect("a submission");
        assert!(
            matches!(
                sent,
                UiRequest::Submit {
                    surface: Surface::Do,
                    ..
                }
            ),
            "{sent:?}"
        );

        state.panel.reset(SessionId::from_u128(2));
        assert!(
            !state.panel.agent_mode,
            "the toggle is session-scoped by design"
        );
    }

    /// The soft lock from the owner's screenshot (2026-08-01): an agent-mode
    /// submission must not open a chat turn the stream will never resolve —
    /// the panel stays Idle and typing continues to work. And a `/agent`
    /// submission, which the panel cannot recognise, is taken back the
    /// moment `TaskStarted` claims it.
    #[test]
    fn an_agent_submission_never_strands_the_panel_in_loading() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        state.panel.phase = Phase::Idle;

        let _ = panel_update(&mut state, panel::Message::ToggleAgentMode);
        assert!(
            matches!(received.try_recv(), Ok(UiRequest::ListWorkdirs { .. })),
            "agent mode warms the workdir candidates for the chip"
        );
        state.panel.set_input("hello");
        let _ = panel_update(&mut state, panel::Message::Submit);
        assert!(matches!(received.try_recv(), Ok(UiRequest::Submit { .. })));
        assert!(
            matches!(state.panel.phase, Phase::Idle),
            "no spinner for a run the task window narrates"
        );
        assert!(state.panel.input.is_empty(), "the composer is consumed");

        // The `/agent` path: the panel began an ordinary turn...
        let _ = panel_update(&mut state, panel::Message::ToggleAgentMode);
        state.panel.set_input("/agent do something");
        let _ = panel_update(&mut state, panel::Message::Submit);
        assert!(matches!(state.panel.phase, Phase::Loading));
        // ...and TaskStarted takes it back.
        let session = state.panel.session;
        let _ = backend_update(
            &mut state,
            UiEvent::TaskStarted {
                task: uuid::Uuid::now_v7(),
                session,
                instruction: "do something".to_owned(),
            },
        );
        assert!(matches!(state.panel.phase, Phase::Idle), "turn retracted");
        assert!(state.panel.handed_off_to_task);
    }

    /// The settings-coverage rules that must not regress: editing the root
    /// list materialises the defaults first (so the list on screen and the
    /// list persisted agree), every edit is sent for persistence, and reset
    /// returns to "no configuration" rather than to a frozen copy of it.
    #[test]
    fn editing_finder_roots_materialises_defaults_and_persists_each_step() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        let _ = backend_update(
            &mut state,
            UiEvent::SettingsLoaded {
                ax_tree_activation: false,
                file_roots: None,
                default_file_roots: vec!["/d/Documents".to_owned(), "/d/Desktop".to_owned()],
                budget: None,
                stt_backend: None,
                stt_end_on_send: true,
                auto_update: true,
                update_channel: crate::bridge::UpdateChannel::Stable,
            },
        );

        let _ = settings_update(&mut state, settings::Message::RootRemove(0));
        assert_eq!(
            state.settings.file_roots,
            Some(vec!["/d/Desktop".to_owned()]),
            "removing a default must start from the default list, not empty"
        );
        assert!(matches!(
            received.try_recv(),
            Ok(UiRequest::SetFileRoots { roots: Some(roots), .. }) if roots == ["/d/Desktop"]
        ));

        let _ = settings_update(&mut state, settings::Message::RootDraft("~/dev".to_owned()));
        assert!(matches!(
            received.try_recv(),
            Ok(UiRequest::ListDirectories { parent, .. }) if parent == "~/"
        ));
        let _ = settings_update(&mut state, settings::Message::RootAdd);
        assert_eq!(
            state.settings.file_roots,
            Some(vec!["/d/Desktop".to_owned(), "~/dev".to_owned()])
        );
        assert!(
            state.settings.root_draft.is_empty(),
            "the draft is consumed"
        );
        assert!(matches!(
            received.try_recv(),
            Ok(UiRequest::SetFileRoots { roots: Some(roots), .. })
                if roots == ["/d/Desktop", "~/dev"]
        ));

        let _ = settings_update(&mut state, settings::Message::RootsReset);
        assert_eq!(state.settings.file_roots, None);
        assert!(matches!(
            received.try_recv(),
            Ok(UiRequest::SetFileRoots { roots: None, .. })
        ));
    }

    #[test]
    fn each_update_choice_is_persisted_and_checked() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);

        for channel in [
            crate::bridge::UpdateChannel::Stable,
            crate::bridge::UpdateChannel::Nightly,
        ] {
            let _ = settings_update(&mut state, settings::Message::SetUpdateChannel(channel));
            assert!(matches!(
                received.try_recv(),
                Ok(UiRequest::SetUpdatePreferences { channel: saved, .. }) if saved == channel
            ));
            assert!(matches!(
                received.try_recv(),
                Ok(UiRequest::CheckForUpdates)
            ));
        }
    }

    #[test]
    fn stale_persistence_failures_cannot_overwrite_newer_file_root_edits() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        state.settings.file_roots = Some(vec!["/a".to_owned()]);

        state.settings.root_draft = "/b".to_owned();
        let _ = settings_update(&mut state, settings::Message::RootAdd);
        let first = match received.try_recv().expect("first write") {
            UiRequest::SetFileRoots { generation, .. } => generation,
            other => panic!("unexpected request: {other:?}"),
        };
        state.settings.root_draft = "/c".to_owned();
        let _ = settings_update(&mut state, settings::Message::RootAdd);
        let second = match received.try_recv().expect("second write") {
            UiRequest::SetFileRoots { generation, .. } => generation,
            other => panic!("unexpected request: {other:?}"),
        };

        let _ = backend_update(
            &mut state,
            UiEvent::PersistenceResult {
                scope: PersistenceScope::FileRoots,
                generation: first,
                succeeded: false,
            },
        );
        assert_eq!(
            state.settings.file_roots,
            Some(vec!["/a".to_owned(), "/b".to_owned(), "/c".to_owned()])
        );

        let _ = backend_update(
            &mut state,
            UiEvent::PersistenceResult {
                scope: PersistenceScope::FileRoots,
                generation: second,
                succeeded: false,
            },
        );
        assert_eq!(
            state.settings.file_roots,
            Some(vec!["/a".to_owned(), "/b".to_owned()])
        );
    }

    #[test]
    fn failed_provider_save_restores_the_secret_draft() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        let mut draft = settings::ProviderDraft::new(settings::Backend::OpenAi);
        draft.set_key("sk-test-only".to_owned());
        state.settings.draft = Some(draft);

        let _ = settings_update(&mut state, settings::Message::DraftSave);
        let generation = match received.try_recv().expect("provider write") {
            UiRequest::SetProviderKey { generation, .. } => generation,
            other => panic!("unexpected request: {other:?}"),
        };
        assert!(state.settings.draft.is_none());

        let _ = backend_update(
            &mut state,
            UiEvent::PersistenceResult {
                scope: PersistenceScope::Provider,
                generation,
                succeeded: false,
            },
        );
        assert_eq!(
            state.settings.draft.as_ref().map(|draft| draft.key_field()),
            Some("sk-test-only")
        );
    }

    #[test]
    fn an_unparseable_hotkey_marks_the_draft_and_persists_nothing() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        let _ = settings_update(
            &mut state,
            settings::Message::HotkeyDraft("bogus+++nope".to_owned()),
        );
        let _ = settings_update(&mut state, settings::Message::HotkeyApply);
        assert!(state.settings.hotkey_draft_invalid);
        assert!(
            received.try_recv().is_err(),
            "a rejected spec must never reach config.toml"
        );
    }

    #[test]
    fn shortcut_editor_persists_the_selected_action() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        let _ = settings_update(
            &mut state,
            settings::Message::HotkeySelect(HotkeyAction::ShowTasks),
        );
        let _ = settings_update(
            &mut state,
            settings::Message::HotkeyDraft("control+shift+Space".to_owned()),
        );
        let _ = settings_update(&mut state, settings::Message::HotkeyApply);

        assert!(matches!(
            received.try_recv(),
            Ok(UiRequest::SetHotkey {
                action: HotkeyAction::ShowTasks,
                spec: Some(spec),
                ..
            }) if spec == "control+shift+Space"
        ));
    }

    #[test]
    fn shortcut_reset_removes_only_the_selected_override() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        let _ = settings_update(
            &mut state,
            settings::Message::HotkeySelect(HotkeyAction::CaptureScreenRegion),
        );
        state.settings.hotkey_draft = "control+shift+Space".to_owned();

        let _ = settings_update(&mut state, settings::Message::HotkeyReset);

        assert!(state.settings.hotkey_draft.is_empty());
        assert!(matches!(
            received.try_recv(),
            Ok(UiRequest::SetHotkey {
                action: HotkeyAction::CaptureScreenRegion,
                spec: None,
                ..
            })
        ));
    }

    #[test]
    fn an_applied_budget_sends_micros_and_remove_clears_the_meter() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        let _ = settings_update(
            &mut state,
            settings::Message::BudgetLimitDraft("15".to_owned()),
        );
        let _ = settings_update(&mut state, settings::Message::BudgetApply);
        assert!(state.settings.budget_configured);
        assert!(matches!(
            received.try_recv(),
            Ok(UiRequest::SetMonthlyBudget {
                limit_micros: Some(15_000_000),
                warn_at_percent: 80,
                hard_stop: false,
                ..
            })
        ));

        state.settings.spend_fraction = Some(0.4);
        let _ = settings_update(&mut state, settings::Message::BudgetRemove);
        assert!(!state.settings.budget_configured);
        assert_eq!(state.settings.spend_fraction, None);
        assert!(matches!(
            received.try_recv(),
            Ok(UiRequest::SetMonthlyBudget {
                limit_micros: None,
                ..
            })
        ));
    }

    #[test]
    fn popup_model_selection_uses_the_validated_runtime_request() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        let selected = model_option("gpt-5.6-terra");
        let _ = backend_update(
            &mut state,
            UiEvent::ModelOptions {
                options: vec![model_option("gpt-5.5"), selected.clone()],
                selected: Some(selected.binding.clone()),
            },
        );
        assert_eq!(state.panel.selected_model, Some(selected.clone()));

        state.panel.phase = Phase::Idle;
        let _ = panel_update(&mut state, panel::Message::SelectModel(selected.clone()));
        assert!(matches!(
            received.try_recv(),
            Ok(UiRequest::SetModel { binding, .. }) if binding == selected.binding
        ));

        state.panel.phase = Phase::Streaming;
        let _ = panel_update(
            &mut state,
            panel::Message::SelectModel(model_option("gpt-5.5")),
        );
        assert!(
            received.try_recv().is_err(),
            "changing models must not cancel an active response"
        );
    }

    /// The ⌘-fallout guard drops exactly the one-character edit a leaked
    /// shortcut produces, and nothing else.
    #[test]
    fn command_fallout_is_a_single_char_insertion() {
        assert!(is_single_char_insertion("", "l"));
        assert!(is_single_char_insertion("call", "calll"));
        assert!(is_single_char_insertion("こんにち", "こんにちは"));
        assert!(is_single_char_insertion("ab", "alb"));

        assert!(!is_single_char_insertion("call", "call"));
        assert!(!is_single_char_insertion("call", "cal"));
        assert!(!is_single_char_insertion("", "こんにちは"));
        assert!(!is_single_char_insertion("ab", "cd"));
    }

    /// Deleting a credential is irreversible, so one press must never do it:
    /// the first arms, the second sends, and navigating away stands down.
    #[test]
    fn forgetting_a_provider_takes_two_presses() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        let provider = ProviderId::OPENAI;

        let _ = settings_update(
            &mut state,
            settings::Message::ForgetProvider(provider.clone()),
        );
        assert_eq!(state.settings.forget_armed, Some(provider.clone()));
        assert!(received.try_recv().is_err(), "the first press only arms");

        let _ = settings_update(
            &mut state,
            settings::Message::ForgetProvider(provider.clone()),
        );
        assert!(state.settings.forget_armed.is_none());
        assert!(matches!(
            received.try_recv(),
            Ok(UiRequest::RemoveProvider { id, .. }) if id == provider.as_str()
        ));

        // Arm again, then navigate away: the armed state must not survive to
        // go off when the user comes back.
        let _ = settings_update(
            &mut state,
            settings::Message::ForgetProvider(provider.clone()),
        );
        let _ = settings_update(
            &mut state,
            settings::Message::Select(settings::Section::About),
        );
        assert!(state.settings.forget_armed.is_none());
    }

    #[test]
    fn request_saturation_preserves_a_bounded_critical_tail() {
        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (state, _task) = boot(UiConfig::default(), requests);
        let session = Uuid::now_v7();

        for _ in 0..(UI_REQUEST_CHANNEL_CAPACITY * 4) {
            state.send(UiRequest::UiReady);
        }
        state.send(UiRequest::Cancel { session });
        for _ in 0..UI_REQUEST_CHANNEL_CAPACITY {
            state.send(UiRequest::Cancel { session });
        }

        let mut queued = Vec::new();
        while let Ok(request) = received.try_recv() {
            queued.push(request);
        }

        assert_eq!(queued.len(), UI_REQUEST_CHANNEL_CAPACITY);
        assert_eq!(
            queued
                .iter()
                .filter(|request| matches!(request, UiRequest::UiReady))
                .count(),
            UI_REQUEST_CHANNEL_CAPACITY - UI_REQUEST_CRITICAL_RESERVE
        );
        assert!(
            queued
                .iter()
                .any(|request| matches!(request, UiRequest::Cancel { session: queued } if *queued == session)),
            "ordinary input must not consume the capacity reserved for cancellation"
        );
    }

    #[test]
    fn shell_saturation_keeps_quit_deliverable() {
        let (sender, mut received) = tokio::sync::mpsc::channel(SHELL_EVENT_CHANNEL_CAPACITY);
        for id in 0..(SHELL_EVENT_CHANNEL_CAPACITY * 4) {
            send_shell_event(&sender, ShellEvent::Hotkey(id as u32));
        }
        send_shell_event(&sender, ShellEvent::Tray(TrayCommand::Quit));

        let mut queued = Vec::new();
        while let Ok(event) = received.try_recv() {
            queued.push(event);
        }

        assert_eq!(queued.len(), SHELL_EVENT_CHANNEL_CAPACITY);
        assert_eq!(
            queued
                .iter()
                .filter(|event| matches!(event, ShellEvent::Hotkey(_)))
                .count(),
            SHELL_EVENT_CHANNEL_CAPACITY - SHELL_EVENT_CRITICAL_RESERVE
        );
        assert!(
            queued
                .iter()
                .any(|event| matches!(event, ShellEvent::Tray(TrayCommand::Quit)))
        );
    }

    #[test]
    fn a_fresh_install_opens_the_functional_provider_setup() {
        let mut state = app();
        let task = backend_update(&mut state, UiEvent::OnboardingRequired);
        assert!(state.settings.onboarding);
        assert_eq!(state.settings.section, settings::Section::Providers);
        assert!(state.settings_window.is_some());
        assert!(task.units() > 0);
    }

    /// The panel is `AlwaysOnTop` and the settings window is not, so with both
    /// on screen settings always sits behind the panel — unreachable without
    /// dragging it out from underneath (owner report, 2026-08-02). Opening
    /// settings therefore hides the panel; the session survives and the hotkey
    /// brings it back.
    #[test]
    fn opening_settings_hides_the_panel_instead_of_covering_the_window() {
        let mut state = app();
        state.panel_visible = true;

        let _ = open_settings(&mut state);

        assert!(!state.panel_visible, "settings must not open behind aibo");
        assert!(state.settings_window.is_some());

        // Re-opening while the settings window already exists takes the same
        // path: focus settings, panel out of the way.
        state.panel_visible = true;
        let _ = open_settings(&mut state);
        assert!(!state.panel_visible);
    }

    #[test]
    fn a_history_recovery_code_is_discarded_when_settings_closes() {
        let mut state = app();
        let _ = open_settings(&mut state);
        let settings_window = state.settings_window.expect("settings window");
        let _ = backend_update(
            &mut state,
            UiEvent::HistoryReady {
                recovery_code: Some(secrecy::SecretString::from(
                    "alpha-bravo-charlie-delta".to_owned(),
                )),
            },
        );
        assert!(state.settings.recovery_code.is_some());
        state.settings.hotkey_recording = true;

        let _ = update(&mut state, Message::WindowClosed(settings_window));
        assert!(state.settings.recovery_code.is_none());
        assert!(!state.settings.hotkey_recording);
    }

    #[test]
    fn stale_file_and_dictation_results_cannot_mutate_a_new_turn() {
        let mut state = app();
        state.file_list_generation = 2;
        state.dictation_turn = 7;
        state.panel.input = "typed".to_owned();
        let session = state.panel.session;

        let _ = backend_update(
            &mut state,
            UiEvent::FileCandidates {
                session,
                generation: 1,
                files: vec![crate::bridge::FileCandidate {
                    display: "stale.txt".to_owned(),
                    path: "/stale.txt".to_owned(),
                }],
            },
        );
        let _ = backend_update(
            &mut state,
            UiEvent::DictationDelta {
                turn: 6,
                text: " stale speech".to_owned(),
            },
        );

        assert!(state.panel.file_finder.results().is_empty());
        assert_eq!(state.panel.input, "typed");
    }

    /// Selecting an `@` result is completion, not submission. Windows used to
    /// deliver Enter to both the finder's `on_submit` handler and the global
    /// window shortcut: the first event closed the finder, then the second saw
    /// an ordinary composer and submitted it. One owner report involved a
    /// binary Japanese-named document, so keep that exact shape in the test.
    #[test]
    fn finder_enter_only_inserts_the_path_in_every_mode() {
        let path = r"C:\Users\ryuichi\Documents\テスト2.docx";

        for agent_mode in [false, true] {
            let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
            let (mut state, _boot) = boot(UiConfig::default(), requests);
            while received.try_recv().is_ok() {}

            state.panel_visible = true;
            state.panel.phase = Phase::Idle;
            state.panel.agent_mode = agent_mode;
            state.panel.set_input("inspect @");
            state.panel.file_finder.open();
            state
                .panel
                .file_finder
                .set_candidates(vec![crate::bridge::FileCandidate {
                    display: "~/Documents/テスト2.docx".to_owned(),
                    path: path.to_owned(),
                }]);

            let panel_window = state.panel_window;
            let _ = update(
                &mut state,
                Message::WindowKey(
                    panel_window,
                    WindowChord::Enter {
                        command: false,
                        shift: false,
                    },
                ),
            );

            assert_eq!(state.panel.input, format!("inspect {path} "));
            assert!(!state.panel.file_finder.open);
            assert!(
                received.try_recv().is_err(),
                "@ completion must send neither AttachFile nor Submit (agent={agent_mode})"
            );
            assert!(matches!(state.panel.phase, Phase::Idle));
        }
    }

    #[test]
    fn crash_recovery_offers_redacted_diagnostics() {
        let mut state = app();
        let _ = backend_update(&mut state, UiEvent::RecoveredFromCrash);
        let toast = state.panel.toast.as_ref().expect("recovery toast");
        assert!(toast.offer_diagnostics);
        let _ = panel::view(&state.panel, state.config.appearance);
    }

    #[test]
    fn the_panel_is_created_hidden_and_warms_up() {
        let settings = panel_window_settings();
        assert!(
            !settings.visible,
            "§6: the panel must be pre-created hidden"
        );
        assert!(!settings.decorations);
        assert_eq!(settings.level, window::Level::AlwaysOnTop);

        let mut state = app();
        let panel_window = state.panel_window;
        let _ = update(&mut state, Message::WindowOpened(panel_window));
        assert!(matches!(state.panel.phase, Phase::WarmingUp { .. }));
        let _ = update(&mut state, Message::FramePainted);
        let _ = update(&mut state, Message::FramePainted);
        assert_eq!(state.panel.phase, Phase::Hidden);
        assert!(!state.panel_visible);
    }

    #[test]
    fn backend_is_notified_once_after_the_panel_window_exists() {
        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);
        let panel_window = state.panel_window;

        let _ = update(&mut state, Message::WindowOpened(panel_window));
        assert!(matches!(events.try_recv(), Ok(UiRequest::UiReady)));
        let _ = update(&mut state, Message::WindowOpened(panel_window));
        assert!(
            events.try_recv().is_err(),
            "UiReady is a one-shot lifecycle event"
        );
    }

    #[test]
    fn events_for_a_stale_session_are_dropped() {
        let mut state = app();
        let stale = Uuid::now_v7();
        assert_ne!(stale, state.panel.session);
        let _ = backend_update(
            &mut state,
            UiEvent::Stream {
                session: stale,
                event: Box::new(StreamEvent::Text("wrong session".to_owned())),
            },
        );
        assert!(state.panel.response.is_empty());
    }

    #[test]
    fn an_ime_composition_puts_the_panel_in_its_ime_state() {
        let mut state = app();
        let session = state.panel.session;
        let _ = backend_update(
            &mut state,
            UiEvent::Context {
                session,
                app: Some(app_info()),
                field: Some(Box::new(field(true))),
                selection: None,
                clipboard: None,
            },
        );
        assert!(matches!(state.panel.context, ContextState::ImeActive));
    }

    #[test]
    fn context_that_never_arrives_is_not_an_error() {
        let mut state = app();
        let session = state.panel.session;
        let _ = backend_update(
            &mut state,
            UiEvent::Context {
                session,
                app: Some(app_info()),
                field: None,
                selection: None,
                clipboard: None,
            },
        );
        assert!(matches!(
            state.panel.context,
            ContextState::Unavailable { .. }
        ));
    }

    #[test]
    fn a_selection_without_a_readable_field_is_prompt_context() {
        let mut state = app();
        let session = state.panel.session;
        let collapsed = state.panel.desired_height();
        let _ = backend_update(
            &mut state,
            UiEvent::Context {
                session,
                app: Some(app_info()),
                field: None,
                selection: Some("the selected terminal output".to_owned()),
                clipboard: None,
            },
        );

        assert!(matches!(
            &state.panel.context,
            ContextState::Available {
                selection: Some(selection),
                ..
            } if selection == "the selected terminal output"
        ));
        assert!(
            state.panel.desired_height() > collapsed,
            "the in-composer selection preview needs visible space"
        );
        assert!(
            state.panel.input.is_empty(),
            "captured text must stay separate from the trusted instruction"
        );
    }

    /// §6's guarantees, restated for the inline model (owner redesign,
    /// 2026-08-02): a run outlives its panel session, ⌘N does not lose it,
    /// and the ⌘T overlay is the thread back to it.
    #[test]
    fn a_run_survives_dismissal_and_new_chats_and_the_overlay_finds_it() {
        let mut state = app();
        let task = Uuid::now_v7();
        let session = state.panel.session;
        let _ = backend_update(
            &mut state,
            UiEvent::TaskStarted {
                task,
                session,
                instruction: "rename the flag".to_owned(),
            },
        );
        assert_eq!(state.panel.tasks.len(), 1);
        assert_eq!(state.panel.session_tasks().count(), 1, "inline card");

        let _ = panel_update(&mut state, panel::Message::Dismiss);
        assert!(!state.panel_visible);
        assert_eq!(
            state.panel.tasks.len(),
            1,
            "§6: dismissing the panel never cancels a run"
        );

        state.panel.reset(SessionId::from_u128(99));
        assert_eq!(state.panel.tasks.len(), 1, "runs are not session state");
        assert_eq!(
            state.panel.session_tasks().count(),
            0,
            "but the card belongs to its own session"
        );

        let _ = focus_first_task(&mut state);
        assert!(state.panel.tasks_open, "the overlay is the thread back");
        assert_eq!(state.panel.selected_task, Some(task));
    }

    /// An approval arriving for any run turns the chip and tray amber; no
    /// window is popped (owner redesign: nothing steals the screen).
    #[test]
    fn a_new_approval_marks_the_run_blocked_without_stealing_focus() {
        use aibo_core::types::{ApprovalKind, ApprovalRequest};

        let mut state = app();
        let task = Uuid::now_v7();
        let session = state.panel.session;
        let _ = backend_update(
            &mut state,
            UiEvent::TaskStarted {
                task,
                session,
                instruction: "clean the build".to_owned(),
            },
        );
        let _ = backend_update(
            &mut state,
            UiEvent::TaskStep {
                task,
                step: Box::new(aibo_core::types::AgentStep::AwaitingApproval(
                    ApprovalRequest {
                        id: "approval-1".to_owned(),
                        kind: ApprovalKind::Command,
                        summary: "remove generated files".to_owned(),
                        command: Some("rm -rf ./build".to_owned()),
                        paths: Vec::new(),
                        originating_instruction: "clean the build".to_owned(),
                        requires_typed_confirmation: true,
                    },
                )),
            },
        );
        assert!(state.panel.tasks[0].is_blocked());
        assert!(state.panel.any_task_blocked(), "the chip turns amber");
        assert!(!state.panel.tasks_open, "no surface is popped uninvited");
    }

    #[test]
    fn destructive_approval_preserves_the_typed_confirmation_on_the_bridge() {
        use aibo_core::types::{ApprovalDecision, ApprovalKind, ApprovalRequest};

        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);
        let task = Uuid::now_v7();
        let session = state.panel.session;
        let _ = backend_update(
            &mut state,
            UiEvent::TaskStarted {
                task,
                session,
                instruction: "clean the build".to_owned(),
            },
        );
        state.panel.tasks[0].push(aibo_core::types::AgentStep::AwaitingApproval(
            ApprovalRequest {
                id: "approval-1".to_owned(),
                kind: ApprovalKind::Command,
                summary: "remove generated files".to_owned(),
                command: Some("rm -rf ./build".to_owned()),
                paths: Vec::new(),
                originating_instruction: "clean the build".to_owned(),
                requires_typed_confirmation: true,
            },
        ));
        state.panel.tasks[0].typed_confirmation = "rm -rf ./build".to_owned();

        let _ = panel_update(
            &mut state,
            panel::Message::TaskDecide(task, ApprovalDecision::Approve),
        );
        assert!(matches!(
            events.try_recv(),
            Ok(UiRequest::Approve {
                decision: ApprovalDecision::Approve,
                typed_confirmation: Some(typed),
                ..
            }) if typed == "rm -rf ./build"
        ));
    }

    /// Regression, F2. `Accept` used to call `send(UiRequest::Insert)` and
    /// return `Task::none()`, so the request reached the runtime while the
    /// panel was still on screen holding focus — inverting §8's ordered insert
    /// sequence, whose first step is "hide the panel" and whose second is a
    /// `restore_focus` that must *confirm* the target got focus back.
    #[test]
    fn accept_hides_the_panel_before_the_insert_is_dispatched() {
        use aibo_core::types::StopReason;

        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);

        state.panel_visible = true;
        state.panel.phase = Phase::Finished {
            reason: StopReason::EndTurn,
        };
        state
            .panel
            .set_response("the deployment should be reverted");
        // §8's insert needs a captured field; `can_accept` now requires one, so
        // without this the test would pass by never dispatching at all.
        state.panel.context = panel::ContextState::Available {
            app: None,
            excerpt: Some("hello".to_owned()),
            selection: None,
            truncated: false,
            caret_bounds: None,
        };
        assert!(state.panel.can_accept());

        let task = panel_update(&mut state, panel::Message::Accept);

        // Step 1 of the sequence, and it has already happened.
        assert!(!state.panel_visible, "§8 step 1: hide the panel");

        // The request must not have been fired yet: it is sequenced *behind*
        // the hide rather than racing it. The old code failed here.
        assert!(
            events.try_recv().is_err(),
            "§8: Insert must not be dispatched while the panel still has focus"
        );

        // Two units of work: the `set_mode(Hidden)` effect, then the dispatch.
        // `Task::none()` — what the old code returned — is zero.
        assert_eq!(task.units(), 2, "the hide and the dispatch are both queued");
    }

    #[test]
    fn command_enter_routes_to_replace_while_the_composer_is_focused() {
        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);
        state.panel_visible = true;
        state.panel.phase = Phase::Finished {
            reason: StopReason::EndTurn,
        };
        state.panel.set_response("replacement");
        // Replace requires a captured field to insert into, so the test needs
        // one: without it `can_accept` refuses and this asserts nothing.
        state.panel.context = panel::ContextState::Available {
            app: None,
            excerpt: Some("hello".to_owned()),
            selection: None,
            truncated: false,
            caret_bounds: None,
        };

        let panel_window = state.panel_window;
        let task = update(
            &mut state,
            Message::WindowKey(
                panel_window,
                WindowChord::Enter {
                    command: true,
                    shift: false,
                },
            ),
        );

        assert!(!state.panel_visible);
        assert_eq!(task.units(), 2);
        assert!(
            events.try_recv().is_err(),
            "Insert remains sequenced after the hide"
        );
    }

    #[test]
    fn enter_does_not_submit_or_replace_during_ime_preedit() {
        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);
        state.panel_visible = true;
        state.panel.phase = Phase::Finished {
            reason: StopReason::EndTurn,
        };
        state.panel.set_response("replacement");
        // Replace requires a captured field to insert into, so the test needs
        // one: without it `can_accept` refuses and this asserts nothing.
        state.panel.context = panel::ContextState::Available {
            app: None,
            excerpt: Some("hello".to_owned()),
            selection: None,
            truncated: false,
            caret_bounds: None,
        };
        let panel_window = state.panel_window;
        state.ime_preedit.insert(panel_window);

        let task = update(
            &mut state,
            Message::WindowKey(
                panel_window,
                WindowChord::Enter {
                    command: true,
                    shift: false,
                },
            ),
        );

        assert_eq!(task.units(), 0);
        assert!(state.panel_visible);
        assert!(events.try_recv().is_err());
    }

    /// The dismiss path has no insert to order, so it must stay a plain hide.
    #[test]
    fn dismiss_still_sends_immediately() {
        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);
        state.panel_visible = true;

        let _ = panel_update(&mut state, panel::Message::Dismiss);
        assert!(!state.panel_visible);
        assert!(matches!(events.try_recv(), Ok(UiRequest::Cancel { .. })));
        assert!(matches!(
            events.try_recv(),
            Ok(UiRequest::DiscardSession { .. })
        ));
    }

    /// The hotkey used to discard the session every time, so dismissing the
    /// panel to look something up lost the thread. Reusing the id is what makes
    /// continuation work — the backend holds the history against it — so a
    /// `DiscardSession` here would silently break the feature.
    #[test]
    fn reopening_the_panel_continues_the_previous_conversation() {
        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);
        let previous = state.panel.session;

        let _ = state.open_panel();
        assert!(matches!(
            events.try_recv(),
            Ok(UiRequest::CaptureContext { session, .. }) if session == previous
        ));
        assert_eq!(state.panel.session, previous, "same session, same history");
    }

    /// The first prompt used to be cropped from its top as these two backend
    /// events populated footer rows after `Submit` had already sized the
    /// window. Both mutations must schedule the matching geometry update.
    #[test]
    fn dispatch_and_usage_rows_resize_the_visible_first_turn() {
        let mut state = app();
        let _ = update(&mut state, monitor(1920.0, 1080.0, 2.0));
        let _ = state.open_panel();
        state.panel.phase = Phase::Idle;
        state.panel.set_input("hello");

        let _ = panel_update(&mut state, panel::Message::Submit);
        let after_submit = state.last_placement.expect("submitted panel is placed");
        let session = state.panel.session;

        let dispatched = backend_update(
            &mut state,
            UiEvent::Dispatched {
                session,
                provider: ProviderId::CODEX,
                model: "gpt-5.6-sol".to_owned(),
                substituted_for: None,
            },
        );
        let after_dispatch = state.last_placement.expect("dispatch resized panel");
        assert!(after_dispatch.size.1 > after_submit.size.1);
        assert!(dispatched.units() > 0, "dispatch schedules window geometry");

        let usage = backend_update(
            &mut state,
            UiEvent::Stream {
                session,
                event: Box::new(StreamEvent::Usage(Usage {
                    input_tokens: 1_120,
                    cached_input_tokens: 3_712,
                    output_tokens: 13,
                    ..Usage::default()
                })),
            },
        );
        let after_usage = state.last_placement.expect("usage resized panel");
        assert!(after_usage.size.1 > after_dispatch.size.1);
        assert!(usage.units() > 0, "usage schedules window geometry");
    }

    /// An explicit new (`⌘N`) and a screen capture both go through
    /// `begin_panel_session`, which must still throw the old one away.
    #[test]
    fn asking_for_a_new_session_still_discards_the_previous_one() {
        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);
        let previous = state.panel.session;

        state.begin_panel_session();
        assert!(matches!(
            events.try_recv(),
            Ok(UiRequest::DiscardSession { session }) if session == previous
        ));
        assert_ne!(state.panel.session, previous);
    }

    /// Arriving with text selected is a new question. Answering it with an
    /// unrelated exchange still above it — and still in the model's history —
    /// is the case this rule exists for.
    #[test]
    fn a_selection_starts_a_new_conversation_but_an_empty_context_does_not() {
        let mut state = app();
        state.panel.active_user = Some("what is this".to_owned());
        assert!(state.panel.has_conversation());
        let before = state.panel.session;

        let _ = backend_update(&mut state, context_with_selection(before, None));
        assert_eq!(
            state.panel.session, before,
            "nothing selected: this is a follow-up"
        );

        let _ = backend_update(
            &mut state,
            context_with_selection(before, Some("fn main() {}")),
        );
        assert_ne!(
            state.panel.session, before,
            "a selection is a new question, not a follow-up"
        );
    }

    /// When a selection replaces an existing conversation, its fresh session
    /// must keep reading from the app that produced the selection. Re-snapshotting
    /// after the panel has focus names aibo on Windows and loses the text.
    #[test]
    fn selection_fresh_start_preserves_the_original_focus_snapshot() {
        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);
        state.panel.active_user = Some("previous question".to_owned());
        let before = state.panel.session;
        let source = app_info();

        let _ = backend_update(
            &mut state,
            UiEvent::Context {
                session: before,
                app: Some(source.clone()),
                field: None,
                selection: Some("selected on Windows".to_owned()),
                clipboard: None,
            },
        );

        assert!(matches!(
            events.try_recv(),
            Ok(UiRequest::DiscardSession { session }) if session == before
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(UiRequest::CaptureContext {
                app_ref: Some(app_ref),
                ..
            }) if app_ref == source.app_ref
        ));
    }

    fn context_with_selection(session: SessionId, selection: Option<&str>) -> UiEvent {
        UiEvent::Context {
            session,
            app: None,
            field: None,
            selection: selection.map(str::to_owned),
            clipboard: None,
        }
    }

    #[test]
    fn enter_is_ignored_during_a_stream_instead_of_resubmitting() {
        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);
        state.panel.phase = Phase::Streaming;
        state.panel.input = "rewrite this".to_owned();

        let task = panel_update(&mut state, panel::Message::Submit);
        assert_eq!(task.units(), 0);
        assert!(events.try_recv().is_err());
        assert_eq!(state.panel.phase, Phase::Streaming);
    }

    #[test]
    fn return_with_an_empty_composer_does_nothing_after_an_answer() {
        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);
        state.panel.active_user = Some("question".to_owned());
        state.panel.set_response("answer");
        state.panel.phase = Phase::Finished {
            reason: StopReason::EndTurn,
        };

        let _ = panel_update(&mut state, panel::Message::Submit);
        assert!(events.try_recv().is_err());
        assert_eq!(
            state.panel.phase,
            Phase::Finished {
                reason: StopReason::EndTurn
            }
        );
        assert_eq!(state.panel.response, "answer");
    }

    #[tokio::test]
    async fn a_follow_up_submission_carries_the_completed_chat_history() {
        let (requests, mut events) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _boot) = boot(UiConfig::default(), requests);
        state.panel.active_user = Some("first question".to_owned());
        state.panel.set_response("first answer");
        state.panel.phase = Phase::Finished {
            reason: StopReason::EndTurn,
        };
        state.panel.input = "follow up".to_owned();

        let _ = panel_update(&mut state, panel::Message::Submit);
        let Some(UiRequest::Submit {
            instruction,
            history,
            ..
        }) = events.recv().await
        else {
            panic!("follow-up submit was not delivered");
        };
        assert_eq!(instruction, "follow up");
        assert_eq!(
            history,
            vec![aibo_core::context::Turn::pair(
                "first question",
                "first answer"
            )]
        );
        assert_eq!(state.panel.turns.len(), 1);
        assert_eq!(state.panel.active_user.as_deref(), Some("follow up"));
    }

    #[test]
    fn window_roles_are_resolved_by_id() {
        let state = app();
        assert_eq!(state.role_of(state.panel_window), Some(Role::Panel));
    }

    // -----------------------------------------------------------------------
    // §9 placement wiring
    //
    // The regression these guard was visible on screen: the panel rendered
    // correctly and sat in the top-left corner of the display. `placement`
    // itself was fine — nothing ever fed it a display, and its empty-list
    // answer was the origin, which `show_panel` then dutifully applied.
    // -----------------------------------------------------------------------

    fn screen(id: u64, x: f64, y: f64, width: f64, height: f64, is_primary: bool) -> DisplayInfo {
        use aibo_core::types::Rect;
        DisplayInfo {
            id,
            bounds: Rect {
                x,
                y,
                width,
                height,
            },
            visible_frame: Rect {
                x,
                y: y + 25.0,
                width,
                height: height - 25.0,
            },
            scale_factor: 2.0,
            is_primary,
        }
    }

    fn monitor(width: f32, height: f32, scale: f32) -> Message {
        Message::Observed {
            monitor: Some(Size::new(width, height)),
            scale,
        }
    }

    /// The ordering fix. With nothing known about the displays the panel must
    /// stay hidden until the window server answers — showing first and
    /// correcting afterwards is what produced the corner, then a jump.
    #[test]
    fn the_panel_is_never_shown_before_its_placement_is_resolved() {
        let mut state = app();
        assert!(!state.geometry_is_known(), "a fresh daemon knows nothing");

        let _ = state.open_panel();
        assert!(
            !state.panel_visible,
            "§9: the panel must not be shown before its placement is resolved"
        );
        assert!(state.last_placement.is_none());
        assert!(state.pending_show);

        let _ = update(&mut state, monitor(1920.0, 1080.0, 2.0));
        assert!(state.panel_visible, "the answer completes the show");
        assert!(!state.pending_show);

        let placement = state.last_placement.expect("placed");
        assert_ne!(placement.position, (0.0, 0.0), "the original bug");
        assert!(placement.position.0 > 0.0 && placement.position.1 > 0.0);
    }

    /// Once geometry is known the panel goes up on the spot (§8 wants it
    /// immediate) and re-probes behind itself.
    #[test]
    fn a_second_show_does_not_wait_for_the_window_server() {
        let mut state = app();
        let _ = update(&mut state, monitor(1920.0, 1080.0, 2.0));

        let task = state.open_panel();
        assert!(state.panel_visible, "§8: show immediately");
        assert!(!state.pending_show);
        assert!(
            task.units() > 5,
            "the show is followed by a re-probe: {}",
            task.units()
        );
    }

    /// The whole point, stated once: no display arrangement the UI can be in
    /// puts the panel in the corner.
    #[test]
    fn no_display_arrangement_puts_the_panel_in_the_corner() {
        let arrangements = vec![
            Vec::new(),
            vec![screen(1, 0.0, 0.0, 1920.0, 1080.0, true)],
            // An external display above and to the left of the laptop.
            vec![
                screen(1, 0.0, 0.0, 1440.0, 900.0, true),
                screen(2, -2560.0, -400.0, 2560.0, 1440.0, false),
            ],
            // Portrait secondary.
            vec![
                screen(1, 0.0, 0.0, 1280.0, 800.0, true),
                screen(3, 1280.0, -600.0, 1080.0, 1920.0, false),
            ],
        ];

        for displays in arrangements {
            let mut state = app();
            let _ = update(&mut state, monitor(1440.0, 900.0, 2.0));
            let _ = update(&mut state, Message::Displays(displays.clone()));
            let _ = state.open_panel();

            let p = state
                .last_placement
                .expect("every show resolves a placement");
            assert!(state.panel_visible);
            assert_ne!(p.position, (0.0, 0.0), "displays: {displays:?}");
            assert!(p.size.0 > 0.0 && p.size.1 > 0.0);
        }
    }

    /// §9: on disconnect or resolution change, re-clamp.
    #[test]
    fn a_resolution_change_reclamps_a_visible_panel() {
        let mut state = app();
        let _ = update(&mut state, monitor(3840.0, 2160.0, 2.0));
        let _ = update(
            &mut state,
            Message::Displays(vec![screen(1, 0.0, 0.0, 3840.0, 2160.0, true)]),
        );
        let _ = state.open_panel();
        let wide = state.last_placement.expect("placed");

        // The same display, now at a much smaller resolution.
        let _ = update(
            &mut state,
            Message::Displays(vec![screen(1, 0.0, 0.0, 1024.0, 768.0, true)]),
        );
        let narrow = state.last_placement.expect("re-placed");
        assert_ne!(wide, narrow, "§9: a resolution change must re-clamp");
        assert!(narrow.position.0 + narrow.size.0 <= 1024.0);
        assert!(narrow.position.1 + narrow.size.1 <= 768.0);
    }

    /// §9: if the remembered display is gone, fall back to the primary.
    #[test]
    fn a_disconnected_display_is_forgotten_and_the_primary_takes_over() {
        let mut state = app();
        let _ = update(&mut state, monitor(1440.0, 900.0, 2.0));
        let _ = update(
            &mut state,
            Message::Displays(vec![
                screen(1, 0.0, 0.0, 1440.0, 900.0, true),
                screen(2, 1440.0, 0.0, 1920.0, 1080.0, false),
            ]),
        );
        // Pretend the last show landed on the external display.
        let _ = state.open_panel();
        state.last_placement = state.last_placement.map(|mut p| {
            p.display_id = 2;
            p
        });

        // Unplug it.
        let _ = update(
            &mut state,
            Message::Displays(vec![screen(1, 0.0, 0.0, 1440.0, 900.0, true)]),
        );
        let p = state.last_placement.expect("re-placed on the primary");
        assert_eq!(p.display_id, 1, "§9: fall back to the primary");
        assert!(p.position.0 >= 0.0 && p.position.0 + p.size.0 <= 1440.0);
    }

    /// A display list that arrives while the panel is hidden still has to clear
    /// a remembered display that no longer exists, or the next show steers by a
    /// dangling id.
    #[test]
    fn a_disconnect_while_hidden_still_forgets_the_display() {
        let mut state = app();
        let _ = update(&mut state, monitor(1440.0, 900.0, 2.0));
        let _ = update(
            &mut state,
            Message::Displays(vec![screen(9, 0.0, 0.0, 1440.0, 900.0, true)]),
        );
        let _ = state.open_panel();
        let _ = panel_update(&mut state, panel::Message::Dismiss);
        assert!(!state.panel_visible);
        assert_eq!(state.last_placement.map(|p| p.display_id), Some(9));

        let _ = update(
            &mut state,
            Message::Displays(vec![screen(4, -1920.0, -200.0, 1920.0, 1080.0, true)]),
        );
        assert!(state.last_placement.is_none(), "§9: forget what is gone");
    }

    /// §9: "recompute scale factor on every show, not just at creation".
    #[test]
    fn the_scale_factor_is_recomputed_on_every_show() {
        let mut state = app();
        let _ = update(&mut state, monitor(1920.0, 1080.0, 2.0));
        let _ = update(
            &mut state,
            Message::Displays(vec![screen(1, 0.0, 0.0, 1920.0, 1080.0, true)]),
        );
        let _ = state.open_panel();
        assert_eq!(state.last_placement.map(|p| p.scale_factor), Some(2.0));

        // The panel was dragged to a 1× display, or the user changed the
        // display's scaling. The snapshot still says 2×; the window server does
        // not, and it is the window server that is right.
        let _ = update(&mut state, monitor(1920.0, 1080.0, 1.0));
        assert_eq!(
            state.last_placement.map(|p| p.scale_factor),
            Some(1.0),
            "a stale factor renders blurry or wrong-sized (§9)"
        );
    }

    /// Resize and scale-factor events used to be mapped to `Ignored`, which is
    /// indistinguishable from caching the value from creation.
    #[test]
    fn a_resize_event_re_reads_the_geometry() {
        let mut state = app();
        assert!(
            update(&mut state, Message::ProbeGeometry).units() > 0,
            "§9: re-layout on size and scale-factor changes"
        );
    }

    #[test]
    fn beginning_a_panel_drag_preserves_the_users_position() {
        let mut state = app();
        assert!(!state.panel_dragged);

        let task = panel_update(&mut state, panel::Message::BeginDrag);

        assert!(state.panel_dragged);
        assert!(task.units() > 0, "the native window drag must be requested");
    }

    /// The gesture end to end: press, two motions, release. The first motion
    /// only anchors, so the size that lands is the travel from *it* — a drag
    /// that started 6 pt inside the grip must not shift the panel by 6 pt.
    #[test]
    fn dragging_the_grip_sizes_the_panel_from_the_first_motion() {
        let mut state = app();
        let _ = update(&mut state, monitor(1920.0, 1080.0, 2.0));
        let _ = state.open_panel();
        let before = state.last_placement.expect("shown").size;

        let _ = panel_update(&mut state, panel::Message::BeginResize);
        let _ = update(&mut state, Message::PanelResizeMoved(Point::new(6.0, 6.0)));
        assert_eq!(
            state.panel.user_size, None,
            "the anchoring motion must not resize anything"
        );

        let _ = update(
            &mut state,
            Message::PanelResizeMoved(Point::new(86.0, 46.0)),
        );
        let size = state.panel.user_size.expect("the drag set a size");
        assert!(
            (size.0 - (before.0 + 80.0)).abs() < 0.5 && (size.1 - (before.1 + 40.0)).abs() < 0.5,
            "expected {:?} + (80, 40), got {size:?}",
            before
        );

        let _ = update(&mut state, Message::PanelResizeEnded);
        assert!(state.panel_resize.is_none(), "the drag is over");
        assert_eq!(
            state.config.panel_size, state.panel.user_size,
            "the size on screen is the size that gets written"
        );
    }

    /// The floors belong to the drag; the display ceiling belongs to
    /// `placement`. Dragging the corner up and to the left past both floors
    /// must stop at them rather than invert the panel.
    #[test]
    fn a_drag_cannot_shrink_the_panel_below_its_floors() {
        let mut state = app();
        let _ = update(&mut state, monitor(1920.0, 1080.0, 2.0));
        let _ = state.open_panel();

        let _ = panel_update(&mut state, panel::Message::BeginResize);
        let _ = update(&mut state, Message::PanelResizeMoved(Point::new(0.0, 0.0)));
        let _ = update(
            &mut state,
            Message::PanelResizeMoved(Point::new(-4000.0, -4000.0)),
        );

        assert_eq!(
            state.panel.user_size,
            Some((ui_theme::PANEL_WIDTH_MIN, ui_theme::PANEL_HEIGHT_COLLAPSED))
        );
    }

    /// Double-clicking the grip hands sizing back to the content, and says so
    /// in the config file rather than leaving the old size behind to come back
    /// at the next launch.
    #[test]
    fn double_clicking_the_grip_returns_the_panel_to_automatic_sizing() {
        let mut state = app();
        let _ = update(&mut state, monitor(1920.0, 1080.0, 2.0));
        let _ = state.open_panel();
        let automatic = state.last_placement.expect("shown").size;

        let _ = panel_update(&mut state, panel::Message::BeginResize);
        let _ = update(&mut state, Message::PanelResizeMoved(Point::new(0.0, 0.0)));
        let _ = update(
            &mut state,
            Message::PanelResizeMoved(Point::new(120.0, 120.0)),
        );
        let _ = update(&mut state, Message::PanelResizeEnded);
        assert!(state.panel.user_size.is_some());

        let _ = panel_update(&mut state, panel::Message::ResetSize);

        assert_eq!(state.panel.user_size, None);
        assert_eq!(state.config.panel_size, None, "and the file is cleared");
        assert_eq!(
            state.last_placement.expect("still shown").size,
            automatic,
            "the panel goes back to the size the content asks for"
        );
    }

    /// A hand-set size is a preference, not session state.
    #[test]
    fn a_new_chat_keeps_the_size_the_user_dragged_to() {
        let mut state = app();
        let _ = update(&mut state, monitor(1920.0, 1080.0, 2.0));
        let _ = state.open_panel();
        state.panel.user_size = Some((800.0, 600.0));

        state.begin_panel_session();

        assert_eq!(state.panel.user_size, Some((800.0, 600.0)));
    }

    /// A size restored from `config.toml` has to reach the first placement, or
    /// the panel opens at the automatic size and only obeys the preference
    /// after something else triggers a resize.
    #[test]
    fn a_restored_size_is_used_by_the_first_show() {
        let (requests, _rx) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(
            UiConfig {
                panel_size: Some((900.0, 640.0)),
                ..UiConfig::default()
            },
            requests,
        );
        let _ = update(&mut state, monitor(1920.0, 1080.0, 2.0));
        let _ = state.open_panel();

        assert_eq!(state.last_placement.expect("shown").size, (900.0, 640.0));
    }

    /// The permission rows had no producer: `PermissionChanged` only ever came
    /// from a capture that had already failed, so the section was blank on a
    /// healthy machine and blank on a broken one until something broke twice.
    #[test]
    fn opening_settings_asks_the_os_for_permission_status() {
        let (requests, mut rx) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);

        let _ = open_settings(&mut state);

        let mut asked = false;
        while let Ok(request) = rx.try_recv() {
            asked |= matches!(request, UiRequest::RefreshPermissions);
        }
        assert!(asked, "settings must ask, never assume, on every open");
    }

    #[test]
    fn a_dragged_panel_fallback_still_applies_an_edge_clamped_position() {
        let mut state = app();
        let _ = update(&mut state, Message::Ready);
        state.panel_dragged = true;
        let placement = Placement {
            display_id: 1,
            position: (320.0, 180.0),
            size: (720.0, 520.0),
            scale_factor: 2.0,
            anchored: false,
            assumed: false,
        };

        let task = update(&mut state, Message::PanelFrameFallback(placement));

        assert_eq!(
            task.units(),
            2,
            "fallback must resize and move even after a user drag"
        );
    }

    /// The re-probe that follows every show must not re-show the panel when
    /// nothing changed, or `resize` → resize event → probe → `resize` never
    /// settles.
    #[test]
    fn an_unchanged_geometry_answer_does_not_re_show_the_panel() {
        let mut state = app();
        let _ = update(&mut state, monitor(1920.0, 1080.0, 2.0));
        let _ = state.open_panel();
        let before = state.last_placement;

        let task = update(&mut state, monitor(1920.0, 1080.0, 2.0));
        assert_eq!(task.units(), 0, "nothing changed, so nothing is re-issued");
        assert_eq!(state.last_placement, before);
    }

    /// An answer that arrives after the user pressed `esc` must not resurrect
    /// the panel.
    #[test]
    fn a_late_geometry_answer_does_not_resurrect_a_dismissed_panel() {
        let mut state = app();
        let _ = state.open_panel();
        assert!(state.pending_show);

        let _ = panel_update(&mut state, panel::Message::Dismiss);
        assert!(!state.pending_show, "the pending show is cancelled with it");

        let _ = update(&mut state, monitor(1920.0, 1080.0, 2.0));
        assert!(!state.panel_visible);
    }

    /// §9: the position and size must reach the window server *before* it is
    /// made visible. `Task::batch` merges with `SelectAll` and makes no such
    /// promise, so the show is a chain — eight effects, in order.
    #[test]
    fn the_show_sequence_is_ordered() {
        let mut state = app();
        let _ = update(&mut state, monitor(1440.0, 900.0, 2.0));
        let placement = state.placement();
        let task = state.show_panel(placement);
        assert_eq!(
            task.units(),
            8,
            "resize, backdrop pin, move, show, native configure, native present, focus window, focus input"
        );
    }

    // -----------------------------------------------------------------------
    // Attachments (§2, §4, §5, §14)
    //
    // The regression, observed 2026-07-26: `has_image` was derived from the
    // clipboard, so taking a screenshot rerouted every subsequent request to
    // the `Vision` role — which nothing binds — and surfaced as "No provider is
    // configured yet" while settings showed a signed-in, healthy provider.
    // -----------------------------------------------------------------------

    fn clipboard(kind: aibo_core::types::ClipboardKind) -> aibo_core::types::ClipboardItem {
        aibo_core::types::ClipboardItem {
            kind,
            text: None,
            files: Vec::new(),
            concealed: false,
            transient: false,
            source_app: Some("Chrome".to_owned()),
            sequence: 1,
            restorable: true,
        }
    }

    fn capture_with(state: &mut Aibo, item: aibo_core::types::ClipboardItem) {
        let session = state.panel.session;
        let _ = backend_update(
            state,
            UiEvent::Context {
                session,
                app: Some(app_info()),
                field: Some(Box::new(field(false))),
                selection: None,
                clipboard: Some(Box::new(item)),
            },
        );
    }

    /// The thesis, at the layer that used to break it. A screenshot on the
    /// clipboard when the hotkey fires is *context*. It never attaches itself,
    /// so it never reaches routing.
    #[test]
    fn a_screenshot_on_the_clipboard_does_not_attach_itself() {
        let mut state = app();
        capture_with(&mut state, clipboard(ClipboardKind::ImageRef));

        assert!(
            state.panel.clipboard.is_image(),
            "it is offered, so ⌘V has something to do"
        );
        assert!(
            !state.panel.has_attachments(),
            "…and nothing is attached until the user says so"
        );
    }

    #[test]
    fn text_on_the_clipboard_is_not_offered_as_an_image() {
        let mut state = app();
        capture_with(&mut state, clipboard(ClipboardKind::Text));
        assert!(!state.panel.clipboard.is_image());
    }

    /// §12: concealed content is never recorded and never sent. Being an image
    /// is not an exemption, and offering it would invite the user to send
    /// exactly what the marker exists to withhold.
    #[test]
    fn concealed_clipboard_content_is_never_offered() {
        let mut state = app();
        let mut item = clipboard(ClipboardKind::ImageRef);
        item.concealed = true;
        capture_with(&mut state, item);
        assert!(!state.panel.clipboard.is_image());
    }

    /// ⌘V is claimed for the whole process, so it has to be scoped to the panel
    /// by hand — pasting into a settings field must not attach anything.
    #[test]
    fn the_attach_chord_only_reaches_a_visible_panel() {
        let mut state = app();
        let panel_window = state.panel_window;
        capture_with(&mut state, clipboard(ClipboardKind::ImageRef));

        // Another window's ⌘V.
        let elsewhere = window::Id::unique();
        state.panel_visible = true;
        let _ = update(
            &mut state,
            Message::WindowKey(elsewhere, WindowChord::Attach),
        );
        assert!(state.panel.toast.is_none(), "not ours to act on");

        // The panel's own ⌘V, but the panel is not on screen.
        state.panel_visible = false;
        let _ = update(
            &mut state,
            Message::WindowKey(panel_window, WindowChord::Attach),
        );
        assert!(state.panel.toast.is_none());
    }

    /// An advertised image without bytes must not expose an enabled action.
    ///
    /// The bridge cannot materialise clipboard pixels yet, so every current
    /// `ImageRef` lands here. Showing an enabled action that deterministically
    /// fails is a false affordance.
    #[test]
    fn an_unreadable_clipboard_image_is_not_advertised_as_attachable() {
        let mut state = app();
        state.panel_visible = true;
        capture_with(&mut state, clipboard(ClipboardKind::ImageRef));
        assert!(state.panel.clipboard.is_image());
        assert!(!state.panel.clipboard.is_attachable());

        let panel_window = state.panel_window;
        let _ = update(
            &mut state,
            Message::WindowKey(panel_window, WindowChord::Attach),
        );
        assert!(state.panel.toast.is_none());
        assert!(!state.panel.has_attachments());
    }

    /// A visible chip and a dispatched image are one transaction. This is the
    /// regression boundary for the old bridge gap, where the UI could hold
    /// pixels but `Submit` had nowhere to carry them.
    #[tokio::test]
    async fn submit_carries_every_deliberately_attached_image() {
        use aibo_core::types::{Attachment, AttachmentSource};

        let (requests, mut received) = tokio::sync::mpsc::channel(UI_REQUEST_CHANNEL_CAPACITY);
        let (mut state, _task) = boot(UiConfig::default(), requests);
        state.panel.phase = Phase::Idle;
        state.panel.input = "what is shown here?".to_owned();
        state
            .panel
            .attach(Attachment::image(
                AttachmentSource::ScreenRegion,
                vec![0x89, b'P', b'N', b'G'],
                "image/png",
                1200,
                750,
                "Screen region",
            ))
            .expect("test image attaches");

        let _ = panel_update(&mut state, panel::Message::Submit);
        let Some(UiRequest::Submit { attachments, .. }) = received.recv().await else {
            panic!("submit request was not delivered");
        };
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].source, AttachmentSource::ScreenRegion);
    }

    /// ⌘V attaches; ⌫ on an empty instruction takes it back off. Both through
    /// the panel's own entry points, so the act stays deliberate at every step.
    #[test]
    fn the_attach_and_detach_chords_round_trip() {
        use aibo_core::types::{Attachment, AttachmentSource};

        let mut state = app();
        state.panel_visible = true;
        state.panel.clipboard = panel::ClipboardOffer::Image {
            label: "Image from Chrome".to_owned(),
            image: Some(Box::new(Attachment::image(
                AttachmentSource::Clipboard,
                vec![0x89, b'P', b'N', b'G'],
                "image/png",
                1200,
                750,
                "Image from Chrome",
            ))),
        };

        let panel_window = state.panel_window;
        let _ = update(
            &mut state,
            Message::WindowKey(panel_window, WindowChord::Attach),
        );
        assert_eq!(state.panel.attachments().len(), 1);
        assert!(state.panel.toast.is_none());

        // ⌫ with an instruction in the field belongs to the instruction. The
        // panel and the focused `text_input` must never both act on one
        // keystroke — the user would lose a character *and* an image.
        state.panel.input = "what is this".to_owned();
        let _ = update(
            &mut state,
            Message::WindowKey(panel_window, WindowChord::DetachLast),
        );
        assert_eq!(
            state.panel.attachments().len(),
            1,
            "⌫ is the text's while there is text"
        );

        // Even with the input empty, the chord must not remove the image: a
        // screen capture opens the panel with the image attached and the
        // input empty, so backspace-detach destroyed screenshots by reflex.
        // Removal is a deliberate act — the chip's `×` or the footer action.
        state.panel.input.clear();
        let _ = update(
            &mut state,
            Message::WindowKey(panel_window, WindowChord::DetachLast),
        );
        assert_eq!(
            state.panel.attachments().len(),
            1,
            "no keystroke may destroy an attachment"
        );

        let _ = panel_update(&mut state, panel::Message::DetachLast);
        assert!(
            !state.panel.has_attachments(),
            "the footer action still removes it deliberately"
        );
    }

    /// §13's one action has to resolve the state. After it runs the panel is
    /// submittable again rather than stuck on a complaint about an image that
    /// is no longer there.
    #[test]
    fn removing_the_image_the_error_named_leaves_a_usable_panel() {
        use aibo_core::types::{Attachment, AttachmentSource, ModelBinding};

        let mut state = app();
        state.panel.input = "what is wrong with this chart".to_owned();
        state
            .panel
            .attach(Attachment::image(
                AttachmentSource::Clipboard,
                vec![0x89, b'P', b'N', b'G'],
                "image/png",
                1200,
                750,
                "chart",
            ))
            .expect("valid");

        let session = state.panel.session;
        let _ = backend_update(
            &mut state,
            UiEvent::Failed {
                session,
                error: std::sync::Arc::new(aibo_core::AiboError::vision_unsupported(
                    ModelBinding {
                        provider: aibo_core::types::ProviderId::CEREBRAS,
                        model: "gpt-oss-120b".to_owned(),
                    },
                    1,
                    Vec::new(),
                )),
            },
        );
        assert_eq!(state.panel.phase, Phase::Failed);
        assert!(
            state.settings_window.is_none(),
            "§13: inline, never the blocking treatment that opens settings"
        );

        let action = state
            .panel
            .error
            .as_ref()
            .and_then(|e| e.action.clone())
            .expect("one action");
        let _ = panel_update(&mut state, panel::Message::Error(action));

        assert!(!state.panel.has_attachments());
        assert_eq!(state.panel.phase, Phase::Idle);
        assert_eq!(state.panel.input, "what is wrong with this chart");
    }

    /// The caret path, end to end through the panel's context state: a caret in
    /// the bottom-right of a display anchors the panel on that display and
    /// nowhere near the corner of the desktop.
    #[test]
    fn a_caret_on_a_secondary_display_anchors_the_panel_there() {
        use aibo_core::types::Rect;

        let mut state = app();
        let _ = update(&mut state, monitor(1440.0, 900.0, 2.0));
        let _ = update(
            &mut state,
            Message::Displays(vec![
                screen(1, 0.0, 0.0, 1440.0, 900.0, true),
                screen(2, -2560.0, -400.0, 2560.0, 1440.0, false),
            ]),
        );

        let session = state.panel.session;
        let mut context = field(false);
        context.caret_bounds = Some(Rect {
            x: -2100.0,
            y: -300.0,
            width: 2.0,
            height: 18.0,
        });
        let _ = backend_update(
            &mut state,
            UiEvent::Context {
                session,
                app: Some(app_info()),
                field: Some(Box::new(context)),
                selection: None,
                clipboard: None,
            },
        );

        // The caret reaches placement through `PanelState::caret_bounds`, which
        // is the wiring §9 depends on and the reason `ContextState` carries
        // bounds at all.
        let placement = state.placement();
        assert_eq!(placement.display_id, 2, "anchored on the caret's display");
        assert!(placement.anchored);
        assert!(placement.position.0 < 0.0 && placement.position.1 < 0.0);
        assert!(placement.position.0 >= -2560.0);
    }
}
