//! One pooled [`reqwest::Client`] per provider, plus the pre-warm hook §15
//! requires.
//!
//! Two things are deliberate here.
//!
//! **The client is per provider, not global.** Connection pools, timeouts and
//! (for Codex, §3a) cookie state are provider-scoped; a shared client makes a
//! misbehaving endpoint everyone's problem and makes §13's per-provider
//! hysteresis harder to reason about.
//!
//! **There is no whole-request timeout on the streaming path.** `reqwest`'s
//! `timeout` covers the response body too, so setting it would cap the length
//! of a completion rather than its latency. Streaming uses `connect_timeout`
//! plus `read_timeout` (an idle-gap ceiling, which is what
//! [`TimeoutPhase::Stream`] actually means); the non-streaming calls
//! (`models`, `health`) set a per-request timeout at the call site.
//!
//! [`TimeoutPhase::Stream`]: aibo_core::error::TimeoutPhase

use std::time::Duration;

use aibo_core::error::{AiboError, Result, TimeoutPhase};
use aibo_core::types::ProviderId;
use reqwest::Client;
use url::Url;

/// Tunables for a provider's HTTP client.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// TCP + TLS connect ceiling. Short: a connect that has not landed inside
    /// this has already missed every surface budget in §1.
    pub connect_timeout: Duration,
    /// Maximum gap between two reads on a streaming response.
    pub read_timeout: Duration,
    /// Timeout for the small non-streaming calls (`models`, `health`).
    pub request_timeout: Duration,
    /// How long an idle pooled connection is kept. Longer than the typical
    /// gap between hotkey presses, so the second request of a session reuses a
    /// warm TLS session (§15).
    pub pool_idle_timeout: Duration,
    /// Idle connections retained per host.
    pub pool_max_idle_per_host: usize,
    /// `User-Agent`.
    pub user_agent: String,
    /// Refuse plaintext HTTP. Off for [`Credential::LocalEndpoint`] providers,
    /// which are `http://localhost` by definition.
    ///
    /// [`Credential::LocalEndpoint`]: aibo_core::types::Credential::LocalEndpoint
    pub https_only: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(3),
            read_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(10),
            pool_idle_timeout: Duration::from_secs(300),
            pool_max_idle_per_host: 4,
            user_agent: concat!("aibo/", env!("CARGO_PKG_VERSION")).to_string(),
            https_only: true,
        }
    }
}

impl HttpConfig {
    /// The local-endpoint variant: plaintext allowed, generous read timeout
    /// because a cold local model can take seconds to produce its first token
    /// (§13's offline story).
    pub fn local() -> Self {
        Self {
            https_only: false,
            read_timeout: Duration::from_secs(120),
            request_timeout: Duration::from_secs(5),
            ..Self::default()
        }
    }
}

/// Build a pooled client for one provider.
pub fn build_client(cfg: &HttpConfig) -> Result<Client> {
    Client::builder()
        .user_agent(cfg.user_agent.clone())
        .connect_timeout(cfg.connect_timeout)
        .read_timeout(cfg.read_timeout)
        .pool_idle_timeout(cfg.pool_idle_timeout)
        .pool_max_idle_per_host(cfg.pool_max_idle_per_host)
        .https_only(cfg.https_only)
        // Keep the compiled-in Mozilla roots *as well as* the OS store, which
        // the `rustls-tls-native-roots` feature adds. Native-only would trade
        // one failure for another: a machine whose store is empty or
        // unreadable would then trust nothing at all.
        .tls_built_in_root_certs(true)
        .tcp_nodelay(true)
        .build()
        .map_err(|e| AiboError::Internal(Box::new(e)))
}

/// Open a connection so the first real request does not pay for DNS + TCP +
/// TLS.
///
/// Called once after the tray shell reports ready. Wake handling deliberately
/// advances health probes without generating background network traffic.
///
/// Failure is not an error: a cold pool is a slow request, not a broken one.
///
pub async fn prewarm(client: &Client, url: &Url) {
    let mut origin = url.clone();
    origin.set_path("/");
    origin.set_query(None);
    match client
        .head(origin.clone())
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(_) => tracing::debug!(host = %origin, "connection pool warmed"),
        Err(e) => {
            tracing::debug!(host = %origin, error = %e, "pre-warm failed; first request will be cold")
        }
    }
}

/// Classify a `reqwest` transport failure into the failure model (§13).
///
/// Offline is inferred from connect failures, never from a reachability API,
/// and is per-provider — the caller applies the hysteresis.
pub fn map_transport_error(provider: &ProviderId, err: &reqwest::Error) -> AiboError {
    // The classification below is deliberately coarse, and that coarseness cost
    // a real debugging session: a TLS handshake rejected because a corporate
    // root was not trusted arrives here as `is_connect()`, becomes `Offline`,
    // and surfaces as "offline" on a machine with working internet. `Offline`
    // is a unit variant with nowhere to carry why, so the reason is logged with
    // its full source chain instead of being discarded.
    let mut chain = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        chain.push_str(": ");
        chain.push_str(&cause.to_string());
        source = cause.source();
    }
    tracing::warn!(provider = %provider.as_str(), error = %chain, "transport failure");

    if err.is_timeout() {
        let phase = if err.is_connect() {
            TimeoutPhase::Connect
        } else {
            TimeoutPhase::Stream
        };
        return AiboError::Timeout { phase };
    }
    if err.is_connect() || err.is_request() {
        return AiboError::Offline;
    }
    if let Some(status) = err.status() {
        return AiboError::ProviderUnavailable {
            provider: provider.clone(),
            status: status.as_u16(),
            // A transport-level status has no body to explain it; the detail is
            // only available where the response was read (`wire::map_status`).
            detail: None,
        };
    }
    AiboError::Offline
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_builds_for_both_postures() {
        assert!(build_client(&HttpConfig::default()).is_ok());
        assert!(build_client(&HttpConfig::local()).is_ok());
    }

    #[test]
    fn local_config_allows_plaintext() {
        assert!(!HttpConfig::local().https_only);
        assert!(HttpConfig::default().https_only);
    }
}
