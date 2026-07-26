//! The live half: one streaming chat-completions call per fixture.
//!
//! Deliberately the *lowest common denominator* wire format — POST
//! `{base_url}/chat/completions` with `stream: true` — so any of the §10
//! OpenAI-compatible providers, plus Ollama, can be swept without the spike
//! growing a provider matrix of its own. `aibo-provider` is where per-backend
//! fidelity belongs; here it would only slow the measurement down.
//!
//! SPIKE: S9 — §5 leaves the FIM question open ("FIM-capable endpoint for
//! Complete only"). This module measures the *instruct-a-chat-model* option
//! only. Measuring a real fill-in-the-middle endpoint needs a second transport
//! and is the obvious next step once the fixture set is real.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt as _;

use crate::prompt::Assembled;

/// Where and how to call a candidate model.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Base URL, e.g. `https://api.openai.com/v1` or `http://localhost:11434/v1`.
    pub base_url: String,
    /// Model id.
    pub model: String,
    /// Bearer token, if the endpoint wants one.
    pub api_key: Option<String>,
    /// Per-request timeout.
    pub timeout: Duration,
}

/// What one call produced.
#[derive(Debug, Clone)]
pub struct Completion {
    /// Concatenated `delta.content`.
    pub text: String,
    /// Milliseconds to the first non-empty content delta.
    pub ttft_ms: Option<u128>,
    /// Milliseconds to the end of the stream.
    pub total_ms: u128,
}

/// Run one assembled prompt against one endpoint.
pub async fn complete(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    assembled: &Assembled,
) -> Result<Completion> {
    let url = format!(
        "{}/chat/completions",
        endpoint.base_url.trim_end_matches('/')
    );
    let mut body = serde_json::json!({
        "model": endpoint.model,
        "stream": true,
        "temperature": assembled.temperature,
        "max_tokens": assembled.max_tokens,
        "messages": [
            { "role": "system", "content": assembled.system },
            { "role": "user", "content": assembled.user },
        ],
    });
    if !assembled.stop.is_empty() {
        body["stop"] = serde_json::json!(assembled.stop);
    }

    let mut request = http
        .post(&url)
        .timeout(endpoint.timeout)
        .header("Accept", "text/event-stream")
        .json(&body);
    if let Some(key) = &endpoint.api_key {
        request = request.header("Authorization", format!("Bearer {key}"));
    }

    let started = Instant::now();
    let response = request.send().await.context("request failed")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "HTTP {status}: {}",
            body.chars().take(400).collect::<String>()
        );
    }

    let mut stream = response.bytes_stream();
    let mut pending = String::new();
    let mut text = String::new();
    let mut ttft_ms = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("stream broke")?;
        pending.push_str(&String::from_utf8_lossy(&chunk));

        // SSE frames are separated by a blank line; keep the trailing partial.
        while let Some(index) = pending.find("\n\n") {
            let frame: String = pending.drain(..index + 2).collect();
            for line in frame.lines() {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                let delta = value
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"))
                    .and_then(|d| d.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or_default();
                if !delta.is_empty() {
                    if ttft_ms.is_none() {
                        ttft_ms = Some(started.elapsed().as_millis());
                    }
                    text.push_str(delta);
                }
            }
        }
    }

    Ok(Completion {
        text,
        ttft_ms,
        total_ms: started.elapsed().as_millis(),
    })
}

/// Build the HTTP client used for the sweep.
///
/// One pooled client for the whole run: reconnecting per request would make the
/// TTFT column measure TLS handshakes rather than models.
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .context("failed to build the HTTP client")
}
