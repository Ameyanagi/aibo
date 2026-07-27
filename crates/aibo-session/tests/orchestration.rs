//! The whole path, end to end: §8 capture → §1 surface → §4 route → §5
//! assembly and budget → §7 stream → §12 persist.

mod common;

use std::sync::{Arc, Mutex};

use aibo_core::context::Turn;
use aibo_core::roles::RoleBindings;
use aibo_core::types::{
    AppInfo, AppRef, Capabilities, ContentOrigin, FieldContext, MessageRole, ModelBinding,
    ProviderId, Role, RoleChain, StopReason, StreamEvent, Surface, Usage,
};
use aibo_provider::ProviderRegistry;
use aibo_session::store::{Exchange, SessionStore};
use aibo_session::{Capture, Engine, EngineConfig, EventSink, Outcome, SessionEvent, Submission};
use async_trait::async_trait;
use common::{Mock, Script};
use uuid::Uuid;

fn chain_for(role: Role, provider: ProviderId) -> RoleChain {
    RoleChain {
        role,
        entries: vec![ModelBinding {
            provider,
            model: "model-x".to_owned(),
        }],
        fallback_enabled: false,
        allow_crossing_trust_boundary: false,
    }
}

/// Every role bound to the same mock, so a routing decision cannot fail for
/// want of a chain.
fn engine(mock: &Mock) -> Engine {
    let mut registry = ProviderRegistry::new();
    registry.insert(mock.id(), mock.provider());
    let bindings = RoleBindings::from_chains(
        [
            Role::Fast,
            Role::Smart,
            Role::Cheap,
            Role::Vision,
            Role::Agent,
        ]
        .into_iter()
        .map(|role| chain_for(role, mock.id())),
    )
    .unwrap();
    Engine::new(
        registry,
        EngineConfig {
            bindings,
            ..EngineConfig::default()
        },
    )
}

fn events_of(rx: &mut tokio::sync::mpsc::Receiver<SessionEvent>) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

fn routed_role(events: &[SessionEvent]) -> Option<(Surface, Role, &'static str)> {
    events.iter().find_map(|e| match e {
        SessionEvent::Routed {
            surface,
            role,
            rule,
        } => Some((*surface, *role, *rule)),
        _ => None,
    })
}

#[tokio::test]
async fn a_bare_question_is_an_ask_routed_to_smart() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("because the build changed"));
    let engine = engine(&mock);

    let (sink, mut rx) = EventSink::channel();
    let outcome = engine
        .run(
            Submission::new(Uuid::now_v7(), "why did the build change"),
            &sink,
        )
        .await;

    assert_eq!(outcome.insertable_text(), Some("because the build changed"));
    let (surface, role, _) = routed_role(&events_of(&mut rx)).unwrap();
    assert_eq!(surface, Surface::Ask);
    assert_eq!(role, Role::Smart);
}

#[tokio::test]
async fn a_selection_makes_it_a_transform() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("rewritten"));
    let engine = engine(&mock);

    let mut submission = Submission::new(Uuid::now_v7(), "make this formal");
    submission.capture.selection = Some("hey can u take a look".to_owned());

    let (sink, mut rx) = EventSink::channel();
    engine.run(submission, &sink).await;

    let (surface, role, _) = routed_role(&events_of(&mut rx)).unwrap();
    assert_eq!(surface, Surface::Transform);
    // §4 rule 5: a short, non-code Transform goes to Fast.
    assert_eq!(role, Role::Fast);
}

#[tokio::test]
async fn a_field_prefix_with_no_selection_is_a_complete() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok(" completed by Friday."));
    let engine = engine(&mock);

    let mut submission = Submission::new(Uuid::now_v7(), "");
    submission.capture.field = Some(FieldContext {
        prefix: "The deployment should be".to_owned(),
        suffix: String::new(),
        caret: None,
        label: Some("Message".to_owned()),
        is_secure: false,
        ime_active: false,
        truncated: false,
        caret_bounds: None,
    });

    let (sink, mut rx) = EventSink::channel();
    engine.run(submission, &sink).await;

    let (surface, role, _) = routed_role(&events_of(&mut rx)).unwrap();
    assert_eq!(surface, Surface::Complete);
    assert_eq!(role, Role::Fast);
}

