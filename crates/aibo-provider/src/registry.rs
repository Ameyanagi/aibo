//! Provider construction from config, and the model catalogue (§4, §10).
//!
//! Two jobs.
//!
//! **Construction.** [`build`] turns one [`ProviderSpec`] into a
//! `Arc<dyn Provider>`, and [`ProviderRegistry`] holds the set the app has
//! configured. Construction is where a wrong credential type is rejected, so a
//! misconfiguration shows up in settings rather than on the first hotkey press.
//!
//! **Catalogue rot.** §10: role bindings point at concrete model ids and
//! providers retire them, so a v1.0 shipped with a hardcoded default starts
//! failing months later with an opaque 400. [`ModelCatalogue::resolve`] answers
//! "does this id still exist, and if not what is the closest" so the UI can say
//! *the model you selected no longer exists, here's the closest* instead of
//! surfacing the 400.
//!
//! **The Codex allowlist.** §3a measured which model ids the Codex endpoint
//! accepts, and the answer is a *namespace*, not a capability: ChatGPT-plan ids
//! work, API-style ids hard-400 with *"not supported when using Codex with a
//! ChatGPT account"*. §4 correctly does **not** fall back on a 400 — that is a
//! bug in aibo, not a reason to spend the user's money elsewhere — so a
//! mis-bound Codex model is an unrecoverable opaque error. [`CODEX_MODELS`] is
//! the known-good set, [`check_codex_model`] rejects the known-bad ones before
//! dispatch with the constraint named, and [`ProviderRegistry::for_binding`]
//! runs that check on the path every request already takes.
//!
//! The refusal is an [`AiboError::ModelRejected`] carrying the refused id and
//! [`codex_alternatives`]. It must not be an [`AiboError::Internal`]: §13 gives
//! `Internal` a generic "something went wrong" plus "copy diagnostics", so
//! boxing the refusal inside one recreates the opaque dead end this check was
//! written to prevent, one layer earlier and with the useful facts discarded.

use std::collections::BTreeMap;
use std::sync::Arc;

use aibo_core::error::{AiboError, Result};
use aibo_core::traits::Provider;
use aibo_core::types::{Capabilities, Credential, ModelBinding, ModelInfo, ProviderId};
use url::Url;

use crate::openai_compat::{
    Quirks, boxed, build as build_compat, cerebras, groq, openai, sambanova, xai,
};

/// Which backend a configured provider is.
///
/// Not `PartialEq`: the Codex variant carries a live token provider, and
/// comparing two of those is meaningless.
#[derive(Debug, Clone)]
pub enum ProviderKind {
    /// Cerebras (§10 ultra-fast).
    Cerebras,
    /// SambaNova (§10 ultra-fast).
    SambaNova,
    /// Groq (§10 ultra-fast). Not xAI.
    Groq,
    /// xAI / Grok (§10 fast-smart). Not Groq.
    Xai,
    /// OpenAI on its native Responses format.
    OpenAi,
    /// OpenAI on Chat Completions, for deployments that need it.
    OpenAiChatCompletions,
    /// Anthropic native `messages`.
    Anthropic,
    /// Azure OpenAI.
    Azure {
        /// Deployment name, when the credential does not carry one (Entra ID).
        deployment: Option<String>,
        /// `api-version`, when the credential does not carry one.
        api_version: Option<String>,
    },
    /// Google Vertex AI.
    Vertex {
        /// GCP project id.
        project: String,
        /// Region; part of the endpoint host.
        region: String,
    },
    /// AWS Bedrock.
    Bedrock,
    /// Ollama / llama.cpp.
    Ollama,
    /// The Codex Responses endpoint (§3a).
    Codex {
        /// Attestation posture, once S6 has answered it.
        attestation: crate::codex::AttestationPolicy,
        /// The device-code token provider.
        tokens: Arc<crate::auth::RefreshingTokenProvider>,
    },
    /// A user-added OpenAI-compatible endpoint. §10: the provider set is open,
    /// which is why [`ProviderId`] is a newtype rather than an enum.
    Custom {
        /// Quirk set, defaulting to plain Chat Completions.
        quirks: Box<Quirks>,
    },
}

