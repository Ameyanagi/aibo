//! Credentials, token refresh and the storage seam (§7, §3a).
//!
//! §7's [`Credential`] enum exists from day one because the "high security"
//! tier does not use API keys at all. This module supplies the two things the
//! enum implies but does not contain: an implementation of
//! [`TokenProvider`] that refreshes ahead of expiry **with jitter**, and a
//! narrow storage trait so the refreshed pair can be persisted without
//! `aibo-provider` depending on `aibo-store`.

use std::sync::Mutex as StdMutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aibo_core::error::{AiboError, AuthKind, Result};
use aibo_core::types::{Credential, ProviderId, TokenProvider};
use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Token material
// ---------------------------------------------------------------------------

/// One OAuth token pair plus the claims aibo needs alongside it.
#[derive(Clone)]
pub struct TokenSet {
    /// The bearer token.
    pub access_token: SecretString,
    /// The refresh token, where the flow issues one.
    ///
    /// §3a: for the ChatGPT device flow these are **single-use**. Every refresh
    /// replaces this value and the old one must never be presented again.
    pub refresh_token: Option<SecretString>,
    /// The raw ID token, kept because the `chatgpt_account_id` claim is read
    /// from it (§3a).
    pub id_token: Option<SecretString>,
    /// Absolute expiry, when the issuer states one.
    pub expires_at: Option<SystemTime>,
    /// Account identifier extracted from the ID token, for the
    /// `ChatGPT-Account-ID` header.
    pub account_id: Option<String>,
}

impl std::fmt::Debug for TokenSet {
    /// Variant metadata only; never the token (see the redaction tests in
    /// `aibo-core::error`).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("id_token", &self.id_token.as_ref().map(|_| "<redacted>"))
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .finish()
    }
}

impl TokenSet {
    /// Whether this token should be refreshed now.
    ///
    /// `skew` is the refresh-ahead window and `jitter` a per-instance offset,
    /// so that many aibo instances (or many providers in one instance) do not
    /// all refresh on the same second.
    pub fn needs_refresh(&self, now: SystemTime, skew: Duration, jitter: Duration) -> bool {
        match self.expires_at {
            None => false,
            Some(exp) => match exp.duration_since(now) {
                Err(_) => true, // already expired
                Ok(remaining) => remaining <= skew + jitter,
            },
        }
    }
}

/// The serialisable form written to credential storage.
///
/// On Windows, aibo stores this as one DPAPI-encrypted file, so a multi-kilobyte
/// JWT is written atomically without Credential Manager's blob-size limit.
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    /// Bearer token.
    pub access_token: String,
    /// Refresh token, single-use for the ChatGPT flow.
    pub refresh_token: Option<String>,
    /// Raw ID token.
    pub id_token: Option<String>,
    /// Expiry as seconds since the Unix epoch.
    pub expires_at_unix: Option<u64>,
    /// Cached `chatgpt_account_id` claim.
    pub account_id: Option<String>,
}

impl std::fmt::Debug for StoredTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredTokens")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("id_token", &self.id_token.as_ref().map(|_| "<redacted>"))
            .field("expires_at_unix", &self.expires_at_unix)
            .field("account_id", &self.account_id)
            .finish()
    }
}

