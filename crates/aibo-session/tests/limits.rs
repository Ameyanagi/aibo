//! §13's hard refusals: the character cap, the wall-clock ceiling, and the
//! pre-dispatch binding refusal.
//!
//! All three share a property that makes them worth their own file: they end a
//! request *without* a provider ever producing an answer, so the only thing the
//! user gets is the error. Each of these tests exists because that error was
//! wrong — in its unit, in its absence, or in its type.

mod common;

use std::time::Duration;

use aibo_core::error::{AiboError, TimeoutPhase};
use aibo_core::roles::RoleBindings;
use aibo_core::types::{ModelBinding, ProviderId, Role, RoleChain, StreamEvent};
use aibo_provider::ProviderRegistry;
use aibo_session::{
    Engine, EngineConfig, EventSink, Outcome, PartialReason, SessionEvent, Submission,
};
use common::{Mock, Script};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn chain(role: Role, entries: Vec<ModelBinding>) -> RoleChain {
    RoleChain {
        role,
        entries,
        fallback_enabled: true,
        allow_crossing_trust_boundary: true,
    }
}

fn binding(provider: &ProviderId, model: &str) -> ModelBinding {
    ModelBinding {
        provider: provider.clone(),
        model: model.to_owned(),
    }
}

/// Every role bound to the same ordered chain, so routing cannot decide the
/// outcome of a test that is not about routing.
fn engine_with(entries: Vec<ModelBinding>, providers: &[&Mock], config: EngineConfig) -> Engine {
    let mut registry = ProviderRegistry::new();
    for mock in providers {
        registry.insert(mock.id(), mock.provider());
    }
    let bindings = RoleBindings::from_chains(
        [
            Role::Fast,
            Role::Smart,
            Role::Cheap,
            Role::Vision,
            Role::Agent,
        ]
        .into_iter()
        .map(|role| chain(role, entries.clone())),
    )
    .unwrap();
    Engine::new(registry, EngineConfig { bindings, ..config })
}

fn drain(rx: &mut tokio::sync::mpsc::Receiver<SessionEvent>) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

/// `n` kana: three bytes each, one character each.
fn japanese(n: usize) -> String {
    "あ".repeat(n)
}

// ---------------------------------------------------------------------------
// §13 large selections — the cap is in characters, not bytes
// ---------------------------------------------------------------------------

/// §13: *"refuse above 200k characters — counted as characters, not
/// `str::len()` bytes. A Japanese selection averages ~3 bytes per character, so
/// a byte-based cap refuses CJK users at ~66k characters."*
///
/// 3 000 kana is 9 000 bytes. Against a 4 000-character cap the byte count
/// refused it and the character count does not.
#[tokio::test]
async fn a_cjk_selection_within_the_character_cap_is_not_refused() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("要約"));
    let engine = engine_with(
        vec![binding(&ProviderId::OPENAI, "model-x")],
        &[&mock],
        EngineConfig {
            max_payload_chars: 4_000,
            ..EngineConfig::default()
        },
    );

    let mut submission = Submission::new(Uuid::now_v7(), "要約して");
    submission.capture.selection = Some(japanese(3_000));
    assert_eq!(
        submission.capture.selection.as_deref().unwrap().len(),
        9_000,
        "the premise: this selection is over the cap in bytes and under it in characters"
    );

    let (sink, _rx) = EventSink::channel();
    let outcome = engine.run(submission, &sink).await;

    assert!(
        matches!(outcome, Outcome::Completed(_)),
        "a 3 000-character Japanese selection under a 4 000-character cap must \
         reach the provider; got {outcome:?}"
    );
    assert_eq!(mock.chat_calls(), 1);
}

