//! The domain vocabulary every other crate codes against.
//!
//! Nothing in this module performs I/O. Types that describe platform state
//! (`FieldContext`, `DisplayInfo`, …) are plain data snapshots produced by
//! [`crate::traits::PlatformBackend`] and consumed by the router, prompt
//! assembly and the UI.

use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::error::Result;

/// A boxed, `Send` stream — the shape every streaming trait method returns.
///
/// Re-exported so downstream crates do not have to agree on a `futures`
/// version just to name a return type.
pub type BoxStream<'a, T> = futures_core::stream::BoxStream<'a, T>;

// ---------------------------------------------------------------------------
// Surfaces, roles, verbs
// ---------------------------------------------------------------------------

/// The product surfaces that produce a model request (§1).
///
/// `Compute` from §1 is deliberately **not** a variant: it is evaluated inline
/// by `fend-core` before any routing happens, never reaches a provider, and is
/// not one of the `complete|transform|ask|do` values stored in the
/// `conversations.surface` column (§12). Treat Compute as a property of the
/// panel input, not as a routed surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    /// Continue the user's text in the focused field.
    Complete,
    /// Rewrite the selection in place.
    Transform,
    /// Chat panel, with capture available as attachments.
    Ask,
    /// Agentic run: tools, code execution, MCP, or a delegate backend.
    Do,
}

impl Surface {
    /// The surface's first-token latency target (§1), used to decide whether a
    /// `429 retry_after` is short enough to wait out or should fall back (§4).
    pub const fn first_token_target(self) -> Duration {
        match self {
            Surface::Complete => Duration::from_millis(250),
            Surface::Transform => Duration::from_millis(400),
            Surface::Ask => Duration::from_millis(600),
            // Streamed steps; no first-token target.
            Surface::Do => Duration::from_secs(30),
        }
    }
}

/// The routing substrate (§4). Every request resolves to exactly one role, and
/// each role binds to an ordered [`RoleChain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Lowest latency; Complete and short Transform.
    Fast,
    /// Highest quality; long or code-bearing Transform, and Ask.
    Smart,
    /// Cheapest available, typically local.
    Cheap,
    /// Image-capable.
    Vision,
    /// Tool-calling backend for the Do surface.
    Agent,
}

/// A leading verb parsed out of the panel input (§4).
///
/// Custom verbs registered by saved actions (§12 `actions.verb`) are not
/// represented here — they are matched by the rule list, which sees the raw
/// input, so this enum stays `Copy` and exhaustively testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    /// Translate the payload; the one verb that overrides language matching (§5).
    Translate,
    /// Define a word or phrase.
    Define,
    /// Fix grammar/spelling/syntax.
    Fix,
    /// Explain the payload.
    Explain,
    /// Summarise the payload.
    Summarise,
    /// Spell-check only.
    Spell,
    /// Unit / format conversion that is not pure Compute.
    Convert,
    /// Rewrite with an instruction.
    Rewrite,
    /// Shorten.
    Shorten,
    /// Expand.
    Expand,
}

/// Input to the router (§4). Every field is cheaply computable during context
/// capture; no tokenizer runs on the hot path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteInput {
    /// The surface resolved by §1's rule, already frozen for the session.
    pub surface: Surface,
    /// Estimated instruction tokens. See [`crate::traits`] note: estimate as
    /// `ascii_chars/4 + cjk_chars`, never `bytes/4` (§4).
    pub prompt_tokens: usize,
    /// Estimated selection + clipboard + field prefix tokens.
    pub payload_tokens: usize,
    /// Fenced block, source app in the code-app list, or >30% non-prose chars.
    pub has_code: bool,
    /// An image attachment is present.
    pub has_image: bool,
    /// Parsed leading verb, if any.
    pub verb: Option<Verb>,
    /// `@model` or `⌘1..4`; wins over every other rule.
    pub user_override: Option<Role>,
}

// ---------------------------------------------------------------------------
// Captured platform context
// ---------------------------------------------------------------------------

/// A rectangle in display coordinates. Coordinates may be negative — displays
/// left of or above the primary are normal (§9).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width in logical points.
    pub width: f64,
    /// Height in logical points.
    pub height: f64,
}

/// A cheap, instantly obtainable handle to an application and window.
///
/// §8 step 1: this and [`DisplayInfo`] are the only things captured
/// synchronously on hotkey-down, because they cannot change and cost nothing.
/// Everything else is captured asynchronously with a deadline.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppRef {
    /// OS process id of the frontmost application.
    pub pid: i32,
    /// Opaque platform window identifier (`CGWindowID` / `HWND` as `u64`).
    pub window: Option<u64>,
}

