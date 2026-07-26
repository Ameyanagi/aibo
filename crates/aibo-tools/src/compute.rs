//! Tier 0: `fend-core` (§1 Compute, §11 tier 0).
//!
//! The Compute surface is the cheapest "feels magic" win in the product: if the
//! panel input parses as an expression, the answer appears above the model
//! suggestions with no model call at all, in ≤ 1 ms (§1).
//!
//! # Two corrections the plan makes, encoded here
//!
//! * **fend's date syntax is `@DATE ± period`**, not natural language.
//!   `@2026-08-01 - 3 weeks` works; "3 weeks before August 1st" does not. UI
//!   copy must not promise the latter — [`DATE_SYNTAX_HINT`] is the string to
//!   show instead.
//! * **Currency conversion is not offline.** fend has no rates of its own; the
//!   host supplies them. [`Compute::new`] therefore has *no* rate source and
//!   currency conversion fails closed with a legible message. Install one with
//!   [`Compute::with_rates`] and surface [`RateFreshness`] next to the answer,
//!   because a cached rate presented as live is a wrong answer with a
//!   confident face. §13's "Compute works with no network" claim holds only
//!   because of this split.
//!
//! # A third, found in the crate rather than the plan
//!
//! `fend_core::Context::set_current_time_v1` is a **no-op** in 1.5.8 — its body
//! is commented out upstream and it assigns `None`. So `now`, `today` and
//! anything relative to the current instant do not evaluate; only absolute
//! `@DATE` arithmetic works. [`Compute::supports_relative_dates`] reports this
//! so the UI can hide an affordance rather than ship a failing one.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use aibo_core::types::{ToolSchema, ToolTier};
use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::args::str_arg;
use crate::{Tool, ToolError, ToolOutput, ToolResult};

/// The date syntax to show users, per §1's first correction.
pub const DATE_SYNTAX_HINT: &str = "@2026-08-01 - 3 weeks";

/// §1's headline example, in the form fend actually accepts.
///
/// The plan writes it as `120 GB / 8 Mbps to hours`. fend 1.5.8 rejects that:
/// implicit multiplication pulls `8 Mbps` into the numerator and the units come
/// out as `bit^2 / second`. The divisor needs parentheses. UI copy and any
/// onboarding example must use this string, not the plan's.
pub const DIVISION_EXAMPLE: &str = "120 GB / (8 Mbps) to hours";

/// Longest a single Compute evaluation may run before it is interrupted.
///
/// §1 budgets ≤ 1 ms for the answer; this is the failsafe for a pathological
/// input, not the target.
pub const DEFAULT_DEADLINE: Duration = Duration::from_millis(50);

/// Why an expression did not produce an answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ComputeError {
    /// The input is not an expression fend understands. This is the common,
    /// boring case: it means "not a Compute input", not "an error occurred",
    /// and must never be surfaced to the user as a failure.
    #[error("not an expression: {0}")]
    NotAnExpression(String),

    /// Evaluation hit the deadline.
    #[error("compute timed out")]
    Timeout,

    /// The expression needs exchange rates and none are installed, or the
    /// installed source does not know the currency (§1).
    #[error("currency conversion needs exchange rates: {0}")]
    RatesUnavailable(String),
}

impl From<ComputeError> for ToolError {
    fn from(e: ComputeError) -> Self {
        match e {
            ComputeError::Timeout => ToolError::Sandbox {
                tier: 0,
                reason: aibo_core::error::SandboxFailure::Timeout,
            },
            other => ToolError::Failed {
                tool: "compute".to_owned(),
                message: other.to_string(),
            },
        }
    }
}

/// How trustworthy the exchange rates behind an answer are (§1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateFreshness {
    /// The answer did not involve currency.
    NotUsed,
    /// Rates were available, with the timestamp they were fetched at.
    Cached {
        /// When the rate table was fetched.
        as_of: SystemTime,
        /// Older than the caller's staleness threshold.
        stale: bool,
    },
}

/// A host-supplied exchange-rate table.
///
/// Deliberately synchronous and non-blocking: it is consulted from inside a
/// fend evaluation on the Compute path, which has a 1 ms budget. Fetching
/// belongs elsewhere; implementations here only read what was already fetched.
pub trait ExchangeRates: Send + Sync + std::fmt::Debug {
    /// Value of `currency` relative to the table's base currency, or `None`
    /// when unknown. Any consistent base works.
    fn relative_to_base(&self, currency: &str) -> Option<f64>;

    /// When the table was fetched.
    fn as_of(&self) -> SystemTime;
}

/// A fixed table of rates with a fetch timestamp.
///
/// This is the shippable form of §1's "ship timestamped cached rates and label
/// them stale".
#[derive(Debug, Clone)]
pub struct CachedRates {
    rates: std::collections::BTreeMap<String, f64>,
    as_of: SystemTime,
}

