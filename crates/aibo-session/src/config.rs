//! The config type the rest of the workspace was blocked on.
//!
//! `src/main.rs` carried this note before this crate existed:
//!
//! > *"`ProviderRegistry::from_specs` needs a config type and `aibo-core`
//! > defines none yet. Until it does the registry is empty, which §13 renders
//! > as the blocking 'no provider configured' state."*
//!
//! It lives here rather than in `aibo-core` because building a
//! [`ProviderSpec`] means naming [`aibo_provider::ProviderKind`], and §6 keeps
//! `aibo-core` free of network dependencies. `aibo-session` is the first crate
//! that legitimately depends on both.
//!
//! ## Secrets are not in this file
//!
//! Provider credentials live in separate credential files, while settings live
//! in plaintext TOML. So the TOML names a provider and its *shape*, and a
//! [`CredentialSource`] resolves the secret separately. A config file that
//! contained an API key would end up in a support bundle, a screenshot, or a
//! dotfiles repo.
//!
//! ## What is deliberately not supported yet
//!
//! `ProviderKind::Vertex`, `ProviderKind::Bedrock` and Entra ID need a live
//! [`aibo_core::types::TokenProvider`] — a service-account JWT exchange, a
//! SigV4 credential chain — that no TOML file can express. Rather than pretend,
//! [`Backend`] simply does not name them: the caller builds those
//! [`ProviderSpec`]s itself and inserts them into the registry
//! [`Config::build`] returns. An unrecognised `backend = "vertex"` is then a
//! parse error naming the field, which is a better failure than a provider that
//! constructs and 401s on first use.
//!
//! ## Codex is the exception, and it had to stop being one
//!
//! Codex was on that list for the same reason — a device-code
//! [`aibo_core::types::TokenProvider`] is not expressible in TOML — and the
//! consequence was that **nothing ever built one**. `aibo-provider`'s verified
//! device flow, its `CodexProvider` and `registry::build`'s `ProviderKind::Codex`
//! arm were all reachable only from their own tests, so the one credential a
//! ChatGPT-subscription user actually has could not configure the app.
//!
//! The seam that fixes it is [`Config::build_with_codex`]: the TOML carries
//! [`CodexConfig`] — *enabled*, the chosen model, and the OAuth client id, which
//! are settings, not secrets — and the caller passes the live
//! [`RefreshingTokenProvider`] it built over credential storage. Tokens never touch
//! this file (§12), and `enabled = true` with no token provider is
//! [`ConfigError::CodexNotSignedIn`] rather than a registry that silently lacks
//! the provider the user turned on.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use aibo_core::cost::{MonthlyBudget, PriceTable, ProviderTier};
use aibo_core::roles::{Availability, RoleBindings};
use aibo_core::types::{Credential, ModelBinding, ProviderId, Role, RoleChain};
use aibo_provider::ProviderRegistry;
use aibo_provider::auth::RefreshingTokenProvider;
use aibo_provider::codex::AttestationPolicy;
use aibo_provider::registry::{ProviderKind, ProviderSpec, check_codex_model};
use secrecy::SecretString;
use serde::Deserialize;
use url::{Host, Url};

use crate::engine::{DEFAULT_MAX_PAYLOAD_CHARS, DEFAULT_REQUEST_DEADLINE, EngineConfig};
use crate::health::{
    DEFAULT_DEGRADE_AFTER, DEFAULT_FIRST_PROBE_AFTER, DEFAULT_MAX_PROBE_BACKOFF, HysteresisPolicy,
};
use crate::trust::{TrustBoundary, TrustMap};

/// Why a configuration could not be turned into a running engine.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("could not read {path}")]
    Io {
        /// The path.
        path: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// The file is not valid TOML, or does not match the schema.
    #[error("could not parse the configuration")]
    Parse(#[source] toml::de::Error),

    /// `base_url` was not a URL.
    #[error("provider `{provider}` has an invalid base_url")]
    InvalidBaseUrl {
        /// The provider.
        provider: String,
        /// The parse failure.
        #[source]
        source: url::ParseError,
    },

    /// A provider was named with no credential available.
    #[error("no API key for `{provider}`; store one in aibo's credential files or set {env}")]
    MissingCredential {
        /// The provider.
        provider: String,
        /// The environment variable [`EnvCredentials`] would have read.
        env: String,
    },

    /// A kind that requires a base URL was given none.
    #[error("provider `{provider}` requires `base_url`")]
    MissingBaseUrl {
        /// The provider.
        provider: String,
    },

    /// A role name that is not one of §4's five.
    #[error("`{name}` is not a role; expected fast, smart, cheap, vision or agent")]
    UnknownRole {
        /// What the file said.
        name: String,
    },

    /// `[codex] enabled = true`, but the caller supplied no token provider.
    ///
    /// Deliberately an error rather than a silent skip. `registry.rs` states
    /// the rule for the same reason: *"a provider that silently vanished from
    /// the registry becomes an unexplained 'no provider configured' later"* —
    /// and for Codex specifically that reads as "aibo forgot I signed in",
    /// which is the failure this whole seam exists to prevent.
    #[error(
        "codex is enabled but no ChatGPT sign-in was supplied; sign in from Settings → Providers"
    )]
    CodexNotSignedIn,

    /// The domain layer rejected the result — §4's `Fast`-never-Codex rule is
    /// the one that fires in practice.
    #[error(transparent)]
    Rejected(#[from] aibo_core::AiboError),
}