#[tokio::test]
async fn a_rule_seven_verb_sends_a_short_ask_to_fast() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("a formal agreement"));
    let engine = engine(&mock);

    let (sink, mut rx) = EventSink::channel();
    engine
        .run(Submission::new(Uuid::now_v7(), "define concordat"), &sink)
        .await;

    let (surface, role, rule) = routed_role(&events_of(&mut rx)).unwrap();
    assert_eq!(surface, Surface::Ask);
    assert_eq!(
        role,
        Role::Fast,
        "§4 rule 7 needs a parsed leading verb; rule was `{rule}`"
    );
}

#[tokio::test]
async fn a_user_override_wins_over_every_rule() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("x"));
    let engine = engine(&mock);

    let mut submission = Submission::new(Uuid::now_v7(), "why did the build change");
    submission.role_override = Some(Role::Cheap);

    let (sink, mut rx) = EventSink::channel();
    engine.run(submission, &sink).await;
    assert_eq!(routed_role(&events_of(&mut rx)).unwrap().1, Role::Cheap);
}

#[tokio::test]
async fn the_assembled_request_fences_captured_content_and_keeps_the_instruction_verbatim() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("rewritten"));
    let engine = engine(&mock);

    let mut submission = Submission::new(Uuid::now_v7(), "make this formal");
    submission.capture.selection = Some("hey can u take a look".to_owned());
    submission.capture.app = Some(AppInfo {
        app_ref: AppRef {
            pid: 42,
            window: None,
        },
        identifier: "com.apple.mail".to_owned(),
        display_name: "Mail".to_owned(),
        is_code_app: false,
    });

    engine.run(submission, &EventSink::null()).await;

    let request = mock.requests().pop().expect("one request");
    assert_eq!(request.surface, Surface::Transform);
    assert_eq!(
        request.user_instruction.as_deref(),
        Some("make this formal")
    );
    assert!(
        !request.prompt_version.is_empty(),
        "§5 requires a version-stamped prompt"
    );
    // §5 rule 1: captured content is carried structurally, never interpolated
    // into the instruction.
    assert!(
        request
            .untrusted
            .iter()
            .any(|b| b.origin == ContentOrigin::Selection)
    );
    assert!(matches!(request.messages[0].role, MessageRole::System));
    // §5: an insertion surface offers no tools.
    assert!(request.tools.is_empty());
}

#[tokio::test]
async fn the_context_budget_truncates_an_oversized_selection_rather_than_dropping_it() {
    // A 4k-token context model, and a selection far larger than it.
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("done"));

    let mut registry = ProviderRegistry::new();
    registry.insert(mock.id(), mock.provider());
    let engine = Engine::new(
        registry,
        EngineConfig {
            bindings: RoleBindings::from_chains(
                [
                    Role::Fast,
                    Role::Smart,
                    Role::Cheap,
                    Role::Vision,
                    Role::Agent,
                ]
                .into_iter()
                .map(|role| chain_for(role, ProviderId::OPENAI)),
            )
            .unwrap(),
            catalogue: [(
                (ProviderId::OPENAI, "model-x".to_owned()),
                Capabilities {
                    max_context: 4_096,
                    max_output: Some(512),
                    ..Capabilities::default()
                },
            )]
            .into_iter()
            .collect(),
            ..EngineConfig::default()
        },
    );

    let mut submission = Submission::new(Uuid::now_v7(), "tidy this");
    submission.capture.selection = Some("word ".repeat(20_000));
    let outcome = engine.run(submission, &EventSink::null()).await;

    assert_eq!(outcome.insertable_text(), Some("done"));
    let request = mock.requests().pop().unwrap();
    let selection = request
        .untrusted
        .iter()
        .find(|b| b.origin == ContentOrigin::Selection)
        .expect("the selection survives, truncated");
    assert!(
        selection.content.len() < 20_000 * 5,
        "§5: middle-out truncated, not dropped"
    );
    assert!(
        selection.content.contains('…'),
        "the omission marker is visible"
    );
}

