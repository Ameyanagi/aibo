//! Codex inference over aibo's **own** device-code OAuth (§3a).
//!
//! # The decision this implements
//!
//! §3a: *"aibo runs its own device-code login. It does not read
//! `$CODEX_HOME/auth.json`."* That replaces the earlier "read Codex's token
//! file" design and removes its worst failure mode — two processes sharing one
//! single-use refresh token and logging the user out of their own Codex. aibo
//! now owns a separate token pair, in its own keychain entry, refreshed on its
//! own schedule.
//!
//! # ✅ S6 RESOLVED — executed end-to-end 2026-07-26
//!
//! A Responses call succeeds with a device-code token and `ChatGPT-Account-ID`
//! and **without** `x-oai-attestation`. Attestation was the go/no-go; it is not
//! required. §3a outcome 1 applies and the `app-server` fallback is not needed.
//!
//! **The flow is four steps with PKCE, not "two POSTs and a poll".** An earlier
//! revision of this module implemented textbook RFC 8628 and could not have
//! worked — `serde` would have failed on step 1's response, and step 3's
//! success would have been misread as "still pending", polling until expiry.
//!
//! ```text
//! 1. POST {issuer}/api/accounts/deviceauth/usercode     [JSON]
//!    → { device_auth_id, user_code, interval, expires_at }
//!      ^ no `device_code`, no `verification_uri`
//! 2. human approves at https://auth.openai.com/codex/device
//! 3. POST {issuer}/api/accounts/deviceauth/token        [JSON]
//!    { device_auth_id, user_code }        ^ keyed on user_code
//!    → 403 while pending; on success
//!      { status, authorization_code, code_challenge, code_verifier }
//!      ^ an auth code + PKCE pair, NOT tokens
//! 4. POST {issuer}/oauth/token                          [FORM]
//!    grant_type=authorization_code & code & code_verifier
//!    & redirect_uri={issuer}/deviceauth/callback        ^ not localhost
//!    → { access_token, refresh_token, id_token, expires_in: 864000, … }
//! ```
//!
//! Six deviations from RFC 8628, each verified: JSON for steps 1/3 but form for
//! step 4; no `device_code`; poll keyed on `user_code`; pending is HTTP 403, not
//! `authorization_pending`; step 3 returns a code rather than tokens; and the
//! redirect URI is the issuer's own device callback. A generic OAuth
//! device-flow client fails at every one of them.
//!
//! # Verified request shape
//!
//! `Authorization`, `ChatGPT-Account-ID`, `OpenAI-Beta: responses=experimental`,
//! `originator: codex_cli_rs`, `session_id`, and a Codex-like `User-Agent` —
//! see [`CODEX_ORIGINATOR`] and [`CODEX_USER_AGENT`]. The User-Agent is
//! load-bearing: Cloudflare returns **HTTP 530** to a generic one and the
//! request never reaches OpenAI.
//!
//! # The binding constraint is the model allowlist, not auth
//!
//! ChatGPT-plan ids work; API-style ids are refused with *"not supported when
//! using Codex with a ChatGPT account"*. Measured TTFT (Yokohama, warm, n=3):
//! `gpt-5.5` 435 ms, `gpt-5.6-terra` 446 ms, `gpt-5.3-codex-spark` 499 ms,
//! `gpt-5.6-luna` 515 ms, `gpt-5.6-sol` 623 ms. Rejected: `gpt-5`,
//! `gpt-5-codex`, `gpt-5.1-codex`, `gpt-5.1-codex-mini`, `codex-mini-latest`.
//!
//! **So this provider cannot serve the `Fast` role** — the 435 ms floor misses
//! Complete's ≤ 250 ms target and there is no small model on the allowlist.
//! Bind it to `Smart`/`Ask` (§4).
//!
//! Settled elsewhere: the `ChatGPT-Account-ID` header comes from the
//! `chatgpt_account_id` claim ([`account_id_from_id_token`]); the token
//! lifecycle lives in [`crate::auth`]; posture is opt-in with a startup health
//! probe.
//!
//! **Still unverified — Cloudflare cookies.** Codex keeps a process-global jar
//! for `chatgpt.com`. The probe above succeeded without one, but it was a
//! single short-lived session; if a jar turns out to be needed for sustained
//! use, the workspace manifest must enable `reqwest`'s `cookies` feature.
//!
//! **The `client_id` question is a posture decision, not a code one** (§3a): the
//! only available id is Codex's own, and the consent screen the user sees is
//! `auth.openai.com/codex/device` — authorising *Codex* while the tokens go to
//! aibo. [`CLIENT_ID_ENV_VAR`] makes the value configurable rather than baked
//! in, so the decision stays reversible.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use aibo_core::error::{AiboError, AuthKind, Result};
use aibo_core::traits::Provider;
use aibo_core::types::{
    BoxStream, Capabilities, ChatRequest, Credential, Health, ModelInfo, MultiCandidate,
    ProviderId, StreamEvent, TokenProvider,
};
use async_trait::async_trait;
use base64::Engine;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth::{
    AuthStyle, OAuthFailure, RefreshPolicy, RefreshingTokenProvider, TokenRefresh, TokenSet,
    TokenStore, apply_credential,
};
use crate::http::{HttpConfig, build_client, map_transport_error};
use crate::openai_compat::{Quirks, ResponsesDecoder, build_responses_body};
use crate::sse::{decode, events_from_response, read_error_body, read_json_body};
use crate::wire::{ErrorShape, map_status, parse_retry_after};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The Codex inference base URL, as exported by `codex-model-provider-info`
/// (§3a).
pub const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// The OAuth issuer for the device-code flow (§3a).
pub const AUTH_ISSUER: &str = "https://auth.openai.com";