/// The same input in ASCII, so the test above cannot pass merely because the
/// cap stopped being enforced.
#[tokio::test]
async fn the_character_cap_still_refuses_and_reports_characters() {
    let mock = Mock::new(ProviderId::OPENAI);
    let engine = engine_with(
        vec![binding(&ProviderId::OPENAI, "model-x")],
        &[&mock],
        EngineConfig {
            max_payload_chars: 4_000,
            ..EngineConfig::default()
        },
    );

    // 4 001 characters across the instruction and the selection: one over.
    let mut submission = Submission::new(Uuid::now_v7(), japanese(1));
    submission.capture.selection = Some(japanese(4_000));

    let (sink, _rx) = EventSink::channel();
    let outcome = engine.run(submission, &sink).await;

    let Some(error) = outcome.error() else {
        panic!("expected a refusal, got {outcome:?}");
    };
    match error.as_ref() {
        AiboError::ContextTooLarge { limit, actual } => {
            assert_eq!(*limit, 4_000);
            // The unit of `actual` must be the unit of `limit`. Before the fix
            // this read 12 003 — the byte count, reported as if it were
            // characters, in an error whose whole job is to tell the user how
            // much to trim.
            assert_eq!(
                *actual, 4_001,
                "`actual` must be characters, the same unit as `limit`"
            );
        }
        other => panic!("expected ContextTooLarge, got {other:?}"),
    }
    assert_eq!(mock.chat_calls(), 0, "refused before any request is built");
}

/// The refusal must count the field prefix and suffix in characters too — they
/// were the other half of the byte-counted sum.
#[tokio::test]
async fn the_cap_counts_the_field_context_in_characters() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("done"));
    let engine = engine_with(
        vec![binding(&ProviderId::OPENAI, "model-x")],
        &[&mock],
        EngineConfig {
            max_payload_chars: 1_000,
            ..EngineConfig::default()
        },
    );

    // 900 characters of field context: 2 700 bytes, well over the cap in bytes.
    let mut submission = Submission::new(Uuid::now_v7(), "続けて");
    submission.capture.field = Some(aibo_core::types::FieldContext {
        prefix: japanese(600),
        suffix: japanese(300),
        caret: None,
        label: None,
        is_secure: false,
        ime_active: false,
        truncated: false,
        caret_bounds: None,
    });

    let (sink, _rx) = EventSink::channel();
    let outcome = engine.run(submission, &sink).await;

    assert!(
        matches!(outcome, Outcome::Completed(_)),
        "903 characters under a 1 000-character cap must not be refused; got {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// §13 wall-clock ceiling — `EngineConfig::request_deadline` is enforced
// ---------------------------------------------------------------------------

/// The knob existed, was documented as *"wall-clock ceiling for the whole
/// request"*, was settable from `request_deadline_secs`, and nothing read it: a
/// provider that accepted the connection and then went silent parked the panel
/// forever.
#[tokio::test(start_paused = true)]
async fn a_stream_that_never_ends_is_cut_at_the_request_deadline() {
    let mock = Mock::new(ProviderId::OPENAI);
    // A stream that yields nothing and never terminates.
    mock.push(Script::Hang(Vec::new()));
    let engine = engine_with(
        vec![binding(&ProviderId::OPENAI, "model-x")],
        &[&mock],
        EngineConfig {
            request_deadline: Duration::from_secs(30),
            ..EngineConfig::default()
        },
    );

    let (sink, mut rx) = EventSink::channel();
    let outcome = engine
        .run(Submission::new(Uuid::now_v7(), "hello"), &sink)
        .await;

    let Some(error) = outcome.error() else {
        panic!("expected the deadline to end the request, got {outcome:?}");
    };
    assert!(
        matches!(
            error.as_ref(),
            AiboError::Timeout {
                phase: TimeoutPhase::Stream
            }
        ),
        "§13 lists Timeout {{ phase: Stream }} for this; got {error:?}"
    );
    assert!(
        drain(&mut rx)
            .iter()
            .any(|e| matches!(e, SessionEvent::Failed(_))),
        "the panel must be told, not left spinning"
    );
}

/// Text that arrived before the ceiling expired is kept. §13: *"a partial
/// stream is never auto-inserted … the partial text stays in the panel marked
/// truncated"* — so the ceiling may not be implemented by dropping the future,
/// which would take the text with it.
#[tokio::test(start_paused = true)]
async fn the_deadline_keeps_the_text_that_had_already_arrived() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::Hang(vec![StreamEvent::Text(
        "half a rewrite".to_owned(),
    )]));
    let engine = engine_with(
        vec![binding(&ProviderId::OPENAI, "model-x")],
        &[&mock],
        EngineConfig {
            request_deadline: Duration::from_secs(30),
            ..EngineConfig::default()
        },
    );

    let (sink, _rx) = EventSink::channel();
    let outcome = engine
        .run(Submission::new(Uuid::now_v7(), "rewrite this"), &sink)
        .await;

    match &outcome {
        Outcome::Partial { text, reason, .. } => {
            assert_eq!(text, "half a rewrite");
            assert_eq!(*reason, PartialReason::StreamFailed);
        }
        other => panic!("expected a partial result, got {other:?}"),
    }
    assert!(
        outcome.insertable_text().is_none(),
        "§13: a partial stream is never auto-inserted"
    );
}

