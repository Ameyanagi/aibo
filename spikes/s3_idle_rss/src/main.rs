//! # S3 — warm-surface idle footprint
//!
//! Throwaway binary for risk-register item **S3** (docs/plan.md §20), which
//! asks:
//!
//! > Does a hidden pre-created iced window hold ≤ 60 MB idle on both OSes?
//!
//! and whose stated fallback is *"drop the surface when idle, accept ~200 ms
//! first-show"*. §15's idle-footprint row (**≤ 100 MB, stretch 60**) is
//! explicitly flagged as an unmeasured aspiration that S3 must replace with a
//! real number. This binary produces that number.
//!
//! ## Design
//!
//! A minimal `iced::daemon` — no widgets worth speaking of, no state, no
//! providers — that runs in three phases inside a single process so the
//! before/after delta is measured on identical conditions:
//!
//! | Phase | What is alive |
//! |---|---|
//! | `baseline` | the iced event loop only, **no window at all** |
//! | `warming`  | the window has just been created; wgpu is still settling |
//! | `steady`   | the pre-created window has been alive for `--settle` seconds |
//!
//! The number that answers S3 is **`steady` p50 minus `baseline` p50**: that is
//! the marginal cost of keeping the surface warm, which is the thing the plan
//! is actually deciding about.
//!
//! Run it with `--release`; a debug build's footprint means nothing.

mod footprint;

use std::time::{Duration, Instant};

use footprint::{PRIMARY_LABEL, SECONDARY_LABEL, Sample};
use iced::widget::{column, text};
use iced::{Element, Subscription, Task, window};

/// Which surface the spike keeps alive during the measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No window is ever created — the baseline the other modes are compared to.
    None,
    /// A pre-created window with `visible: false` — the warm-surface trick S3 tests.
    Hidden,
    /// A pre-created, actually-shown window — the upper bound.
    Visible,
}

impl Mode {
    /// Parse `--mode <none|hidden|visible>`.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "hidden" => Some(Self::Hidden),
            "visible" => Some(Self::Visible),
            _ => None,
        }
    }

    /// Human name used in the report.
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Hidden => "hidden",
            Self::Visible => "visible",
        }
    }
}

/// Command-line configuration.
#[derive(Debug, Clone, Copy)]
struct Config {
    mode: Mode,
    /// Total wall-clock run time.
    total: Duration,
    /// Seconds spent with no window before the window is created.
    baseline: Duration,
    /// Seconds after window creation that are discarded as `warming`.
    settle: Duration,
    /// Sampling interval.
    interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Hidden,
            total: Duration::from_secs(10 * 60),
            baseline: Duration::from_secs(60),
            settle: Duration::from_secs(60),
            interval: Duration::from_secs(5),
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
            let next = |i: usize| -> Option<u64> { args.get(i + 1).and_then(|v| v.parse().ok()) };
            match args[i].as_str() {
                "--mode" => {
                    if let Some(m) = args.get(i + 1).and_then(|s| Mode::parse(s)) {
                        cfg.mode = m;
                    }
                    i += 1;
                }
                "--minutes" => {
                    if let Some(v) = next(i) {
                        cfg.total = Duration::from_secs(v * 60);
                    }
                    i += 1;
                }
                "--seconds" => {
                    if let Some(v) = next(i) {
                        cfg.total = Duration::from_secs(v);
                    }
                    i += 1;
                }
                "--baseline" => {
                    if let Some(v) = next(i) {
                        cfg.baseline = Duration::from_secs(v);
                    }
                    i += 1;
                }
                "--settle" => {
                    if let Some(v) = next(i) {
                        cfg.settle = Duration::from_secs(v);
                    }
                    i += 1;
                }
                "--interval" => {
                    if let Some(v) = next(i) {
                        cfg.interval = Duration::from_secs(v.max(1));
                    }
                    i += 1;
                }
                other => eprintln!("s3: ignoring unknown argument {other:?}"),
            }
            i += 1;
        }
        cfg
    }
}

/// Messages driving the daemon.
#[derive(Debug, Clone)]
enum Message {
    /// A sampling tick.
    Tick,
    /// The pre-created window finished opening.
    Opened(window::Id),
}

