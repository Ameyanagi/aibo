//! One Responses call to `CHATGPT_CODEX_BASE_URL`, repeated across a small
//! header matrix.
//!
//! This is the whole point of S6 (§20):
//!
//! > **Does it succeed without `x-oai-attestation`?**
//!
//! The matrix exists because "it failed" is not an answer. Four variants
//! separate the plausible causes of a rejection:
//!
//! | Variant | Isolates |
//! |---|---|
//! | `minimal` | the actual go/no-go: bearer + account id, nothing else |
//! | `no_account_id` | whether `ChatGPT-Account-ID` is genuinely mandatory |
//! | `codex_like` | whether the originator / beta / UA headers are load-bearing |
//! | `bogus_attestation` | whether the backend *validates* the attestation or merely checks that it is present — a presence check and a signature check are very different outcomes for §3a |
//!
//! SPIKE: S6 — the request body below is a plausible Responses payload, not a
//! verified one. Read `codex-api` in `openai/codex` for the real shape before
//! trusting a 400.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt as _;
use serde::Serialize;

/// `codex-model-provider-info::CHATGPT_CODEX_BASE_URL` (§3a).
pub const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Response headers worth carrying into the writeup.
const INTERESTING_HEADERS: &[&str] = &[
    "www-authenticate",
    "cf-ray",
    "cf-mitigated",
    "x-request-id",
    "x-ratelimit-limit-requests",
    "retry-after",
    "content-type",
    "server",
];

/// One row of the header matrix.
#[derive(Debug, Clone)]
pub struct Variant {
    /// Short name used in the result table.
    pub name: &'static str,
    /// What a pass or fail on this row would mean.
    pub isolates: &'static str,
    /// Whether `ChatGPT-Account-ID` is sent.
    pub account_id: bool,
    /// Whether the Codex-client-ish headers are sent.
    pub codex_headers: bool,
    /// Whether a deliberately invalid `x-oai-attestation` is sent.
    pub bogus_attestation: bool,
}

/// The four variants, in the order they should be run and reported.
pub fn variants() -> Vec<Variant> {
    vec![
        Variant {
            name: "minimal",
            isolates: "the go/no-go: Authorization + ChatGPT-Account-ID, no attestation",
            account_id: true,
            codex_headers: false,
            bogus_attestation: false,
        },
        Variant {
            name: "no_account_id",
            isolates: "is ChatGPT-Account-ID really mandatory?",
            account_id: false,
            codex_headers: false,
            bogus_attestation: false,
        },
        Variant {
            name: "codex_like",
            isolates: "are originator / OpenAI-Beta / User-Agent load-bearing?",
            account_id: true,
            codex_headers: true,
            bogus_attestation: false,
        },
        Variant {
            name: "bogus_attestation",
            isolates: "does the backend validate the attestation or only check presence?",
            account_id: true,
            codex_headers: true,
            bogus_attestation: true,
        },
    ]
}

/// Everything one variant produced, ready to be serialised into the report.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    /// Variant name.
    pub variant: String,
    /// HTTP status, or `None` if the request never completed.
    pub status: Option<u16>,
    /// Selected response headers.
    pub headers: Vec<(String, String)>,
    /// Milliseconds from send to response headers.
    pub headers_ms: u128,
    /// Milliseconds from send to the first SSE `data:` payload, when streaming.
    pub ttft_ms: Option<u128>,
    /// The first part of the body (truncated); for a stream, the first events.
    pub body_head: String,
    /// Transport-level failure, if any.
    pub transport_error: Option<String>,
}

impl ProbeResult {
    /// Did the endpoint accept the request?
    pub fn accepted(&self) -> bool {
        matches!(self.status, Some(s) if (200..300).contains(&s))
    }
}

/// Inputs shared by every variant.
pub struct ProbeConfig {
    /// Base URL; defaults to [`CHATGPT_CODEX_BASE_URL`].
    pub base_url: String,
    /// Bearer token from the device flow.
    pub access_token: String,
    /// `chatgpt_account_id` claim, when one was found.
    pub account_id: Option<String>,
    /// Model id to ask for.
    pub model: String,
    /// Whether to request an SSE stream (which is what gives a TTFT number).
    pub stream: bool,
    /// How long to wait before giving up on a variant.
    pub timeout: Duration,
    /// Bytes of body to keep.
    pub body_head_bytes: usize,
}

/// The Responses request body.
///
/// SPIKE: S6 — field-for-field this is the shape `codex-api` appears to send,
/// but it is [unverified]. If every variant returns 400 with a schema
/// complaint, fix this before concluding anything about attestation.
fn request_body(config: &ProbeConfig) -> serde_json::Value {
    serde_json::json!({
        "model": config.model,
        "instructions": "You are a terse assistant.",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": "Reply with exactly: pong" }]
        }],
        "tools": [],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        // Codex sets `store: false`; a ChatGPT-subscription account has no
        // Responses storage to write to.
        "store": false,
        "stream": config.stream,
    })
}

