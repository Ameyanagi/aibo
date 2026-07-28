//! Price table, spend meter and budget enforcement (§14).
//!
//! BYOK means **the user pays for every mistake aibo makes**, which is why
//! this is a v1 module and not a later release.
//!
//! Four things live here:
//!
//! * **[`PriceTable`]** — shipped as TOML and user-updatable, because prices
//!   change faster than releases. A single input/output pair is not enough to
//!   price any current frontier model, so cached-input, reasoning, image and
//!   **provider-tier** rates are all first-class.
//! * **Estimate before dispatch, reconcile after.** [`SpendMeter::reserve`]
//!   holds an estimate, [`SpendMeter::reconcile`] replaces it with the real
//!   figure, [`SpendMeter::release`] drops it on failure. `Usage` never
//!   arrives on a cancelled or failed stream, so a meter that only counts
//!   completed responses systematically under-reports — and budget enforcement
//!   that waits for `Usage` cannot stop anything.
//! * **The monthly soft budget** — warn at 80%, optional hard stop, off by
//!   default.
//! * **[`AgentLimitTracker`]** — [`AgentLimits`] are *mandatory, not
//!   advisory*. A runaway loop on a metered provider is a support incident.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::context::{estimate_tokens, message_tokens};
use crate::error::{AiboError, Result};
use crate::types::{AgentLimits, BudgetKind, ChatRequest, ProviderId, Role, Usage};

// ---------------------------------------------------------------------------
// Money
// ---------------------------------------------------------------------------

/// Money, in millionths of the account currency (§12 `messages.cost_micros`).
///
/// Integer throughout: floating-point money in a spend meter accumulates a
/// visible drift over a month of use, and the user can see the number.
pub type Micros = u64;

/// One million — the denominator of both the per-Mtok rate and the micro.
const MILLION: u128 = 1_000_000;

/// The provider tier a price applies to.
///
/// §14 requires provider-tier rates: the same model costs different amounts on
/// a batch, flex, priority or provisioned tier, and "one price per model" is
/// wrong on every current frontier provider.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProviderTier(String);

impl ProviderTier {
    /// The synchronous, on-demand tier — what a request gets unless something
    /// says otherwise.
    pub const STANDARD: &'static str = "standard";

    /// A named tier.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The tier name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProviderTier {
    fn default() -> Self {
        Self(Self::STANDARD.to_string())
    }
}

impl std::fmt::Display for ProviderTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Per-million-token rates for one model on one provider tier (§14).
///
/// Every rate is **micros per million tokens**: `3_000_000` is $3.00 / Mtok.
/// The optional rates fall back to the ones a provider would bill them at when
/// it does not price them separately, so a minimal table entry is still
/// arithmetically correct rather than silently zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrices {
    /// Uncached prompt tokens.
    pub input: Micros,
    /// Prompt tokens served from the provider's cache. `None` → billed as
    /// `input`. Prompt caching changes this rate, which is why
    /// [`crate::types::Capabilities::prompt_cache`] exists (§14).
    pub cached_input: Option<Micros>,
    /// Completion tokens.
    pub output: Micros,
    /// Reasoning tokens where they are priced separately. `None` → billed as
    /// `output`, which is what most providers do.
    pub reasoning: Option<Micros>,
    /// Image input tokens. `None` → billed as `input`.
    pub image: Option<Micros>,
}

impl ModelPrices {
    /// The simplest possible entry.
    pub const fn simple(input: Micros, output: Micros) -> Self {
        Self {
            input,
            cached_input: None,
            output,
            reasoning: None,
            image: None,
        }
    }

    /// Cost of a [`Usage`], in micros.
    ///
    /// Saturating and integer-only. A `u128` intermediate keeps a 200k-token
    /// request at a $100/Mtok rate from overflowing.
    pub fn cost(&self, usage: &Usage) -> Micros {
        let mul = |tokens: u64, rate: Micros| -> u128 {
            (tokens as u128).saturating_mul(rate as u128) / MILLION
        };
        let total = mul(usage.input_tokens, self.input)
            + mul(
                usage.cached_input_tokens,
                self.cached_input.unwrap_or(self.input),
            )
            + mul(usage.output_tokens, self.output)
            + mul(
                usage.reasoning_tokens,
                self.reasoning.unwrap_or(self.output),
            )
            + mul(usage.image_tokens, self.image.unwrap_or(self.input));
        u64::try_from(total).unwrap_or(u64::MAX)
    }
}

/// One row of the shipped price table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceRow {
    /// Provider the row applies to.
    pub provider: ProviderId,
    /// Wire model id.
    pub model: String,
    /// Provider tier. Defaults to [`ProviderTier::STANDARD`].
    #[serde(default)]
    pub tier: ProviderTier,
    /// The rates.
    #[serde(flatten)]
    pub prices: ModelPrices,
}

