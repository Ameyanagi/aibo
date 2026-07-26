//! Google Vertex AI — native Gemini, regional endpoints, service-account JWT
//! refresh (§10). **Stub: auth and routing wired, wire format not implemented.**
//!
//! §10 puts Vertex last in the implementation order (shared OpenAI-compat →
//! Anthropic → Bedrock → Vertex), so what exists here is the part that is
//! already decided and cheap to get wrong later:
//!
//! - **Regional endpoints.** The host is `{region}-aiplatform.googleapis.com`
//!   and the model path embeds project and location. There is no global
//!   endpoint for the models aibo would bind, so the region is part of the
//!   provider's identity, not a request parameter.
//! - **Auth is a token provider, not a key.** A service-account JWT is
//!   exchanged for a short-lived OAuth2 access token and refreshed on a
//!   schedule — [`Credential::GcpServiceAccount`] wraps exactly that, and
//!   [`RefreshingTokenProvider`] already implements the refresh-with-jitter and
//!   single-flight behaviour it needs.
//!
//! What is **not** implemented: the Gemini `generateContent` request body and
//! its `streamGenerateContent?alt=sse` event shape. It shares nothing with the
//! OpenAI or Anthropic decoders — `contents`/`parts`, `functionCall`,
//! `usageMetadata`, `safetyRatings`, `finishReason` — so it needs its own
//! decoder plus a golden-fixture set, budgeted at the 1–3 days §10 states.
//!
//! [`RefreshingTokenProvider`]: crate::auth::RefreshingTokenProvider

use std::time::Instant;

use aibo_core::error::{AiboError, Result};
use aibo_core::traits::Provider;
use aibo_core::types::{
    BoxStream, Capabilities, ChatRequest, Credential, Health, ModelInfo, MultiCandidate,
    ProviderId, StreamEvent,
};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth::{AuthStyle, apply_credential};
use crate::http::{HttpConfig, build_client};
use crate::wire::Unimplemented;

/// Provider defaults. Per-model values come from the §19 manifest.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        vision: true,
        streaming: true,
        json_schema: true,
        prompt_cache: true,
        multi_candidate: MultiCandidate::Native,
        max_context: 1_000_000,
        max_output: Some(65_536),
        ..Capabilities::default()
    }
}

/// Vertex AI, scoped to one project and region.
pub struct Vertex {
    id: ProviderId,
    project: String,
    region: String,
    credential: Credential,
    client: reqwest::Client,
    capabilities: Capabilities,
}

impl std::fmt::Debug for Vertex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vertex")
            .field("project", &self.project)
            .field("region", &self.region)
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

impl Vertex {
    /// Build the provider.
    ///
    /// Rejects anything but [`Credential::GcpServiceAccount`] at construction:
    /// an API key against Vertex is a 401 on the first hotkey press, and
    /// failing here means the user sees it in settings instead.
    pub fn new(
        project: impl Into<String>,
        region: impl Into<String>,
        credential: Credential,
    ) -> Result<Self> {
        let id = ProviderId::VERTEX;
        if !matches!(credential, Credential::GcpServiceAccount(_)) {
            return Err(Unimplemented::err(
                id,
                "Vertex accepts only Credential::GcpServiceAccount",
            ));
        }
        Ok(Self {
            id,
            project: project.into(),
            region: region.into(),
            credential,
            client: build_client(&HttpConfig::default())?,
            capabilities: default_capabilities(),
        })
    }

    /// The regional API host. There is no global endpoint for these models.
    pub fn host(&self) -> String {
        format!("https://{}-aiplatform.googleapis.com", self.region)
    }

    /// The streaming URL for one model. `alt=sse` is what turns Vertex's
    /// chunked-JSON-array response into server-sent events; without it the
    /// body is a JSON array and the SSE parser sees nothing.
    pub fn stream_url(&self, model: &str) -> Result<Url> {
        let s = format!(
            "{}/v1/projects/{}/locations/{}/publishers/google/models/{}:streamGenerateContent?alt=sse",
            self.host(),
            self.project,
            self.region,
            model
        );
        Url::parse(&s).map_err(|e| AiboError::Internal(Box::new(e)))
    }
}

#[async_trait]
impl Provider for Vertex {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn chat(
        &self,
        _req: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        // TODO(§10, after Bedrock): Gemini `contents`/`parts` request body and
        // the `streamGenerateContent` event decoder, plus golden fixtures.
        // Returning a handled error rather than `todo!()` keeps a
        // mis-configured provider from panicking the tray process (§6).
        Err(Unimplemented::err(
            self.id.clone(),
            "the Gemini streamGenerateContent wire format",
        ))
    }

    async fn models(&self) -> Result<Vec<ModelInfo>> {
        // Vertex's model garden listing is paginated and permission-scoped; the
        // shipped manifest (§19) is the source of truth here rather than a
        // runtime call.
        Ok(Vec::new())
    }

    async fn health(&self) -> Result<Health> {
        // Auth is the part that actually fails for Vertex — an expired JWT
        // exchange, not an unreachable host. Probing the token provider
        // exercises the refresh path without depending on the unimplemented
        // wire format.
        let started = Instant::now();
        let rb = self.client.get(self.host());
        match apply_credential(&self.id, &self.credential, AuthStyle::Bearer, rb).await {
            Ok(_) => Ok(Health::Ok {
                latency: started.elapsed(),
            }),
            Err(e) => Ok(Health::Degraded {
                reason: e.to_string(),
                consecutive_failures: 1,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::StaticTokenProvider;
    use secrecy::SecretString;
    use std::sync::Arc;

    fn vertex() -> Vertex {
        Vertex::new(
            "my-project",
            "us-central1",
            Credential::GcpServiceAccount(Arc::new(StaticTokenProvider::new(
                SecretString::from("t".to_string()),
                "test",
            ))),
        )
        .unwrap()
    }

    #[test]
    fn the_endpoint_is_regional_and_asks_for_sse() {
        let url = vertex().stream_url("gemini-3-pro").unwrap();
        assert_eq!(
            url.host_str(),
            Some("us-central1-aiplatform.googleapis.com")
        );
        assert!(
            url.as_str()
                .contains("/projects/my-project/locations/us-central1/")
        );
        assert!(url.as_str().ends_with("streamGenerateContent?alt=sse"));
    }

    #[test]
    fn an_api_key_is_rejected_at_construction() {
        assert!(
            Vertex::new(
                "p",
                "us-central1",
                Credential::ApiKey(SecretString::from("k".to_string()))
            )
            .is_err()
        );
    }
}