/// Identity of the application that had focus when the hotkey fired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    /// The handle this info was resolved from.
    pub app_ref: AppRef,
    /// Bundle identifier on macOS, executable name on Windows (§12
    /// `conversations.source_app`).
    pub identifier: String,
    /// Localised display name, for UI only.
    pub display_name: String,
    /// The app is on the code-app list, which feeds `RouteInput::has_code` (§4).
    pub is_code_app: bool,
}

/// A snapshot of the focused text field (§7, §5).
///
/// Both `prefix` and `suffix` are bounded at the capture boundary — §5 forbids
/// pulling a whole document out of the target app before deciding it is too
/// long.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldContext {
    /// Text immediately before the caret (§5 targets ~800 characters).
    pub prefix: String,
    /// Text immediately after the caret, kept separate and labelled as such —
    /// completing into the middle of existing text without it produces
    /// duplicates (§5).
    pub suffix: String,
    /// Byte offset of the caret within the field's full value, when the
    /// platform reports it. `None` when only a selection range was available.
    pub caret: Option<usize>,
    /// The field's accessibility label, included in the Complete prompt (§5).
    pub label: Option<String>,
    /// The field is a password/secure field. When true, `prefix` and `suffix`
    /// MUST be empty: §5 forbids capturing from secure fields at all.
    pub is_secure: bool,
    /// An IME composition is active (§9). When true aibo neither reads the
    /// field nor inserts — it shows "finish typing to continue".
    pub ime_active: bool,
    /// The capture was cut short by the byte/character cap.
    pub truncated: bool,
    /// Caret or selection bounds in display coordinates, used to anchor the
    /// panel (§9). `None` falls back to the display-centre placement rule.
    pub caret_bounds: Option<Rect>,
}

/// What kind of payload the clipboard currently holds (§12
/// `clipboard_history.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardKind {
    /// Plain or rich text, flattened to text.
    Text,
    /// An image, referenced rather than inlined.
    ImageRef,
    /// One or more file paths.
    Files,
    /// Present but of a type aibo does not handle.
    Unsupported,
    /// The clipboard is empty.
    Empty,
}

/// A clipboard snapshot, with the hygiene flags §12 requires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardItem {
    /// Payload kind.
    pub kind: ClipboardKind,
    /// Text payload. Always `None` when `concealed` is true — concealed items
    /// are never recorded and never sent (§12).
    pub text: Option<String>,
    /// File payload.
    pub files: Vec<PathBuf>,
    /// Marked concealed by the source (`org.nspasteboard.ConcealedType` /
    /// `ExcludeClipboardContentFromMonitorProcessing`), or the source app is on
    /// the denylist.
    pub concealed: bool,
    /// Marked transient by the source; usable now, never persisted.
    pub transient: bool,
    /// The app that placed the item, when known.
    pub source_app: Option<String>,
    /// macOS `changeCount` / Windows clipboard sequence number at capture time.
    ///
    /// Save/restore is a race, not an assignment: if this changed for a reason
    /// that was not aibo, do not restore (§12).
    pub sequence: u64,
    /// The payload could not be faithfully restored (promised/deferred
    /// formats). Detect and decline rather than replacing rich content with
    /// plain text (§12).
    pub restorable: bool,
}

/// A display, for panel placement (§9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// Stable platform identifier, used to remember placement across shows.
    pub id: u64,
    /// Full bounds, possibly negative.
    pub bounds: Rect,
    /// Bounds minus menu bar / taskbar. The panel is clamped inside this.
    pub visible_frame: Rect,
    /// Backing scale factor. Recompute on every show, not just at creation (§9).
    pub scale_factor: f64,
    /// This is the primary display, the fallback when a remembered one is gone.
    pub is_primary: bool,
}

/// How text is written back into the target application (§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsertMode {
    /// Default: write the pasteboard, synthesise the paste chord, restore the
    /// previous clipboard if and only if the sequence number still matches.
    PasteAndRestore,
    /// Paste without restoring — used when restore would destroy something the
    /// user copied in the meantime, or the previous content was concealed.
    PasteKeepNew,
    /// Synthesise keystrokes directly (`CGEvent::set_string` / `SendInput`).
    /// Short inserts only; see §8 for the enigo failure modes.
    Keystroke,
}

/// Everything that must still be true at insert time (§8).
///
/// Captured alongside the context and re-validated immediately before every
/// insert. If any field differs, aibo does not insert — it offers "copy
/// instead". Pasting a rewrite over the wrong content is unrecoverable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsertTarget {
    /// The app and window that were focused at capture.
    pub app_ref: AppRef,
    /// Opaque identity of the focused element, when the platform can express one.
    pub focused_element: Option<String>,
    /// Hash of the selection text as captured.
    pub selection_hash: Option<u64>,
    /// Hash of the field prefix as captured.
    pub prefix_hash: Option<u64>,
}

