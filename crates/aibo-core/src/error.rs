//! The failure model (§13).
//!
//! A tray app that fails silently is worse than one that crashes. Every error
//! aibo can produce is one variant of [`AiboError`], and every variant has a
//! fixed user-facing [`Treatment`] — that mapping is code, not convention, so
//! it cannot drift between surfaces.

use std::time::Duration;

use thiserror::Error;

use crate::types::{BudgetKind, ProviderId};

/// Convenience alias used throughout the workspace.
pub type Result<T, E = AiboError> = std::result::Result<T, E>;

/// Why an authentication attempt failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthKind {
    /// The token expired and refresh did not succeed.
    Expired,
    /// The credential was rejected as malformed or wrong.
    Invalid,
    /// The credential was revoked server-side.
    Revoked,
}

impl std::fmt::Display for AuthKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AuthKind::Expired => "expired",
            AuthKind::Invalid => "invalid",
            AuthKind::Revoked => "revoked",
        })
    }
}

/// Which phase of a request timed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeoutPhase {
    /// TCP/TLS connect.
    Connect,
    /// Connected, but no first token within the surface's budget (§1).
    FirstToken,
    /// The stream stalled after starting.
    Stream,
}

impl std::fmt::Display for TimeoutPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TimeoutPhase::Connect => "connect",
            TimeoutPhase::FirstToken => "first token",
            TimeoutPhase::Stream => "stream",
        })
    }
}

/// Why context capture failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureFailure {
    /// The app exposes no usable accessibility tree.
    NoAxTree,
    /// The OS permission is missing or the app blocked the read.
    Denied,
    /// An IME composition is active; reading returns text the user cannot see
    /// (§9), so aibo declines rather than guessing.
    ImeActive,
}

impl std::fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CaptureFailure::NoAxTree => "no accessibility tree",
            CaptureFailure::Denied => "denied",
            CaptureFailure::ImeActive => "IME composition active",
        })
    }
}

/// Why writing text back failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InsertFailure {
    /// The OS permission for synthetic events is missing, or secure input mode
    /// is enabled globally (§8).
    PermissionDenied,
    /// The target app swallowed or ignored the paste.
    AppRejected,
    /// An IME composition is active; pasting would corrupt the buffer (§9).
    ImeActive,
    /// The user cancelled, or the target changed between capture and insert (§8).
    Cancelled,
}

impl std::fmt::Display for InsertFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            InsertFailure::PermissionDenied => "permission denied",
            InsertFailure::AppRejected => "the app rejected the insert",
            InsertFailure::ImeActive => "IME composition active",
            InsertFailure::Cancelled => "cancelled",
        })
    }
}

/// Why sandboxed code execution stopped (§11 tier 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxFailure {
    /// Wall-clock limit reached. On the rquickjs path this is a cooperative
    /// interrupt handler, **not** deterministic fuel metering — fuel and epoch
    /// interruption are wasmtime concepts (§11).
    Timeout,
    /// Memory limit reached.
    OutOfMemory,
    /// The guest trapped.
    Trap,
}

impl std::fmt::Display for SandboxFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SandboxFailure::Timeout => "timed out",
            SandboxFailure::OutOfMemory => "out of memory",
            SandboxFailure::Trap => "trapped",
        })
    }
}