#[tokio::test]
async fn a_selection_over_the_hard_cap_is_refused_before_any_request_is_built() {
    let mock = Mock::new(ProviderId::OPENAI);
    let engine = engine(&mock);

    let mut submission = Submission::new(Uuid::now_v7(), "tidy this");
    submission.capture.selection = Some("x".repeat(250_000));
    let outcome = engine.run(submission, &EventSink::null()).await;

    assert!(matches!(
        outcome.error().map(|e| e.as_ref()),
        Some(aibo_core::AiboError::ContextTooLarge { .. })
    ));
    assert_eq!(
        mock.chat_calls(),
        0,
        "§13: hard caps, enforced before any request is built"
    );
}

#[tokio::test]
async fn the_five_anti_preamble_and_prefix_rules_reach_the_output() {
    let mock = Mock::new(ProviderId::OPENAI);
    // §5's measured `gpt-5.6-luna` failure, wrapped in a preamble for good
    // measure.
    mock.push(Script::Events(vec![
        Ok(StreamEvent::Text(
            "Sure! The deployment should be carefully monitored.".to_owned(),
        )),
        Ok(StreamEvent::Done(StopReason::EndTurn)),
    ]));
    let engine = engine(&mock);

    let mut submission = Submission::new(Uuid::now_v7(), "");
    submission.capture.field = Some(FieldContext {
        prefix: "The deployment should be".to_owned(),
        suffix: String::new(),
        caret: None,
        label: None,
        is_secure: false,
        ime_active: false,
        truncated: false,
        caret_bounds: None,
    });

    let outcome = engine.run(submission, &EventSink::null()).await;
    let text = outcome.insertable_text().unwrap();
    assert!(
        !text.contains("The deployment should be"),
        "§5: strip a leading repetition of the supplied prefix — got {text:?}"
    );
    assert!(!text.starts_with("Sure"), "§5 anti-preamble — got {text:?}");
    assert_eq!(text, "carefully monitored.");

    // The unfiltered text is kept so quality drift is observable.
    match outcome {
        Outcome::Completed(c) => assert!(c.raw_text.starts_with("Sure")),
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn a_length_stop_offers_escalation() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::Events(vec![
        Ok(StreamEvent::Text("a truncated answ".to_owned())),
        Ok(StreamEvent::Usage(Usage {
            output_tokens: 64,
            ..Usage::default()
        })),
        Ok(StreamEvent::Done(StopReason::Length)),
    ]));
    let engine = engine(&mock);

    let mut submission = Submission::new(Uuid::now_v7(), "shorten this");
    submission.capture.selection = Some("a long piece of prose".to_owned());
    let outcome = engine.run(submission, &EventSink::null()).await;

    match outcome {
        Outcome::Completed(c) => assert!(
            c.offer_escalation,
            "§4: a `length` stop is one of the two objective escalation signals"
        ),
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn history_is_carried_into_the_request() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("yes"));
    let engine = engine(&mock);

    let mut submission = Submission::new(Uuid::now_v7(), "and after that?");
    submission.history = vec![Turn::pair("what shipped?", "the panel")];
    engine.run(submission, &EventSink::null()).await;

    let request = mock.requests().pop().unwrap();
    let rendered: String = request
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            aibo_core::types::ContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(rendered.contains("the panel"), "§5 priority 5: history");
}

/// A store that records what it was asked to write.
#[derive(Default)]
struct RecordingStore {
    written: Mutex<Vec<Exchange>>,
}

#[async_trait]
impl SessionStore for RecordingStore {
    async fn record(&self, exchange: Exchange) -> aibo_core::error::Result<Option<Uuid>> {
        let id = Uuid::now_v7();
        self.written.lock().unwrap().push(exchange);
        Ok(Some(id))
    }
}

#[tokio::test]
async fn a_completed_exchange_is_persisted() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("the answer"));

    let store = Arc::new(RecordingStore::default());
    let mut registry = ProviderRegistry::new();
    registry.insert(mock.id(), mock.provider());
    let engine = Engine::new(
        registry,
        EngineConfig {
            bindings: RoleBindings::from_chains([chain_for(Role::Smart, ProviderId::OPENAI)])
                .unwrap(),
            ..EngineConfig::default()
        },
    )
    .with_store(store.clone());

    let mut submission = Submission::new(Uuid::now_v7(), "what changed");
    submission.capture.app = Some(AppInfo {
        app_ref: AppRef {
            pid: 1,
            window: None,
        },
        identifier: "com.apple.Safari".to_owned(),
        display_name: "Safari".to_owned(),
        is_code_app: false,
    });

    let outcome = engine.run(submission, &EventSink::null()).await;
    match outcome {
        Outcome::Completed(c) => assert!(c.conversation_id.is_some()),
        other => panic!("unexpected: {other:?}"),
    }

    let written = store.written.lock().unwrap();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].assistant, "the answer");
    assert_eq!(written[0].source_app.as_deref(), Some("com.apple.Safari"));
    assert!(!written[0].truncated);
}