/// One configured provider.
///
/// Not `Deserialize`: [`Credential`] holds secrets and token providers, so the
/// settings layer resolves those from the keychain and builds this by hand.
pub struct ProviderSpec {
    /// The id this provider is addressed by. Overrides the kind's default so a
    /// user can configure two Ollama endpoints.
    pub id: Option<ProviderId>,
    /// Which backend.
    pub kind: ProviderKind,
    /// Base URL override. Required for `Custom`, `Azure` and local endpoints.
    pub base_url: Option<Url>,
    /// How to authenticate.
    pub credential: Credential,
}

impl std::fmt::Debug for ProviderSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSpec")
            .field("id", &self.id)
            .field("base_url", &self.base_url.as_ref().map(Url::as_str))
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

/// Build one provider from its spec.
pub fn build(spec: ProviderSpec) -> Result<Arc<dyn Provider>> {
    let ProviderSpec {
        id,
        kind,
        base_url,
        credential,
    } = spec;

    Ok(match kind {
        ProviderKind::Cerebras => boxed(cerebras::provider(credential)?),
        ProviderKind::SambaNova => boxed(sambanova::provider(credential)?),
        ProviderKind::Groq => boxed(groq::provider(credential)?),
        ProviderKind::Xai => boxed(xai::provider(credential)?),
        ProviderKind::OpenAi => boxed(openai::provider(credential)?),
        ProviderKind::OpenAiChatCompletions => {
            boxed(openai::chat_completions_provider(credential)?)
        }
        ProviderKind::Anthropic => Arc::new(crate::anthropic::Anthropic::new(credential)?),
        ProviderKind::Azure {
            deployment,
            api_version,
        } => {
            let endpoint = base_url.ok_or(AiboError::NoProviderConfigured)?;
            boxed(crate::azure::provider(
                endpoint.as_str(),
                credential,
                deployment,
                api_version,
            )?)
        }
        ProviderKind::Vertex { project, region } => {
            Arc::new(crate::vertex::Vertex::new(project, region, credential)?)
        }
        ProviderKind::Bedrock => Arc::new(crate::bedrock::Bedrock::new(credential)?),
        ProviderKind::Ollama => boxed(crate::ollama::provider(base_url)?),
        ProviderKind::Codex {
            attestation,
            tokens,
        } => Arc::new(crate::codex::CodexProvider::new(
            tokens,
            base_url,
            attestation,
        )?),
        ProviderKind::Custom { quirks } => {
            let url = base_url.ok_or(AiboError::NoProviderConfigured)?;
            let id = id.clone().ok_or(AiboError::NoProviderConfigured)?;
            boxed(build_compat(id, url.as_str(), *quirks, credential)?)
        }
    })
}

/// The set of providers the app has configured.
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<ProviderId, Arc<dyn Provider>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("ids", &self.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ProviderRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a registry from specs, failing on the first bad one.
    ///
    /// Deliberately not lenient: a provider that silently vanished from the
    /// registry becomes an unexplained "no provider configured" later, which
    /// §13 makes the one error allowed to interrupt.
    pub fn from_specs(specs: Vec<ProviderSpec>) -> Result<Self> {
        let mut registry = Self::new();
        for spec in specs {
            let id = spec
                .id
                .clone()
                .unwrap_or_else(|| default_id_for(&spec.kind));
            registry.insert(id, build(spec)?);
        }
        Ok(registry)
    }

    /// Add or replace a provider.
    pub fn insert(&mut self, id: ProviderId, provider: Arc<dyn Provider>) {
        self.providers.insert(id, provider);
    }

    /// Look one up.
    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.providers.get(id).cloned()
    }

    /// Resolve a [`ModelBinding`] to its provider.
    ///
    /// This is also where §3a's Codex allowlist is enforced. §4 does not fall
    /// back on a 400, and it is right not to — so a Codex binding carrying an
    /// API-style id would otherwise reach the wire, come back as an opaque
    /// *"not supported when using Codex with a ChatGPT account"*, and stop
    /// there. Checking here costs a string comparison on a path every request
    /// already takes, and turns that into an [`AiboError::ModelRejected`] that
    /// names the constraint and the ids that work.
    pub fn for_binding(&self, binding: &ModelBinding) -> Result<Arc<dyn Provider>> {
        if binding.provider == ProviderId::CODEX {
            check_codex_model(&binding.model)?;
        }
        self.get(&binding.provider)
            .ok_or(AiboError::NoProviderConfigured)
    }

    /// Every configured id, in a stable order.
    pub fn ids(&self) -> Vec<ProviderId> {
        self.providers.keys().cloned().collect()
    }

    /// Whether anything is configured at all — the check behind
    /// [`AiboError::NoProviderConfigured`], the only error §13 lets interrupt.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