/// A ceiling that restarts on the next chain entry is not a ceiling. The
/// deadline error is deliberately `Stream`, which
/// `AiboError::is_fallback_eligible` refuses, so the request stops.
#[tokio::test(start_paused = true)]
async fn the_deadline_bounds_the_request_not_each_chain_entry() {
    let primary = Mock::new(ProviderId::OPENAI);
    primary.push(Script::Hang(Vec::new()));
    let secondary = Mock::new(ProviderId::GROQ);
    secondary.push(Script::ok("the substitute answered"));

    let engine = engine_with(
        vec![
            binding(&ProviderId::OPENAI, "model-x"),
            binding(&ProviderId::GROQ, "model-y"),
        ],
        &[&primary, &secondary],
        EngineConfig {
            request_deadline: Duration::from_secs(30),
            ..EngineConfig::default()
        },
    );

    let (sink, _rx) = EventSink::channel();
    let outcome = engine
        .run(Submission::new(Uuid::now_v7(), "hello"), &sink)
        .await;

    assert!(
        matches!(
            outcome.error().map(|e| e.as_ref()),
            Some(AiboError::Timeout {
                phase: TimeoutPhase::Stream
            })
        ),
        "got {outcome:?}"
    );
    assert_eq!(
        secondary.chat_calls(),
        0,
        "falling through after the ceiling expired would let a two-entry chain \
         take twice the configured maximum"
    );
}

/// The ceiling covers the initial POST, not only the stream: a provider that
/// never returns a stream at all must not park the panel either.
#[tokio::test(start_paused = true)]
async fn the_deadline_covers_a_chat_call_that_never_returns() {
    struct NeverAnswers(ProviderId);

    #[async_trait::async_trait]
    impl aibo_core::traits::Provider for NeverAnswers {
        fn id(&self) -> ProviderId {
            self.0.clone()
        }
        fn capabilities(&self) -> aibo_core::types::Capabilities {
            aibo_core::types::Capabilities {
                max_context: 128_000,
                max_output: Some(4_096),
                ..aibo_core::types::Capabilities::default()
            }
        }
        async fn chat(
            &self,
            _req: aibo_core::types::ChatRequest,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> aibo_core::error::Result<
            aibo_core::types::BoxStream<'static, aibo_core::error::Result<StreamEvent>>,
        > {
            std::future::pending().await
        }
        async fn models(&self) -> aibo_core::error::Result<Vec<aibo_core::types::ModelInfo>> {
            Ok(Vec::new())
        }
        async fn health(&self) -> aibo_core::error::Result<aibo_core::types::Health> {
            Ok(aibo_core::types::Health::Ok {
                latency: Duration::from_millis(1),
            })
        }
    }

    let mut registry = ProviderRegistry::new();
    registry.insert(
        ProviderId::OPENAI,
        std::sync::Arc::new(NeverAnswers(ProviderId::OPENAI)),
    );
    let bindings = RoleBindings::from_chains(
        [
            Role::Fast,
            Role::Smart,
            Role::Cheap,
            Role::Vision,
            Role::Agent,
        ]
        .into_iter()
        .map(|role| chain(role, vec![binding(&ProviderId::OPENAI, "model-x")])),
    )
    .unwrap();
    let engine = Engine::new(
        registry,
        EngineConfig {
            bindings,
            request_deadline: Duration::from_secs(30),
            ..EngineConfig::default()
        },
    );

    let (sink, _rx) = EventSink::channel();
    let outcome = engine
        .run(Submission::new(Uuid::now_v7(), "hello"), &sink)
        .await;

    assert!(
        matches!(
            outcome.error().map(|e| e.as_ref()),
            Some(AiboError::Timeout {
                phase: TimeoutPhase::Stream
            })
        ),
        "got {outcome:?}"
    );
}

/// A ceiling that is not reached must not change anything.
#[tokio::test(start_paused = true)]
async fn a_request_inside_the_ceiling_is_untouched() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("answered in time"));
    let engine = engine_with(
        vec![binding(&ProviderId::OPENAI, "model-x")],
        &[&mock],
        EngineConfig {
            request_deadline: Duration::from_secs(30),
            ..EngineConfig::default()
        },
    );

    let (sink, _rx) = EventSink::channel();
    let outcome = engine
        .run(Submission::new(Uuid::now_v7(), "hello"), &sink)
        .await;

    assert_eq!(outcome.insertable_text(), Some("answered in time"));
}

