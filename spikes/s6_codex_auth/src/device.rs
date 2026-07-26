//! The OAuth 2.0 device-code flow against `auth.openai.com` (§3a).
//!
//! ```text
//! POST {issuer}/api/accounts/deviceauth/usercode   -> { user_code, device_code, ... }
//!      show code, open https://auth.openai.com/codex/device
//! POST {issuer}/api/accounts/deviceauth/token      -> poll until authorised
//! ```
//!
//! SPIKE: S6 — the plan names the two endpoints but **not** their request
//! encoding or exact field names, and nothing here may be treated as verified.
//! Both a JSON body and a form body are implemented and selectable so the
//! operator can find out which one the server accepts, and every raw response
//! is echoed so the real field names end up in the writeup rather than in a
//! guess.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// How the two POST bodies are encoded on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BodyEncoding {
    /// `application/json`.
    Json,
    /// `application/x-www-form-urlencoded` — what RFC 8628 specifies.
    Form,
}

/// The successful half of the `usercode` response.
///
/// Unknown fields are kept in [`Self::raw`] rather than dropped: the point of
/// the spike is to learn the real shape.
#[derive(Debug, Clone)]
pub struct UserCode {
    /// Shown to the human, typed into the consent page.
    pub user_code: String,
    /// Sent back on every poll.
    pub device_code: String,
    /// Where the human should go, if the server names one.
    pub verification_uri: Option<String>,
    /// Seconds between polls, per RFC 8628. Defaults to 5 when absent.
    pub interval_secs: u64,
    /// Seconds until the device code dies. Defaults to 900 when absent.
    pub expires_in_secs: u64,
    /// The whole decoded body, for the writeup.
    pub raw: serde_json::Value,
}

/// The token pair aibo would own after a successful device-code login.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    /// Bearer token for `CHATGPT_CODEX_BASE_URL`.
    pub access_token: String,
    /// Single-use refresh token (§3a: `refresh_token_reused` is a first-class
    /// error state once aibo owns the lifecycle).
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Carries the `chatgpt_account_id` claim.
    #[serde(default)]
    pub id_token: Option<String>,
    /// Lifetime in seconds, when reported.
    #[serde(default)]
    pub expires_in: Option<u64>,
    /// The whole decoded body, for the writeup.
    #[serde(default)]
    pub raw: serde_json::Value,
}

/// Client for the two device-auth endpoints.
pub struct DeviceAuth {
    http: reqwest::Client,
    issuer: String,
    client_id: String,
    encoding: BodyEncoding,
}

impl DeviceAuth {
    /// Build a client. `issuer` is the origin, e.g. `https://auth.openai.com`.
    pub fn new(
        http: reqwest::Client,
        issuer: impl Into<String>,
        client_id: impl Into<String>,
        encoding: BodyEncoding,
    ) -> Self {
        Self {
            http,
            issuer: issuer.into(),
            client_id: client_id.into(),
            encoding,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.issuer.trim_end_matches('/'), path)
    }

    /// Step 1: ask for a user code.
    pub async fn request_user_code(&self, scope: &str) -> Result<UserCode> {
        let url = self.url("/api/accounts/deviceauth/usercode");
        let fields: Vec<(&str, String)> = vec![
            ("client_id", self.client_id.clone()),
            ("scope", scope.to_owned()),
        ];

        let (status, body) = self.post(&url, &fields).await?;
        eprintln!("--- usercode: HTTP {status} ---\n{body}\n");
        if !status.is_success() {
            bail!("usercode endpoint returned HTTP {status}");
        }

        let raw: serde_json::Value =
            serde_json::from_str(&body).context("usercode response was not JSON")?;

        let user_code = pick_str(&raw, &["user_code", "userCode"])
            .context("no user_code field in the usercode response")?;
        let device_code = pick_str(&raw, &["device_code", "deviceCode", "device_auth_id"])
            .context("no device_code field in the usercode response")?;

        Ok(UserCode {
            user_code,
            device_code,
            verification_uri: pick_str(
                &raw,
                &[
                    "verification_uri_complete",
                    "verification_uri",
                    "verificationUri",
                    "url",
                ],
            ),
            interval_secs: pick_u64(&raw, &["interval"]).unwrap_or(5),
            expires_in_secs: pick_u64(&raw, &["expires_in", "expiresIn"]).unwrap_or(900),
            raw,
        })
    }

    /// Step 2: poll until the human approves, the code expires, or the server
    /// returns a terminal error.
    ///
    /// Implements the RFC 8628 poll contract: `authorization_pending` means
    /// keep going, `slow_down` means widen the interval, anything else is
    /// terminal.
    pub async fn poll_for_tokens(&self, code: &UserCode) -> Result<Tokens> {
        let url = self.url("/api/accounts/deviceauth/token");
        let mut interval = Duration::from_secs(code.interval_secs.max(1));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(code.expires_in_secs);

        let fields: Vec<(&str, String)> = vec![
            ("client_id", self.client_id.clone()),
            ("device_code", code.device_code.clone()),
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:device_code".to_owned(),
            ),
        ];

        loop {
            if tokio::time::Instant::now() >= deadline {
                bail!("device code expired before it was approved");
            }
            tokio::time::sleep(interval).await;

            let (status, body) = self.post(&url, &fields).await?;
            let raw: serde_json::Value =
                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
            let error = pick_str(&raw, &["error"]).unwrap_or_default();

            if status.is_success() && raw.get("access_token").is_some() {
                eprintln!("\n--- token: HTTP {status} (authorised) ---");
                let mut tokens: Tokens =
                    serde_json::from_value(raw.clone()).context("token response was not a token")?;
                tokens.raw = raw;
                return Ok(tokens);
            }

            match error.as_str() {
                // RFC 8628: keep polling. An empty error with a non-success
                // status is treated the same way — some servers answer 202 or a
                // bare 400 while waiting, and giving up early would waste the
                // operator's whole login.
                "authorization_pending" | "" => eprint!("."),
                "slow_down" => {
                    interval += Duration::from_secs(5);
                    eprint!("~");
                }
                _ => {
                    eprintln!("\n--- token: HTTP {status} ---\n{body}\n");
                    bail!("device-code poll returned a terminal error: {error:?}");
                }
            }
        }
    }

    async fn post(
        &self,
        url: &str,
        fields: &[(&str, String)],
    ) -> Result<(reqwest::StatusCode, String)> {
        let request = self.http.post(url).header("Accept", "application/json");
        let request = match self.encoding {
            BodyEncoding::Json => {
                let map: serde_json::Map<String, serde_json::Value> = fields
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), serde_json::Value::String(v.clone())))
                    .collect();
                request.json(&serde_json::Value::Object(map))
            }
            BodyEncoding::Form => {
                let pairs: Vec<(&str, &str)> =
                    fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
                request.form(&pairs)
            }
        };

        let response = request.send().await.context("device-auth request failed")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Ok((status, body))
    }
}

fn pick_str(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| value.get(*k).and_then(|v| v.as_str()))
        .map(str::to_owned)
}

fn pick_u64(value: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| value.get(*k).and_then(|v| v.as_u64()))
}
