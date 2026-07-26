//! `aibo-session` — the orchestration layer.
//!
//! Every type this crate needs already existed; what did not exist was the
//! state machine that connects them. `src/main.rs` routed a request and then
//! said:
//!
//! ```text
//! TODO(P1): assemble the prompt, apply the context budget, resolve the role
//!           chain to a provider, stream
//! ```
//!
//! This crate is that sentence, made real:
//!
//! ```text
//! capture (§8) → surface inference (§1) → route (§4)
//!   → assemble prompt + context budget (§5)
//!   → resolve the role chain to a provider (§4)
//!   → reserve the estimated cost (§14)
//!   → stream (§7)
//!   → reconcile usage (§14)
//!   → persist (§12)
//!   → SessionEvents to the UI
//! ```
//!
//! # The five rules with teeth
//!
//! | Rule | Where |
//! |---|---|
//! | §4 fallback across a chain; a **400 does not** trigger it | [`engine`] |
//! | §13 per-provider offline state with hysteresis, never a global boolean | [`health`] |
//! | §13 cancellation threaded end to end; a new submission cancels the previous | [`Engine::run`] |
//! | §13 a partial stream is **never** auto-inserted | [`Outcome::insertable_text`] |
//! | §14 reserve at dispatch, reconcile after; mandatory [`aibo_core::types::AgentLimits`] | [`engine`], [`agent`] |
//!
//! # Shape
//!
//! [`Engine`] is built once and shared behind an `Arc`. It owns the provider
//! registry, the §4 rule list, the §13 health table, the §14 spend meter and
//! the cancellation registry. It borrows nothing from the UI: events go out
//! over an [`EventSink`], and the binary translates them into `aibo_ui::UiEvent`.
//!
//! ```no_run
//! # async fn example() {
//! use aibo_session::{Engine, EngineConfig, EventSink, Submission};
//! use aibo_provider::ProviderRegistry;
//! use uuid::Uuid;
//!
//! let engine = Engine::new(ProviderRegistry::new(), EngineConfig::default());
//! let (events, mut rx) = EventSink::channel();
//!
//! let outcome = engine
//!     .run(Submission::new(Uuid::now_v7(), "summarise this"), &events)
//!     .await;
//!
//! // §13: only a completed stream may ever be written into someone else's app.
//! if let Some(text) = outcome.insertable_text() {
//!     println!("{text}");
//! }
//! while let Ok(event) = rx.try_recv() {
//!     println!("{event:?}");
//! }
//! # }
//! ```
//!
//! # Testing
//!
//! Nothing here touches the network, the OS or a database on its own: the
//! provider is a `dyn Provider`, the store is a [`store::SessionStore`] whose
//! default is [`store::NoStore`], and the health machine takes an explicit
//! `Instant`. The integration tests in `tests/` drive the whole path with a
//! scripted mock provider (§18).

#![forbid(unsafe_code)]

pub mod agent;
pub mod config;
mod dispatch;
pub mod engine;
pub mod event;
pub mod filter;
pub mod health;
pub mod store;
pub mod trust;
pub mod verb;

pub use agent::{AgentEvent, AgentSink};
pub use config::{Config, ConfigError, CredentialSource, EnvCredentials, NoCredentials};
pub use engine::{Engine, EngineConfig};
pub use event::{
    Capture, Completion, EventSink, Outcome, PartialReason, SessionEvent, SkipReason, Submission,
    char_len,
};
pub use health::{HealthTable, HysteresisPolicy, Usability};
pub use store::{Exchange, NoStore, SessionStore, SqliteStore};
pub use trust::{TrustBoundary, TrustMap};
