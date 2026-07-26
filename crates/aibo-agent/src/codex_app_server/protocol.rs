//! Vendored subset of the `codex app-server` wire protocol (§3).
//!
//! # Why these types are here and not from crates.io
//!
//! §3, verbatim: "**Vendor the protocol types — do not take them from
//! crates.io.** The `codex-app-server-protocol` crate published there is **not
//! OpenAI's**: it is owned by a third party and points at a fork. OpenAI's
//! workspace pins `version = "0.0.0"` for every one of these crates, i.e. they
//! deliberately publish nothing." Codex is Apache-2.0, so vendoring into a
//! closed-source product is fine **with NOTICE attribution** — that obligation
//! is real and belongs in the distribution work (§19).
//!
//! # Why a strict JSON-RPC codec will not work
//!
//! §3: the protocol is "**JSON-RPC-2-*like*, not wire-compatible with a strict
//! codec** (the `"jsonrpc":"2.0"` field is deliberately omitted)". Every type
//! below is therefore hand-rolled over plain NDJSON. Do not reach for a
//! jsonrpc crate; it will reject every frame.
//!
//! Transport is **stdio with newline-delimited JSON**. The unix-socket option
//! carries *websocket* frames over an HTTP Upgrade, not NDJSON, and websocket is
//! marked experimental/unsupported — stdio is the right default (§3).
//!
//! # Parse permissively
//!
//! §3: "vendor types for a tested min/max range, generate at build time where
//! possible, parse permissively (ignore unknown fields), and fail with a clear
//! 'your `codex` is newer/older than this build supports' rather than a
//! deserialisation error." So: no `deny_unknown_fields` anywhere, every
//! discriminator is an `Option<String>` compared at runtime rather than an enum
//! that fails to deserialise, and unknown events are dropped with a `trace!`.
//!
//! SPIKE: S5 — every method name, params shape and event shape below is
//! **[unverified]** against a real binary. §3's recommended approach is to
//! **generate the schema from the installed `codex`**, which is guaranteed to
//! match that binary. Until S5 reports, treat these as the interface, confirm
//! each one against the generated schema, and correct the constants here rather
//! than papering over mismatches at the call sites.

use std::str::FromStr;

use aibo_core::types::{ApprovalDecision, ApprovalKind, ApprovalRequest, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Method names
// ---------------------------------------------------------------------------

/// Method names on the app-server protocol.
///
/// SPIKE: S5 — confirm each against `codex`'s generated schema. The
/// `account/*` names are the ones §3 states explicitly and are the most
/// trustworthy entries here; the `thread/*` names are the least.
pub mod method {
    /// Handshake. Sent once, before anything else.
    pub const INITIALIZE: &str = "initialize";
    /// Client → server notification completing the handshake.
    pub const INITIALIZED: &str = "initialized";

    /// §3: returns `{account, requires_openai_auth}` — **no rate limits**.
    pub const ACCOUNT_READ: &str = "account/read";
    /// §3: notification carrying `{auth_mode, plan_type}` — **no rate limits**.
    pub const ACCOUNT_UPDATED: &str = "account/updated";
    /// §3: rate limits live on their own channel, not on `account/read`.
    pub const ACCOUNT_RATE_LIMITS_READ: &str = "account/rateLimits/read";
    /// §3: a **sparse rolling update you must merge**, not a full snapshot.
    pub const ACCOUNT_RATE_LIMITS_UPDATED: &str = "account/rateLimits/updated";

    /// Start a new thread. §3b: Do always starts a *new* thread.
    ///
    /// SPIKE: S5 — name and params unverified.
    pub const THREAD_START: &str = "thread/start";
    /// Send a user turn into a thread.
    ///
    /// SPIKE: S5 — name and params unverified.
    pub const THREAD_SEND_MESSAGE: &str = "thread/sendMessage";
    /// Abort the in-flight turn. §13: `esc` must abort immediately.
    ///
    /// SPIKE: S5 — name unverified.
    pub const THREAD_INTERRUPT: &str = "thread/interrupt";
    /// Server → client notification carrying turn events.
    ///
    /// SPIKE: S5 — name and payload unverified.
    pub const THREAD_EVENT: &str = "thread/event";

    // -- server → client *requests* (they carry an id and expect a reply) ----

    /// Codex asks whether a command may run. §11 tier 4: this is the real
    /// pre-write gate, surfaced in aibo's UI.
    ///
    /// SPIKE: S5 — name unverified.
    pub const EXEC_COMMAND_APPROVAL: &str = "execCommandApproval";
    /// Codex asks whether a patch may be applied.
    ///
    /// SPIKE: S5 — name unverified.
    pub const APPLY_PATCH_APPROVAL: &str = "applyPatchApproval";
    /// §3a: app-server does **not** generate `x-oai-attestation`; it requests
    /// one from the connected client, and the implementation lives in OpenAI's
    /// own VS Code extension, not in the OSS tree. aibo cannot answer this, and
    /// must reply with an error rather than hang.
    pub const ATTESTATION_GENERATE: &str = "attestation/generate";
}

// ---------------------------------------------------------------------------
// Versioning
// ---------------------------------------------------------------------------

/// A `major.minor.patch` version, ordered.
///
/// A three-field struct rather than a `semver` dependency: the workspace pins no
/// semver crate, and the only operations needed are parse, compare and display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    /// Major.
    pub major: u32,
    /// Minor.
    pub minor: u32,
    /// Patch.
    pub patch: u32,
}

