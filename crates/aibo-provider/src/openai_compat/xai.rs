//! xAI (Grok) — fast/smart (§10).
//!
//! Distinct from Groq. Both are shipped and both are disambiguated in settings;
//! the ids are `xai` and `groq`.

use aibo_core::error::Result;
use aibo_core::types::{Capabilities, Credential, MultiCandidate, ProviderId};

use super::{OpenAiCompat, Quirks, ReasoningStyle, UsagePlacement, build, require_api_key};

/// xAI's OpenAI-compatible base URL.
pub const BASE_URL: &str = "https://api.x.ai/v1";

/// Provider defaults. Per-model values come from the §19 manifest.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        vision: true,
        streaming: true,
        json_schema: true,
        reasoning_effort: true,
        multi_candidate: MultiCandidate::Unsupported,
        max_context: 131_072,
        max_output: Some(16_384),
        ..Capabilities::default()
    }
}

/// The quirk set.
///
/// Reasoning models stream their thinking on a separate delta field, which must
/// land on [`StreamEvent::Reasoning`] and never on
/// [`StreamEvent::Text`] — §7 is explicit that reasoning is rendered collapsed
/// and never inserted into the user's document. [unverified: the field spelling
/// is pinned by `tests/fixtures/xai_reasoning.sse` and must be re-checked when
/// a new Grok model ships.]
///
/// [`StreamEvent::Reasoning`]: aibo_core::types::StreamEvent::Reasoning
/// [`StreamEvent::Text`]: aibo_core::types::StreamEvent::Text
pub fn quirks() -> Quirks {
    Quirks {
        usage: UsagePlacement::FinalChunk,
        reasoning: ReasoningStyle::DeltaReasoningContent,
        reasoning_effort: true,
        json_schema: true,
        seed: true,
        ..Quirks::chat_completions()
    }
}

/// Build the provider.
pub fn provider(credential: Credential) -> Result<OpenAiCompat> {
    let id = ProviderId::XAI;
    require_api_key(&id, &credential)?;
    Ok(build(id, BASE_URL, quirks(), credential)?.with_capabilities(default_capabilities()))
}
