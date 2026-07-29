//! §13 cancellation and partial results.
//!
//! > *"Every request carries a `CancellationToken`. `esc` cancels in-flight
//! > work and closes the panel; a new submission cancels the previous one."*
//!
//! > *"**A partial stream is never auto-inserted.** … Silent insertion of half
//! > a rewrite over a user's selection is the worst failure this product can
//! > have."*

mod common;

use std::sync::Arc;
use std::time::Duration;

use aibo_core::AiboError;
use aibo_core::roles::RoleBindings;
use aibo_core::types::{ModelBinding, ProviderId, Role, RoleChain, StopReason, StreamEvent};
use aibo_provider::ProviderRegistry;
use aibo_session::{
    Engine, EngineConfig, EventSink, Outcome, PartialReason, SessionEvent, Submission,
};
use common::{Mock, Script};
use uuid::Uuid;

fn engine_with(mocks: &[&Mock], fallback: bool) -> Arc<Engine> {
    let mut registry = ProviderRegistry::new();
    for mock in mocks {
        registry.insert(mock.id(), mock.provider());
    }
    let bindings = RoleBindings::from_chains([RoleChain {
        role: Role::Smart,
        entries: mocks
            .iter()
            .map(|m| ModelBinding {
                provider: m.id(),
                model: "model-x".to_owned(),
            })
            .collect(),
        fallback_enabled: fallback,
        allow_crossing_trust_boundary: true,
    }])
    .unwrap();

    Arc::new(Engine::new(
        registry,
        EngineConfig {
            bindings,
            ..EngineConfig::default()
        },
    ))
}

fn ask(text: &str) -> Submission {
    Submission::new(Uuid::now_v7(), text)
}

#[tokio::test]
async fn esc_cancels_and_the_partial_is_not_insertable() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::Hang(vec![StreamEvent::Text(
        "half a rewrite".to_owned(),
    )]));

    let engine = engine_with(&[&mock], false);
    let session = Uuid::now_v7();
    let submission = Submission::new(session, "rewrite this");

    let runner = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run(submission, &EventSink::null()).await })
    };

    // Let the first chunk land, then press `esc`.
    tokio::time::sleep(Duration::from_millis(30)).await;
    engine.cancel(session);

    let outcome = tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .expect("cancellation must be prompt")
        .unwrap();

    assert!(matches!(
        outcome,
        Outcome::Partial {
            reason: PartialReason::Cancelled,
            ..
        }
    ));
    assert_eq!(outcome.displayable_text(), Some("half a rewrite"));
    assert_eq!(
        outcome.insertable_text(),
        None,
        "§13: a partial stream is NEVER auto-inserted"
    );
}

#[tokio::test]
async fn a_new_submission_cancels_the_previous_one() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::Hang(vec![StreamEvent::Text("first".to_owned())]));
    mock.push(Script::ok("second"));

    let engine = engine_with(&[&mock], false);

    let first = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run(ask("first"), &EventSink::null()).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;

    // §13: "one panel, one session" — the second submission supersedes the
    // first, with no explicit cancel call from the caller.
    let second = engine.run(ask("second"), &EventSink::null()).await;

    let first = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("the superseded request must not hang")
        .unwrap();

    assert!(matches!(
        first,
        Outcome::Partial {
            reason: PartialReason::Cancelled,
            ..
        }
    ));
    assert_eq!(second.insertable_text(), Some("second"));
}

#[tokio::test]
async fn a_cancel_for_a_superseded_session_does_not_touch_the_new_one() {
    let mock = Mock::new(ProviderId::OPENAI);
    let engine = engine_with(&[&mock], false);

    let stale = Uuid::now_v7();
    // Nothing is in flight for `stale`; the `esc` keystroke lost the race.
    engine.cancel(stale);

    let outcome = engine.run(ask("still works"), &EventSink::null()).await;
    assert_eq!(outcome.insertable_text(), Some("ok"));
}

#[tokio::test]
async fn a_mid_stream_failure_yields_a_partial_and_never_retries_elsewhere() {
    let primary = Mock::new(ProviderId::ANTHROPIC);
    primary.push(Script::breaks_after(
        "half a rewr",
        AiboError::ProviderUnavailable {
            provider: ProviderId::ANTHROPIC,
            status: 503,
            detail: None,
        },
    ));
    let secondary = Mock::new(ProviderId::OPENAI);

    let engine = engine_with(&[&primary, &secondary], true);
    let (sink, mut rx) = EventSink::channel();
    let outcome = engine.run(ask("rewrite this"), &sink).await;

    assert!(matches!(
        outcome,
        Outcome::Partial {
            reason: PartialReason::StreamFailed,
            ..
        }
    ));
    assert_eq!(outcome.displayable_text(), Some("half a rewr"));
    assert_eq!(outcome.insertable_text(), None);
    assert_eq!(
        secondary.chat_calls(),
        0,
        "§13: never retry after a partial stream — it risks double-billing and duplicated output"
    );

    // §13 still wants the failure surfaced so the panel shows retry and copy.
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(events.iter().any(|e| matches!(e, SessionEvent::Failed(_))));
}

#[tokio::test]
async fn a_cancelled_stop_reason_is_also_a_partial() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::Events(vec![
        Ok(StreamEvent::Text("part".to_owned())),
        Ok(StreamEvent::Done(StopReason::Cancelled)),
    ]));

    let engine = engine_with(&[&mock], false);
    let outcome = engine.run(ask("go"), &EventSink::null()).await;

    assert!(matches!(
        outcome,
        Outcome::Partial {
            reason: PartialReason::Cancelled,
            ..
        }
    ));
    assert_eq!(outcome.insertable_text(), None);
}

#[tokio::test]
async fn a_stream_that_simply_stops_is_not_treated_as_complete() {
    // No `Done` event. Accepting this as a completion would auto-insert a
    // truncated rewrite, which is the exact failure §13 forbids.
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::Events(vec![Ok(StreamEvent::Text(
        "truncated".to_owned(),
    ))]));

    let engine = engine_with(&[&mock], false);
    let outcome = engine.run(ask("go"), &EventSink::null()).await;
    assert_eq!(outcome.insertable_text(), None);
}

#[tokio::test]
async fn cancel_all_stops_everything() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::Hang(vec![StreamEvent::Text("x".to_owned())]));
    let engine = engine_with(&[&mock], false);

    let running = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run(ask("go"), &EventSink::null()).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    engine.cancel_all();

    let outcome = tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("shutdown must not hang")
        .unwrap();
    assert!(matches!(outcome, Outcome::Partial { .. }));
}
