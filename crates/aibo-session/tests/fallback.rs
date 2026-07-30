//! §4 fallback across a role chain, and the cases that must **not** fall back.
//!
//! > *"Fallback within a chain triggers on: connect failure, 5xx, 429 with
//! > `retry_after` beyond the surface's latency budget, or a failed health
//! > probe. It does **not** trigger on a 400 — that's a bug in aibo, and it
//! > should surface as one rather than silently retrying elsewhere."*

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use aibo_core::AiboError;
use aibo_core::cost::PriceTable;
use aibo_core::error::TimeoutPhase;
use aibo_core::roles::RoleBindings;
use aibo_core::types::{Health, ModelBinding, ProviderId, Role, RoleChain};
use aibo_provider::ProviderRegistry;
use aibo_session::{
    Engine, EngineConfig, EventSink, Outcome, SessionEvent, SkipReason, Submission,
};
use common::{Mock, Script};
use uuid::Uuid;

fn binding(provider: ProviderId, model: &str) -> ModelBinding {
    ModelBinding {
        provider,
        model: model.to_owned(),
    }
}

/// A `Smart` chain over the given providers, with fallback on.
fn chain(providers: &[ProviderId], fallback: bool, cross: bool) -> RoleBindings {
    RoleBindings::from_chains([RoleChain {
        role: Role::Smart,
        entries: providers
            .iter()
            .map(|p| binding(p.clone(), "model-x"))
            .collect(),
        fallback_enabled: fallback,
        allow_crossing_trust_boundary: cross,
    }])
    .expect("valid chain")
}

fn engine(mocks: &[&Mock], bindings: RoleBindings) -> Engine {
    let mut registry = ProviderRegistry::new();
    for mock in mocks {
        registry.insert(mock.id(), mock.provider());
    }
    Engine::new(
        registry,
        EngineConfig {
            bindings,
            prices: PriceTable::empty(),
            tiers: BTreeMap::new(),
            ..EngineConfig::default()
        },
    )
}

/// An `Ask` submission, which §4 rule 8 routes to `Smart`.
fn ask() -> Submission {
    Submission::new(Uuid::now_v7(), "what changed in this release")
}

fn drain(rx: &mut tokio::sync::mpsc::Receiver<SessionEvent>) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

#[tokio::test]
async fn a_5xx_moves_to_the_next_chain_entry() {
    let primary = Mock::new(ProviderId::ANTHROPIC);
    primary.push(Script::Reject(AiboError::ProviderUnavailable {
        provider: ProviderId::ANTHROPIC,
        status: 503,
        detail: None,
    }));
    let secondary = Mock::new(ProviderId::OPENAI);
    secondary.push(Script::ok("the answer"));

    let engine = engine(
        &[&primary, &secondary],
        chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], true, false),
    );
    let (sink, mut rx) = EventSink::channel();
    let outcome = engine.run(ask(), &sink).await;

    assert_eq!(outcome.insertable_text(), Some("the answer"));
    assert_eq!(primary.chat_calls(), 1);
    assert_eq!(secondary.chat_calls(), 1);

    // §14: "must be visible when they fire".
    let dispatched = drain(&mut rx)
        .into_iter()
        .find_map(|e| match e {
            SessionEvent::Dispatched {
                provider,
                substituted_for,
                ..
            } => Some((provider, substituted_for)),
            _ => None,
        })
        .expect("a Dispatched event");
    assert_eq!(dispatched.0, ProviderId::OPENAI);
    assert_eq!(dispatched.1, Some(ProviderId::ANTHROPIC));
}

#[tokio::test]
async fn a_connect_failure_moves_to_the_next_chain_entry() {
    let primary = Mock::new(ProviderId::ANTHROPIC);
    primary.push(Script::Reject(AiboError::Offline));
    let secondary = Mock::new(ProviderId::OPENAI);
    secondary.push(Script::ok("second"));

    let engine = engine(
        &[&primary, &secondary],
        chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], true, false),
    );
    let outcome = engine.run(ask(), &EventSink::null()).await;
    assert_eq!(outcome.insertable_text(), Some("second"));
}

#[tokio::test]
async fn a_first_token_timeout_moves_to_the_next_chain_entry() {
    let primary = Mock::new(ProviderId::ANTHROPIC);
    primary.push(Script::Reject(AiboError::Timeout {
        phase: TimeoutPhase::FirstToken,
    }));
    let secondary = Mock::new(ProviderId::OPENAI);
    secondary.push(Script::ok("second"));

    let engine = engine(
        &[&primary, &secondary],
        chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], true, false),
    );
    assert_eq!(
        engine
            .run(ask(), &EventSink::null())
            .await
            .insertable_text(),
        Some("second")
    );
}

#[tokio::test]
async fn a_rate_limit_moves_to_the_next_chain_entry() {
    let primary = Mock::new(ProviderId::ANTHROPIC);
    primary.push(Script::Reject(AiboError::RateLimited {
        provider: ProviderId::ANTHROPIC,
        retry_after: Some(Duration::from_secs(120)),
    }));
    let secondary = Mock::new(ProviderId::OPENAI);
    secondary.push(Script::ok("second"));

    let engine = engine(
        &[&primary, &secondary],
        chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], true, false),
    );
    assert_eq!(
        engine
            .run(ask(), &EventSink::null())
            .await
            .insertable_text(),
        Some("second")
    );
    // §13: a 429 is not evidence about the network path, so it must not push
    // the provider towards "offline".
    assert!(!engine.health().is_degraded(&ProviderId::ANTHROPIC));
}

