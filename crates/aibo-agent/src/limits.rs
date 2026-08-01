//! AgentLimits enforcement — mandatory, not advisory (§14).
//!
//! §14 is explicit: "Agent limits are mandatory, not advisory — a runaway loop
//! on a metered provider is a support incident." Codex's own limits apply too,
//! but aibo must not depend on them, so every backend in this crate drives the
//! same [`LimitTracker`] and stops with [`AiboError::BudgetExceeded`] the moment
//! a ceiling is crossed.
//!
//! Two properties matter and are easy to get wrong:
//!
//! - **The wall clock keeps running while nothing happens.** A tracker that is
//!   only consulted when an event arrives cannot stop a backend that has hung.
//!   [`LimitTracker::deadline`] exists so callers can arm a timer and race it
//!   against the event stream rather than polling.
//! - **Usage accumulates across turns, not per turn.**
//!   [`AgentLimits::max_total_tokens`] is a ceiling for the whole run, and §14
//!   also notes `Usage` never arrives on a cancelled or failed stream — so the
//!   tracker reports what it actually observed and never guesses.

use std::time::{Duration, Instant};

use aibo_core::error::AiboError;
use aibo_core::types::{AgentLimits, AgentOutcome, AgentStatus, BudgetKind, Usage};

/// Build the error a crossed ceiling produces.
///
/// Split out because every backend needs the exact same mapping and §13 gives
/// [`AiboError::BudgetExceeded`] a fixed treatment (inline, with a "continue
/// anyway" affordance).
pub const fn budget_error(kind: BudgetKind) -> AiboError {
    AiboError::BudgetExceeded { kind }
}

/// Add `delta` into `total`, field by field.
///
/// [`Usage`] is `Copy` and has no arithmetic impl in `aibo-core`; the agent loop
/// is the only place that needs to sum it, so the helper lives here.
pub fn accumulate(total: &mut Usage, delta: Usage) {
    total.input_tokens += delta.input_tokens;
    total.cached_input_tokens += delta.cached_input_tokens;
    total.output_tokens += delta.output_tokens;
    total.reasoning_tokens += delta.reasoning_tokens;
    total.image_tokens += delta.image_tokens;
}

/// Running enforcement state for one agent run (§14).
///
/// Cheap and synchronous by design: it is consulted on every step, every tool
/// call and every usage report, so it never allocates and never locks.
#[derive(Debug, Clone)]
pub struct LimitTracker {
    limits: AgentLimits,
    started: Instant,
    steps: u32,
    tool_calls: u32,
    usage: Usage,
    /// Set once a ceiling has been crossed, so a run cannot drift past it by
    /// accident. Clearing it is the explicit "continue anyway" path.
    exceeded: Option<BudgetKind>,
}

impl LimitTracker {
    /// Start tracking. The wall clock starts now, not at the first event.
    pub fn new(limits: AgentLimits) -> Self {
        Self {
            limits,
            started: Instant::now(),
            steps: 0,
            tool_calls: 0,
            usage: Usage::default(),
            exceeded: None,
        }
    }

    /// The ceilings being enforced.
    pub const fn limits(&self) -> AgentLimits {
        self.limits
    }

    /// Steps taken so far.
    pub const fn steps(&self) -> u32 {
        self.steps
    }

    /// Tool calls made so far.
    pub const fn tool_calls(&self) -> u32 {
        self.tool_calls
    }

    /// Tokens observed so far. Never an estimate — §14 reserves estimates at
    /// dispatch in `aibo-core`, not here.
    pub const fn usage(&self) -> Usage {
        self.usage
    }

    /// Time since the run started.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The instant at which [`AgentLimits::max_wall_clock`] is crossed.
    ///
    /// Callers should arm a timer on this and race it against the backend's
    /// event stream; a tracker consulted only on events cannot stop a hang.
    pub fn deadline(&self) -> Instant {
        self.started + self.limits.max_wall_clock
    }

    /// Which ceiling, if any, has already been crossed.
    pub const fn exceeded(&self) -> Option<BudgetKind> {
        self.exceeded
    }

    /// Charge one step. Call **before** doing the step's work.
    pub fn record_step(&mut self) -> Result<(), BudgetKind> {
        self.check_wall_clock()?;
        if self.steps >= self.limits.max_steps {
            return Err(self.trip(BudgetKind::Steps));
        }
        self.steps += 1;
        Ok(())
    }

    /// Charge one tool call. Call **before** the call has any side effect.
    pub fn record_tool_call(&mut self) -> Result<(), BudgetKind> {
        self.check_wall_clock()?;
        if self.tool_calls >= self.limits.max_tool_calls {
            return Err(self.trip(BudgetKind::Steps));
        }
        self.tool_calls += 1;
        Ok(())
    }

