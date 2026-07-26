//! S10 — IME **into** aibo's panel (§20). **Critical path.**
//!
//! > Can a Japanese user type Japanese into an iced overlay window? `Ime`
//! > events, `set_ime_allowed`, `set_ime_cursor_area` candidate placement.
//! > **If it fails: Critical path — if this fails the Japanese market is closed
//! > and the UI stack decision reopens.** — §20
//!
//! §9 says the same thing in product terms:
//!
//! > Typing Japanese *into* aibo's own panel is a separate problem and a market
//! > blocker if it fails. It needs winit/iced IME support — `Ime` events,
//! > `set_ime_allowed`, and candidate-window placement via `set_ime_cursor_area`
//! > — which has historically been incomplete in iced and is made harder by an
//! > overlay window.
//!
//! ## What this binary is
//!
//! Two text inputs and a live event log. Every `Event::InputMethod` and every
//! `Event::Keyboard` is timestamped and shown on screen and on stdout, so the
//! operator can see the exact sequence the runtime produced while they typed
//! `にほんご` — and, critically, whether the preedit ever arrived at all.
//!
//! The `--overlay` flag is not decoration. §9 says an overlay window is what
//! makes this *harder*, and §8 says the platforms are asymmetric here: on macOS
//! a floating window can take key input, on Windows `WS_EX_NOACTIVATE` and a
//! text input are mutually exclusive. **A pass in a normal window and a fail in
//! the overlay is still a fail**, because the overlay is the product.
//!
//! ## What iced 0.14 actually provides
//!
//! Verified by reading the source, not assumed:
//!
//! - `iced_core::event::Event::InputMethod(input_method::Event)` with `Opened`,
//!   `Preedit(String, Option<Range<usize>>)`, `Commit(String)`, `Closed`.
//! - `iced_winit` maps winit's `Ime::{Enabled, Preedit, Commit, Disabled}` onto
//!   those, and calls `set_ime_allowed` / `set_ime_cursor_area` in response to a
//!   widget's `Shell::request_input_method`.
//! - `iced_widget::text_input` issues that request, so IME is driven by the
//!   focused widget rather than by the application.
//!
//! So the plumbing exists. What S10 has to establish is whether it *works* on a
//! wgpu surface, in an overlay, with a real Japanese input method — and whether
//! the candidate window lands next to the caret or in the corner of the screen.

use std::collections::VecDeque;
use std::time::Instant;

use iced::advanced::input_method;
use iced::widget::{Space, button, checkbox, column, container, row, scrollable, text, text_input};
use iced::{Element, Event, Length, Subscription, Task, window};

/// How many log lines to keep on screen.
const LOG_CAPACITY: usize = 200;

fn main() -> iced::Result {
    let overlay = std::env::args().any(|arg| arg == "--overlay");

    println!("S10 — IME into aibo's panel. Plan §9, §20.\n");
    if overlay {
        println!("OVERLAY MODE: level=AlwaysOnTop, undecorated, 680pt wide (§16's panel width).");
        println!("This is the mode that matters. A pass in a normal window is not a pass.\n");
    } else {
        println!("Normal window. Re-run with `--overlay` afterwards — that is the real test.\n");
    }
    println!("Add Japanese-Romaji in System Settings ▸ Keyboard ▸ Input Sources first.");
    println!("Then type: n i h o n g o, Space to convert, Return to commit.\n");
    println!("Every InputMethod and Keyboard event is echoed here as well as on screen.\n");

    let settings = window::Settings {
        // §16 pins the panel at 680 pt. §9 notes that fixed width is
        // localisation-hostile; the height is generous so the log is readable.
        size: iced::Size::new(680.0, 620.0),
        decorations: !overlay,
        level: if overlay {
            window::Level::AlwaysOnTop
        } else {
            window::Level::Normal
        },
        ..window::Settings::default()
    };

    iced::application(State::new, State::update, State::view)
        .title("S10 — IME into the panel")
        .subscription(State::subscription)
        .window(settings)
        .run()
}

/// One logged runtime event.
#[derive(Debug, Clone)]
struct LogLine {
    /// Milliseconds since launch.
    t_ms: u128,
    /// Rendered event.
    body: String,
    /// True for `InputMethod` events — the ones this spike is about.
    ime: bool,
}

/// The harness state.
struct State {
    started: Instant,
    /// The single-line input, the shape of the panel's prompt box.
    single: String,
    /// A second input, so focus movement mid-composition can be tested.
    second: String,
    /// Rolling event log.
    log: VecDeque<LogLine>,
    /// Whether keyboard events are logged too. On by default; noisy but it is
    /// the only way to see §9 rule 1 (the IME swallowing a key).
    log_keys: bool,
    /// Count of each IME event kind, so a summary is available without reading
    /// the whole log.
    opened: usize,
    preedits: usize,
    commits: usize,
    closed: usize,
    /// The last preedit content, kept because an empty preedit means "cleared"
    /// and is easy to miss scrolling past.
    last_preedit: String,
}

#[derive(Debug, Clone)]
enum Message {
    SingleChanged(String),
    SecondChanged(String),
    Runtime(Event),
    ToggleKeys(bool),
    Clear,
    Summarise,
}

