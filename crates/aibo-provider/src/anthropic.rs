//! Anthropic native `messages` SSE (§10).
//!
//! Not an OpenAI-compatible endpoint and not worth pretending otherwise: the
//! request shape, the auth header, the streaming event set, tool-use encoding
//! and thinking blocks all differ. §10's note — "distinct tool-use and
//! thinking-block handling" — is the whole reason this is a separate module.
//!
//! Three differences drive the code below.
//!
//! 1. **The system prompt is a top-level `system` field**, not a message. An
//!    Anthropic request with a `system`-role message is a 400.
//! 2. **Content arrives as indexed blocks.** Text, thinking and tool-use are
//!    separate blocks addressed by index, and a tool call's arguments stream as
//!    `input_json_delta` fragments that are only valid JSON once the block
//!    closes.
//! 3. **Usage arrives twice.** Input counts land on `message_start`, output
//!    counts on `message_delta`; the spend meter needs both, so a single
//!    [`Usage`] is accumulated and emitted once (§14).

use std::time::{Duration, Instant};

use aibo_core::error::{AiboError, Result};
use aibo_core::traits::Provider;
use aibo_core::types::{
    BoxStream, Capabilities, ChatRequest, ContentPart, Credential, Health, Message, MessageRole,
    ModelInfo, MultiCandidate, ProviderId, ReasoningEffort, StopReason, StreamEvent, Usage,
};
use async_trait::async_trait;
use eventsource_stream::Event;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth::{AuthStyle, apply_credential};
use crate::http::{HttpConfig, build_client, map_transport_error};
use crate::sse::{
    Flow, MAX_BUFFERED_TOOL_BYTES, MAX_TOOL_CALLS_PER_RESPONSE, SseDecoder, decode,
    events_from_response, read_error_body,
};
use crate::wire::{ErrorShape, flatten_text, map_status, parse_retry_after, parse_tool_args};

/// Anthropic's API base URL.
pub const BASE_URL: &str = "https://api.anthropic.com/v1";

/// The `anthropic-version` header value. Required on every request; omitting it
/// is a 400, and changing it changes the response shape, so it is pinned here
/// and bumped deliberately with a fixture re-record.
pub const API_VERSION: &str = "2023-06-01";

/// Provider defaults. Per-model values come from the §19 manifest.
///
/// `vision: true` is a statement about the whole current catalogue on this
/// endpoint, not a guess: every model `GET /v1/models` returns accepts an
/// `image` content block. It is still only the *fallback* — §10 puts
/// capabilities on [`ModelInfo`] because one provider routinely serves a vision
/// model and a text-only one, and if Anthropic ships a text-only model this must
/// become a per-model value rather than staying true by inertia.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        vision: true,
        streaming: true,
        reasoning_effort: true,
        json_schema: false,
        prompt_cache: true,
        fim: false,
        multi_candidate: MultiCandidate::Unsupported,
        max_context: 200_000,
        max_output: Some(64_000),
    }
}

/// The Anthropic provider.
pub struct Anthropic {
    id: ProviderId,
    base_url: Url,
    credential: Credential,
    client: reqwest::Client,
    capabilities: Capabilities,
}

impl std::fmt::Debug for Anthropic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Anthropic")
            .field("base_url", &self.base_url.as_str())
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

impl Anthropic {
    /// Build the provider with its own pooled client.
    pub fn new(credential: Credential) -> Result<Self> {
        let id = ProviderId::ANTHROPIC;
        crate::openai_compat::require_api_key(&id, &credential)?;
        Ok(Self {
            id,
            base_url: Url::parse(BASE_URL).map_err(|e| AiboError::Internal(Box::new(e)))?,
            credential,
            client: build_client(&HttpConfig::default())?,
            capabilities: default_capabilities(),
        })
    }

    /// The base URL, for pre-warming (§15).
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Open a pooled connection ahead of the first request (§15, §13 wake).
    pub async fn prewarm(&self) {
        crate::http::prewarm(&self.client, &self.base_url).await;
    }

