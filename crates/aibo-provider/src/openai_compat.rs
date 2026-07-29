//! The shared OpenAI-compatible transport, parameterised by base URL and quirk
//! flags (§10).
//!
//! **"One OpenAI-compatible module covers seven providers" was over-optimistic.**
//! What this module shares is the HTTP plumbing, the SSE plumbing, the
//! cancellation contract and the usage normalisation. What it *parameterises*
//! is the list §10 gives as the reason P3 slipped:
//!
//! | Difference | Expressed as |
//! |---|---|
//! | Responses vs Chat Completions | [`WireFormat`] |
//! | Azure deployment URLs + `api-version` | [`UrlStyle`] |
//! | SSE framing / terminator conventions | [`Quirks::done_sentinel`] and the decoders |
//! | Where and whether `usage` appears | [`UsagePlacement`] |
//! | Tool-call encoding | [`WireFormat`] plus [`Quirks::tools`] |
//! | Error body shapes | [`ErrorShape`] |
//! | Reasoning-token handling | [`ReasoningStyle`] |
//! | Model catalogues | [`Quirks::models_endpoint`] and the §19 manifest |
//!
//! Every concrete provider is a [`Quirks`] constructor plus a base URL. When a
//! provider is found to differ in a way this table cannot express, add a flag
//! here rather than forking the module — but expect to add flags.
//!
//! [`ErrorShape`]: crate::wire::ErrorShape

pub mod cerebras;
pub mod groq;
pub mod openai;
pub mod openrouter;
pub mod sambanova;
pub mod xai;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aibo_core::error::{AiboError, Result};
use aibo_core::traits::Provider;
use aibo_core::types::{
    BoxStream, Capabilities, ChatRequest, ContentPart, Credential, Health, Message, MessageRole,
    ModelInfo, MultiCandidate, ProviderId, ReasoningEffort, StopReason, StreamEvent, Usage,
};
use async_trait::async_trait;
use eventsource_stream::Event;
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth::{AuthStyle, apply_credential};
use crate::http::{HttpConfig, build_client, map_transport_error};
use crate::sse::{
    DONE_SENTINEL, Flow, MAX_BUFFERED_TOOL_BYTES, MAX_TOOL_CALLS_PER_RESPONSE, SseDecoder, decode,
    events_from_response, read_error_body,
};
use crate::wire::{
    ErrorShape, OpenAiUsage, Unimplemented, data_uri, flatten_text, has_image, map_status,
    parse_retry_after, parse_tool_args, render_untrusted, role_name,
};

// ---------------------------------------------------------------------------
// Quirks
// ---------------------------------------------------------------------------

/// Which request/response shape the endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    /// `POST /chat/completions` with `{messages, choices[].delta}`.
    ChatCompletions,
    /// `POST /responses` with `{input, output_*}` events. OpenAI's native
    /// format and the one `CHATGPT_CODEX_BASE_URL` speaks (§3a).
    Responses,
}

/// How the request path is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlStyle {
    /// `{base}/chat/completions`, `{base}/responses`, `{base}/models`.
    PathSuffix,
    /// Azure: `{base}/openai/deployments/{deployment}/{op}?api-version={v}`.
    ///
    /// The deployment name is **not** the model id, and `api-version` matters
    /// (§10) — an omitted or stale one is a 400, not a warning.
    AzureDeployment {
        /// Deployment name from the Azure resource.
        deployment: String,
        /// `api-version` query parameter.
        api_version: String,
    },
}

/// Where `usage` shows up in a stream, which decides whether the spend meter
/// gets real numbers or only the reservation made at dispatch (§14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsagePlacement {
    /// Present on the final chunk without being asked for.
    FinalChunk,
    /// Only sent when the request carries `stream_options.include_usage`.
    /// Omitting the opt-in silently under-reports spend.
    RequiresStreamOptions,
    /// Carried on the `response.completed` event (Responses wire format).
    ResponseCompleted,
    /// Never reported. The meter falls back to the dispatch-time reservation.
    Absent,
}

/// How reasoning text is delivered, so it can be routed to
/// [`StreamEvent::Reasoning`] and rendered collapsed rather than inserted (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningStyle {
    /// The endpoint never emits reasoning text.
    None,
    /// `choices[].delta.reasoning_content` — the DeepSeek-derived spelling,
    /// which several OpenAI-compatible hosts adopted.
    DeltaReasoningContent,
    /// `choices[].delta.reasoning` — xAI's spelling.
    DeltaReasoning,
    /// Responses `response.reasoning_summary_text.delta` /
    /// `response.reasoning_text.delta`.
    ResponsesItem,
}

