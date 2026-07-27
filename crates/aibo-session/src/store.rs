//! §12 persistence, behind a trait.
//!
//! The engine writes history through [`SessionStore`] rather than against
//! `aibo_store::Db` directly, for two reasons:
//!
//! * the orchestration tests must run with no SQLite, no SQLCipher key and no
//!   keychain prompt — [`NoStore`] is what makes that possible;
//! * "the database could not be opened" must degrade to *aibo works, history
//!   does not*, never to *aibo does not start*. §12 designs paths for locked,
//!   corrupt and half-migrated databases; a hotkey tool that refuses to answer
//!   because it cannot write a log line is worse than one that forgets.

use aibo_core::cost::Micros;
use aibo_core::error::Result;
use aibo_core::types::{MessageRole, ProviderId, Surface, Usage};
use async_trait::async_trait;
use uuid::Uuid;

/// One completed request/response pair, ready to be written to §12's
/// `conversations` + `messages` tables.
#[derive(Debug, Clone)]
pub struct Exchange {
    /// Append to this conversation, or start a new one when `None`.
    pub conversation_id: Option<Uuid>,
    /// Which surface produced it (§1).
    pub surface: Surface,
    /// Bundle id / executable name of the app that had focus.
    pub source_app: Option<String>,
    /// The user's own typed instruction, verbatim.
    pub instruction: Option<String>,
    /// The assistant's text, after the §5 anti-preamble filter.
    pub assistant: String,
    /// Provider that served it.
    pub provider: ProviderId,
    /// Wire model id.
    pub model: String,
    /// Reported token accounting; all zeroes when the provider sent none.
    pub usage: Usage,
    /// Reconciled cost, `None` when the model is unpriced (§14 — "cost unknown
    /// for N requests" is honest, "0.00" is not).
    pub cost_micros: Option<Micros>,
    /// Wall clock from dispatch to the last event.
    pub latency_ms: u64,
    /// The stream ended early. §13 keeps the text — the user may still want to
    /// copy it — but it was never auto-inserted.
    pub truncated: bool,
}

/// Where completed exchanges go (§12).
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Write one exchange. Returns the conversation it landed in, or `None`
    /// when this store does not persist.
    async fn record(&self, exchange: Exchange) -> Result<Option<Uuid>>;
}

/// The null store: history is disabled, or the database could not be opened.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoStore;

#[async_trait]
impl SessionStore for NoStore {
    async fn record(&self, _exchange: Exchange) -> Result<Option<Uuid>> {
        Ok(None)
    }
}

/// [`SessionStore`] over the real §12 encrypted database.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    db: aibo_store::Db,
}

impl SqliteStore {
    /// Wrap an already-opened database.
    pub fn new(db: aibo_store::Db) -> Self {
        Self { db }
    }
}

/// A store failure, kept out of [`aibo_core::AiboError`]'s enumerated variants
/// because §13 has no *"history write failed"* treatment — it is an
/// [`aibo_core::AiboError::Internal`], which renders generically with a "copy
/// diagnostics" button.
#[derive(Debug, thiserror::Error)]
#[error("could not persist the conversation")]
pub struct PersistFailed(#[source] pub aibo_store::StoreError);

#[async_trait]
impl SessionStore for SqliteStore {
    async fn record(&self, exchange: Exchange) -> Result<Option<Uuid>> {
        // Every SQLite call goes through `Db::call`, which is `spawn_blocking`
        // — §6 forbids SQLite on the UI thread and this is reached from a
        // tokio worker either way.
        let id = self
            .db
            .call(move |conn| {
                aibo_store::history::insert_exchange(
                    conn,
                    &aibo_store::history::NewExchange {
                        conversation_id: exchange.conversation_id,
                        surface: exchange.surface,
                        source_app: exchange.source_app,
                        instruction: exchange.instruction,
                        assistant: aibo_store::history::NewMessage {
                            role: MessageRole::Assistant,
                            content: exchange.assistant,
                            provider: Some(exchange.provider.as_str().to_owned()),
                            model: Some(exchange.model),
                            usage_in: i64::try_from(exchange.usage.input_tokens).ok(),
                            usage_out: i64::try_from(exchange.usage.output_tokens).ok(),
                            cost_micros: exchange.cost_micros.and_then(|c| i64::try_from(c).ok()),
                            latency_ms: i64::try_from(exchange.latency_ms).ok(),
                        },
                    },
                )
            })
            .await
            .map_err(|e| aibo_core::AiboError::Internal(Box::new(PersistFailed(e))))?;

        Ok(Some(id))
    }
}