/// Where an API key comes from.
///
/// The engine never reads a secret itself; credentials are resolved through
/// this seam. The binary implements a file-backed source over
/// `aibo_store::SecretStorage`; tests and headless runs use
/// [`EnvCredentials`].
pub trait CredentialSource: Send + Sync {
    /// The API key for a provider, if one is available.
    fn api_key(&self, provider: &ProviderId) -> Option<SecretString>;
}

/// Reads `AIBO_<PROVIDER>_API_KEY`, upper-cased with `-` mapped to `_`.
///
/// For CI, the eval harness and a first run before anything has been stored in
/// credential storage.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvCredentials;

impl EnvCredentials {
    /// The variable name consulted for a provider.
    pub fn var_name(provider: &ProviderId) -> String {
        format!(
            "AIBO_{}_API_KEY",
            provider.as_str().to_uppercase().replace('-', "_")
        )
    }
}

impl CredentialSource for EnvCredentials {
    fn api_key(&self, provider: &ProviderId) -> Option<SecretString> {
        std::env::var(Self::var_name(provider))
            .ok()
            .filter(|v| !v.is_empty())
            .map(SecretString::from)
    }
}

/// A [`CredentialSource`] that knows nothing. Local-only setups (Ollama) need
/// no key at all, and this makes that explicit rather than accidental.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCredentials;

impl CredentialSource for NoCredentials {
    fn api_key(&self, _provider: &ProviderId) -> Option<SecretString> {
        None
    }
}

/// Which backend a configured provider is, in the vocabulary a TOML file can
/// express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// Cerebras (§10 ultra-fast).
    Cerebras,
    /// SambaNova.
    SambaNova,
    /// Groq. Not xAI.
    Groq,
    /// xAI / Grok. Not Groq.
    Xai,
    /// OpenRouter: one key fronting many upstream vendors.
    OpenRouter,
    /// Google Gemini on the direct Generative Language API.
    Gemini,
    /// OpenAI on its native Responses format.
    OpenAi,
    /// OpenAI on Chat Completions, for deployments that need it.
    OpenAiChatCompletions,
    /// Anthropic native `messages`.
    Anthropic,
    /// Azure OpenAI with a key. Managed identity needs a token provider.
    Azure,
    /// Ollama / llama.cpp. No auth.
    Ollama,
    /// A user-added OpenAI-compatible endpoint (§10: the provider set is open).
    Custom,
}

impl Backend {
    fn default_id(self) -> ProviderId {
        match self {
            Self::Cerebras => ProviderId::CEREBRAS,
            Self::SambaNova => ProviderId::SAMBANOVA,
            Self::Groq => ProviderId::GROQ,
            Self::Xai => ProviderId::XAI,
            Self::OpenRouter => ProviderId::OPENROUTER,
            Self::Gemini => ProviderId::GEMINI,
            Self::OpenAi | Self::OpenAiChatCompletions => ProviderId::OPENAI,
            Self::Anthropic => ProviderId::ANTHROPIC,
            Self::Azure => ProviderId::AZURE_OPENAI,
            Self::Ollama => ProviderId::OLLAMA,
            Self::Custom => ProviderId::new("custom"),
        }
    }

    /// Whether this backend authenticates with a plain API key.
    const fn wants_api_key(self) -> bool {
        !matches!(self, Self::Ollama)
    }
}

/// One `[[providers]]` entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Address this provider by a custom id — needed to configure two Ollama
    /// endpoints, or two custom ones.
    #[serde(default)]
    pub id: Option<String>,
    /// Which backend.
    pub backend: Backend,
    /// Base URL. Required for `azure`, `custom` and a non-default Ollama.
    ///
    /// A string rather than a [`Url`]: `url`'s `serde` feature is not enabled
    /// workspace-wide, and parsing here gives a better error than serde's.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Azure deployment name (part of the URL, not a model id).
    #[serde(default)]
    pub deployment: Option<String>,
    /// Azure `api-version`. It matters (§10).
    #[serde(default)]
    pub api_version: Option<String>,
    /// Pricing tier, when the account is on a non-standard one (§14).
    #[serde(default)]
    pub tier: Option<String>,
    /// Override the §14 trust classification. Set `private` for a self-hosted
    /// endpoint aibo cannot recognise.
    #[serde(default)]
    pub trust: Option<Trust>,
}

/// §14's privacy classification, as spelled in TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trust {
    /// The user's own machine or infrastructure they administer.
    Private,
    /// A shared multi-tenant API.
    Public,
}

impl From<Trust> for TrustBoundary {
    fn from(t: Trust) -> Self {
        match t {
            Trust::Private => Self::Private,
            Trust::Public => Self::Public,
        }
    }
}

/// One `(provider, model)` entry in a role chain.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryConfig {
    /// Provider id.
    pub provider: String,
    /// Wire model id.
    pub model: String,
}

/// One `[roles.<name>]` table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RoleConfig {
    /// Ordered candidates; the first is the primary.
    pub entries: Vec<EntryConfig>,
    /// §14: fallback is opt-in per role, because a silent retry can
    /// double-spend and can send the user's text somewhere they did not
    /// choose.
    pub fallback: bool,
    /// §14: and crossing from a provider the user administers to one they do
    /// not needs its own consent.
    pub allow_crossing_trust_boundary: bool,
}

/// The `[budget]` table (§14).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    /// Monthly ceiling, in millionths of a currency unit.
    pub limit_micros: u64,
    /// Warn at this percentage. §14 says 80.
    #[serde(default = "default_warn_at")]
    pub warn_at_percent: u8,
    /// Refuse new requests past the limit. Off by default (§14).
    #[serde(default)]
    pub hard_stop: bool,
}