fn default_id_for(kind: &ProviderKind) -> ProviderId {
    match kind {
        ProviderKind::Cerebras => ProviderId::CEREBRAS,
        ProviderKind::SambaNova => ProviderId::SAMBANOVA,
        ProviderKind::Groq => ProviderId::GROQ,
        ProviderKind::Xai => ProviderId::XAI,
        ProviderKind::OpenAi | ProviderKind::OpenAiChatCompletions => ProviderId::OPENAI,
        ProviderKind::Anthropic => ProviderId::ANTHROPIC,
        ProviderKind::Azure { .. } => ProviderId::AZURE_OPENAI,
        ProviderKind::Vertex { .. } => ProviderId::VERTEX,
        ProviderKind::Bedrock => ProviderId::BEDROCK,
        ProviderKind::Ollama => ProviderId::OLLAMA,
        ProviderKind::Codex { .. } => ProviderId::CODEX,
        ProviderKind::Custom { .. } => ProviderId::new("custom"),
    }
}

// ---------------------------------------------------------------------------
// The Codex model allowlist (§3a)
// ---------------------------------------------------------------------------

/// One measured entry on the Codex endpoint's allowlist (§3a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexModel {
    /// The wire id, as sent.
    pub id: &'static str,
    /// Name for the model picker.
    pub display_name: &'static str,
    /// Measured TTFT p50 in milliseconds — Yokohama, warm connection, n=3,
    /// ~10-token prompt (§3a). Prefill is negligible at this scale: `gpt-5.5`
    /// measured 430 ms at ~900 tokens against 435 ms at ~10, i.e. within noise.
    /// The number is fixed overhead, not input processing.
    pub ttft_p50_ms: u32,
}

/// The Codex ids §3a verified as **working** — HTTP 200, streamed SSE, a real
/// completion returned.
///
/// The distinction from [`CODEX_REJECTED_MODELS`] is the id *namespace*, not
/// model capability: these are ChatGPT-plan ids, those are API-style ids, and
/// the endpoint is reached with a ChatGPT subscription.
///
/// Ordered by measured latency, so the picker's first entry is the fastest.
/// **None of them is fast enough for `Fast`** — see [`CODEX_TTFT_FLOOR_MS`].
pub const CODEX_MODELS: &[CodexModel] = &[
    CodexModel {
        id: "gpt-5.5",
        display_name: "GPT-5.5",
        ttft_p50_ms: 435,
    },
    CodexModel {
        id: "gpt-5.6-terra",
        display_name: "GPT-5.6 Terra",
        ttft_p50_ms: 446,
    },
    CodexModel {
        id: "gpt-5.3-codex-spark",
        display_name: "GPT-5.3 Codex Spark",
        ttft_p50_ms: 499,
    },
    CodexModel {
        id: "gpt-5.6-luna",
        display_name: "GPT-5.6 Luna",
        ttft_p50_ms: 515,
    },
    CodexModel {
        id: "gpt-5.6-sol",
        display_name: "GPT-5.6 Sol",
        ttft_p50_ms: 623,
    },
];

/// The ids §3a verified as **rejected** — HTTP 400 with
/// [`CODEX_ACCOUNT_CONSTRAINT`].
///
/// Listed explicitly rather than inferred, because the failure they produce is
/// the worst kind: §4 does not fall back on a 400, so the request stops dead
/// with a message that reads like an aibo bug. Naming them lets the error say
/// *why*. An id that is on neither list is treated as unknown, not rejected —
/// the allowlist is a measurement from one day and OpenAI can add to it.
pub const CODEX_REJECTED_MODELS: &[&str] = &[
    "gpt-5",
    "gpt-5-codex",
    "gpt-5.1-codex",
    "gpt-5.1-codex-mini",
    "codex-mini-latest",
];

/// The 400 body §3a observed for every rejected id, quoted verbatim so the
/// pre-dispatch refusal and the post-dispatch failure say the same thing.
pub const CODEX_ACCOUNT_CONSTRAINT: &str = "not supported when using Codex with a ChatGPT account";

/// The lowest measured TTFT p50 across the whole allowlist (§3a).
///
/// `Complete`'s budget is 250 ms, so this is the number that disqualifies Codex
/// from `Fast` — the constraint enforced in `aibo_core::roles`.
pub const CODEX_TTFT_FLOOR_MS: u32 = 435;

