//! §14 reserve-at-dispatch, reconcile-after.
//!
//! > *"`Usage` never arrives on a cancelled or failed stream, so a meter that
//! > only counts completed responses systematically under-reports — and budget
//! > enforcement that waits for `Usage` cannot stop anything. Reserve an
//! > estimated cost at dispatch, reconcile when the real number lands, release
//! > on failure."*

mod common;

use aibo_core::AiboError;
use aibo_core::cost::{BudgetStatus, PriceTable};
use aibo_core::roles::RoleBindings;
use aibo_core::types::{ModelBinding, ProviderId, Role, RoleChain, StreamEvent};
use aibo_provider::ProviderRegistry;
use aibo_session::{Engine, EngineConfig, EventSink, Outcome, SessionEvent, Submission};
use common::{Mock, Script};
use uuid::Uuid;

/// $1 per million input tokens, $10 per million output.
const PRICES: &str = r#"
version = "test"

[[model]]
provider = "openai"
model = "model-x"
input = 1000000
output = 10000000
"#;

fn engine(
    mock: &Mock,
    prices: PriceTable,
    budget: Option<aibo_core::cost::MonthlyBudget>,
) -> Engine {
    let mut registry = ProviderRegistry::new();
    registry.insert(mock.id(), mock.provider());
    let bindings = RoleBindings::from_chains([RoleChain {
        role: Role::Smart,
        entries: vec![ModelBinding {
            provider: mock.id(),
            model: "model-x".to_owned(),
        }],
        fallback_enabled: false,
        allow_crossing_trust_boundary: false,
    }])
    .unwrap();

    Engine::new(
        registry,
        EngineConfig {
            bindings,
            prices,
            monthly_budget: budget,
            ..EngineConfig::default()
        },
    )
}

fn ask() -> Submission {
    Submission::new(Uuid::now_v7(), "what changed")
}

#[tokio::test]
async fn a_completed_request_settles_the_real_cost() {
    let mock = Mock::new(ProviderId::OPENAI);
    // 100 input @ $1/Mtok = 100 micros; 20 output @ $10/Mtok = 200 micros.
    mock.push(Script::ok("answer"));

    let engine = engine(&mock, PriceTable::from_toml_str(PRICES).unwrap(), None);
    let (sink, mut rx) = EventSink::channel();
    engine.run(ask(), &sink).await;

    let (settled, committed, status) = engine.spend_snapshot();
    assert_eq!(settled, 300);
    assert_eq!(
        committed, 300,
        "the reserve must be released once the real number lands"
    );
    assert_eq!(status, BudgetStatus::Ok);

    let mut costs = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let SessionEvent::Cost { cost_micros, .. } = event {
            costs.push(cost_micros);
        }
    }
    assert_eq!(costs, vec![Some(300)]);
}

#[tokio::test]
async fn a_failed_request_releases_its_reserve() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::Reject(AiboError::ProviderUnavailable {
        provider: ProviderId::OPENAI,
        status: 503,
    }));

    let engine = engine(&mock, PriceTable::from_toml_str(PRICES).unwrap(), None);
    engine.run(ask(), &EventSink::null()).await;

    let (settled, committed, _) = engine.spend_snapshot();
    assert_eq!(settled, 0);
    assert_eq!(
        committed, 0,
        "§14: release on failure — a leaked reserve would eventually block the cap"
    );
}

#[tokio::test]
async fn a_stream_with_no_usage_releases_rather_than_guessing() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::Events(vec![
        Ok(StreamEvent::Text("answer".to_owned())),
        Ok(StreamEvent::Done(aibo_core::types::StopReason::EndTurn)),
    ]));

    let engine = engine(&mock, PriceTable::from_toml_str(PRICES).unwrap(), None);
    let outcome = engine.run(ask(), &EventSink::null()).await;

    assert_eq!(outcome.insertable_text(), Some("answer"));
    let (settled, committed, _) = engine.spend_snapshot();
    assert_eq!(settled, 0, "no Usage means no bill aibo can prove");
    assert_eq!(committed, 0);
}

#[tokio::test]
async fn a_hard_stop_refuses_before_dispatch() {
    let mock = Mock::new(ProviderId::OPENAI);
    let engine = engine(
        &mock,
        PriceTable::from_toml_str(PRICES).unwrap(),
        Some(aibo_core::cost::MonthlyBudget {
            limit_micros: 10,
            warn_at_percent: 80,
            hard_stop: true,
        }),
    );

    let outcome = engine.run(ask(), &EventSink::null()).await;
    assert!(matches!(outcome, Outcome::Failed(_)));
    assert!(matches!(
        outcome.error().map(|e| e.as_ref()),
        Some(AiboError::BudgetExceeded { .. })
    ));
    assert_eq!(
        mock.chat_calls(),
        0,
        "§14: enforcement that waits for Usage cannot stop anything"
    );
}