/// Every failure aibo can surface (§13).
///
/// Display strings are the *diagnostic* form. User-facing copy is chosen by the
/// UI from the variant plus its [`Treatment`]; never render `Display` straight
/// into the panel.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AiboError {
    /// No provider has been configured. The only error allowed to interrupt.
    #[error("no provider configured")]
    NoProviderConfigured,

    /// Authentication failed.
    #[error("authentication with {provider} failed: {kind}")]
    Auth {
        /// The provider that rejected the credential.
        provider: ProviderId,
        /// How it failed.
        kind: AuthKind,
    },

    /// The provider rate-limited the request.
    #[error("{provider} rate limited the request")]
    RateLimited {
        /// The provider.
        provider: ProviderId,
        /// Server-supplied wait, when present. A `retry_after` beyond the
        /// surface's latency budget triggers fallback rather than a wait (§4).
        retry_after: Option<Duration>,
    },

    /// No usable network path. Detected from connect failures, never from a
    /// reachability API, and tracked per provider with hysteresis (§13).
    #[error("offline")]
    Offline,

    /// The provider answered, but not usefully.
    #[error("{provider} returned HTTP {status}")]
    ProviderUnavailable {
        /// The provider.
        provider: ProviderId,
        /// HTTP status. A 4xx here is a bug in aibo and must not fall back (§4).
        status: u16,
    },

    /// The assembled request exceeds the model's context.
    #[error("context too large: {actual} tokens, limit {limit}")]
    ContextTooLarge {
        /// Budgeted limit.
        limit: usize,
        /// Estimated actual size.
        actual: usize,
    },

    /// A phase of the request timed out.
    #[error("timed out during {phase}")]
    Timeout {
        /// Which phase.
        phase: TimeoutPhase,
    },

    /// Context capture failed.
    #[error("could not read context from {app}: {reason}")]
    CaptureFailed {
        /// Bundle id / executable name of the target app.
        app: String,
        /// Why.
        reason: CaptureFailure,
    },

    /// Writing text back failed.
    #[error("could not insert text: {reason}")]
    InsertFailed {
        /// Why.
        reason: InsertFailure,
    },

    /// Sandboxed code execution stopped.
    #[error("sandbox (tier {tier}) {reason}")]
    Sandbox {
        /// Permission tier (§11).
        tier: u8,
        /// Why.
        reason: SandboxFailure,
    },

    /// The configured agent backend is not installed or not on PATH.
    #[error("agent backend `{which}` is not available")]
    AgentBackendMissing {
        /// Backend name, e.g. `codex`.
        which: &'static str,
    },

    /// A budget ceiling stopped the work (§14).
    #[error("budget exceeded: {kind:?}")]
    BudgetExceeded {
        /// Which ceiling.
        kind: BudgetKind,
    },

    /// The binding names a model this provider will not serve. Caught before
    /// dispatch (§4, §10).
    ///
    /// Deliberately **not** [`AiboError::ProviderUnavailable`]: the provider is
    /// healthy, the network is fine and the credential is valid — the *binding*
    /// is wrong, so no retry and no other entry in the role chain can fix it,
    /// and §4 is explicit that a 400 must not fall back.
    ///
    /// Deliberately **not** [`AiboError::Internal`] either. `Internal` is the
    /// one thing §13 renders as "something went wrong" plus "copy diagnostics",
    /// which is precisely the opaque dead end a pre-dispatch check exists to
    /// prevent: wrapping this in `Internal` throws away the model id and the
    /// list of ids that do work at the exact moment the user needs both. Typing
    /// it lets the panel spend its one §13 action offering a model that works.
    #[error("{provider} does not accept model `{model}`: {constraint}")]
    ModelRejected {
        /// The provider the binding names.
        provider: ProviderId,
        /// The model id it refuses.
        model: String,
        /// Why, in the provider's own terms. Diagnostic form, for logs and the
        /// §19 bundle — the UI writes its own sentence from the other fields.
        constraint: String,
        /// Ids known to work on this provider, best first. Empty when nothing
        /// is known, which is the only case where the UI has no model to offer.
        alternatives: Vec<String>,
    },

    /// An unexpected internal failure.
    ///
    /// **Never shown raw** (§13): the UI renders a generic message plus a "copy
    /// diagnostics" button. The plan writes this as `Internal(anyhow::Error)`;
    /// `aibo-core` is a library and uses a boxed `std` error instead so that
    /// `anyhow` stays confined to the binary.
    #[error("internal error")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// The fixed user-facing treatment for an error (§13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Treatment {
    /// Try the next entry in the role chain; a subtle footnote names the
    /// substitute. Only ever silent when fallback is enabled for the role (§14)
    /// — otherwise the caller escalates to [`Treatment::Inline`].
    SilentFallback,
    /// One sentence in the panel plus one action button.
    Inline,
    /// Non-blocking toast. The result stays in the panel so the user can copy
    /// it manually.
    Toast,
    /// Opens settings. The only treatment allowed to interrupt.
    Blocking,
}

impl AiboError {
    /// The treatment this error gets (§13).
    ///
    /// `RateLimited` and `ProviderUnavailable` map to
    /// [`Treatment::SilentFallback`] regardless of `retry_after`: §4 makes a
    /// long `retry_after` a *stronger* reason to move down the chain, not a
    /// weaker one. If the role has no fallback enabled, the caller renders the
    /// error inline instead — see [`AiboError::is_fallback_eligible`].
    pub fn treatment(&self) -> Treatment {
        match self {
            AiboError::ProviderUnavailable { .. } | AiboError::RateLimited { .. } => {
                Treatment::SilentFallback
            }
            AiboError::Auth { .. }
            | AiboError::ContextTooLarge { .. }
            | AiboError::Timeout { .. }
            | AiboError::BudgetExceeded { .. }
            | AiboError::Offline
            | AiboError::Sandbox { .. }
            | AiboError::AgentBackendMissing { .. }
            | AiboError::ModelRejected { .. }
            | AiboError::Internal(_) => Treatment::Inline,
            AiboError::InsertFailed { .. } | AiboError::CaptureFailed { .. } => Treatment::Toast,
            AiboError::NoProviderConfigured => Treatment::Blocking,
        }
    }

    /// Whether this failure may move the request to the next entry in the role
    /// chain (§4).
    ///
    /// A 4xx other than 429 is a bug in aibo and must surface as one rather
    /// than being retried elsewhere — that would both hide the bug and spend
    /// the user's money twice (§14).
    pub fn is_fallback_eligible(&self) -> bool {
        match self {
            AiboError::RateLimited { .. } => true,
            AiboError::ProviderUnavailable { status, .. } => *status >= 500,
            AiboError::Timeout { phase } => {
                matches!(phase, TimeoutPhase::Connect | TimeoutPhase::FirstToken)
            }
            AiboError::Offline => true,
            _ => false,
        }
    }

