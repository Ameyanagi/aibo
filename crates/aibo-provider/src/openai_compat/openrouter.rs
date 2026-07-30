//! OpenRouter — one key, many upstream models (§10).
//!
//! OpenAI-compatible, so the wire work is the `openai_compat` layer's. What
//! makes OpenRouter different is not the protocol but the *shape* of what it
//! serves: a single credential fronts hundreds of models from dozens of
//! upstream vendors, and that has two consequences this module has to respect.
//!
//! **The catalogue cannot be shipped.** §10's usual answer — bake a model list
//! into the binary — is hopeless here: the list changes weekly and is far too
//! long to curate by hand. `ModelCatalogue::refresh_from` calling `/models` is
//! the only workable source, which is why OpenRouter is the provider that most
//! needs that path to exist.
//!
//! **Capabilities vary per model, not per provider.** A request routed to a
//! 8 k-context model and one routed to a 200 k-context model share this
//! provider and nothing else. [`default_capabilities`] is therefore a floor
//! that exists to be overridden by the catalogue, not a description of what
//! OpenRouter can do — and the floor is set high enough not to truncate a
//! long prompt while still being honest about the conservative case.

use aibo_core::error::Result;
use aibo_core::types::{Capabilities, Credential, MultiCandidate, ProviderId};

use super::{OpenAiCompat, Quirks, UsagePlacement, build, require_api_key};

/// OpenRouter's OpenAI-compatible base URL.
pub const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Provider defaults, superseded per model by the catalogue.
///
/// `vision` and `tools` are declared true because the *service* supports both
/// and the per-model catalogue is what narrows it; declaring false here would
/// make the engine refuse an image before the model that could see it was ever
/// consulted.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        vision: true,
        streaming: true,
        prompt_cache: true,
        multi_candidate: MultiCandidate::Unsupported,
        max_context: 128_000,
        max_output: Some(32_768),
        ..Capabilities::default()
    }
}

/// The quirk set.
///
/// Plain Chat Completions with `stream_options: {include_usage: true}` — the
/// OpenAI spelling, which OpenRouter implements. Usage arrives on a final chunk
/// carrying no choices.
pub fn quirks() -> Quirks {
    Quirks {
        usage: UsagePlacement::RequiresStreamOptions,
        ..Quirks::chat_completions()
    }
}

/// Build the provider.
pub fn provider(credential: Credential) -> Result<OpenAiCompat> {
    let id = ProviderId::OPENROUTER;
    require_api_key(&id, &credential)?;
    Ok(build(id, BASE_URL, quirks(), credential)?.with_capabilities(default_capabilities()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The floor exists to be replaced by the catalogue, but while it is in
    /// force it must not be the 8 192-token default — a one-key-many-models
    /// provider routed through that would truncate almost every request.
    #[test]
    fn the_capability_floor_is_not_the_conservative_default() {
        assert!(default_capabilities().max_context > Capabilities::default().max_context);
    }

    #[test]
    fn usage_is_requested_the_openai_way() {
        assert_eq!(quirks().usage, UsagePlacement::RequiresStreamOptions);
    }
}
