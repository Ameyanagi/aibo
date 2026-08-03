//! The orchestrator.
//!
//! One function, [`Engine::run`], turns a captured context into streamed
//! tokens:
//!
//! ```text
//! capture (§8) → surface inference (§1) → route (§4)
//!   → assemble prompt + apply the context budget (§5)
//!   → resolve the role chain to a provider (§4, §13, §14)
//!   → reserve the estimated cost (§14)
//!   → stream (§7) → reconcile usage (§14) → persist (§12)
//! ```
//!
//! Each arrow is a plan requirement rather than a step someone liked, and the
//! ones with teeth are:
//!
//! * **§4 fallback across a chain.** Connect failure, 5xx, a 429 whose
//!   `retry_after` exceeds the surface's budget, and a failed health probe move
//!   to the next entry. A **400 does not** — that is a bug in aibo, and it
//!   surfaces as one.
//! * **§13 per-provider offline state**, in [`crate::health`], never a global
//!   boolean.
//! * **§13 cancellation**, threaded end to end; a new submission cancels the
//!   previous one.
//! * **§13's partial-stream invariant**, expressed as
//!   [`Outcome::insertable_text`] returning `None`.
//! * **§14 reserve-then-reconcile**, because `Usage` never arrives on a
//!   cancelled or failed stream.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use aibo_core::AiboError;
use aibo_core::context::{Chars, Tokens};
use aibo_core::cost::{
    BudgetStatus, Micros, MonthlyBudget, PriceTable, ProviderTier, RoleCaps, SpendMeter,
    default_role_caps, estimate_request_usage,
};
use aibo_core::prompts::{self, PromptInputs, attachable_clipboard_text};
use aibo_core::roles::{RoleBindings, vision_providers};
use aibo_core::router::{
    DoVerbRegistry, Router, SurfaceInput, infer_surface, should_offer_escalation,
};
use aibo_core::types::{
    Capabilities, ChatRequest, ModelBinding, ProviderId, Role, RouteInput, StopReason, Surface,
    Usage, validate_attachments,
};
use aibo_provider::ProviderRegistry;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::dispatch;
use crate::event::{
    Completion, EventSink, Outcome, PartialReason, SessionEvent, SkipReason, Submission,
};
use crate::health::{FailureKind, HealthTable, HysteresisPolicy, Usability};
use crate::store::{Exchange, NoStore, SessionStore};
use crate::trust::TrustMap;

/// §13: *"refuse above 200k characters with a clear message"*, enforced before
/// any request is built.
///
/// A raw `usize` because it is a serde-loaded configuration knob
/// ([`crate::config`]); every *comparison* against it wraps it in
/// [`Chars`] first, and [`crate::event::Submission::total_chars`] is typed, so
/// the two sides of the cap cannot be in different units.
pub const DEFAULT_MAX_PAYLOAD_CHARS: usize = 200_000;

/// Default wall-clock ceiling for one request, carried on
/// [`aibo_core::types::RequestBudget::deadline`] and **enforced by the engine**
/// — see [`EngineConfig::request_deadline`].
pub const DEFAULT_REQUEST_DEADLINE: Duration = Duration::from_secs(60);

/// The smallest ceiling [`EngineConfig::request_deadline`] may be set to.
///
/// A configured `0` would refuse every request before it started, which is not
/// a setting anyone means. Clamped rather than rejected, in the same shape as
/// the §13 hysteresis knobs in [`crate::config`].
pub const MIN_REQUEST_DEADLINE: Duration = Duration::from_secs(1);

/// Everything the engine needs that does not change per request.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// §4's role → chain map. `fallback_enabled` is per role and off by
    /// default (§14).
    pub bindings: RoleBindings,
    /// §14's price table, for the pre-dispatch estimate and the reconcile.
    pub prices: PriceTable,
    /// The monthly soft budget, off by default (§14).
    pub monthly_budget: Option<MonthlyBudget>,
    /// §13's offline hysteresis knobs.
    pub hysteresis: HysteresisPolicy,
    /// §14's privacy classification for fallback.
    pub trust: TrustMap,
    /// Per-provider pricing tier, when the user is on one.
    pub tiers: BTreeMap<ProviderId, ProviderTier>,
    /// Per-model capabilities (§10: capabilities are per model, not per
    /// provider). Falls back to `Provider::capabilities` when absent.
    pub catalogue: BTreeMap<(ProviderId, String), Capabilities>,
    /// §1's Do trigger words.
    pub do_verbs: DoVerbRegistry,
    /// §13's large-selection refusal, in characters (Unicode scalar values,
    /// never bytes — see [`crate::event::char_len`]).
    pub max_payload_chars: usize,
    /// Wall-clock ceiling for one request, measured from [`Engine::run`] entry
    /// and covering assembly, the initial POST and the stream.
    ///
    /// Enforced by [`Engine::run`]: when it expires the in-flight request is
    /// cancelled and the outcome is
    /// [`AiboError::Timeout`]`{ phase: Stream }` (§13), with any text that had
    /// already arrived kept as an [`Outcome::Partial`] rather than discarded.
    /// It is also projected onto
    /// [`aibo_core::types::RequestBudget::deadline`] so a provider that can
    /// enforce its own transport timeout has the number.
    pub request_deadline: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            bindings: RoleBindings::default(),
            prices: PriceTable::empty(),
            monthly_budget: None,
            hysteresis: HysteresisPolicy::default(),
            trust: TrustMap::new(),
            tiers: BTreeMap::new(),
            catalogue: BTreeMap::new(),
            do_verbs: DoVerbRegistry::builtin(),
            max_payload_chars: DEFAULT_MAX_PAYLOAD_CHARS,
            request_deadline: DEFAULT_REQUEST_DEADLINE,
        }
    }
}

/// The orchestration layer.
///
/// Shared behind an `Arc` and driven from the tokio half of §6's diagram. All
/// interior mutability is short-lived `Mutex` state — the spend meter, the
/// health table and the cancellation registry — never held across an `await`.
pub struct Engine {
    providers: ProviderRegistry,
    router: Router,
    config: EngineConfig,
    /// Runtime-refreshed per-model facts. `EngineConfig::catalogue` remains the
    /// construction snapshot exposed for diagnostics; dispatch reads this
    /// lock so a successful `/models` refresh affects the very next request.
    live_catalogue: RwLock<BTreeMap<(ProviderId, String), Capabilities>>,
    health: Arc<HealthTable>,
    spend: Mutex<SpendMeter>,
    store: Arc<dyn SessionStore>,
    /// §13: *"one panel, one session"*. At most one panel request is ever in
    /// flight; installing a new one cancels the old.
    inflight: Mutex<Option<InflightRequest>>,
    /// Agent runs, which §13 explicitly exempts: *"pressing it during an agent
    /// run does not interrupt — the run continues in the task window"*.
    tasks: Mutex<BTreeMap<Uuid, RegisteredTask>>,
}

