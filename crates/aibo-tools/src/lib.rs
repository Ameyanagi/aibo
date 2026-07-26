//! `aibo-tools` — the five permission tiers of execution (§11).
//!
//! Tier 0 [`builtin`] and [`compute`] are pure and need no consent. Tier 1
//! [`js`] (rquickjs) and [`wasm`] (wasmtime) are sandboxed — note the two
//! sandboxes are not equivalent: rquickjs offers a memory limit, a stack limit
//! and a cooperative wall-clock interrupt, but **no fuel metering and no epoch
//! interruption**; those are wasmtime concepts. Tier 2 [`mcp`], tier 3
//! [`shell`]/fs. Tier 4 (delegate) lives in `aibo-agent`, not here.
//!
//! Tool *results* are untrusted input, exactly like a selection (§5, §11):
//! every [`ToolOutput`] reports [`aibo_core::types::ContentOrigin::ToolResult`]
//! from [`ToolOutput::origin`], and that origin can never authorise another
//! tool call.
//!
//! # What this crate is not
//!
//! §11's honest summary: the tiers are a UX pattern, not a security boundary.
//! Everything in [`shell`] is defence in depth — the real boundary is a
//! sandbox (Codex's, or the OS's). Nothing here should be described to a user
//! as containment.

#![forbid(unsafe_code)]

pub mod builtin;
pub mod compute;
pub mod mcp;
pub mod shell;
pub mod wasm;

use std::collections::BTreeMap;
use std::sync::Arc;

use aibo_core::error::{AiboError, SandboxFailure};
use aibo_core::types::{ContentOrigin, ToolSchema, ToolTier};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// The result type used throughout `aibo-tools`.
pub type ToolResult<T> = std::result::Result<T, ToolError>;

/// Why a tool call was refused before it ran (§11 threat model).
///
/// Every variant is a *fail-closed* decision. The rule for adding one: if a
/// check cannot prove the operation is inside the approved envelope, it denies.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DenyReason {
    /// The path lies outside every directory the user added.
    #[error("path `{path}` is outside the allowed roots")]
    OutsideScope {
        /// The offending path, as resolved.
        path: String,
    },

    /// The path resolved through a symlink or junction that leaves the scope.
    ///
    /// Containment is re-checked **after** resolution, never before (§11) —
    /// this variant exists because checking before is the classic bug.
    #[error("path `{requested}` resolves to `{resolved}`, outside the allowed roots")]
    SymlinkEscape {
        /// What the caller asked for.
        requested: String,
        /// What it actually pointed at.
        resolved: String,
    },

    /// The command class requires the user to type it out to confirm.
    #[error("`{command}` needs typed confirmation: {why}")]
    NeedsTypedConfirmation {
        /// The command as approved.
        command: String,
        /// The class that triggered it, e.g. "recursive delete".
        why: &'static str,
    },

    /// No approval record covers this call.
    #[error("no approval on record for `{what}`")]
    NotApproved {
        /// Command or tool name.
        what: String,
    },

    /// The world changed between approval and execution (§11 TOCTOU row).
    #[error("approval no longer matches the request: {detail}")]
    Stale {
        /// What differs now.
        detail: String,
    },

    /// An MCP server or one of its tools is denied by remembered consent.
    #[error("MCP tool `{server}/{tool}` is denied")]
    McpDenied {
        /// Server id.
        server: String,
        /// Tool name.
        tool: String,
    },
}

/// Everything a tool call can fail with.
///
/// Libraries use `thiserror`; the binary is the only place `anyhow` appears.
/// The `From<ToolError>` impl for [`AiboError`] maps into the failure model of
/// §13 — sandbox stops keep their structure, everything else becomes
/// `Internal` and is never rendered raw.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolError {
    /// No tool is registered under that name.
    #[error("unknown tool `{0}`")]
    UnknownTool(String),

    /// Arguments did not match the tool's schema.
    #[error("invalid arguments for `{tool}`: {reason}")]
    InvalidArguments {
        /// Tool name.
        tool: String,
        /// Human-readable reason; fine in diagnostics, not in the panel.
        reason: String,
    },

    /// The tool ran and reported failure.
    #[error("tool `{tool}` failed: {message}")]
    Failed {
        /// Tool name.
        tool: String,
        /// Message from the tool.
        message: String,
    },

    /// Sandboxed execution stopped (§11 tier 1).
    #[error("sandbox (tier {tier}) {reason}")]
    Sandbox {
        /// Permission tier.
        tier: u8,
        /// Why.
        reason: SandboxFailure,
    },

    /// A permission or containment check refused the call.
    #[error("denied: {0}")]
    Denied(#[from] DenyReason),

    /// The caller's [`CancellationToken`] fired.
    #[error("cancelled")]
    Cancelled,

    /// Underlying I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<ToolError> for AiboError {
    fn from(e: ToolError) -> Self {
        match e {
            ToolError::Sandbox { tier, reason } => AiboError::Sandbox { tier, reason },
            other => AiboError::Internal(Box::new(other)),
        }
    }
}