/// The per-provider deltas from plain OpenAI Chat Completions.
///
/// Fields marked `[unverified]` in the concrete constructors are the ones a
/// golden fixture captured from real traffic must confirm; the defaults are the
/// conservative choice (ask for less, tolerate more) rather than a claim about
/// the endpoint.
#[derive(Debug, Clone)]
pub struct Quirks {
    /// Request/response shape.
    pub wire: WireFormat,
    /// Path construction.
    pub url: UrlStyle,
    /// How the credential is presented.
    pub auth: AuthStyle,
    /// Where `usage` appears.
    pub usage: UsagePlacement,
    /// Error body shape.
    pub error_shape: ErrorShape,
    /// Reasoning delivery.
    pub reasoning: ReasoningStyle,
    /// The endpoint terminates the stream with `data: [DONE]`.
    pub done_sentinel: bool,
    /// The output cap is spelled `max_completion_tokens` rather than
    /// `max_tokens`. OpenAI renamed it and rejects the old spelling on its
    /// newer models; most compatible hosts still take `max_tokens` only.
    pub max_completion_tokens: bool,
    /// Function/tool calling is accepted.
    pub tools: bool,
    /// `response_format: {type: json_schema, …}` is accepted.
    pub json_schema: bool,
    /// `seed` is accepted.
    pub seed: bool,
    /// `stop` is accepted.
    pub stop: bool,
    /// `n > 1` behaviour. "Three candidates" is not portable (§5): some hosts
    /// honour `n`, some ignore it, some charge for it.
    pub multi_candidate: MultiCandidate,
    /// `reasoning_effort` (Chat Completions) / `reasoning.effort` (Responses)
    /// is accepted.
    pub reasoning_effort: bool,
    /// `GET {base}/models` exists.
    pub models_endpoint: bool,
    /// Sampling parameters (`temperature`, `top_p`) are accepted.
    ///
    /// **False for the ChatGPT-backed Codex endpoint.** It serves only
    /// reasoning-family models, which reject `temperature` outright — the
    /// request fails with `HTTP 400` *after* auth has succeeded, which reads
    /// like a model or credential problem and is neither. Observed 2026-07-26:
    /// a request that omitted `temperature` and `max_output_tokens` returned
    /// 200 against the same endpoint, model and token that a request including
    /// them 400'd on.
    pub sampling_params: bool,
    /// An explicit output cap (`max_output_tokens` / `max_tokens`) is accepted.
    ///
    /// Also false for Codex — see [`Self::sampling_params`]. Aibo retains a
    /// finite planning reserve and a hard byte-safety ceiling, but cannot ask
    /// that server to enforce a token limit.
    pub output_cap: bool,
    /// Extra headers sent on every request.
    pub extra_headers: Vec<(String, String)>,
}

impl Quirks {
    /// The baseline: plain Chat Completions, bearer auth, `[DONE]` terminator,
    /// usage only when `stream_options.include_usage` asks for it.
    pub fn chat_completions() -> Self {
        Self {
            wire: WireFormat::ChatCompletions,
            url: UrlStyle::PathSuffix,
            auth: AuthStyle::Bearer,
            usage: UsagePlacement::RequiresStreamOptions,
            error_shape: ErrorShape::OpenAiEnvelope,
            reasoning: ReasoningStyle::None,
            done_sentinel: true,
            max_completion_tokens: false,
            tools: true,
            json_schema: false,
            seed: false,
            stop: true,
            multi_candidate: MultiCandidate::Unsupported,
            reasoning_effort: false,
            models_endpoint: true,
            // Every ordinary OpenAI-compatible host takes both; only the
            // ChatGPT-backed Codex endpoint refuses them (see the field docs).
            sampling_params: true,
            output_cap: true,
            extra_headers: Vec::new(),
        }
    }

    /// The baseline for the Responses wire format.
    pub fn responses() -> Self {
        Self {
            wire: WireFormat::Responses,
            usage: UsagePlacement::ResponseCompleted,
            reasoning: ReasoningStyle::ResponsesItem,
            done_sentinel: false,
            reasoning_effort: true,
            json_schema: true,
            ..Self::chat_completions()
        }
    }