struct InflightRequest {
    session: Uuid,
    run: Uuid,
    cancel: CancellationToken,
}

struct RegisteredTask {
    run: Uuid,
    cancel: CancellationToken,
}

/// Scope ownership for the panel slot. Dropping an aborted `Engine::run`
/// future retires only the generation it installed.
struct InflightGuard<'a> {
    engine: &'a Engine,
    session: Uuid,
    run: Uuid,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.engine.finish(self.session, self.run);
    }
}

/// Scope ownership for an agent registry entry.
pub(crate) struct TaskGuard<'a> {
    engine: &'a Engine,
    task: Uuid,
    run: Uuid,
    /// Token passed to the backend and polled by the engine.
    pub(crate) cancel: CancellationToken,
}

impl Drop for TaskGuard<'_> {
    fn drop(&mut self) {
        self.engine.retire_task(self.task, self.run);
    }
}

/// Releases a held estimate if an attempt future is aborted or returns along
/// an error path before explicit reconciliation.
struct ReservationGuard<'a> {
    engine: &'a Engine,
    request: Uuid,
    active: bool,
}

impl ReservationGuard<'_> {
    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            self.engine.release(self.request);
        }
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("providers", &self.providers)
            .field("health", &self.health.snapshot())
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Build an engine over a provider registry.
    pub fn new(providers: ProviderRegistry, config: EngineConfig) -> Self {
        let config = EngineConfig {
            // Now that the ceiling is enforced, a `0` from the config file
            // would refuse every request before it started. Clamped here rather
            // than in [`crate::config`] so every construction path is covered.
            request_deadline: config.request_deadline.max(MIN_REQUEST_DEADLINE),
            ..config
        };
        let mut spend = SpendMeter::new();
        spend.set_budget(config.monthly_budget);
        let live_catalogue = RwLock::new(config.catalogue.clone());
        Self {
            health: Arc::new(HealthTable::new(config.hysteresis)),
            providers,
            router: Router::with_defaults(),
            config,
            live_catalogue,
            spend: Mutex::new(spend),
            store: Arc::new(NoStore),
            inflight: Mutex::new(None),
            tasks: Mutex::new(BTreeMap::new()),
        }
    }

    /// Replace the §4 rule list — the hook for per-app defaults and pinned
    /// saved actions, which §4 requires the rules engine to absorb without a
    /// refactor.
    #[must_use]
    pub fn with_router(mut self, router: Router) -> Self {
        self.router = router;
        self
    }

    /// Attach §12 persistence. Without this history is simply not written.
    #[must_use]
    pub fn with_store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.store = store;
        self
    }

    /// Attach persistence and seed this engine with spend already settled in
    /// the current month. Use this construction path for a durable store so an
    /// app restart or provider-registry rebuild cannot reset the hard stop.
    #[must_use]
    pub async fn with_store_loaded(mut self, store: Arc<dyn SessionStore>) -> Self {
        match store.settled_spend_this_month().await {
            Ok(settled) => {
                let mut meter = self.spend.lock().unwrap_or_else(|e| e.into_inner());
                let budget = meter.budget();
                *meter = SpendMeter::with_settled(settled);
                meter.set_budget(budget);
            }
            Err(error) => {
                // History remains optional. If it cannot be read, requests
                // still work, but the failure is visible rather than silently
                // claiming the month began at zero.
                tracing::warn!(%error, "could not load this month's settled spend (§12, §14)");
            }
        }
        self.store = store;
        self
    }

    /// Seed the meter with the month's already-settled spend, as loaded from
    /// the store.
    #[must_use]
    pub fn with_settled_spend(self, settled: Micros) -> Self {
        {
            let mut meter = self.spend.lock().unwrap_or_else(|e| e.into_inner());
            let budget = meter.budget();
            *meter = SpendMeter::with_settled(settled);
            meter.set_budget(budget);
        }
        self
    }

    /// Replace the monthly budget while running (§14, settings).
    ///
    /// Enforcement reads the meter, not `EngineConfig`, so this is the whole
    /// live path — the next request sees the new ceiling.
    pub fn set_monthly_budget(&self, budget: Option<MonthlyBudget>) {
        let mut meter = self.spend.lock().unwrap_or_else(|e| e.into_inner());
        meter.set_budget(budget);
    }

    /// The budget currently enforced, for §14's meter fraction.
    pub fn monthly_budget(&self) -> Option<MonthlyBudget> {
        let meter = self.spend.lock().unwrap_or_else(|e| e.into_inner());
        meter.budget()
    }

    /// The §13 offline table, so the shell can re-probe on wake and render
    /// per-provider badges.
    pub fn health(&self) -> &Arc<HealthTable> {
        &self.health
    }

    /// The configured providers.
    pub fn providers(&self) -> &ProviderRegistry {
        &self.providers
    }

    /// §14's price table, for showing a cost band next to a model.
    pub fn prices(&self) -> &aibo_core::cost::PriceTable {
        &self.config.prices
    }

    /// The engine's configuration.
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Replace the per-model capability snapshot after a live catalogue
    /// refresh. Dispatch takes only a short read lock and never holds it over
    /// provider I/O.
    pub fn replace_model_catalogue(&self, catalogue: BTreeMap<(ProviderId, String), Capabilities>) {
        *self
            .live_catalogue
            .write()
            .unwrap_or_else(|error| error.into_inner()) = catalogue;
    }

    /// Settled spend, committed spend (settled + reserved) and where that sits
    /// against the monthly budget (§14).
    pub fn spend_snapshot(&self) -> (Micros, Micros, BudgetStatus) {
        let meter = self.spend.lock().unwrap_or_else(|e| e.into_inner());
        (
            meter.settled_micros(),
            meter.committed_micros(),
            meter.status(),
        )
    }

    // -- cancellation (§13) --------------------------------------------------

    /// Install a fresh token for `session`, cancelling whatever was in flight.
    ///
    /// §13: *"Pressing the hotkey while a Complete is streaming: the in-flight
    /// request is cancelled, the panel is re-captured for the new context, and
    /// the old session is discarded."* That is one slot, not a map — a map
    /// would let two panel requests stream at once, which the product does not
    /// have a place to show.
    fn begin(&self, session: Uuid) -> (Uuid, CancellationToken) {
        let token = CancellationToken::new();
        let run = Uuid::now_v7();
        let mut slot = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = slot.replace(InflightRequest {
            session,
            run,
            cancel: token.clone(),
        }) {
            if previous.session != session {
                tracing::debug!(previous = %previous.session, %session, "a new submission cancels the previous one");
            }
            previous.cancel.cancel();
        }
        (run, token)
    }

    /// Retire our slot, if it is still ours.
    fn finish(&self, session: Uuid, run: Uuid) {
        let mut slot = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .is_some_and(|current| current.session == session && current.run == run)
        {
            *slot = None;
        }
    }

    /// `esc`: cancel the in-flight request for `session`.
    ///
    /// A cancel for a session that has already been superseded is a no-op, not
    /// an error — the keystroke and the supersession race, and losing that race
    /// must not cancel the *new* request.
    pub fn cancel(&self, session: Uuid) {
        let slot = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(current) = slot.as_ref()
            && current.session == session
        {
            current.cancel.cancel();
        }
    }

    /// Cancel everything, panel and agent runs alike. Shutdown only.
    pub fn cancel_all(&self) {
        if let Some(current) = self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            current.cancel.cancel();
        }
        for (_, task) in std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner()))
        {
            task.cancel.cancel();
        }
    }

    pub(crate) fn register_task(&self, task: Uuid) -> TaskGuard<'_> {
        let token = CancellationToken::new();
        let run = Uuid::now_v7();
        if let Some(previous) = self.tasks.lock().unwrap_or_else(|e| e.into_inner()).insert(
            task,
            RegisteredTask {
                run,
                cancel: token.clone(),
            },
        ) {
            previous.cancel.cancel();
        }
        TaskGuard {
            engine: self,
            task,
            run,
            cancel: token,
        }
    }

    fn retire_task(&self, task: Uuid, run: Uuid) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if tasks.get(&task).is_some_and(|current| current.run == run) {
            tasks.remove(&task);
        }
    }

    /// Cancel one agent run.
    pub fn cancel_task(&self, task: Uuid) {
        if let Some(current) = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&task)
        {
            current.cancel.cancel();
        }
    }

    // -- the request path ----------------------------------------------------

    /// Run one submission to completion, cancellation or failure.
    ///
    /// Installs the session's cancellation token first, so a submission that
    /// arrives while another is streaming cancels it before doing any work of
    /// its own (§13).
    pub async fn run(&self, submission: Submission, events: &EventSink) -> Outcome {
        let session = submission.session;
        let (run, cancel) = self.begin(session);
        let _guard = InflightGuard {
            engine: self,
            session,
            run,
        };
        let outcome = self.run_inner(submission, &cancel, events).await;

        if let Outcome::Failed(error) = &outcome {
            events.emit(SessionEvent::Failed(error.clone()));
        }
        outcome
    }

    async fn run_inner(
        &self,
        submission: Submission,
        cancel: &CancellationToken,
        events: &EventSink,
    ) -> Outcome {
        // §13's wall-clock ceiling for the whole request, armed before any work
        // is done so it covers assembly and the initial POST, not just the
        // stream. Absolute and in tokio's clock so the tests can drive it with
        // `tokio::time::pause` instead of sleeping for a minute.
        let deadline = tokio::time::Instant::now() + self.config.request_deadline;

        // §13 large selections: "hard caps, enforced before any request is
        // built". The unit is characters — *"counted as characters, not
        // `str::len()` bytes"* — and [`Submission::total_chars`] is the single
        // place that counts them, so the limit and the reported `actual` can
        // never again be in different units. The point of refusing here is to
        // refuse before the 40 MB string is tokenised, estimated or copied.
        let chars = submission.total_chars();
        let cap = Chars::new(self.config.max_payload_chars);
        if chars > cap {
            return failed(AiboError::ContextTooLarge {
                // `AiboError::ContextTooLarge` carries bare `usize`s and is
                // raised both here (characters) and by the §5 budget (tokens).
                // Both sides of *this* pair are characters; typing the error
                // itself needs `error.rs`.
                limit: cap.get(),
                actual: chars.get(),
            });
        }

        // The same refusal, in the other unit. §13's cap is stated in
        // characters and an image has none — `Chars` deliberately will not add
        // to bytes, so attachments get their own ceiling rather than being
        // laundered into the character count. `validate_attachments` owns it:
        // per-item size, media type, count, and the summed-bytes cap that is
        // what actually binds a multi-image request.
        //
        // Enforced here, before a request is built, for the reason §4 gives:
        // aibo does not fall back on a 400, so discovering a provider's payload
        // ceiling as a rejected request costs a round trip and then dead-ends.
        if let Err(error) = validate_attachments(&submission.attachments) {
            return failed(error);
        }

        // §1: the surface, frozen. The panel usually supplies it; inferring it
        // here is the path a headless caller and the eval harness take.
        let surface = submission.surface.unwrap_or_else(|| {
            infer_surface(
                &SurfaceInput {
                    panel_input: &submission.instruction,
                    selection: submission.capture.selection.as_deref().unwrap_or(""),
                    field_prefix: submission
                        .capture
                        .field
                        .as_ref()
                        .map_or("", |f| f.prefix.as_str()),
                    capture_timed_out: false,
                },
                &self.config.do_verbs,
            )
        });

        // §4: routing is a pure function over the capture. It runs before any
        // provider is touched so the decision is in the log even for a request
        // that never dispatches.
        let routed = self.router.route(&route_input(&submission, surface));
        events.emit(SessionEvent::Routed {
            surface,
            role: routed.role,
            rule: routed.rule,
        });
        tracing::info!(role = ?routed.role, rule = routed.rule, ?surface, "routed");

        let chain = self.config.bindings.dispatch_order(routed.role);
        if chain.is_empty() {
            // The 2026-07-26 report, in one line: an empty chain is `Vision`'s
            // *correct* state when no vision provider is configured (every
            // entry in §4's chain is `Precondition::Configured`), and reporting
            // it as `NoProviderConfigured` claims nothing works while the
            // user's text setup is signed in and healthy — a contradiction the
            // user cannot act on, and §13's only Blocking treatment to boot.
            //
            // With an attachment in hand the honest error names the modality
            // and the fix: "attach needs a vision-capable provider; configure
            // OpenAI, Anthropic or Vertex", Inline, session intact.
            return failed(self.no_provider_for(&submission));
        }
        let primary = chain[0].provider.clone();
        let allow_crossing = self
            .config
            .bindings
            .chain(routed.role)
            .is_some_and(|c| c.allow_crossing_trust_boundary);

        // One id for the whole submission, so a reserve made for attempt 1 is
        // *replaced* by attempt 2's rather than stacking (§14).
        let request_id = Uuid::now_v7();
        let started = Instant::now();

        let mut last_error: Option<AiboError> = None;
        let mut any_reachable = false;

        for (index, binding) in chain.iter().enumerate() {
            if cancel.is_cancelled() {
                return Outcome::Partial {
                    text: String::new(),
                    reason: PartialReason::Cancelled,
                    provider: None,
                };
            }

            // The ceiling bounds the *request*, not each attempt: starting
            // another chain entry after it has expired would multiply the knob
            // by the length of the chain, which is not a ceiling at all.
            if tokio::time::Instant::now() >= deadline {
                return failed(deadline_expired());
            }

            // §14: fallback must never cross a provider's trust boundary
            // without consent. Only entries *after* the primary are a
            // substitution, so the primary itself is never gated.
            if index > 0
                && !allow_crossing
                && self
                    .config
                    .trust
                    .crosses_outwards(&primary, &binding.provider)
            {
                events.emit(SessionEvent::Skipped {
                    provider: binding.provider.clone(),
                    reason: SkipReason::TrustBoundary,
                });
                tracing::info!(
                    provider = %binding.provider,
                    "skipped: falling here would move the user's text outside the boundary they chose (§14)"
                );
                continue;
            }

            let provider = match self.providers.for_binding(binding) {
                Ok(provider) => provider,
                // A chain entry the user has since removed is a gap in the
                // configuration, not a failure of this request.
                Err(AiboError::NoProviderConfigured) => {
                    events.emit(SessionEvent::Skipped {
                        provider: binding.provider.clone(),
                        reason: SkipReason::NotConfigured,
                    });
                    continue;
                }
                // Anything else — notably §3a's Codex allowlist rejection — is
                // a 400-class bug. §4 does not fall back on those.
                //
                // The error is returned **verbatim**, and that is the whole
                // point: `AiboError::ModelRejected` carries the refused id and
                // the ids that do work, and §13 gives it the Inline treatment —
                // one sentence and *one action button*, which the panel can
                // only spend on "switch to gpt-5.6-sol" if the alternatives
                // survive this line. Collapsing a pre-dispatch rejection into
                // `Internal`, or into anything that keeps only a message, would
                // hand the user "something went wrong · copy diagnostics" for
                // the one failure in the product that already knows its own fix
                // — the exact opaque dead end checking before dispatch exists
                // to prevent.
                Err(error) => {
                    if let AiboError::ModelRejected {
                        model,
                        alternatives,
                        ..
                    } = &error
                    {
                        tracing::warn!(
                            provider = %binding.provider,
                            %model,
                            ?alternatives,
                            "binding refused before dispatch; §4 does not fall back on a 400"
                        );
                    }
                    return failed(error);
                }
            };

            // §13: per-provider offline state with hysteresis. A degraded
            // provider is skipped without a network call until its backoff
            // expires, then probed — §4 lists "a failed health probe" as a
            // fallback trigger in its own right.
            match self.health.usability(&binding.provider) {
                Usability::Healthy => {}
                Usability::Skip => {
                    events.emit(SessionEvent::Skipped {
                        provider: binding.provider.clone(),
                        reason: SkipReason::Degraded,
                    });
                    continue;
                }
                Usability::ProbeRequired => {
                    let probe = tokio::select! {
                        biased;
                        () = cancel.cancelled() => {
                            return Outcome::Partial {
                                text: String::new(),
                                reason: PartialReason::Cancelled,
                                provider: None,
                            };
                        }
                        () = tokio::time::sleep_until(deadline) => {
                            return failed(deadline_expired());
                        }
                        result = crate::health::probe(&self.health, &*provider) => result,
                    };
                    let (usable, changed) = probe;
                    if let Some(health) = changed {
                        events.emit(SessionEvent::ProviderHealth {
                            provider: binding.provider.clone(),
                            health,
                        });
                    }
                    if !usable {
                        events.emit(SessionEvent::Skipped {
                            provider: binding.provider.clone(),
                            reason: SkipReason::FailedHealthProbe,
                        });
                        continue;
                    }
                }
            }
            any_reachable = true;

            let substituted_for = (index > 0).then(|| primary.clone());
            match self
                .attempt(
                    &submission,
                    surface,
                    routed.role,
                    binding,
                    provider.as_ref(),
                    request_id,
                    started,
                    deadline,
                    substituted_for,
                    cancel,
                    events,
                )
                .await
            {
                AttemptResult::Done(outcome) => return outcome,
                AttemptResult::TryNext(error) => {
                    tracing::info!(
                        provider = %binding.provider,
                        %error,
                        "chain entry failed; moving to the next (§4)"
                    );
                    last_error = Some(error);
                }
                AttemptResult::Fatal(error) => return failed(error),
            }
        }

        // Nothing in the chain worked. Which error to show is a §13 question:
        // "every cloud provider we could reach was degraded" is Offline, and
        // "the chain has no configured entry" is the one error allowed to
        // interrupt.
        match last_error {
            Some(error) => failed(error),
            None if any_reachable => failed(AiboError::Offline),
            None => {
                let all_degraded = chain.iter().any(|b| self.health.is_degraded(&b.provider));
                if all_degraded {
                    failed(AiboError::Offline)
                } else {
                    failed(self.no_provider_for(&submission))
                }
            }
        }
    }

    /// "Nothing in the chain can serve this", named as specifically as the
    /// submission allows.
    ///
    /// Without an attachment that is [`AiboError::NoProviderConfigured`] — the
    /// one error §13 lets interrupt, because aibo genuinely cannot do anything.
    /// With one it is [`AiboError::VisionUnsupported`] with no binding, which is
    /// Inline: the user's text setup is fine and the fix is to configure a
    /// vision provider or remove the image. Collapsing the second into the
    /// first is the defect this whole feature exists to retire.
    fn no_provider_for(&self, submission: &Submission) -> AiboError {
        if submission.attachments.is_empty() {
            return AiboError::NoProviderConfigured;
        }
        AiboError::no_vision_provider(
            submission.attachments.len(),
            // The chain's own entries when it has some (a provider was
            // configured but every entry was skipped), otherwise §4's list of
            // providers the user could configure. Either way the message ends
            // in something to do.
            match self.config.bindings.vision_alternatives() {
                empty if empty.is_empty() => {
                    vision_providers().iter().map(ToString::to_string).collect()
                }
                alternatives => alternatives,
            },
        )
    }

    /// §14's pre-dispatch reserve, **with attachments counted**.
    ///
    /// [`estimate_request_usage`] measures the assembled *messages*, and an
    /// attachment is not one — it rides on [`ChatRequest::attachments`]. Left
    /// at the zero that function returns, a screenshot reserves nothing while
    /// billing as several thousand input tokens: on a model priced at $2.50/Mtok
    /// a single retina capture is more than a whole Ask turn of text, and §14's
    /// hard stop would let it straight through the ceiling it exists to hold.
    ///
    /// `Usage::image_tokens` is the field the price table already meters
    /// (`ModelPrices::image`, falling back to `input`, which is what OpenAI and
    /// Anthropic actually do), so the reconcile after the stream lands in the
    /// same slot and replaces the estimate with the provider's real figure.
    fn estimate_cost(
        &self,
        request: &ChatRequest,
        binding: &ModelBinding,
        tier: Option<&ProviderTier>,
    ) -> Option<Micros> {
        let mut usage = estimate_request_usage(request);
        usage.image_tokens = request.estimated_attachment_tokens() as u64;
        self.config
            .prices
            .cost(&binding.provider, &binding.model, tier, &usage)
    }

    /// Model capabilities for one binding, clamped by §14's per-role caps.
    ///
    /// §10 puts capabilities on the model, not the provider, so the catalogue
    /// wins where it has an entry. The clamp is §14's first bullet — *"per-role
    /// caps … enforced in `aibo-core` before the request is built"* — applied
    /// by shrinking the capabilities prompt assembly derives its budget from,
    /// which is the only way to enforce it without a second budget type.
    fn capabilities_for(
        &self,
        binding: &ModelBinding,
        provider: &dyn aibo_core::traits::Provider,
        role: Role,
    ) -> Capabilities {
        let base = self
            .live_catalogue
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(binding.provider.clone(), binding.model.clone()))
            .cloned()
            .unwrap_or_else(|| provider.capabilities());

        let caps: RoleCaps = default_role_caps(role);
        Capabilities {
            max_context: base.max_context.min(caps.max_context_tokens),
            max_output: Some(
                base.max_output
                    .unwrap_or(usize::MAX)
                    .min(caps.max_output_tokens as usize),
            ),
            ..base
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn attempt(
        &self,
        submission: &Submission,
        surface: Surface,
        role: Role,
        binding: &ModelBinding,
        provider: &dyn aibo_core::traits::Provider,
        request_id: Uuid,
        started: Instant,
        deadline: tokio::time::Instant,
        substituted_for: Option<ProviderId>,
        cancel: &CancellationToken,
        events: &EventSink,
    ) -> AttemptResult {
        let capabilities = self.capabilities_for(binding, provider, role);

        // §10: capabilities are per model, so the gate is per binding — one
        // provider routinely serves a vision model and a text-only one.
        //
        // **Refuse; never strip.** Sending the text with the image removed
        // gets a fluent, confident answer about an image the model never saw,
        // with nothing in the transcript to say so. `Capabilities::default()`
        // has `vision: false`, so a model whose capabilities were never
        // populated refuses rather than dropping.
        //
        // Before assembly, deliberately. The §5 budget would otherwise refuse
        // the same request first with `ContextTooLarge`, and "this model cannot
        // see" is the more actionable of the two — it names the binding, offers
        // models that would work, and §13 spends its one Inline action on
        // "switch model" rather than on "copy diagnostics". Before the reserve
        // too: §14 must not charge for a request that was never going to go.
        //
        // This binding is unusable without spending a request. When fallback
        // is enabled, let the loop inspect the next binding: capabilities are
        // per model, so a blind primary says nothing about a vision-capable
        // secondary. With fallback disabled, dispatch_order contains only this
        // entry and the same typed error is returned below.
        let unsupported = submission
            .attachments
            .iter()
            .filter(|a| !capabilities.accepts(a))
            .count();
        if unsupported > 0 {
            tracing::info!(
                provider = %binding.provider,
                model = %binding.model,
                attachments = unsupported,
                "binding cannot accept the attachments; refusing rather than dropping them (§10)"
            );
            return AttemptResult::TryNext(AiboError::vision_unsupported(
                binding.clone(),
                unsupported,
                self.config.bindings.vision_alternatives(),
            ));
        }

        // §5: assembly applies the priority table and the middle-out
        // truncation. An oversized *instruction* is `ContextTooLarge`, which is
        // the user's problem to fix and never another provider's to absorb.
        let mut inputs =
            PromptInputs::new(request_id, surface, role, binding.clone(), capabilities);
        inputs.conversation_id = submission.conversation_id;
        // What is left of the ceiling, not the whole of it. This is the number
        // that lands on `RequestBudget::deadline`, so a provider that enforces
        // its own transport timeout enforces the same instant the engine does
        // — a second chain entry inheriting a fresh 60 s would let the two
        // disagree about when the request is over.
        inputs.deadline = deadline.saturating_duration_since(tokio::time::Instant::now());
        inputs.instruction = Some(submission.instruction.clone());
        inputs.app = submission.capture.app.clone();
        inputs.field = submission.capture.field.clone();
        inputs.selection = submission.capture.selection.clone();
        inputs.clipboard = submission.capture.clipboard.clone();
        // The deliberate act, carried through. Assembly charges these against
        // the §5 budget and frames them as quoted data; it never drops one.
        inputs.attachments = submission.attachments.clone();
        inputs.history = submission.history.clone();
        inputs.verb = crate::verb::parse_leading_verb(&submission.instruction);

        let assembled = match prompts::assemble(&inputs) {
            Ok(assembled) => assembled,
            Err(error) => return AttemptResult::Fatal(error),
        };
        let mut request = assembled.request;
        tracing::debug!(
            provider = %binding.provider,
            model = %binding.model,
            tokens = %assembled.report.total_tokens,
            truncated = assembled.report.payload_truncated,
            "assembled (§5)"
        );

        // §15: prompt caching is "the real lever" for TTFT. Assembly guarantees
        // the leading messages are byte-identical across invocations; this is
        // where that guarantee becomes observable, so a prefix that stops being
        // stable shows up as a churning `key` in the log rather than as an
        // unexplained TTFT regression. The marker itself is emitted by the
        // provider adapter, which recomputes the same plan with
        // `prompts::cache_plan(&req, &caps)`.
        let cache = assembled.cache;
        tracing::debug!(
            provider = %binding.provider,
            supported = cache.supported,
            messages = cache.stable_messages,
            prefix = %cache.prefix_tokens,
            key = format_args!("{:016x}", cache.prefix_key),
            worthwhile = cache.worthwhile(),
            "prompt cache prefix (§15)"
        );

        // §14: estimate before dispatch, reconcile after. `Usage` never arrives
        // on a cancelled or failed stream, so a meter that waits for it both
        // under-reports and cannot stop anything.
        let tier = self.config.tiers.get(&binding.provider);
        let estimate = self.estimate_cost(&request, binding, tier);
        let reserved = {
            let mut meter = self.spend.lock().unwrap_or_else(|e| e.into_inner());
            match meter.reserve(request_id, estimate) {
                Ok(reservation) => reservation.micros,
                // A hard monthly stop is §13's Inline treatment, not a reason
                // to try somewhere else — the next provider costs money too.
                Err(error) => return AttemptResult::Fatal(error),
            }
        };
        request.budget.reserved_cost_micros = reserved;
        let mut reservation = ReservationGuard {
            engine: self,
            request: request_id,
            active: true,
        };

        // §13's wall-clock ceiling covers the initial POST too: a provider that
        // accepts the connection and then never answers must not park the panel
        // forever just because no token ever arrived to start the stream clock.
        // `attempt_cancel` is a child of the session token so expiry aborts this
        // attempt's in-flight request without cancelling the session — `esc`
        // still propagates down from the parent.
        let attempt_cancel = cancel.child_token();
        let stream = loop {
            let chat = provider.chat(request.clone(), attempt_cancel.clone());
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    attempt_cancel.cancel();
                    return AttemptResult::Done(Outcome::Partial {
                        text: String::new(),
                        reason: PartialReason::Cancelled,
                        provider: Some(binding.provider.clone()),
                    });
                }
                () = tokio::time::sleep_until(deadline) => {
                    attempt_cancel.cancel();
                    return AttemptResult::Fatal(deadline_expired());
                }
                result = chat => result,
            };

            match result {
                Ok(stream) => break stream,
                Err(AiboError::RateLimited {
                    provider: limited_provider,
                    retry_after: Some(wait),
                }) if wait <= surface.first_token_target() => {
                    // A short server-directed pause is cheaper and more
                    // private than substituting another provider. It remains
                    // inside the request's absolute deadline and is
                    // cancellation-aware.
                    tracing::debug!(
                        provider = %limited_provider,
                        retry_after_ms = wait.as_millis(),
                        "short rate limit; waiting within the surface budget"
                    );
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => {
                            attempt_cancel.cancel();
                            return AttemptResult::Done(Outcome::Partial {
                                text: String::new(),
                                reason: PartialReason::Cancelled,
                                provider: Some(binding.provider.clone()),
                            });
                        }
                        () = tokio::time::sleep_until(deadline) => {
                            attempt_cancel.cancel();
                            return AttemptResult::Fatal(deadline_expired());
                        }
                        () = tokio::time::sleep(wait) => {}
                    }
                }
                Err(error) => {
                    self.note_failure(&binding.provider, &error, events);
                    return if fallback_eligible_for_surface(&error, surface) {
                        AttemptResult::TryNext(error)
                    } else {
                        // §4: "It does not trigger on a 400 — that's a bug in aibo,
                        // and it should surface as one rather than silently
                        // retrying elsewhere."
                        AttemptResult::Fatal(error)
                    };
                }
            }
        };

        events.emit(SessionEvent::Dispatched {
            provider: binding.provider.clone(),
            model: binding.model.clone(),
            substituted_for: substituted_for.clone(),
        });
        if let Some(original) = &substituted_for {
            // §14: "must be visible when they fire".
            tracing::info!(
                substitute = %binding.provider,
                original = %original,
                "role chain fell back (§4)"
            );
        }

        let dispatched_at = Instant::now();
        let result = dispatch::drive(stream, &attempt_cancel, deadline, started, events).await;
        let latency_ms = dispatched_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;

        // -- §14 reconcile ---------------------------------------------------
        let cost = result.usage.as_ref().and_then(|usage| {
            self.config
                .prices
                .cost(&binding.provider, &binding.model, tier, usage)
        });
        match result.usage {
            Some(usage) => {
                let committed = {
                    let mut meter = self.spend.lock().unwrap_or_else(|e| e.into_inner());
                    meter.reconcile(request_id, cost);
                    meter.committed_micros()
                };
                events.emit(SessionEvent::Cost {
                    usage,
                    cost_micros: cost,
                    committed_micros: committed,
                });
                reservation.disarm();
            }
            // §14: "release on failure". No usage means no bill we can prove,
            // and holding the reserve forever would block the monthly cap.
            None => {
                self.release(request_id);
                reservation.disarm();
            }
        }

        // -- §13 health ------------------------------------------------------
        let clean_terminal = result.error.is_none()
            && result.stop.is_some()
            && result.stop.as_ref() != Some(&StopReason::Cancelled)
            && !result.cancelled;
        if clean_terminal {
            if let Some(health) = self
                .health
                .record_success(&binding.provider, Duration::from_millis(latency_ms))
            {
                events.emit(SessionEvent::ProviderHealth {
                    provider: binding.provider.clone(),
                    health,
                });
            }
        } else if let Some(error) = &result.error {
            self.note_failure(&binding.provider, error, events);
        }

        // -- §13 partial results ---------------------------------------------
        if result.cancelled {
            return AttemptResult::Done(
                self.partial(
                    submission,
                    surface,
                    binding,
                    result.text,
                    PartialReason::Cancelled,
                    usage_or_default(&result.usage),
                    cost,
                    latency_ms,
                )
                .await,
            );
        }

        let may_fall_back = result.may_fall_back();
        let tokens_seen = result.tokens_seen;
        if let Some(error) = result.error {
            if may_fall_back {
                return AttemptResult::TryNext(error);
            }
            if tokens_seen {
                // The user has seen text. It stays in the panel marked
                // truncated; it is never inserted, and it is never retried
                // elsewhere (§13).
                events.emit(SessionEvent::Failed(Arc::new(error)));
                return AttemptResult::Done(
                    self.partial(
                        submission,
                        surface,
                        binding,
                        result.text,
                        PartialReason::StreamFailed,
                        usage_or_default(&result.usage),
                        cost,
                        latency_ms,
                    )
                    .await,
                );
            }
            return AttemptResult::Fatal(error);
        }

        let stop = result.stop.unwrap_or(StopReason::EndTurn);
        if stop == StopReason::Cancelled {
            return AttemptResult::Done(
                self.partial(
                    submission,
                    surface,
                    binding,
                    result.text,
                    PartialReason::Cancelled,
                    usage_or_default(&result.usage),
                    cost,
                    latency_ms,
                )
                .await,
            );
        }
        if matches!(stop, StopReason::ToolUse | StopReason::ContentFilter) {
            return AttemptResult::Done(
                self.partial(
                    submission,
                    surface,
                    binding,
                    result.text,
                    PartialReason::StoppedEarly(stop),
                    usage_or_default(&result.usage),
                    cost,
                    latency_ms,
                )
                .await,
            );
        }

        // -- §5 post-filter ---------------------------------------------------
        let source = submission
            .capture
            .selection
            .clone()
            .or_else(|| submission.capture.field.as_ref().map(|f| f.prefix.clone()))
            .unwrap_or_default();
        // Order matters. `post_process` runs first because a model that stacks
        // both defects emits the preamble *outside* the repeated prefix
        // ("Sure, The deployment should be…"); stripping the prefix first would
        // then find nothing to strip. The reverse order handles no case the
        // first does not.
        let raw_text = result.text;
        let filtered = prompts::post_process(surface, &source, &raw_text);
        let text = crate::filter::strip_prefix_repetition(surface, &source, &filtered.text);
        if text != filtered.text {
            // §5: "Log when the filter fires so you can see quality drift."
            tracing::info!(
                provider = %binding.provider,
                model = %binding.model,
                "stripped a leading repetition of the supplied prefix (§5)"
            );
        }

        let usage = result.usage.unwrap_or_default();
        let output_tokens = if usage.output_tokens > 0 {
            usage.output_tokens as usize
        } else {
            Tokens::estimate(&text).get()
        };

        let completion = Completion {
            text,
            raw_text,
            filtered: filtered.fired,
            stop: stop.clone(),
            provider: binding.provider.clone(),
            model: binding.model.clone(),
            usage,
            cost_micros: cost,
            latency_ms,
            // §4: escalation is offered only on the two cheap objective
            // signals, never taken automatically.
            offer_escalation: should_offer_escalation(
                role,
                stop == StopReason::Length,
                output_tokens,
            ),
            conversation_id: submission.conversation_id,
        };

        // -- §12 persist ------------------------------------------------------
        let conversation_id = self
            .persist(Exchange {
                conversation_id: submission.conversation_id,
                surface,
                source_app: source_app(submission),
                instruction: Some(submission.instruction.clone()),
                assistant: completion.text.clone(),
                provider: completion.provider.clone(),
                model: completion.model.clone(),
                usage: completion.usage,
                cost_micros: completion.cost_micros,
                latency_ms: completion.latency_ms,
                truncated: false,
            })
            .await;

        AttemptResult::Done(Outcome::Completed(Box::new(Completion {
            conversation_id,
            ..completion
        })))
    }

    /// Build — and persist — a §13 partial result.
    ///
    /// The text is written to history because a user who lost half a rewrite
    /// to a dropped connection should still be able to find it, and it is
    /// *only* written to history: [`Outcome::Partial`] has no insertable text
    /// by construction.
    #[allow(clippy::too_many_arguments)]
    async fn partial(
        &self,
        submission: &Submission,
        surface: Surface,
        binding: &ModelBinding,
        text: String,
        reason: PartialReason,
        usage: Usage,
        cost: Option<Micros>,
        latency_ms: u64,
    ) -> Outcome {
        if !text.is_empty() {
            self.persist(Exchange {
                conversation_id: submission.conversation_id,
                surface,
                source_app: source_app(submission),
                instruction: Some(submission.instruction.clone()),
                assistant: text.clone(),
                provider: binding.provider.clone(),
                model: binding.model.clone(),
                usage,
                cost_micros: cost,
                latency_ms,
                truncated: true,
            })
            .await;
        }
        Outcome::Partial {
            text,
            reason,
            provider: Some(binding.provider.clone()),
        }
    }

    fn release(&self, request: Uuid) {
        self.spend
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .release(request);
    }

    /// Fold one failure into the §13 health table and emit the transition.
    fn note_failure(&self, provider: &ProviderId, error: &AiboError, events: &EventSink) {
        let Some(kind) = FailureKind::classify(error) else {
            // A 429, a 400 or an auth failure says nothing about reachability.
            return;
        };
        if let Some(health) = self.health.record_failure(provider, kind) {
            events.emit(SessionEvent::ProviderHealth {
                provider: provider.clone(),
                health,
            });
        }
    }

    async fn persist(&self, exchange: Exchange) -> Option<Uuid> {
        match self.store.record(exchange).await {
            Ok(id) => id,
            Err(error) => {
                // §12/§13: history is not the product. A failed write is
                // logged and the answer is still delivered.
                tracing::warn!(%error, "could not persist the conversation (§12)");
                None
            }
        }
    }
}

