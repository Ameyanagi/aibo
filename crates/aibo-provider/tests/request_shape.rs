//! Golden files for the **outgoing** request bodies.
//!
//! The decoder goldens in `golden.rs` catch a provider changing what it sends.
//! These catch aibo changing what it sends — which is the failure §10 actually
//! warns about, because the symptom is a 400 with an unhelpful body and §4
//! forbids falling back on a 4xx, so it surfaces as a hard error for every user
//! of that provider at once.
//!
//! Re-record with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p aibo-provider --test request_shape
//! ```
//!
//! The input is one deserialised [`ChatRequest`] shared by every case, so a
//! diff between two goldens is purely the provider difference.

use std::path::PathBuf;

use aibo_core::types::ChatRequest;
use aibo_provider::anthropic::build_messages_body;
use aibo_provider::openai_compat::{
    build_chat_completions_body, build_responses_body, cerebras, openai,
};
use serde_json::Value;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn request() -> ChatRequest {
    let raw =
        std::fs::read_to_string(fixtures().join("request_ask.json")).expect("request fixture");
    serde_json::from_str(&raw).expect("request fixture deserialises")
}

fn golden(name: &str, body: &Value) {
    let path = fixtures().join(format!("{name}.request.json"));
    let actual = serde_json::to_string_pretty(body).expect("serialise");

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, format!("{actual}\n")).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden {name}.request.json ({e})"));
    assert_eq!(
        actual.trim(),
        expected.trim(),
        "{name}: request body drifted"
    );
}

#[test]
fn cerebras_chat_completions_request() {
    golden(
        "cerebras",
        &build_chat_completions_body(&request(), &cerebras::quirks()),
    );
}

/// The single most common cross-provider break: newer OpenAI models reject
/// `max_tokens` and require `max_completion_tokens`.
#[test]
fn openai_chat_completions_request_uses_max_completion_tokens() {
    let body = build_chat_completions_body(&request(), &openai::chat_completions_quirks());
    assert!(body.get("max_completion_tokens").is_some());
    assert!(body.get("max_tokens").is_none());
    golden("openai_chat_completions", &body);
}

/// The Responses format hoists the system prompt into `instructions`; sending
/// it as a message is a 400.
#[test]
fn openai_responses_request_hoists_the_system_prompt() {
    let body = build_responses_body(&request(), &openai::quirks());
    assert!(body.get("instructions").is_some());
    let roles: Vec<&str> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m.get("role").and_then(Value::as_str))
        .collect();
    assert!(!roles.contains(&"system"), "{roles:?}");
    golden("openai_responses", &body);
}

#[test]
fn anthropic_messages_request_uses_a_top_level_system_field() {
    let body = build_messages_body(&request());
    assert!(body.get("system").is_some());
    assert!(body["tools"][0].get("input_schema").is_some());
    golden("anthropic", &body);
}

/// §5's security control, checked on the wire rather than in prose: captured
/// content is fenced and labelled untrusted in **every** format, and never
/// interpolated inline with the user's instruction.
#[test]
fn captured_content_is_fenced_in_every_wire_format() {
    let req = request();
    let bodies = [
        build_chat_completions_body(&req, &cerebras::quirks()),
        build_responses_body(&req, &openai::quirks()),
        build_messages_body(&req),
    ];
    for body in &bodies {
        let rendered = body.to_string();
        assert!(
            rendered.contains("<<<untrusted"),
            "capture was not fenced: {rendered}"
        );
        assert!(
            rendered.contains("origin=selection"),
            "capture was not labelled: {rendered}"
        );
    }
}

/// aibo keeps its own encrypted history (§12), so there is no reason to leave a
/// server-side copy.
#[test]
fn the_responses_format_does_not_ask_the_provider_to_store_the_turn() {
    let body = build_responses_body(&request(), &openai::quirks());
    assert_eq!(body["store"], Value::Bool(false));
}

/// §14: the output cap sent on the wire is the *lower* of the sampling
/// parameter and the enforced budget. Sending the larger one would let a
/// misconfigured surface outspend its own ceiling.
#[test]
fn the_output_cap_never_exceeds_the_budget() {
    let mut req = request();
    req.params.max_tokens = 100_000;
    req.budget.max_output_tokens = 512;

    let chat = build_chat_completions_body(&req, &cerebras::quirks());
    assert_eq!(chat["max_tokens"], 512);
    let responses = build_responses_body(&req, &openai::quirks());
    assert_eq!(responses["max_output_tokens"], 512);
    let anthropic = build_messages_body(&req);
    assert_eq!(anthropic["max_tokens"], 512);
}
