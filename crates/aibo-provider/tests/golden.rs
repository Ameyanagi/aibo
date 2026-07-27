//! Golden-file tests over recorded SSE traffic (§10, §3b).
//!
//! §10 budgets each provider at "1–3 days of quirk-hunting **plus a
//! golden-fixture set**", and §3b requires the Codex wire layer specifically to
//! be "isolated in one module with golden-file tests captured from real
//! traffic, so a shape change is a contained fix". This is that set.
//!
//! Each case is a `.sse` body under `tests/fixtures/` plus an `.expected.json`
//! listing the [`StreamEvent`]s it must decode to. Bodies are replayed through
//! the *production* parser in 17-byte chunks, so a decoder that only works when
//! frames arrive whole fails here rather than against a real socket.
//!
//! Re-record after capturing new traffic with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p aibo-provider --test golden
//! ```
//!
//! and **read the diff** — a changed expectation is either a provider changing
//! its wire format or a regression, and the two look identical until someone
//! looks.

use std::path::PathBuf;

use aibo_core::types::{ProviderId, StreamEvent};
use aibo_provider::anthropic::MessagesDecoder;
use aibo_provider::openai_compat::{
    ChatCompletionsDecoder, ResponsesDecoder, cerebras, groq, openai, xai,
};
use aibo_provider::sse::{SseDecoder, decode, events_from_bytes};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Replay `<name>.sse` through `decoder` and compare with `<name>.expected.json`.
async fn golden<D: SseDecoder + 'static>(name: &str, provider: ProviderId, decoder: D) {
    let dir = fixtures();
    let body = std::fs::read(dir.join(format!("{name}.sse")))
        .unwrap_or_else(|e| panic!("missing fixture {name}.sse: {e}"));

    let events: Vec<StreamEvent> = decode(
        events_from_bytes(body),
        decoder,
        provider,
        CancellationToken::new(),
    )
    .map(|r| r.unwrap_or_else(|e| panic!("{name}: stream error: {e}")))
    .collect()
    .await;

    // Every stream must end with exactly one terminal event: downstream (the
    // panel, the spend meter, the insert path) is written against that.
    assert!(
        matches!(events.last(), Some(StreamEvent::Done(_))),
        "{name}: stream did not end with Done: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Done(_)))
            .count(),
        1,
        "{name}: more than one terminal event"
    );

    let actual = serde_json::to_string_pretty(&events).expect("serialise");
    let expected_path = dir.join(format!("{name}.expected.json"));

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&expected_path, format!("{actual}\n")).expect("write golden");
        return;
    }

    let expected = std::fs::read_to_string(&expected_path).unwrap_or_else(|e| {
        panic!("missing golden {name}.expected.json ({e}); re-run with UPDATE_GOLDEN=1")
    });
    let expected = expected.replace("\r\n", "\n");
    assert_eq!(
        actual.trim(),
        expected.trim(),
        "{name}: decoded events differ from the golden file"
    );
}

// ---------------------------------------------------------------------------
// Chat Completions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cerebras_chat_completions() {
    golden(
        "cerebras_chat_completions",
        ProviderId::CEREBRAS,
        ChatCompletionsDecoder::new(cerebras::quirks()),
    )
    .await;
}

/// Groq reports usage under a vendor extension rather than the standard field.
/// Without the quirk the spend meter sees nothing at all (§14).
#[tokio::test]
async fn groq_reports_usage_under_its_vendor_extension() {
    golden(
        "groq_usage_extension",
        ProviderId::GROQ,
        ChatCompletionsDecoder::new(groq::quirks()),
    )
    .await;
}

/// Tool-call arguments stream as fragments and are only valid JSON once the
/// stream ends. Emitting them early would hand the permission gate a
/// half-parsed object (§11).
#[tokio::test]
async fn a_fragmented_tool_call_is_assembled_before_it_is_emitted() {
    golden(
        "openai_chat_completions_tool_call",
        ProviderId::OPENAI,
        ChatCompletionsDecoder::new(openai::chat_completions_quirks()),
    )
    .await;
}

/// Reasoning must land on its own channel: §7 renders it collapsed and never
/// inserts it into the user's document.
#[tokio::test]
async fn reasoning_never_arrives_as_insertable_text() {
    golden(
        "xai_reasoning",
        ProviderId::XAI,
        ChatCompletionsDecoder::new(xai::quirks()),
    )
    .await;
}

/// Several endpoints simply close the socket instead of sending `[DONE]`.
#[tokio::test]
async fn a_stream_that_closes_without_a_terminator_still_completes() {
    golden(
        "ollama_no_terminator",
        ProviderId::OLLAMA,
        ChatCompletionsDecoder::new(aibo_provider::ollama::quirks()),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Responses (OpenAI native / Codex, §3a)
// ---------------------------------------------------------------------------

/// SPIKE: S6 — recorded against the *public* Responses API. The Codex
/// subscription endpoint must be captured with mitmproxy and this fixture
/// re-recorded before the direct path can be called verified.
#[tokio::test]
async fn codex_responses_stream() {
    golden(
        "codex_responses",
        ProviderId::CODEX,
        ResponsesDecoder::default(),
    )
    .await;
}

/// SPIKE: S6 — same caveat. A Responses stream that ends in a tool call
/// terminates with `ToolUse`, not `EndTurn`.
#[tokio::test]
async fn codex_responses_tool_call() {
    golden(
        "codex_responses_tool_call",
        ProviderId::CODEX,
        ResponsesDecoder::default(),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

/// Thinking blocks, `ping`, `signature_delta` and usage split across
/// `message_start` and `message_delta` — the four things §10 means by
/// "distinct tool-use and thinking-block handling".
#[tokio::test]
async fn anthropic_messages_stream() {
    golden(
        "anthropic_messages",
        ProviderId::ANTHROPIC,
        MessagesDecoder::default(),
    )
    .await;
}

#[tokio::test]
async fn anthropic_tool_use_stream() {
    golden(
        "anthropic_tool_use",
        ProviderId::ANTHROPIC,
        MessagesDecoder::default(),
    )
    .await;
}