impl From<&TokenSet> for StoredTokens {
    fn from(t: &TokenSet) -> Self {
        Self {
            access_token: t.access_token.expose_secret().to_string(),
            refresh_token: t
                .refresh_token
                .as_ref()
                .map(|s| s.expose_secret().to_string()),
            id_token: t.id_token.as_ref().map(|s| s.expose_secret().to_string()),
            expires_at_unix: t
                .expires_at
                .and_then(|e| e.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            account_id: t.account_id.clone(),
        }
    }
}

impl From<StoredTokens> for TokenSet {
    fn from(s: StoredTokens) -> Self {
        Self {
            access_token: SecretString::from(s.access_token),
            refresh_token: s.refresh_token.map(SecretString::from),
            id_token: s.id_token.map(SecretString::from),
            expires_at: s
                .expires_at_unix
                .map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
            account_id: s.account_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Storage seam
// ---------------------------------------------------------------------------

/// Persistence for a refreshed token pair.
///
/// **Cross-crate note.** The concrete implementation belongs to `aibo-store`
/// (`secrets` module, §12) or to the binary. It is declared here rather than
/// imported so `aibo-provider` keeps no dependency on the storage crate; the
/// dependency edge that does exist (`aibo-store` → `aibo-provider`, or the
/// binary wiring both) stays acyclic either way.
#[async_trait]
pub trait TokenStore: Send + Sync {
    /// Read the stored pair for `key`, if any.
    async fn load(&self, key: &str) -> Result<Option<StoredTokens>>;
    /// Replace the stored pair for `key`.
    async fn save(&self, key: &str, tokens: &StoredTokens) -> Result<()>;
    /// Forget the pair for `key` — used when refresh fails terminally and the
    /// user must log in again (§3a).
    async fn clear(&self, key: &str) -> Result<()>;
}

/// A process-lifetime [`TokenStore`], for tests and for the `Ephemeral`
/// posture. Never survives a restart, which is exactly the point.
#[derive(Debug, Default)]
pub struct InMemoryTokenStore {
    entries: StdMutex<std::collections::HashMap<String, StoredTokens>>,
}

#[async_trait]
impl TokenStore for InMemoryTokenStore {
    async fn load(&self, key: &str) -> Result<Option<StoredTokens>> {
        Ok(self.lock().get(key).cloned())
    }

    async fn save(&self, key: &str, tokens: &StoredTokens) -> Result<()> {
        self.lock().insert(key.to_string(), tokens.clone());
        Ok(())
    }

    async fn clear(&self, key: &str) -> Result<()> {
        self.lock().remove(key);
        Ok(())
    }
}

impl InMemoryTokenStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, StoredTokens>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

/// The first-class OAuth failure states §3a names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum OAuthFailure {
    /// The authorisation server says this refresh token was already spent.
    /// Single-use tokens make this a normal race, not a corruption.
    #[error("refresh token was already used")]
    RefreshTokenReused,
    /// The token was invalidated server-side; the user must log in again.
    #[error("refresh token was invalidated")]
    RefreshTokenInvalidated,
    /// No refresh token is held at all.
    #[error("no refresh token stored")]
    NoRefreshToken,
    /// The device-code grant has not been approved yet.
    #[error("authorization pending")]
    AuthorizationPending,
    /// The device-code poll is too fast.
    #[error("slow down")]
    SlowDown,
    /// The device code expired before the user approved it.
    #[error("device code expired")]
    ExpiredToken,
    /// The user declined.
    #[error("access denied")]
    AccessDenied,
}

impl OAuthFailure {
    /// Parse an RFC 8628 / OpenAI `error` code.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "refresh_token_reused" => Some(Self::RefreshTokenReused),
            "refresh_token_invalidated" | "invalid_grant" => Some(Self::RefreshTokenInvalidated),
            "authorization_pending" => Some(Self::AuthorizationPending),
            "slow_down" => Some(Self::SlowDown),
            "expired_token" => Some(Self::ExpiredToken),
            "access_denied" => Some(Self::AccessDenied),
            _ => None,
        }
    }

    /// Whether the user has to complete a fresh login.
    pub const fn requires_relogin(self) -> bool {
        matches!(
            self,
            Self::RefreshTokenReused
                | Self::RefreshTokenInvalidated
                | Self::NoRefreshToken
                | Self::ExpiredToken
                | Self::AccessDenied
        )
    }

    /// The failure-model mapping (§13).
    pub fn into_error(self, provider: ProviderId) -> AiboError {
        match self {
            Self::RefreshTokenReused | Self::RefreshTokenInvalidated => AiboError::Auth {
                provider,
                kind: AuthKind::Revoked,
            },
            Self::NoRefreshToken | Self::ExpiredToken | Self::AccessDenied => AiboError::Auth {
                provider,
                kind: AuthKind::Expired,
            },
            Self::AuthorizationPending | Self::SlowDown => AiboError::Internal(Box::new(self)),
        }
    }
}

/// How a [`RefreshingTokenProvider`] obtains a new token.
#[async_trait]
pub trait TokenRefresh: Send + Sync {
    /// Exchange `current` for a fresh [`TokenSet`].
    ///
    /// `current` is `None` on the very first call when nothing was loaded from
    /// storage; implementations that cannot bootstrap without a refresh token
    /// return [`OAuthFailure::NoRefreshToken`].
    async fn refresh(&self, current: Option<&TokenSet>) -> Result<TokenSet>;