#[tokio::test]
async fn a_400_surfaces_as_a_bug_and_never_falls_back() {
    let primary = Mock::new(ProviderId::ANTHROPIC);
    primary.push(Script::Reject(AiboError::ProviderUnavailable {
        provider: ProviderId::ANTHROPIC,
        status: 400,
        detail: None,
    }));
    let secondary = Mock::new(ProviderId::OPENAI);

    let engine = engine(
        &[&primary, &secondary],
        chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], true, false),
    );
    let outcome = engine.run(ask(), &EventSink::null()).await;

    assert!(matches!(outcome, Outcome::Failed(_)));
    assert!(matches!(
        outcome.error().map(|e| e.as_ref()),
        Some(AiboError::ProviderUnavailable { status: 400, .. })
    ));
    assert_eq!(
        secondary.chat_calls(),
        0,
        "§4: a 400 is a bug in aibo; retrying elsewhere hides it and double-spends"
    );
}

#[tokio::test]
async fn an_auth_failure_never_falls_back() {
    let primary = Mock::new(ProviderId::ANTHROPIC);
    primary.push(Script::Reject(AiboError::Auth {
        provider: ProviderId::ANTHROPIC,
        kind: aibo_core::error::AuthKind::Expired,
    }));
    let secondary = Mock::new(ProviderId::OPENAI);

    let engine = engine(
        &[&primary, &secondary],
        chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], true, false),
    );
    let outcome = engine.run(ask(), &EventSink::null()).await;
    assert!(matches!(
        outcome.error().map(|e| e.as_ref()),
        Some(AiboError::Auth { .. })
    ));
    assert_eq!(secondary.chat_calls(), 0);
}

#[tokio::test]
async fn a_failed_health_probe_moves_to_the_next_chain_entry() {
    // Drive the primary to degraded first, then let its probe fail.
    let primary = Mock::new(ProviderId::ANTHROPIC);
    let secondary = Mock::new(ProviderId::OPENAI);
    let bindings = chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], true, false);
    let engine = engine(&[&primary, &secondary], bindings);

    // Three connect failures cross the default hysteresis threshold.
    for _ in 0..3 {
        primary.push(Script::Reject(AiboError::Offline));
        secondary.push(Script::ok("second"));
        engine.run(ask(), &EventSink::null()).await;
    }
    assert!(engine.health().is_degraded(&ProviderId::ANTHROPIC));

    // Now force the backoff to expire, and script a failing probe.
    engine.health().probe_all_now();
    primary.push_health(Err(AiboError::Offline));
    secondary.push(Script::ok("second"));

    let (sink, mut rx) = EventSink::channel();
    let before = primary.chat_calls();
    let outcome = engine.run(ask(), &sink).await;

    assert_eq!(outcome.insertable_text(), Some("second"));
    assert_eq!(
        primary.chat_calls(),
        before,
        "a failed probe must move to the next entry without spending a request"
    );
    assert!(drain(&mut rx).iter().any(|e| matches!(
        e,
        SessionEvent::Skipped {
            reason: SkipReason::FailedHealthProbe,
            ..
        }
    )));
}

#[tokio::test]
async fn a_successful_probe_puts_the_provider_back_on_the_chain() {
    let primary = Mock::new(ProviderId::ANTHROPIC);
    let secondary = Mock::new(ProviderId::OPENAI);
    let engine = engine(
        &[&primary, &secondary],
        chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], true, false),
    );

    for _ in 0..3 {
        primary.push(Script::Reject(AiboError::Offline));
        secondary.push(Script::ok("second"));
        engine.run(ask(), &EventSink::null()).await;
    }
    assert!(engine.health().is_degraded(&ProviderId::ANTHROPIC));

    engine.health().probe_all_now();
    primary.push_health(Ok(Health::Ok {
        latency: Duration::from_millis(12),
    }));
    primary.push(Script::ok("primary is back"));

    let outcome = engine.run(ask(), &EventSink::null()).await;
    assert_eq!(outcome.insertable_text(), Some("primary is back"));
    assert!(!engine.health().is_degraded(&ProviderId::ANTHROPIC));
}

#[tokio::test]
async fn fallback_is_off_unless_the_role_enables_it() {
    let primary = Mock::new(ProviderId::ANTHROPIC);
    primary.push(Script::Reject(AiboError::ProviderUnavailable {
        provider: ProviderId::ANTHROPIC,
        status: 503,
        detail: None,
    }));
    let secondary = Mock::new(ProviderId::OPENAI);

    let engine = engine(
        &[&primary, &secondary],
        chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], false, false),
    );
    let outcome = engine.run(ask(), &EventSink::null()).await;

    assert!(matches!(outcome, Outcome::Failed(_)));
    assert_eq!(
        secondary.chat_calls(),
        0,
        "§14: fallback is a spend and privacy decision, so it is opt-in per role"
    );
}