    /// Fold a usage report in and re-check the token ceiling.
    pub fn record_usage(&mut self, delta: Usage) -> Result<(), BudgetKind> {
        if let Some(kind) = self.exceeded {
            return Err(kind);
        }
        accumulate(&mut self.usage, delta);
        if self.usage.total() > self.limits.max_total_tokens {
            return Err(self.trip(BudgetKind::Tokens));
        }
        self.check_wall_clock()
    }

    /// Give back time spent waiting on the user.
    ///
    /// An approval prompt can sit for minutes while the user reads it, and
    /// that time is the *user's*, not the agent's — charging it to
    /// [`AgentLimits::max_wall_clock`] killed the first interactive run at
    /// 436 s of mostly waiting (observed 2026-08-01). The start instant moves
    /// forward by the wait, clamped so a bogus duration cannot push the
    /// deadline past "now plus the full budget".
    pub fn credit_wait(&mut self, wait: Duration) {
        let now = Instant::now();
        self.started = (self.started + wait).min(now);
    }

    /// Re-check the wall clock without charging anything.
    pub fn check_wall_clock(&mut self) -> Result<(), BudgetKind> {
        if let Some(kind) = self.exceeded {
            return Err(kind);
        }
        if self.started.elapsed() >= self.limits.max_wall_clock {
            // §14 has no dedicated wall-clock `BudgetKind`; `Steps` is the
            // step/tool-call/duration bucket and the UI copy names which one.
            return Err(self.trip(BudgetKind::Steps));
        }
        Ok(())
    }

    /// Raise the ceilings and clear the trip flag — the "continue anyway"
    /// button in §14. The elapsed clock is **not** reset: the user extended the
    /// budget, they did not start a new run.
    pub fn continue_anyway(&mut self, limits: AgentLimits) {
        self.limits = limits;
        self.exceeded = None;
    }

    /// Build the terminal payload for this run.
    pub fn outcome(&self, status: AgentStatus) -> AgentOutcome {
        AgentOutcome {
            status,
            usage: self.usage,
            steps: self.steps,
        }
    }

    /// Build the terminal payload for a run stopped by a ceiling.
    pub fn budget_outcome(&self, kind: BudgetKind) -> AgentOutcome {
        self.outcome(AgentStatus::BudgetExceeded(kind))
    }

    fn trip(&mut self, kind: BudgetKind) -> BudgetKind {
        self.exceeded = Some(kind);
        kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Approval waits are the user's time: crediting them moves the deadline
    /// out by the wait, and a wait longer than the elapsed run cannot move
    /// the start into the future.
    #[test]
    fn credited_waits_extend_the_deadline_but_never_past_now() {
        let mut t = LimitTracker::new(AgentLimits {
            max_wall_clock: Duration::from_secs(60),
            ..AgentLimits::default()
        });
        let before = t.deadline();
        t.credit_wait(Duration::from_millis(1));
        // A 1 ms credit on a run that has barely started clamps to "now":
        // the deadline may only move forward, and by at most the credit.
        assert!(t.deadline() >= before);
        assert!(t.deadline() <= before + Duration::from_secs(1));
        assert!(t.check_wall_clock().is_ok());

        // An absurd credit must not mint future start time.
        t.credit_wait(Duration::from_secs(3600));
        assert!(
            t.elapsed() <= Duration::from_secs(1),
            "started clamps to now"
        );
    }

    fn tight() -> AgentLimits {
        AgentLimits {
            max_steps: 2,
            max_tool_calls: 1,
            max_wall_clock: Duration::from_secs(60),
            max_total_tokens: 100,
        }
    }

    #[test]
    fn steps_are_capped() {
        let mut t = LimitTracker::new(tight());
        assert!(t.record_step().is_ok());
        assert!(t.record_step().is_ok());
        assert_eq!(t.record_step(), Err(BudgetKind::Steps));
        // Once tripped it stays tripped.
        assert_eq!(t.exceeded(), Some(BudgetKind::Steps));
        assert_eq!(t.record_usage(Usage::default()), Err(BudgetKind::Steps));
    }

    #[test]
    fn tokens_are_capped_on_the_total_not_the_turn() {
        let mut t = LimitTracker::new(tight());
        let turn = Usage {
            output_tokens: 60,
            ..Usage::default()
        };
        assert!(t.record_usage(turn).is_ok());
        assert_eq!(t.record_usage(turn), Err(BudgetKind::Tokens));
        assert_eq!(t.usage().total(), 120);
    }

    #[test]
    fn continue_anyway_clears_the_trip() {
        let mut t = LimitTracker::new(tight());
        assert!(t.record_tool_call().is_ok());
        assert!(t.record_tool_call().is_err());
        t.continue_anyway(AgentLimits {
            max_tool_calls: 10,
            ..tight()
        });
        assert!(t.record_tool_call().is_ok());
    }

    #[test]
    fn wall_clock_trips_without_any_event() {
        let mut t = LimitTracker::new(AgentLimits {
            max_wall_clock: Duration::ZERO,
            ..AgentLimits::default()
        });
        assert_eq!(t.check_wall_clock(), Err(BudgetKind::Steps));
    }
}