    /// Stable label for logs. Never contains the token.
    fn label(&self) -> &str;
}

/// Refresh-ahead configuration.
#[derive(Debug, Clone, Copy)]
pub struct RefreshPolicy {
    /// How far ahead of expiry to refresh. §3a: Codex uses five minutes.
    pub skew: Duration,
    /// Upper bound of the random offset added to `skew`, so instances do not
    /// stampede the token endpoint at the same second.
    pub max_jitter: Duration,
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self {
            skew: Duration::from_secs(5 * 60),
            max_jitter: Duration::from_secs(60),
        }
    }
}

/// A [`TokenProvider`] that keeps a token fresh.
///
/// Concurrency: reads take a shared lock; a refresh takes an exclusive one and
/// re-checks, so a burst of simultaneous requests performs **one** refresh.
/// That single-flight property is not a nicety — with single-use refresh tokens
/// a concurrent double refresh invalidates the pair (§3a).
pub struct RefreshingTokenProvider {
    provider: ProviderId,
    storage_key: String,
    source: std::sync::Arc<dyn TokenRefresh>,
    store: std::sync::Arc<dyn TokenStore>,
    policy: RefreshPolicy,
    jitter: Duration,
    cached: tokio::sync::RwLock<Option<TokenSet>>,
    refresh_lock: tokio::sync::Mutex<()>,
    label: String,
}

impl std::fmt::Debug for RefreshingTokenProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshingTokenProvider")
            .field("provider", &self.provider)
            .field("storage_key", &self.storage_key)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl RefreshingTokenProvider {
    /// Build a provider. Nothing is loaded or refreshed until the first
    /// [`TokenProvider::token`] call, so construction cannot block startup.
    pub fn new(
        provider: ProviderId,
        storage_key: impl Into<String>,
        source: std::sync::Arc<dyn TokenRefresh>,
        store: std::sync::Arc<dyn TokenStore>,
        policy: RefreshPolicy,
    ) -> Self {
        let label = format!("{provider}:{}", source.label());
        Self {
            provider,
            storage_key: storage_key.into(),
            source,
            store,
            policy,
            jitter: pseudo_jitter(policy.max_jitter),
            cached: tokio::sync::RwLock::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
            label,
        }
    }

    /// Seed the cache with a set obtained out of band (e.g. straight from a
    /// completed device-code flow) and persist it.
    pub async fn seed(&self, tokens: TokenSet) -> Result<()> {
        self.store
            .save(&self.storage_key, &StoredTokens::from(&tokens))
            .await?;
        *self.cached.write().await = Some(tokens);
        Ok(())
    }

    /// The `chatgpt_account_id`-style claim carried alongside the token, if the
    /// current set has one. Used for the `ChatGPT-Account-ID` header (§3a).
    pub async fn account_id(&self) -> Option<String> {
        if let Some(t) = self.cached.read().await.as_ref() {
            return t.account_id.clone();
        }
        self.store
            .load(&self.storage_key)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.account_id)
    }

    /// Drop the stored credential. Called when refresh fails terminally, so the
    /// UI can prompt for a fresh login instead of retrying a dead token (§3a).
    pub async fn forget(&self) -> Result<()> {
        *self.cached.write().await = None;
        self.store.clear(&self.storage_key).await
    }

    async fn current(&self) -> Result<Option<TokenSet>> {
        if let Some(t) = self.cached.read().await.clone() {
            return Ok(Some(t));
        }
        let loaded = self
            .store
            .load(&self.storage_key)
            .await?
            .map(TokenSet::from);
        if let Some(t) = loaded.clone() {
            *self.cached.write().await = Some(t);
        }
        Ok(loaded)
    }
}

