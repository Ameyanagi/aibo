//! SambaNova — the second ultra-fast `Fast` option (§10).

use aibo_core::error::Result;
use aibo_core::types::{Capabilities, Credential, MultiCandidate, ProviderId};

use super::{OpenAiCompat, Quirks, UsagePlacement, build, require_api_key};
use crate::wire::ErrorShape;

/// SambaNova's OpenAI-compatible base URL.
pub const BASE_URL: &str = "https://api.sambanova.ai/v1";

/// Provider defaults. Per-model values come from the §19 manifest.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        streaming: true,
        multi_candidate: MultiCandidate::Unsupported,
        max_context: 16_384,
        max_output: Some(4_096),
        ..Capabilities::default()
    }
}

/// The quirk set.
///
/// The stack is vLLM-derived, which shows up in exactly the place §10 warns
/// about: errors come back as `{"detail": …}` rather than OpenAI's
/// `{"error": {...}}` envelope, so a shared error parser reports nothing useful
/// unless the shape is declared. [unverified — confirm against a captured 4xx.]
pub fn quirks() -> Quirks {
    Quirks {
        usage: UsagePlacement::FinalChunk,
        error_shape: ErrorShape::Detail,
        seed: false,
        json_schema: false,
        ..Quirks::chat_completions()
    }
}

/// Build the provider.
pub fn provider(credential: Credential) -> Result<OpenAiCompat> {
    let id = ProviderId::SAMBANOVA;
    require_api_key(&id, &credential)?;
    Ok(build(id, BASE_URL, quirks(), credential)?.with_capabilities(default_capabilities()))
}