    fn url(&self, suffix: &str) -> Result<Url> {
        let s = format!("{}/{suffix}", self.base_url.as_str().trim_end_matches('/'));
        Url::parse(&s).map_err(|e| AiboError::Internal(Box::new(e)))
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
            ErrorShape::AnthropicEnvelope,
            &body,
        ))
    }
}

#[async_trait]
impl Provider for Anthropic {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn prewarm(&self) {
        Anthropic::prewarm(self).await;
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        // Refuse, never strip (§10) — then downscale before the pixels are
        // base64'd into the body (§14). See `crate::attachment`.
        crate::attachment::guard(&self.capabilities, &req, Vec::new())?;
        let req = crate::attachment::prepare(req).await?;

        let body = build_messages_body(&req);
        let rb = self
            .client
            .post(self.url("messages")?)
            .header("anthropic-version", API_VERSION)
            .header("accept", "text/event-stream")
            .json(&body);
        let rb = apply_credential(&self.id, &self.credential, AuthStyle::XApiKey, rb).await?;

        let response = rb
            .send()
            .await
            .map_err(|e| map_transport_error(&self.id, &e))?;
        let response = self.check_status(response).await?;

        Ok(decode(
            events_from_response(response),
            MessagesDecoder::default(),
            self.id.clone(),
            cancel,
        ))
    }

    async fn models(&self) -> Result<Vec<ModelInfo>> {
        let rb = self
            .client
            .get(self.url("models")?)
            .header("anthropic-version", API_VERSION)
            .timeout(Duration::from_secs(10));
        let rb = apply_credential(&self.id, &self.credential, AuthStyle::XApiKey, rb).await?;
        let response = rb
            .send()
            .await
            .map_err(|e| map_transport_error(&self.id, &e))?;
        let response = self.check_status(response).await?;

        #[derive(Deserialize)]
        struct List {
            #[serde(default)]
            data: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            id: String,
            #[serde(default)]
            display_name: Option<String>,
        }

        let list: List = response
            .json()
            .await
            .map_err(|e| map_transport_error(&self.id, &e))?;
        Ok(list
            .data
            .into_iter()
            .map(|e| ModelInfo {
                provider: self.id.clone(),
                display_name: e.display_name.unwrap_or_else(|| e.id.clone()),
                id: e.id,
                capabilities: self.capabilities.clone(),
                released_at: None,
                deprecated: false,
                replaced_by: None,
            })
            .collect())
    }

