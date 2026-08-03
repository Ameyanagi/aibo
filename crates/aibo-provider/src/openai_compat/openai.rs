//! OpenAI — the `Smart` tier, native wire format (§10).
//!
//! §10 lists OpenAI's wire format as **native**, i.e. Responses rather than
//! Chat Completions, so [`provider`] builds the Responses variant. Chat
//! Completions is still reachable via [`chat_completions_provider`] because
//! some deployments and older models only speak it — and because that path
//! carries a quirk of its own: newer models reject `max_tokens` and require
//! `max_completion_tokens`.

use aibo_core::error::Result;
use aibo_core::types::{Capabilities, Credential, MultiCandidate, ProviderId};

use super::{OpenAiCompat, Quirks, build, require_api_key};

/// OpenAI's API base URL.
pub const BASE_URL: &str = "https://api.openai.com/v1";

/// Provider defaults. Per-model values come from the §19 manifest.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        vision: true,
        streaming: true,
        reasoning_effort: true,
        json_schema: true,
        prompt_cache: true,
        // The provider can generate `n > 1`, but `StreamEvent` currently has no
        // candidate identity. Advertising Native would make the decoder merge
        // independent answers, so request one until the event model can carry
        // them separately.
        multi_candidate: MultiCandidate::Unsupported,
        max_context: 400_000,
        max_output: Some(128_000),
        ..Capabilities::default()
    }
}

/// The Responses quirk set — OpenAI's native format.
pub fn quirks() -> Quirks {
    Quirks {
        seed: false,
        multi_candidate: MultiCandidate::Unsupported,
        ..Quirks::responses()
    }
}

/// The Chat Completions quirk set, for deployments that need it.
pub fn chat_completions_quirks() -> Quirks {
    Quirks {
        // Newer models 400 on `max_tokens`. This is the single most common
        // cause of "it works on Groq but not on OpenAI".
        max_completion_tokens: true,
        json_schema: true,
        seed: true,
        reasoning_effort: true,
        multi_candidate: MultiCandidate::Unsupported,
        ..Quirks::chat_completions()
    }
}

/// Build the provider on the native Responses format.
pub fn provider(credential: Credential) -> Result<OpenAiCompat> {
    let id = ProviderId::OPENAI;
    require_api_key(&id, &credential)?;
    Ok(build(id, BASE_URL, quirks(), credential)?.with_capabilities(default_capabilities()))
}

/// Build the provider on Chat Completions.
pub fn chat_completions_provider(credential: Credential) -> Result<OpenAiCompat> {
    let id = ProviderId::OPENAI;
    require_api_key(&id, &credential)?;
    Ok(build(id, BASE_URL, chat_completions_quirks(), credential)?
        .with_capabilities(default_capabilities()))
}