/// Power / display transitions the platform layer forwards (§13 sleep-wake).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerEvent {
    /// The machine is about to sleep; pooled connections will die.
    WillSleep,
    /// The machine woke. Re-warm the connection pool and re-probe health —
    /// otherwise the first hotkey of the day misses the latency budget (§13).
    DidWake,
    /// Displays were added, removed or reconfigured; re-clamp the panel (§9).
    DisplaysChanged,
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// An OS permission aibo may need (§8, §17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// macOS Accessibility (AX reads). A distinct TCC service from
    /// [`Permission::PostEvents`] despite sharing one settings pane, and the
    /// one that is incompatible with the App Sandbox (§8).
    Accessibility,
    /// macOS keystroke synthesis (`CGEventPost`). Not required for AX reads.
    PostEvents,
    /// Windows: read/`SendInput` against elevated windows. Requires
    /// `uiAccess=true`, Authenticode signing and installation under Program
    /// Files (§8); usually [`PermissionStatus::NotApplicable`].
    ElevatedWindowAccess,
    /// User-visible notifications.
    Notifications,
    /// Launch at login (`SMAppService` / Run key).
    Autostart,
}

/// The state of a [`Permission`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionStatus {
    /// Granted and usable now.
    Granted,
    /// Explicitly denied by the user.
    Denied,
    /// Never asked.
    NotDetermined,
    /// Blocked by policy (MDM, parental controls).
    Restricted,
    /// Meaningless on this platform.
    NotApplicable,
    /// Previously granted, now gone — typically a TCC reset after an update
    /// (§17). Distinguished from `Denied` because it gets a recovery screen.
    Revoked,
}

// ---------------------------------------------------------------------------
// Models, capabilities, usage
// ---------------------------------------------------------------------------

/// Identifier for a configured provider backend.
///
/// A newtype rather than an enum: users can add OpenAI-compatible endpoints
/// that ship with no code change, so the set is open.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProviderId(Cow<'static, str>);

impl ProviderId {
    /// Cerebras — primary `Fast` binding (§10).
    pub const CEREBRAS: Self = Self::from_static("cerebras");
    /// SambaNova (§10).
    pub const SAMBANOVA: Self = Self::from_static("sambanova");
    /// Groq (§10). Distinct from xAI's Grok.
    pub const GROQ: Self = Self::from_static("groq");
    /// xAI (§10). Distinct from Groq.
    pub const XAI: Self = Self::from_static("xai");
    /// OpenAI (§10).
    pub const OPENAI: Self = Self::from_static("openai");
    /// Anthropic (§10).
    pub const ANTHROPIC: Self = Self::from_static("anthropic");
    /// Azure OpenAI (§10).
    pub const AZURE_OPENAI: Self = Self::from_static("azure-openai");
    /// Google Vertex AI (§10).
    pub const VERTEX: Self = Self::from_static("vertex");
    /// AWS Bedrock (§10).
    pub const BEDROCK: Self = Self::from_static("bedrock");
    /// Ollama / llama.cpp — the offline story (§13).
    pub const OLLAMA: Self = Self::from_static("ollama");
    /// The Codex Responses endpoint reached with aibo's own device-code OAuth
    /// (§3a). Contingent on spike S6.
    pub const CODEX: Self = Self::from_static("codex");

    /// Build an id from a `'static` string, usable in `const` position.
    pub const fn from_static(s: &'static str) -> Self {
        Self(Cow::Borrowed(s))
    }

    /// Build an id from a runtime string (user-added endpoints).
    pub fn new(s: impl Into<String>) -> Self {
        Self(Cow::Owned(s.into()))
    }

    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a backend handles a request for more than one candidate (§5).
///
/// "Three candidates" is not portable: some providers support `n>1` natively,
/// some ignore it, some charge for it. The fallback is documented per model
/// rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultiCandidate {
    /// `n` is honoured natively and billed once for the shared prompt.
    Native,
    /// `n` is accepted and silently ignored; ask for one.
    Ignored,
    /// Not supported; either issue parallel requests (at cost) or ask for a
    /// single response containing labelled options.
    Unsupported,
}