impl Version {
    /// Construct.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for Version {
    type Err = String;

    /// Tolerant: accepts a leading `v`, and a trailing pre-release or build
    /// suffix (`0.64.0-alpha.1`), which nightly Codex builds carry.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().trim_start_matches('v');
        let core = s.split(['-', '+']).next().unwrap_or(s);
        let mut parts = core.split('.');
        let mut next = |what: &str| -> Result<u32, String> {
            parts
                .next()
                .ok_or_else(|| format!("missing {what} in version `{s}`"))?
                .parse::<u32>()
                .map_err(|e| format!("bad {what} in version `{s}`: {e}"))
        };
        let major = next("major")?;
        let minor = next("minor")?;
        // A two-component version is common enough to accept.
        let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Ok(Self::new(major, minor, patch))
    }
}

/// The lowest protocol version this build of aibo understands.
///
/// §20/S5 asks the question directly: "Does published protocol 0.63.0
/// deserialise today's binary? Minimum version floor?" 0.63.0 is that floor
/// until S5 reports otherwise.
pub const MIN_SUPPORTED: Version = Version::new(0, 63, 0);

/// The highest protocol version this build has actually been tested against.
///
/// Newer is a **warning**, not an error: §3 says the `initialize` capabilities
/// "do not negotiate protocol versions — they opt into experimental behaviour",
/// so a floor alone is insufficient and a hard upper bound would break aibo
/// every time Codex ships. Permissive parsing plus a loud log is the honest
/// middle. SPIKE: S5 — raise this as versions are tested.
pub const MAX_TESTED: Version = Version::new(0, 63, 0);

/// What a detected version means for this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionVerdict {
    /// Within the tested range.
    Supported,
    /// Below [`MIN_SUPPORTED`]. Refuse with a clear message; the user needs to
    /// update `codex`.
    TooOld,
    /// Above [`MAX_TESTED`]. Proceed, but say so — a schema change may surface
    /// as a dropped event rather than an error, because parsing is permissive.
    NewerThanTested,
}

/// Classify a detected protocol version.
pub const fn classify(found: Version) -> VersionVerdict {
    // `const fn` cannot call `PartialOrd::lt`, so compare field-wise via the
    // packed form.
    const fn packed(v: Version) -> u128 {
        ((v.major as u128) << 64) | ((v.minor as u128) << 32) | (v.patch as u128)
    }
    if packed(found) < packed(MIN_SUPPORTED) {
        VersionVerdict::TooOld
    } else if packed(found) > packed(MAX_TESTED) {
        VersionVerdict::NewerThanTested
    } else {
        VersionVerdict::Supported
    }
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// A request id. Codex uses numbers; strings are accepted defensively.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id.
    Number(i64),
    /// String id.
    Text(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::Text(s) => f.write_str(s),
        }
    }
}