/// The price table (§14).
///
/// Shipped as TOML and **user-updatable**, since prices change faster than
/// releases. The shape is:
///
/// ```toml
/// version = "1"
/// updated = "2026-07-26"
///
/// [[model]]
/// provider = "openai"
/// model    = "gpt-5-mini"
/// tier     = "standard"
/// input    = 250000          # micros per million tokens = $0.25/Mtok
/// output   = 2000000
/// cached_input = 25000
/// ```
///
/// A model with no row is **unpriced**, not free: [`PriceTable::lookup`]
/// returns `None` and callers must show "cost unknown" rather than zero.
/// Reporting $0.00 for a model whose price aibo does not know is worse than
/// reporting nothing.
///
/// # The shipped table is a default, and it will drift
///
/// [`PriceTable::shipped`] parses `crates/aibo-core/prices.toml`, which is
/// compiled in with `include_str!`. It exists so the spend meter, the 80 %
/// warning and the hard stop can function from the first launch — not because
/// its numbers are authoritative. They are published list prices in USD with
/// no enterprise discount, written down on [`PriceTable::updated`], and §14
/// ships the table as TOML *precisely because* prices change faster than
/// releases. [`PriceTable::load`] overlays a user file on top, row for row,
/// and that is the intended way to correct a rate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceTable {
    /// Table schema version.
    #[serde(default)]
    pub version: String,
    /// When the rates were last written down, for the "prices may be stale"
    /// affordance in settings.
    #[serde(default)]
    pub updated: String,
    /// Free-text provenance, rendered next to `updated`. The shipped file says
    /// so in as many words.
    #[serde(default)]
    pub source: String,
    /// The rows, as parsed.
    #[serde(default, rename = "model")]
    pub rows: Vec<PriceRow>,
    /// Index built from `rows`. Skipped by serde; rebuilt by
    /// [`PriceTable::reindex`].
    #[serde(skip)]
    index: BTreeMap<(ProviderId, String, ProviderTier), ModelPrices>,
    /// Whether a user file has been overlaid. Skipped by serde.
    #[serde(skip)]
    overridden: bool,
}

impl PriceTable {
    /// The shipped default table, as TOML.
    ///
    /// Compiled in rather than read from disk: a price table that can go
    /// missing at run time turns every cost into "unknown", and the file is a
    /// few kilobytes of text.
    pub const SHIPPED_TOML: &'static str = include_str!("../prices.toml");

    /// An empty table. Every lookup misses, so every cost is "unknown".
    ///
    /// For tests and for the deliberate "price nothing" posture. Startup wants
    /// [`PriceTable::load`].
    pub fn empty() -> Self {
        Self::default()
    }

    /// The shipped default table (§14).
    ///
    /// Fallible in signature only — the file is compiled in and a test in this
    /// module proves it parses, so a failure here means the file was edited
    /// into invalidity and the build was not run.
    pub fn shipped() -> Result<Self> {
        Self::from_toml_str(Self::SHIPPED_TOML)
    }

    /// The startup path: the shipped table, with the user's file overlaid.
    ///
    /// §14 makes the table user-updatable because prices change faster than
    /// releases. `user_toml` is whatever was read from the config directory;
    /// `None` when there is no user file. A malformed user file is an **error**
    /// rather than a silent fallback to the shipped rates — mis-pricing after
    /// the user tried to fix a rate is exactly the failure §14's meter exists
    /// to prevent.
    pub fn load(user_toml: Option<&str>) -> Result<Self> {
        let mut table = Self::shipped()?;
        if let Some(src) = user_toml {
            table.overlay(Self::from_toml_str(src)?);
        }
        Ok(table)
    }

    /// Overlay another table on this one.
    ///
    /// A row matching on `(provider, model, tier)` **replaces** the existing
    /// one; anything else is appended. Shipped rows the user did not mention
    /// survive, so correcting one rate does not un-price everything else.
    pub fn overlay(&mut self, other: PriceTable) {
        let PriceTable {
            version,
            updated,
            source,
            rows,
            ..
        } = other;
        for row in rows {
            match self
                .rows
                .iter_mut()
                .find(|r| r.provider == row.provider && r.model == row.model && r.tier == row.tier)
            {
                Some(existing) => *existing = row,
                None => self.rows.push(row),
            }
        }
        if !version.is_empty() {
            self.version = version;
        }
        if !updated.is_empty() {
            self.updated = updated;
        }
        if !source.is_empty() {
            self.source = source;
        }
        self.overridden = true;
        self.reindex();
    }

    /// Whether the user has overlaid anything, for the settings readout.
    pub fn is_shipped_default(&self) -> bool {
        !self.overridden
    }

    /// Parse a table from TOML.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let mut t: PriceTable = toml::from_str(s)
            .map_err(|e| AiboError::Internal(Box::new(std::io::Error::other(e.to_string()))))?;
        t.reindex();
        Ok(t)
    }

    /// Build a table from rows directly.
    pub fn from_rows(rows: Vec<PriceRow>) -> Self {
        let mut t = Self {
            rows,
            ..Default::default()
        };
        t.reindex();
        t
    }

    /// Rebuild the lookup index. Call after mutating [`PriceTable::rows`].
    pub fn reindex(&mut self) {
        self.index = self
            .rows
            .iter()
            .map(|r| {
                (
                    (r.provider.clone(), r.model.clone(), r.tier.clone()),
                    r.prices,
                )
            })
            .collect();
    }

    /// Rates for a model on a tier, falling back to the standard tier.
    ///
    /// `None` means unpriced. Do not substitute zero.
    pub fn lookup(
        &self,
        provider: &ProviderId,
        model: &str,
        tier: Option<&ProviderTier>,
    ) -> Option<&ModelPrices> {
        let tier = tier.cloned().unwrap_or_default();
        let key = (provider.clone(), model.to_string(), tier);
        if let Some(p) = self.index.get(&key) {
            return Some(p);
        }
        let standard = (key.0, key.1, ProviderTier::default());
        self.index.get(&standard)
    }

    /// Cost of a usage, or `None` when the model is unpriced.
    pub fn cost(
        &self,
        provider: &ProviderId,
        model: &str,
        tier: Option<&ProviderTier>,
        usage: &Usage,
    ) -> Option<Micros> {
        self.lookup(provider, model, tier).map(|p| p.cost(usage))
    }
}

