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
use std::collections::BTreeSet;
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

const fn default_true() -> bool {
    true
}

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

    /// Two configured backends would occupy the same registry slot.
    #[error("provider id `{provider}` is configured more than once")]
    DuplicateProviderId {
        /// The colliding id.
        provider: String,
    },

    /// A custom id impersonates a different built-in backend.
    #[error("provider id `{provider}` is reserved for a built-in backend")]
    ReservedProviderId {
        /// The reserved id.
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

/// The [`ProviderId`] a `backend = "…"` string will be addressed by.
///
/// **The credential store must be keyed by this, not by the backend string.**
/// The two differ for `open-ai`, `open-router` and `samba-nova` — serde's
/// kebab-case spelling is not the provider's id — so storing a key under the
/// backend name files it where nothing will ever look. `Config::build` then
/// reports a missing credential and the provider is silently never constructed,
/// which from the settings window looks like "saving did nothing".
pub fn provider_id_for_backend(backend: &str) -> Option<ProviderId> {
    let parsed: Backend = serde::Deserialize::deserialize(serde::de::IntoDeserializer::<
        serde::de::value::Error,
    >::into_deserializer(backend))
    .ok()?;
    Some(parsed.default_id())
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

    /// The conservative default privacy classification for this concrete
    /// backend. This deliberately does not inspect the user-selectable id: an
    /// OpenAI-compatible endpoint called `vertex` is still a public custom
    /// endpoint unless the user explicitly says otherwise.
    fn default_trust(self, base_url: Option<&str>) -> TrustBoundary {
        match self {
            Self::Azure => TrustBoundary::Private,
            Self::Ollama => {
                let remote = base_url
                    .and_then(|raw| Url::parse(raw).ok())
                    .is_some_and(|url| !Config::endpoint_is_loopback(&url));
                if remote {
                    TrustBoundary::Public
                } else {
                    TrustBoundary::Private
                }
            }
            Self::Cerebras
            | Self::SambaNova
            | Self::Groq
            | Self::Xai
            | Self::OpenRouter
            | Self::Gemini
            | Self::OpenAi
            | Self::OpenAiChatCompletions
            | Self::Anthropic
            | Self::Custom => TrustBoundary::Public,
        }
    }
}

fn is_reserved_provider_id(id: &ProviderId) -> bool {
    matches!(
        id.as_str(),
        "cerebras"
            | "sambanova"
            | "groq"
            | "xai"
            | "openrouter"
            | "gemini"
            | "openai"
            | "anthropic"
            | "azure-openai"
            | "vertex"
            | "bedrock"
            | "ollama"
            | "codex"
    )
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
    /// Azure deployment name (part of the URL, not a model id). Omit it to
    /// use the `v1` surface instead, where `models` below is the catalogue.
    #[serde(default)]
    pub deployment: Option<String>,
    /// Azure `api-version`. It matters (§10) — classic wire only; the `v1`
    /// surface has none.
    #[serde(default)]
    pub api_version: Option<String>,
    /// Deployment names served through Azure's `v1` surface, each listed in
    /// the model picker as itself. The data plane publishes no deployment
    /// listing, so this statement is the catalogue (§10).
    #[serde(default)]
    pub models: Option<Vec<String>>,
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
    /// Pinned quick-pick models (`[pins]`).
    pub pins: PinsSettings,
    /// The `@` file finder's search roots (`[files]`).
    pub files: FilesSettings,
    /// Dictation source selection (`[stt]`).
    pub stt: SttSettings,
    /// Wall-clock ceiling for one request, in seconds.
    pub request_deadline_secs: Option<u64>,
    /// §13's large-selection refusal, in characters.
    pub max_payload_chars: Option<usize>,
}

/// Persisted quick-pick pins (`[pins]`).
///
/// `Some` — including `Some(vec![])` — means the user has curated the set and
/// the derived defaults must stay out of the way; absent means never touched.
/// A pin is "a deliberate statement about what the user works with, and losing
/// it on quit would make pinning pointless", which is why this exists.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PinsSettings {
    /// `provider/model` entries — the same spelling role chains use.
    pub models: Option<Vec<String>>,
}