impl CachedRates {
    /// Build a table. The base currency must map to `1.0`; that is the
    /// caller's contract, not something fend checks.
    pub fn new(rates: impl IntoIterator<Item = (String, f64)>, as_of: SystemTime) -> Self {
        Self {
            rates: rates.into_iter().collect(),
            as_of,
        }
    }

    /// Whether the table is older than `max_age`.
    pub fn is_stale(&self, max_age: Duration) -> bool {
        self.as_of.elapsed().map(|d| d > max_age).unwrap_or(true)
    }
}

impl ExchangeRates for CachedRates {
    fn relative_to_base(&self, currency: &str) -> Option<f64> {
        self.rates.get(currency).copied()
    }

    fn as_of(&self) -> SystemTime {
        self.as_of
    }
}

/// Bridges [`ExchangeRates`] onto fend's handler trait.
#[derive(Debug)]
struct RateAdapter(Arc<dyn ExchangeRates>);

impl fend_core::ExchangeRateFnV2 for RateAdapter {
    fn relative_to_base_currency(
        &self,
        currency: &str,
        _options: &fend_core::ExchangeRateFnV2Options,
    ) -> Result<f64, Box<dyn std::error::Error + Send + Sync + 'static>> {
        match self.0.relative_to_base(currency) {
            Some(rate) => Ok(rate),
            None => {
                let e: Box<dyn std::error::Error + Send + Sync> =
                    format!("no cached exchange rate for {currency}").into();
                Err(e)
            }
        }
    }
}

/// Interrupt that fires once a deadline passes.
///
/// fend evaluation is synchronous, so this is the only bound available. It is
/// checked cooperatively — exactly like the rquickjs interrupt in tier 1, and
/// with the same caveat: a single non-yielding step can overrun it.
struct Deadline {
    end: Instant,
}

impl fend_core::Interrupt for Deadline {
    fn should_interrupt(&self) -> bool {
        Instant::now() >= self.end
    }
}

/// One evaluated expression.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputeAnswer {
    /// The input as given.
    pub input: String,
    /// fend's rendered answer.
    pub result: String,
    /// fend's "hide this output by default" flag — set when the result would
    /// just be the unit `()` type. The panel should not render a Compute
    /// answer at all when this is true.
    pub output_is_empty: bool,
    /// Rate provenance, for the "rates from …" footnote (§1).
    pub rates: RateFreshness,
}

/// The Compute evaluator.
///
/// Holds a fend context so variables defined in one input (`a = 2; 5a`) survive
/// into the next within a session.
pub struct Compute {
    ctx: fend_core::Context,
    rates: Option<Arc<dyn ExchangeRates>>,
    staleness_threshold: Duration,
    deadline: Duration,
}

impl std::fmt::Debug for Compute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compute")
            .field("has_rates", &self.rates.is_some())
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl Default for Compute {
    fn default() -> Self {
        Self::new()
    }
}

impl Compute {
    /// An evaluator with **no** exchange rates: currency conversion fails
    /// closed rather than answering from data that was never supplied (§1).
    pub fn new() -> Self {
        let mut ctx = fend_core::Context::new();
        // Deterministic: `roll d6` in a hotkey panel is a novelty, and an RNG
        // makes the Compute path untestable and its answers uncacheable.
        ctx.disable_rng();
        Self {
            ctx,
            rates: None,
            staleness_threshold: Duration::from_secs(24 * 60 * 60),
            deadline: DEFAULT_DEADLINE,
        }
    }

    /// An evaluator with a host-supplied rate table.
    pub fn with_rates(rates: Arc<dyn ExchangeRates>) -> Self {
        let mut this = Self::new();
        this.ctx
            .set_exchange_rate_handler_v2(RateAdapter(Arc::clone(&rates)));
        this.rates = Some(rates);
        this
    }

    /// How old rates may be before answers are labelled stale. Default 24 h.
    pub fn set_staleness_threshold(&mut self, max_age: Duration) {
        self.staleness_threshold = max_age;
    }

    /// Override the evaluation deadline.
    pub fn set_deadline(&mut self, deadline: Duration) {
        self.deadline = deadline;
    }

    /// Whether `now` / `today` style relative dates evaluate.
    ///
    /// Always `false` on fend-core 1.5.8: `set_current_time_v1` is a documented
    /// no-op there ("This method currently has no effect!"). Kept as a method
    /// so call sites need not know that, and so a version bump that fixes it is
    /// a one-line change here.
    pub const fn supports_relative_dates(&self) -> bool {
        false
    }