const fn default_warn_at() -> u8 {
    80
}

// ---------------------------------------------------------------------------
// Codex (§3a)
// ---------------------------------------------------------------------------

/// The model bound to Codex when the user has expressed no preference.
///
/// §3a measured the whole ChatGPT-plan allowlist from Yokohama on a warm
/// connection: `gpt-5.5` is the **fastest at 435 ms** TTFT p50, ahead of
/// `gpt-5.6-terra` (446 ms) and well ahead of `gpt-5.6-sol` (623 ms), which is
/// what `aibo_core::roles::SMART_CHAIN` hardcodes. Prefill is negligible at this
/// scale — the same model measured 430 ms at ~900 prompt tokens — so the number
/// is fixed overhead and the ordering holds for real prompts.
///
/// The same measurement is why this default can never reach [`Role::Fast`]:
/// 435 ms is the *floor*, `Complete`'s budget is 250 ms, and
/// `aibo_core::roles` enforces the prohibition rather than documenting it (§4).
pub const DEFAULT_CODEX_MODEL: &str = "gpt-5.5";

/// The OAuth client id the device flow is run under (§3a).
///
/// **This is a posture decision, and it is recorded here deliberately.** §3a:
/// the device flow requires a client id, the only one that exists is Codex's
/// own from the OSS tree, and the consent screen the user is sent to is
/// literally `auth.openai.com/codex/device` — so the user authorises *Codex*
/// while the tokens go to aibo. §3a's own words: *"that's a materially
/// different posture from reusing a credential the user's own Codex already
/// minted, and it's the part to be deliberate about rather than the code."*
///
/// It is a default rather than a hardcode: `[codex] client_id` overrides it,
/// and so does `AIBO_CODEX_CLIENT_ID` (`aibo_provider::codex::CLIENT_ID_ENV_VAR`),
/// so the decision stays reversible without a release. It is **not a secret** —
/// it is published in `openai/codex` — so §12's "no credentials in this file"
/// rule does not reach it.
pub const DEFAULT_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The `[codex]` table (§3a).
///
/// Three settings and **no tokens**. Credentials live in separate files,
/// so what persists here is only the fact that the user signed in, which model
/// they chose, and — because it is a posture knob rather than a secret — the
/// OAuth client id.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CodexConfig {
    /// Whether the Codex provider is built at all.
    ///
    /// Written by the settings UI when a device-code login completes, and
    /// cleared on sign-out. `false` is the fresh-install state.
    pub enabled: bool,

    /// The wire model id, from §3a's verified allowlist.
    pub model: String,

    /// Overrides [`DEFAULT_CODEX_CLIENT_ID`].
    pub client_id: Option<String>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: DEFAULT_CODEX_MODEL.to_owned(),
            client_id: None,
        }
    }
}

impl CodexConfig {
    /// The client id to run the device flow under.
    ///
    /// Precedence: the config file, then `AIBO_CODEX_CLIENT_ID`, then
    /// [`DEFAULT_CODEX_CLIENT_ID`]. The env var comes second rather than first
    /// so an explicit setting always wins over ambient environment — the
    /// opposite order makes a machine-wide variable silently override what the
    /// user chose in settings.
    pub fn client_id(&self) -> String {
        self.client_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var(aibo_provider::codex::CLIENT_ID_ENV_VAR)
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| DEFAULT_CODEX_CLIENT_ID.to_owned())
    }
}

/// The `[health]` table (§13).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HealthConfig {
    /// Consecutive failures before a provider is degraded.
    pub degrade_after: u32,
    /// Seconds from degradation to the first re-probe.
    pub first_probe_after_secs: u64,
    /// Ceiling on the doubling probe backoff, in seconds.
    pub max_probe_backoff_secs: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            degrade_after: DEFAULT_DEGRADE_AFTER,
            first_probe_after_secs: DEFAULT_FIRST_PROBE_AFTER.as_secs(),
            max_probe_backoff_secs: DEFAULT_MAX_PROBE_BACKOFF.as_secs(),
        }
    }
}

/// aibo's on-disk configuration.
///
/// Written atomically by the settings UI (`paths::atomic_write` in the binary),
/// so a crash mid-write cannot leave unparseable TOML and a dead app on the
/// next launch (§6).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Configured providers.
    pub providers: Vec<ProviderConfig>,
    /// Role chains (§4). Empty means "seed §4's defaults for whatever is
    /// configured".
    pub roles: BTreeMap<String, RoleConfig>,
    /// The Codex subscription provider (§3a). Not a `[[providers]]` entry
    /// because its credential is a device-code token pair, not an API key.
    pub codex: CodexConfig,
    /// The monthly soft budget (§14).
    pub budget: Option<BudgetConfig>,
    /// Offline hysteresis (§13).
    pub health: HealthConfig,
    /// Non-secret desktop-shell preferences.
    pub ui: UiSettings,
    /// Wall-clock ceiling for one request, in seconds.
    pub request_deadline_secs: Option<u64>,
    /// §13's large-selection refusal, in characters.
    pub max_payload_chars: Option<usize>,
}