#[tokio::test]
async fn a_budget_stop_is_not_a_reason_to_try_another_provider() {
    // The next provider costs money too. §13 gives BudgetExceeded the Inline
    // treatment, not SilentFallback.
    let first = Mock::new(ProviderId::OPENAI);
    let second = Mock::new(ProviderId::ANTHROPIC);

    let mut registry = ProviderRegistry::new();
    registry.insert(first.id(), first.provider());
    registry.insert(second.id(), second.provider());
    let bindings = RoleBindings::from_chains([RoleChain {
        role: Role::Smart,
        entries: vec![
            ModelBinding {
                provider: ProviderId::OPENAI,
                model: "model-x".to_owned(),
            },
            ModelBinding {
                provider: ProviderId::ANTHROPIC,
                model: "model-x".to_owned(),
            },
        ],
        fallback_enabled: true,
        allow_crossing_trust_boundary: true,
    }])
    .unwrap();

    let engine = Engine::new(
        registry,
        EngineConfig {
            bindings,
            prices: PriceTable::from_toml_str(PRICES).unwrap(),
            monthly_budget: Some(aibo_core::cost::MonthlyBudget {
                limit_micros: 10,
                warn_at_percent: 80,
                hard_stop: true,
            }),
            ..EngineConfig::default()
        },
    );

    let outcome = engine.run(ask(), &EventSink::null()).await;
    assert!(matches!(
        outcome.error().map(|e| e.as_ref()),
        Some(AiboError::BudgetExceeded { .. })
    ));
    assert_eq!(first.chat_calls(), 0);
    assert_eq!(second.chat_calls(), 0);
}

#[tokio::test]
async fn a_soft_budget_warns_without_refusing() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("answer"));

    let engine = engine(
        &mock,
        PriceTable::from_toml_str(PRICES).unwrap(),
        Some(aibo_core::cost::MonthlyBudget {
            limit_micros: 350,
            warn_at_percent: 80,
            hard_stop: false,
        }),
    );
    let outcome = engine.run(ask(), &EventSink::null()).await;

    assert_eq!(outcome.insertable_text(), Some("answer"));
    // 300 of 350 is 85%.
    assert_eq!(engine.spend_snapshot().2, BudgetStatus::Warning);
}

#[tokio::test]
async fn an_unpriced_model_still_runs() {
    // Refusing to answer because aibo does not know a price would be absurd,
    // and reporting $0.00 would be a lie. §14 counts it instead.
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("answer"));

    let engine = engine(&mock, PriceTable::empty(), None);
    let (sink, mut rx) = EventSink::channel();
    let outcome = engine.run(ask(), &sink).await;

    assert_eq!(outcome.insertable_text(), Some("answer"));
    let mut saw_unpriced = false;
    while let Ok(event) = rx.try_recv() {
        if let SessionEvent::Cost { cost_micros, .. } = event {
            assert_eq!(cost_micros, None);
            saw_unpriced = true;
        }
    }
    assert!(saw_unpriced);
    assert_eq!(engine.spend_snapshot().0, 0);
}

#[tokio::test]
async fn the_reserve_does_not_stack_across_a_fallback_attempt() {
    // One request id for the whole submission, so attempt 2's reserve replaces
    // attempt 1's rather than doubling the committed figure.
    let first = Mock::new(ProviderId::OPENAI);
    first.push(Script::Reject(AiboError::ProviderUnavailable {
        provider: ProviderId::OPENAI,
        status: 503,
    }));
    let second = Mock::new(ProviderId::ANTHROPIC);
    second.push(Script::ok("answer"));

    let mut registry = ProviderRegistry::new();
    registry.insert(first.id(), first.provider());
    registry.insert(second.id(), second.provider());
    let bindings = RoleBindings::from_chains([RoleChain {
        role: Role::Smart,
        entries: vec![
            ModelBinding {
                provider: ProviderId::OPENAI,
                model: "model-x".to_owned(),
            },
            ModelBinding {
                provider: ProviderId::ANTHROPIC,
                model: "model-x".to_owned(),
            },
        ],
        fallback_enabled: true,
        allow_crossing_trust_boundary: true,
    }])
    .unwrap();

    let engine = Engine::new(
        registry,
        EngineConfig {
            bindings,
            prices: PriceTable::from_toml_str(PRICES).unwrap(),
            ..EngineConfig::default()
        },
    );
    engine.run(ask(), &EventSink::null()).await;

    let (_, committed, _) = engine.spend_snapshot();
    // Anthropic is unpriced in the test table, so nothing settles; what matters
    // is that no reserve was left behind by the failed first attempt.
    assert_eq!(committed, 0);
}