/// §4's [`RouteInput`], built from the capture alone — no allocation beyond
/// the input itself and no tokenizer, per §4's own constraint.
///
/// A free function rather than a method: it reads nothing from the engine, and
/// routing is the part of the request path that §4 wants exhaustively testable
/// without one.
fn route_input(submission: &Submission, surface: Surface) -> RouteInput {
    let capture = &submission.capture;
    let selection = capture.selection.as_deref().unwrap_or("");
    let (prefix, suffix) = capture
        .field
        .as_ref()
        .map_or(("", ""), |f| (f.prefix.as_str(), f.suffix.as_str()));

    // §4, verbatim: `payload_tokens` is "selection + clipboard + field prefix".
    // The clipboard is not incidental to routing — §5 gives it its own budget
    // priority and it is the thing `has_image` is derived from two lines below.
    // Omitting it kept a Transform carrying a large clipboard attachment on
    // `Fast` (rule 5's `payload_tokens <= 400`) when the request it actually
    // builds is a `Smart`-sized one.
    let clipboard = capture
        .clipboard
        .as_ref()
        .and_then(attachable_clipboard_text)
        .unwrap_or("");

    let payload_tokens = Tokens::estimate(selection)
        + Tokens::estimate(clipboard)
        + Tokens::estimate(prefix)
        + Tokens::estimate(suffix);

    // §4: a fenced block, OR a source app on the code-app list. The
    // "> 30% non-prose characters" clause is deliberately not implemented
    // here — it needs a calibrated threshold that only S9's eval harness
    // can supply, and guessing one would mis-route real prose.
    let has_code = capture.app.as_ref().is_some_and(|a| a.is_code_app)
        || submission.instruction.contains("```")
        || selection.contains("```");

    // §4 rule 2 routes `has_image` to `Vision`, so this flag decides the entire
    // request. It means "the user attached an image", and it must never again
    // mean "an image happens to be on the clipboard".
    //
    // Deriving it from ambient clipboard content was a real defect, observed
    // 2026-07-26: taking any screenshot silently rerouted every subsequent
    // request to `Vision`, and because nothing binds that role the failure
    // surfaced as "No provider is configured yet" while Settings simultaneously
    // showed Codex signed in and healthy — an unactionable contradiction.
    //
    // The fix is structural rather than a comment: `Submission::attachments`
    // can only be populated by a gesture, and the flag is a fact about that
    // list. Note that `clipboard` is read a few lines above for
    // `payload_tokens` and is *not* read here — an ambient clipboard image
    // stays context at §5's budget priority 4, and context is never a routing
    // decision.
    let has_image = submission.has_image_attachment();

    RouteInput {
        surface,
        // `RouteInput`'s fields are bare `usize` (they live in `types.rs`), so
        // the unit is asserted rather than checked at this one boundary.
        prompt_tokens: Tokens::estimate(&submission.instruction).get(),
        payload_tokens: payload_tokens.get(),
        has_code,
        has_image,
        verb: crate::verb::parse_leading_verb(&submission.instruction),
        user_override: submission.role_override,
    }
}