impl State {
    fn new() -> (Self, Task<Message>) {
        (
            Self {
                started: Instant::now(),
                single: String::new(),
                second: String::new(),
                log: VecDeque::new(),
                log_keys: true,
                opened: 0,
                preedits: 0,
                commits: 0,
                closed: 0,
                last_preedit: String::new(),
            },
            // Focus the first input immediately: an unfocused text_input never
            // issues `request_input_method`, so without this the operator can
            // type for a minute and conclude, wrongly, that IME is broken.
            iced::widget::operation::focus(SINGLE_ID),
        )
    }

    fn push(&mut self, body: String, ime: bool) {
        let line = LogLine {
            t_ms: self.started.elapsed().as_millis(),
            body,
            ime,
        };
        println!(
            "{:>7} ms  {}{}",
            line.t_ms,
            if line.ime { "IME  " } else { "key  " },
            line.body
        );
        self.log.push_back(line);
        while self.log.len() > LOG_CAPACITY {
            self.log.pop_front();
        }
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::SingleChanged(value) => self.single = value,
            Message::SecondChanged(value) => self.second = value,
            Message::ToggleKeys(on) => self.log_keys = on,
            Message::Clear => {
                self.log.clear();
                self.opened = 0;
                self.preedits = 0;
                self.commits = 0;
                self.closed = 0;
                self.last_preedit.clear();
            }
            Message::Summarise => {
                let summary = format!(
                    "SUMMARY  Opened={} Preedit={} Commit={} Closed={}  single={:?}",
                    self.opened, self.preedits, self.commits, self.closed, self.single
                );
                self.push(summary, true);
                println!(
                    "\n  Answer these from the log above:\n\
                     \x20   • Did any Preedit event arrive at all? If Opened/Closed fire but\n\
                     \x20     Preedit never does, the composition is happening somewhere the\n\
                     \x20     runtime cannot see and the panel cannot render it inline.\n\
                     \x20   • Did the candidate window appear NEXT TO THE CARET, or somewhere\n\
                     \x20     else? That is set_ime_cursor_area, and it is the half of this\n\
                     \x20     spike no event log can answer — you have to look.\n\
                     \x20   • Did the committed text match what the candidate window showed?\n"
                );
            }
            Message::Runtime(event) => match event {
                Event::InputMethod(ime) => {
                    let body = match &ime {
                        input_method::Event::Opened => {
                            self.opened += 1;
                            "Opened — the runtime called set_ime_allowed(true)".to_owned()
                        }
                        input_method::Event::Preedit(content, selection) => {
                            self.preedits += 1;
                            self.last_preedit = content.clone();
                            if content.is_empty() {
                                "Preedit CLEARED (empty content)".to_owned()
                            } else {
                                format!(
                                    "Preedit {content:?}  selection={selection:?} \
                                     (byte-indexed)  {} chars",
                                    content.chars().count()
                                )
                            }
                        }
                        input_method::Event::Commit(content) => {
                            self.commits += 1;
                            format!("Commit {content:?}  {} chars", content.chars().count())
                        }
                        input_method::Event::Closed => {
                            self.closed += 1;
                            "Closed — set_ime_allowed(false)".to_owned()
                        }
                    };
                    self.push(body, true);
                }
                Event::Keyboard(keyboard) if self.log_keys => {
                    self.push(format!("{keyboard:?}"), false);
                }
                _ => {}
            },
        }
    }

    /// Listen to raw runtime events.
    ///
    /// `listen` (as opposed to `listen_with`) reports events the widgets already
    /// consumed as well, which is exactly what is wanted: the question is what
    /// the *runtime* produced, not what a widget chose to keep.
    fn subscription(&self) -> Subscription<Message> {
        iced::event::listen().map(Message::Runtime)
    }

    fn view(&self) -> Element<'_, Message> {
        let counters = text(format!(
            "Opened {}   Preedit {}   Commit {}   Closed {}   last preedit: {:?}",
            self.opened, self.preedits, self.commits, self.closed, self.last_preedit
        ))
        .size(13);

        let log = scrollable(
            column(self.log.iter().map(|line| {
                text(format!(
                    "{:>7} {} {}",
                    line.t_ms,
                    if line.ime { "IME " } else { "key " },
                    line.body
                ))
                .size(12)
                .into()
            }))
            .spacing(1)
            .width(Length::Fill),
        )
        .height(Length::Fill);

        container(
            column![
                text("S10 — type Japanese here").size(18),
                text("n i h o n g o → Space → Return.  Watch WHERE the candidate window appears.")
                    .size(12),
                text_input("prompt (the panel's input)", &self.single)
                    .id(SINGLE_ID)
                    .on_input(Message::SingleChanged)
                    .padding(10)
                    .size(16),
                text("Second field — start a composition above, then click here mid-composition.")
                    .size(12),
                text_input("second field", &self.second)
                    .id(SECOND_ID)
                    .on_input(Message::SecondChanged)
                    .padding(10)
                    .size(16),
                Space::new().height(6),
                row![
                    checkbox(self.log_keys)
                        .label("log key events")
                        .on_toggle(Message::ToggleKeys),
                    Space::new().width(Length::Fill),
                    button(text("summary").size(13)).on_press(Message::Summarise),
                    button(text("clear").size(13)).on_press(Message::Clear),
                ]
                .spacing(8),
                counters,
                log,
            ]
            .spacing(8)
            .padding(16),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}

/// Widget ids, so focus can be driven from `Task`s.
const SINGLE_ID: &str = "single";
/// See [`SINGLE_ID`].
const SECOND_ID: &str = "second";