/// Whether §3a's Codex allowlist has **verified** image-input support.
///
/// `false`, and it is a measurement gap rather than a finding: §3a sent text and
/// only text. Named as a constant so the claim has one home — `codex_models`
/// asserts against it, `codex::default_capabilities` carries the same `SPIKE`
/// note, and a future probe flips one place.
pub const CODEX_VISION_UNVERIFIED: bool = true;

/// The allowlist as ids, best (fastest) first — [`AiboError::ModelRejected`]'s
/// `alternatives`, and the order the model picker offers them in.
pub fn codex_alternatives() -> Vec<String> {
    CODEX_MODELS.iter().map(|m| m.id.to_string()).collect()
}

/// Whether `model` is on §3a's verified-working allowlist.
pub fn is_codex_model_allowed(model: &str) -> bool {
    CODEX_MODELS.iter().any(|m| m.id == model)
}

/// The error a Codex binding on §3a's measured-rejected list produces.
///
/// [`AiboError::ModelRejected`] rather than [`AiboError::Internal`], and that
/// choice is the whole point of checking here. `Internal` is the one variant
/// §13 renders as "something went wrong" plus "copy diagnostics" — so wrapping
/// this in it reproduced, one layer earlier, exactly the opaque unrecoverable
/// error the pre-dispatch check exists to prevent. The typed variant carries
/// the refused id and the ids that do work, which is what lets the panel spend
/// its one §13 action on "use `gpt-5.5` instead".
pub fn codex_model_rejected(model: &str) -> AiboError {
    AiboError::ModelRejected {
        provider: ProviderId::CODEX,
        model: model.to_string(),
        constraint: format!(
            "`{model}` is an API-style id and the Codex endpoint refuses it with \
             \"{CODEX_ACCOUNT_CONSTRAINT}\": aibo reaches it with a ChatGPT subscription, \
             not an API key, so only ChatGPT-plan ids work"
        ),
        alternatives: codex_alternatives(),
    }
}

/// Reject a Codex model id known to hard-400, **before dispatch** (§3a, §4).
///
/// Deliberately *not* a strict allowlist. An id that is on neither list is let
/// through: [`CODEX_MODELS`] is a measurement taken on one day, OpenAI adds
/// ids, and refusing an id that would have worked is its own bug. What this
/// catches is the failure mode that has no recovery — an API-style id, which
/// 400s, which §4 does not fall back on, which surfaces as an opaque error the
/// user cannot act on.
pub fn check_codex_model(model: &str) -> Result<()> {
    if CODEX_REJECTED_MODELS.contains(&model) {
        return Err(codex_model_rejected(model));
    }
    Ok(())
}

/// The Codex allowlist as catalogue entries (§3a, §10).
///
/// The endpoint publishes no `/models`, so `CodexProvider::models()` has
/// nothing to return and the catalogue is the only source. Capabilities come
/// from `codex::default_capabilities()`, which is the provider's own statement
/// about the endpoint.
pub fn codex_models() -> Vec<ModelInfo> {
    let capabilities = crate::codex::default_capabilities();
    CODEX_MODELS
        .iter()
        .map(|m| ModelInfo {
            provider: ProviderId::CODEX,
            id: m.id.to_string(),
            display_name: m.display_name.to_string(),
            capabilities: capabilities.clone(),
            deprecated: false,
            replaced_by: None,
        })
        .collect()
}