/// What a **model** can do.
///
/// §10 correction: an earlier draft hung this off `Provider`, which breaks the
/// moment one provider serves both a vision model and a text-only one.
/// `Capabilities` belongs to [`ModelInfo`]; [`crate::traits::Provider::capabilities`]
/// only returns the provider's defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Tool / function calling.
    pub tools: bool,
    /// Image input.
    pub vision: bool,
    /// Incremental streaming (all v1 providers; `false` forces buffering).
    pub streaming: bool,
    /// A `reasoning_effort`-style knob is accepted.
    pub reasoning_effort: bool,
    /// Structured output constrained by a JSON schema.
    pub json_schema: bool,
    /// Prompt caching, which changes the price table's cached-input rate (§14).
    pub prompt_cache: bool,
    /// Fill-in-the-middle endpoint. Gated on spike S9 (§5) — leave `false`
    /// until the eval harness says FIM beats chat-instruct for Complete.
    pub fim: bool,
    /// Multi-candidate behaviour (§5).
    pub multi_candidate: MultiCandidate,
    /// Total context window in tokens.
    pub max_context: usize,
    /// Maximum output tokens, when the provider states one.
    pub max_output: Option<usize>,
}

impl Default for Capabilities {
    /// The conservative floor: text in, text out, streamed, 8k context.
    fn default() -> Self {
        Self {
            tools: false,
            vision: false,
            streaming: true,
            reasoning_effort: false,
            json_schema: false,
            prompt_cache: false,
            fim: false,
            multi_candidate: MultiCandidate::Unsupported,
            max_context: 8_192,
            max_output: None,
        }
    }
}

/// A single model offered by a provider.
///
/// Model catalogues rot (§10): role bindings point at concrete ids and
/// providers retire them, so `deprecated` and `replaced_by` exist to render
/// "the model you selected no longer exists, here's the closest" instead of an
/// opaque 400.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// The provider serving this model.
    pub provider: ProviderId,
    /// Wire identifier, e.g. `llama-3.3-70b` or a Bedrock region-scoped ARN.
    pub id: String,
    /// Human-readable name for settings.
    pub display_name: String,
    /// Per-model capabilities (§10).
    pub capabilities: Capabilities,
    /// The provider has announced retirement.
    pub deprecated: bool,
    /// Suggested successor when `deprecated`.
    pub replaced_by: Option<String>,
}

/// Token accounting for one request.
///
/// A single input/output pair is not enough to price any current frontier model
/// (§14), so cached input, reasoning and image tokens are tracked separately.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached prompt tokens.
    pub input_tokens: u64,
    /// Prompt tokens served from the provider's cache, billed differently.
    pub cached_input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Reasoning tokens, where the provider reports them separately.
    pub reasoning_tokens: u64,
    /// Image input tokens.
    pub image_tokens: u64,
}

impl Usage {
    /// Total tokens, for [`AgentLimits::max_total_tokens`] enforcement.
    pub const fn total(&self) -> u64 {
        self.input_tokens
            + self.cached_input_tokens
            + self.output_tokens
            + self.reasoning_tokens
            + self.image_tokens
    }
}

/// Why a stream ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished normally.
    EndTurn,
    /// Hit the output token limit. One of the two objective signals that offer
    /// escalation to `Smart` (§4).
    Length,
    /// Hit a configured stop sequence.
    StopSequence,
    /// The model wants a tool result before continuing.
    ToolUse,
    /// Blocked by the provider's content filter.
    ContentFilter,
    /// Cancelled by the user. The partial text stays in the panel marked
    /// truncated and is never auto-inserted (§13).
    Cancelled,
}

/// One event from a provider stream (§7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamEvent {
    /// Assistant text.
    Text(String),
    /// Reasoning content on its own channel: render collapsed, never insert.
    Reasoning(String),
    /// A tool invocation request.
    ToolCall {
        /// Provider-assigned call id, echoed back with the result.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments as the model produced them; validate before use.
        args: serde_json::Value,
    },
    /// Token accounting; drives the spend meter (§14). May never arrive on a
    /// cancelled or failed stream, which is why cost is reserved at dispatch.
    Usage(Usage),
    /// Terminal event.
    Done(StopReason),
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// Where a piece of content came from (§5).
///
/// The distinction is a security control, not bookkeeping: content whose origin
/// is capture can never authorise a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentOrigin {
    /// The user's own typed instruction. The **only** origin allowed to
    /// authorise a tool call (§5 rule 2).
    UserInstruction,
    /// Selected text read from the target app.
    Selection,
    /// Text before the caret in the focused field.
    FieldPrefix,
    /// Text after the caret in the focused field.
    FieldSuffix,
    /// The system clipboard.
    Clipboard,
    /// A file the user attached.
    File,
    /// The result of a tool call.
    ToolResult,
    /// A result returned by an MCP server.
    McpResult,
}