    /// Whether the user can meaningfully retry the same request unchanged.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            AiboError::RateLimited { .. }
                | AiboError::Offline
                | AiboError::ProviderUnavailable { .. }
                | AiboError::Timeout { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Credential, CredentialChain};
    use secrecy::SecretString;

    /// The literal used across the redaction tests. If this string ever appears
    /// in a `Debug` or `Display` rendering, a secret has leaked into a log, a
    /// panic message or a diagnostics export.
    const CANARY: &str = "sk-live-CANARY-0dd1b8a7-must-never-be-printed";

    fn sample_credentials() -> Vec<Credential> {
        vec![
            Credential::ApiKey(SecretString::from(CANARY.to_string())),
            Credential::AzureKey {
                key: SecretString::from(CANARY.to_string()),
                deployment: "prod-gpt".to_string(),
                api_version: "2026-01-01".to_string(),
            },
            Credential::AwsSigV4 {
                chain: CredentialChain::Profile(CANARY.to_string()),
                region: "us-east-1".to_string(),
            },
            Credential::LocalEndpoint(
                url::Url::parse(&format!("http://user:{CANARY}@localhost:11434")).unwrap(),
            ),
        ]
    }

    #[test]
    fn credential_debug_never_leaks_the_secret() {
        for cred in sample_credentials() {
            let rendered = format!("{cred:?}");
            assert!(
                !rendered.contains(CANARY),
                "Credential Debug leaked a secret: {rendered}"
            );
        }
    }

    #[test]
    fn credential_debug_still_identifies_the_variant() {
        // Redaction must not make the value useless for diagnostics.
        let rendered = format!(
            "{:?}",
            Credential::ApiKey(SecretString::from(CANARY.to_string()))
        );
        assert!(rendered.contains("ApiKey"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn error_rendering_never_leaks_a_credential() {
        // An error built from a credential-shaped provider id, plus an
        // `Internal` wrapping a source that itself carries a credential value:
        // neither the Display nor the Debug form may reproduce the secret.
        let boxed: Box<dyn std::error::Error + Send + Sync> =
            Box::new(std::io::Error::other(format!(
                "{:?}",
                Credential::ApiKey(SecretString::from(CANARY.to_string()))
            )));

        let errors = vec![
            AiboError::NoProviderConfigured,
            AiboError::Auth {
                provider: ProviderId::ANTHROPIC,
                kind: AuthKind::Revoked,
            },
            AiboError::RateLimited {
                provider: ProviderId::CEREBRAS,
                retry_after: Some(Duration::from_secs(30)),
            },
            AiboError::ProviderUnavailable {
                provider: ProviderId::OPENAI,
                status: 503,
            },
            AiboError::Internal(boxed),
        ];

        for err in &errors {
            let display = err.to_string();
            let debug = format!("{err:?}");
            assert!(!display.contains(CANARY), "Display leaked: {display}");
            assert!(!debug.contains(CANARY), "Debug leaked: {debug}");
        }
    }

    #[test]
    fn internal_display_is_generic() {
        // §13: `Internal` is never shown raw. Its Display must not carry the
        // source's message, only the generic sentence.
        let err = AiboError::Internal(Box::new(std::io::Error::other("stack trace with paths")));
        assert_eq!(err.to_string(), "internal error");
    }

    #[test]
    fn treatments_match_the_section_13_table() {
        assert_eq!(
            AiboError::NoProviderConfigured.treatment(),
            Treatment::Blocking
        );
        assert_eq!(
            AiboError::ProviderUnavailable {
                provider: ProviderId::GROQ,
                status: 502
            }
            .treatment(),
            Treatment::SilentFallback
        );
        assert_eq!(
            AiboError::CaptureFailed {
                app: "com.tinyspeck.slackmacgap".into(),
                reason: CaptureFailure::NoAxTree
            }
            .treatment(),
            Treatment::Toast
        );
        assert_eq!(
            AiboError::InsertFailed {
                reason: InsertFailure::ImeActive
            }
            .treatment(),
            Treatment::Toast
        );
        assert_eq!(
            AiboError::ContextTooLarge {
                limit: 8192,
                actual: 90_000
            }
            .treatment(),
            Treatment::Inline
        );
    }

    #[test]
    fn a_400_never_falls_back() {
        assert!(
            !AiboError::ProviderUnavailable {
                provider: ProviderId::OPENAI,
                status: 400
            }
            .is_fallback_eligible()
        );
        assert!(
            AiboError::ProviderUnavailable {
                provider: ProviderId::OPENAI,
                status: 500
            }
            .is_fallback_eligible()
        );
    }
}