    /// Add a fixed header to every request.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Resolve the URL for one operation, honouring [`UrlStyle`].
    fn op_url(&self, base: &Url, op: Op) -> Result<Url> {
        match &self.url {
            UrlStyle::PathSuffix => join(base, op.path_suffix()),
            UrlStyle::AzureDeployment {
                deployment,
                api_version,
            } => {
                let mut u = match op {
                    // Azure lists models at the resource level, not per
                    // deployment.
                    Op::Models => join(base, "openai/models")?,
                    _ => join(
                        base,
                        &format!("openai/deployments/{deployment}/{}", op.path_suffix()),
                    )?,
                };
                u.query_pairs_mut().append_pair("api-version", api_version);
                Ok(u)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Chat,
    Responses,
    Models,
}

impl Op {
    const fn path_suffix(self) -> &'static str {
        match self {
            Op::Chat => "chat/completions",
            Op::Responses => "responses",
            Op::Models => "models",
        }
    }
}

/// Join a path suffix onto a base URL without `Url::join`'s last-segment
/// replacement, which silently eats `/v1` from `https://host/v1`.
fn join(base: &Url, suffix: &str) -> Result<Url> {
    let mut s = base.as_str().trim_end_matches('/').to_string();
    s.push('/');
    s.push_str(suffix);
    Url::parse(&s).map_err(|e| AiboError::Internal(Box::new(e)))
}

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

/// An OpenAI-compatible backend: a base URL, a [`Quirks`] set, a credential and
/// its own pooled client.
pub struct OpenAiCompat {
    id: ProviderId,
    base_url: Url,
    quirks: Quirks,
    credential: Credential,
    client: reqwest::Client,
    http: HttpConfig,
    capabilities: Capabilities,
    static_models: Vec<ModelInfo>,
}

impl std::fmt::Debug for OpenAiCompat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompat")
            .field("id", &self.id)
            .field("base_url", &self.base_url.as_str())
            .field("quirks", &self.quirks)
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompat {
    /// Build a provider with its own connection pool.
    pub fn new(
        id: ProviderId,
        base_url: Url,
        quirks: Quirks,
        credential: Credential,
        http: HttpConfig,
    ) -> Result<Self> {
        Ok(Self {
            id,
            base_url,
            quirks,
            credential,
            client: build_client(&http)?,
            http,
            capabilities: Capabilities::default(),
            static_models: Vec::new(),
        })
    }

    /// Override the provider's default capabilities.
    ///
    /// §10: capabilities are per-model. These are only the fallback used before
    /// the catalogue is known.
    pub fn with_capabilities(mut self, caps: Capabilities) -> Self {
        self.capabilities = caps;
        self
    }

    /// Supply a shipped model list, used when the endpoint exposes no
    /// catalogue and as the fallback when it is unreachable.
    pub fn with_static_models(mut self, models: Vec<ModelInfo>) -> Self {
        self.static_models = models;
        self
    }

    /// The base URL, for pre-warming (§15).
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// The quirk set, exposed for tests and for the registry's diagnostics.
    pub fn quirks(&self) -> &Quirks {
        &self.quirks
    }

    /// The pooled client, so a wrapping provider can reuse it.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Open a pooled connection ahead of the first request (§15; re-run on
    /// [`PowerEvent::DidWake`], §13).
    ///
    /// [`PowerEvent::DidWake`]: aibo_core::types::PowerEvent::DidWake
    pub async fn prewarm(&self) {
        crate::http::prewarm(&self.client, &self.base_url).await;
    }

    async fn send(&self, url: Url, body: Value) -> Result<reqwest::Response> {
        let mut rb = self
            .client
            .post(url)
            .header("accept", "text/event-stream")
            .json(&body);
        for (k, v) in &self.quirks.extra_headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        rb = apply_credential(&self.id, &self.credential, self.quirks.auth, rb).await?;

        let response = rb
            .send()
            .await
            .map_err(|e| map_transport_error(&self.id, &e))?;
        self.check_status(response).await
    }

    async fn check_status(&self, response: reqwest::Response) -> Result<reqwest::Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        let body = read_error_body(response, &self.id)
            .await
            .unwrap_or_default();
        Err(map_status(
            &self.id,
            status.as_u16(),
            retry_after,
            self.quirks.error_shape,
            &body,
        ))
    }
}

#[async_trait]
impl Provider for OpenAiCompat {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn prewarm(&self) {
        OpenAiCompat::prewarm(self).await;
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        // Refuse before spending anything, and never by stripping the image
        // (§10). The authoritative per-model check runs in dispatch with real
        // `ModelInfo` capabilities and real alternatives; this is the last line
        // of defence, so it can only offer what the provider itself declares.
        crate::attachment::guard(&self.capabilities, &req, Vec::new())?;
        // Downscale before the bytes are base64'd into the body (§14): a 4 MB
        // retina capture is billed as ~19k image tokens and may be refused
        // outright, and §4 does not fall back on a 400.
        let req = crate::attachment::prepare(req).await?;

        let (url, body) = match self.quirks.wire {
            WireFormat::ChatCompletions => (
                self.quirks.op_url(&self.base_url, Op::Chat)?,
                build_chat_completions_body(&req, &self.quirks),
            ),
            WireFormat::Responses => (
                self.quirks.op_url(&self.base_url, Op::Responses)?,
                build_responses_body(&req, &self.quirks),
            ),
        };

        let response = self.send(url, body).await?;
        let events = events_from_response(response);

        Ok(match self.quirks.wire {
            WireFormat::ChatCompletions => decode(
                events,
                ChatCompletionsDecoder::new(self.quirks.clone()),
                self.id.clone(),
                cancel,
            ),
            WireFormat::Responses => {
                decode(events, ResponsesDecoder::default(), self.id.clone(), cancel)
            }
        })
    }