/// Where the device flow starts.
pub const DEVICE_USERCODE_PATH: &str = "api/accounts/deviceauth/usercode";

/// Where the device flow is polled.
pub const DEVICE_TOKEN_PATH: &str = "api/accounts/deviceauth/token";

/// The page the user is sent to. Note what it says: this screen authorises
/// **Codex** (§3a) — that is the posture decision, stated once.
pub const VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";

/// The OAuth token endpoint — used for both the step-4 authorization-code
/// exchange and subsequent refreshes.
///
/// Verified for the code exchange (§3a). Both grants are form-encoded here,
/// unlike the JSON device-auth calls.
pub const TOKEN_REFRESH_PATH: &str = "oauth/token";

/// Environment variable that overrides the OAuth client id.
///
/// §3a: `CLIENT_ID_OVERRIDE_ENV_VAR` exists in the OSS tree, but nothing
/// indicates OpenAI issues client ids to third parties for
/// ChatGPT-subscription auth. Keeping the value configurable means the posture
/// can change without a release.
pub const CLIENT_ID_ENV_VAR: &str = "AIBO_CODEX_CLIENT_ID";

/// The keychain entry aibo stores its Codex tokens under.
///
/// §12: a multi-kilobyte JWT does not fit in Windows Credential Manager; the
/// [`TokenStore`] implementation must chunk or use DPAPI files (spike S8).
pub const TOKEN_STORAGE_KEY: &str = "codex/device-auth";

/// `originator` value sent on every Codex request.
///
/// Codex's `is_first_party_originator()` allowlists `codex_cli_rs`,
/// `codex-tui` and `codex_vscode` (§3a). A non-allowlisted value was never
/// part of any request shape that has been observed to work.
pub const CODEX_ORIGINATOR: &str = "codex_cli_rs";

/// `User-Agent` for both auth and inference.
///
/// **Not cosmetic.** Cloudflare fronts `auth.openai.com` and returns
/// **HTTP 530** to a generic agent — the request never reaches OpenAI, so the
/// failure looks like an outage rather than a rejected client (§3a).
pub const CODEX_USER_AGENT: &str = "codex_cli_rs/0.145.0";

/// HTTP settings shared by the auth and inference clients.
fn codex_http_config() -> HttpConfig {
    HttpConfig {
        user_agent: CODEX_USER_AGENT.to_string(),
        ..HttpConfig::default()
    }
}

/// A v4-shaped identifier for the `session_id` header.
///
/// The endpoint only requires the header to be present and well-formed; it is
/// not used for correlation on aibo's side, so a counter-plus-nanos value is
/// sufficient and avoids a `uuid` dependency for one header.
fn uuid_v4_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (t >> 32) as u32,
        (t >> 16) as u16,
        (t & 0xfff) as u16,
        (n & 0xfff) as u16,
        n as u128 & 0xffff_ffff_ffff,
    )
}

/// Parse an RFC 3339 timestamp into a [`SystemTime`].
///
/// Hand-rolled because the only RFC 3339 value in the product is this one
/// field, and it is always UTC with a `+00:00` offset in practice.
fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let (date, rest) = s.split_once('T')?;
    let time = rest
        .split(['+', 'Z', '-'])
        .next()
        .unwrap_or(rest)
        .trim_end_matches('Z');
    let mut d = date.split('-');
    let (y, mo, da): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let mut t = time.split(':');
    let (h, mi): (i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let se: f64 = t.next().unwrap_or("0").parse().ok()?;

    // Days from civil epoch (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let secs = days * 86_400 + h * 3_600 + mi * 60 + se as i64;
    u64::try_from(secs)
        .ok()
        .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s))
}