/// An error object on a response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// Numeric code.
    #[serde(default)]
    pub code: i64,
    /// Human-readable message. **Not** shown raw to the user (§13).
    #[serde(default)]
    pub message: String,
    /// Structured payload, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

/// So it can be a `#[source]` on [`crate::codex_app_server::CodexError`]. The
/// `data` payload is deliberately never part of `Display` — it can carry user
/// content (§11 threat model, secrets in logs).
impl std::error::Error for RpcError {}

/// One decoded NDJSON line, before classification.
///
/// Deliberately all-optional: this is the permissive parse §3 asks for. Note the
/// **absent `jsonrpc` field** — writing one, or requiring one, breaks the wire.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawMessage {
    /// Present on requests and responses, absent on notifications.
    #[serde(default)]
    pub id: Option<RequestId>,
    /// Present on requests and notifications.
    #[serde(default)]
    pub method: Option<String>,
    /// Request/notification payload.
    #[serde(default)]
    pub params: Option<Value>,
    /// Response payload.
    #[serde(default)]
    pub result: Option<Value>,
    /// Response failure.
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// A classified inbound message.
#[derive(Debug, Clone)]
pub enum Inbound {
    /// A reply to something aibo sent.
    Response {
        /// Correlates with the outgoing request.
        id: RequestId,
        /// `Ok` payload or the server's error object.
        outcome: Result<Value, RpcError>,
    },
    /// The server is asking aibo something and expects a reply — approvals, and
    /// `attestation/generate` (§3a).
    Request {
        /// Reply with this id.
        id: RequestId,
        /// Method name.
        method: String,
        /// Payload.
        params: Value,
    },
    /// Fire-and-forget event.
    Notification {
        /// Method name.
        method: String,
        /// Payload.
        params: Value,
    },
}

impl RawMessage {
    /// Classify a decoded line.
    ///
    /// Returns `None` for a frame that is none of the three shapes; the caller
    /// logs and drops it rather than failing the run (§3: parse permissively).
    pub fn classify(self) -> Option<Inbound> {
        match (self.id, self.method) {
            (Some(id), Some(method)) => Some(Inbound::Request {
                id,
                method,
                params: self.params.unwrap_or(Value::Null),
            }),
            (Some(id), None) => {
                let outcome = match self.error {
                    Some(e) => Err(e),
                    None => Ok(self.result.unwrap_or(Value::Null)),
                };
                Some(Inbound::Response { id, outcome })
            }
            (None, Some(method)) => Some(Inbound::Notification {
                method,
                params: self.params.unwrap_or(Value::Null),
            }),
            (None, None) => None,
        }
    }
}

/// An outgoing request. **No `jsonrpc` field** — see the module docs.
#[derive(Debug, Clone, Serialize)]
pub struct OutboundRequest<'a> {
    /// Correlation id.
    pub id: RequestId,
    /// Method name.
    pub method: &'a str,
    /// Payload; omitted entirely when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// An outgoing notification: a request with no id.
#[derive(Debug, Clone, Serialize)]
pub struct OutboundNotification<'a> {
    /// Method name.
    pub method: &'a str,
    /// Payload; omitted entirely when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A reply to a server → client request.
#[derive(Debug, Clone, Serialize)]
pub struct OutboundResponse {
    /// The id being answered.
    pub id: RequestId,
    /// Success payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Failure payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

/// Who is connecting.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// Machine-readable client id.
    pub name: String,
    /// Human-readable name.
    pub title: String,
    /// aibo's version.
    pub version: String,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            name: "aibo".to_owned(),
            title: "aibo".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// `initialize` params.
///
/// §3: capabilities here "**do not negotiate protocol versions** — they opt into
/// experimental behaviour". aibo opts into nothing, which is why `capabilities`
/// is omitted rather than sent empty.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Client identity.
    pub client_info: ClientInfo,
}

