//! Azure OpenAI — deployment-scoped URLs, `api-version`, key **or** Entra ID
//! (§10).
//!
//! Azure is the one "OpenAI-compatible" provider whose difference is entirely
//! in the envelope rather than the body, so it is not a stub in the way Vertex
//! and Bedrock are: the wire format is Chat Completions and the shared decoder
//! handles it unchanged. What is Azure-specific:
//!
//! - **The URL is deployment-scoped.** `{endpoint}/openai/deployments/{d}/…`,
//!   where `d` is a name the user chose in the portal, **not** a model id. The
//!   `model` field in the body is ignored.
//! - **`api-version` matters** (§10). A missing or retired one is a 400 with an
//!   unhelpful body, so it is a required part of the credential rather than a
//!   default hidden in this module.
//! - **Two auth modes.** [`Credential::AzureKey`] sends `api-key: …`;
//!   [`Credential::EntraId`] sends `Authorization: Bearer …` from a
//!   [`TokenProvider`] (managed identity or device code) and refreshes on its
//!   own schedule.
//!
//! Remaining work before this can be called shipped: the no-data-retention
//! posture §10 asks to document, and a golden fixture per `api-version`.
//!
//! [`TokenProvider`]: aibo_core::types::TokenProvider

use aibo_core::error::{AiboError, Result};
use aibo_core::types::{Capabilities, Credential, MultiCandidate, ProviderId};
use url::Url;

use crate::auth::AuthStyle;
use crate::http::HttpConfig;
use crate::openai_compat::{OpenAiCompat, Quirks, UrlStyle};
use crate::wire::Unimplemented;

/// A conservative default `api-version`.
///
/// Deliberately not used as a fallback: [`provider`] requires the version to be
/// stated. This constant exists so onboarding has something to pre-fill.
/// [unverified — check against the resource before shipping.]
pub const SUGGESTED_API_VERSION: &str = "2024-10-21";

/// Provider defaults. Real capabilities depend on which model the deployment
/// points at, which Azure does not report — so the catalogue must come from the
/// §19 manifest keyed by deployment, not from the endpoint.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        vision: false,
        streaming: true,
        json_schema: true,
        prompt_cache: true,
        multi_candidate: MultiCandidate::Native,
        max_context: 128_000,
        max_output: Some(16_384),
        ..Capabilities::default()
    }
}

/// The quirk set for a deployment.
pub fn quirks(deployment: String, api_version: String, auth: AuthStyle) -> Quirks {
    Quirks {
        url: UrlStyle::AzureDeployment {
            deployment,
            api_version,
        },
        auth,
        max_completion_tokens: true,
        json_schema: true,
        seed: true,
        // Azure's `/openai/models` lists the *resource's* models, not the
        // deployments the user can actually call. Treated as unavailable until
        // the deployment-listing endpoint is wired up.
        models_endpoint: false,
        ..Quirks::chat_completions()
    }
}

/// Build the Azure provider.
///
/// `endpoint` is the resource root, e.g.
/// `https://my-resource.openai.azure.com` — **without** a `/v1` suffix, which
/// Azure does not use.
///
/// For [`Credential::EntraId`] the deployment and `api-version` cannot be read
/// off the credential, so they are passed explicitly.
pub fn provider(
    endpoint: &str,
    credential: Credential,
    deployment: Option<String>,
    api_version: Option<String>,
) -> Result<OpenAiCompat> {
    let id = ProviderId::AZURE_OPENAI;
    let url = Url::parse(endpoint).map_err(|e| AiboError::Internal(Box::new(e)))?;

    let (deployment, api_version, auth) = match (&credential, deployment, api_version) {
        (
            Credential::AzureKey {
                deployment: d,
                api_version: v,
                ..
            },
            _,
            _,
        ) => (d.clone(), v.clone(), AuthStyle::AzureApiKey),
        (Credential::EntraId(_), Some(d), Some(v)) => (d, v, AuthStyle::Bearer),
        (Credential::EntraId(_), _, _) => {
            return Err(Unimplemented::err(
                id,
                "Entra ID auth needs an explicit deployment and api-version",
            ));
        }
        _ => {
            return Err(Unimplemented::err(
                id,
                "Azure OpenAI accepts only Credential::AzureKey or Credential::EntraId",
            ));
        }
    };

    Ok(OpenAiCompat::new(
        id,
        url,
        quirks(deployment, api_version, auth),
        credential,
        HttpConfig::default(),
    )?
    .with_capabilities(default_capabilities()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai_compat::UrlStyle;
    use secrecy::SecretString;

    #[test]
    fn a_key_credential_carries_its_own_deployment_and_version() {
        let cred = Credential::AzureKey {
            key: SecretString::from("k".to_string()),
            deployment: "prod-gpt".to_string(),
            api_version: "2026-01-01".to_string(),
        };
        let p = provider("https://r.openai.azure.com", cred, None, None).unwrap();
        assert!(matches!(
            &p.quirks().url,
            UrlStyle::AzureDeployment { deployment, api_version }
                if deployment == "prod-gpt" && api_version == "2026-01-01"
        ));
        assert_eq!(p.quirks().auth, AuthStyle::AzureApiKey);
    }

    #[test]
    fn an_api_key_credential_is_rejected_rather_than_silently_mis_signed() {
        let cred = Credential::ApiKey(SecretString::from("k".to_string()));
        assert!(provider("https://r.openai.azure.com", cred, None, None).is_err());
    }
}