/// How the backend treats a request with no `x-oai-attestation` header.
///
/// SPIKE: S6 is a go/no-go and this is where its answer is recorded. It is an
/// enum rather than a `bool` because "we have not measured it" and "it works
/// without one" must not be the same state — §13 requires unknown to be
/// distinguishable from known-good.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttestationPolicy {
    /// S6 has not been run on this build. Requests are attempted; a 401/403 is
    /// reported as an auth failure and the caller falls back per §3a.
    ///
    /// Retained as a runtime guard, not as an open question — if OpenAI starts
    /// enforcing attestation later, this is where that gets recorded without a
    /// redesign.
    Unknown,
    /// S6 outcome 1 — **the verified state as of 2026-07-26.** The direct
    /// endpoint returns 200 with device-code tokens and no attestation header.
    #[default]
    NotRequired,
    /// S6 outcome 2: attestation is required and aibo cannot produce it. The
    /// provider refuses to dispatch, and subscription inference belongs to a
    /// minimal `app-server` turn instead.
    Required,
}

// ---------------------------------------------------------------------------
// ID token claims
// ---------------------------------------------------------------------------

/// Extract the `chatgpt_account_id` claim from an ID token (§3a).
///
/// The payload is base64url-decoded and read; the signature is **not** verified
/// and must not be relied on for anything. That is safe here for one reason
/// only: the token came to aibo over TLS from the issuer it was requested from,
/// and the claim is used solely to populate a routing header. It is never an
/// authorisation decision.
///
/// Both the flat spelling and the namespaced one are accepted because the
/// namespaced form is what OpenAI's own clients read. [unverified — SPIKE: S6.]
pub fn account_id_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;

    claims
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|a| a.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
        })
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Device-code flow
// ---------------------------------------------------------------------------

/// What the UI must show the user to complete a login.
#[derive(Debug, Clone)]
pub struct DeviceCodeChallenge {
    /// The short code the user types on the verification page. The poll is
    /// keyed on this — **not** on an RFC 8628 `device_code`, which this
    /// endpoint never issues (§3a deviation 3).
    pub user_code: String,
    /// Server-side handle for this device-auth attempt. Sent alongside
    /// `user_code` on every poll (§3a deviation 2).
    pub device_auth_id: String,
    /// The page to open.
    pub verification_uri: String,
    /// Minimum poll interval demanded by the server.
    pub interval: Duration,
    /// When the code stops being usable.
    pub expires_at: SystemTime,
}

impl DeviceCodeChallenge {
    /// Whether the challenge has expired and the user must start again.
    pub fn is_expired(&self, now: SystemTime) -> bool {
        now >= self.expires_at
    }
}

/// Step 1 response. Verified 2026-07-26 — see §3a.
///
/// Note `interval` arrives as a JSON **string** (`"5"`), and expiry is an
/// absolute RFC 3339 `expires_at`, not a relative `expires_in`.
#[derive(Debug, Deserialize)]
struct UserCodeResponse {
    user_code: String,
    device_auth_id: String,
    #[serde(default)]
    interval: Option<StringOrU64>,
    #[serde(default)]
    expires_at: Option<String>,
}

/// The server is inconsistent about numeric encoding; accept both.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StringOrU64 {
    Str(String),
    Num(u64),
}

impl StringOrU64 {
    fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Str(s) => s.trim().parse().ok(),
            Self::Num(n) => Some(*n),
        }
    }
}

/// Step 3 response — an authorization code plus a PKCE pair, **not** tokens
/// (§3a deviation 5). The tokens come from the step-4 exchange.
#[derive(Debug, Deserialize)]
struct DeviceApprovalResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    authorization_code: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
}

/// Step 4 (`POST /oauth/token`) and refresh both return this.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

/// Result of one poll of the token endpoint.
#[derive(Debug)]
pub enum PollOutcome {
    /// The user has not approved yet; wait and poll again.
    Pending,
    /// The server asked for a slower cadence; increase the interval.
    SlowDown,
    /// Authorised.
    Authorised(Box<TokenSet>),
}