#[async_trait]
impl TokenProvider for RefreshingTokenProvider {
    async fn token(&self) -> Result<SecretString> {
        let now = SystemTime::now();
        if let Some(t) = self.current().await?
            && !t.needs_refresh(now, self.policy.skew, self.jitter)
        {
            return Ok(t.access_token.clone());
        }

        let _guard = self.refresh_lock.lock().await;

        // Re-check: another task may have refreshed while we waited. Doing the
        // work twice would burn a single-use refresh token (§3a).
        let current = self.current().await?;
        if let Some(t) = current.clone()
            && !t.needs_refresh(SystemTime::now(), self.policy.skew, self.jitter)
        {
            return Ok(t.access_token.clone());
        }

        match self.source.refresh(current.as_ref()).await {
            Ok(fresh) => {
                self.store
                    .save(&self.storage_key, &StoredTokens::from(&fresh))
                    .await?;
                let access = fresh.access_token.clone();
                *self.cached.write().await = Some(fresh);
                Ok(access)
            }
            Err(err) => {
                if let AiboError::Auth { kind, .. } = &err
                    && matches!(kind, AuthKind::Revoked | AuthKind::Expired)
                {
                    // Terminal: keeping the dead pair only guarantees the next
                    // request fails the same way.
                    let _ = self.forget().await;
                }
                Err(err)
            }
        }
    }

    fn label(&self) -> &str {
        &self.label
    }
}

/// A [`TokenProvider`] over a token that never changes.
///
/// Useful for tests, for `EntraId` when the caller already holds a managed
/// identity token, and as the trivial adapter from an API key.
#[derive(Debug)]
pub struct StaticTokenProvider {
    token: SecretString,
    label: String,
}

impl StaticTokenProvider {
    /// Wrap a fixed token.
    pub fn new(token: SecretString, label: impl Into<String>) -> Self {
        Self {
            token,
            label: label.into(),
        }
    }
}

#[async_trait]
impl TokenProvider for StaticTokenProvider {
    async fn token(&self) -> Result<SecretString> {
        Ok(self.token.clone())
    }

    fn label(&self) -> &str {
        &self.label
    }
}

// ---------------------------------------------------------------------------
// Applying a credential to a request
// ---------------------------------------------------------------------------

/// How a provider expects its credential to be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStyle {
    /// `Authorization: Bearer …` — every OpenAI-compatible endpoint.
    Bearer,
    /// `api-key: …` — Azure OpenAI with a key.
    AzureApiKey,
    /// `x-api-key: …` — Anthropic.
    XApiKey,
    /// No credential is sent (Ollama / llama.cpp).
    None,
}

/// Attach `credential` to `builder` in the style the provider expects.
///
/// [`Credential::AwsSigV4`] is rejected: Bedrock signs per request rather than
/// carrying a bearer token, which is on its own enough to justify a separate
/// implementation (§7).
pub async fn apply_credential(
    provider: &ProviderId,
    credential: &Credential,
    style: AuthStyle,
    builder: reqwest::RequestBuilder,
) -> Result<reqwest::RequestBuilder> {
    let secret: Option<SecretString> = match credential {
        Credential::ApiKey(k) => Some(k.clone()),
        Credential::AzureKey { key, .. } => Some(key.clone()),
        Credential::EntraId(tp)
        | Credential::GcpServiceAccount(tp)
        | Credential::ChatGptOAuth(tp) => Some(tp.token().await?),
        Credential::LocalEndpoint(_) => None,
        Credential::AwsSigV4 { .. } => {
            return Err(AiboError::Internal(Box::new(crate::wire::Unimplemented {
                provider: provider.clone(),
                what: "SigV4 credentials cannot be presented as a header; use the bedrock module",
            })));
        }
    };

    Ok(match (style, secret) {
        (AuthStyle::None, _) | (_, None) => builder,
        (AuthStyle::Bearer, Some(s)) => {
            builder.header("authorization", format!("Bearer {}", s.expose_secret()))
        }
        (AuthStyle::AzureApiKey, Some(s)) => builder.header("api-key", s.expose_secret()),
        (AuthStyle::XApiKey, Some(s)) => builder.header("x-api-key", s.expose_secret()),
    })
}