// ---------------------------------------------------------------------------
// Estimate before dispatch
// ---------------------------------------------------------------------------

/// The usage a request is *expected* to produce, for the pre-dispatch reserve
/// (§14).
///
/// Input is the §4 character-class estimate of the assembled messages; output
/// is the explicit generation cap, or the context-planning reserve when the
/// cap is unset. Either way, erring high is
/// deliberate: an over-reserve is released on completion, an under-reserve
/// lets a runaway request past a hard stop.
pub fn estimate_request_usage(req: &ChatRequest) -> Usage {
    let input: usize = req.messages.iter().map(message_tokens).sum();
    // `0` means "let the provider/model choose". Cost reservation still needs
    // a finite estimate, so use prompt assembly's context-planning reserve
    // without turning that estimate into a generation cap.
    let estimated_output = if req.params.max_tokens == 0 {
        req.budget.max_output_tokens
    } else {
        req.params.max_tokens.min(req.budget.max_output_tokens)
    };
    Usage {
        input_tokens: input as u64,
        cached_input_tokens: 0,
        output_tokens: u64::from(estimated_output)
            .saturating_mul(u64::from(req.params.candidates.max(1))),
        reasoning_tokens: 0,
        image_tokens: 0,
    }
}

/// The estimated cost of a request before dispatch, or `None` if unpriced.
pub fn estimate_request_cost(
    table: &PriceTable,
    req: &ChatRequest,
    tier: Option<&ProviderTier>,
) -> Option<Micros> {
    let usage = estimate_request_usage(req);
    table.cost(&req.binding.provider, &req.binding.model, tier, &usage)
}

/// Estimated tokens of an arbitrary prompt string. Thin re-export so callers
/// outside `aibo-core` do not have to reach into [`crate::context`] for the
/// one function they need.
pub fn estimate_prompt_tokens(s: &str) -> usize {
    estimate_tokens(s)
}

// ---------------------------------------------------------------------------
// The spend meter
// ---------------------------------------------------------------------------

/// The monthly soft budget (§14). Off by default; visible in onboarding so it
/// is a known feature rather than a surprise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonthlyBudget {
    /// Ceiling for the calendar month.
    pub limit_micros: Micros,
    /// Warn at this percentage of the limit. §14: 80.
    pub warn_at_percent: u8,
    /// Refuse new requests once the limit is reached. §14: optional, off by
    /// default — a hard stop mid-sentence is its own kind of support incident.
    pub hard_stop: bool,
}

impl Default for MonthlyBudget {
    fn default() -> Self {
        Self {
            limit_micros: 0,
            warn_at_percent: 80,
            hard_stop: false,
        }
    }
}

/// Where the month's spend sits relative to the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStatus {
    /// No budget configured, or comfortably under it.
    Ok,
    /// At or past [`MonthlyBudget::warn_at_percent`]. Show the warning.
    Warning,
    /// At or past the limit. New requests are refused iff
    /// [`MonthlyBudget::hard_stop`].
    Exceeded,
}

/// A held cost estimate. Dropping one without reconciling or releasing it
/// leaks reserved spend, so the type is deliberately noisy in `Debug`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    /// The request this reserves for — [`ChatRequest::id`].
    pub request: Uuid,
    /// What was held.
    pub micros: Micros,
}

/// Aggregate spend, reserved and settled (§14).
///
/// Not a store: persistence is `aibo-store`'s job (§12 `messages.cost_micros`).
/// This is the in-memory view that enforcement reads, and it is deliberately
/// synchronous and lock-free so it can be consulted on the dispatch path.
#[derive(Debug, Clone, Default)]
pub struct SpendMeter {
    outstanding: BTreeMap<Uuid, Micros>,
    reserved_micros: Micros,
    settled_micros: Micros,
    unpriced_requests: u64,
    budget: Option<MonthlyBudget>,
}

impl SpendMeter {
    /// A meter with no budget.
    pub fn new() -> Self {
        Self::default()
    }

    /// A meter seeded with the month's already-settled spend, as loaded from
    /// the store.
    pub fn with_settled(settled_micros: Micros) -> Self {
        Self {
            settled_micros,
            ..Self::default()
        }
    }

    /// Attach or replace the monthly budget.
    pub fn set_budget(&mut self, budget: Option<MonthlyBudget>) {
        self.budget = budget;
    }

    /// The configured budget.
    pub fn budget(&self) -> Option<MonthlyBudget> {
        self.budget
    }

    /// Settled spend — real `Usage` that has landed.
    pub fn settled_micros(&self) -> Micros {
        self.settled_micros
    }

    /// Spend held against in-flight requests.
    pub fn reserved_micros(&self) -> Micros {
        self.reserved_micros
    }

    /// What the user is on the hook for right now: settled plus outstanding
    /// reserves. **This**, not `settled`, is what enforcement compares against
    /// the budget — otherwise ten parallel runaway requests all pass the check
    /// before any of them reports usage.
    pub fn committed_micros(&self) -> Micros {
        self.settled_micros.saturating_add(self.reserved_micros)
    }

    /// Requests whose model was not in the price table. Surfaced in settings:
    /// "cost unknown for N requests" is honest, "$0.00" is not.
    pub fn unpriced_requests(&self) -> u64 {
        self.unpriced_requests
    }

