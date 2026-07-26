//! §13 offline detection: **per provider, with hysteresis, never one global
//! boolean**.
//!
//! > *"Detect from connect failures, not a reachability API — reachability
//! > lies. Offline is per-provider with hysteresis, not one global boolean: a
//! > single failed connection to Cerebras says nothing about Ollama or about
//! > Bedrock behind a corporate proxy. Mark a provider degraded after N
//! > consecutive failures, probe before clearing it, and never flap."*
//!
//! Three rules, and each is a distinct piece of state:
//!
//! | Rule | Mechanism |
//! |---|---|
//! | *N consecutive failures* | [`ProviderState::consecutive_failures`], reset by any success |
//! | *probe before clearing* | degraded can only be cleared by [`HealthTable::record_probe_at`] with an `Ok` health, or by a **real request succeeding** |
//! | *never flap* | a failed probe doubles the backoff up to [`HysteresisPolicy::max_probe_backoff`]; while the backoff is unexpired the provider is skipped without a network call |
//!
//! ## What counts as a health signal
//!
//! Not every failure says anything about reachability, and conflating them is
//! how a single 429 takes a working provider offline. [`FailureKind::classify`]
//! is the filter:
//!
//! * a connect failure, a DNS failure, a connect/first-token timeout and a 5xx
//!   are evidence about the *path* → they count;
//! * a 429 means the provider answered, promptly, with a quota decision → it
//!   triggers §4 fallback but **not** degradation;
//! * a 400, an auth failure or a budget stop are aibo's own problem → neither.
//!
//! ## Clock
//!
//! Every method has an `_at(now: Instant)` form. The convenience wrappers call
//! [`Instant::now`]; the tests drive the state machine with a synthetic clock,
//! which is the only way to assert "does not flap" without sleeping.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aibo_core::error::{AiboError, TimeoutPhase};
use aibo_core::traits::Provider;
use aibo_core::types::{Health, ProviderId};

/// §13's *N*. Three consecutive failures, so one dropped packet and one
/// stale-pool retry (§13 "sleep, wake and stale connections") do not take a
/// working provider off the chain.
pub const DEFAULT_DEGRADE_AFTER: u32 = 3;

/// How long after being marked degraded a provider is first re-probed.
pub const DEFAULT_FIRST_PROBE_AFTER: Duration = Duration::from_secs(15);

/// Ceiling on the doubling backoff. A provider that has been down for an hour
/// is still re-probed every five minutes, so recovery is bounded.
pub const DEFAULT_MAX_PROBE_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// The tunable half of §13's hysteresis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HysteresisPolicy {
    /// Consecutive health-relevant failures before a provider is degraded.
    pub degrade_after: u32,
    /// Delay from degradation to the first re-probe.
    pub first_probe_after: Duration,
    /// Ceiling for the doubling backoff between failed probes.
    pub max_probe_backoff: Duration,
}

impl Default for HysteresisPolicy {
    fn default() -> Self {
        Self {
            degrade_after: DEFAULT_DEGRADE_AFTER,
            first_probe_after: DEFAULT_FIRST_PROBE_AFTER,
            max_probe_backoff: DEFAULT_MAX_PROBE_BACKOFF,
        }
    }
}

/// A failure that is evidence about the network path (§13).
///
/// Constructed only through [`FailureKind::classify`], which is where the
/// "a 429 is not an outage" rule lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// No usable network path at all — [`AiboError::Offline`].
    Connect,
    /// Connected but silent: a connect or first-token timeout.
    Timeout,
    /// The provider answered 5xx. It is reachable but not serving.
    Server,
}

impl FailureKind {
    /// Classify an error as a reachability signal, or `None` if it says
    /// nothing about the path.
    ///
    /// A 429 is deliberately `None`: the provider answered, and answering is
    /// the opposite of being offline. §4 still falls back on it — that is
    /// [`AiboError::is_fallback_eligible`], a different question.
    pub fn classify(error: &AiboError) -> Option<Self> {
        match error {
            AiboError::Offline => Some(Self::Connect),
            AiboError::Timeout { phase } => match phase {
                TimeoutPhase::Connect | TimeoutPhase::FirstToken => Some(Self::Timeout),
                // A stream that stalls after tokens have arrived is a stream
                // problem, not a reachability one, and §13 forbids retrying it
                // anyway.
                TimeoutPhase::Stream => None,
            },
            AiboError::ProviderUnavailable { status, .. } if *status >= 500 => Some(Self::Server),
            _ => None,
        }
    }