/// The `@` file finder's search roots (`[files]`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FilesSettings {
    /// Directories the finder indexes. Absent means the platform defaults
    /// (Documents, Desktop, Downloads under the home directory).
    pub roots: Option<Vec<String>>,
}

/// Dictation source selection (`[stt]`).
///
/// `backend` is a plain string so the config file stays hand-editable and an
/// unknown value degrades to the default rather than failing the whole parse:
/// `"auto"` (absent), `"openai"` — the realtime API with the OpenAI key — or
/// `"chatgpt"` — the ChatGPT plan's transcription endpoint via the Codex
/// sign-in (owner request, 2026-08-02).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SttSettings {
    /// `"auto"`, `"openai"`, `"chatgpt"` or `"azure"`. Absent means auto.
    pub backend: Option<String>,
    /// End the dictation turn when the message is sent (owner, 2026-08-03).
    /// Absent means true: a microphone that keeps typing into the next
    /// composer after ⏎ is the surprising behaviour, not the default.
    #[serde(default)]
    pub end_on_send: Option<bool>,
    /// Azure Foundry dictation (owner request, 2026-08-03).
    #[serde(default)]
    pub azure: AzureSttSettings,
}

/// Where the `azure` dictation backend points.
///
/// Everything optional, because the easy path needs nothing here: the
/// endpoint falls back to the first `[[providers]]` azure entry's `base_url`
/// (one resource serves chat and STT alike), and the deployment names default
/// to the models' own names — which is what the portal suggests.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AzureSttSettings {
    /// The Foundry resource endpoint, when it differs from the chat entry's.
    pub endpoint: Option<String>,
    /// Realtime deployment name. Defaults to `gpt-live-transcribe`.
    pub live_deployment: Option<String>,
    /// Batch-fallback deployment name. Defaults to `gpt-transcribe`.
    pub deployment: Option<String>,
}