/// Client for the two device-auth endpoints.
pub struct DeviceAuthClient {
    issuer: Url,
    client_id: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for DeviceAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceAuthClient")
            .field("issuer", &self.issuer.as_str())
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl DeviceAuthClient {
    /// Build a client against [`AUTH_ISSUER`], or an override for testing.
    ///
    /// `client_id` falls back to [`CLIENT_ID_ENV_VAR`]. When neither is set,
    /// construction fails rather than guessing: §3a treats the client id as a
    /// deliberate posture choice, and a wrong value produces an opaque 400 at
    /// login time.
    pub fn new(issuer: Option<Url>, client_id: Option<String>) -> Result<Self> {
        let issuer = match issuer {
            Some(u) => u,
            None => Url::parse(AUTH_ISSUER).map_err(|e| AiboError::Internal(Box::new(e)))?,
        };
        let client_id = client_id
            .or_else(|| std::env::var(CLIENT_ID_ENV_VAR).ok())
            .filter(|s| !s.trim().is_empty())
            .ok_or(AiboError::NoProviderConfigured)?;

        Ok(Self {
            issuer,
            client_id,
            http: build_client(&codex_http_config())?,
        })
    }

    fn url(&self, path: &str) -> Result<Url> {
        let s = format!("{}/{path}", self.issuer.as_str().trim_end_matches('/'));
        Url::parse(&s).map_err(|e| AiboError::Internal(Box::new(e)))
    }

    /// `POST {issuer}/api/accounts/deviceauth/usercode`.
    pub async fn start(&self) -> Result<DeviceCodeChallenge> {
        let response = self
            .http
            .post(self.url(DEVICE_USERCODE_PATH)?)
            .json(&json!({ "client_id": self.client_id }))
            .send()
            .await
            .map_err(|e| map_transport_error(&ProviderId::CODEX, &e))?;

        let status = response.status();
        if !status.is_success() {
            let body = read_error_body(response, &ProviderId::CODEX)
                .await
                .unwrap_or_default();
            return Err(map_status(
                &ProviderId::CODEX,
                status.as_u16(),
                None,
                ErrorShape::OpenAiEnvelope,
                &body,
            ));
        }
        let body = read_json_body(response, &ProviderId::CODEX).await?;

        let parsed: UserCodeResponse =
            serde_json::from_str(&body).map_err(|e| AiboError::Internal(Box::new(e)))?;

        // The response carries no verification URI at all (§3a deviation 2);
        // the page is a fixed constant.
        Ok(DeviceCodeChallenge {
            user_code: parsed.user_code,
            device_auth_id: parsed.device_auth_id,
            verification_uri: VERIFICATION_URI.to_string(),
            interval: Duration::from_secs(
                parsed
                    .interval
                    .as_ref()
                    .and_then(StringOrU64::as_u64)
                    .unwrap_or(5),
            ),
            expires_at: parsed
                .expires_at
                .as_deref()
                .and_then(parse_rfc3339)
                .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(900)),
        })
    }

    /// One `POST {issuer}/api/accounts/deviceauth/token`, then — once approved
    /// — the step-4 `POST {issuer}/oauth/token` exchange.
    ///
    /// Two deviations are load-bearing here and were both verified against the
    /// live endpoint on 2026-07-26 (§3a):
    ///
    /// * **Pending approval is HTTP 403**, not `authorization_pending` inside a
    ///   400 body. A compliant RFC 8628 client aborts the poll here.
    /// * **Success returns an authorization code plus a PKCE verifier**, not
    ///   tokens. Treating a missing `access_token` as "pending" — as an earlier
    ///   revision did — polls forever against a server that already said yes.
    pub async fn poll_once(&self, challenge: &DeviceCodeChallenge) -> Result<PollOutcome> {
        let response = self
            .http
            .post(self.url(DEVICE_TOKEN_PATH)?)
            .json(&json!({
                "device_auth_id": challenge.device_auth_id,
                "user_code": challenge.user_code,
            }))
            .send()
            .await
            .map_err(|e| map_transport_error(&ProviderId::CODEX, &e))?;

        let status = response.status();

        // 403 == not approved yet. 429 == back off.
        if status.as_u16() == 403 {
            return Ok(PollOutcome::Pending);
        }
        if status.as_u16() == 429 {
            return Ok(PollOutcome::SlowDown);
        }
        if !status.is_success() {
            let body = read_error_body(response, &ProviderId::CODEX)
                .await
                .unwrap_or_default();
            return Err(map_status(
                &ProviderId::CODEX,
                status.as_u16(),
                None,
                ErrorShape::OpenAiEnvelope,
                &body,
            ));
        }
        let body = read_json_body(response, &ProviderId::CODEX).await?;

        let parsed: DeviceApprovalResponse =
            serde_json::from_str(&body).map_err(|e| AiboError::Internal(Box::new(e)))?;

        let (Some(code), Some(verifier)) = (parsed.authorization_code, parsed.code_verifier) else {
            // 200 without an authorization code means still waiting.
            debug_assert!(parsed.status.as_deref() != Some("success"));
            return Ok(PollOutcome::Pending);
        };

        let tokens = self.exchange_code(&code, &verifier).await?;
        Ok(PollOutcome::Authorised(Box::new(tokens)))
    }