/// `initialize` result, parsed permissively.
///
/// SPIKE: S5 — the field carrying the protocol version is unverified, so
/// [`InitializeResult::detected_version`] tries the plausible names and reports
/// failure clearly rather than guessing.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// Server user agent string, when present.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Everything else, kept so the version can be hunted for and so a
    /// diagnostics export can show what the binary actually said.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl InitializeResult {
    /// Field names that have plausibly carried the version. Tried in order.
    const VERSION_KEYS: [&'static str; 4] = [
        "protocolVersion",
        "protocol_version",
        "version",
        "appServerVersion",
    ];

    /// Best-effort protocol version detection.
    ///
    /// Returns `None` when nothing version-shaped is present, which the caller
    /// must surface as "could not determine your `codex` version" rather than
    /// silently assuming compatibility.
    pub fn detected_version(&self) -> Option<Version> {
        for key in Self::VERSION_KEYS {
            if let Some(Value::String(s)) = self.extra.get(key)
                && let Ok(v) = s.parse::<Version>()
            {
                return Some(v);
            }
        }
        // Some builds embed it in the user agent, e.g. `codex_app_server/0.63.0`.
        let ua = self.user_agent.as_deref()?;
        ua.split(['/', ' '])
            .filter_map(|part| part.parse::<Version>().ok())
            .next()
    }
}

// ---------------------------------------------------------------------------
// account
// ---------------------------------------------------------------------------

/// `account/read` result.
///
/// §3: this carries `{account, requires_openai_auth}` and **no rate limits**.
/// Do not add a rate-limit field here however convenient it would be — the
/// quota readout has its own channel and its own merge logic.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRead {
    /// Opaque account object; shape unverified (SPIKE: S5).
    #[serde(default)]
    pub account: Option<Value>,
    /// Whether the user still has to log in. aibo never runs the login flow
    /// itself for app-server — §3: "Auth is Codex's problem, not aibo's."
    #[serde(default)]
    pub requires_openai_auth: Option<bool>,
}

/// `account/updated` notification payload.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUpdated {
    /// `apiKey`, `chatgpt`, … §3 marks `chatgptAuthTokens` as
    /// "OPENAI INTERNAL USE ONLY — DO NOT USE", so treat it as absent.
    #[serde(default)]
    pub auth_mode: Option<String>,
    /// Subscription plan.
    #[serde(default)]
    pub plan_type: Option<String>,
}

/// One rate-limit window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    /// Percentage of the window consumed.
    #[serde(default)]
    pub used_percent: Option<f64>,
    /// Window length in minutes.
    #[serde(default)]
    pub window_minutes: Option<u64>,
    /// Seconds until the window resets.
    #[serde(default)]
    pub resets_in_seconds: Option<u64>,
}

impl RateLimitWindow {
    /// Field-wise merge: `Some` in the update wins, `None` keeps the old value.
    fn merge(&mut self, update: RateLimitWindow) {
        if update.used_percent.is_some() {
            self.used_percent = update.used_percent;
        }
        if update.window_minutes.is_some() {
            self.window_minutes = update.window_minutes;
        }
        if update.resets_in_seconds.is_some() {
            self.resets_in_seconds = update.resets_in_seconds;
        }
    }
}

/// Quota state, as aibo maintains it.
///
/// §3, and this is the part that is easy to get wrong: `account/rateLimits/updated`
/// is **"a sparse rolling update you must merge, not a full snapshot"**.
/// Replacing the snapshot on each notification silently blanks whichever window
/// the update did not mention. [`RateLimitSnapshot::merge`] is the correct
/// behaviour and the reason this type exists at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitSnapshot {
    /// Short window (typically hourly).
    #[serde(default)]
    pub primary: Option<RateLimitWindow>,
    /// Long window (typically weekly).
    #[serde(default)]
    pub secondary: Option<RateLimitWindow>,
}

impl RateLimitSnapshot {
    /// Fold a sparse update in. Never replaces a known window with `None`.
    pub fn merge(&mut self, update: RateLimitSnapshot) {
        merge_window(&mut self.primary, update.primary);
        merge_window(&mut self.secondary, update.secondary);
    }
}

fn merge_window(slot: &mut Option<RateLimitWindow>, update: Option<RateLimitWindow>) {
    let Some(update) = update else { return };
    match slot {
        Some(existing) => existing.merge(update),
        None => *slot = Some(update),
    }
}