/// Which phase a sample belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Baseline,
    Warming,
    Steady,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Warming => "warming",
            Self::Steady => "steady",
        }
    }
}

/// One recorded row.
struct Row {
    at: Duration,
    phase: Phase,
    sample: Sample,
}

/// Daemon state.
struct State {
    cfg: Config,
    started: Instant,
    /// Sample taken before `iced` was touched at all.
    pre_iced: Sample,
    window: Option<window::Id>,
    /// When the window finished opening.
    opened_at: Option<Duration>,
    /// Set once the open task has been issued, so it is issued exactly once.
    open_requested: bool,
    rows: Vec<Row>,
    finished: bool,
}

impl State {
    /// Classify a sample taken `at` into a phase.
    ///
    /// Note the `open_requested` arm: `window::open` can block the event loop
    /// for a long time on a cold wgpu pipeline cache, and every sample drained
    /// afterwards carries a post-open timestamp. Without this arm they are
    /// filed under `baseline` and poison its p95 with the creation spike.
    fn phase_at(&self, at: Duration) -> Phase {
        match self.opened_at {
            None if self.open_requested => Phase::Warming,
            None => Phase::Baseline,
            Some(opened) if at.saturating_sub(opened) < self.cfg.settle => Phase::Warming,
            Some(_) => Phase::Steady,
        }
    }
}

fn main() -> iced::Result {
    let cfg = Config::from_args();
    // Sample *before* iced is constructed, so the report can separate "the cost
    // of being a Rust process at all" from "the cost of the iced event loop".
    let pre_iced = footprint::sample();

    println!("# S3 — warm-surface idle footprint");
    println!("# mode={} total={:?} baseline={:?} settle={:?} interval={:?}",
        cfg.mode.name(), cfg.total, cfg.baseline, cfg.settle, cfg.interval);
    println!("# os={} arch={} profile={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(debug_assertions) { "debug (NUMBERS ARE MEANINGLESS)" } else { "release" });
    println!("# primary={PRIMARY_LABEL} secondary={SECONDARY_LABEL}");
    println!("# columns: t_s phase {PRIMARY_LABEL}_mb {SECONDARY_LABEL}_mb cpu_ms");
    println!("PRE_ICED 0.0 pre {:.1} {:.1} {:.0}",
        mb(pre_iced.footprint_bytes), mb(pre_iced.secondary_bytes), ms(pre_iced.cpu_ns()));

    iced::daemon(
        move || State {
            cfg,
            started: Instant::now(),
            pre_iced,
            window: None,
            opened_at: None,
            open_requested: false,
            rows: Vec::new(),
            finished: false,
        },
        update,
        view,
    )
    .title(|_state: &State, _id| "aibo S3 warm-surface probe".to_string())
    .subscription(subscription)
    .run()
}

/// Tick on the configured interval. Nothing else — an idle daemon must stay idle.
fn subscription(state: &State) -> Subscription<Message> {
    iced::time::every(state.cfg.interval).map(|_| Message::Tick)
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Opened(id) => {
            let at = state.started.elapsed();
            state.window = Some(id);
            state.opened_at = Some(at);
            println!("# window opened at t={:.1}s id={id:?}", at.as_secs_f64());
            Task::none()
        }
        Message::Tick => {
            if state.finished {
                return Task::none();
            }
            let at = state.started.elapsed();
            let sample = footprint::sample();
            let phase = state.phase_at(at);
            println!(
                "SAMPLE {:.1} {} {:.1} {:.1} {:.0}",
                at.as_secs_f64(),
                phase.name(),
                mb(sample.footprint_bytes),
                mb(sample.secondary_bytes),
                ms(sample.cpu_ns()),
            );
            state.rows.push(Row { at, phase, sample });

            if at >= state.cfg.total {
                state.finished = true;
                report(state);
                return iced::exit();
            }

            if !state.open_requested && state.cfg.mode != Mode::None && at >= state.cfg.baseline {
                state.open_requested = true;
                let visible = state.cfg.mode == Mode::Visible;
                // The warm-surface trick under test: create the window up front
                // and simply never show it (§15 "warm surface only for the
                // pre-created panel").
                let settings = window::Settings {
                    size: iced::Size::new(720.0, 420.0),
                    visible,
                    decorations: false,
                    transparent: true,
                    level: window::Level::AlwaysOnTop,
                    ..window::Settings::default()
                };
                let (_id, task) = window::open(settings);
                return task.map(Message::Opened);
            }

            Task::none()
        }
    }
}