    /// Evaluate, updating session variables on success.
    pub fn evaluate(&mut self, input: &str) -> Result<ComputeAnswer, ComputeError> {
        let interrupt = Deadline {
            end: Instant::now() + self.deadline,
        };
        let started = Instant::now();
        match fend_core::evaluate_with_interrupt(input, &mut self.ctx, &interrupt) {
            Ok(res) => {
                if res.get_main_result().is_empty() {
                    return Err(ComputeError::NotAnExpression(input.to_owned()));
                }
                Ok(ComputeAnswer {
                    input: input.to_owned(),
                    result: res.get_main_result().to_owned(),
                    output_is_empty: res.output_is_empty(),
                    rates: self.freshness_for(input),
                })
            }
            Err(msg) => Err(self.classify(&msg, started)),
        }
    }

    /// Evaluate **without** touching session state, for the live preview the
    /// panel renders while the user types (§1).
    ///
    /// fend filters preview results itself: overly long output, multi-line
    /// output and bare unit types come back empty and are reported as `None`.
    pub fn preview(&self, input: &str) -> Option<ComputeAnswer> {
        let interrupt = Deadline {
            end: Instant::now() + self.deadline,
        };
        let res = fend_core::evaluate_preview_with_interrupt(input, &self.ctx, &interrupt);
        if res.get_main_result().is_empty() {
            return None;
        }
        Some(ComputeAnswer {
            input: input.to_owned(),
            result: res.get_main_result().to_owned(),
            output_is_empty: res.output_is_empty(),
            rates: self.freshness_for(input),
        })
    }

    /// Whether this input should route to Compute at all (§1's "if the panel
    /// input parses as an expression"). Non-mutating.
    pub fn is_expression(&self, input: &str) -> bool {
        !input.trim().is_empty() && self.preview(input).is_some()
    }

    fn freshness_for(&self, input: &str) -> RateFreshness {
        // fend does not report whether the rate handler was consulted, so
        // provenance is attached whenever rates are installed and the input
        // could name a currency. Over-reporting is the safe direction: a
        // footnote saying where rates came from is never harmful; a missing one
        // is exactly the failure §1 warns about.
        let Some(rates) = self.rates.as_ref() else {
            return RateFreshness::NotUsed;
        };
        let currency_ish = input
            .chars()
            .any(|c| c.is_ascii_alphabetic() || matches!(c, '$' | '£' | '€' | '¥'));
        if !currency_ish {
            return RateFreshness::NotUsed;
        }
        let as_of = rates.as_of();
        let stale = as_of
            .elapsed()
            .map(|d| d > self.staleness_threshold)
            .unwrap_or(true);
        RateFreshness::Cached { as_of, stale }
    }

    fn classify(&self, msg: &str, started: Instant) -> ComputeError {
        if started.elapsed() >= self.deadline {
            return ComputeError::Timeout;
        }
        let lowered = msg.to_lowercase();
        if lowered.contains("exchange rate") || lowered.contains("currency") {
            return ComputeError::RatesUnavailable(msg.to_owned());
        }
        ComputeError::NotAnExpression(msg.to_owned())
    }
}

/// Compute exposed as a tier-0 tool, for the Do surface.
///
/// The panel calls [`Compute`] directly — routing through the registry would
/// add a JSON round-trip to a 1 ms budget. This wrapper exists so an agent run
/// can do arithmetic without a model call.
#[derive(Debug)]
pub struct ComputeTool {
    inner: std::sync::Mutex<Compute>,
}

impl Default for ComputeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ComputeTool {
    /// A tool with no exchange rates.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(Compute::new()),
        }
    }

    /// A tool with host-supplied rates.
    pub fn with_rates(rates: Arc<dyn ExchangeRates>) -> Self {
        Self {
            inner: std::sync::Mutex::new(Compute::with_rates(rates)),
        }
    }
}