impl ContentOrigin {
    /// Whether content from this origin may authorise a tool call (§5 rule 2).
    pub const fn may_authorise_tools(self) -> bool {
        matches!(self, ContentOrigin::UserInstruction)
    }
}

/// A block of attacker-controlled content (§5, "Captured content is untrusted
/// input").
///
/// Selections, clipboard contents, file contents and tool results are all
/// attacker-controlled: any web page can place text designed to read as
/// instructions. Blocks are **structurally fenced and labelled untrusted** in
/// every prompt and never interpolated inline with the user's instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UntrustedBlock {
    /// Where the content came from.
    pub origin: ContentOrigin,
    /// A short label rendered in the fence header, e.g. `selection from Slack`.
    pub label: String,
    /// The content itself, already truncated to the budget.
    pub content: String,
    /// The content was middle-out truncated (§5) and carries an omission marker.
    pub truncated: bool,
}

/// Who authored a message (§12 `messages.role`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    /// The versioned system prompt (§5).
    System,
    /// The user.
    User,
    /// The model.
    Assistant,
    /// A tool result being fed back.
    Tool,
}

/// One part of a multi-part message body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentPart {
    /// Trusted text authored by aibo or typed by the user.
    Text(String),
    /// Captured content, already fenced and labelled by prompt assembly.
    Untrusted(UntrustedBlock),
    /// An image.
    Image {
        /// MIME type, e.g. `image/png`.
        mime: String,
        /// Base64-encoded bytes.
        data_base64: String,
    },
}

/// A conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Author.
    pub role: MessageRole,
    /// Body.
    pub parts: Vec<ContentPart>,
    /// For [`MessageRole::Tool`]: the call this result answers.
    pub tool_call_id: Option<String>,
}

impl Message {
    /// A plain single-part text message.
    pub fn text(role: MessageRole, body: impl Into<String>) -> Self {
        Self {
            role,
            parts: vec![ContentPart::Text(body.into())],
            tool_call_id: None,
        }
    }
}

/// A tool exposed to the model by `NativeLoop` (§5 "Do").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Tool name as the model will call it.
    pub name: String,
    /// One-line description.
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: serde_json::Value,
    /// Permission tier 0..=4 (§11), shown in the approval prompt.
    pub tier: u8,
}

/// Sampling and decoding parameters (§5 per-surface specs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationParams {
    /// Output cap. §5: 64 for Complete.
    pub max_tokens: u32,
    /// §5: 0.2 for Complete and Transform, 0.7 for Ask.
    pub temperature: f32,
    /// Nucleus sampling, when the provider supports it.
    pub top_p: Option<f32>,
    /// Stop sequences. §5: `\n\n` for Complete.
    pub stop: Vec<String>,
    /// Candidate count. Honoured per [`Capabilities::multi_candidate`].
    pub candidates: u8,
    /// Reasoning effort, when [`Capabilities::reasoning_effort`].
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Response schema, when [`Capabilities::json_schema`].
    pub json_schema: Option<serde_json::Value>,
    /// Determinism hint, where supported.
    pub seed: Option<u64>,
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            max_tokens: 1024,
            temperature: 0.2,
            top_p: None,
            stop: Vec::new(),
            candidates: 1,
            reasoning_effort: None,
            json_schema: None,
            seed: None,
        }
    }
}

/// Provider-independent reasoning effort knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Minimal reasoning; lowest latency.
    Low,
    /// Provider default.
    Medium,
    /// Maximum reasoning; highest cost.
    High,
}

/// A concrete `(provider, model)` pair (§4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelBinding {
    /// Provider to dispatch to.
    pub provider: ProviderId,
    /// Wire model id.
    pub model: String,
}

/// A role's ordered fallback chain (§4).
///
/// §14: fallback is a spend *and* privacy decision. `fallback_enabled` is
/// per-role and off unless the user turns it on, because a silent retry can
/// double-spend and can send the user's selected text to a provider they did
/// not choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleChain {
    /// The role this chain serves.
    pub role: Role,
    /// Ordered candidates; the first is the primary.
    pub entries: Vec<ModelBinding>,
    /// Whether entries after the first may be used at all.
    pub fallback_enabled: bool,
    /// Whether fallback may cross a provider trust boundary (secure tier →
    /// public tier). Requires explicit consent (§14).
    pub allow_crossing_trust_boundary: bool,
}