    /// Reserve an estimate at dispatch (§14).
    ///
    /// `estimate` is `None` when the model is unpriced — the request is still
    /// allowed (refusing to run because aibo does not know a price would be
    /// absurd) but it is counted so the UI can say so.
    ///
    /// Fails with [`AiboError::BudgetExceeded`] when a hard stop is configured
    /// and the reserve would cross the limit.
    pub fn reserve(&mut self, request: Uuid, estimate: Option<Micros>) -> Result<Reservation> {
        let micros = match estimate {
            Some(m) => m,
            None => {
                self.unpriced_requests += 1;
                0
            }
        };
        if let Some(b) = self.budget
            && b.hard_stop
            && b.limit_micros > 0
            && self.committed_micros().saturating_add(micros) > b.limit_micros
        {
            return Err(AiboError::BudgetExceeded {
                kind: BudgetKind::Cost,
            });
        }
        // A repeated reserve for the same id replaces the old one rather than
        // stacking — a retry of the same request must not double-count.
        if let Some(prev) = self.outstanding.insert(request, micros) {
            self.reserved_micros = self.reserved_micros.saturating_sub(prev);
        }
        self.reserved_micros = self.reserved_micros.saturating_add(micros);
        Ok(Reservation { request, micros })
    }

    /// Replace a reservation with the real cost once `Usage` lands (§14).
    ///
    /// Returns the settled cost. Reconciling an unknown id is not an error: a
    /// resumed conversation can produce usage for a request this process never
    /// reserved for, and dropping that on the floor would under-report.
    pub fn reconcile(&mut self, request: Uuid, actual: Option<Micros>) -> Micros {
        if let Some(held) = self.outstanding.remove(&request) {
            self.reserved_micros = self.reserved_micros.saturating_sub(held);
        }
        let cost = actual.unwrap_or(0);
        if actual.is_none() {
            self.unpriced_requests += 1;
        }
        self.settled_micros = self.settled_micros.saturating_add(cost);
        cost
    }

    /// Release a reservation without settling it — a cancelled or failed
    /// stream (§14: "release on failure").
    pub fn release(&mut self, request: Uuid) {
        if let Some(held) = self.outstanding.remove(&request) {
            self.reserved_micros = self.reserved_micros.saturating_sub(held);
        }
    }

    /// Where the month's spend sits (§14: warn at 80%, optional hard stop).
    pub fn status(&self) -> BudgetStatus {
        let Some(b) = self.budget else {
            return BudgetStatus::Ok;
        };
        if b.limit_micros == 0 {
            return BudgetStatus::Ok;
        }
        let committed = self.committed_micros() as u128;
        let limit = b.limit_micros as u128;
        if committed >= limit {
            BudgetStatus::Exceeded
        } else if committed * 100 >= limit * u128::from(b.warn_at_percent) {
            BudgetStatus::Warning
        } else {
            BudgetStatus::Ok
        }
    }
}

// ---------------------------------------------------------------------------
// Per-role caps
// ---------------------------------------------------------------------------

/// Per-role ceilings, enforced in `aibo-core` **before the request is built**
/// (§14, first bullet).
///
/// Deliberately separate from [`crate::context::ContextBudget`], which is
/// derived from the *model*: this is the user's policy for a role, and it
/// clamps the model-derived budget rather than the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCaps {
    /// Hard ceiling on output tokens for this role.
    pub max_output_tokens: u32,
    /// Hard ceiling on input tokens for this role.
    pub max_context_tokens: usize,
}

/// The shipped defaults.
///
/// These are policy, not measurement — §5's own thresholds are "an
/// unfalsifiable guess" until S9's eval harness exists, and so are these.
/// They exist so the caps are enforced from day one rather than added after
/// the first surprise bill.
pub const fn default_role_caps(role: Role) -> RoleCaps {
    match role {
        // Complete's binding. Short prompt, 64-token answer (§5).
        Role::Fast => RoleCaps {
            max_output_tokens: 512,
            max_context_tokens: 16_384,
        },
        Role::Smart => RoleCaps {
            max_output_tokens: 8_192,
            max_context_tokens: 128_000,
        },
        Role::Cheap => RoleCaps {
            max_output_tokens: 2_048,
            max_context_tokens: 32_768,
        },
        Role::Vision => RoleCaps {
            max_output_tokens: 4_096,
            max_context_tokens: 64_000,
        },
        // The agent loop's per-request cap; the run-level ceiling is
        // `AgentLimits`.
        Role::Agent => RoleCaps {
            max_output_tokens: 8_192,
            max_context_tokens: 200_000,
        },
    }
}

impl RoleCaps {
    /// Clamp a model-derived budget to this role's policy.
    pub fn clamp(&self, budget: crate::context::ContextBudget) -> crate::context::ContextBudget {
        crate::context::ContextBudget {
            max_context_tokens: budget.max_context_tokens.min(self.max_context_tokens),
            max_payload_tokens: budget.max_payload_tokens.min(self.max_context_tokens / 2),
            max_clipboard_tokens: budget.max_clipboard_tokens,
            max_output_tokens: budget.max_output_tokens.min(self.max_output_tokens),
        }
    }
}

// ---------------------------------------------------------------------------
// Agent limits
// ---------------------------------------------------------------------------

/// Enforces [`AgentLimits`] over the life of one agent run (§14).
///
/// §14: "**Agent limits are mandatory**, not advisory — a runaway loop on a
/// metered provider is a support incident. Exceeding one stops the run with
/// `BudgetExceeded` and a 'continue anyway' button. Codex's own limits apply
/// too, but aibo must not depend on them."
///
/// The "continue anyway" affordance is the *caller's*: on
/// [`AiboError::BudgetExceeded`] the UI offers to raise the ceiling and the
/// caller calls [`AgentLimitTracker::extend`]. The tracker itself never
/// forgives a limit.
#[derive(Debug, Clone)]
pub struct AgentLimitTracker {
    limits: AgentLimits,
    started: Instant,
    steps: u32,
    tool_calls: u32,
    tokens: u64,
    cost_micros: Micros,
}