/// What a tool produced.
///
/// The text is **untrusted input**. It is fenced and labelled by §5's prompt
/// assembly, and [`ToolOutput::origin`] is fixed at
/// [`ContentOrigin::ToolResult`] so the permission gate can prove a tool result
/// never authorised the next tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// Human/model-readable payload.
    pub text: String,
    /// Machine-readable payload when the tool has one.
    pub structured: Option<serde_json::Value>,
    /// The tool ran but reported a domain error (not a transport failure).
    pub is_error: bool,
}

impl ToolOutput {
    /// A successful text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            structured: None,
            is_error: false,
        }
    }

    /// A successful result with both a rendering and a machine payload.
    pub fn json(text: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            text: text.into(),
            structured: Some(value),
            is_error: false,
        }
    }

    /// A domain error the model is expected to read and react to.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            structured: None,
            is_error: true,
        }
    }

    /// Always [`ContentOrigin::ToolResult`] (§5 rule 2).
    pub const fn origin(&self) -> ContentOrigin {
        ContentOrigin::ToolResult
    }
}

/// One callable tool.
///
/// Implementations are shared behind an [`Arc`]; the registry stores them as
/// trait objects. `call` must be cancellation-aware: when `cancel` fires it
/// returns [`ToolError::Cancelled`] promptly, and any blocking work belongs on
/// a blocking thread rather than the runtime (§13).
#[async_trait]
pub trait Tool: Send + Sync {
    /// The schema advertised to the model (§5 "Do").
    fn schema(&self) -> ToolSchema;

    /// Which permission tier this tool sits at (§11). The UI picks the
    /// approval affordance from this.
    fn tier(&self) -> ToolTier;

    /// Run the tool.
    async fn call(
        &self,
        args: serde_json::Value,
        cancel: CancellationToken,
    ) -> ToolResult<ToolOutput>;
}

/// The set of tools offered for one run.
///
/// Ordered by name so the schema list handed to a provider is stable — an
/// unstable tool order defeats prompt caching (§14).
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry preloaded with every tier-0 tool (§11: no consent needed).
    pub fn with_builtins() -> Self {
        let mut this = Self::new();
        for tool in builtin::all() {
            this.register(tool);
        }
        this
    }

    /// Add a tool, replacing any tool of the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.schema().name, tool);
    }

    /// Look a tool up by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Every schema, in stable order.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Dispatch a call by name.
    ///
    /// The registry does **not** consult consent: approval is the permission
    /// gate's job in `aibo-agent`, and duplicating it here would give two
    /// places to forget it.
    pub async fn call(
        &self,
        name: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
    ) -> ToolResult<ToolOutput> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::UnknownTool(name.to_owned()))?;
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        tool.call(args, cancel).await
    }
}

/// Argument extraction helpers shared by the tool implementations.
pub(crate) mod args {
    use super::{ToolError, ToolResult};

    pub(crate) fn str_arg<'a>(
        args: &'a serde_json::Value,
        tool: &str,
        key: &str,
    ) -> ToolResult<&'a str> {
        args.get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments {
                tool: tool.to_owned(),
                reason: format!("`{key}` must be a string"),
            })
    }

    pub(crate) fn opt_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
        args.get(key).and_then(|v| v.as_str())
    }

    pub(crate) fn opt_bool(args: &serde_json::Value, key: &str) -> Option<bool> {
        args.get(key).and_then(|v| v.as_bool())
    }

    pub(crate) fn invalid(tool: &str, reason: impl Into<String>) -> ToolError {
        ToolError::InvalidArguments {
            tool: tool.to_owned(),
            reason: reason.into(),
        }
    }

    pub(crate) fn failed(tool: &str, message: impl Into<String>) -> ToolError {
        ToolError::Failed {
            tool: tool.to_owned(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_origin_is_always_untrusted() {
        let out = ToolOutput::text("anything");
        assert_eq!(out.origin(), ContentOrigin::ToolResult);
        assert!(!out.origin().may_authorise_tools());
    }

    #[tokio::test]
    async fn unknown_tool_is_an_error_not_a_panic() {
        let reg = ToolRegistry::new();
        let err = reg
            .call("nope", serde_json::json!({}), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::UnknownTool(n) if n == "nope"));
    }

    #[tokio::test]
    async fn a_cancelled_token_short_circuits_dispatch() {
        let reg = ToolRegistry::with_builtins();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = reg
            .call("hash", serde_json::json!({"text": "x"}), cancel)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[test]
    fn builtin_registry_is_stable_and_all_tier_zero() {
        let reg = ToolRegistry::with_builtins();
        let names: Vec<String> = reg.schemas().into_iter().map(|s| s.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "schemas must be in stable order");
        assert!(reg.schemas().iter().all(|s| s.tier == 0));
    }
}