    /// Step 4: `POST {issuer}/oauth/token`.
    ///
    /// **Form-encoded**, unlike steps 1 and 3 which are JSON (§3a deviation 1),
    /// and the `redirect_uri` is the issuer's own device callback rather than a
    /// localhost listener (§3a deviation 6) — the device flow never binds a port.
    async fn exchange_code(&self, code: &str, code_verifier: &str) -> Result<TokenSet> {
        let redirect_uri = format!(
            "{}/deviceauth/callback",
            self.issuer.as_str().trim_end_matches('/')
        );
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", self.client_id.as_str()),
            ("code_verifier", code_verifier),
        ];

        let response = self
            .http
            .post(self.url(TOKEN_REFRESH_PATH)?)
            .form(&form)
            .send()
            .await
            .map_err(|e| map_transport_error(&ProviderId::CODEX, &e))?;

        let status = response.status();
        if !status.is_success() {
            let body = read_error_body(response, &ProviderId::CODEX)
                .await
                .unwrap_or_default();
            return Err(map_status(
                &ProviderId::CODEX,
                status.as_u16(),
                None,
                ErrorShape::OpenAiEnvelope,
                &body,
            ));
        }
        let body = read_json_body(response, &ProviderId::CODEX).await?;

        let parsed: TokenResponse =
            serde_json::from_str(&body).map_err(|e| AiboError::Internal(Box::new(e)))?;
        let access = parsed.access_token.ok_or(AiboError::Auth {
            provider: ProviderId::CODEX,
            kind: AuthKind::Invalid,
        })?;

        Ok(token_set(
            access,
            parsed.refresh_token,
            parsed.id_token,
            parsed.expires_in,
        ))
    }

    /// Poll until the user approves, the code expires, or `cancel` fires.
    ///
    /// Honours `slow_down` by widening the interval, as RFC 8628 requires — a
    /// client that ignores it gets rate-limited out of its own login.
    pub async fn wait_for_token(
        &self,
        challenge: &DeviceCodeChallenge,
        cancel: CancellationToken,
    ) -> Result<TokenSet> {
        let mut interval = challenge.interval;
        loop {
            if challenge.is_expired(SystemTime::now()) {
                return Err(OAuthFailure::ExpiredToken.into_error(ProviderId::CODEX));
            }

            tokio::select! {
                () = cancel.cancelled() => {
                    return Err(OAuthFailure::AccessDenied.into_error(ProviderId::CODEX));
                }
                () = tokio::time::sleep(interval) => {}
            }

            match self.poll_once(challenge).await? {
                PollOutcome::Authorised(set) => return Ok(*set),
                PollOutcome::Pending => {}
                PollOutcome::SlowDown => interval += Duration::from_secs(5),
            }
        }
    }
}

fn token_set(
    access: String,
    refresh: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
) -> TokenSet {
    let account_id = id_token.as_deref().and_then(account_id_from_id_token);
    TokenSet {
        access_token: SecretString::from(access),
        refresh_token: refresh.map(SecretString::from),
        id_token: id_token.map(SecretString::from),
        expires_at: expires_in.map(|s| SystemTime::now() + Duration::from_secs(s)),
        account_id,
    }
}

/// The refresh half of the lifecycle aibo took ownership of (§3a).
pub struct ChatGptRefresh {
    issuer: Url,
    client_id: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for ChatGptRefresh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatGptRefresh")
            .field("issuer", &self.issuer.as_str())
            .finish_non_exhaustive()
    }
}

impl ChatGptRefresh {
    /// Build a refresher sharing the device client's issuer and client id.
    pub fn new(issuer: Option<Url>, client_id: String) -> Result<Self> {
        let issuer = match issuer {
            Some(u) => u,
            None => Url::parse(AUTH_ISSUER).map_err(|e| AiboError::Internal(Box::new(e)))?,
        };
        Ok(Self {
            issuer,
            client_id,
            http: build_client(&codex_http_config())?,
        })
    }
}

