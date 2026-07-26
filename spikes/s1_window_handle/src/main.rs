//! # S1 — iced 0.14 → native window handle
//!
//! Throwaway binary for risk-register item **S1** (docs/plan.md §20).
//!
//! §20 states S1 is *"largely de-risked already"*: `iced::window::run(id, f:
//! impl FnOnce(&dyn Window))` hands you a `HasWindowHandle`, and iced 0.14 and
//! `window-vibrancy` 0.8 both speak `raw-window-handle` 0.6, so they compose
//! directly. **What remains is the part this binary exists to answer:**
//!
//! > does an inserted `NSVisualEffectView` composite correctly *behind* iced's
//! > `CAMetalLayer`-backed view? Plus all-Spaces and acrylic on Windows.
//!
//! and, from §9, the all-Spaces requirement:
//!
//! > the panel must join all Spaces or it appears on the wrong desktop when the
//! > user is in a fullscreen app. That needs the native window handle.
//!
//! ## The compositing trap this spike is really testing
//!
//! `window_vibrancy::apply_vibrancy` does, on macOS:
//!
//! ```text
//! view.addSubview_positioned_relativeTo(&blurred_view, NSWindowOrderingMode::Below, None)
//! ```
//!
//! i.e. it adds the `NSVisualEffectView` as a **subview of the view you hand
//! it**, ordered below that view's *other subviews*. But the view iced hands
//! you (`AppKitWindowHandle::ns_view`) is winit's own layer-backed view, and in
//! AppKit a layer-backed view's own layer is the **parent** of its subviews'
//! layers — so a subview cannot render behind its host's layer content. If
//! iced's `CAMetalLayer` clears opaque, the blur is invisible; if it clears
//! transparent, the blur may still land in front of, not behind, the UI.
//!
//! So the spike offers two insertion strategies and lets you compare:
//!
//! | `--strategy` | What it does |
//! |---|---|
//! | `subview` | `window_vibrancy::apply_vibrancy` verbatim — the off-the-shelf path |
//! | `sibling` | reparent: new container becomes `contentView`, the effect view is added **first**, iced's view on top of it — a true sibling ordering |
//! | `none` | no vibrancy; isolates the level / all-Spaces half |
//!
//! Everything else (`NSWindow` level, `collectionBehavior`, opacity) is done by
//! hand through `objc2` on the handle iced gave us, and every mutation is read
//! back so the report states what the OS actually accepted rather than what we
//! asked for.

mod probe;

use std::time::{Duration, Instant};

use iced::widget::{center, column, text};
use iced::{Color, Element, Subscription, Task, window};

use probe::{Report, Strategy};

/// Command-line configuration.
#[derive(Debug, Clone, Copy)]
struct Config {
    strategy: Strategy,
    /// Auto-exit after this long. `--hold` sets it to effectively never.
    run_for: Duration,
    /// Start the window transparent (required for vibrancy to be visible at all).
    transparent: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            strategy: Strategy::Sibling,
            run_for: Duration::from_secs(30),
            transparent: true,
        }
    }
}

impl Config {
    /// Parse the (deliberately tiny) argument set.
    fn from_args() -> Self {
        let mut cfg = Self::default();
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--strategy" => {
                    if let Some(s) = args.get(i + 1).and_then(|s| Strategy::parse(s)) {
                        cfg.strategy = s;
                    }
                    i += 1;
                }
                "--seconds" => {
                    if let Some(v) = args.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
                        cfg.run_for = Duration::from_secs(v);
                    }
                    i += 1;
                }
                "--hold" => cfg.run_for = Duration::from_secs(u32::MAX as u64),
                "--opaque" => cfg.transparent = false,
                other => eprintln!("s1: ignoring unknown argument {other:?}"),
            }
            i += 1;
        }
        cfg
    }
}

/// Messages driving the spike.
#[derive(Debug, Clone)]
enum Message {
    /// Sampling / lifecycle tick.
    Tick,
    /// Result of asking iced for the id of the most recently opened window.
    Latest(Option<window::Id>),
    /// The native probe finished, on the event-loop thread.
    Probed(Report),
}

/// Spike state.
struct State {
    cfg: Config,
    started: Instant,
    window: Option<window::Id>,
    report: Option<Report>,
}

fn main() -> iced::Result {
    let cfg = Config::from_args();

    println!("# S1 — iced 0.14 native window handle probe");
    println!(
        "# os={} arch={} strategy={} transparent={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        cfg.strategy.name(),
        cfg.transparent
    );
    println!("# the window stays up for {:?}; LOOK AT IT — see README", cfg.run_for);

    let window_settings = window::Settings {
        size: iced::Size::new(680.0, 320.0),
        // §9: default panel width 680 pt.
        decorations: false,
        transparent: cfg.transparent,
        // §8: `Level::AlwaysOnTop` → `kCGFloatingWindowLevel`. The probe also
        // sets `NSFloatingWindowLevel` natively so the two can be compared.
        level: window::Level::AlwaysOnTop,
        resizable: false,
        ..window::Settings::default()
    };

    iced::application(
        move || State {
            cfg,
            started: Instant::now(),
            window: None,
            report: None,
        },
        update,
        view,
    )
    .title("aibo S1 window-handle probe")
    .window(window_settings)
    // A transparent app background is a precondition for seeing anything the
    // NSVisualEffectView draws. If this is opaque the spike answers nothing.
    .style(|_state: &State, _theme: &iced::Theme| iced::theme::Style {
        background_color: Color::TRANSPARENT,
        text_color: Color::WHITE,
    })
    .subscription(subscription)
    .run()
}

/// Tick four times a second: enough to find the window promptly, cheap enough
/// not to perturb anything.
fn subscription(_state: &State) -> Subscription<Message> {
    iced::time::every(Duration::from_millis(250)).map(|_| Message::Tick)
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            if state.started.elapsed() >= state.cfg.run_for {
                return iced::exit();
            }
            if state.window.is_none() {
                // `window::latest()` is the id-agnostic way to find the window
                // `iced::application` opened for us.
                return window::latest().map(Message::Latest);
            }
            Task::none()
        }
        Message::Latest(None) => Task::none(),
        Message::Latest(Some(id)) => {
            if state.window.is_some() {
                return Task::none();
            }
            state.window = Some(id);
            let strategy = state.cfg.strategy;
            // THE call under test. The closure runs on iced's event-loop
            // thread, which on macOS is the main thread — a precondition for
            // every AppKit call the probe makes, and something the probe
            // asserts rather than assumes.
            window::run(id, move |handle| probe::run(handle, strategy)).map(Message::Probed)
        }
        Message::Probed(report) => {
            report.print();
            state.report = Some(report);
            Task::none()
        }
    }
}

fn view(state: &State) -> Element<'_, Message> {
    let status = match &state.report {
        None => "probing…".to_string(),
        Some(r) => r.verdict_line(),
    };
    center(
        column![
            text("aibo — S1 window handle probe").size(26),
            text(status).size(15),
            text("If you can see your desktop blurred behind this text,").size(13),
            text("the NSVisualEffectView composites BEHIND iced's layer: PASS.").size(13),
            text("If the background is plain black/clear with no blur, it does not.").size(13),
        ]
        .spacing(10),
    )
    .into()
}