/// Per-request limits enforced in `aibo-core` before the request is built (§14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestBudget {
    /// Context cap for this role, derived from the model's context minus an
    /// output reserve (§5).
    pub max_context_tokens: usize,
    /// Hard cap on captured payload, §5: 50% of the model's context regardless
    /// of budget, so a huge selection can never crowd out the instruction.
    pub max_payload_tokens: usize,
    /// Output cap; mirrors [`GenerationParams::max_tokens`].
    pub max_output_tokens: u32,
    /// Cost reserved at dispatch and reconciled when real `Usage` lands (§14).
    /// A meter that only counts completed responses under-reports.
    pub reserved_cost_micros: u64,
    /// Wall-clock ceiling for the whole request.
    pub deadline: Duration,
}

/// A fully assembled chat request.
///
/// Not specified by the plan; defined here as the contract between
/// `aibo-core`'s prompt assembly and every `Provider` implementation.
///
/// **Invariant.** `messages` is the complete, ordered payload to send.
/// `untrusted` is the structural record of the captured content that prompt
/// assembly already fenced into those messages — providers must **never**
/// re-inline it. It exists so that the permission gate can prove no tool call
/// originated from capture (§5 rule 2) and so tier 3/4 approval prompts can
/// show the originating instruction (§11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Correlation id; also the `messages.id` root for persistence (§12).
    pub id: Uuid,
    /// Conversation this belongs to, when there is history.
    pub conversation_id: Option<Uuid>,
    /// The frozen surface (§1).
    pub surface: Surface,
    /// The routed role (§4).
    pub role: Role,
    /// The chain entry actually being attempted.
    pub binding: ModelBinding,
    /// Ordered messages, system prompt first.
    pub messages: Vec<Message>,
    /// Sampling parameters.
    pub params: GenerationParams,
    /// Enforced budget (§14).
    pub budget: RequestBudget,
    /// Tools offered to the model. Empty for Complete/Transform/Ask.
    pub tools: Vec<ToolSchema>,
    /// The user's own typed instruction, verbatim.
    ///
    /// Kept separate from `messages` because it is the *origin check* for tool
    /// authorisation and the text shown in tier 3/4 approval prompts (§5, §11).
    pub user_instruction: Option<String>,
    /// Captured, attacker-controlled content in structural form. See the
    /// invariant on this type.
    pub untrusted: Vec<UntrustedBlock>,
    /// Version stamp of the prompt template used, so golden tests and the eval
    /// harness can attribute a regression to a prompt edit (§5).
    pub prompt_version: String,
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// A unit of agentic work handed to an [`crate::traits::AgentBackend`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTask {
    /// Correlation id.
    pub id: Uuid,
    /// The user's instruction. For `CodexAppServer` this is sent largely
    /// unmodified — Codex owns its own system prompt (§5).
    pub instruction: String,
    /// Working directory for file and shell operations.
    pub workspace: Option<PathBuf>,
    /// Captured context, fenced and labelled untrusted (§5).
    pub context: Vec<UntrustedBlock>,
    /// Model binding for `NativeLoop`. Ignored by delegates that pick their own.
    pub binding: Option<ModelBinding>,
    /// Conversation to append steps to (§12).
    pub conversation_id: Option<Uuid>,
}

/// What tier a tool call sits at (§11), carried on every step so the UI can
/// show the right approval affordance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTier {
    /// Builtin, pure, no I/O.
    Builtin,
    /// Sandboxed code (rquickjs / wasmtime).
    Sandboxed,
    /// MCP server.
    Mcp,
    /// Shell and filesystem.
    ShellFs,
    /// Delegated to an agent backend with its own approval protocol.
    Delegate,
}

/// What is being approved (§11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// Execute a command.
    Command,
    /// Write or delete files.
    FileWrite,
    /// Make network requests.
    Network,
    /// Call a tool on an MCP server.
    McpTool,
}

/// A blocking approval request.
///
/// §11: approval happens **before** the write, not after — by the time there is
/// a diff, the side effects already happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Backend-assigned id, echoed in the decision.
    pub id: String,
    /// What kind of action.
    pub kind: ApprovalKind,
    /// One-line summary for the prompt.
    pub summary: String,
    /// The exact command, when `kind` is [`ApprovalKind::Command`].
    pub command: Option<String>,
    /// Paths that will be touched, canonicalised.
    pub paths: Vec<PathBuf>,
    /// The user instruction this action traces back to. Shown so the user can
    /// see the request did not come from a selection (§5 rule 3).
    pub originating_instruction: String,
    /// Typed confirmation is required (destructive command class, §11).
    pub requires_typed_confirmation: bool,
}

/// The user's answer to an [`ApprovalRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Allow once.
    Approve,
    /// Allow and remember for this session.
    ApproveForSession,
    /// Refuse.
    Deny,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Finished normally.
    Completed,
    /// Cancelled by the user.
    Cancelled,
    /// Failed. The message is already redacted for display.
    Failed(String),
    /// Stopped by an [`AgentLimits`] ceiling; the UI offers "continue anyway"
    /// (§14).
    BudgetExceeded(BudgetKind),
}