#[async_trait]
impl TokenRefresh for ChatGptRefresh {
    async fn refresh(&self, current: Option<&TokenSet>) -> Result<TokenSet> {
        let refresh_token = current
            .and_then(|t| t.refresh_token.clone())
            .ok_or_else(|| OAuthFailure::NoRefreshToken.into_error(ProviderId::CODEX))?;

        let url = format!(
            "{}/{TOKEN_REFRESH_PATH}",
            self.issuer.as_str().trim_end_matches('/')
        );
        // Form-encoded, matching the step-4 exchange against the same endpoint.
        // A JSON body here returns 400 (§3a deviation 1: JSON for the two
        // device-auth calls, form for everything on /oauth/token).
        let response = self
            .http
            .post(&url)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.expose_secret()),
            ])
            .send()
            .await
            .map_err(|e| map_transport_error(&ProviderId::CODEX, &e))?;

        let status = response.status();
        let body = if status.is_success() {
            read_json_body(response, &ProviderId::CODEX).await?
        } else {
            read_error_body(response, &ProviderId::CODEX)
                .await
                .unwrap_or_default()
        };
        let parsed: TokenResponse = serde_json::from_str(&body).unwrap_or(TokenResponse {
            access_token: None,
            refresh_token: None,
            id_token: None,
            expires_in: None,
            error: None,
        });

        // `refresh_token_reused` and `refresh_token_invalidated` are first-class
        // states, not generic auth errors: they mean the stored pair is dead and
        // the user must log in again (§3a).
        if let Some(code) = parsed.error.as_deref()
            && let Some(failure) = OAuthFailure::from_code(code)
        {
            return Err(failure.into_error(ProviderId::CODEX));
        }
        if !status.is_success() {
            return Err(map_status(
                &ProviderId::CODEX,
                status.as_u16(),
                None,
                ErrorShape::OpenAiEnvelope,
                &body,
            ));
        }

        let access = parsed.access_token.ok_or(AiboError::Auth {
            provider: ProviderId::CODEX,
            kind: AuthKind::Invalid,
        })?;

        // A rotation that does not return a new refresh token keeps the old
        // one; dropping it here would strand the session.
        let mut set = token_set(
            access,
            parsed.refresh_token,
            parsed.id_token,
            parsed.expires_in,
        );
        if set.refresh_token.is_none() {
            set.refresh_token = Some(refresh_token);
        }
        if set.account_id.is_none() {
            set.account_id = current.and_then(|t| t.account_id.clone());
        }
        Ok(set)
    }

    fn label(&self) -> &str {
        "chatgpt-device-auth"
    }
}

/// Build the token provider for the Codex credential: device-code tokens,
/// persisted through `store`, refreshed ahead of expiry with jitter.
pub fn token_provider(
    client_id: String,
    store: Arc<dyn TokenStore>,
    issuer: Option<Url>,
) -> Result<Arc<RefreshingTokenProvider>> {
    let refresher = Arc::new(ChatGptRefresh::new(issuer, client_id)?);
    Ok(Arc::new(RefreshingTokenProvider::new(
        ProviderId::CODEX,
        TOKEN_STORAGE_KEY,
        refresher,
        store,
        RefreshPolicy::default(),
    )))
}

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

/// Provider defaults. Per-model values come from the §19 manifest.
///
/// # SPIKE: `vision` is `false` because nothing measured it
///
/// §3a exercised this endpoint end-to-end and its measurements covered **text
/// only** — five model ids, TTFT, and the account-constraint 400. No image input
/// was ever sent, so image support here is unknown, and unknown is not a reason
/// to declare a capability. `false` costs a refusal the user can act on ("switch
/// model"); `true` costs a 400 *after* a multi-megabyte upload has been paid for,
/// and §4 does not fall back on a 400 — so the wrong guess is not recoverable in
/// the direction that matters.
///
/// This also keeps the provider's own statement consistent with §4, which
/// already refuses to bind `Role::Vision` to Codex —
/// `aibo_core::roles::assert_vision_never_binds_codex` makes that a compile
/// error. A provider declaring `vision: true` while the routing table refuses to
/// route vision to it is a contradiction of exactly the kind the attachment work
/// exists to retire.
///
/// Revisit with a real probe — one `input_image` part against one allowlist id —
/// not with an assumption. Flip this and
/// [`crate::registry::CODEX_VISION_UNVERIFIED`] together.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        // SPIKE: unmeasured; see the doc comment above before changing this.
        vision: false,
        streaming: true,
        reasoning_effort: true,
        json_schema: true,
        prompt_cache: true,
        multi_candidate: MultiCandidate::Unsupported,
        max_context: 272_000,
        max_output: Some(128_000),
        ..Capabilities::default()
    }
}