/// Persisted desktop-shell preferences.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UiSettings {
    /// BCP-47 language tag. Unsupported tags fall back in the UI layer.
    pub language: Option<String>,
    /// Let aibo switch on an application's accessibility tree so its content
    /// can be read (§8, macOS only).
    ///
    /// Off by default, and that default is a judgement rather than caution for
    /// its own sake. Chrome honours `AXEnhancedUserInterface` and Electron
    /// honours `AXManualAccessibility`; §8 records that the Chrome flag "breaks
    /// window positioning and makes resizing sluggish", so a tray utility
    /// setting it unasked degrades an app the user did not consent to have
    /// touched.
    ///
    /// Leaving it off has a cost the user should get to weigh: Chrome, Edge,
    /// Slack, VS Code, Discord and every other Electron app return **no
    /// context at all**. That is most of the surface this product is sold
    /// into, which is why this is a setting and not a permanent no.
    #[serde(default)]
    pub allow_ax_tree_activation: bool,
}

impl Config {
    /// Parse a configuration from TOML.
    pub fn from_toml_str(src: &str) -> Result<Self, ConfigError> {
        toml::from_str(src).map_err(ConfigError::Parse)
    }

    /// Read and parse the configuration file.
    ///
    /// A missing file is [`Config::default`], not an error: a fresh install has
    /// no config, and §13 already has a treatment for "no provider configured"
    /// that is better than refusing to start.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(src) => Self::from_toml_str(&src),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Build the registry and the engine configuration.
    ///
    /// `prices` is the §14 table, already loaded — the file lives beside the
    /// config and is user-updatable because prices change faster than releases.
    ///
    /// Equivalent to [`Config::build_with_codex`] with no token provider, so
    /// `[codex] enabled = true` is [`ConfigError::CodexNotSignedIn`] here. Use
    /// the other entry point from the binary, which owns credential storage.
    pub fn build(
        &self,
        credentials: &dyn CredentialSource,
        prices: PriceTable,
    ) -> Result<(ProviderRegistry, EngineConfig), ConfigError> {
        self.build_with_codex(credentials, prices, None)
    }

    /// Build the registry, inserting the Codex provider when the caller has a
    /// live device-code token provider for it (§3a).
    ///
    /// `codex_tokens` is the thing TOML cannot express and the reason this
    /// second entry point exists: a [`RefreshingTokenProvider`] over credential
    /// files, built by the binary. It is passed even when no token has been
    /// stored yet — construction touches neither the network nor storage,
    /// and `CodexProvider::health` then reports *"sign-in required"* instead of
    /// the provider being absent, which is the difference between a settings
    /// row the user can act on and a provider that does not exist.
    pub fn build_with_codex(
        &self,
        credentials: &dyn CredentialSource,
        prices: PriceTable,
        codex_tokens: Option<Arc<RefreshingTokenProvider>>,
    ) -> Result<(ProviderRegistry, EngineConfig), ConfigError> {
        let mut registry = ProviderRegistry::new();
        let mut trust = TrustMap::new();
        let mut tiers = BTreeMap::new();

        for provider in &self.providers {
            let id = provider
                .id
                .clone()
                .map(ProviderId::new)
                .unwrap_or_else(|| provider.backend.default_id());

            if let Some(t) = provider.trust {
                trust.set(id.clone(), t.into());
            } else if provider.backend == Backend::Ollama
                && provider
                    .base_url
                    .as_deref()
                    .and_then(|raw| Url::parse(raw).ok())
                    .is_some_and(|url| !Self::endpoint_is_loopback(&url))
            {
                // A remote Ollama is not automatically the user's machine.
                // Classify it conservatively unless the user explicitly marks
                // administered infrastructure as private.
                trust.set(id.clone(), TrustBoundary::Public);
            }
            if let Some(tier) = &provider.tier {
                tiers.insert(id.clone(), ProviderTier::new(tier.clone()));
            }

            let spec = self.spec_for(provider, &id, credentials)?;
            registry.insert(id, aibo_provider::registry::build(spec)?);
        }

        let codex_authenticated = self.insert_codex(&mut registry, codex_tokens)?;
        let bindings = self.bindings(&registry, codex_authenticated)?;

        Ok((
            registry,
            EngineConfig {
                bindings,
                prices,
                monthly_budget: self.budget.map(|b| MonthlyBudget {
                    limit_micros: b.limit_micros,
                    warn_at_percent: b.warn_at_percent,
                    hard_stop: b.hard_stop,
                }),
                hysteresis: HysteresisPolicy {
                    degrade_after: self.health.degrade_after.max(1),
                    first_probe_after: std::time::Duration::from_secs(
                        self.health.first_probe_after_secs.max(1),
                    ),
                    max_probe_backoff: std::time::Duration::from_secs(
                        self.health.max_probe_backoff_secs.max(1),
                    ),
                },
                trust,
                tiers,
                // The shipped half of §10's catalogue. A live
                // `Provider::models()` refresh merges over this once the
                // network has answered; until then these are the only per-model
                // capabilities there are, and an empty map here means every
                // binding silently inherits `Capabilities::default()` — an
                // 8 192-token context window applied to models with twenty-five
                // times that.
                catalogue: aibo_provider::ModelCatalogue::shipped().capabilities_by_binding(),
                do_verbs: aibo_core::router::DoVerbRegistry::builtin(),
                max_payload_chars: self.max_payload_chars.unwrap_or(DEFAULT_MAX_PAYLOAD_CHARS),
                request_deadline: self
                    .request_deadline_secs
                    .map(std::time::Duration::from_secs)
                    .unwrap_or(DEFAULT_REQUEST_DEADLINE),
            },
        ))
    }

    fn endpoint_is_loopback(url: &Url) -> bool {
        match url.host() {
            Some(Host::Ipv4(address)) => address.is_loopback(),
            Some(Host::Ipv6(address)) => address.is_loopback(),
            Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
            None => false,
        }
    }

