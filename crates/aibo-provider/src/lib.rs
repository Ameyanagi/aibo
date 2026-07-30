//! `aibo-provider` — one [`Provider`] implementation per backend (§10).
//!
//! "One OpenAI-compatible module covers seven providers" was over-optimistic.
//! `openai_compat` is a real saving, but each backend still differs in wire
//! format (Responses vs Chat Completions), URL shape, SSE framing, where
//! `usage` appears, tool-call encoding, error body shape, reasoning-token
//! handling and model catalogue. Budget each one at 1–3 days of quirk-hunting
//! plus a golden-fixture set.
//!
//! Implementation order per §10: `openai_compat`, then `anthropic`, then
//! `bedrock` (SigV4 is the fiddly one), then `vertex`.
//!
//! [`Provider`]: aibo_core::traits::Provider

#![forbid(unsafe_code)]

pub mod anthropic;
pub mod attachment;
pub mod auth;
pub mod azure;
pub mod bedrock;
pub mod codex;
pub mod gemini;
pub mod http;
pub mod ollama;
pub mod openai_compat;
pub mod registry;
pub mod sse;
pub mod vertex;
pub mod wire;

pub use auth::{
    AuthStyle, RefreshPolicy, RefreshingTokenProvider, StoredTokens, TokenRefresh, TokenSet,
    TokenStore,
};
pub use http::HttpConfig;
pub use openai_compat::{OpenAiCompat, Quirks, UrlStyle, UsagePlacement, WireFormat};
pub use registry::{ModelCatalogue, ProviderKind, ProviderRegistry, ProviderSpec, Resolution};