/// Run a single variant.
pub async fn run_variant(
    http: &reqwest::Client,
    config: &ProbeConfig,
    variant: &Variant,
) -> ProbeResult {
    let url = format!("{}/responses", config.base_url.trim_end_matches('/'));
    let mut request = http
        .post(&url)
        .timeout(config.timeout)
        .header("Authorization", format!("Bearer {}", config.access_token))
        .header("Content-Type", "application/json")
        .header(
            "Accept",
            if config.stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        );

    if variant.account_id
        && let Some(id) = &config.account_id
    {
        request = request.header("ChatGPT-Account-ID", id);
    }
    if variant.codex_headers {
        // SPIKE: S6 — these are the headers Codex is understood to send. Confirm
        // the exact set and values with mitmproxy + a trusted CA (§3b); the
        // plan explicitly rules out `codex-responses-api-proxy` for this.
        request = request
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "codex_cli_rs")
            .header("session_id", "00000000-0000-4000-8000-000000000000")
            .header("User-Agent", "codex_cli_rs/0.0.0 (aibo-spike-s6)");
    }
    if variant.bogus_attestation {
        request = request.header("x-oai-attestation", "s6-deliberately-invalid");
    }

    let body = request_body(config);
    let started = Instant::now();
    let response = match request.json(&body).send().await {
        Ok(response) => response,
        Err(error) => {
            return ProbeResult {
                variant: variant.name.to_owned(),
                status: None,
                headers: Vec::new(),
                headers_ms: started.elapsed().as_millis(),
                ttft_ms: None,
                body_head: String::new(),
                transport_error: Some(error.to_string()),
            };
        }
    };

    let headers_ms = started.elapsed().as_millis();
    let status = response.status().as_u16();
    let headers = INTERESTING_HEADERS
        .iter()
        .filter_map(|name| {
            response
                .headers()
                .get(*name)
                .and_then(|v| v.to_str().ok())
                .map(|v| ((*name).to_owned(), v.to_owned()))
        })
        .collect();

    let (ttft_ms, body_head, transport_error) =
        read_head(response, started, config.body_head_bytes).await;

    ProbeResult {
        variant: variant.name.to_owned(),
        status: Some(status),
        headers,
        headers_ms,
        ttft_ms,
        body_head,
        transport_error,
    }
}

/// Read the beginning of the body, timing the first SSE payload on the way.
///
/// The TTFT number is what §20 asks for ("TTFT vs Cerebras") and it is only
/// meaningful when the first `data:` line — not merely the first byte — arrives.
async fn read_head(
    response: reqwest::Response,
    started: Instant,
    limit: usize,
) -> (Option<u128>, String, Option<String>) {
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::with_capacity(limit.min(64 * 1024));
    let mut ttft: Option<u128> = None;
    let mut error = None;

    while buffer.len() < limit {
        match stream.next().await {
            None => break,
            Some(Err(e)) => {
                error = Some(e.to_string());
                break;
            }
            Some(Ok(chunk)) => {
                buffer.extend_from_slice(&chunk);
                if ttft.is_none() && has_sse_payload(&buffer) {
                    ttft = Some(started.elapsed().as_millis());
                }
            }
        }
    }

    buffer.truncate(limit);
    let text = String::from_utf8_lossy(&buffer).into_owned();
    (ttft, text, error)
}

/// Has a non-empty SSE `data:` line arrived yet?
fn has_sse_payload(buffer: &[u8]) -> bool {
    String::from_utf8_lossy(buffer).lines().any(|line| {
        line.strip_prefix("data:")
            .is_some_and(|rest| !rest.trim().is_empty())
    })
}

/// Render the matrix as the markdown table that belongs in the go/no-go note.
pub fn markdown_table(results: &[ProbeResult], variants: &[Variant]) -> String {
    let mut out = String::from(
        "| Variant | Status | TTFT (ms) | Headers (ms) | Verdict | Isolates |\n\
         |---|---|---|---|---|---|\n",
    );
    for result in results {
        let isolates = variants
            .iter()
            .find(|v| v.name == result.variant)
            .map(|v| v.isolates)
            .unwrap_or("");
        let status = result
            .status
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("transport error: {:?}", result.transport_error));
        let ttft = result
            .ttft_ms
            .map(|ms| ms.to_string())
            .unwrap_or_else(|| "-".into());
        let verdict = if result.accepted() {
            "accepted"
        } else {
            "rejected"
        };
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | **{}** | {} |\n",
            result.variant, status, ttft, result.headers_ms, verdict, isolates
        ));
    }
    out
}

/// Map the matrix onto the three §3a outcomes so the operator writes down a
/// decision rather than a pile of status codes.
pub fn outcome(results: &[ProbeResult]) -> &'static str {
    let find = |name: &str| results.iter().find(|r| r.variant == name);
    let minimal_ok = find("minimal").is_some_and(ProbeResult::accepted);
    let codex_like_ok = find("codex_like").is_some_and(ProbeResult::accepted);
    let bogus_ok = find("bogus_attestation").is_some_and(ProbeResult::accepted);

    if minimal_ok {
        "OUTCOME 1 — direct endpoint works with device-code tokens and no attestation. \
         Ship the §3a design: aibo's own login, own token lifecycle, pooled HTTPS."
    } else if codex_like_ok {
        "OUTCOME 1 (conditional) — the call succeeds, but only with the Codex-client headers. \
         Record exactly which header flipped it; treat that set as part of the wire contract."
    } else if bogus_ok {
        "OUTCOME 1 (fragile) — an INVALID attestation was accepted, so the backend checks \
         presence, not validity. That is a hole that will close. Ship behind the §3b fallback \
         chain and do not make it the default Fast binding."
    } else {
        "OUTCOME 2 or 3 — the direct endpoint rejects device-code auth. Subscription inference \
         falls back to `codex app-server` (tools disabled, approvalPolicy \"never\"), or Codex \
         stays agent-only. Check the bodies below for whether the rejection names attestation."
    }
}

/// Build the HTTP client used for every request.
///
/// `cookie_store(true)` is deliberate — §3a lists Codex's process-global
/// `chatgpt.com` cookie jar as one of the unknowns, and the spike must be able
/// to hold one to tell whether the endpoint needs it.
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .context("failed to build the HTTP client")
}