/// Terminal payload of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOutcome {
    /// Why the run ended.
    pub status: AgentStatus,
    /// Token accounting for the whole run, for the spend meter (§14).
    pub usage: Usage,
    /// Steps executed, for limit reporting.
    pub steps: u32,
}

/// One streamed event from an agent run (§7).
///
/// Designed against `codex app-server`'s JSON-RPC events and approval requests;
/// `NativeLoop` conforms to this, not the other way round.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStep {
    /// Reasoning text; render collapsed.
    Thought(String),
    /// A tool is being invoked.
    ToolUse {
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments.
        args: serde_json::Value,
        /// Permission tier (§11).
        tier: ToolTier,
    },
    /// A file change, as a unified diff.
    ///
    /// §11: this is "revert these file changes", not an undo for the whole
    /// operation — it cannot reverse processes started or network calls made.
    FileDiff {
        /// Path affected.
        path: PathBuf,
        /// Unified diff text.
        unified_diff: String,
    },
    /// Assistant message to show the user.
    Message(String),
    /// The run is blocked on the user.
    AwaitingApproval(ApprovalRequest),
    /// Terminal.
    Done(AgentOutcome),
}

/// Which budget ceiling was hit (§13, §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetKind {
    /// Token ceiling.
    Tokens,
    /// Monetary ceiling.
    Cost,
    /// Step or tool-call ceiling.
    Steps,
}

/// Mandatory, not advisory (§14). A runaway loop on a metered provider is a
/// support incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLimits {
    /// Maximum agent steps.
    pub max_steps: u32,
    /// Maximum tool calls.
    pub max_tool_calls: u32,
    /// Wall-clock ceiling.
    pub max_wall_clock: Duration,
    /// Token ceiling across the whole run.
    pub max_total_tokens: u64,
}

impl Default for AgentLimits {
    /// The defaults §14 specifies: 25 steps, 50 tool calls, 5 minutes, 200k tokens.
    fn default() -> Self {
        Self {
            max_steps: 25,
            max_tool_calls: 50,
            max_wall_clock: Duration::from_secs(5 * 60),
            max_total_tokens: 200_000,
        }
    }
}

/// How a run's tools are isolated (§11 threat model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    /// No isolation; path checks are advisory only.
    None,
    /// The backend runs work in its own sandbox (Codex). The strongest
    /// configuration in the product (§11).
    Delegated,
    /// OS-level sandbox (seatbelt / job object + AppContainer).
    Os,
}

/// What an [`crate::traits::AgentBackend`] supports (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentFeatures {
    /// Can edit files.
    pub file_edits: bool,
    /// Can run shell commands.
    pub shell: bool,
    /// Can call MCP servers.
    pub mcp: bool,
    /// Emits [`AgentStep::AwaitingApproval`] before side effects.
    pub pre_write_approval: bool,
    /// Emits [`AgentStep::FileDiff`] as it goes.
    pub streaming_diffs: bool,
    /// Honours [`AgentTask::binding`].
    pub model_selection: bool,
    /// A run can be resumed after aibo restarts.
    pub resume: bool,
    /// Isolation level.
    pub sandbox: SandboxKind,
}

// ---------------------------------------------------------------------------
// Credentials and health
// ---------------------------------------------------------------------------

/// Supplies a bearer token, refreshing it internally (§7).
///
/// Implementations must refresh ahead of expiry with jitter, and must be safe
/// to call concurrently.
#[async_trait::async_trait]
pub trait TokenProvider: Send + Sync {
    /// A currently valid token.
    async fn token(&self) -> Result<SecretString>;

    /// A stable label for logs and settings. Must never contain the token.
    fn label(&self) -> &str;
}

/// AWS credential resolution strategy for SigV4 (§7).
///
/// Bedrock signs per-request rather than carrying a bearer token, which is on
/// its own enough to justify per-provider implementations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialChain {
    /// The standard provider chain: env, profile, IMDS, container.
    Default,
    /// A named profile from the shared config file.
    Profile(String),
    /// A named role to assume.
    AssumeRole {
        /// Role ARN.
        role_arn: String,
        /// Session name.
        session_name: String,
    },
}