impl AgentLimitTracker {
    /// Start tracking now.
    pub fn new(limits: AgentLimits) -> Self {
        Self::started_at(limits, Instant::now())
    }

    /// Start tracking from a given instant — used by tests, and by a resumed
    /// run that must keep its original wall clock.
    pub fn started_at(limits: AgentLimits, started: Instant) -> Self {
        Self {
            limits,
            started,
            steps: 0,
            tool_calls: 0,
            tokens: 0,
            cost_micros: 0,
        }
    }

    /// The limits in force.
    pub fn limits(&self) -> AgentLimits {
        self.limits
    }

    /// Steps taken.
    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// Tool calls made.
    pub fn tool_calls(&self) -> u32 {
        self.tool_calls
    }

    /// Tokens consumed across the run.
    pub fn tokens(&self) -> u64 {
        self.tokens
    }

    /// Spend attributed to this run.
    pub fn cost_micros(&self) -> Micros {
        self.cost_micros
    }

    /// Wall clock elapsed.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Raise the ceilings after the user pressed "continue anyway".
    ///
    /// The wall clock restarts, because the user has just re-consented to the
    /// run continuing from now.
    pub fn extend(&mut self, limits: AgentLimits) {
        self.limits = limits;
        self.started = Instant::now();
    }

    /// Record a step and check every ceiling.
    pub fn on_step(&mut self) -> Result<()> {
        self.steps = self.steps.saturating_add(1);
        if self.steps > self.limits.max_steps {
            return Err(exceeded(BudgetKind::Steps, "max_steps", self.steps as u64));
        }
        self.check(Instant::now())
    }

    /// Record a tool call and check every ceiling.
    pub fn on_tool_call(&mut self) -> Result<()> {
        self.tool_calls = self.tool_calls.saturating_add(1);
        if self.tool_calls > self.limits.max_tool_calls {
            return Err(exceeded(
                BudgetKind::Steps,
                "max_tool_calls",
                self.tool_calls as u64,
            ));
        }
        self.check(Instant::now())
    }

    /// Record usage from a `StreamEvent::Usage` and check every ceiling.
    pub fn on_usage(&mut self, usage: &Usage, cost: Option<Micros>) -> Result<()> {
        self.tokens = self.tokens.saturating_add(usage.total());
        self.cost_micros = self.cost_micros.saturating_add(cost.unwrap_or(0));
        self.check(Instant::now())
    }

    /// Check every ceiling without recording anything. Call this from the
    /// run loop's tick so a long-running tool cannot outlive the wall clock
    /// simply by never producing a step.
    pub fn check_now(&self) -> Result<()> {
        self.check(Instant::now())
    }

    fn check(&self, now: Instant) -> Result<()> {
        if self.tokens > self.limits.max_total_tokens {
            return Err(exceeded(
                BudgetKind::Tokens,
                "max_total_tokens",
                self.tokens,
            ));
        }
        if now.saturating_duration_since(self.started) > self.limits.max_wall_clock {
            return Err(exceeded(
                BudgetKind::Steps,
                "max_wall_clock",
                now.saturating_duration_since(self.started).as_secs(),
            ));
        }
        Ok(())
    }
}