// ---------------------------------------------------------------------------
// §3a / §10 — a pre-dispatch binding refusal keeps its type
// ---------------------------------------------------------------------------

/// An engine whose `Smart` chain is the given Codex binding.
///
/// §3a's TTFT floor disqualifies Codex from `Fast`, which `RoleBindings`
/// enforces, so the chain under test is bound to `Smart` and the submission
/// overrides the role rather than relying on routing.
fn codex_engine(mock: &Mock, model: &str) -> Engine {
    let mut registry = ProviderRegistry::new();
    registry.insert(mock.id(), mock.provider());
    let bindings =
        RoleBindings::from_chains([chain(Role::Smart, vec![binding(&ProviderId::CODEX, model)])])
            .unwrap();
    Engine::new(
        registry,
        EngineConfig {
            bindings,
            ..EngineConfig::default()
        },
    )
}

fn ask_smart(instruction: &str) -> Submission {
    let mut submission = Submission::new(Uuid::now_v7(), instruction);
    submission.role_override = Some(Role::Smart);
    submission
}

/// §3a's Codex allowlist rejects an API-style id before it reaches the wire.
/// §4 does not fall back on a 400, so this ends the request — but it must end
/// it with the *typed* error. §13 gives it one action button, and the panel can
/// only spend that on "switch to a model that works" if `model` and
/// `alternatives` survive the engine.
#[tokio::test]
async fn a_rejected_codex_binding_reaches_the_caller_typed() {
    let codex = Mock::new(ProviderId::CODEX);
    let engine = codex_engine(&codex, "gpt-5-codex");

    let (sink, mut rx) = EventSink::channel();
    let outcome = engine.run(ask_smart("explain this"), &sink).await;

    let Some(error) = outcome.error() else {
        panic!("expected a refusal, got {outcome:?}");
    };
    match error.as_ref() {
        AiboError::ModelRejected {
            provider,
            model,
            alternatives,
            ..
        } => {
            assert_eq!(*provider, ProviderId::CODEX);
            assert_eq!(model, "gpt-5-codex");
            assert!(
                !alternatives.is_empty(),
                "the alternatives are what the panel's one action button offers; \
                 flattening this error into a message throws them away"
            );
        }
        other => panic!("expected the typed ModelRejected, got {other:?}"),
    }

    assert_eq!(codex.chat_calls(), 0, "refused before dispatch");

    // The same typed value must reach the UI over the event channel, not just
    // as a return value.
    let failed = drain(&mut rx)
        .into_iter()
        .find_map(|e| match e {
            SessionEvent::Failed(error) => Some(error),
            _ => None,
        })
        .expect("the panel is told");
    assert!(matches!(failed.as_ref(), AiboError::ModelRejected { .. }));
}

/// The refusal is specific to the ids §3a measured as rejected: an ordinary
/// Codex binding still dispatches.
#[tokio::test]
async fn an_allowed_codex_binding_is_not_refused() {
    let codex = Mock::new(ProviderId::CODEX);
    codex.push(Script::ok("fine"));
    let engine = codex_engine(&codex, "gpt-5.6-sol");

    let (sink, _rx) = EventSink::channel();
    let outcome = engine.run(ask_smart("explain this"), &sink).await;

    assert_eq!(outcome.insertable_text(), Some("fine"));
}