/// Persisted desktop-shell preferences.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UiSettings {
    /// BCP-47 language tag. Unsupported tags fall back in the UI layer.
    pub language: Option<String>,
    /// Model chosen in the panel quick-pick, as `provider/model`.
    ///
    /// Kept separate from `[codex].model`: the picker covers every configured
    /// provider, including user-named Azure deployments.
    #[serde(default)]
    pub selected_model: Option<String>,
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
    /// Override the panel hotkey, e.g. `"control+alt+Space"`.
    ///
    /// The syntax is what `aibo_ui::hotkey::parse` accepts — modifiers
    /// from `control`, `alt`, `shift`, `super`, joined by `+`, then one key
    /// code. An unparseable value is reported and ignored rather than fatal;
    /// refusing to start over a typo in a shortcut is the same lockout by
    /// another route.
    #[serde(default)]
    pub panel_hotkey: Option<String>,
    /// Override the screen-region capture hotkey.
    #[serde(default)]
    pub screen_capture_hotkey: Option<String>,
    /// Global shortcut that brings the task window forward. Unbound by
    /// default because common editors already claim the obvious choices.
    #[serde(default)]
    pub show_tasks_hotkey: Option<String>,
    /// `"dark"`, `"light"` or `"system"`. Absent means dark — the product
    /// default (§16) — so existing installs keep their look. The UI layer
    /// parses and applies it; an unknown value is reported and falls dark.
    #[serde(default)]
    pub appearance: Option<String>,
    /// Panel width in logical points, as last set by dragging the corner grip.
    ///
    /// Present only once the user has resized by hand; absent means the panel
    /// sizes itself from its content and the display, which is the default and
    /// stays the default. Both this and [`Self::panel_height`] are required
    /// before either is honoured — half a size is not a size.
    #[serde(default)]
    pub panel_width: Option<f32>,
    /// Panel height in logical points; see [`Self::panel_width`].
    #[serde(default)]
    pub panel_height: Option<f32>,
    /// Check the selected GitHub release stream at startup and once per day.
    /// Enabled by default so installed builds receive security and reliability
    /// fixes without requiring users to watch the releases page.
    #[serde(default = "default_true")]
    pub auto_update: bool,
    /// `"stable"` or `"nightly"`. An absent value is resolved from the
    /// embedded build version, so development builds stay on their stream.
    #[serde(default)]
    pub update_channel: Option<String>,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            language: None,
            selected_model: None,
            allow_ax_tree_activation: false,
            panel_hotkey: None,
            screen_capture_hotkey: None,
            show_tasks_hotkey: None,
            appearance: None,
            panel_width: None,
            panel_height: None,
            auto_update: true,
            update_channel: None,
        }
    }
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
        let mut failures: Vec<ConfigError> = Vec::new();
        let mut provider_ids = BTreeSet::new();

        for provider in &self.providers {
            let id = provider
                .id
                .clone()
                .map(ProviderId::new)
                .unwrap_or_else(|| provider.backend.default_id());

            if provider.id.is_some()
                && id != provider.backend.default_id()
                && is_reserved_provider_id(&id)
            {
                return Err(ConfigError::ReservedProviderId {
                    provider: id.to_string(),
                });
            }
            if !provider_ids.insert(id.clone()) {
                return Err(ConfigError::DuplicateProviderId {
                    provider: id.to_string(),
                });
            }

            // Always classify the concrete backend, then key that decision by
            // the configured id. Falling back to TrustMap's id-based shipped
            // table here lets a custom endpoint impersonate `vertex` or
            // `azure-openai` and silently cross a private -> public boundary.
            trust.set(
                id.clone(),
                provider.trust.map(Into::into).unwrap_or_else(|| {
                    provider.backend.default_trust(provider.base_url.as_deref())
                }),
            );
            if let Some(tier) = &provider.tier {
                tiers.insert(id.clone(), ProviderTier::new(tier.clone()));
            }

            // **One bad provider must not take the others down with it.**
            //
            // These two `?`s used to propagate, and `Bootstrap::config` treats a
            // failed build as "the configuration could not be applied" and
            // hands back an *empty* registry. So a single `[[providers]]` entry
            // whose key was missing disabled every other provider — including a
            // signed-in Codex, which needs no key at all. Adding a provider and
            // mistyping its key logged one line and silently unconfigured the
            // product.
            //
            // §13 wants a misconfiguration to "show up in settings rather than
            // on the first hotkey press", and §17's onboarding is built around
            // per-provider state. Skipping the entry gives both: the working
            // providers stay, and the broken one is absent from the registry,
            // which is exactly what the settings row renders as unconfigured.
            match self
                .spec_for(provider, &id, credentials)
                .and_then(|spec| aibo_provider::registry::build(spec).map_err(Into::into))
            {
                Ok(built) => {
                    registry.insert(id, built);
                }
                Err(error) => {
                    tracing::warn!(
                        provider = %id,
                        %error,
                        "a provider could not be built; continuing with the others"
                    );
                    // Kept, not discarded: if it turns out *nothing* could be
                    // built, this is the message the user needs — it names the
                    // provider and the environment variable to set. Reported
                    // below, once whether anything survived is known.
                    failures.push(error);
                }
            }
        }

        let codex_authenticated = self.insert_codex(&mut registry, codex_tokens)?;

        // **The rule, and why it is this one.**
        //
        // A provider that cannot be built must not disable the ones that can:
        // `Bootstrap::config` falls back to an empty registry on error, so
        // propagating unconditionally meant one `[[providers]]` entry with a
        // missing key unconfigured the whole product — including a signed-in
        // Codex, which needs no key at all. That was observed in the wild.
        //
        // But skipping unconditionally is also wrong. On a first run with a
        // single misconfigured provider, the error is the *only* thing that
        // tells the user which provider failed and which environment variable
        // to set; swallowing it leaves an app that starts and does nothing.
        //
        // So: report the failure when nothing at all survived, and skip when
        // something did. Checked after Codex is inserted, because Codex
        // surviving is precisely the case that must not be sacrificed.
        if registry.is_empty()
            && let Some(error) = failures.into_iter().next()
        {
            return Err(error);
        }
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
                models: provider.models.clone().unwrap_or_default(),
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
        let mut bindings = if self.roles.is_empty() {
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
            seeded
        } else {
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
            RoleBindings::from_chains(chains)?
        };
        self.apply_selected_model(&mut bindings, registry)?;
        Ok(bindings)
    }

    /// Put the quick-pick's explicit choice at the front of the roles that
    /// represent a user-selected general model. The provider must still exist
    /// in the built registry; a stale preference after provider removal is
    /// ignored rather than breaking startup.
    fn apply_selected_model(
        &self,
        bindings: &mut RoleBindings,
        registry: &ProviderRegistry,
    ) -> Result<(), ConfigError> {
        let Some(raw) = self.ui.selected_model.as_deref() else {
            return Ok(());
        };
        let Some((provider, model)) = raw.split_once('/') else {
            tracing::warn!(selected_model = raw, "ignoring malformed selected model");
            return Ok(());
        };
        if provider.is_empty() || model.is_empty() {
            tracing::warn!(selected_model = raw, "ignoring malformed selected model");
            return Ok(());
        }
        let selected = ModelBinding {
            provider: ProviderId::new(provider.to_owned()),
            model: model.to_owned(),
        };
        if registry.get(&selected.provider).is_none() {
            tracing::warn!(
                selected_model = raw,
                "selected model provider is not configured"
            );
            return Ok(());
        }
        if selected.provider == ProviderId::CODEX {
            check_codex_model(&selected.model)?;
        }

        for role in [Role::Smart, Role::Vision, Role::Agent] {
            let mut chain = bindings.chain(role).cloned().unwrap_or(RoleChain {
                role,
                entries: Vec::new(),
                fallback_enabled: false,
                allow_crossing_trust_boundary: false,
            });
            chain.entries.retain(|binding| binding != &selected);
            chain.entries.insert(0, selected.clone());
            bindings.set_chain(chain)?;
        }
        Ok(())
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
    /// A provider that cannot be built must not disable the ones that can.
    ///
    /// `Bootstrap::config` treats a failed build as "the configuration could not
    /// be applied" and falls back to an *empty* registry, so propagating here
    /// meant one `[[providers]]` entry with a missing key unconfigured the whole
    /// product — including a signed-in Codex, which needs no key at all.
    /// Observed in the wild: adding an OpenAI key whose secret was filed under
    /// the wrong account silently killed Codex.
    #[test]
    fn a_provider_with_no_credential_does_not_take_the_others_with_it() {
        let config = Config::from_toml_str(
            "[[providers]]\nbackend = \"groq\"\n\n[[providers]]\nbackend = \"open-ai\"\n",
        )
        .expect("valid toml");

        // Only Groq has a key; OpenAI's is missing.
        struct OnlyGroq;
        impl CredentialSource for OnlyGroq {
            fn api_key(&self, provider: &ProviderId) -> Option<secrecy::SecretString> {
                (provider.as_str() == "groq")
                    .then(|| secrecy::SecretString::from("gsk-test".to_owned()))
            }
        }

        let (registry, _) = config
            .build(&OnlyGroq, PriceTable::default())
            .expect("the build must succeed despite one unbuildable provider");

        let ids: Vec<String> = registry
            .ids()
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect();
        assert!(
            ids.contains(&"groq".to_owned()),
            "the working provider survives"
        );
        assert!(
            !ids.contains(&"openai".to_owned()),
            "the one with no credential is absent, which is what settings renders as unconfigured"
        );
    }

    /// The other half of the rule: with nothing left standing, the error is the
    /// only thing that tells a first-run user what to fix, so it must surface.
    #[test]
    fn a_lone_unbuildable_provider_still_reports_why() {
        let config =
            Config::from_toml_str("[[providers]]\nbackend = \"open-ai\"\n").expect("valid toml");

        let error = config
            .build(&NoCredentials, PriceTable::default())
            .expect_err("nothing could be built, so the reason must reach the caller");

        let message = error.to_string();
        assert!(
            message.contains("AIBO_OPENAI_API_KEY"),
            "the message must name the variable to set, got: {message}"
        );
    }

    /// The credential store is keyed by [`ProviderId`], not by the
    /// `backend = "…"` string, and for three backends those differ.
    ///
    /// Getting this wrong is silent in the worst way: the key is written, the
    /// config entry is written, `Save` reports nothing amiss, and then
    /// `Config::build` cannot find a credential for `openai` because the secret
    /// was filed under `open-ai`. The provider is never constructed and the
    /// settings window shows a row that does nothing.
    #[test]
    fn a_backend_string_maps_to_the_id_the_registry_uses() {
        // The three that differ — the whole reason this function exists.
        assert_eq!(
            provider_id_for_backend("open-ai")
                .as_ref()
                .map(ProviderId::as_str),
            Some("openai")
        );
        assert_eq!(
            provider_id_for_backend("open-router")
                .as_ref()
                .map(ProviderId::as_str),
            Some("openrouter")
        );
        assert_eq!(
            provider_id_for_backend("samba-nova")
                .as_ref()
                .map(ProviderId::as_str),
            Some("sambanova")
        );

        // And the ones that match, so a future rename cannot quietly break them.
        for backend in ["anthropic", "groq", "cerebras", "xai", "gemini", "ollama"] {
            assert_eq!(
                provider_id_for_backend(backend)
                    .as_ref()
                    .map(ProviderId::as_str),
                Some(backend),
                "{backend} should address itself"
            );
        }

        assert!(provider_id_for_backend("not-a-backend").is_none());
    }

    struct FixedKey;
    impl CredentialSource for FixedKey {
        fn api_key(&self, _provider: &ProviderId) -> Option<SecretString> {
            Some(SecretString::from("sk-test".to_string()))
        }
    }

    #[test]
    fn an_empty_config_is_valid_and_yields_no_providers() {
        let config = Config::from_toml_str("").unwrap();
        assert!(config.ui.auto_update, "fresh installs check for updates");
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
    fn a_selected_azure_deployment_becomes_the_general_model() {
        let config = Config::from_toml_str(
            r#"
            [ui]
            selected_model = "azure-openai/team-gpt"

            [[providers]]
            backend = "azure"
            base_url = "https://team.services.ai.azure.com"
            models = ["team-gpt"]
            "#,
        )
        .unwrap();

        let (_, engine) = config.build(&FixedKey, PriceTable::empty()).unwrap();
        for role in [Role::Smart, Role::Vision, Role::Agent] {
            assert_eq!(
                engine.bindings.primary(role),
                Some(&ModelBinding {
                    provider: ProviderId::AZURE_OPENAI,
                    model: "team-gpt".to_owned(),
                }),
                "{role:?} must use the explicit quick-pick choice"
            );
        }
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

    #[test]
    fn custom_provider_cannot_impersonate_a_private_builtin_id() {
        let config = Config::from_toml_str(
            r#"
            [[providers]]
            id = "vertex"
            backend = "custom"
            base_url = "https://shared.example.test/v1"
            "#,
        )
        .unwrap();

        assert!(matches!(
            config.build(&FixedKey, PriceTable::empty()),
            Err(ConfigError::ReservedProviderId { provider }) if provider == "vertex"
        ));
    }

    #[test]
    fn duplicate_provider_ids_are_rejected_before_registry_overwrite() {
        let config = Config::from_toml_str(
            r#"
            [[providers]]
            backend = "open-ai"

            [[providers]]
            backend = "open-ai-chat-completions"
            "#,
        )
        .unwrap();

        assert!(matches!(
            config.build(&FixedKey, PriceTable::empty()),
            Err(ConfigError::DuplicateProviderId { provider }) if provider == "openai"
        ));
    }

    #[test]
    fn a_custom_alias_is_public_by_backend_not_by_its_name() {
        let config = Config::from_toml_str(
            r#"
            [[providers]]
            id = "shared-edge"
            backend = "custom"
            base_url = "https://shared.example.test/v1"
            "#,
        )
        .unwrap();
        let (_, engine) = config.build(&FixedKey, PriceTable::empty()).unwrap();
        assert_eq!(
            engine.trust.boundary(&ProviderId::new("shared-edge")),
            TrustBoundary::Public
        );
    }
}