// ---------------------------------------------------------------------------
// thread
// ---------------------------------------------------------------------------

/// `thread/start` params.
///
/// SPIKE: S5 — field names unverified.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    /// Working directory for the thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Model to use, when aibo overrides Codex's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Codex's approval policy. §11 recommends leaving Codex's own approval
    /// protocol as the real gate, so the default is *not* `"never"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    /// Codex's sandbox policy. §11: the sandbox is the boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<String>,
}

/// `thread/start` result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResult {
    /// The new thread's id.
    #[serde(alias = "threadId", alias = "thread_id", alias = "conversationId")]
    pub thread_id: Option<String>,
}

/// `thread/sendMessage` params.
///
/// SPIKE: S5 — field names unverified.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageParams {
    /// Target thread.
    pub thread_id: String,
    /// The turn's text. §3b: for a fresh Do run this is the instruction plus a
    /// replayable plain-text summary of the captured context.
    pub text: String,
}

/// `thread/interrupt` params.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterruptParams {
    /// Thread to abort.
    pub thread_id: String,
}

/// Token accounting as app-server reports it.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// Uncached prompt tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Prompt tokens served from cache.
    #[serde(default, alias = "cachedInputTokens")]
    pub cached_input_tokens: u64,
    /// Completion tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// Reasoning tokens.
    #[serde(default, alias = "reasoningOutputTokens")]
    pub reasoning_tokens: u64,
}

impl TokenUsage {
    /// Convert to aibo's [`Usage`]. §3b: "Cost accounting stays unified
    /// regardless: Codex turns report usage through `AgentStep`, and it lands in
    /// the same `messages` ledger."
    pub const fn to_usage(self) -> Usage {
        Usage {
            input_tokens: self.input_tokens,
            cached_input_tokens: self.cached_input_tokens,
            output_tokens: self.output_tokens,
            reasoning_tokens: self.reasoning_tokens,
            image_tokens: 0,
        }
    }
}

/// One item inside a thread event.
///
/// Every field optional, discriminator a plain string: this is the permissive
/// parse. SPIKE: S5 — item kinds and field names unverified.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadItem {
    /// Item id.
    #[serde(default)]
    pub id: Option<String>,
    /// Discriminator, e.g. `agent_message`, `reasoning`, `command_execution`,
    /// `file_change`.
    #[serde(default, rename = "type", alias = "itemType", alias = "kind")]
    pub kind: Option<String>,
    /// Text payload for message and reasoning items.
    #[serde(default)]
    pub text: Option<String>,
    /// Command line for exec items.
    #[serde(default)]
    pub command: Option<Value>,
    /// Path for file-change items.
    #[serde(default)]
    pub path: Option<String>,
    /// Unified diff for file-change items.
    #[serde(default, alias = "diff")]
    pub unified_diff: Option<String>,
    /// Anything else the binary sent.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// A `thread/event` payload.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadEvent {
    /// Which thread this belongs to. Used to demultiplex concurrent runs on one
    /// connection; when absent the event is accepted by the only active run.
    #[serde(default, alias = "conversationId")]
    pub thread_id: Option<String>,
    /// Event discriminator, e.g. `item.started`, `item.completed`,
    /// `turn.completed`, `turn.failed`.
    #[serde(default, rename = "type", alias = "kind")]
    pub kind: Option<String>,
    /// Incremental text, for streaming deltas.
    #[serde(default)]
    pub delta: Option<String>,
    /// The item this event concerns.
    #[serde(default)]
    pub item: Option<ThreadItem>,
    /// Token accounting, on turn-completion events.
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    /// Error message, on failure events. Never rendered raw (§13).
    #[serde(default)]
    pub error: Option<String>,
    /// Anything else the binary sent.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl ThreadEvent {
    /// Whether this event terminates the turn.
    pub fn is_turn_end(&self) -> bool {
        self.kind.as_deref().is_some_and(|k| {
            k.starts_with("turn.completed")
                || k.starts_with("turn.failed")
                || k.starts_with("turn.aborted")
                || k == "turnComplete"
        })
    }

    /// Whether this event reports a failed turn.
    pub fn is_turn_failure(&self) -> bool {
        self.kind
            .as_deref()
            .is_some_and(|k| k.starts_with("turn.failed"))
    }
}