/// A cheap, dependency-free jitter source.
///
/// This is scheduling jitter, not a security primitive: it only has to stop
/// many instances refreshing on the same second. Seeded from the wall clock and
/// a per-process counter so two providers built in the same millisecond still
/// differ.
fn pseudo_jitter(max: Duration) -> Duration {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) ^ nanos;
    // splitmix64 finaliser
    let mut z = n.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;

    let max_ms = max.as_millis().max(1) as u64;
    Duration::from_millis(z % max_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingRefresh {
        calls: AtomicUsize,
        ttl: Duration,
    }

    #[async_trait]
    impl TokenRefresh for CountingRefresh {
        async fn refresh(&self, _current: Option<&TokenSet>) -> Result<TokenSet> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(TokenSet {
                access_token: SecretString::from(format!("access-{n}")),
                refresh_token: Some(SecretString::from(format!("refresh-{n}"))),
                id_token: None,
                expires_at: Some(SystemTime::now() + self.ttl),
                account_id: Some("acct_123".into()),
            })
        }

        fn label(&self) -> &str {
            "counting"
        }
    }

    fn provider(ttl: Duration) -> (Arc<CountingRefresh>, RefreshingTokenProvider) {
        let source = Arc::new(CountingRefresh {
            calls: AtomicUsize::new(0),
            ttl,
        });
        let tp = RefreshingTokenProvider::new(
            ProviderId::CODEX,
            "codex/oauth",
            source.clone(),
            Arc::new(InMemoryTokenStore::default()),
            RefreshPolicy {
                skew: Duration::from_secs(60),
                max_jitter: Duration::from_secs(1),
            },
        );
        (source, tp)
    }

    #[tokio::test]
    async fn a_valid_token_is_reused() {
        let (source, tp) = provider(Duration::from_secs(3600));
        let a = tp.token().await.unwrap();
        let b = tp.token().await.unwrap();
        assert_eq!(a.expose_secret(), b.expose_secret());
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_callers_refresh_once() {
        // With single-use refresh tokens a double refresh invalidates the pair.
        let (source, tp) = provider(Duration::from_secs(3600));
        let tp = Arc::new(tp);
        let mut set = Vec::new();
        for _ in 0..16 {
            let tp = tp.clone();
            set.push(tokio::spawn(async move { tp.token().await.map(|_| ()) }));
        }
        for h in set {
            h.await.unwrap().unwrap();
        }
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_token_inside_the_skew_window_is_refreshed() {
        let (source, tp) = provider(Duration::from_secs(10));
        tp.token().await.unwrap();
        tp.token().await.unwrap();
        assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn tokens_survive_a_round_trip_through_the_store() {
        let store = Arc::new(InMemoryTokenStore::default());
        let set = TokenSet {
            access_token: SecretString::from("a".to_string()),
            refresh_token: Some(SecretString::from("r".to_string())),
            id_token: Some(SecretString::from("i".to_string())),
            expires_at: Some(UNIX_EPOCH + Duration::from_secs(1_800_000_000)),
            account_id: Some("acct".into()),
        };
        store.save("k", &StoredTokens::from(&set)).await.unwrap();
        let back: TokenSet = store.load("k").await.unwrap().unwrap().into();
        assert_eq!(back.access_token.expose_secret(), "a");
        assert_eq!(back.expires_at, set.expires_at);
        assert_eq!(back.account_id.as_deref(), Some("acct"));
    }

    #[test]
    fn oauth_failures_map_to_the_failure_model() {
        assert_eq!(
            OAuthFailure::from_code("refresh_token_reused"),
            Some(OAuthFailure::RefreshTokenReused)
        );
        assert!(OAuthFailure::RefreshTokenReused.requires_relogin());
        assert!(!OAuthFailure::AuthorizationPending.requires_relogin());
        assert!(matches!(
            OAuthFailure::RefreshTokenInvalidated.into_error(ProviderId::CODEX),
            AiboError::Auth {
                kind: AuthKind::Revoked,
                ..
            }
        ));
    }

    #[test]
    fn debug_never_leaks_a_token() {
        let set = TokenSet {
            access_token: SecretString::from("sk-CANARY".to_string()),
            refresh_token: Some(SecretString::from("rt-CANARY".to_string())),
            id_token: Some(SecretString::from("id-CANARY".to_string())),
            expires_at: None,
            account_id: None,
        };
        let rendered = format!("{set:?}");
        assert!(!rendered.contains("CANARY"), "{rendered}");
        let stored = format!("{:?}", StoredTokens::from(&set));
        assert!(!stored.contains("CANARY"), "{stored}");
    }

    #[test]
    fn jitter_stays_inside_its_bound() {
        for _ in 0..1000 {
            assert!(pseudo_jitter(Duration::from_secs(60)) < Duration::from_secs(60));
        }
    }
}
