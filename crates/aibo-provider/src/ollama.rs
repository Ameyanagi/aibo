//! Ollama / llama.cpp — the `Cheap` binding **and the offline story** (§10, §13).
//!
//! §13 makes this more than a cost tier: when the network is gone, "Ollama, if
//! configured, works" is the strongest argument for shipping local inference in
//! v1 rather than after it. Two consequences show up in this module.
//!
//! **Nothing here may treat a global "offline" flag as meaningful.** A failed
//! connection to Cerebras says nothing about a model running on `localhost`.
//! Health is probed against the local endpoint and nowhere else.
//!
//! **The timeouts are different.** A cold local model loads weights from disk
//! before the first token, so the read timeout is minutes, not seconds
//! ([`HttpConfig::local`]). Applying the cloud budget here would report a
//! working setup as broken.

use aibo_core::error::{AiboError, Result};
use aibo_core::types::{Capabilities, Credential, MultiCandidate, ProviderId};
use url::Url;

use crate::http::HttpConfig;
use crate::openai_compat::{OpenAiCompat, Quirks, UsagePlacement};
use crate::wire::ErrorShape;

/// The default Ollama endpoint. Its OpenAI-compatible surface lives under
/// `/v1`; the native API (`/api/*`) is not used, so one decoder covers both
/// Ollama and llama.cpp's server.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

/// Provider defaults.
///
/// Conservative on purpose: what a local install can do depends entirely on
/// which weights the user pulled, so the floor here is "text in, text out" and
/// the real values come from the catalogue.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: false,
        vision: false,
        streaming: true,
        multi_candidate: MultiCandidate::Unsupported,
        max_context: 8_192,
        max_output: Some(4_096),
        ..Capabilities::default()
    }
}

/// The quirk set.
///
/// Usage is reported on the final chunk; `stream_options` is not sent because
/// llama.cpp's server rejects unknown fields on some builds. Errors use the
/// flat `{"error": "…"}` shape, not OpenAI's envelope. [unverified — pinned by
/// `tests/fixtures/ollama_chat_completions.sse`.]
pub fn quirks() -> Quirks {
    Quirks {
        usage: UsagePlacement::FinalChunk,
        error_shape: ErrorShape::FlatError,
        tools: false,
        json_schema: false,
        seed: true,
        ..Quirks::chat_completions()
    }
}

/// Build the provider against `base_url`, or [`DEFAULT_BASE_URL`] when `None`.
///
/// The credential is always [`Credential::LocalEndpoint`]: there is no auth,
/// and accepting an API key here would let a misconfiguration send a paid key
/// to an arbitrary local port.
pub fn provider(base_url: Option<Url>) -> Result<OpenAiCompat> {
    let url = match base_url {
        Some(u) => u,
        None => Url::parse(DEFAULT_BASE_URL).map_err(|e| AiboError::Internal(Box::new(e)))?,
    };
    Ok(OpenAiCompat::new(
        ProviderId::OLLAMA,
        url.clone(),
        quirks(),
        Credential::LocalEndpoint(url),
        HttpConfig::local(),
    )?
    .with_capabilities(default_capabilities()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aibo_core::traits::Provider;

    #[test]
    fn the_default_endpoint_is_local_and_plaintext() {
        let p = provider(None).unwrap();
        assert_eq!(p.id(), ProviderId::OLLAMA);
        assert_eq!(p.base_url().scheme(), "http");
        assert_eq!(p.base_url().host_str(), Some("localhost"));
    }

    #[test]
    fn a_custom_endpoint_is_honoured() {
        let u = Url::parse("http://192.168.1.10:8080/v1").unwrap();
        let p = provider(Some(u.clone())).unwrap();
        assert_eq!(p.base_url(), &u);
    }
}