    fn spec_for(
        &self,
        provider: &ProviderConfig,
        id: &ProviderId,
        credentials: &dyn CredentialSource,
    ) -> Result<ProviderSpec, ConfigError> {
        let base_url = provider
            .base_url
            .as_deref()
            .map(|raw| {
                Url::parse(raw).map_err(|source| ConfigError::InvalidBaseUrl {
                    provider: id.as_str().to_owned(),
                    source,
                })
            })
            .transpose()?;

        let credential = if provider.backend.wants_api_key() {
            let key = credentials
                .api_key(id)
                .ok_or_else(|| ConfigError::MissingCredential {
                    provider: id.as_str().to_owned(),
                    env: EnvCredentials::var_name(id),
                })?;
            match provider.backend {
                Backend::Azure => Credential::AzureKey {
                    key,
                    deployment: provider.deployment.clone().unwrap_or_default(),
                    api_version: provider
                        .api_version
                        .clone()
                        .unwrap_or_else(|| "2024-10-21".to_owned()),
                },
                _ => Credential::ApiKey(key),
            }
        } else {
            // §10/§13: a detected Ollama needs no credential, and the default
            // endpoint is the one `ollama serve` binds.
            Credential::LocalEndpoint(
                base_url
                    .clone()
                    .unwrap_or_else(|| Url::parse("http://127.0.0.1:11434").expect("static URL")),
            )
        };

        let kind = match provider.backend {
            Backend::Cerebras => ProviderKind::Cerebras,
            Backend::SambaNova => ProviderKind::SambaNova,
            Backend::Groq => ProviderKind::Groq,
            Backend::Xai => ProviderKind::Xai,
            Backend::OpenRouter => ProviderKind::OpenRouter,
            Backend::Gemini => ProviderKind::Gemini,
            Backend::OpenAi => ProviderKind::OpenAi,
            Backend::OpenAiChatCompletions => ProviderKind::OpenAiChatCompletions,
            Backend::Anthropic => ProviderKind::Anthropic,
            Backend::Azure => ProviderKind::Azure {
                deployment: provider.deployment.clone(),
                api_version: provider.api_version.clone(),
            },
            Backend::Ollama => ProviderKind::Ollama,
            Backend::Custom => ProviderKind::Custom {
                quirks: Box::new(aibo_provider::Quirks::chat_completions()),
            },
        };

        if matches!(provider.backend, Backend::Azure | Backend::Custom)
            && provider.base_url.is_none()
        {
            return Err(ConfigError::MissingBaseUrl {
                provider: id.as_str().to_owned(),
            });
        }

        Ok(ProviderSpec {
            id: Some(id.clone()),
            kind,
            base_url,
            credential,
        })
    }

    /// Insert the Codex provider, returning whether it is now available.
    ///
    /// The return value is `Availability::codex_authenticated` (§4). Without
    /// it, `RoleBindings::seed` drops the `Smart` chain's Codex entry —
    /// `Availability::configured` sets only `configured`, and Codex's
    /// precondition is `Authenticated` because *"authentication **is** the
    /// credential for Codex — there is no API key to configure separately"*.
    /// So a signed-in user with a Codex provider sitting in the registry would
    /// still have had every request routed past it.
    fn insert_codex(
        &self,
        registry: &mut ProviderRegistry,
        codex_tokens: Option<Arc<RefreshingTokenProvider>>,
    ) -> Result<bool, ConfigError> {
        if !self.codex.enabled {
            return Ok(false);
        }
        let tokens = codex_tokens.ok_or(ConfigError::CodexNotSignedIn)?;

        // §3a: API-style ids hard-400 with "not supported when using Codex with
        // a ChatGPT account", and §4 correctly does not fall back on a 400. Left
        // to dispatch that is an unrecoverable opaque error; caught here it is a
        // startup error naming the constraint and the ids that work.
        check_codex_model(&self.codex.model)?;

        let spec = ProviderSpec {
            id: Some(ProviderId::CODEX),
            kind: ProviderKind::Codex {
                // §3a / S6, executed end-to-end on 2026-07-26: the direct
                // endpoint returns 200 with device-code tokens and no
                // `x-oai-attestation`. `NotRequired` is the measured state and
                // is `AttestationPolicy`'s own default.
                attestation: AttestationPolicy::default(),
                tokens: tokens.clone(),
            },
            base_url: None,
            credential: Credential::ChatGptOAuth(tokens),
        };
        registry.insert(ProviderId::CODEX, aibo_provider::registry::build(spec)?);
        Ok(true)
    }

    fn bindings(
        &self,
        registry: &ProviderRegistry,
        codex_authenticated: bool,
    ) -> Result<RoleBindings, ConfigError> {
        if self.roles.is_empty() {
            // §4's shipped table, filtered to what is actually usable. A chain
            // whose primary is a provider the user never configured spends its
            // first request discovering that.
            let availability = Availability {
                codex_authenticated,
                ..Availability::configured(registry.ids())
            };
            let mut seeded = RoleBindings::seed(&availability);
            if codex_authenticated {
                self.retarget_codex_model(&mut seeded)?;
            }
            return Ok(seeded);
        }

        let mut chains = Vec::new();
        for (name, role_config) in &self.roles {
            let role = parse_role(name)?;
            let entries: Vec<ModelBinding> = role_config
                .entries
                .iter()
                .map(|e| ModelBinding {
                    provider: ProviderId::new(e.provider.clone()),
                    model: e.model.clone(),
                })
                .collect();
            // Same pre-dispatch refusal as the seeded path, applied to a chain
            // the user typed. §4's no-fallback-on-400 rule makes a rejected
            // Codex id a dead end wherever it came from.
            for binding in &entries {
                if binding.provider == ProviderId::CODEX {
                    check_codex_model(&binding.model)?;
                }
            }
            chains.push(RoleChain {
                role,
                entries,
                fallback_enabled: role_config.fallback,
                allow_crossing_trust_boundary: role_config.allow_crossing_trust_boundary,
            });
        }
        Ok(RoleBindings::from_chains(chains)?)
    }