    /// The redacted `reason` string carried on [`Health::Degraded`] and
    /// [`Health::Unavailable`].
    ///
    /// Deliberately a fixed set of literals: §6 keeps captured text and
    /// credentials out of anything that can reach a diagnostics bundle, and a
    /// provider error body is neither trusted nor short.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Connect => "connect failed",
            Self::Timeout => "timed out before the first token",
            Self::Server => "server error",
        }
    }
}

/// One provider's offline state.
#[derive(Debug, Clone)]
pub struct ProviderState {
    /// The health the UI should show (§13 offline badge).
    pub health: Health,
    /// Failures since the last success. Reset by a success, *not* by a failed
    /// probe — that is what makes the counter hysteresis rather than a window.
    pub consecutive_failures: u32,
    /// Whether the provider is currently off the chain.
    pub degraded: bool,
    /// When degraded, the earliest instant a probe may be attempted.
    next_probe_at: Option<Instant>,
    /// Current backoff, doubled on each failed probe.
    probe_backoff: Duration,
}

impl ProviderState {
    fn unknown(policy: &HysteresisPolicy) -> Self {
        Self {
            health: Health::Unknown,
            consecutive_failures: 0,
            degraded: false,
            next_probe_at: None,
            probe_backoff: policy.first_probe_after,
        }
    }
}

/// What the dispatcher should do with a provider right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usability {
    /// Use it.
    Healthy,
    /// Degraded, and the backoff has expired: probe before using it. §13's
    /// *"probe before clearing it"*.
    ProbeRequired,
    /// Degraded and the backoff has not expired. Skip to the next chain entry
    /// with no network call at all — this is what stops the flap.
    Skip,
}

/// Per-provider offline state for the whole app (§13).
///
/// Shared behind an `Arc`. The mutex is held for the duration of a map
/// update only — never across an `await` — so this is safe to consult on the
/// dispatch path.
#[derive(Debug)]
pub struct HealthTable {
    policy: HysteresisPolicy,
    states: Mutex<BTreeMap<ProviderId, ProviderState>>,
}

impl Default for HealthTable {
    fn default() -> Self {
        Self::new(HysteresisPolicy::default())
    }
}

impl HealthTable {
    /// An empty table. Every provider starts [`Health::Unknown`] and usable —
    /// aibo does not probe before the first request, because the first request
    /// *is* the probe (§15's cold-start budget).
    pub fn new(policy: HysteresisPolicy) -> Self {
        Self {
            policy,
            states: Mutex::new(BTreeMap::new()),
        }
    }

    /// The policy in force.
    pub const fn policy(&self) -> &HysteresisPolicy {
        &self.policy
    }