#[async_trait]
impl Tool for ComputeTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "compute".to_owned(),
            description: format!(
                "Evaluate a units-aware expression: arithmetic, unit and base conversion, and \
                 absolute date arithmetic written as `{DATE_SYNTAX_HINT}`. Currency needs \
                 host-supplied rates and fails when none are configured."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "expression": {
                        "type": "string",
                        "examples": [DIVISION_EXAMPLE, DATE_SYNTAX_HINT, "0xff to decimal"]
                    }
                },
                "required": ["expression"]
            }),
            tier: 0,
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Builtin
    }

    async fn call(&self, args: serde_json::Value, _c: CancellationToken) -> ToolResult<ToolOutput> {
        let expression = str_arg(&args, "compute", "expression")?;
        // The critical section is bounded by `deadline`. A poisoned lock means
        // a panic inside fend, which is a bug worth surfacing rather than
        // papering over with a fresh context.
        let mut guard = self.inner.lock().map_err(|_| ToolError::Failed {
            tool: "compute".to_owned(),
            message: "compute context poisoned by an earlier panic".to_owned(),
        })?;
        match guard.evaluate(expression) {
            Ok(answer) => {
                let stale = matches!(answer.rates, RateFreshness::Cached { stale: true, .. });
                Ok(ToolOutput::json(
                    answer.result.clone(),
                    json!({
                        "result": answer.result,
                        "output_is_empty": answer.output_is_empty,
                        "rates_stale": stale,
                    }),
                ))
            }
            // "Not an expression" is a normal answer for a tool the model
            // pointed at prose, so it comes back as a tool-visible error rather
            // than a transport failure that aborts the run.
            Err(e @ ComputeError::NotAnExpression(_)) => Ok(ToolOutput::error(e.to_string())),
            Err(other) => Err(other.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_and_units() {
        let mut c = Compute::new();
        assert_eq!(c.evaluate("1 + 1").unwrap().result, "2");
        // §1 writes this example as `120 GB / 8 Mbps to hours`, which fend
        // 1.5.8 rejects: implicit multiplication binds `8 Mbps` into the
        // numerator, giving `bit^2 / second`. The parenthesised form is the one
        // that evaluates, and it is the one UI copy must show.
        assert!(c.evaluate("120 GB / 8 Mbps to hours").is_err());
        let net = c.evaluate("120 GB / (8 Mbps) to hours").unwrap();
        assert!(net.result.contains("hour"), "{}", net.result);
    }

    #[test]
    fn base_conversion() {
        let mut c = Compute::new();
        assert!(
            c.evaluate("0xff to decimal")
                .unwrap()
                .result
                .contains("255")
        );
    }

    #[test]
    fn the_documented_date_syntax_is_the_one_that_works() {
        let mut c = Compute::new();
        let ok = c.evaluate(DATE_SYNTAX_HINT).unwrap();
        assert!(ok.result.contains("2026"), "{}", ok.result);
        // §1: the natural-language form must not be promised in UI copy.
        assert!(c.evaluate("3 weeks before August 1st").is_err());
    }

    #[test]
    fn relative_dates_are_not_supported_and_we_say_so() {
        let c = Compute::new();
        assert!(!c.supports_relative_dates());
    }

    #[test]
    fn currency_fails_closed_without_host_rates() {
        let mut c = Compute::new();
        assert!(c.evaluate("10 USD to EUR").is_err());
    }

    #[test]
    fn currency_works_and_is_labelled_once_rates_are_supplied() {
        let rates = CachedRates::new(
            [("USD".to_owned(), 1.0), ("EUR".to_owned(), 0.5)],
            SystemTime::now(),
        );
        let mut c = Compute::with_rates(Arc::new(rates));
        let answer = c.evaluate("10 USD to EUR").unwrap();
        assert!(answer.result.contains('5'), "{}", answer.result);
        assert!(matches!(
            answer.rates,
            RateFreshness::Cached { stale: false, .. }
        ));
    }

    #[test]
    fn old_rates_are_reported_as_stale() {
        let old = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 30);
        let rates = CachedRates::new([("USD".to_owned(), 1.0), ("EUR".to_owned(), 0.5)], old);
        assert!(rates.is_stale(Duration::from_secs(3600)));
        let mut c = Compute::with_rates(Arc::new(rates));
        let answer = c.evaluate("10 USD to EUR").unwrap();
        assert!(matches!(
            answer.rates,
            RateFreshness::Cached { stale: true, .. }
        ));
    }

    #[test]
    fn an_unknown_currency_still_fails_with_rates_installed() {
        let rates = CachedRates::new([("USD".to_owned(), 1.0)], SystemTime::now());
        let mut c = Compute::with_rates(Arc::new(rates));
        assert!(c.evaluate("10 USD to XYZ").is_err());
    }

    #[test]
    fn prose_is_not_an_expression() {
        let c = Compute::new();
        assert!(!c.is_expression("summarise this paragraph for me"));
        assert!(!c.is_expression(""));
        assert!(c.is_expression("2 + 2"));
    }

    #[test]
    fn preview_does_not_mutate_session_state() {
        let mut c = Compute::new();
        c.evaluate("a = 7").unwrap();
        let _ = c.preview("a = 99");
        assert_eq!(c.evaluate("a").unwrap().result, "7");
    }

    #[tokio::test]
    async fn the_tool_wrapper_answers_and_reports_non_expressions_softly() {
        let tool = ComputeTool::new();
        let out = tool
            .call(
                json!({"expression": "2 GB to MB"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.text);

        let soft = tool
            .call(
                json!({"expression": "write me a poem about ferris"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(soft.is_error);
    }
}