    /// Point every seeded Codex entry at the model the user actually chose.
    ///
    /// `aibo_core::roles::SMART_CHAIN` hardcodes `gpt-5.6-sol`, the **slowest**
    /// id on §3a's allowlist at 623 ms. That is a defensible constant for a
    /// shipped table but it is not a setting, and §3a's measurement makes
    /// `gpt-5.5` (435 ms) the right default — a 188 ms saving on every `Smart`
    /// and `Ask` request, which is most of what a subscription user runs.
    ///
    /// `Role::Fast` is untouched on purpose: it has no Codex entry to retarget,
    /// and `RoleBindings::set_chain` re-validates, so an attempt to put one
    /// there fails rather than silently binding a 435 ms floor to a 250 ms
    /// budget (§4).
    fn retarget_codex_model(&self, bindings: &mut RoleBindings) -> Result<(), ConfigError> {
        for role in [
            Role::Fast,
            Role::Smart,
            Role::Cheap,
            Role::Vision,
            Role::Agent,
        ] {
            let Some(chain) = bindings.chain(role) else {
                continue;
            };
            let needs_change = chain
                .entries
                .iter()
                .any(|b| b.provider == ProviderId::CODEX && b.model != self.codex.model);
            if !needs_change {
                continue;
            }
            let mut chain = chain.clone();
            for binding in &mut chain.entries {
                if binding.provider == ProviderId::CODEX {
                    binding.model.clone_from(&self.codex.model);
                }
            }
            bindings.set_chain(chain)?;
        }
        Ok(())
    }
}