fn exceeded(kind: BudgetKind, which: &'static str, value: u64) -> AiboError {
    tracing::warn!(limit = which, value, ?kind, "agent limit exceeded (§14)");
    AiboError::BudgetExceeded { kind }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Capabilities, GenerationParams, Message, MessageRole, ModelBinding, RequestBudget, Role,
        Surface,
    };

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn table() -> PriceTable {
        PriceTable::from_rows(vec![
            PriceRow {
                provider: ProviderId::OPENAI,
                model: "gpt-test".into(),
                tier: ProviderTier::default(),
                prices: ModelPrices {
                    input: 1_000_000,            // $1.00 / Mtok
                    cached_input: Some(100_000), // $0.10 / Mtok
                    output: 4_000_000,           // $4.00 / Mtok
                    reasoning: Some(8_000_000),
                    image: Some(2_000_000),
                },
            },
            PriceRow {
                provider: ProviderId::OPENAI,
                model: "gpt-test".into(),
                tier: ProviderTier::new("batch"),
                prices: ModelPrices::simple(500_000, 2_000_000),
            },
        ])
    }

    // -- price table --------------------------------------------------------

    #[test]
    fn a_single_input_output_pair_is_not_enough() {
        // §14: cached-input, reasoning and image rates are all distinct.
        let p = table();
        let prices = p
            .lookup(&ProviderId::OPENAI, "gpt-test", None)
            .expect("priced");
        let usage = Usage {
            input_tokens: 1_000_000,
            cached_input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            reasoning_tokens: 1_000_000,
            image_tokens: 1_000_000,
        };
        // 1.00 + 0.10 + 4.00 + 8.00 + 2.00 = $15.10
        assert_eq!(prices.cost(&usage), 15_100_000);
    }

    #[test]
    fn optional_rates_fall_back_rather_than_to_zero() {
        let p = ModelPrices::simple(1_000_000, 4_000_000);
        let usage = Usage {
            cached_input_tokens: 1_000_000,
            reasoning_tokens: 1_000_000,
            image_tokens: 1_000_000,
            ..Default::default()
        };
        // cached -> input, reasoning -> output, image -> input.
        assert_eq!(p.cost(&usage), 1_000_000 + 4_000_000 + 1_000_000);
    }

    #[test]
    fn provider_tiers_are_priced_separately() {
        let p = table();
        let usage = Usage {
            input_tokens: 1_000_000,
            ..Default::default()
        };
        let std = p
            .cost(&ProviderId::OPENAI, "gpt-test", None, &usage)
            .unwrap();
        let batch = p
            .cost(
                &ProviderId::OPENAI,
                "gpt-test",
                Some(&ProviderTier::new("batch")),
                &usage,
            )
            .unwrap();
        assert_eq!(std, 1_000_000);
        assert_eq!(batch, 500_000);
    }

    #[test]
    fn an_unknown_tier_falls_back_to_standard() {
        let p = table();
        let usage = Usage {
            input_tokens: 1_000_000,
            ..Default::default()
        };
        let cost = p
            .cost(
                &ProviderId::OPENAI,
                "gpt-test",
                Some(&ProviderTier::new("flex")),
                &usage,
            )
            .unwrap();
        assert_eq!(cost, 1_000_000);
    }

    #[test]
    fn an_unpriced_model_is_unknown_not_free() {
        let p = table();
        assert!(
            p.cost(
                &ProviderId::ANTHROPIC,
                "some-new-model",
                None,
                &Usage::default()
            )
            .is_none(),
            "an unpriced model must be None, never Some(0)"
        );
    }

    #[test]
    fn the_table_round_trips_through_toml() {
        let src = r#"
version = "1"
updated = "2026-07-26"

[[model]]
provider = "cerebras"
model = "llama-3.3-70b"
input = 600000
output = 1200000

[[model]]
provider = "cerebras"
model = "llama-3.3-70b"
tier = "batch"
input = 300000
output = 600000
"#;
        let t = PriceTable::from_toml_str(src).unwrap();
        assert_eq!(t.version, "1");
        assert_eq!(t.rows.len(), 2);
        let usage = Usage {
            input_tokens: 2_000_000,
            ..Default::default()
        };
        assert_eq!(
            t.cost(&ProviderId::CEREBRAS, "llama-3.3-70b", None, &usage),
            Some(1_200_000)
        );
    }

    // -- the shipped table (§14) --------------------------------------------

    #[test]
    fn the_shipped_table_parses() {
        // `shipped()` is fallible in signature only; this is what makes that
        // true. If this fails, `prices.toml` was edited into invalidity.
        let t = PriceTable::shipped().expect("crates/aibo-core/prices.toml must parse");
        assert!(!t.rows.is_empty());
        assert_eq!(t.version, "1");
    }

    #[test]
    fn the_shipped_table_says_it_will_drift() {
        // §14 ships this as TOML because prices change faster than releases.
        // The staleness affordance in settings needs both fields populated, and
        // a silent undated table is how a wrong number becomes an authority.
        let t = PriceTable::shipped().unwrap();
        assert!(!t.updated.is_empty(), "no `updated` date to show as stale");
        assert!(t.source.contains("drift"), "source = {:?}", t.source);
        assert!(t.is_shipped_default());
    }

    #[test]
    fn every_default_role_binding_is_priced() {
        // The point of shipping the file at all: §14's spend meter, 80 %
        // warning and hard stop cannot function while the default chains are
        // unpriced. This is the test that would have caught the missing file.
        let t = PriceTable::shipped().unwrap();
        let bindings = crate::roles::RoleBindings::seed_unfiltered();
        for chain in bindings.chains() {
            for binding in &chain.entries {
                assert!(
                    t.lookup(&binding.provider, &binding.model, None).is_some(),
                    "{:?} chain entry {}/{} is unpriced",
                    chain.role,
                    binding.provider,
                    binding.model
                );
            }
        }
    }

    #[test]
    fn every_codex_allowlist_model_is_priced_and_no_rejected_one_is() {
        // §3a: the five ChatGPT-plan ids work; the API-style ids hard-400. A
        // price for a rejected id would be a price for a request that cannot
        // be made.
        let t = PriceTable::shipped().unwrap();
        for good in [
            "gpt-5.5",
            "gpt-5.6-terra",
            "gpt-5.3-codex-spark",
            "gpt-5.6-luna",
            "gpt-5.6-sol",
        ] {
            assert!(
                t.lookup(&ProviderId::CODEX, good, None).is_some(),
                "codex/{good} is on the allowlist but unpriced"
            );
        }
        for bad in [
            "gpt-5",
            "gpt-5-codex",
            "gpt-5.1-codex",
            "gpt-5.1-codex-mini",
            "codex-mini-latest",
        ] {
            assert!(
                t.lookup(&ProviderId::CODEX, bad, None).is_none(),
                "codex/{bad} hard-400s and must not be priced"
            );
        }
    }

    #[test]
    fn a_subscription_or_local_zero_is_a_row_not_a_missing_one() {
        // Zero has to be a *stated* rate. An omitted row would read as
        // "unknown" and inflate `unpriced_requests` on every offline and every
        // Codex request.
        let t = PriceTable::shipped().unwrap();
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            t.cost(&ProviderId::OLLAMA, "qwen3:8b", None, &usage),
            Some(0)
        );
        assert_eq!(
            t.cost(&ProviderId::CODEX, "gpt-5.6-sol", None, &usage),
            Some(0)
        );
    }

    #[test]
    fn the_shipped_table_prices_a_real_request() {
        let t = PriceTable::shipped().unwrap();
        let usage = Usage {
            input_tokens: 10_000,
            cached_input_tokens: 90_000,
            output_tokens: 1_000,
            ..Default::default()
        };
        // gpt-5: 10k @ $1.25 + 90k @ $0.125 + 1k @ $10.00 /Mtok
        //      = 12_500 + 11_250 + 10_000 micros
        assert_eq!(
            t.cost(&ProviderId::OPENAI, "gpt-5", None, &usage),
            Some(33_750)
        );
    }

    #[test]
    fn a_user_row_overrides_a_shipped_one_without_dropping_the_rest() {
        let user = r#"
updated = "2026-09-01"
source = "hand-corrected"

[[model]]
provider = "openai"
model = "gpt-5"
input = 999_000
output = 999_000
"#;
        let t = PriceTable::load(Some(user)).unwrap();
        let usage = Usage {
            input_tokens: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            t.cost(&ProviderId::OPENAI, "gpt-5", None, &usage),
            Some(999_000),
            "the user's rate must win"
        );
        assert!(
            t.lookup(&ProviderId::CEREBRAS, "llama-3.3-70b", None)
                .is_some(),
            "correcting one rate must not un-price everything else"
        );
        assert_eq!(t.updated, "2026-09-01");
        assert!(!t.is_shipped_default());
    }

    #[test]
    fn a_user_row_for_an_unknown_model_is_added() {
        let user = r#"
[[model]]
provider = "ollama"
model = "my-finetune:latest"
input = 0
output = 0
"#;
        let t = PriceTable::load(Some(user)).unwrap();
        assert!(
            t.lookup(&ProviderId::OLLAMA, "my-finetune:latest", None)
                .is_some()
        );
    }

    #[test]
    fn a_user_row_overrides_only_its_own_tier() {
        let user = r#"
[[model]]
provider = "openai"
model = "gpt-5"
tier = "batch"
input = 1
output = 1
"#;
        let t = PriceTable::load(Some(user)).unwrap();
        let usage = Usage {
            input_tokens: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            t.cost(
                &ProviderId::OPENAI,
                "gpt-5",
                Some(&ProviderTier::new("batch")),
                &usage
            ),
            Some(1)
        );
        assert_eq!(
            t.cost(&ProviderId::OPENAI, "gpt-5", None, &usage),
            Some(1_250_000),
            "the standard tier must be untouched"
        );
    }

    #[test]
    fn a_malformed_user_file_is_an_error_not_a_silent_fallback() {
        // Falling back to the shipped rates after the user tried to correct one
        // is precisely the mis-pricing §14's meter exists to prevent.
        assert!(PriceTable::load(Some("this is not toml [[[")).is_err());
    }

    #[test]
    fn load_without_a_user_file_is_the_shipped_table() {
        assert_eq!(
            PriceTable::load(None).unwrap().rows,
            PriceTable::shipped().unwrap().rows
        );
    }

    // -- estimate before dispatch -------------------------------------------

    fn request(max_tokens: u32, candidates: u8) -> ChatRequest {
        ChatRequest {
            id: id(1),
            conversation_id: None,
            surface: Surface::Complete,
            role: Role::Fast,
            binding: ModelBinding {
                provider: ProviderId::OPENAI,
                model: "gpt-test".into(),
            },
            messages: vec![
                Message::text(MessageRole::System, "you are a helpful assistant"),
                Message::text(MessageRole::User, "日本語のテキストを続けてください"),
            ],
            params: GenerationParams {
                max_tokens,
                candidates,
                ..Default::default()
            },
            budget: RequestBudget {
                max_context_tokens: 8_000,
                max_payload_tokens: 4_000,
                max_output_tokens: max_tokens,
                reserved_cost_micros: 0,
                deadline: Duration::from_secs(30),
            },
            tools: Vec::new(),
            user_instruction: None,
            untrusted: Vec::new(),
            attachments: Vec::new(),
            prompt_version: "complete/1".into(),
        }
    }

    #[test]
    fn the_estimate_uses_the_cjk_aware_heuristic() {
        let u = estimate_request_usage(&request(64, 1));
        // The Japanese message alone is 15 CJK chars = 15 tokens; bytes/4
        // would have said 11. The point is only that it is not bytes/4.
        assert!(u.input_tokens >= 20, "{u:?}");
        assert_eq!(u.output_tokens, 64);
    }

    #[test]
    fn the_estimate_accounts_for_every_candidate() {
        // §5 asks Complete for 3 candidates; the reserve must cover all three
        // or a hard stop can be walked straight past.
        let u = estimate_request_usage(&request(64, 3));
        assert_eq!(u.output_tokens, 192);
    }

    #[test]
    fn an_unset_model_cap_still_uses_the_planning_reserve_for_cost() {
        let request = request(0, 1);
        let u = estimate_request_usage(&request);
        assert_eq!(u.output_tokens, u64::from(request.budget.max_output_tokens));
    }

    // -- the spend meter ----------------------------------------------------

    #[test]
    fn reserve_then_reconcile_replaces_the_estimate() {
        let mut m = SpendMeter::new();
        m.reserve(id(1), Some(1_000)).unwrap();
        assert_eq!(m.reserved_micros(), 1_000);
        assert_eq!(m.settled_micros(), 0);
        assert_eq!(m.committed_micros(), 1_000);

        m.reconcile(id(1), Some(400));
        assert_eq!(m.reserved_micros(), 0);
        assert_eq!(m.settled_micros(), 400);
        assert_eq!(m.committed_micros(), 400);
    }

    #[test]
    fn a_failed_stream_releases_its_reserve() {
        // §14: "Usage never arrives on a cancelled or failed stream."
        let mut m = SpendMeter::new();
        m.reserve(id(1), Some(5_000)).unwrap();
        m.release(id(1));
        assert_eq!(m.reserved_micros(), 0);
        assert_eq!(m.settled_micros(), 0);
    }

    #[test]
    fn enforcement_counts_reserves_not_just_settled_spend() {
        // The whole point of reserving: ten parallel requests must not all
        // pass the hard stop before any of them reports usage.
        let mut m = SpendMeter::new();
        m.set_budget(Some(MonthlyBudget {
            limit_micros: 10_000,
            warn_at_percent: 80,
            hard_stop: true,
        }));
        for i in 0..10 {
            m.reserve(id(i), Some(1_000)).unwrap();
        }
        let err = m.reserve(id(99), Some(1_000)).unwrap_err();
        assert!(matches!(
            err,
            AiboError::BudgetExceeded {
                kind: BudgetKind::Cost
            }
        ));
    }

    #[test]
    fn a_soft_budget_warns_but_never_blocks() {
        let mut m = SpendMeter::with_settled(8_500);
        m.set_budget(Some(MonthlyBudget {
            limit_micros: 10_000,
            warn_at_percent: 80,
            hard_stop: false,
        }));
        assert_eq!(m.status(), BudgetStatus::Warning);
        // Off by default means it does not stop anything.
        m.reserve(id(1), Some(100_000)).unwrap();
        assert_eq!(m.status(), BudgetStatus::Exceeded);
    }

    #[test]
    fn no_budget_is_always_ok() {
        let m = SpendMeter::with_settled(u64::MAX / 2);
        assert_eq!(m.status(), BudgetStatus::Ok);
    }

    #[test]
    fn an_unpriced_request_is_counted_not_hidden() {
        let mut m = SpendMeter::new();
        m.reserve(id(1), None).unwrap();
        m.reconcile(id(1), None);
        assert_eq!(m.settled_micros(), 0);
        assert_eq!(m.unpriced_requests(), 2, "reserve and reconcile both count");
    }

    #[test]
    fn reserving_twice_for_one_request_does_not_double_count() {
        let mut m = SpendMeter::new();
        m.reserve(id(1), Some(1_000)).unwrap();
        m.reserve(id(1), Some(2_000)).unwrap();
        assert_eq!(m.reserved_micros(), 2_000);
    }

    // -- role caps ----------------------------------------------------------

    #[test]
    fn role_caps_clamp_a_generous_model_budget() {
        let caps = Capabilities {
            max_context: 1_000_000,
            ..Default::default()
        };
        let budget = crate::context::ContextBudget::from_capabilities(&caps, 100_000);
        let clamped = default_role_caps(Role::Fast).clamp(budget);
        assert_eq!(clamped.max_context_tokens, 16_384);
        assert_eq!(clamped.max_output_tokens, 512);
        assert!(clamped.max_payload_tokens <= 16_384 / 2);
    }

    // -- agent limits -------------------------------------------------------

    #[test]
    fn the_step_ceiling_stops_the_run() {
        let mut t = AgentLimitTracker::new(AgentLimits {
            max_steps: 3,
            ..Default::default()
        });
        for _ in 0..3 {
            t.on_step().unwrap();
        }
        let err = t.on_step().unwrap_err();
        assert!(matches!(
            err,
            AiboError::BudgetExceeded {
                kind: BudgetKind::Steps
            }
        ));
    }

    #[test]
    fn the_tool_call_ceiling_stops_the_run() {
        let mut t = AgentLimitTracker::new(AgentLimits {
            max_tool_calls: 1,
            ..Default::default()
        });
        t.on_tool_call().unwrap();
        assert!(t.on_tool_call().is_err());
    }

    #[test]
    fn the_token_ceiling_stops_the_run() {
        let mut t = AgentLimitTracker::new(AgentLimits {
            max_total_tokens: 100,
            ..Default::default()
        });
        let err = t
            .on_usage(
                &Usage {
                    input_tokens: 200,
                    ..Default::default()
                },
                None,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            AiboError::BudgetExceeded {
                kind: BudgetKind::Tokens
            }
        ));
    }

    #[test]
    fn the_wall_clock_ceiling_stops_the_run() {
        let started = Instant::now();
        let t = AgentLimitTracker::started_at(
            AgentLimits {
                max_wall_clock: Duration::from_secs(60),
                ..Default::default()
            },
            started,
        );
        let err = t.check(started + Duration::from_secs(61)).unwrap_err();
        assert!(matches!(
            err,
            AiboError::BudgetExceeded {
                kind: BudgetKind::Steps
            }
        ));
    }

    #[test]
    fn continue_anyway_raises_the_ceiling_and_restarts_the_clock() {
        let mut t = AgentLimitTracker::new(AgentLimits {
            max_steps: 1,
            ..Default::default()
        });
        t.on_step().unwrap();
        assert!(t.on_step().is_err());
        t.extend(AgentLimits {
            max_steps: 50,
            ..Default::default()
        });
        t.on_step().unwrap();
    }

    #[test]
    fn the_defaults_are_the_section_14_numbers() {
        let d = AgentLimits::default();
        assert_eq!(d.max_steps, 25);
        assert_eq!(d.max_tool_calls, 50);
        assert_eq!(d.max_wall_clock, Duration::from_secs(300));
        assert_eq!(d.max_total_tokens, 200_000);
    }
}
