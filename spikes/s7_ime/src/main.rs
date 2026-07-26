//! S7 — IME composition detection (§20).
//!
//! > Can composition state be detected cross-process on both OSes? What happens
//! > if you paste mid-composition in Japanese in Slack, VS Code, Word?
//! > **If it fails: block insert whenever the source app is in a known-IME
//! > state; document the limitation.** — §20
//!
//! §9 states the stakes and the known gap:
//!
//! > `FieldContext` carries an `ime_active` flag. If composition is active, aibo
//! > does not read the field and does not insert. […] Windows detection is
//! > `ImmGetContext` + `ImmGetCompositionString` on the foreground window.
//! > **macOS has no clean cross-process API for this**, which is why this is 🔴
//! > and why it needs spike S7 rather than a paragraph of confidence.
//!
//! So this binary does not claim to detect composition. It gathers evidence:
//!
//! - `source` — what Text Input Services reports (coarse: "an IME is selected").
//! - `watch` — a timestamped transcript of `AXValue` and `AXSelectedTextRange`
//!   while the operator types Japanese, which is how §9 rule 3 gets tested.
//! - `attributes` — hunt the focused element for any composition-related AX
//!   attribute. Finding one would beat §20's fallback outright.

#[cfg(target_os = "macos")]
mod ax;
#[cfg(target_os = "macos")]
mod tis;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!(
        "S7's macOS half only builds on macOS.\n\n\
         The Windows half is `ImmGetContext` + `ImmGetCompositionString` on the\n\
         foreground window (§9) — the platform where the answer is expected to be\n\
         clean. It is NOT implemented here. See README.md for what it must cover.\n\
         SPIKE: S7 — Windows composition detection."
    );
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    macos::run()
}

#[cfg(target_os = "macos")]
mod macos {
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use clap::{Parser, Subcommand};

    use crate::{ax, tis};