/// The rejected ids as *retired* catalogue entries pointing at a live one.
///
/// §10 asks the UI to say "the model you selected no longer exists, here's the
/// closest" instead of surfacing a 400. These ids never existed *on this path*,
/// which produces the same user-visible situation and deserves the same
/// treatment — and it is the only way a user who typed `gpt-5-codex` from
/// muscle memory gets told what to type instead.
pub fn codex_rejected_models() -> Vec<ModelInfo> {
    // `gpt-5.3-codex-spark` is the coding-shaped id on the allowlist, so it is
    // the honest successor for the `*-codex*` ones.
    const CODING_SUCCESSOR: &str = "gpt-5.3-codex-spark";
    const GENERAL_SUCCESSOR: &str = "gpt-5.5";

    CODEX_REJECTED_MODELS
        .iter()
        .map(|id| ModelInfo {
            provider: ProviderId::CODEX,
            id: (*id).to_string(),
            display_name: format!("{id} (not available on a ChatGPT account)"),
            // Nothing is known about a model that cannot be called; the
            // conservative floor is the honest default.
            capabilities: Capabilities::default(),
            deprecated: true,
            replaced_by: Some(
                if id.contains("codex") {
                    CODING_SUCCESSOR
                } else {
                    GENERAL_SUCCESSOR
                }
                .to_string(),
            ),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Catalogue
// ---------------------------------------------------------------------------

/// What happened when a binding was looked up in the catalogue (§10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The id exists and is current.
    Current(ModelInfo),
    /// The id exists but is retired, with a suggested successor. The UI says
    /// so rather than letting the request 400.
    Retired {
        /// The retired entry.
        model: ModelInfo,
        /// Successor id, when the provider named one.
        replacement: Option<String>,
    },
    /// The id is not in the catalogue at all. Not automatically an error — the
    /// catalogue may simply be stale, and `Provider::models()` is the runtime
    /// fallback (§10).
    Unknown,
}

/// The shipped model catalogue.
///
/// §10: this arrives in the same signed weekly manifest as the AX quirks table
/// (§19), with `Provider::models()` as the runtime fallback.
#[derive(Debug, Clone, Default)]
pub struct ModelCatalogue {
    entries: Vec<ModelInfo>,
}

impl ModelCatalogue {
    /// Build from a list of entries.
    pub fn new(entries: Vec<ModelInfo>) -> Self {
        Self { entries }
    }

    /// The catalogue aibo ships for providers that publish none.
    ///
    /// Only Codex, for now. Every other provider in §10 has a working `/models`
    /// endpoint, so [`ModelCatalogue::merge`] fills those in at runtime and
    /// hardcoding them here would only add rot. The Codex endpoint has no
    /// catalogue at all — `CodexProvider::models()` returns an empty list by
    /// design — so §3a's measured allowlist is the only source there is.
    ///
    /// Both halves of §3a's measurement are included: the working ids as live
    /// entries, and the refused ids as retired ones carrying a successor, so
    /// [`ModelCatalogue::resolve`] can explain an API-style id instead of
    /// letting it 400.
    pub fn shipped() -> Self {
        let mut entries = codex_models();
        entries.extend(codex_rejected_models());
        Self::new(entries)
    }

    /// Merge in entries fetched at runtime, without letting them delete shipped
    /// ones: a provider whose `/models` call fails must not empty the picker.
    pub fn merge(&mut self, fetched: Vec<ModelInfo>) {
        for model in fetched {
            match self
                .entries
                .iter_mut()
                .find(|e| e.provider == model.provider && e.id == model.id)
            {
                Some(existing) => *existing = model,
                None => self.entries.push(model),
            }
        }
    }

    /// Every entry.
    pub fn entries(&self) -> &[ModelInfo] {
        &self.entries
    }

    /// Look a binding up.
    pub fn resolve(&self, binding: &ModelBinding) -> Resolution {
        match self
            .entries
            .iter()
            .find(|e| e.provider == binding.provider && e.id == binding.model)
        {
            Some(m) if m.deprecated => Resolution::Retired {
                model: m.clone(),
                replacement: m.replaced_by.clone().or_else(|| self.closest(binding)),
            },
            Some(m) => Resolution::Current(m.clone()),
            None => Resolution::Unknown,
        }
    }

    /// Whether the catalogue knows this binding accepts image input.
    ///
    /// `None` means *the catalogue has never heard of this id* — which is not
    /// the same as "it cannot see" and must not be rendered as one. §10 makes
    /// `Provider::models()` the runtime fallback for exactly this case, and
    /// `Capabilities::default()` supplies the conservative floor once an entry
    /// does exist. Returning `Option` rather than `bool` is what stops a stale
    /// catalogue from silently becoming a refusal.
    pub fn supports_vision(&self, binding: &ModelBinding) -> Option<bool> {
        self.entries
            .iter()
            .find(|e| e.provider == binding.provider && e.id == binding.model)
            .map(|e| e.capabilities.vision)
    }

    /// Every live `provider/model` in the catalogue that declares image input.
    ///
    /// Feeds [`AiboError::VisionUnsupported::alternatives`] so the panel's one
    /// §13 action can offer a model that actually works. Deprecated entries are
    /// excluded: suggesting a retired id trades one dead end for another.
    ///
    /// Complements `RoleBindings::vision_alternatives`, which answers the same
    /// question from §4's *configured* chain. This one answers it from what the
    /// catalogue knows exists, which is the better list when the user has a
    /// working provider but has bound a text-only model on it.
    pub fn vision_alternatives(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| !e.deprecated && e.capabilities.vision)
            .map(|e| format!("{}/{}", e.provider, e.id))
            .collect()
    }

    /// The closest live model from the same provider, by shared id prefix.
    ///
    /// Crude on purpose: it only has to produce *a* sensible suggestion for the
    /// "here's the closest" message, and a cleverer ranking would be a guess
    /// dressed up as a recommendation.
    pub fn closest(&self, binding: &ModelBinding) -> Option<String> {
        self.entries
            .iter()
            .filter(|e| e.provider == binding.provider && !e.deprecated)
            .max_by_key(|e| shared_prefix(&e.id, &binding.model))
            .map(|e| e.id.clone())
    }
}

fn shared_prefix(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;
    use aibo_core::types::Capabilities;

    fn model(id: &str, deprecated: bool, replaced_by: Option<&str>) -> ModelInfo {
        ModelInfo {
            provider: ProviderId::CEREBRAS,
            id: id.to_string(),
            display_name: id.to_string(),
            capabilities: Capabilities::default(),
            deprecated,
            replaced_by: replaced_by.map(str::to_string),
        }
    }

    fn binding(id: &str) -> ModelBinding {
        ModelBinding {
            provider: ProviderId::CEREBRAS,
            model: id.to_string(),
        }
    }

    #[test]
    fn a_retired_model_reports_its_successor_instead_of_a_400() {
        let cat = ModelCatalogue::new(vec![
            model("llama-3.1-8b", true, Some("llama-3.3-70b")),
            model("llama-3.3-70b", false, None),
        ]);
        assert_eq!(
            cat.resolve(&binding("llama-3.1-8b")),
            Resolution::Retired {
                model: model("llama-3.1-8b", true, Some("llama-3.3-70b")),
                replacement: Some("llama-3.3-70b".into()),
            }
        );
    }

    #[test]
    fn a_retired_model_without_a_stated_successor_gets_the_closest() {
        let cat = ModelCatalogue::new(vec![
            model("llama-3.1-8b", true, None),
            model("llama-3.3-70b", false, None),
            model("qwen-3-32b", false, None),
        ]);
        match cat.resolve(&binding("llama-3.1-8b")) {
            Resolution::Retired { replacement, .. } => {
                assert_eq!(replacement.as_deref(), Some("llama-3.3-70b"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unknown_id_is_not_an_error() {
        let cat = ModelCatalogue::new(vec![model("llama-3.3-70b", false, None)]);
        assert_eq!(cat.resolve(&binding("who-knows")), Resolution::Unknown);
    }

    #[test]
    fn a_failed_runtime_fetch_cannot_empty_the_catalogue() {
        let mut cat = ModelCatalogue::new(vec![model("llama-3.3-70b", false, None)]);
        cat.merge(Vec::new());
        assert_eq!(cat.entries().len(), 1);
    }

    #[test]
    fn an_empty_registry_is_the_blocking_case() {
        let r = ProviderRegistry::new();
        assert!(r.is_empty());
        assert!(matches!(
            r.for_binding(&binding("x")),
            Err(AiboError::NoProviderConfigured)
        ));
    }

    // -- the Codex allowlist (§3a) ------------------------------------------

    fn codex_binding(id: &str) -> ModelBinding {
        ModelBinding {
            provider: ProviderId::CODEX,
            model: id.to_string(),
        }
    }

    #[test]
    fn the_allowlist_is_the_measured_working_set() {
        let ids: Vec<&str> = CODEX_MODELS.iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            [
                "gpt-5.5",
                "gpt-5.6-terra",
                "gpt-5.3-codex-spark",
                "gpt-5.6-luna",
                "gpt-5.6-sol"
            ]
        );
        for id in ids {
            assert!(is_codex_model_allowed(id));
            assert!(check_codex_model(id).is_ok());
        }
    }

    #[test]
    fn a_rejected_id_is_refused_before_dispatch_with_the_constraint_named() {
        // §4 does not fall back on a 400, so this is the only chance to say
        // anything useful about it.
        for id in CODEX_REJECTED_MODELS {
            assert!(!is_codex_model_allowed(id), "{id}");
            let err = check_codex_model(id).unwrap_err();
            let AiboError::ModelRejected {
                provider,
                model,
                constraint,
                alternatives,
            } = &err
            else {
                panic!("{err:?}");
            };
            assert_eq!(provider, &ProviderId::CODEX);
            assert_eq!(model, id);
            assert!(constraint.contains(id), "{constraint}");
            assert!(
                constraint.contains(CODEX_ACCOUNT_CONSTRAINT),
                "{constraint}"
            );
            assert_eq!(
                alternatives,
                &codex_alternatives(),
                "the error must carry what to use instead, not just say so in prose"
            );
            assert!(alternatives.iter().all(|a| is_codex_model_allowed(a)));
        }
    }

    #[test]
    fn a_rejected_id_is_actionable_rather_than_something_went_wrong() {
        // The regression: `AiboError::Internal` is the one variant §13 renders
        // as a generic message plus "copy diagnostics", so wrapping the refusal
        // in it produced exactly the opaque error this module exists to
        // prevent — and §4 does not fall back on a 400, so it is terminal.
        let err = check_codex_model("gpt-5-codex").unwrap_err();
        assert!(
            !matches!(err, AiboError::Internal(_)),
            "must not be the generic-treatment variant"
        );
        assert!(
            err.source().is_none(),
            "the facts belong in typed fields, not in a source the UI drops"
        );
        assert_eq!(err.treatment(), aibo_core::error::Treatment::Inline);
        assert!(!err.is_fallback_eligible());
        assert!(!err.is_retryable(), "retrying the same binding cannot work");
    }

    #[test]
    fn the_registry_refuses_a_rejected_id_on_the_dispatch_path() {
        // Not a unit-test-only check: `for_binding` is the call every request
        // makes, which is the point of putting it there.
        let mut r = ProviderRegistry::new();
        // Any provider will do; the check runs before the lookup.
        r.insert(ProviderId::CODEX, Arc::new(NeverCalled));
        assert!(r.for_binding(&codex_binding("gpt-5.6-sol")).is_ok());
        match r.for_binding(&codex_binding("gpt-5-codex")) {
            Err(AiboError::ModelRejected {
                model,
                alternatives,
                ..
            }) => {
                assert_eq!(model, "gpt-5-codex");
                assert!(!alternatives.is_empty());
            }
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("a rejected id must not resolve to a provider"),
        }
    }

    #[test]
    fn a_rejected_codex_binding_never_becomes_a_fallback() {
        // §4: a 400 is a bug in aibo and must surface as one rather than being
        // retried elsewhere — which would both hide the bug and spend the
        // user's money twice (§14).
        let err = check_codex_model("codex-mini-latest").unwrap_err();
        assert!(!err.is_fallback_eligible());
    }

    #[test]
    fn an_unmeasured_id_is_unknown_rather_than_rejected() {
        // The allowlist is one day's measurement and OpenAI adds ids. Refusing
        // an id that would have worked is its own bug, so only the ids measured
        // as refused are refused.
        assert!(check_codex_model("gpt-5.7-something").is_ok());
        assert!(!is_codex_model_allowed("gpt-5.7-something"));
    }

    #[test]
    fn no_id_is_on_both_lists() {
        for m in CODEX_MODELS {
            assert!(!CODEX_REJECTED_MODELS.contains(&m.id), "{}", m.id);
        }
    }

    #[test]
    fn the_measured_floor_disqualifies_codex_from_fast() {
        // §3a's consequence for §4, as an assertion rather than a comment: the
        // fastest id on the whole allowlist still misses Complete's 250 ms
        // first-token target. `aibo-core::roles` enforces the binding rule; this
        // asserts the number it rests on.
        let fastest = CODEX_MODELS.iter().map(|m| m.ttft_p50_ms).min().unwrap();
        assert_eq!(fastest, CODEX_TTFT_FLOOR_MS);
        let complete_budget_ms = aibo_core::types::Surface::Complete
            .first_token_target()
            .as_millis() as u32;
        assert!(
            fastest > complete_budget_ms,
            "{fastest} ms vs a {complete_budget_ms} ms budget"
        );
    }

    #[test]
    fn the_shipped_catalogue_explains_an_api_style_id_instead_of_letting_it_400() {
        let cat = ModelCatalogue::shipped();
        match cat.resolve(&codex_binding("gpt-5-codex")) {
            Resolution::Retired { replacement, .. } => {
                assert_eq!(replacement.as_deref(), Some("gpt-5.3-codex-spark"));
            }
            other => panic!("{other:?}"),
        }
        match cat.resolve(&codex_binding("gpt-5")) {
            Resolution::Retired { replacement, .. } => {
                assert_eq!(replacement.as_deref(), Some("gpt-5.5"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            cat.resolve(&codex_binding("gpt-5.6-sol")),
            Resolution::Current(_)
        ));
    }

    #[test]
    fn the_shipped_catalogue_carries_the_endpoints_own_capabilities() {
        let cat = ModelCatalogue::shipped();
        let Resolution::Current(m) = cat.resolve(&codex_binding("gpt-5.5")) else {
            panic!("gpt-5.5 must be current");
        };
        assert_eq!(m.capabilities, crate::codex::default_capabilities());
        assert!(!m.deprecated);
    }

    #[test]
    fn a_runtime_fetch_cannot_delete_the_codex_allowlist() {
        // `CodexProvider::models()` returns an empty list by design — the
        // endpoint publishes no catalogue — so the shipped entries have to
        // survive a merge of nothing.
        let mut cat = ModelCatalogue::shipped();
        let before = cat.entries().len();
        cat.merge(Vec::new());
        assert_eq!(cat.entries().len(), before);
    }

    // -- vision truthfulness (§10) ------------------------------------------

    #[test]
    fn every_codex_allowlist_entry_declares_no_vision() {
        // SPIKE: §3a measured text only. Declaring `vision: true` here would
        // cost a 400 after a multi-megabyte upload, and §4 does not fall back on
        // a 400 — so the honest value is the unverified one.
        // The constant and the provider's own declaration are two statements of
        // one fact; a probe that verifies vision has to flip both.
        assert_eq!(
            CODEX_VISION_UNVERIFIED,
            !crate::codex::default_capabilities().vision
        );
        for m in codex_models() {
            assert!(
                !m.capabilities.vision,
                "{} must not claim unmeasured image input",
                m.id
            );
        }
    }

    #[test]
    fn the_shipped_catalogue_offers_no_vision_alternative_it_cannot_stand_behind() {
        // Codex is the only shipped catalogue, and none of it can see. An empty
        // list is the correct answer; a populated one would be an invention.
        assert!(ModelCatalogue::shipped().vision_alternatives().is_empty());
    }

    #[test]
    fn vision_alternatives_are_live_entries_only() {
        let seeing = Capabilities {
            vision: true,
            ..Capabilities::default()
        };
        let cat = ModelCatalogue::new(vec![
            ModelInfo {
                capabilities: seeing.clone(),
                ..model("sees", false, None)
            },
            ModelInfo {
                capabilities: seeing,
                ..model("saw", true, Some("sees"))
            },
            model("blind", false, None),
        ]);
        // A retired id is not an alternative: it trades one dead end for another.
        assert_eq!(cat.vision_alternatives(), ["cerebras/sees"]);
    }

    #[test]
    fn an_id_the_catalogue_never_heard_of_is_unknown_not_blind() {
        // The distinction the `Option` exists for: §10 makes `Provider::models()`
        // the runtime fallback for a stale catalogue, so `None` must not be
        // rendered as "this model cannot see".
        let cat = ModelCatalogue::new(vec![model("blind", false, None)]);
        assert_eq!(cat.supports_vision(&binding("blind")), Some(false));
        assert_eq!(cat.supports_vision(&binding("who-knows")), None);
    }

    /// A `Provider` that panics if anything actually dispatches to it. The
    /// allowlist check must fire before the provider is ever touched.
    #[derive(Debug)]
    struct NeverCalled;

    #[async_trait::async_trait]
    impl Provider for NeverCalled {
        fn id(&self) -> ProviderId {
            ProviderId::CODEX
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn chat(
            &self,
            _req: aibo_core::types::ChatRequest,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<aibo_core::types::BoxStream<'static, Result<aibo_core::types::StreamEvent>>>
        {
            unreachable!("a rejected model must never reach the wire")
        }
        async fn models(&self) -> Result<Vec<ModelInfo>> {
            Ok(Vec::new())
        }
        async fn health(&self) -> Result<aibo_core::types::Health> {
            unreachable!()
        }
    }

    #[test]
    fn groq_and_xai_are_distinct_ids() {
        // §10 ships both and disambiguates them in settings; a single "grok"
        // id would make that impossible.
        assert_ne!(
            default_id_for(&ProviderKind::Groq),
            default_id_for(&ProviderKind::Xai)
        );
    }
}
