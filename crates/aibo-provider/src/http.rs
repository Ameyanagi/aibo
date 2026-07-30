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
    let mut builder = Client::builder();

    // §13: a managed network is a first-class failure mode, not an edge case.
    //
    // reqwest reads `HTTPS_PROXY`/`HTTP_PROXY` from the environment by itself,
    // and that path is left alone — an explicit proxy would override the env
    // vars *and* their `NO_PROXY` exclusions, which is a regression for anyone
    // who already has them set correctly. The system proxy is consulted only
    // when the environment says nothing, which is the case that used to fail
    // with no route and no explanation.
    if !env_proxy_configured()
        && let Some(url) = system_proxy()
    {
        {
            match reqwest::Proxy::all(url) {
                Ok(proxy) => {
                    tracing::info!(proxy = %url, "using the system proxy");
                    builder = builder.proxy(proxy);
                }
                // A malformed proxy is worth saying out loud and then ignoring:
                // failing client construction would take down every provider
                // over a value the user never typed.
                Err(error) => {
                    tracing::warn!(proxy = %url, %error, "ignoring an unusable system proxy");
                }
            }
        }
    }

    builder
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

/// The system proxy, as discovered by the platform layer at startup.
static SYSTEM_PROXY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Record the OS-level proxy, once, before any provider is built.
///
/// A seam rather than a [`HttpConfig`] field because every provider constructs
/// its own config (see `codex_http_config`), so a field would have to be
/// threaded through all of them and each new provider could silently forget it.
/// It is also not something `aibo-provider` can discover for itself: reading it
/// is platform work, and this crate deliberately does not depend on
/// `aibo-platform`.
///
/// Later calls are ignored, so a client built before this ran cannot be
/// contradicted by one built after.
pub fn set_system_proxy(url: Option<String>) {
    let _ = SYSTEM_PROXY.set(url);
}

fn system_proxy() -> Option<&'static str> {
    SYSTEM_PROXY.get().and_then(|value| value.as_deref())
}

/// Whether the environment already specifies a proxy.
///
/// Checked because reqwest honours these on its own, together with `NO_PROXY`.
/// Overriding them with a system proxy would break the exclusion list of anyone
/// who has configured this deliberately.
fn env_proxy_configured() -> bool {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
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