// ---------------------------------------------------------------------------
// approvals
// ---------------------------------------------------------------------------

/// Params of an `execCommandApproval` request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecApprovalParams {
    /// Codex's call id.
    #[serde(default)]
    pub call_id: Option<String>,
    /// Thread the request belongs to.
    #[serde(default, alias = "conversationId")]
    pub thread_id: Option<String>,
    /// Argv, or a single string, depending on the build.
    #[serde(default)]
    pub command: Option<Value>,
    /// Working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Codex's own explanation, shown alongside aibo's summary.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Params of an `applyPatchApproval` request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchApprovalParams {
    /// Codex's call id.
    #[serde(default)]
    pub call_id: Option<String>,
    /// Thread the request belongs to.
    #[serde(default, alias = "conversationId")]
    pub thread_id: Option<String>,
    /// Map of path → change descriptor.
    #[serde(default)]
    pub changes: serde_json::Map<String, Value>,
    /// Codex's own explanation.
    #[serde(default)]
    pub reason: Option<String>,
    /// Root Codex would like granted for the rest of the session.
    #[serde(default)]
    pub grant_root: Option<String>,
}

/// Render a `command` field that may be argv or a string.
pub fn render_command(command: Option<&Value>) -> Option<String> {
    match command? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .map(|p| match p {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(" "),
        ),
        other => Some(other.to_string()),
    }
}

impl ExecApprovalParams {
    /// Convert into aibo's approval request.
    ///
    /// `originating_instruction` is the user's own typed instruction, carried in
    /// so the prompt can show that the action does not trace back to a selection
    /// (§5 rule 3).
    pub fn to_request(&self, id: String, originating_instruction: String) -> ApprovalRequest {
        let command = render_command(self.command.as_ref());
        let summary = match (&command, &self.reason) {
            (Some(c), Some(r)) => format!("{r}: {c}"),
            (Some(c), None) => format!("run `{c}`"),
            (None, Some(r)) => r.clone(),
            (None, None) => "run a command".to_owned(),
        };
        let requires_typed_confirmation = command
            .as_deref()
            .is_some_and(crate::permission_gate::is_destructive_command);
        ApprovalRequest {
            id,
            kind: ApprovalKind::Command,
            summary,
            command,
            paths: self.cwd.iter().map(std::path::PathBuf::from).collect(),
            originating_instruction,
            requires_typed_confirmation,
        }
    }
}

impl PatchApprovalParams {
    /// Convert into aibo's approval request.
    pub fn to_request(&self, id: String, originating_instruction: String) -> ApprovalRequest {
        let paths: Vec<std::path::PathBuf> =
            self.changes.keys().map(std::path::PathBuf::from).collect();
        let summary = match &self.reason {
            Some(r) => r.clone(),
            None => format!("apply changes to {} file(s)", paths.len()),
        };
        ApprovalRequest {
            id,
            kind: ApprovalKind::FileWrite,
            summary,
            command: None,
            paths,
            originating_instruction,
            // A patch is reviewable in the diff view; the typed-confirmation
            // class in §11 is destructive *commands*, not ordinary edits.
            requires_typed_confirmation: false,
        }
    }
}

