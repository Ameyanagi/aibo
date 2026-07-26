//! Cerebras — the primary `Fast` binding (§4, §10).
//!
//! Ultra-fast OpenAI-compatible SSE with an API key. This is the provider the
//! 250 ms Complete budget (§1) is actually written against, so nothing here
//! adds a round trip: no catalogue fetch on the hot path, no auth exchange.

use aibo_core::error::Result;
use aibo_core::types::{Capabilities, Credential, MultiCandidate, ProviderId};

use super::{OpenAiCompat, Quirks, UsagePlacement, build, require_api_key};

/// Cerebras' OpenAI-compatible base URL.
pub const BASE_URL: &str = "https://api.cerebras.ai/v1";

/// Provider defaults. Per-model values come from the §19 manifest.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        streaming: true,
        multi_candidate: MultiCandidate::Unsupported,
        max_context: 128_000,
        max_output: Some(8_192),
        ..Capabilities::default()
    }
}

/// The quirk set.
///
/// `usage` is read from the final chunk without an opt-in, and
/// `stream_options` is **not** sent: Cerebras rejects unknown top-level request
/// fields rather than ignoring them. [unverified — confirm with a captured 400
/// and a golden fixture before P3 sign-off.]
pub fn quirks() -> Quirks {
    Quirks {
        usage: UsagePlacement::FinalChunk,
        seed: false,
        json_schema: false,
        ..Quirks::chat_completions()
    }
}

/// Build the provider.
pub fn provider(credential: Credential) -> Result<OpenAiCompat> {
    let id = ProviderId::CEREBRAS;
    require_api_key(&id, &credential)?;
    Ok(build(id, BASE_URL, quirks(), credential)?.with_capabilities(default_capabilities()))
}