    #[derive(Parser, Debug)]
    #[command(
        name = "s7_ime",
        about = "S7 — IME composition detection probe (plan §9, §20)"
    )]
    struct Cli {
        /// Seconds before capture, so the operator can focus the target app.
        #[arg(long, default_value_t = 5)]
        delay: u64,
        /// AX messaging timeout, seconds.
        #[arg(long, default_value_t = 1.0)]
        ax_timeout: f32,
        #[command(subcommand)]
        command: Command,
    }

    #[derive(Subcommand, Debug)]
    enum Command {
        /// What Text Input Services reports right now.
        ///
        /// Run it once with a US layout selected and once with Japanese-Romaji
        /// selected. The difference is the whole of §20's fallback signal.
        Source {
            /// Keep printing whenever the input source changes. Switch layouts
            /// with ⌃Space / the menu bar and watch the transitions.
            #[arg(long)]
            watch: bool,
        },

        /// **The main experiment.** Sample the focused field while you type.
        ///
        /// Start it, focus the target, switch to Japanese input, and type
        /// `にほんご` slowly — then press Space to convert, then Return to commit.
        /// Every change to `AXValue` or `AXSelectedTextRange` is printed with a
        /// timestamp. Read the transcript against §9 rule 3.
        Watch {
            /// Sample interval, milliseconds.
            #[arg(long, default_value_t = 60)]
            interval: u64,
            /// How long to watch, seconds.
            #[arg(long, default_value_t = 40)]
            duration: u64,
            /// Write the transcript as JSONL.
            #[arg(long)]
            out: Option<std::path::PathBuf>,
        },

        /// List every AX attribute the focused element advertises, highlighting
        /// anything that could carry composition state.
        Attributes,
    }

    fn frontmost() -> Result<(String, String, i32)> {
        use objc2_app_kit::NSWorkspace;
        let app = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .context("no frontmost application")?;
        Ok((
            app.localizedName()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            app.bundleIdentifier()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            app.processIdentifier(),
        ))
    }

    fn countdown(seconds: u64, what: &str) {
        if seconds == 0 {
            return;
        }
        eprintln!("{what}");
        for remaining in (1..=seconds).rev() {
            eprint!("\r  starting in {remaining}s… ");
            std::thread::sleep(Duration::from_secs(1));
        }
        eprintln!("\r  go.                     ");
    }

    fn print_source(source: &tis::InputSource) {
        println!(
            "  id   {}\n  name {}\n  kind {}\n  can compose: {}   CJK IME: {}",
            source.id,
            source.name,
            source.kind,
            source.can_compose(),
            source.is_cjk()
        );
    }

    /// One sample of the focused field.
    #[derive(Debug, Clone, PartialEq, serde::Serialize)]
    struct Sample {
        /// Milliseconds since the watch started.
        t_ms: u128,
        /// `AXValue`, or `None` when unreadable.
        value: Option<String>,
        /// Length of `AXValue` in UTF-16 code units.
        value_utf16: Option<usize>,
        /// `AXSelectedTextRange` location, UTF-16 units.
        sel_location: Option<isize>,
        /// `AXSelectedTextRange` length, UTF-16 units.
        sel_length: Option<isize>,
        /// `AXSelectedText`.
        selected: Option<String>,
        /// Input source id at sample time.
        input_source: Option<String>,
    }

    impl Sample {
        /// Everything except the timestamp — used to suppress unchanged samples.
        fn same_state(&self, other: &Sample) -> bool {
            self.value == other.value
                && self.sel_location == other.sel_location
                && self.sel_length == other.sel_length
                && self.selected == other.selected
                && self.input_source == other.input_source
        }
    }

    fn watch(
        pid: i32,
        ax_timeout: f32,
        interval: u64,
        duration: u64,
        out: Option<std::path::PathBuf>,
    ) -> Result<()> {
        let app = ax::application(pid, ax_timeout).context("cannot create the app element")?;

        println!(
            "\n  Type now. Suggested script:\n\
             \x20   1. type  n i h o n g o   slowly — watch what appears here DURING composition\n\
             \x20   2. press Space to convert to kanji\n\
             \x20   3. press Return to commit\n\
             \x20   4. repeat, but press ⌘V (with something on the clipboard) at step 2\n\
             \x20      — §9 rule 2: \"synthetic paste during composition corrupts the buffer\"\n"
        );
        println!(
            "  {:>7}  {:>12}  {:>16}  value",
            "t (ms)", "sel", "value len (u16)"
        );
        println!("  {:->7}  {:->12}  {:->16}  {:-<50}", "", "", "", "");

        let started = Instant::now();
        let mut previous: Option<Sample> = None;
        let mut transcript: Vec<Sample> = Vec::new();

        while started.elapsed() < Duration::from_secs(duration) {
            // Re-read the focused element every sample: switching fields
            // mid-composition is one of the cases worth catching.
            let focused = app.focused();
            let sample = match &focused {
                None => Sample {
                    t_ms: started.elapsed().as_millis(),
                    value: None,
                    value_utf16: None,
                    sel_location: None,
                    sel_length: None,
                    selected: None,
                    input_source: tis::current().map(|s| s.id),
                },
                Some(element) => {
                    let value = element.text_value();
                    let range = element.selected_range();
                    Sample {
                        t_ms: started.elapsed().as_millis(),
                        value_utf16: value.as_deref().map(|v| v.encode_utf16().count()),
                        value,
                        sel_location: range.map(|r| r.0),
                        sel_length: range.map(|r| r.1),
                        selected: element.selected_text().filter(|s| !s.is_empty()),
                        input_source: tis::current().map(|s| s.id),
                    }
                }
            };

            let changed = previous.as_ref().is_none_or(|p| !p.same_state(&sample));
            if changed {
                println!(
                    "  {:>7}  {:>12}  {:>16}  {}",
                    sample.t_ms,
                    match (sample.sel_location, sample.sel_length) {
                        (Some(l), Some(n)) => format!("{l}+{n}"),
                        _ => "-".to_owned(),
                    },
                    sample
                        .value_utf16
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "-".to_owned()),
                    sample
                        .value
                        .as_deref()
                        .map(|v| {
                            let tail: String = v
                                .chars()
                                .rev()
                                .take(48)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect();
                            format!("…{}", tail.replace('\n', "\\n"))
                        })
                        .unwrap_or_else(|| "(unreadable)".to_owned())
                );
                transcript.push(sample.clone());
                previous = Some(sample);
            }
            std::thread::sleep(Duration::from_millis(interval));
        }

        println!("\n  {} state changes recorded.", transcript.len());
        println!(
            "\n  Now answer, from the transcript above:\n\
             \x20   • During composition, did AXValue show the uncommitted reading (かな),\n\
             \x20     the pre-composition text, or nothing at all? §9 rule 3 claims it is\n\
             \x20     one of the first two and that NEITHER is what the user sees.\n\
             \x20   • Was there ANY value of (sel_location, sel_length, value) that reliably\n\
             \x20     distinguishes 'composing' from 'not composing'? If yes, that beats\n\
             \x20     §20's fallback and is the headline finding.\n\
             \x20   • Did ⌘V mid-composition corrupt the buffer, get swallowed, or commit\n\
             \x20     the composition first?"
        );

        if let Some(path) = out {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&path)
                .with_context(|| format!("cannot create {}", path.display()))?;
            for sample in &transcript {
                writeln!(file, "{}", serde_json::to_string(sample)?)?;
            }
            eprintln!("transcript written to {}", path.display());
        }
        Ok(())
    }

    pub fn run() -> Result<()> {
        let cli = Cli::parse();

        match cli.command {
            Command::Source { watch: keep } => {
                if !keep {
                    match tis::current() {
                        Some(source) => print_source(&source),
                        None => println!("  TISCopyCurrentKeyboardInputSource returned null"),
                    }
                    return Ok(());
                }
                println!("Switch input sources (⌃Space, or the menu bar). ⌃C to stop.\n");
                let mut previous: Option<String> = None;
                loop {
                    if let Some(source) = tis::current()
                        && previous.as_deref() != Some(source.id.as_str())
                    {
                        println!("--");
                        print_source(&source);
                        previous = Some(source.id.clone());
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
            }

            Command::Watch {
                interval,
                duration,
                out,
            } => {
                countdown(
                    cli.delay,
                    "Focus the target app, put the caret in a text field, and switch to Japanese input.",
                );
                let (name, bundle_id, pid) = frontmost()?;
                println!("\n== {name} ({bundle_id}) pid {pid} ==");
                if let Some(source) = tis::current() {
                    println!();
                    print_source(&source);
                    if !source.can_compose() {
                        println!(
                            "\n  WARNING: a plain keyboard layout is selected. Switch to a Japanese\n\
                             input source or this transcript shows nothing interesting."
                        );
                    }
                }
                watch(pid, cli.ax_timeout, interval, duration, out)
            }

            Command::Attributes => {
                countdown(
                    cli.delay,
                    "Focus the target app and put the caret in a text field.",
                );
                let (name, bundle_id, pid) = frontmost()?;
                println!("\n== {name} ({bundle_id}) pid {pid} ==\n");

                let app = ax::application(pid, cli.ax_timeout)
                    .context("cannot create the app element")?;
                let focused = app.focused().context(
                    "no focused UI element — the app publishes no AX tree, or Accessibility \
                     is not granted to this binary",
                )?;

                let names = focused.attribute_names();
                println!("  {} attributes advertised:", names.len());
                let mut interesting = Vec::new();
                for attribute in &names {
                    let lower = attribute.to_lowercase();
                    let hit = lower.contains("mark")
                        || lower.contains("composi")
                        || lower.contains("input")
                        || lower.contains("candidate");
                    println!("    {}{attribute}", if hit { ">> " } else { "   " });
                    if hit {
                        interesting.push(attribute.clone());
                    }
                }

                println!();
                if interesting.is_empty() {
                    println!(
                        "  Nothing composition-related. This is the expected result and it is\n\
                         §9's claim restated as data: macOS publishes no cross-process marked-text\n\
                         attribute. Record it for this app and move on."
                    );
                } else {
                    println!(
                        "  CANDIDATES: {interesting:?}\n\
                         Read each one with `watch` running and see whether it changes during\n\
                         composition. If one does, S7 has a real answer instead of §20's fallback,\n\
                         and §9's 🔴 can come down for this app class."
                    );
                }
                Ok(())
            }
        }
    }
}