/// The reply body aibo sends to an approval request.
///
/// SPIKE: S5 — the exact string values are unverified. They are centralised here
/// so a correction is one edit.
pub fn approval_reply(decision: ApprovalDecision) -> Value {
    let value = match decision {
        ApprovalDecision::Approve => "approved",
        ApprovalDecision::ApproveForSession => "approved_for_session",
        ApprovalDecision::Deny => "denied",
    };
    serde_json::json!({ "decision": value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_no_jsonrpc_field() {
        // §3: the protocol deliberately omits `"jsonrpc":"2.0"`. Writing one
        // would be wrong, so assert we never do.
        let req = OutboundRequest {
            id: RequestId::Number(1),
            method: method::INITIALIZE,
            params: Some(serde_json::json!({})),
        };
        let encoded = serde_json::to_string(&req).unwrap();
        assert!(!encoded.contains("jsonrpc"), "{encoded}");
        assert!(encoded.contains("\"method\":\"initialize\""));
    }

    #[test]
    fn classification_of_the_three_shapes() {
        let response: RawMessage = serde_json::from_str(r#"{"id":1,"result":{}}"#).unwrap();
        assert!(matches!(
            response.classify(),
            Some(Inbound::Response { .. })
        ));

        let server_request: RawMessage =
            serde_json::from_str(r#"{"id":2,"method":"execCommandApproval","params":{}}"#).unwrap();
        assert!(matches!(
            server_request.classify(),
            Some(Inbound::Request { .. })
        ));

        let notification: RawMessage =
            serde_json::from_str(r#"{"method":"thread/event","params":{}}"#).unwrap();
        assert!(matches!(
            notification.classify(),
            Some(Inbound::Notification { .. })
        ));
    }

    #[test]
    fn unknown_fields_are_ignored_not_rejected() {
        let event: ThreadEvent = serde_json::from_str(
            r#"{"type":"item.completed","threadId":"t1","somethingNew":{"a":1}}"#,
        )
        .expect("permissive parse");
        assert_eq!(event.thread_id.as_deref(), Some("t1"));
        assert!(event.extra.contains_key("somethingNew"));
    }

    #[test]
    fn rate_limits_merge_rather_than_replace() {
        // §3: `account/rateLimits/updated` is a sparse rolling update.
        let mut snapshot = RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: Some(10.0),
                window_minutes: Some(60),
                resets_in_seconds: Some(1800),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: Some(3.0),
                window_minutes: Some(10_080),
                resets_in_seconds: None,
            }),
        };
        let update: RateLimitSnapshot =
            serde_json::from_str(r#"{"primary":{"usedPercent":12.5}}"#).unwrap();
        snapshot.merge(update);

        let primary = snapshot.primary.unwrap();
        assert_eq!(primary.used_percent, Some(12.5));
        // The field the update did not mention survives — this is the bug the
        // merge exists to prevent.
        assert_eq!(primary.window_minutes, Some(60));
        // And so does the window the update did not mention at all.
        assert_eq!(snapshot.secondary.unwrap().used_percent, Some(3.0));
    }

    #[test]
    fn version_parsing_and_classification() {
        assert_eq!("0.63.0".parse::<Version>().unwrap(), Version::new(0, 63, 0));
        assert_eq!("v0.64".parse::<Version>().unwrap(), Version::new(0, 64, 0));
        assert_eq!(
            "0.65.1-alpha.2".parse::<Version>().unwrap(),
            Version::new(0, 65, 1)
        );
        assert!("nonsense".parse::<Version>().is_err());

        assert_eq!(classify(Version::new(0, 62, 9)), VersionVerdict::TooOld);
        assert_eq!(classify(MIN_SUPPORTED), VersionVerdict::Supported);
        assert_eq!(
            classify(Version::new(9, 0, 0)),
            VersionVerdict::NewerThanTested
        );
    }

    #[test]
    fn version_is_found_in_the_user_agent_when_no_field_carries_it() {
        let result: InitializeResult =
            serde_json::from_str(r#"{"userAgent":"codex_app_server/0.63.0 (macos)"}"#).unwrap();
        assert_eq!(result.detected_version(), Some(Version::new(0, 63, 0)));
    }

    #[test]
    fn argv_commands_render() {
        let v = serde_json::json!(["git", "push", "--force"]);
        assert_eq!(
            render_command(Some(&v)).as_deref(),
            Some("git push --force")
        );
    }

    #[test]
    fn force_push_approval_demands_typed_confirmation() {
        let params = ExecApprovalParams {
            command: Some(serde_json::json!(["git", "push", "--force"])),
            ..ExecApprovalParams::default()
        };
        let req = params.to_request("1".into(), "ship it".into());
        assert!(req.requires_typed_confirmation);
        assert_eq!(req.originating_instruction, "ship it");
    }
}