    async fn health(&self) -> Result<Health> {
        let started = Instant::now();
        let rb = self
            .client
            .get(self.url("models")?)
            .header("anthropic-version", API_VERSION)
            .timeout(Duration::from_secs(5));
        let rb = apply_credential(&self.id, &self.credential, AuthStyle::XApiKey, rb).await?;
        match rb.send().await {
            Ok(r) if r.status().is_success() => Ok(Health::Ok {
                latency: started.elapsed(),
            }),
            Ok(r) => Ok(Health::Degraded {
                reason: format!("HTTP {}", r.status().as_u16()),
                consecutive_failures: 1,
            }),
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

// ---------------------------------------------------------------------------
// Request body
// ---------------------------------------------------------------------------

/// Build a `POST /v1/messages` body.
///
/// Pure, so it can be pinned by a golden file.
///
/// # Images are a fourth difference from the OpenAI-compatible path
///
/// Anthropic takes an image as `{"type": "image", "source": {"type": "base64",
/// "media_type": …, "data": …}}` — the bytes are a bare base64 string in a
/// nested `source` object, with the media type as its own field. The Responses
/// format wants an `input_image` part whose `image_url` is a `data:` URL, and
/// Chat Completions wants an `image_url` *object*. Three shapes for one image is
/// precisely why §10 keeps per-provider implementations instead of a
/// lowest-common-denominator layer.
///
/// [`ChatRequest::attachments`] are folded onto the last user message by
/// [`crate::attachment::fold_into_messages`], which is also what guarantees they
/// never land in `system` — Anthropic rejects a `system`-role message outright,
/// and §5 would have an image in the instructions authorising things.
pub fn build_messages_body(req: &ChatRequest) -> Value {
    let mut system = String::new();
    let mut messages: Vec<Value> = Vec::new();

    let folded = crate::attachment::fold_into_messages(req);
    for msg in folded.iter() {
        match msg.role {
            MessageRole::System => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&flatten_text(msg));
            }
            MessageRole::Tool => messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id.clone().unwrap_or_default(),
                    "content": flatten_text(msg),
                }],
            })),
            MessageRole::User | MessageRole::Assistant => messages.push(json!({
                "role": if msg.role == MessageRole::User { "user" } else { "assistant" },
                "content": content_blocks(msg),
            })),
        }
    }

    // Anthropic requires `max_tokens`; unlike OpenAI-compatible APIs it cannot
    // express "provider default". Use the finite planning reserve when the
    // provider-neutral parameter is unset.
    let max_tokens = if req.params.max_tokens == 0 {
        req.budget.max_output_tokens
    } else {
        req.params.max_tokens.min(req.budget.max_output_tokens)
    };
    let mut body = json!({
        "model": req.binding.model,
        "messages": messages,
        "stream": true,
        "max_tokens": max_tokens,
        "temperature": req.params.temperature,
    });
    let obj = body.as_object_mut().expect("object literal");

    if !system.is_empty() {
        obj.insert("system".into(), json!(system));
    }
    if let Some(p) = req.params.top_p {
        obj.insert("top_p".into(), json!(p));
    }
    if !req.params.stop.is_empty() {
        obj.insert("stop_sequences".into(), json!(req.params.stop));
    }
    if let Some(effort) = req.params.reasoning_effort {
        // Anthropic budgets thinking in tokens rather than taking an effort
        // enum, so the §5 knob is mapped onto a budget. The budget must stay
        // below `max_tokens` or the request is rejected.
        let max = max_tokens;
        let budget = match effort {
            ReasoningEffort::Low => max / 4,
            ReasoningEffort::Medium => max / 2,
            ReasoningEffort::High => max.saturating_sub(max / 4),
        };
        if budget >= 1024 {
            obj.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
            // Anthropic requires temperature 1 whenever thinking is enabled.
            obj.insert("temperature".into(), json!(1));
        }
    }
    if !req.tools.is_empty() {
        obj.insert(
            "tools".into(),
            json!(
                req.tools
                    .iter()
                    .map(|t| json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }
    // Anthropic's hosted web search (owner, 2026-08-02): a server-side tool
    // the API executes itself — results stream back as ordinary text with
    // `server_tool_use`/`web_search_tool_result` blocks the decoder skips.
    // Capped so a single question cannot fan out into a research bill (§14).
    if req.web_search {
        let search = json!({
            "type": "web_search_20250305",
            "name": "web_search",
            "max_uses": 5,
        });
        match obj.get_mut("tools").and_then(Value::as_array_mut) {
            Some(tools) => tools.push(search),
            None => {
                obj.insert("tools".into(), json!([search]));
            }
        }
    }

    body
}

fn content_blocks(msg: &Message) -> Vec<Value> {
    msg.parts
        .iter()
        .map(|p| match p {
            ContentPart::Text(t) => json!({"type": "text", "text": t}),
            ContentPart::Untrusted(b) => {
                json!({"type": "text", "text": crate::wire::render_untrusted(b)})
            }
            ContentPart::Image { mime, data_base64 } => json!({
                "type": "image",
                "source": {"type": "base64", "media_type": mime, "data": data_base64},
            }),
            // The assistant turn that made a tool call; the `tool_result`
            // user turn that follows answers this block's `id`.
            ContentPart::ToolCall { id, name, args } => json!({
                "type": "tool_use", "id": id, "name": name, "input": args,
            }),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AnthropicEvent {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    message: Option<MessageStart>,
    #[serde(default)]
    content_block: Option<BlockStart>,
    #[serde(default)]
    delta: Option<BlockDelta>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct MessageStart {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct BlockStart {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BlockDelta {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, Default)]
struct ToolBlock {
    id: String,
    name: String,
    args: String,
}

/// Anthropic's `messages` SSE state machine.
#[derive(Debug, Default)]
pub struct MessagesDecoder {
    /// Open tool-use blocks by block index. Text and thinking blocks stream
    /// straight through; only tool blocks have to be buffered, because
    /// `input_json_delta` fragments are not parseable until the block closes.
    tool_blocks: std::collections::BTreeMap<usize, ToolBlock>,
    buffered_tool_bytes: usize,
    tool_calls_seen: usize,
    usage: Usage,
    saw_usage: bool,
    stop: Option<StopReason>,
    emitted_terminal: bool,
}

impl MessagesDecoder {
    fn reject_tool_volume(
        &mut self,
        out: &mut Vec<Result<StreamEvent>>,
        message: &'static str,
    ) -> Flow {
        self.emitted_terminal = true;
        self.tool_blocks.clear();
        self.buffered_tool_bytes = 0;
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
        if self.saw_usage {
            out.push(Ok(StreamEvent::Usage(self.usage)));
        }
        out.push(Ok(StreamEvent::Done(
            self.stop.clone().unwrap_or(StopReason::EndTurn),
        )));
    }
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "max_tokens" => StopReason::Length,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "refusal" => StopReason::ContentFilter,
        _ => StopReason::EndTurn,
    }
}

impl SseDecoder for MessagesDecoder {
    fn on_event(&mut self, ev: &Event, out: &mut Vec<Result<StreamEvent>>) -> Flow {
        let data = ev.data.trim();
        if data.is_empty() {
            return Flow::Continue;
        }
        let parsed: AnthropicEvent = match serde_json::from_str(data) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %e, "unparsable anthropic event; ignored");
                return Flow::Continue;
            }
        };
        let kind = if parsed.kind.is_empty() {
            ev.event.as_str()
        } else {
            parsed.kind.as_str()
        };

        match kind {
            "message_start" => {
                if let Some(u) = parsed.message.and_then(|m| m.usage) {
                    self.saw_usage = true;
                    // `input_tokens` excludes the cached counts on this API, so
                    // no subtraction here — the opposite of the OpenAI shape.
                    self.usage.input_tokens = u.input_tokens + u.cache_creation_input_tokens;
                    self.usage.cached_input_tokens = u.cache_read_input_tokens;
                    self.usage.output_tokens = u.output_tokens;
                }
            }
            "content_block_start" => {
                if let Some(block) = parsed.content_block
                    && block.kind == "tool_use"
                {
                    if self.tool_calls_seen >= MAX_TOOL_CALLS_PER_RESPONSE
                        || self.tool_blocks.contains_key(&parsed.index)
                    {
                        return self.reject_tool_volume(
                            out,
                            "provider exceeded the tool-call count limit",
                        );
                    }
                    self.tool_calls_seen += 1;
                    let id = block.id.unwrap_or_default();
                    let name = block.name.unwrap_or_default();
                    let added = id.len().saturating_add(name.len());
                    if self.buffered_tool_bytes.saturating_add(added) > MAX_BUFFERED_TOOL_BYTES {
                        return self.reject_tool_volume(
                            out,
                            "provider exceeded the buffered tool-call limit",
                        );
                    }
                    self.buffered_tool_bytes += added;
                    self.tool_blocks.insert(
                        parsed.index,
                        ToolBlock {
                            id,
                            name,
                            args: String::new(),
                        },
                    );
                }
            }
            "content_block_delta" => {
                if let Some(delta) = parsed.delta {
                    match delta.kind.as_str() {
                        "text_delta" => {
                            if let Some(t) = delta.text.filter(|t| !t.is_empty()) {
                                out.push(Ok(StreamEvent::Text(t)));
                            }
                        }
                        "thinking_delta" => {
                            if let Some(t) = delta.thinking.filter(|t| !t.is_empty()) {
                                out.push(Ok(StreamEvent::Reasoning(t)));
                            }
                        }
                        "input_json_delta" => {
                            if let Some(frag) = delta.partial_json
                                && self.tool_blocks.contains_key(&parsed.index)
                            {
                                if self.buffered_tool_bytes.saturating_add(frag.len())
                                    > MAX_BUFFERED_TOOL_BYTES
                                {
                                    return self.reject_tool_volume(
                                        out,
                                        "provider exceeded the buffered tool-argument limit",
                                    );
                                }
                                self.buffered_tool_bytes += frag.len();
                                let block = self
                                    .tool_blocks
                                    .get_mut(&parsed.index)
                                    .expect("presence checked above");
                                block.args.push_str(&frag);
                            }
                        }
                        // `signature_delta` and anything added later: ignored,
                        // never rendered as text.
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                if let Some(block) = self.tool_blocks.remove(&parsed.index) {
                    let released = block
                        .id
                        .len()
                        .saturating_add(block.name.len())
                        .saturating_add(block.args.len());
                    self.buffered_tool_bytes = self.buffered_tool_bytes.saturating_sub(released);
                    match parse_tool_args(&block.args) {
                        Ok(args) => out.push(Ok(StreamEvent::ToolCall {
                            id: block.id,
                            name: block.name,
                            args,
                        })),
                        Err(e) => out.push(Err(e)),
                    }
                }
            }
            "message_delta" => {
                if let Some(d) = parsed.delta
                    && let Some(reason) = d.stop_reason
                {
                    self.stop = Some(map_stop_reason(&reason));
                }
                if let Some(u) = parsed.usage {
                    self.saw_usage = true;
                    self.usage.output_tokens = u.output_tokens;
                }
            }
            "message_stop" => {
                self.finish(out);
                return Flow::Stop;
            }
            "error" => {
                self.emitted_terminal = true;
                let message = parsed
                    .error
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "provider reported a stream error".into());
                out.push(Err(AiboError::Internal(Box::new(std::io::Error::other(
                    message,
                )))));
                return Flow::Stop;
            }
            // `ping` and future event types.
            _ => {}
        }
        Flow::Continue
    }

    fn on_end(&mut self, out: &mut Vec<Result<StreamEvent>>) {
        self.finish(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The assistant turn that called a tool becomes a `tool_use` block, so
    /// the `tool_result` user turn that follows has an id to answer.
    #[test]
    fn an_assistant_tool_call_becomes_a_tool_use_block() {
        let assistant = Message {
            role: MessageRole::Assistant,
            parts: vec![ContentPart::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: json!({"command": "ls"}),
            }],
            tool_call_id: None,
            tool_name: None,
        };
        let blocks = content_blocks(&assistant);
        assert_eq!(blocks[0]["type"], json!("tool_use"));
        assert_eq!(blocks[0]["id"], json!("c1"));
        assert_eq!(blocks[0]["name"], json!("bash"));
        assert_eq!(
            blocks[0]["input"],
            json!({"command": "ls"}),
            "input stays a JSON object here, unlike OpenAI's stringified spelling"
        );
    }

    fn event(value: Value) -> Event {
        Event {
            data: value.to_string(),
            ..Event::default()
        }
    }

    #[test]
    fn fragmented_tool_arguments_have_a_cumulative_memory_bound() {
        let fragment = "x".repeat((MAX_BUFFERED_TOOL_BYTES / 2) + 1);
        let mut decoder = MessagesDecoder::default();
        let mut out = Vec::new();
        assert_eq!(
            decoder.on_event(
                &event(json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "call-1",
                        "name": "run"
                    }
                })),
                &mut out,
            ),
            Flow::Continue
        );
        for expected in [Flow::Continue, Flow::Stop] {
            assert_eq!(
                decoder.on_event(
                    &event(json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": &fragment
                        }
                    })),
                    &mut out,
                ),
                expected
            );
        }
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Err(AiboError::Internal(_))));
    }
}
