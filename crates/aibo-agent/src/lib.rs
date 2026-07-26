//! `aibo-agent` — delegates and the native loop behind one interface (§7).
//!
//! `AgentStep` is designed against `codex app-server`'s JSON-RPC events and
//! approval requests; `native_loop` conforms to that, not the other way round.
//!
//! Two rules that shape everything here:
//! - Limits are mandatory, not advisory (§14). Exceeding one stops the run with
//!   `AiboError::BudgetExceeded` and a "continue anyway" affordance.
//! - Approval happens **before** the write (§11). By the time there is a diff,
//!   the side effects have already happened, so a post-hoc reject means nothing.

#![forbid(unsafe_code)]

pub mod claude_code;
pub mod codex_app_server;
pub mod limits;
pub mod native_loop;
pub mod permission_gate;

pub use claude_code::{ClaudeCodeCli, ClaudeCodeConfig, ClaudeCodeError};
pub use codex_app_server::{CodexAppServer, CodexConfig, CodexError};
pub use limits::LimitTracker;
pub use native_loop::{
    NativeLoop, NativeLoopConfig, NoTools, ToolExecutor, ToolIntent, ToolInvocation, ToolOutput,
};
pub use permission_gate::{
    ApprovalUi, Authorisation, DenyReason, GatedCall, PermissionGate, TierTable,
};
