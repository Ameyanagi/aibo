//! `aibo-core` — the domain layer.
//!
//! This crate holds the vocabulary ([`types`]), the contracts ([`traits`]), the
//! failure model ([`error`]), the router, prompt assembly and the context and
//! cost budgets. Per §6 of the plan it compiles with **no platform and no
//! network dependencies**, which is what makes the router, prompt assembly and
//! the budget exhaustively unit-testable. That boundary is worth defending:
//! if something here needs `reqwest`, `objc2`, `windows` or `rusqlite`, it
//! belongs in another crate.
//!
//! Section references in doc comments (§n) point at `docs/plan.md`.

#![forbid(unsafe_code)]

pub mod context;
pub mod cost;
pub mod error;
pub mod license;
pub mod prompts;
pub mod roles;
pub mod router;
pub mod traits;
pub mod types;

pub use error::{AiboError, Result, Treatment};
pub use traits::{AgentBackend, PlatformBackend, Provider};
pub use types::{
    AgentFeatures, AgentLimits, AgentStep, AgentTask, AppInfo, AppRef, BoxStream, Capabilities,
    ChatRequest, ClipboardItem, Credential, DisplayInfo, FieldContext, Health, InsertMode,
    ModelInfo, Permission, PermissionStatus, ProviderId, Role, RouteInput, StopReason, StreamEvent,
    Surface, Usage, Verb,
};
