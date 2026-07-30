//! The failure model (§13).
//!
//! A tray app that fails silently is worse than one that crashes. Every error
//! aibo can produce is one variant of [`AiboError`], and every variant has a
//! fixed user-facing [`Treatment`] — that mapping is code, not convention, so
//! it cannot drift between surfaces.

use std::time::Duration;

use thiserror::Error;

use crate::types::{BudgetKind, ModelBinding, ProviderId};

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
    /// Secure input mode is active, so no process can read the field (§8).
    ///
    /// Deliberately **not** [`CaptureFailure::Denied`]. Both end in "aibo could
    /// not read that", but the recovery differs completely and getting it wrong
    /// is worse than saying nothing: `Denied` sends the user to the
    /// Accessibility pane, and when secure input is the cause that checkbox is
    /// already ticked. They are then told to fix a setting that is not broken,
    /// with no way to tell the app is wrong rather than themselves.
    ///
    /// Secure input has no user action at all — a password field has focus, or
    /// another process left the flag set globally — so the honest treatment is
    /// to name it and offer nothing.
    SecureInput,
    /// An IME composition is active; reading returns text the user cannot see
    /// (§9), so aibo declines rather than guessing.
    ImeActive,
}

impl std::fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CaptureFailure::NoAxTree => "no accessibility tree",
            CaptureFailure::Denied => "denied",
            CaptureFailure::SecureInput => "secure input is active",
            CaptureFailure::ImeActive => "IME composition active",
        })
    }
}

/// Why writing text back failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InsertFailure {
    /// The OS permission for synthetic events is missing (§8).
    PermissionDenied,
    /// Secure input mode is active, so no process may synthesise keystrokes.
    ///
    /// Split from [`InsertFailure::PermissionDenied`] for the same reason as
    /// [`CaptureFailure::SecureInput`]: the permission pane is the right answer
    /// to one and a dead end for the other.
    SecureInput,
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
            InsertFailure::SecureInput => "secure input is active",
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

/// Why an attachment was refused before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttachmentRejection {
    /// One item exceeds [`crate::types::MAX_ATTACHMENT_BYTES`].
    TooLarge {
        /// Actual raw size.
        bytes: usize,
        /// The cap.
        limit: usize,
    },
    /// The whole set exceeds [`crate::types::MAX_TOTAL_ATTACHMENT_BYTES`].
    TotalTooLarge {
        /// Summed raw size.
        bytes: usize,
        /// The cap.
        limit: usize,
    },
    /// More items than [`crate::types::MAX_ATTACHMENTS`].
    TooMany {
        /// Actual count.
        count: usize,
        /// The cap.
        limit: usize,
    },
    /// Not one of [`crate::types::SUPPORTED_IMAGE_MEDIA_TYPES`].
    UnsupportedMediaType,
    /// Zero bytes — a failed capture or a decode that produced nothing.
    Empty,
}