fn source_app(submission: &Submission) -> Option<String> {
    submission
        .capture
        .app
        .as_ref()
        .map(|a| a.identifier.clone())
}

fn usage_or_default(usage: &Option<Usage>) -> Usage {
    usage.unwrap_or_default()
}

/// What one chain entry produced.
enum AttemptResult {
    /// The request is over, one way or another.
    Done(Outcome),
    /// §4 says try the next entry.
    TryNext(AiboError),
    /// §4 says stop: this is a bug in aibo or a budget refusal, and retrying
    /// elsewhere would hide it and spend the user's money twice.
    Fatal(AiboError),
}

fn failed(error: AiboError) -> Outcome {
    Outcome::Failed(Arc::new(error))
}

/// The error [`EngineConfig::request_deadline`] produces when it expires.
///
/// §13 names three timeout phases and this is `Stream`, uniformly, wherever in
/// the request the ceiling runs out. Two reasons, and the second is the one
/// that matters:
///
/// * `Connect` and `FirstToken` are *per-attempt* budgets — "this provider is
///   slow, try the next one". The wall clock is not: it is the budget for the
///   whole request, and by the time it expires there is no next one.
/// * [`AiboError::is_fallback_eligible`] is true for `Connect` and
///   `FirstToken`. Reporting either of those would send the request down the
///   rest of the chain *after* its ceiling had already elapsed, so a two-entry
///   chain would take twice the configured maximum. `Stream` is not
///   fallback-eligible, which is what makes the ceiling a ceiling.
///
/// §13's treatment for `Timeout` is Inline — one sentence and a retry button —
/// which is the right shape for "this took too long".
const fn deadline_expired() -> AiboError {
    AiboError::Timeout {
        phase: aibo_core::error::TimeoutPhase::Stream,
    }
}

