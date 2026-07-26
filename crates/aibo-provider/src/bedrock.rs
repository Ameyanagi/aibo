//! AWS Bedrock — `converse-stream`, signed per request with SigV4 (§10).
//! **Stub: routing and credential resolution wired, signing not implemented.**
//!
//! §10 calls SigV4 "the fiddly one" and puts Bedrock third in the
//! implementation order. Three things make it genuinely different from every
//! other provider in the matrix, and all three are why it cannot be a [`Quirks`]
//! row on the OpenAI-compatible module:
//!
//! 1. **There is no bearer token.** Every request is signed with a derived key
//!    over a canonical request; §7 says this alone justifies a per-provider
//!    implementation, and [`apply_credential`] refuses
//!    [`Credential::AwsSigV4`] for exactly that reason.
//! 2. **`converse-stream` is not SSE.** The response is
//!    `application/vnd.amazon.eventstream`: length-prefixed binary frames with
//!    headers and a CRC, not `data:` lines. None of [`crate::sse`] applies — it
//!    needs its own framing decoder.
//! 3. **Model ids are region-scoped**, and inference-profile ARNs differ from
//!    the bare ids, so the catalogue is per region rather than global.
//!
//! **Blocked on a workspace dependency.** Signing needs `aws-sigv4` (and
//! `aws-config` for [`CredentialChain::Default`], which resolves env → profile
//! → IMDS → container). Neither is in the workspace `Cargo.toml`, which this
//! module does not own. Adding them is a prerequisite for finishing this file.
//!
//! [`Quirks`]: crate::openai_compat::Quirks
//! [`apply_credential`]: crate::auth::apply_credential

use aibo_core::error::Result;
use aibo_core::traits::Provider;
use aibo_core::types::{
    BoxStream, Capabilities, ChatRequest, Credential, CredentialChain, Health, ModelInfo,
    MultiCandidate, ProviderId, StreamEvent,
};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::http::{HttpConfig, build_client};
use crate::wire::Unimplemented;

/// Provider defaults. Per-model values come from the §19 manifest.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        vision: true,
        streaming: true,
        prompt_cache: true,
        multi_candidate: MultiCandidate::Unsupported,
        max_context: 200_000,
        max_output: Some(8_192),
        ..Capabilities::default()
    }
}

/// The Bedrock service name used in the SigV4 credential scope.
pub const SIGNING_SERVICE: &str = "bedrock";

/// AWS Bedrock, scoped to one region.
pub struct Bedrock {
    id: ProviderId,
    region: String,
    chain: CredentialChain,
    credential: Credential,
    client: reqwest::Client,
    capabilities: Capabilities,
}

impl std::fmt::Debug for Bedrock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bedrock")
            .field("region", &self.region)
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

impl Bedrock {
    /// Build the provider.
    ///
    /// Region comes off the credential rather than being a separate argument:
    /// a SigV4 signature is scoped to a region, so a mismatch between the
    /// signing region and the endpoint is a 403 with a misleading message.
    pub fn new(credential: Credential) -> Result<Self> {
        let id = ProviderId::BEDROCK;
        let (chain, region) = match &credential {
            Credential::AwsSigV4 { chain, region } => (chain.clone(), region.clone()),
            _ => {
                return Err(Unimplemented::err(
                    id,
                    "Bedrock accepts only Credential::AwsSigV4",
                ));
            }
        };
        Ok(Self {
            id,
            region,
            chain,
            credential,
            client: build_client(&HttpConfig::default())?,
            capabilities: default_capabilities(),
        })
    }

    /// The regional runtime host.
    pub fn host(&self) -> String {
        format!("https://bedrock-runtime.{}.amazonaws.com", self.region)
    }

    /// The streaming URL for a model id or inference-profile ARN.
    ///
    /// The id is path-encoded because ARNs contain `/` and `:`.
    pub fn converse_stream_url(&self, model_id: &str) -> String {
        let encoded: String = model_id
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect();
        format!("{}/model/{encoded}/converse-stream", self.host())
    }

    /// The credential resolution strategy in force.
    pub fn chain(&self) -> &CredentialChain {
        &self.chain
    }

    /// The pooled client, so the signing implementation reuses this provider's
    /// connections rather than opening its own.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

#[async_trait]
impl Provider for Bedrock {
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
        // TODO(§10, third in order): resolve `self.chain` to credentials, sign
        // the `converse-stream` POST with SigV4, and decode
        // `application/vnd.amazon.eventstream` frames into `StreamEvent`.
        // Needs `aws-sigv4` + `aws-config` in the workspace manifest.
        Err(Unimplemented::err(
            self.id.clone(),
            "SigV4 request signing and the converse-stream event framing",
        ))
    }

    async fn models(&self) -> Result<Vec<ModelInfo>> {
        // `ListFoundationModels` is itself a signed call, so it lands with the
        // rest of the signing work. Until then the §19 manifest is the source.
        Ok(Vec::new())
    }

    async fn health(&self) -> Result<Health> {
        // An unsigned probe of a Bedrock endpoint returns 403 whether the
        // service is healthy or not, so it would report nothing useful.
        // `Unknown` is the honest answer until signing exists — §13 requires
        // "never probed" to be distinguishable from "probed and failing".
        Ok(Health::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bedrock() -> Bedrock {
        Bedrock::new(Credential::AwsSigV4 {
            chain: CredentialChain::Profile("work".into()),
            region: "us-east-1".into(),
        })
        .unwrap()
    }

    #[test]
    fn the_endpoint_is_region_scoped() {
        assert_eq!(
            bedrock().host(),
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        );
    }

    #[test]
    fn an_arn_model_id_is_path_encoded() {
        let url = bedrock().converse_stream_url(
            "arn:aws:bedrock:us-east-1:1234:inference-profile/us.anthropic.claude",
        );
        assert!(!url.trim_start_matches("https://").contains(':'), "{url}");
        assert!(url.ends_with("/converse-stream"), "{url}");
    }

    #[test]
    fn a_bearer_credential_is_rejected_at_construction() {
        use secrecy::SecretString;
        assert!(Bedrock::new(Credential::ApiKey(SecretString::from("k".to_string()))).is_err());
    }

    #[tokio::test]
    async fn health_is_unknown_rather_than_a_misleading_failure() {
        assert_eq!(bedrock().health().await.unwrap(), Health::Unknown);
    }
}