    fn with<T>(&self, f: impl FnOnce(&mut BTreeMap<ProviderId, ProviderState>) -> T) -> T {
        // A poisoned mutex here must not take the tray down (§6): the map is
        // plain data, so recovering the inner value loses nothing.
        let mut guard = self.states.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    /// A provider's current health, `Unknown` if never seen.
    pub fn health(&self, provider: &ProviderId) -> Health {
        self.with(|s| {
            s.get(provider)
                .map(|state| state.health.clone())
                .unwrap_or(Health::Unknown)
        })
    }

    /// Whether a provider is currently degraded (§13's offline badge).
    pub fn is_degraded(&self, provider: &ProviderId) -> bool {
        self.with(|s| s.get(provider).is_some_and(|state| state.degraded))
    }

    /// Every provider the table has an opinion about, in a stable order.
    pub fn snapshot(&self) -> Vec<(ProviderId, Health)> {
        self.with(|s| {
            s.iter()
                .map(|(id, state)| (id.clone(), state.health.clone()))
                .collect()
        })
    }

    /// [`HealthTable::usability_at`] with the real clock.
    pub fn usability(&self, provider: &ProviderId) -> Usability {
        self.usability_at(provider, Instant::now())
    }

    /// What to do with `provider` at `now`.
    pub fn usability_at(&self, provider: &ProviderId, now: Instant) -> Usability {
        self.with(|s| {
            let Some(state) = s.get(provider) else {
                return Usability::Healthy;
            };
            if !state.degraded {
                return Usability::Healthy;
            }
            match state.next_probe_at {
                Some(at) if now >= at => Usability::ProbeRequired,
                Some(_) => Usability::Skip,
                None => Usability::ProbeRequired,
            }
        })
    }

    /// [`HealthTable::record_failure_at`] with the real clock.
    pub fn record_failure(&self, provider: &ProviderId, kind: FailureKind) -> Option<Health> {
        self.record_failure_at(provider, kind, Instant::now())
    }

    /// Record one health-relevant failure.
    ///
    /// Returns the new [`Health`] **only when it changed**, so a caller can
    /// emit one `ProviderHealth` event per transition rather than one per
    /// request.
    pub fn record_failure_at(
        &self,
        provider: &ProviderId,
        kind: FailureKind,
        now: Instant,
    ) -> Option<Health> {
        let policy = self.policy;
        self.with(|s| {
            let state = s
                .entry(provider.clone())
                .or_insert_with(|| ProviderState::unknown(&policy));
            let before = state.health.clone();

            state.consecutive_failures = state.consecutive_failures.saturating_add(1);

            if state.consecutive_failures >= policy.degrade_after {
                if !state.degraded {
                    // First crossing: this is where the backoff clock starts.
                    state.degraded = true;
                    state.probe_backoff = policy.first_probe_after;
                    state.next_probe_at = Some(now + state.probe_backoff);
                }
                state.health = match kind {
                    // §13 asks for connect failure and DNS failure to be
                    // distinguishable from "reachable but broken".
                    FailureKind::Connect | FailureKind::Timeout => Health::Unavailable {
                        reason: kind.reason().to_owned(),
                    },
                    FailureKind::Server => Health::Degraded {
                        reason: kind.reason().to_owned(),
                        consecutive_failures: state.consecutive_failures,
                    },
                };
            } else {
                // Below the threshold the provider stays on the chain, but the
                // failures are visible — a UI that only ever shows "fine" until
                // it shows "offline" is the silent failure §13 forbids.
                state.health = Health::Degraded {
                    reason: kind.reason().to_owned(),
                    consecutive_failures: state.consecutive_failures,
                };
            }

            (before != state.health).then(|| state.health.clone())
        })
    }

    /// [`HealthTable::record_success_at`] with the real clock.
    pub fn record_success(&self, provider: &ProviderId, latency: Duration) -> Option<Health> {
        self.record_success_at(provider, latency, Instant::now())
    }

    /// Record a **real request** succeeding.
    ///
    /// This clears degradation outright, and legitimately so: a completed
    /// request is strictly stronger evidence than the health probe §13 asks
    /// for. It cannot cause a flap either, because going degraded again needs
    /// [`HysteresisPolicy::degrade_after`] fresh consecutive failures.
    pub fn record_success_at(
        &self,
        provider: &ProviderId,
        latency: Duration,
        _now: Instant,
    ) -> Option<Health> {
        let policy = self.policy;
        self.with(|s| {
            let state = s
                .entry(provider.clone())
                .or_insert_with(|| ProviderState::unknown(&policy));
            let before = state.health.clone();

            state.consecutive_failures = 0;
            state.degraded = false;
            state.next_probe_at = None;
            state.probe_backoff = policy.first_probe_after;
            state.health = Health::Ok { latency };

            (before != state.health).then(|| state.health.clone())
        })
    }

    /// Record the outcome of an explicit [`Provider::health`] probe.
    ///
    /// A probe reporting [`Health::Ok`] clears the degradation — this is the
    /// *"probe before clearing it"* half of §13. Anything else keeps it and
    /// doubles the backoff, which is the *"never flap"* half.
    pub fn record_probe_at(
        &self,
        provider: &ProviderId,
        result: &aibo_core::error::Result<Health>,
        now: Instant,
    ) -> Option<Health> {
        let policy = self.policy;
        self.with(|s| {
            let state = s
                .entry(provider.clone())
                .or_insert_with(|| ProviderState::unknown(&policy));
            let before = state.health.clone();

            match result {
                Ok(Health::Ok { latency }) => {
                    state.consecutive_failures = 0;
                    state.degraded = false;
                    state.next_probe_at = None;
                    state.probe_backoff = policy.first_probe_after;
                    state.health = Health::Ok { latency: *latency };
                }
                other => {
                    state.degraded = true;
                    state.probe_backoff = (state.probe_backoff * 2).min(policy.max_probe_backoff);
                    state.next_probe_at = Some(now + state.probe_backoff);
                    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                    state.health = match other {
                        Ok(h) => h.clone(),
                        Err(e) => Health::Unavailable {
                            // `AiboError`'s `Display` is the diagnostic form and
                            // never contains captured text or a secret (§13).
                            reason: e.to_string(),
                        },
                    };
                }
            }

            (before != state.health).then(|| state.health.clone())
        })
    }

    /// §13 wake handling: *"re-probe provider health and clear the degraded
    /// flags"* — except the flags are **not** cleared here, only the backoff.
    ///
    /// Clearing on a wake notification would be exactly the reachability-API
    /// lie §13 warns about: the lid opening says nothing about whether the
    /// corporate proxy is back. What it does say is that waiting out a
    /// five-minute backoff is now pointless, so the next request probes
    /// immediately.
    pub fn probe_all_now_at(&self, now: Instant) {
        self.with(|s| {
            for state in s.values_mut() {
                if state.degraded {
                    state.next_probe_at = Some(now);
                    state.probe_backoff = self.policy.first_probe_after;
                }
            }
        });
    }

    /// [`HealthTable::probe_all_now_at`] with the real clock.
    pub fn probe_all_now(&self) {
        self.probe_all_now_at(Instant::now());
    }
}

/// Run one health probe and fold the result into the table.
///
/// Returns `true` when the provider may be used, and the health transition (if
/// any) so the caller can emit a single `ProviderHealth` event.
pub async fn probe(table: &HealthTable, provider: &dyn Provider) -> (bool, Option<Health>) {
    let id = provider.id();
    let result = provider.health().await;
    let usable = matches!(result, Ok(Health::Ok { .. }));
    let changed = table.record_probe_at(&id, &result, Instant::now());
    if !usable {
        tracing::debug!(provider = %id, "health probe failed; provider stays degraded");
    }
    (usable, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> HealthTable {
        HealthTable::new(HysteresisPolicy {
            degrade_after: 3,
            first_probe_after: Duration::from_secs(10),
            max_probe_backoff: Duration::from_secs(80),
        })
    }

    fn cerebras() -> ProviderId {
        ProviderId::CEREBRAS
    }

    #[test]
    fn a_single_failure_does_not_degrade() {
        let t = table();
        let now = Instant::now();
        t.record_failure_at(&cerebras(), FailureKind::Connect, now);
        assert!(!t.is_degraded(&cerebras()));
        assert_eq!(t.usability_at(&cerebras(), now), Usability::Healthy);
    }

    #[test]
    fn n_consecutive_failures_degrade() {
        let t = table();
        let now = Instant::now();
        for _ in 0..3 {
            t.record_failure_at(&cerebras(), FailureKind::Connect, now);
        }
        assert!(t.is_degraded(&cerebras()));
        assert_eq!(t.usability_at(&cerebras(), now), Usability::Skip);
    }

    #[test]
    fn alternating_failure_and_success_never_degrades() {
        // The flap test: with degrade_after = 3, a 50% failure rate must never
        // take the provider off the chain, because the counter is consecutive.
        let t = table();
        let now = Instant::now();
        for _ in 0..50 {
            t.record_failure_at(&cerebras(), FailureKind::Connect, now);
            t.record_failure_at(&cerebras(), FailureKind::Connect, now);
            t.record_success_at(&cerebras(), Duration::from_millis(20), now);
            assert!(!t.is_degraded(&cerebras()));
        }
    }

    #[test]
    fn degraded_is_skipped_until_the_backoff_expires_then_probed() {
        let t = table();
        let start = Instant::now();
        for _ in 0..3 {
            t.record_failure_at(&cerebras(), FailureKind::Connect, start);
        }
        assert_eq!(
            t.usability_at(&cerebras(), start + Duration::from_secs(9)),
            Usability::Skip
        );
        assert_eq!(
            t.usability_at(&cerebras(), start + Duration::from_secs(10)),
            Usability::ProbeRequired
        );
    }

    #[test]
    fn a_failed_probe_keeps_the_degradation_and_doubles_the_backoff() {
        let t = table();
        let start = Instant::now();
        for _ in 0..3 {
            t.record_failure_at(&cerebras(), FailureKind::Connect, start);
        }

        let at = start + Duration::from_secs(10);
        t.record_probe_at(&cerebras(), &Err(AiboError::Offline), at);
        assert!(t.is_degraded(&cerebras()));
        // 10 s doubled to 20 s.
        assert_eq!(
            t.usability_at(&cerebras(), at + Duration::from_secs(19)),
            Usability::Skip
        );
        assert_eq!(
            t.usability_at(&cerebras(), at + Duration::from_secs(20)),
            Usability::ProbeRequired
        );
    }

    #[test]
    fn the_backoff_is_capped() {
        let t = table();
        let mut at = Instant::now();
        for _ in 0..3 {
            t.record_failure_at(&cerebras(), FailureKind::Connect, at);
        }
        for _ in 0..10 {
            at += Duration::from_secs(600);
            t.record_probe_at(&cerebras(), &Err(AiboError::Offline), at);
        }
        // Capped at 80 s, so it is probe-required again 80 s later, not 3 hours.
        assert_eq!(
            t.usability_at(&cerebras(), at + Duration::from_secs(80)),
            Usability::ProbeRequired
        );
    }

    #[test]
    fn only_a_successful_probe_clears_the_degradation() {
        let t = table();
        let start = Instant::now();
        for _ in 0..3 {
            t.record_failure_at(&cerebras(), FailureKind::Connect, start);
        }
        let at = start + Duration::from_secs(10);

        // A probe that answers "degraded" is still an answer, and it must not
        // clear anything.
        t.record_probe_at(
            &cerebras(),
            &Ok(Health::Degraded {
                reason: "slow".to_owned(),
                consecutive_failures: 1,
            }),
            at,
        );
        assert!(t.is_degraded(&cerebras()));

        t.record_probe_at(
            &cerebras(),
            &Ok(Health::Ok {
                latency: Duration::from_millis(30),
            }),
            at + Duration::from_secs(60),
        );
        assert!(!t.is_degraded(&cerebras()));
        assert_eq!(
            t.usability_at(&cerebras(), at + Duration::from_secs(60)),
            Usability::Healthy
        );
    }

    #[test]
    fn degradation_is_per_provider() {
        let t = table();
        let now = Instant::now();
        for _ in 0..5 {
            t.record_failure_at(&ProviderId::CEREBRAS, FailureKind::Connect, now);
        }
        assert!(t.is_degraded(&ProviderId::CEREBRAS));
        assert!(!t.is_degraded(&ProviderId::OLLAMA));
        assert_eq!(
            t.usability_at(&ProviderId::OLLAMA, now),
            Usability::Healthy,
            "§13: a failed connection to Cerebras says nothing about Ollama"
        );
    }

    #[test]
    fn a_rate_limit_is_not_an_outage() {
        assert_eq!(
            FailureKind::classify(&AiboError::RateLimited {
                provider: ProviderId::GROQ,
                retry_after: Some(Duration::from_secs(60)),
            }),
            None
        );
        assert_eq!(
            FailureKind::classify(&AiboError::Offline),
            Some(FailureKind::Connect)
        );
        assert_eq!(
            FailureKind::classify(&AiboError::ProviderUnavailable {
                provider: ProviderId::GROQ,
                status: 503,
            }),
            Some(FailureKind::Server)
        );
        assert_eq!(
            FailureKind::classify(&AiboError::ProviderUnavailable {
                provider: ProviderId::GROQ,
                status: 400,
            }),
            None,
            "§4: a 400 is a bug in aibo, not evidence about the network"
        );
    }

    #[test]
    fn wake_shortens_the_backoff_but_does_not_clear_the_flag() {
        let t = table();
        let start = Instant::now();
        for _ in 0..3 {
            t.record_failure_at(&cerebras(), FailureKind::Connect, start);
        }
        let woke = start + Duration::from_secs(1);
        t.probe_all_now_at(woke);
        assert!(
            t.is_degraded(&cerebras()),
            "a wake notification is not evidence the network came back"
        );
        assert_eq!(t.usability_at(&cerebras(), woke), Usability::ProbeRequired);
    }

    #[test]
    fn health_transitions_are_reported_once() {
        let t = table();
        let now = Instant::now();
        assert!(
            t.record_failure_at(&cerebras(), FailureKind::Connect, now)
                .is_some()
        );
        // Same kind, higher count → the `Degraded` payload changes, so it is a
        // real transition and is reported.
        assert!(
            t.record_failure_at(&cerebras(), FailureKind::Connect, now)
                .is_some()
        );
        assert!(
            t.record_success_at(&cerebras(), Duration::ZERO, now)
                .is_some()
        );
        assert!(
            t.record_success_at(&cerebras(), Duration::ZERO, now)
                .is_none(),
            "an unchanged health must not produce a second event"
        );
    }
}