    async fn models(&self) -> Result<Vec<ModelInfo>> {
        if !self.quirks.models_endpoint {
            return Ok(self.static_models.clone());
        }
        let url = self.quirks.op_url(&self.base_url, Op::Models)?;
        let mut rb = self.client.get(url).timeout(self.http.request_timeout);
        for (k, v) in &self.quirks.extra_headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        rb = apply_credential(&self.id, &self.credential, self.quirks.auth, rb).await?;
        let response = rb
            .send()
            .await
            .map_err(|e| map_transport_error(&self.id, &e))?;
        let response = self.check_status(response).await?;
        let body: ModelsResponse = response
            .json()
            .await
            .map_err(|e| map_transport_error(&self.id, &e))?;

        Ok(body
            .data
            .into_iter()
            .map(|m| ModelInfo {
                provider: self.id.clone(),
                display_name: m.id.clone(),
                id: m.id,
                // No provider in the §10 matrix returns capability information
                // from `/models`. The authoritative values come from the signed
                // weekly manifest (§19); this is the floor until it lands.
                capabilities: self.capabilities.clone(),
                deprecated: false,
                replaced_by: None,
            })
            .collect())
    }

    async fn health(&self) -> Result<Health> {
        let started = Instant::now();
        let url = if self.quirks.models_endpoint {
            self.quirks.op_url(&self.base_url, Op::Models)?
        } else {
            self.base_url.clone()
        };
        // A health probe must not outlive the surface budget it protects.
        let mut rb = self
            .client
            .get(url)
            .timeout(self.http.request_timeout.min(Duration::from_secs(5)));
        rb = apply_credential(&self.id, &self.credential, self.quirks.auth, rb).await?;

        match rb.send().await {
            Ok(r) if r.status().is_success() => Ok(Health::Ok {
                latency: started.elapsed(),
            }),
            Ok(r) => Ok(Health::Degraded {
                reason: format!("HTTP {}", r.status().as_u16()),
                consecutive_failures: 1,
            }),
            // §13: distinguish connect failure from timeout in `reason`; a
            // reachability API lies. The caller applies the hysteresis.
            Err(e) => Ok(Health::Unavailable {
                reason: if e.is_connect() {
                    "connect failed".into()
                } else if e.is_timeout() {
                    "timed out".into()
                } else {
                    "request failed".into()
                },
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

const fn effort_str(e: ReasoningEffort) -> &'static str {
    match e {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
    }
}

/// Build a Chat Completions request body.
///
/// Pure, so `tests/request_shape.rs` can pin it against a golden file: a
/// silently renamed field is a 400 in production and a diff here.
///
/// [`ChatRequest::attachments`] are folded into the last user message by
/// [`crate::attachment::fold_into_messages`] and encoded as `image_url` parts
/// carrying a `data:` URL — the shape every OpenAI-compatible endpoint takes.
/// The pixels are used as attached, so call [`crate::attachment::prepare`]
/// first; `Provider::chat` does.
pub fn build_chat_completions_body(req: &ChatRequest, q: &Quirks) -> Value {
    let messages = crate::attachment::fold_into_messages(req);
    let mut body = json!({
        "model": req.binding.model,
        "messages": messages.iter().map(chat_message).collect::<Vec<_>>(),
        "stream": true,
        "temperature": req.params.temperature,
    });
    let obj = body.as_object_mut().expect("object literal");

    if req.params.max_tokens > 0 {
        let max_tokens = req.params.max_tokens.min(req.budget.max_output_tokens);
        if q.max_completion_tokens {
            obj.insert("max_completion_tokens".into(), json!(max_tokens));
        } else {
            obj.insert("max_tokens".into(), json!(max_tokens));
        }
    }

    if let Some(p) = req.params.top_p {
        obj.insert("top_p".into(), json!(p));
    }
    if q.stop && !req.params.stop.is_empty() {
        obj.insert("stop".into(), json!(req.params.stop));
    }
    if q.seed
        && let Some(seed) = req.params.seed
    {
        obj.insert("seed".into(), json!(seed));
    }
    if q.reasoning_effort
        && let Some(e) = req.params.reasoning_effort
    {
        obj.insert("reasoning_effort".into(), json!(effort_str(e)));
    }
    if q.json_schema
        && let Some(schema) = &req.params.json_schema
    {
        obj.insert(
            "response_format".into(),
            json!({"type": "json_schema", "json_schema": schema}),
        );
    }
    if req.params.candidates > 1 && q.multi_candidate == MultiCandidate::Native {
        obj.insert("n".into(), json!(req.params.candidates));
    }
    if q.tools && !req.tools.is_empty() {
        obj.insert(
            "tools".into(),
            json!(
                req.tools
                    .iter()
                    .map(|t| json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }
    if q.usage == UsagePlacement::RequiresStreamOptions {
        obj.insert("stream_options".into(), json!({"include_usage": true}));
    }

    body
}

fn chat_message(msg: &Message) -> Value {
    let mut out = json!({ "role": role_name(msg.role) });
    let obj = out.as_object_mut().expect("object literal");

    if has_image(msg) {
        let parts: Vec<Value> = msg
            .parts
            .iter()
            .map(|p| match p {
                ContentPart::Text(t) => json!({"type": "text", "text": t}),
                ContentPart::Untrusted(b) => json!({"type": "text", "text": render_untrusted(b)}),
                ContentPart::Image { mime, data_base64 } => json!({
                    "type": "image_url",
                    "image_url": { "url": data_uri(mime, data_base64) }
                }),
            })
            .collect();
        obj.insert("content".into(), Value::Array(parts));
    } else {
        obj.insert("content".into(), json!(flatten_text(msg)));
    }

    if let Some(id) = &msg.tool_call_id {
        obj.insert("tool_call_id".into(), json!(id));
    }
    out
}

/// Build a Responses request body.
///
/// The system prompt becomes `instructions` rather than a message, which is the
/// shape `codex-api` uses against `CHATGPT_CODEX_BASE_URL` (§3a — read
/// `codex-api` as the reference rather than guessing).
///
/// `store: false` is deliberate: aibo keeps its own encrypted history (§12) and
/// has no reason to leave a server-side copy.
///
/// [`ChatRequest::attachments`] become `input_image` content parts whose
/// `image_url` is a `data:` URL — the Responses spelling, which differs from
/// Chat Completions (an object, not a string) and from Anthropic (a `source`
/// block). §10 keeps the three implementations separate for exactly this reason.
/// Call [`crate::attachment::prepare`] first so the bytes are the downscaled
/// ones; `Provider::chat` does.
pub fn build_responses_body(req: &ChatRequest, q: &Quirks) -> Value {
    let mut instructions = String::new();
    let mut input: Vec<Value> = Vec::new();

    let messages = crate::attachment::fold_into_messages(req);
    for msg in messages.iter() {
        if msg.role == MessageRole::System {
            if !instructions.is_empty() {
                instructions.push_str("\n\n");
            }
            instructions.push_str(&flatten_text(msg));
            continue;
        }
        input.push(responses_message(msg));
    }

    let mut body = json!({
        "model": req.binding.model,
        "input": input,
        "stream": true,
        "store": false,
    });
    let obj = body.as_object_mut().expect("object literal");

    // Gated: the ChatGPT-backed Codex endpoint 400s on either of these, after
    // auth has already succeeded. See `Quirks::sampling_params`.
    if q.output_cap && req.params.max_tokens > 0 {
        obj.insert(
            "max_output_tokens".into(),
            json!(req.params.max_tokens.min(req.budget.max_output_tokens)),
        );
    }
    if q.sampling_params {
        obj.insert("temperature".into(), json!(req.params.temperature));
    }

    if !instructions.is_empty() {
        obj.insert("instructions".into(), json!(instructions));
    }
    if q.sampling_params
        && let Some(p) = req.params.top_p
    {
        obj.insert("top_p".into(), json!(p));
    }
    if q.reasoning_effort
        && let Some(e) = req.params.reasoning_effort
    {
        obj.insert("reasoning".into(), json!({"effort": effort_str(e)}));
    }
    if q.json_schema
        && let Some(schema) = &req.params.json_schema
    {
        obj.insert(
            "text".into(),
            json!({"format": {"type": "json_schema", "schema": schema}}),
        );
    }
    if q.tools && !req.tools.is_empty() {
        obj.insert(
            "tools".into(),
            json!(
                req.tools
                    .iter()
                    .map(|t| json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }

    body
}

fn responses_message(msg: &Message) -> Value {
    if msg.role == MessageRole::Tool {
        return json!({
            "type": "function_call_output",
            "call_id": msg.tool_call_id.clone().unwrap_or_default(),
            "output": flatten_text(msg),
        });
    }

    let text_type = if msg.role == MessageRole::Assistant {
        "output_text"
    } else {
        "input_text"
    };
    let content: Vec<Value> = msg
        .parts
        .iter()
        .map(|p| match p {
            ContentPart::Text(t) => json!({"type": text_type, "text": t}),
            ContentPart::Untrusted(b) => json!({"type": text_type, "text": render_untrusted(b)}),
            ContentPart::Image { mime, data_base64 } => json!({
                "type": "input_image",
                "image_url": data_uri(mime, data_base64),
            }),
        })
        .collect();

    json!({
        "type": "message",
        "role": role_name(msg.role),
        "content": content,
    })
}

// ---------------------------------------------------------------------------
// Chat Completions decoder
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    /// Groq's vendor extension. §10's "where and whether `usage` appears"
    /// differs, concretely: Groq puts the token counts under `x_groq.usage` on
    /// the terminal chunk and leaves the standard field null, so a decoder that
    /// only reads `usage` reports zero spend for every Groq request (§14).
    #[serde(default)]
    x_groq: Option<VendorUsage>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct VendorUsage {
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
    /// DeepSeek-derived spelling, adopted by several compatible hosts.
    #[serde(default)]
    reasoning_content: Option<String>,
    /// xAI's spelling.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    args: String,
}

/// The Chat Completions state machine: Cerebras, SambaNova, Groq, xAI, Ollama
/// and Azure.
#[derive(Debug)]
pub struct ChatCompletionsDecoder {
    quirks: Quirks,
    tool_calls: Vec<PartialToolCall>,
    stop: Option<StopReason>,
    usage: Option<Usage>,
    emitted_terminal: bool,
}

impl ChatCompletionsDecoder {
    /// Build a decoder for a quirk set.
    pub fn new(quirks: Quirks) -> Self {
        Self {
            quirks,
            tool_calls: Vec::new(),
            stop: None,
            usage: None,
            emitted_terminal: false,
        }
    }

    fn buffered_tool_bytes(&self) -> usize {
        self.tool_calls.iter().fold(0, |total, call| {
            total
                .saturating_add(call.id.len())
                .saturating_add(call.name.len())
                .saturating_add(call.args.len())
        })
    }

    fn reject_tool_volume(
        &mut self,
        out: &mut Vec<Result<StreamEvent>>,
        message: &'static str,
    ) -> Flow {
        self.emitted_terminal = true;
        self.tool_calls.clear();
        out.push(Err(AiboError::Internal(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )))));
        Flow::Stop
    }

    fn finish(&mut self, out: &mut Vec<Result<StreamEvent>>) {
        if self.emitted_terminal {
            return;
        }
        self.emitted_terminal = true;

        // Tool calls arrive as fragments across many chunks and are only whole
        // at the end; emitting them earlier would hand the permission gate a
        // half-parsed argument object (§11).
        for call in std::mem::take(&mut self.tool_calls) {
            if call.name.is_empty() {
                continue;
            }
            match parse_tool_args(&call.args) {
                Ok(args) => out.push(Ok(StreamEvent::ToolCall {
                    id: call.id,
                    name: call.name,
                    args,
                })),
                Err(e) => out.push(Err(e)),
            }
        }
        if let Some(u) = self.usage.take() {
            out.push(Ok(StreamEvent::Usage(u)));
        }
        out.push(Ok(StreamEvent::Done(
            self.stop.clone().unwrap_or(StopReason::EndTurn),
        )));
    }
}

fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "length" | "max_tokens" => StopReason::Length,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::ContentFilter,
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

impl SseDecoder for ChatCompletionsDecoder {
    fn on_event(&mut self, ev: &Event, out: &mut Vec<Result<StreamEvent>>) -> Flow {
        let data = ev.data.trim();
        if data.is_empty() {
            return Flow::Continue;
        }
        if data == DONE_SENTINEL {
            self.finish(out);
            return Flow::Stop;
        }

        let chunk: ChatChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            // An unknown frame must not kill a working stream: providers add
            // fields and event types without notice (§10).
            Err(e) => {
                tracing::debug!(error = %e, "unparsable chat.completion chunk; ignored");
                return Flow::Continue;
            }
        };

        // Some hosts deliver a mid-stream failure as a normal data frame rather
        // than closing with a status code.
        if let Some(err) = chunk.error {
            self.emitted_terminal = true;
            let message = crate::wire::error_message(self.quirks.error_shape, &err.to_string())
                .unwrap_or_else(|| "provider reported a stream error".into());
            out.push(Err(AiboError::Internal(Box::new(std::io::Error::other(
                message,
            )))));
            return Flow::Stop;
        }

        if let Some(u) = chunk.usage.or_else(|| chunk.x_groq.and_then(|x| x.usage)) {
            self.usage = Some(u.normalise());
        }

        for choice in chunk.choices {
            if let Some(t) = choice.delta.content.filter(|t| !t.is_empty()) {
                out.push(Ok(StreamEvent::Text(t)));
            }
            let reasoning = match self.quirks.reasoning {
                ReasoningStyle::DeltaReasoningContent => choice.delta.reasoning_content,
                ReasoningStyle::DeltaReasoning => choice.delta.reasoning,
                // Tolerate either spelling even when none is declared: it costs
                // nothing, and a provider that starts emitting reasoning must
                // never have it rendered as insertable text (§7).
                _ => choice.delta.reasoning_content.or(choice.delta.reasoning),
            };
            if let Some(r) = reasoning.filter(|r| !r.is_empty()) {
                out.push(Ok(StreamEvent::Reasoning(r)));
            }
            for tc in choice.delta.tool_calls {
                if tc.index >= MAX_TOOL_CALLS_PER_RESPONSE {
                    return self
                        .reject_tool_volume(out, "provider exceeded the tool-call count limit");
                }
                if self.tool_calls.len() <= tc.index {
                    self.tool_calls
                        .resize(tc.index + 1, PartialToolCall::default());
                }
                let slot = &mut self.tool_calls[tc.index];
                if let Some(id) = tc.id {
                    slot.id = id;
                }
                if let Some(f) = tc.function {
                    if let Some(name) = f.name {
                        slot.name.push_str(&name);
                    }
                    if let Some(args) = f.arguments {
                        slot.args.push_str(&args);
                    }
                }
                if self.buffered_tool_bytes() > MAX_BUFFERED_TOOL_BYTES {
                    return self
                        .reject_tool_volume(out, "provider exceeded the buffered tool-call limit");
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.stop = Some(map_finish_reason(&reason));
            }
        }

        // Endpoints that send no `[DONE]` end on the chunk carrying
        // `finish_reason` — but when usage was opted into it arrives in a
        // *later* chunk, so stopping there would drop the spend numbers (§14).
        if !self.quirks.done_sentinel
            && self.stop.is_some()
            && self.quirks.usage != UsagePlacement::RequiresStreamOptions
        {
            self.finish(out);
            return Flow::Stop;
        }
        Flow::Continue
    }

    fn on_end(&mut self, out: &mut Vec<Result<StreamEvent>>) {
        self.finish(out);
    }
}

// ---------------------------------------------------------------------------
// Responses decoder
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ResponsesEvent {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    item: Option<ResponsesItem>,
    #[serde(default)]
    response: Option<ResponsesBody>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ResponsesItem {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesBody {
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

/// The Responses state machine: OpenAI native and the Codex endpoint (§3a).
///
/// SPIKE: S6 — the Codex-subscription endpoint's exact event set is only
/// obtainable with mitmproxy against a real session, not from the OSS tree. The
/// arms below are the public Responses API's shape. Unknown `type` values are
/// ignored deliberately, so an added event degrades rather than breaks, and the
/// golden fixture in `tests/fixtures/` must be re-recorded once S6 runs.
#[derive(Debug, Default)]
pub struct ResponsesDecoder {
    saw_tool_call: bool,
    tool_calls_seen: usize,
    stop: Option<StopReason>,
    emitted_terminal: bool,
}

impl ResponsesDecoder {
    fn finish(&mut self, out: &mut Vec<Result<StreamEvent>>) {
        if self.emitted_terminal {
            return;
        }
        self.emitted_terminal = true;
        let stop = self.stop.clone().unwrap_or(if self.saw_tool_call {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });
        out.push(Ok(StreamEvent::Done(stop)));
    }
}

impl SseDecoder for ResponsesDecoder {
    fn on_event(&mut self, ev: &Event, out: &mut Vec<Result<StreamEvent>>) -> Flow {
        let data = ev.data.trim();
        if data == DONE_SENTINEL {
            self.finish(out);
            return Flow::Stop;
        }
        if data.is_empty() {
            return Flow::Continue;
        }

        let parsed: ResponsesEvent = match serde_json::from_str(data) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %e, "unparsable responses event; ignored");
                return Flow::Continue;
            }
        };
        // The SSE `event:` name and the JSON `type` agree on this API; prefer
        // the body, which survives a proxy that drops event names.
        let kind = if parsed.kind.is_empty() {
            ev.event.as_str()
        } else {
            parsed.kind.as_str()
        };

        match kind {
            "response.output_text.delta" => {
                if let Some(d) = parsed.delta.filter(|d| !d.is_empty()) {
                    out.push(Ok(StreamEvent::Text(d)));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = parsed.delta.filter(|d| !d.is_empty()) {
                    out.push(Ok(StreamEvent::Reasoning(d)));
                }
            }
            "response.output_item.done" => {
                if let Some(item) = parsed.item
                    && item.kind == "function_call"
                {
                    if self.tool_calls_seen >= MAX_TOOL_CALLS_PER_RESPONSE {
                        self.emitted_terminal = true;
                        out.push(Err(AiboError::Internal(Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "provider exceeded the tool-call count limit",
                        )))));
                        return Flow::Stop;
                    }
                    self.tool_calls_seen += 1;
                    self.saw_tool_call = true;
                    let raw = item.arguments.unwrap_or_default();
                    match parse_tool_args(&raw) {
                        Ok(args) => out.push(Ok(StreamEvent::ToolCall {
                            id: item.call_id.or(item.id).unwrap_or_default(),
                            name: item.name.unwrap_or_default(),
                            args,
                        })),
                        Err(e) => out.push(Err(e)),
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                if let Some(body) = parsed.response {
                    if let Some(u) = body.usage {
                        out.push(Ok(StreamEvent::Usage(u.normalise())));
                    }
                    if let Some(d) = body.incomplete_details
                        && d.reason.as_deref() == Some("max_output_tokens")
                    {
                        self.stop = Some(StopReason::Length);
                    }
                }
                self.finish(out);
                return Flow::Stop;
            }
            "response.failed" | "error" => {
                let message = parsed
                    .error
                    .or_else(|| parsed.response.and_then(|r| r.error))
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "provider reported a stream error".into());
                self.emitted_terminal = true;
                out.push(Err(AiboError::Internal(Box::new(std::io::Error::other(
                    message,
                )))));
                return Flow::Stop;
            }
            _ => {}
        }
        Flow::Continue
    }

    fn on_end(&mut self, out: &mut Vec<Result<StreamEvent>>) {
        self.finish(out);
    }
}

// ---------------------------------------------------------------------------
// Shared construction helpers
// ---------------------------------------------------------------------------

/// Build an OpenAI-compatible provider, choosing the HTTP posture from the
/// credential (a [`Credential::LocalEndpoint`] is plaintext `localhost`).
pub fn build(
    id: ProviderId,
    base_url: &str,
    quirks: Quirks,
    credential: Credential,
) -> Result<OpenAiCompat> {
    let url = Url::parse(base_url).map_err(|e| AiboError::Internal(Box::new(e)))?;
    let http = if matches!(credential, Credential::LocalEndpoint(_)) {
        HttpConfig::local()
    } else {
        HttpConfig::default()
    };
    OpenAiCompat::new(id, url, quirks, credential, http)
}

/// Reject a credential that cannot work with a key-authenticated provider, at
/// construction rather than on the first hotkey press.
pub fn require_api_key(id: &ProviderId, credential: &Credential) -> Result<()> {
    match credential {
        Credential::ApiKey(k) if !k.expose_secret().trim().is_empty() => Ok(()),
        Credential::ApiKey(_) => Err(AiboError::Auth {
            provider: id.clone(),
            kind: aibo_core::error::AuthKind::Invalid,
        }),
        _ => Err(Unimplemented::err(
            id.clone(),
            "this provider only accepts Credential::ApiKey",
        )),
    }
}

/// Sugar for a shipped catalogue entry (§19 manifest, §10 fallback).
pub fn model(
    provider: &ProviderId,
    id: &str,
    display_name: &str,
    capabilities: Capabilities,
) -> ModelInfo {
    ModelInfo {
        provider: provider.clone(),
        id: id.to_string(),
        display_name: display_name.to_string(),
        capabilities,
        deprecated: false,
        replaced_by: None,
    }
}

/// Erase to the trait object the registry stores.
pub fn boxed(p: OpenAiCompat) -> Arc<dyn Provider> {
    Arc::new(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;

    fn base() -> Url {
        Url::parse("https://api.example.com/v1").unwrap()
    }

    #[test]
    fn a_versioned_base_path_is_not_eaten_by_the_join() {
        // `Url::join` would turn `https://api.example.com/v1` + `models` into
        // `https://api.example.com/models`, which 404s on every provider.
        let q = Quirks::chat_completions();
        assert_eq!(
            q.op_url(&base(), Op::Chat).unwrap().as_str(),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            q.op_url(&base(), Op::Models).unwrap().as_str(),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn azure_urls_are_deployment_scoped_and_versioned() {
        let mut q = Quirks::chat_completions();
        q.url = UrlStyle::AzureDeployment {
            deployment: "prod-gpt".into(),
            api_version: "2026-01-01".into(),
        };
        let base = Url::parse("https://my-resource.openai.azure.com").unwrap();
        assert_eq!(
            q.op_url(&base, Op::Chat).unwrap().as_str(),
            "https://my-resource.openai.azure.com/openai/deployments/prod-gpt/chat/completions?api-version=2026-01-01"
        );
    }

    #[test]
    fn a_trailing_slash_on_the_base_does_not_double_up() {
        let q = Quirks::chat_completions();
        let base = Url::parse("https://api.example.com/v1/").unwrap();
        assert_eq!(
            q.op_url(&base, Op::Chat).unwrap().as_str(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn a_hostile_tool_index_cannot_resize_the_decoder_without_bound() {
        let body = format!(
            "data: {}\n\n",
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": MAX_TOOL_CALLS_PER_RESPONSE,
                            "id": "call-hostile",
                            "function": {"name": "run", "arguments": "{}"}
                        }]
                    }
                }]
            })
        );
        let out: Vec<_> = decode(
            crate::sse::events_from_bytes(body),
            ChatCompletionsDecoder::new(Quirks::chat_completions()),
            ProviderId::OPENAI,
            CancellationToken::new(),
        )
        .collect()
        .await;

        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Err(AiboError::Internal(_))));
    }

    #[test]
    fn responses_tool_call_fan_out_has_a_per_response_ceiling() {
        let mut decoder = ResponsesDecoder::default();
        let mut out = Vec::new();

        for index in 0..MAX_TOOL_CALLS_PER_RESPONSE {
            let event = Event {
                data: json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "function_call",
                        "call_id": format!("call-{index}"),
                        "name": "run",
                        "arguments": "{}"
                    }
                })
                .to_string(),
                ..Event::default()
            };
            assert_eq!(decoder.on_event(&event, &mut out), Flow::Continue);
        }

        let overflow = Event {
            data: json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "call_id": "call-overflow",
                    "name": "run",
                    "arguments": "{}"
                }
            })
            .to_string(),
            ..Event::default()
        };
        assert_eq!(decoder.on_event(&overflow, &mut out), Flow::Stop);
        assert!(matches!(out.last(), Some(Err(AiboError::Internal(_)))));
    }
}