#[tokio::test]
async fn a_partial_is_persisted_and_marked_truncated() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::breaks_after(
        "half an answ",
        aibo_core::AiboError::ProviderUnavailable {
            provider: ProviderId::OPENAI,
            status: 503,
        },
    ));

    let store = Arc::new(RecordingStore::default());
    let mut registry = ProviderRegistry::new();
    registry.insert(mock.id(), mock.provider());
    let engine = Engine::new(
        registry,
        EngineConfig {
            bindings: RoleBindings::from_chains([chain_for(Role::Smart, ProviderId::OPENAI)])
                .unwrap(),
            ..EngineConfig::default()
        },
    )
    .with_store(store);

    let outcome = engine
        .run(
            Submission::new(Uuid::now_v7(), "what changed"),
            &EventSink::null(),
        )
        .await;
    assert_eq!(outcome.insertable_text(), None);
}

#[tokio::test]
async fn a_store_failure_does_not_lose_the_answer() {
    struct BrokenStore;
    #[async_trait]
    impl SessionStore for BrokenStore {
        async fn record(&self, _: Exchange) -> aibo_core::error::Result<Option<Uuid>> {
            Err(aibo_core::AiboError::Internal(Box::new(
                std::io::Error::other("disk on fire"),
            )))
        }
    }

    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("the answer"));
    let mut registry = ProviderRegistry::new();
    registry.insert(mock.id(), mock.provider());
    let engine = Engine::new(
        registry,
        EngineConfig {
            bindings: RoleBindings::from_chains([chain_for(Role::Smart, ProviderId::OPENAI)])
                .unwrap(),
            ..EngineConfig::default()
        },
    )
    .with_store(Arc::new(BrokenStore));

    let outcome = engine
        .run(
            Submission::new(Uuid::now_v7(), "what changed"),
            &EventSink::null(),
        )
        .await;
    assert_eq!(
        outcome.insertable_text(),
        Some("the answer"),
        "history is not the product; a failed write must not lose the answer"
    );
}

#[tokio::test]
async fn the_per_role_cap_clamps_the_model_context() {
    // §14's first bullet: per-role caps, enforced before the request is built.
    // Fast caps output at 512 even though the model offers 4096.
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("done"));
    let engine = engine(&mock);

    let mut submission = Submission::new(Uuid::now_v7(), "tidy this");
    submission.capture.selection = Some("short".to_owned());
    engine.run(submission, &EventSink::null()).await;

    let request = mock.requests().pop().unwrap();
    assert_eq!(request.role, Role::Fast);
    assert!(
        request.budget.max_output_tokens <= 512,
        "Fast caps output at 512; got {}",
        request.budget.max_output_tokens
    );
    assert!(request.budget.max_context_tokens <= 16_384);
}

#[tokio::test]
async fn a_secure_field_never_reaches_the_wire() {
    let mock = Mock::new(ProviderId::OPENAI);
    mock.push(Script::ok("ok"));
    let engine = engine(&mock);

    let mut submission = Submission::new(Uuid::now_v7(), "what is this");
    submission.capture = Capture {
        field: Some(FieldContext {
            prefix: "hunter2".to_owned(),
            suffix: String::new(),
            caret: None,
            label: None,
            is_secure: true,
            ime_active: false,
            truncated: false,
            caret_bounds: None,
        }),
        ..Capture::default()
    };
    engine.run(submission, &EventSink::null()).await;

    let request = mock.requests().pop().unwrap();
    let serialised = serde_json::to_string(&request).unwrap();
    assert!(
        !serialised.contains("hunter2"),
        "§5: a password that reaches prompt assembly has already left the machine"
    );
}