fn parse_role(name: &str) -> Result<Role, ConfigError> {
    match name.to_ascii_lowercase().as_str() {
        "fast" => Ok(Role::Fast),
        "smart" => Ok(Role::Smart),
        "cheap" => Ok(Role::Cheap),
        "vision" => Ok(Role::Vision),
        "agent" => Ok(Role::Agent),
        _ => Err(ConfigError::UnknownRole {
            name: name.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedKey;
    impl CredentialSource for FixedKey {
        fn api_key(&self, _provider: &ProviderId) -> Option<SecretString> {
            Some(SecretString::from("sk-test".to_string()))
        }
    }

    #[test]
    fn an_empty_config_is_valid_and_yields_no_providers() {
        let config = Config::from_toml_str("").unwrap();
        let (registry, engine) = config.build(&NoCredentials, PriceTable::empty()).unwrap();
        assert!(registry.is_empty());
        // §4: with nothing configured every role is empty, which §13 renders as
        // the blocking "no provider configured" state.
        assert_eq!(engine.bindings.unbound_roles().len(), 5);
    }

    #[test]
    fn a_provider_and_a_role_chain_round_trip() {
        let config = Config::from_toml_str(
            r#"
            [[providers]]
            backend = "cerebras"

            [[providers]]
            backend = "groq"

            [roles.fast]
            entries = [
              { provider = "cerebras", model = "llama-3.3-70b" },
              { provider = "groq", model = "llama-3.3-70b-versatile" },
            ]
            fallback = true
            "#,
        )
        .unwrap();

        let (registry, engine) = config.build(&FixedKey, PriceTable::empty()).unwrap();
        assert_eq!(registry.ids().len(), 2);
        let order = engine.bindings.dispatch_order(Role::Fast);
        assert_eq!(order.len(), 2, "fallback = true exposes the whole chain");
        assert_eq!(order[0].provider, ProviderId::CEREBRAS);
    }

    #[test]
    fn fallback_is_off_unless_asked_for() {
        let config = Config::from_toml_str(
            r#"
            [[providers]]
            backend = "cerebras"

            [roles.fast]
            entries = [
              { provider = "cerebras", model = "a" },
              { provider = "cerebras", model = "b" },
            ]
            "#,
        )
        .unwrap();
        let (_, engine) = config.build(&FixedKey, PriceTable::empty()).unwrap();
        assert_eq!(
            engine.bindings.dispatch_order(Role::Fast).len(),
            1,
            "§14: fallback is a spend and privacy decision, so it is opt-in"
        );
    }

    #[test]
    fn the_fast_never_codex_rule_is_enforced_on_a_user_config() {
        let config = Config::from_toml_str(
            r#"
            [roles.fast]
            entries = [{ provider = "codex", model = "gpt-5.5" }]
            "#,
        )
        .unwrap();
        let error = config
            .build(&NoCredentials, PriceTable::empty())
            .unwrap_err();
        assert!(matches!(error, ConfigError::Rejected(_)), "{error}");
    }

    #[test]
    fn a_missing_key_names_the_environment_variable() {
        let config = Config::from_toml_str("[[providers]]\nbackend = \"anthropic\"\n").unwrap();
        let error = config
            .build(&NoCredentials, PriceTable::empty())
            .unwrap_err();
        match error {
            ConfigError::MissingCredential { provider, env } => {
                assert_eq!(provider, "anthropic");
                assert_eq!(env, "AIBO_ANTHROPIC_API_KEY");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn ollama_needs_no_credential() {
        let config = Config::from_toml_str("[[providers]]\nbackend = \"ollama\"\n").unwrap();
        let (registry, _) = config.build(&NoCredentials, PriceTable::empty()).unwrap();
        assert!(registry.get(&ProviderId::OLLAMA).is_some());
    }

    #[test]
    fn azure_requires_a_base_url() {
        let config = Config::from_toml_str("[[providers]]\nbackend = \"azure\"\n").unwrap();
        assert!(matches!(
            config.build(&FixedKey, PriceTable::empty()),
            Err(ConfigError::MissingBaseUrl { .. })
        ));
    }

    #[test]
    fn an_unknown_role_is_named() {
        let config = Config::from_toml_str("[roles.turbo]\nentries = []\n").unwrap();
        assert!(matches!(
            config.build(&NoCredentials, PriceTable::empty()),
            Err(ConfigError::UnknownRole { .. })
        ));
    }

    // -- Codex (§3a) --------------------------------------------------------

    /// A token provider that never has a token. Enough for every assertion
    /// here: `RefreshingTokenProvider` reads nothing at construction, and none
    /// of these tests dispatches a request.
    fn offline_codex_tokens() -> Arc<RefreshingTokenProvider> {
        aibo_provider::codex::token_provider(
            "test-client".to_owned(),
            Arc::new(aibo_provider::auth::InMemoryTokenStore::default()),
            None,
        )
        .expect("a token provider constructs without touching the network")
    }

    /// **The blocker, as a test.** Before this seam existed, `Backend` named no
    /// Codex variant and no caller reached `registry.rs`'s `ProviderKind::Codex`
    /// arm — so a user whose only credential is a ChatGPT subscription could not
    /// produce a registry containing the provider at all.
    #[test]
    fn enabling_codex_puts_it_in_the_registry() {
        let config = Config::from_toml_str("[codex]\nenabled = true\n").unwrap();
        let (registry, _) = config
            .build_with_codex(
                &NoCredentials,
                PriceTable::empty(),
                Some(offline_codex_tokens()),
            )
            .unwrap();

        assert!(
            registry.get(&ProviderId::CODEX).is_some(),
            "codex was enabled and a token provider was supplied, so the registry must hold it"
        );
    }

    /// …and it must be *reachable*, not merely present. Codex's precondition is
    /// `Authenticated`, which `Availability::configured` never sets, so seeding
    /// used to drop the entry and route every request past a provider that was
    /// sitting right there.
    #[test]
    fn an_enabled_codex_is_bound_to_smart_not_just_registered() {
        let config = Config::from_toml_str("[codex]\nenabled = true\n").unwrap();
        let (_, engine) = config
            .build_with_codex(
                &NoCredentials,
                PriceTable::empty(),
                Some(offline_codex_tokens()),
            )
            .unwrap();

        let primary = engine
            .bindings
            .primary(Role::Smart)
            .expect("Smart must have a chain");
        assert_eq!(primary.provider, ProviderId::CODEX);
        assert!(
            !engine.bindings.unbound_roles().contains(&Role::Smart),
            "a signed-in subscription user must have a usable Smart role"
        );
    }

    /// §3a's measurement, applied. The shipped `SMART_CHAIN` hardcodes
    /// `gpt-5.6-sol` at 623 ms; the fastest id on the allowlist is `gpt-5.5` at
    /// 435 ms, and that is what a user who chose nothing gets.
    #[test]
    fn the_default_codex_model_is_the_fastest_measured_one() {
        assert_eq!(DEFAULT_CODEX_MODEL, "gpt-5.5");

        let config = Config::from_toml_str("[codex]\nenabled = true\n").unwrap();
        let (_, engine) = config
            .build_with_codex(
                &NoCredentials,
                PriceTable::empty(),
                Some(offline_codex_tokens()),
            )
            .unwrap();

        assert_eq!(
            engine.bindings.primary(Role::Smart).unwrap().model,
            "gpt-5.5",
            "§3a: gpt-5.5 is 435 ms against gpt-5.6-sol's 623 ms"
        );
        // The fastest allowlist entry is still 435 ms, so §4's prohibition
        // holds regardless of which model was chosen.
        assert!(
            engine
                .bindings
                .chain(Role::Fast)
                .is_none_or(|c| c.entries.iter().all(|b| b.provider != ProviderId::CODEX)),
            "§4: Fast must never bind Codex — the allowlist floor is 435 ms and \
             Complete's budget is 250 ms"
        );
    }

    #[test]
    fn a_chosen_codex_model_reaches_the_binding() {
        let config =
            Config::from_toml_str("[codex]\nenabled = true\nmodel = \"gpt-5.6-terra\"\n").unwrap();
        let (_, engine) = config
            .build_with_codex(
                &NoCredentials,
                PriceTable::empty(),
                Some(offline_codex_tokens()),
            )
            .unwrap();
        assert_eq!(
            engine.bindings.primary(Role::Smart).unwrap().model,
            "gpt-5.6-terra"
        );
    }

    /// §3a: API-style ids hard-400, and §4 does not fall back on a 400. Caught
    /// at build time the user gets the constraint and the working ids; left to
    /// dispatch they get an opaque dead end.
    #[test]
    fn an_api_style_codex_model_is_refused_before_the_first_request() {
        let config =
            Config::from_toml_str("[codex]\nenabled = true\nmodel = \"gpt-5-codex\"\n").unwrap();
        let error = config
            .build_with_codex(
                &NoCredentials,
                PriceTable::empty(),
                Some(offline_codex_tokens()),
            )
            .unwrap_err();
        match error {
            ConfigError::Rejected(aibo_core::AiboError::ModelRejected {
                model,
                alternatives,
                ..
            }) => {
                assert_eq!(model, "gpt-5-codex");
                assert!(alternatives.contains(&"gpt-5.5".to_owned()));
            }
            other => panic!("expected a ModelRejected naming the alternatives: {other}"),
        }
    }

    /// The same refusal for a chain the user typed in `[roles.*]`, which is the
    /// other way a Codex id reaches dispatch.
    #[test]
    fn an_api_style_codex_model_in_a_user_chain_is_refused_too() {
        let config = Config::from_toml_str(
            r#"
            [roles.smart]
            entries = [{ provider = "codex", model = "codex-mini-latest" }]
            "#,
        )
        .unwrap();
        assert!(matches!(
            config.build(&NoCredentials, PriceTable::empty()),
            Err(ConfigError::Rejected(
                aibo_core::AiboError::ModelRejected { .. }
            ))
        ));
    }

    /// `enabled = true` with nothing to authenticate with must say so, not
    /// quietly produce a registry missing the provider the user turned on.
    #[test]
    fn enabled_codex_without_a_token_provider_is_a_named_error() {
        let config = Config::from_toml_str("[codex]\nenabled = true\n").unwrap();
        assert!(matches!(
            config.build(&NoCredentials, PriceTable::empty()),
            Err(ConfigError::CodexNotSignedIn)
        ));
    }

    /// Signing out is `enabled = false`: the provider disappears and `Smart`
    /// falls back to whatever else is configured — here, nothing.
    #[test]
    fn a_disabled_codex_is_absent_even_when_tokens_are_supplied() {
        let config = Config::from_toml_str("[codex]\nenabled = false\n").unwrap();
        let (registry, engine) = config
            .build_with_codex(
                &NoCredentials,
                PriceTable::empty(),
                Some(offline_codex_tokens()),
            )
            .unwrap();
        assert!(registry.get(&ProviderId::CODEX).is_none());
        assert!(engine.bindings.unbound_roles().contains(&Role::Smart));
    }

    /// §12: the config file holds settings, never secrets. The `[codex]` table
    /// must therefore refuse to carry anything token-shaped rather than accept
    /// it and write it to a plaintext file that ends up in a support bundle.
    #[test]
    fn the_codex_table_cannot_carry_a_token() {
        for field in ["access_token", "refresh_token", "id_token", "token"] {
            let src = format!("[codex]\nenabled = true\n{field} = \"secret-value\"\n");
            assert!(
                matches!(Config::from_toml_str(&src), Err(ConfigError::Parse(_))),
                "`{field}` must not be an accepted key in [codex] (§12)"
            );
        }
    }

    /// The client id is a posture knob, not a secret (§3a). An explicit setting
    /// wins over the ambient environment; with neither, the published Codex id
    /// is the default — which is what makes sign-in work on a fresh install
    /// without the user editing anything.
    #[test]
    fn the_client_id_precedence_is_config_then_env_then_default() {
        let explicit = CodexConfig {
            client_id: Some("app_explicit".to_owned()),
            ..CodexConfig::default()
        };
        assert_eq!(explicit.client_id(), "app_explicit");

        // Blank is treated as absent rather than as an empty id, which
        // `DeviceAuthClient::new` would reject with `NoProviderConfigured`.
        let blank = CodexConfig {
            client_id: Some("   ".to_owned()),
            ..CodexConfig::default()
        };
        assert_eq!(
            blank.client_id(),
            std::env::var(aibo_provider::codex::CLIENT_ID_ENV_VAR)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_CODEX_CLIENT_ID.to_owned())
        );
        assert!(!DEFAULT_CODEX_CLIENT_ID.trim().is_empty());
    }

    #[test]
    fn a_trust_override_reaches_the_engine() {
        let config = Config::from_toml_str(
            r#"
            [[providers]]
            id = "self-hosted"
            backend = "custom"
            base_url = "https://llm.internal.example/v1"
            trust = "private"
            "#,
        )
        .unwrap();
        let (_, engine) = config.build(&FixedKey, PriceTable::empty()).unwrap();
        assert_eq!(
            engine.trust.boundary(&ProviderId::new("self-hosted")),
            TrustBoundary::Private
        );
    }

    #[test]
    fn remote_ollama_is_public_unless_explicitly_overridden() {
        let remote = Config::from_toml_str(
            r#"
            [[providers]]
            backend = "ollama"
            base_url = "https://ollama.example.test/v1"
            "#,
        )
        .unwrap();
        let (_, engine) = remote.build(&NoCredentials, PriceTable::empty()).unwrap();
        assert_eq!(
            engine.trust.boundary(&ProviderId::OLLAMA),
            TrustBoundary::Public
        );

        let administered = Config::from_toml_str(
            r#"
            [[providers]]
            backend = "ollama"
            base_url = "https://ollama.internal.example/v1"
            trust = "private"
            "#,
        )
        .unwrap();
        let (_, engine) = administered
            .build(&NoCredentials, PriceTable::empty())
            .unwrap();
        assert_eq!(
            engine.trust.boundary(&ProviderId::OLLAMA),
            TrustBoundary::Private
        );
    }
}