fn view(state: &State, _id: window::Id) -> Element<'_, Message> {
    // Deliberately trivial: S3 measures the surface, not a UI.
    column![
        text("aibo S3 warm-surface probe").size(20),
        text(format!("{} samples", state.rows.len())).size(14),
    ]
    .spacing(8)
    .into()
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Bytes → MiB.
fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Nanoseconds → milliseconds.
fn ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

/// Percentile of a sorted slice, nearest-rank.
fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Summarise one phase.
fn phase_stats(rows: &[Row], phase: Phase) -> Option<(usize, f64, f64, f64, f64)> {
    let mut v: Vec<f64> = rows
        .iter()
        .filter(|r| r.phase == phase)
        .map(|r| mb(r.sample.footprint_bytes))
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some((v.len(), v[0], pct(&v, 50.0), pct(&v, 95.0), v[v.len() - 1]))
}

/// Print the closing verdict block.
fn report(state: &State) {
    println!("\n--- S3 report ---------------------------------------------");
    println!("mode                  {}", state.cfg.mode.name());
    println!("samples               {}", state.rows.len());
    println!("pre-iced {PRIMARY_LABEL}  {:.1} MB", mb(state.pre_iced.footprint_bytes));

    let mut baseline_p50 = f64::NAN;
    let mut steady_p50 = f64::NAN;
    for phase in [Phase::Baseline, Phase::Warming, Phase::Steady] {
        match phase_stats(&state.rows, phase) {
            Some((n, min, p50, p95, max)) => {
                println!(
                    "{:<9} n={:<4} min={:>7.1} p50={:>7.1} p95={:>7.1} max={:>7.1}  MB",
                    phase.name(),
                    n,
                    min,
                    p50,
                    p95,
                    max
                );
                if phase == Phase::Baseline {
                    baseline_p50 = p50;
                }
                if phase == Phase::Steady {
                    steady_p50 = p50;
                }
            }
            None => println!("{:<9} (no samples)", phase.name()),
        }
    }

    // Idle CPU over the steady phase (§15 claims ~0.1–0.3% on macOS).
    let steady: Vec<&Row> = state.rows.iter().filter(|r| r.phase == Phase::Steady).collect();
    if let (Some(first), Some(last)) = (steady.first(), steady.last())
        && last.at > first.at
    {
        let cpu_ms = ms(last.sample.cpu_ns().saturating_sub(first.sample.cpu_ns()));
        let wall_ms = (last.at - first.at).as_secs_f64() * 1000.0;
        println!("idle CPU (steady)     {:.3}% over {:.0}s", 100.0 * cpu_ms / wall_ms, wall_ms / 1000.0);
    }

    if baseline_p50.is_finite() && steady_p50.is_finite() {
        println!("\nWARM SURFACE COST     {:+.1} MB   (steady p50 - baseline p50)", steady_p50 - baseline_p50);
        println!("IDLE FOOTPRINT        {steady_p50:.1} MB  (§15 budget: <= 100 MB, stretch 60)");
        println!(
            "VERDICT               {}",
            if steady_p50 <= 60.0 {
                "PASS — meets the stretch budget; keep the pre-created surface"
            } else if steady_p50 <= 100.0 {
                "PASS (budget), FAIL (stretch) — rewrite the §15 row with this number"
            } else {
                "FAIL — take the S3 fallback: drop the surface when idle, accept ~200 ms first-show"
            }
        );
    }
    println!(
        "\nNOT MEASURED HERE: GPU memory (§15 says report it separately) and the\n\
         whole-process-tree number (a resident `codex app-server` is not free).\n\
         SPIKE: S3 — pair this with `vmmap --summary <pid>` for the GPU/IOKit split."
    );
    println!("-----------------------------------------------------------");
}
