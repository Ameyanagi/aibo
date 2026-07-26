//! Groq — LPU inference, second ultra-fast option (§10).
//!
//! Not the same company as xAI's Grok. §10 requires shipping both and
//! disambiguating them in settings; the two [`ProviderId`]s (`groq`, `xai`)
//! exist precisely so a user cannot silently configure the wrong one.

use aibo_core::error::Result;
use aibo_core::types::{Capabilities, Credential, MultiCandidate, ProviderId};

use super::{OpenAiCompat, Quirks, UsagePlacement, build, require_api_key};

/// Groq's OpenAI-compatible base URL. Note the `/openai` segment — the root of
/// `api.groq.com` is a different API.
pub const BASE_URL: &str = "https://api.groq.com/openai/v1";

/// Provider defaults. Per-model values come from the §19 manifest.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        streaming: true,
        json_schema: true,
        multi_candidate: MultiCandidate::Unsupported,
        max_context: 131_072,
        max_output: Some(8_192),
        ..Capabilities::default()
    }
}

/// The quirk set.
///
/// The interesting one: Groq reports token usage under a vendor extension
/// (`x_groq.usage`) on the terminal chunk rather than in the standard `usage`
/// field, which the shared decoder handles. Without that, the spend meter would
/// see nothing at all for every Groq request (§14). [unverified — pinned by
/// `tests/fixtures/groq_chat_completions.sse`.]
pub fn quirks() -> Quirks {
    Quirks {
        usage: UsagePlacement::FinalChunk,
        json_schema: true,
        seed: true,
        ..Quirks::chat_completions()
    }
}

/// Build the provider.
pub fn provider(credential: Credential) -> Result<OpenAiCompat> {
    let id = ProviderId::GROQ;
    require_api_key(&id, &credential)?;
    Ok(build(id, BASE_URL, quirks(), credential)?.with_capabilities(default_capabilities()))
}