/// How aibo authenticates to a provider (§7).
///
/// The "high security" tier does not use API keys at all, so this abstraction
/// exists from day one rather than being retrofitted.
///
/// `Debug` is implemented by hand and prints variant names only — see the
/// redaction test in [`crate::error`].
#[derive(Clone)]
pub enum Credential {
    /// A bearer API key: Cerebras, SambaNova, Groq, xAI, OpenAI, Anthropic.
    ApiKey(SecretString),
    /// Azure OpenAI with a deployment-scoped URL and an `api-version`.
    AzureKey {
        /// The key.
        key: SecretString,
        /// Deployment name (part of the URL, not a model id).
        deployment: String,
        /// `api-version` query parameter; it matters (§10).
        api_version: String,
    },
    /// Azure with managed identity or device code.
    EntraId(Arc<dyn TokenProvider>),
    /// Vertex: service-account JWT exchanged for an OAuth2 token, auto-refreshed.
    GcpServiceAccount(Arc<dyn TokenProvider>),
    /// Bedrock: per-request SigV4 signing, region-scoped.
    AwsSigV4 {
        /// Credential resolution strategy.
        chain: CredentialChain,
        /// AWS region.
        region: String,
    },
    /// Ollama / llama.cpp — no auth.
    LocalEndpoint(Url),
    /// aibo's own device-code tokens for the Codex endpoint (§3a).
    ///
    /// Storage note (§12): a multi-kilobyte JWT does not fit in Windows
    /// Credential Manager (~1280 ASCII characters after `keyring`'s UTF-16
    /// doubling); token-shaped secrets need DPAPI file storage or chunking.
    ChatGptOAuth(Arc<dyn TokenProvider>),
}

impl fmt::Debug for Credential {
    /// Prints the variant name and non-secret metadata only. Never the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credential::ApiKey(_) => f.write_str("Credential::ApiKey(<redacted>)"),
            Credential::AzureKey {
                deployment,
                api_version,
                ..
            } => write!(
                f,
                "Credential::AzureKey {{ key: <redacted>, deployment: {deployment:?}, api_version: {api_version:?} }}"
            ),
            Credential::EntraId(_) => f.write_str("Credential::EntraId(<redacted>)"),
            Credential::GcpServiceAccount(_) => {
                f.write_str("Credential::GcpServiceAccount(<redacted>)")
            }
            Credential::AwsSigV4 { region, .. } => {
                write!(
                    f,
                    "Credential::AwsSigV4 {{ chain: <redacted>, region: {region:?} }}"
                )
            }
            // A local endpoint URL can embed credentials; print the host only.
            Credential::LocalEndpoint(url) => write!(
                f,
                "Credential::LocalEndpoint({:?})",
                url.host_str().unwrap_or("<none>")
            ),
            Credential::ChatGptOAuth(_) => f.write_str("Credential::ChatGptOAuth(<redacted>)"),
        }
    }
}

/// Result of a provider health probe.
///
/// §13: offline is **per-provider with hysteresis**, never one global boolean.
/// Mark a provider degraded after N consecutive failures, probe before clearing
/// it, and never flap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Reachable, with the observed round trip.
    Ok {
        /// Probe latency.
        latency: Duration,
    },
    /// Reachable but failing, or recently failed and not yet re-probed.
    Degraded {
        /// Human-readable, already redacted.
        reason: String,
        /// Consecutive failures, for the hysteresis rule.
        consecutive_failures: u32,
    },
    /// Not reachable at all — connect failure, DNS failure or captive portal.
    /// Distinguish these in `reason`; a reachability API lies (§13).
    Unavailable {
        /// Human-readable, already redacted.
        reason: String,
    },
    /// Never probed.
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_are_const_constructible() {
        const CEREBRAS: ProviderId = ProviderId::CEREBRAS;
        assert_eq!(CEREBRAS.as_str(), "cerebras");
        assert_eq!(ProviderId::new("my-endpoint").to_string(), "my-endpoint");
    }

    #[test]
    fn agent_limits_match_section_14_defaults() {
        let l = AgentLimits::default();
        assert_eq!(l.max_steps, 25);
        assert_eq!(l.max_tool_calls, 50);
        assert_eq!(l.max_wall_clock, Duration::from_secs(300));
        assert_eq!(l.max_total_tokens, 200_000);
    }

    #[test]
    fn only_the_user_instruction_may_authorise_tools() {
        assert!(ContentOrigin::UserInstruction.may_authorise_tools());
        for origin in [
            ContentOrigin::Selection,
            ContentOrigin::FieldPrefix,
            ContentOrigin::FieldSuffix,
            ContentOrigin::Clipboard,
            ContentOrigin::File,
            ContentOrigin::ToolResult,
            ContentOrigin::McpResult,
        ] {
            assert!(
                !origin.may_authorise_tools(),
                "{origin:?} must be data, not a request"
            );
        }
    }
}