impl std::fmt::Display for AttachmentRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachmentRejection::TooLarge { bytes, limit } => {
                write!(f, "{bytes} bytes exceeds the {limit} byte limit")
            }
            AttachmentRejection::TotalTooLarge { bytes, limit } => {
                write!(f, "{bytes} bytes total exceeds the {limit} byte limit")
            }
            AttachmentRejection::TooMany { count, limit } => {
                write!(f, "{count} attachments exceeds the limit of {limit}")
            }
            AttachmentRejection::UnsupportedMediaType => f.write_str("unsupported media type"),
            AttachmentRejection::Empty => f.write_str("empty"),
        }
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
    #[error("{provider} returned HTTP {status}{}", detail.as_deref().map(|d| format!(": {d}")).unwrap_or_default())]
    ProviderUnavailable {
        /// The provider.
        provider: ProviderId,
        /// HTTP status. A 4xx here is a bug in aibo and must not fall back (§4).
        status: u16,
        /// The provider's own explanation, when its error body carried one.
        ///
        /// **Kept because discarding it made a whole class of bug
        /// undiagnosable.** A 400 from OpenAI says exactly what is wrong —
        /// "Unsupported parameter: 'temperature' is not supported with this
        /// model" — and throwing that away left nothing but a status code.
        /// Finding the cause of one such 400 took reproducing the request by
        /// hand against the live API; the user saw only "openai is not
        /// responding", which was not even true.
        detail: Option<String>,
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

    /// An image was attached, and the request cannot be served with vision.
    ///
    /// Two shapes, one user-facing fact and one action:
    ///
    /// - `binding: Some(_)` — the bound model's [`crate::types::Capabilities::vision`]
    ///   is `false`. Action: **switch model** (offer `alternatives`), or remove
    ///   the attachment.
    /// - `binding: None` — [`crate::types::Role::Vision`] has no chain at all,
    ///   because no vision-capable provider is configured. Action: **configure
    ///   one** (`alternatives` names §4's — OpenAI, Anthropic, Vertex).
    ///
    /// Deliberately **not** [`AiboError::Internal`]. §13 renders `Internal` as
    /// an opaque "something went wrong" plus "copy diagnostics", which is the
    /// exact dead end this error exists to replace — it would discard the model
    /// id, the attachment count and the list of models that *would* work, at the
    /// moment the user needs all three to fix it in one click.
    ///
    /// Deliberately **not** [`AiboError::NoProviderConfigured`] in the `None`
    /// case either, despite the resemblance. That variant is §13's only
    /// `Blocking` treatment: it interrupts and opens settings, which is correct
    /// for "aibo cannot do anything at all" and wrong here — the user has a
    /// working text setup and one attachment too many. [`Treatment::Inline`]
    /// keeps their session, their typed instruction and their attachment intact
    /// while offering the fix. Sending it as `NoProviderConfigured` reproduces
    /// the 2026-07-26 defect this whole feature was built to retire.
    ///
    /// Never falls back and never auto-retries: no other entry in the chain is
    /// reached by retrying, and §14 forbids spending the user's money to
    /// discover that.
    #[error(
        "{} cannot accept image input ({attachments} attached)",
        match binding {
            Some(b) => format!("{}/{}", b.provider, b.model),
            None => "no configured model".to_string(),
        }
    )]
    VisionUnsupported {
        /// The binding that would have served the request, or `None` when the
        /// `Vision` role has no chain.
        binding: Option<ModelBinding>,
        /// How many attachments would have to be dropped to proceed. Never
        /// dropped — this is the count the message quotes.
        attachments: usize,
        /// What to offer instead, best first: `provider/model` ids when a
        /// binding is known to work, otherwise the provider ids §4's `Vision`
        /// chain draws on. Empty only when nothing is known.
        alternatives: Vec<String>,
    },

    /// An attachment was refused before dispatch (size, count, media type).
    ///
    /// `Inline` with one action — remove the attachment. Caught in `aibo-core`
    /// rather than at the provider because §4 forbids falling back on a 400, so
    /// discovering a cap as a rejected request costs a round trip and dead-ends.
    #[error("attachment `{label}` ({media_type}) rejected: {reason}")]
    AttachmentRejected {
        /// The chip label, so the panel can name which one.
        label: String,
        /// Its media type. Empty when the rejection is about the whole set.
        media_type: String,
        /// Why.
        reason: AttachmentRejection,
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
    /// Build [`AiboError::VisionUnsupported`] for a bound model that cannot see.
    ///
    /// `alternatives` should be `provider/model` ids known to accept images —
    /// `RoleBindings::vision_alternatives` produces them from §4's chain.
    pub fn vision_unsupported(
        binding: ModelBinding,
        attachments: usize,
        alternatives: Vec<String>,
    ) -> Self {
        AiboError::VisionUnsupported {
            binding: Some(binding),
            attachments,
            alternatives,
        }
    }

    /// Build [`AiboError::VisionUnsupported`] for the case where
    /// [`crate::types::Role::Vision`] has no chain at all.
    ///
    /// `alternatives` should be the provider ids §4's `Vision` chain draws on —
    /// `roles::vision_providers` produces them.
    pub fn no_vision_provider(attachments: usize, alternatives: Vec<String>) -> Self {
        AiboError::VisionUnsupported {
            binding: None,
            attachments,
            alternatives,
        }
    }

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
            // §13 Inline: one sentence, one action ("Switch model" /
            // "Remove image"). See the variant docs for why neither of these is
            // `Internal` and why `VisionUnsupported` is not `Blocking`.
            | AiboError::VisionUnsupported { .. }
            | AiboError::AttachmentRejected { .. }
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
                detail: None,
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
                status: 502,
                detail: None,
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

    // -- vision (§10, §13) --------------------------------------------------

    fn binding() -> ModelBinding {
        ModelBinding {
            provider: ProviderId::CEREBRAS,
            model: "llama-3.3-70b".into(),
        }
    }

    #[test]
    fn vision_unsupported_is_inline_with_one_action() {
        // §13: not `Internal` (opaque "something went wrong" + copy
        // diagnostics), and not `Blocking` — the user has a working text setup
        // and one attachment too many, so their session must survive the error.
        let err = AiboError::vision_unsupported(binding(), 1, vec!["openai/gpt-5".into()]);
        assert_eq!(err.treatment(), Treatment::Inline);
        assert_eq!(
            AiboError::no_vision_provider(1, vec!["openai".into()]).treatment(),
            Treatment::Inline
        );
    }

    #[test]
    fn vision_unsupported_never_falls_back_and_never_retries() {
        // No other entry in the chain is reached by retrying, and §14 forbids
        // spending the user's money to discover that.
        let err = AiboError::vision_unsupported(binding(), 2, Vec::new());
        assert!(!err.is_fallback_eligible());
        assert!(!err.is_retryable());
    }

    #[test]
    fn vision_unsupported_names_the_model_or_says_none_is_configured() {
        let bound = AiboError::vision_unsupported(binding(), 1, Vec::new()).to_string();
        assert!(bound.contains("cerebras/llama-3.3-70b"), "{bound}");
        assert!(bound.contains('1'), "{bound}");

        let unbound = AiboError::no_vision_provider(3, Vec::new()).to_string();
        assert!(unbound.contains("no configured model"), "{unbound}");
        assert!(unbound.contains('3'), "{unbound}");
    }

    #[test]
    fn vision_unsupported_carries_what_the_one_action_needs() {
        // The panel's single §13 action has to offer something that works; the
        // error is the only thing it gets.
        let err = AiboError::vision_unsupported(
            binding(),
            1,
            vec!["openai/gpt-5".into(), "anthropic/claude-sonnet-4-5".into()],
        );
        let AiboError::VisionUnsupported {
            binding: b,
            attachments,
            alternatives,
        } = err
        else {
            panic!("wrong variant");
        };
        assert_eq!(b.unwrap().model, "llama-3.3-70b");
        assert_eq!(attachments, 1);
        assert_eq!(alternatives.first().unwrap(), "openai/gpt-5");
    }

    #[test]
    fn attachment_rejected_is_inline_and_terminal() {
        let err = AiboError::AttachmentRejected {
            label: "Screenshot".into(),
            media_type: "image/gif".into(),
            reason: AttachmentRejection::UnsupportedMediaType,
        };
        assert_eq!(err.treatment(), Treatment::Inline);
        assert!(!err.is_fallback_eligible());
        assert!(!err.is_retryable());
        let rendered = err.to_string();
        assert!(rendered.contains("Screenshot"), "{rendered}");
        assert!(rendered.contains("unsupported media type"), "{rendered}");
    }

    #[test]
    fn a_400_never_falls_back() {
        assert!(
            !AiboError::ProviderUnavailable {
                provider: ProviderId::OPENAI,
                status: 400,
                detail: None,
            }
            .is_fallback_eligible()
        );
        assert!(
            AiboError::ProviderUnavailable {
                provider: ProviderId::OPENAI,
                status: 500,
                detail: None,
            }
            .is_fallback_eligible()
        );
    }
}