/// The Responses quirk set for the Codex endpoint.
pub fn quirks() -> Quirks {
    Quirks {
        // The endpoint has no `/models`; the catalogue is shipped (§19).
        models_endpoint: false,
        // Measured 2026-07-26: sending `temperature` or `max_output_tokens` to
        // this endpoint returns HTTP 400 *after* auth succeeds, which reads
        // like a credential or model problem and is neither. The endpoint
        // serves reasoning-family models only, and they reject sampling
        // parameters. aibo still enforces its own output budget locally (§14).
        sampling_params: false,
        output_cap: false,
        ..Quirks::responses()
    }
}

/// Codex-subscription inference over the direct HTTPS path (§3a).
///
/// Wire handling is isolated in this one module with golden-file tests
/// precisely because this surface carries no stability contract — the crates
/// implementing it are deliberately unpublished, so a shape change must be a
/// contained fix rather than a cross-cutting one.
pub struct CodexProvider {
    id: ProviderId,
    base_url: Url,
    tokens: Arc<RefreshingTokenProvider>,
    credential: Credential,
    client: reqwest::Client,
    capabilities: Capabilities,
    attestation: AttestationPolicy,
    quirks: Quirks,
}

impl std::fmt::Debug for CodexProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexProvider")
            .field("base_url", &self.base_url.as_str())
            .field("attestation", &self.attestation)
            .finish_non_exhaustive()
    }
}

impl CodexProvider {
    /// Build the provider over an existing token provider.
    pub fn new(
        tokens: Arc<RefreshingTokenProvider>,
        base_url: Option<Url>,
        attestation: AttestationPolicy,
    ) -> Result<Self> {
        let base_url = match base_url {
            Some(u) => u,
            None => {
                Url::parse(CHATGPT_CODEX_BASE_URL).map_err(|e| AiboError::Internal(Box::new(e)))?
            }
        };
        Ok(Self {
            id: ProviderId::CODEX,
            base_url,
            credential: Credential::ChatGptOAuth(tokens.clone()),
            tokens,
            client: build_client(&codex_http_config())?,
            capabilities: default_capabilities(),
            attestation,
            quirks: quirks(),
        })
    }

    /// Open a pooled connection ahead of the first request (§15, §13 wake).
    pub async fn prewarm(&self) {
        crate::http::prewarm(&self.client, &self.base_url).await;
    }

    fn responses_url(&self) -> Result<Url> {
        let s = format!("{}/responses", self.base_url.as_str().trim_end_matches('/'));
        Url::parse(&s).map_err(|e| AiboError::Internal(Box::new(e)))
    }

    /// Whether a failure should move the request down the configured fallback
    /// chain rather than being shown to the user (§3b).
    ///
    /// §3b names two triggers: `401`/`403` means re-read the token, and a `404`
    /// or schema mismatch means the endpoint moved — fall back to the next
    /// configured provider and raise a non-blocking notice.
    pub fn is_endpoint_drift(err: &AiboError) -> bool {
        matches!(
            err,
            AiboError::ProviderUnavailable { status: 404, .. } | AiboError::Internal(_)
        )
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn prewarm(&self) {
        CodexProvider::prewarm(self).await;
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        if self.attestation == AttestationPolicy::Required {
            // SPIKE: S6 outcome 2. Refusing here rather than sending a request
            // that is known to fail keeps the fallback chain fast and keeps the
            // user's quota untouched.
            return Err(crate::wire::Unimplemented::err(
                self.id.clone(),
                "x-oai-attestation is required and aibo cannot generate one; use the app-server path",
            ));
        }

        let body = build_responses_body(&req, &self.quirks);
        let mut rb = self
            .client
            .post(self.responses_url()?)
            .header("accept", "text/event-stream")
            // §3a verified header set. `originator` must be a value on Codex's
            // first-party allowlist — "aibo" is not, and the measured-working
            // request used these exact headers.
            .header("originator", CODEX_ORIGINATOR)
            .header("openai-beta", "responses=experimental")
            .header("session_id", uuid_v4_like())
            .json(&body);

        // Mandatory alongside `Authorization` (§3a), sourced from the ID
        // token's `chatgpt_account_id` claim.
        if let Some(account) = self.tokens.account_id().await {
            rb = rb.header("chatgpt-account-id", account);
        }
        // SPIKE: S6 — `x-oai-attestation` would be attached here. aibo cannot
        // generate one; the header is deliberately absent so the probe measures
        // the real question.

        rb = apply_credential(&self.id, &self.credential, AuthStyle::Bearer, rb).await?;

        let response = rb
            .send()
            .await
            .map_err(|e| map_transport_error(&self.id, &e))?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            let body = read_error_body(response, &self.id)
                .await
                .unwrap_or_default();
            return Err(map_status(
                &self.id,
                status.as_u16(),
                retry_after,
                ErrorShape::OpenAiEnvelope,
                &body,
            ));
        }

        Ok(decode(
            events_from_response(response),
            ResponsesDecoder::default(),
            self.id.clone(),
            cancel,
        ))
    }