/// §4's fallback predicate with the surface-specific part kept at the
/// orchestration boundary. A short `Retry-After` is handled by waiting and
/// retrying the same provider; only a wait that misses the target (or an
/// unspecified wait) is a substitution signal.
fn fallback_eligible_for_surface(error: &AiboError, surface: Surface) -> bool {
    match error {
        AiboError::RateLimited {
            retry_after: Some(wait),
            ..
        } => *wait > surface.first_token_target(),
        _ => error.is_fallback_eligible(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Capture;
    use aibo_core::router::Router;
    use aibo_core::types::{Attachment, AttachmentSource, ClipboardItem, ClipboardKind};

    fn screenshot() -> Attachment {
        Attachment::image(
            AttachmentSource::ScreenRegion,
            vec![0u8; 1024],
            "image/png",
            1568,
            882,
            "Screenshot 14:32",
        )
    }

    fn clipboard(text: &str) -> ClipboardItem {
        ClipboardItem {
            kind: ClipboardKind::Text,
            text: Some(text.to_owned()),
            files: Vec::new(),
            concealed: false,
            transient: false,
            source_app: Some("Safari".into()),
            sequence: 1,
            restorable: true,
        }
    }

    fn submission(capture: Capture) -> Submission {
        let mut s = Submission::new(Uuid::now_v7(), "make this formal");
        s.capture = capture;
        s
    }

    /// §4: `payload_tokens` is "selection + clipboard + field prefix". The
    /// clipboard was missing, so its tokens were invisible to the router.
    #[test]
    fn route_input_counts_the_clipboard_in_payload_tokens() {
        let selection = "a".repeat(400); // 100 tokens
        let attachment = "b".repeat(4_000); // 1000 tokens

        let without = route_input(
            &submission(Capture {
                selection: Some(selection.clone()),
                ..Capture::default()
            }),
            Surface::Transform,
        );
        let with = route_input(
            &submission(Capture {
                selection: Some(selection),
                clipboard: Some(clipboard(&attachment)),
                ..Capture::default()
            }),
            Surface::Transform,
        );

        assert_eq!(without.payload_tokens, 100);
        assert_eq!(with.payload_tokens, 1_100);
    }

    /// The consequence §4 rule 5 makes concrete: a Transform whose selection is
    /// under the 400-token threshold but whose clipboard attachment is not
    /// belongs on `Smart`, because the request that actually goes out carries
    /// both. With the clipboard omitted it stayed on `Fast`.
    #[test]
    fn a_transform_with_a_large_clipboard_escalates_to_smart() {
        let router = Router::with_defaults();
        let selection = "a".repeat(400); // 100 tokens: comfortably under rule 5

        let small = router.route(&route_input(
            &submission(Capture {
                selection: Some(selection.clone()),
                ..Capture::default()
            }),
            Surface::Transform,
        ));
        assert_eq!(small.role, Role::Fast, "rule {}", small.rule);

        let large = router.route(&route_input(
            &submission(Capture {
                selection: Some(selection),
                clipboard: Some(clipboard(&"b".repeat(4_000))),
                ..Capture::default()
            }),
            Surface::Transform,
        ));
        assert_eq!(large.role, Role::Smart, "rule {}", large.rule);
    }

    /// …but only a clipboard that will actually be sent. A concealed item
    /// (§12) never reaches the prompt, so it must not change the route either —
    /// routing and assembly measure the same bytes.
    #[test]
    fn route_input_ignores_a_clipboard_prompt_assembly_will_drop() {
        let mut concealed = clipboard(&"b".repeat(4_000));
        concealed.concealed = true;
        let input = route_input(
            &submission(Capture {
                selection: Some("a".repeat(400)),
                clipboard: Some(concealed),
                ..Capture::default()
            }),
            Surface::Transform,
        );
        assert_eq!(input.payload_tokens, 100);
    }

    /// §4 rule 2: `has_image` is a fact about what the user attached.
    #[test]
    fn has_image_is_true_exactly_when_something_was_attached() {
        let plain = Submission::new(Uuid::now_v7(), "what is this");
        assert!(!route_input(&plain, Surface::Ask).has_image);

        let attached = plain.clone().with_attachment(screenshot());
        assert!(route_input(&attached, Surface::Ask).has_image);
    }

    /// **The regression this whole feature exists for.** An image sitting on
    /// the pasteboard is ambient state: taking a screenshot must not reroute
    /// the next typed question to `Vision`, which nothing binds, and surface as
    /// "No provider is configured yet" beside a healthy provider.
    ///
    /// Note the deliberate asymmetry with the test above: the *same* image, in
    /// the clipboard, changes nothing; attached, it changes everything.
    #[test]
    fn an_image_on_the_clipboard_is_not_an_attachment() {
        let mut item = clipboard("");
        item.kind = ClipboardKind::ImageRef;
        let s = submission(Capture {
            clipboard: Some(item),
            ..Capture::default()
        });

        let input = route_input(&s, Surface::Ask);
        assert!(
            !input.has_image,
            "ambient clipboard content is context (§5 priority 4), never a routing decision"
        );
        assert_eq!(
            Router::with_defaults().route(&input).role,
            Role::Smart,
            "an Ask with nothing attached belongs on Smart, not Vision"
        );
    }

    /// …and with the attachment made deliberately, §4 rule 2 does fire.
    #[test]
    fn an_attached_image_routes_to_vision() {
        let s = Submission::new(Uuid::now_v7(), "what is in this").with_attachment(screenshot());
        let routed = Router::with_defaults().route(&route_input(&s, Surface::Ask));
        assert_eq!(routed.role, Role::Vision, "rule {}", routed.rule);
    }

    /// §13's cap and §4's estimate now share a definition of "character", and
    /// the clipboard is inside both.
    #[test]
    fn the_large_selection_cap_sees_the_clipboard_too() {
        let mut s = Submission::new(Uuid::now_v7(), String::new());
        s.capture.clipboard = Some(clipboard(&"あ".repeat(250_000)));
        assert!(s.total_chars() > Chars::new(DEFAULT_MAX_PAYLOAD_CHARS));
    }
}