#[tokio::test]
async fn fallback_does_not_leave_the_trust_boundary_without_consent() {
    // Azure is the user's own tenant; OpenAI is not. §14 forbids the silent
    // move for exactly the audience the plan targets.
    let azure = Mock::new(ProviderId::AZURE_OPENAI);
    azure.push(Script::Reject(AiboError::ProviderUnavailable {
        provider: ProviderId::AZURE_OPENAI,
        status: 503,
        detail: None,
    }));
    let openai = Mock::new(ProviderId::OPENAI);

    let engine = engine(
        &[&azure, &openai],
        chain(&[ProviderId::AZURE_OPENAI, ProviderId::OPENAI], true, false),
    );
    let (sink, mut rx) = EventSink::channel();
    let outcome = engine.run(ask(), &sink).await;

    assert!(matches!(outcome, Outcome::Failed(_)));
    assert_eq!(openai.chat_calls(), 0);
    assert!(drain(&mut rx).iter().any(|e| matches!(
        e,
        SessionEvent::Skipped {
            reason: SkipReason::TrustBoundary,
            ..
        }
    )));
}

#[tokio::test]
async fn consent_re_enables_the_crossing() {
    let azure = Mock::new(ProviderId::AZURE_OPENAI);
    azure.push(Script::Reject(AiboError::ProviderUnavailable {
        provider: ProviderId::AZURE_OPENAI,
        status: 503,
        detail: None,
    }));
    let openai = Mock::new(ProviderId::OPENAI);
    openai.push(Script::ok("crossed with consent"));

    let engine = engine(
        &[&azure, &openai],
        chain(&[ProviderId::AZURE_OPENAI, ProviderId::OPENAI], true, true),
    );
    assert_eq!(
        engine
            .run(ask(), &EventSink::null())
            .await
            .insertable_text(),
        Some("crossed with consent")
    );
}

#[tokio::test]
async fn falling_inward_needs_no_consent() {
    // Public → private leaks nothing the user had not already accepted.
    let openai = Mock::new(ProviderId::OPENAI);
    openai.push(Script::Reject(AiboError::Offline));
    let ollama = Mock::new(ProviderId::OLLAMA);
    ollama.push(Script::ok("local"));

    let engine = engine(
        &[&openai, &ollama],
        chain(&[ProviderId::OPENAI, ProviderId::OLLAMA], true, false),
    );
    assert_eq!(
        engine
            .run(ask(), &EventSink::null())
            .await
            .insertable_text(),
        Some("local")
    );
}

#[tokio::test]
async fn an_unconfigured_chain_entry_is_skipped_not_fatal() {
    let secondary = Mock::new(ProviderId::OPENAI);
    secondary.push(Script::ok("second"));

    // The chain names Anthropic, but only OpenAI is in the registry.
    let engine = engine(
        &[&secondary],
        chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], true, false),
    );
    let (sink, mut rx) = EventSink::channel();
    assert_eq!(
        engine.run(ask(), &sink).await.insertable_text(),
        Some("second")
    );
    assert!(drain(&mut rx).iter().any(|e| matches!(
        e,
        SessionEvent::Skipped {
            reason: SkipReason::NotConfigured,
            ..
        }
    )));
}

#[tokio::test]
async fn an_empty_registry_is_the_one_blocking_error() {
    let engine = engine(&[], chain(&[ProviderId::ANTHROPIC], true, false));
    let outcome = engine.run(ask(), &EventSink::null()).await;
    assert!(matches!(
        outcome.error().map(|e| e.as_ref()),
        Some(AiboError::NoProviderConfigured)
    ));
}

#[tokio::test]
async fn every_entry_failing_reports_the_last_error() {
    let primary = Mock::always_failing(ProviderId::ANTHROPIC, || AiboError::Offline);
    let secondary = Mock::always_failing(ProviderId::OPENAI, || AiboError::ProviderUnavailable {
        provider: ProviderId::OPENAI,
        status: 502,
        detail: None,
    });

    let engine = engine(
        &[&primary, &secondary],
        chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], true, false),
    );
    let outcome = engine.run(ask(), &EventSink::null()).await;
    assert!(matches!(
        outcome.error().map(|e| e.as_ref()),
        Some(AiboError::ProviderUnavailable { status: 502, .. })
    ));
}

#[tokio::test]
async fn degradation_is_per_provider_not_global() {
    let flaky = Mock::always_failing(ProviderId::ANTHROPIC, || AiboError::Offline);
    let healthy = Mock::new(ProviderId::OPENAI);

    let engine = engine(
        &[&flaky, &healthy],
        chain(&[ProviderId::ANTHROPIC, ProviderId::OPENAI], true, false),
    );
    for _ in 0..4 {
        engine.run(ask(), &EventSink::null()).await;
    }

    assert!(engine.health().is_degraded(&ProviderId::ANTHROPIC));
    assert!(
        !engine.health().is_degraded(&ProviderId::OPENAI),
        "§13: a failed connection to one provider says nothing about another"
    );
}