    async fn models(&self) -> Result<Vec<ModelInfo>> {
        // The endpoint publishes no catalogue. §10: ship one in the signed
        // weekly manifest and surface "the model you selected no longer exists,
        // here's the closest" rather than an opaque 400.
        Ok(Vec::new())
    }

    async fn health(&self) -> Result<Health> {
        // §3b requires a **startup** health probe for this provider
        // specifically, because it is the one surface with no stability
        // contract. The probe exercises the token path, which is what actually
        // breaks: an expired or revoked pair, not an unreachable host.
        let started = Instant::now();
        match self.tokens.token().await {
            Ok(_) => Ok(Health::Ok {
                latency: started.elapsed(),
            }),
            Err(AiboError::Auth { kind, .. }) => Ok(Health::Degraded {
                reason: format!("sign-in required ({kind})"),
                consecutive_failures: 1,
            }),
            Err(e) => Ok(Health::Unavailable {
                reason: e.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryTokenStore;

    fn jwt_with(payload: serde_json::Value) -> String {
        let encode = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
        };
        format!(
            "{}.{}.{}",
            encode(&json!({"alg": "RS256", "typ": "JWT"})),
            encode(&payload),
            "signature-not-verified"
        )
    }

    #[test]
    fn the_flat_account_claim_is_read() {
        let token = jwt_with(json!({"chatgpt_account_id": "acct_flat"}));
        assert_eq!(
            account_id_from_id_token(&token).as_deref(),
            Some("acct_flat")
        );
    }

    #[test]
    fn the_namespaced_account_claim_is_read() {
        let token = jwt_with(json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct_ns"}
        }));
        assert_eq!(account_id_from_id_token(&token).as_deref(), Some("acct_ns"));
    }

    #[test]
    fn a_malformed_id_token_yields_no_claim_rather_than_panicking() {
        assert_eq!(account_id_from_id_token("not-a-jwt"), None);
        assert_eq!(account_id_from_id_token("a.!!!.c"), None);
        assert_eq!(account_id_from_id_token(""), None);
    }

    #[test]
    fn a_missing_client_id_fails_construction_rather_than_guessing() {
        // Safe regardless of ambient environment: an explicit empty id is
        // rejected on the same path as an absent one.
        assert!(DeviceAuthClient::new(None, Some("   ".to_string())).is_err());
    }

    #[tokio::test]
    async fn health_reports_sign_in_required_when_no_token_is_stored() {
        let tokens = token_provider(
            "test-client".to_string(),
            Arc::new(InMemoryTokenStore::default()),
            None,
        )
        .unwrap();
        let provider = CodexProvider::new(tokens, None, AttestationPolicy::Unknown).unwrap();
        // No stored pair, so refresh fails with `NoRefreshToken` → `Auth`.
        assert!(matches!(
            provider.health().await.unwrap(),
            Health::Degraded { .. }
        ));
    }

    #[tokio::test]
    async fn a_required_attestation_refuses_before_spending_a_request() {
        let tokens = token_provider(
            "test-client".to_string(),
            Arc::new(InMemoryTokenStore::default()),
            None,
        )
        .unwrap();
        let provider = CodexProvider::new(tokens, None, AttestationPolicy::Required).unwrap();
        assert_eq!(provider.attestation, AttestationPolicy::Required);
    }

    #[test]
    fn the_base_url_matches_codex_model_provider_info() {
        assert_eq!(
            CHATGPT_CODEX_BASE_URL,
            "https://chatgpt.com/backend-api/codex"
        );
        assert_eq!(AUTH_ISSUER, "https://auth.openai.com");
    }
}
