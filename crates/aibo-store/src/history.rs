//! Conversations, messages and history export (§12).
//!
//! Ids are uuid v7 so rows sort by creation time without a second index, which
//! is why the schema says "uuid v7, time-sortable".
//!
//! **Export is not a nice-to-have.** §12: "a privacy-positioned local-only tool
//! needs a history export (JSON + markdown). It is also the honest answer to
//! machine transfer and to 'what if I stop paying'. Cheap to build; conspicuous
//! by its absence."

use aibo_core::types::{MessageRole, Surface};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::codec;
use crate::error::{Result, StoreError};
use crate::now_unix;

/// A conversation row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    /// uuid v7.
    pub id: Uuid,
    /// Which surface produced it (§1).
    pub surface: Surface,
    /// Bundle id / executable name of the app that was focused.
    pub source_app: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds; bumped by every message.
    pub updated_at: i64,
    /// Optional human title.
    pub title: Option<String>,
}

/// A message about to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessage {
    /// Author.
    pub role: MessageRole,
    /// Flattened body. Multi-part messages are flattened by the caller —
    /// §12's `content` is one TEXT column, and the FTS index is over it.
    pub content: String,
    /// Provider that produced it, for assistant messages.
    pub provider: Option<String>,
    /// Model id.
    pub model: Option<String>,
    /// Prompt tokens.
    pub usage_in: Option<i64>,
    /// Completion tokens.
    pub usage_out: Option<i64>,
    /// Cost in millionths of a unit of currency (§14 spend meter).
    pub cost_micros: Option<i64>,
    /// Wall-clock latency.
    pub latency_ms: Option<i64>,
}

impl Default for NewMessage {
    /// A user message with no body. `aibo-core`'s `MessageRole` has no
    /// `Default` — deliberately, since "which role" is never a safe guess — so
    /// the default is spelled out here for struct-update syntax.
    fn default() -> Self {
        Self {
            role: MessageRole::User,
            content: String::new(),
            provider: None,
            model: None,
            usage_in: None,
            usage_out: None,
            cost_micros: None,
            latency_ms: None,
        }
    }
}

/// A message row as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMessage {
    /// uuid v7.
    pub id: Uuid,
    /// Owning conversation.
    pub conv_id: Uuid,
    /// Author.
    pub role: MessageRole,
    /// Body.
    pub content: String,
    /// Provider that produced it.
    pub provider: Option<String>,
    /// Model id.
    pub model: Option<String>,
    /// Prompt tokens.
    pub usage_in: Option<i64>,
    /// Completion tokens.
    pub usage_out: Option<i64>,
    /// Cost in micros (§14).
    pub cost_micros: Option<i64>,
    /// Wall-clock latency.
    pub latency_ms: Option<i64>,
    /// Unix seconds.
    pub created_at: i64,
}

/// Start a conversation and return its id.
pub fn create_conversation(
    conn: &Connection,
    surface: Surface,
    source_app: Option<&str>,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    let now = now_unix();
    conn.execute(
        "INSERT INTO conversations (id, surface, source_app, created_at, updated_at, title)
         VALUES (?1, ?2, ?3, ?4, ?4, NULL)",
        params![id, codec::surface_to_str(surface), source_app, now],
    )?;
    Ok(id)
}

/// Append a message and bump the conversation's `updated_at`.
///
/// Both statements run in one transaction: a message whose conversation still
/// claims to be untouched sorts wrongly in the history list forever.
pub fn insert_message(conn: &mut Connection, conv_id: Uuid, msg: &NewMessage) -> Result<Uuid> {
    let id = Uuid::now_v7();
    let now = now_unix();
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO messages
           (id, conv_id, role, content, provider, model,
            usage_in, usage_out, cost_micros, latency_ms, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            id,
            conv_id,
            codec::message_role_to_str(msg.role),
            msg.content,
            msg.provider,
            msg.model,
            msg.usage_in,
            msg.usage_out,
            msg.cost_micros,
            msg.latency_ms,
            now,
        ],
    )?;
    tx.execute(
        "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
        params![now, conv_id],
    )?;
    tx.commit()?;
    Ok(id)
}

/// Set (or clear) a conversation title.
pub fn set_title(conn: &Connection, conv_id: Uuid, title: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE conversations SET title = ?1 WHERE id = ?2",
        params![title, conv_id],
    )?;
    Ok(())
}

/// Fetch one conversation.
pub fn get_conversation(conn: &Connection, id: Uuid) -> Result<Option<Conversation>> {
    let row = conn
        .query_row(
            "SELECT id, surface, source_app, created_at, updated_at, title
             FROM conversations WHERE id = ?1",
            params![id],
            read_conversation,
        )
        .optional()?;
    row.transpose()
}

/// Most recently updated conversations first — the order `idx_conv_updated`
/// exists to serve.
pub fn recent_conversations(conn: &Connection, limit: usize) -> Result<Vec<Conversation>> {
    let mut stmt = conn.prepare(
        "SELECT id, surface, source_app, created_at, updated_at, title
         FROM conversations ORDER BY updated_at DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], read_conversation)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// Every message in a conversation, oldest first.
pub fn messages(conn: &Connection, conv_id: Uuid) -> Result<Vec<StoredMessage>> {
    let mut stmt = conn.prepare(
        "SELECT id, conv_id, role, content, provider, model,
                usage_in, usage_out, cost_micros, latency_ms, created_at
         FROM messages WHERE conv_id = ?1 ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map(params![conv_id], read_message)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// Delete a conversation. Messages go with it via `ON DELETE CASCADE`, which is
/// why `PRAGMA foreign_keys=ON` is not optional (§12).
pub fn delete_conversation(conn: &Connection, conv_id: Uuid) -> Result<usize> {
    Ok(conn.execute("DELETE FROM conversations WHERE id = ?1", params![conv_id])?)
}

/// Delete all history. The "purge" the settings pane offers.
pub fn purge_all(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM conversations", [])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Which conversations an export covers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportFilter {
    /// Only this conversation.
    pub conversation: Option<Uuid>,
    /// Only conversations updated at or after this unix second.
    pub since: Option<i64>,
    /// Only conversations updated strictly before this unix second.
    pub until: Option<i64>,
}

/// A conversation with its messages, as it appears in a JSON export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedConversation {
    /// The conversation.
    #[serde(flatten)]
    pub conversation: Conversation,
    /// Its messages, oldest first.
    pub messages: Vec<StoredMessage>,
}

/// A whole export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Export {
    /// Format version, so an importer can tell what it is holding.
    pub format: u32,
    /// Unix seconds the export was taken.
    pub exported_at: i64,
    /// Schema version the data came from.
    pub schema_version: i64,
    /// The conversations.
    pub conversations: Vec<ExportedConversation>,
}

/// Export format version. Bump when the shape changes incompatibly.
pub const EXPORT_FORMAT: u32 = 1;

/// Gather the conversations and messages an export covers.
pub fn collect_export(conn: &Connection, filter: &ExportFilter) -> Result<Export> {
    let mut sql = String::from(
        "SELECT id, surface, source_app, created_at, updated_at, title FROM conversations WHERE 1=1",
    );
    if filter.conversation.is_some() {
        sql.push_str(" AND id = :conv");
    }
    if filter.since.is_some() {
        sql.push_str(" AND updated_at >= :since");
    }
    if filter.until.is_some() {
        sql.push_str(" AND updated_at < :until");
    }
    sql.push_str(" ORDER BY created_at");

    let mut stmt = conn.prepare(&sql)?;
    let mut named: Vec<(&str, &dyn rusqlite::ToSql)> = Vec::new();
    if let Some(id) = filter.conversation.as_ref() {
        named.push((":conv", id));
    }
    if let Some(since) = filter.since.as_ref() {
        named.push((":since", since));
    }
    if let Some(until) = filter.until.as_ref() {
        named.push((":until", until));
    }
    let rows = stmt.query_map(named.as_slice(), read_conversation)?;

    let mut conversations = Vec::new();
    for row in rows {
        let conversation = row??;
        let messages = messages(conn, conversation.id)?;
        conversations.push(ExportedConversation {
            conversation,
            messages,
        });
    }

    Ok(Export {
        format: EXPORT_FORMAT,
        exported_at: now_unix(),
        schema_version: crate::migrations::current_version(conn)?,
        conversations,
    })
}

/// Export history as pretty-printed JSON — the machine-transfer format.
pub fn export_json(conn: &Connection, filter: &ExportFilter) -> Result<String> {
    Ok(serde_json::to_string_pretty(&collect_export(
        conn, filter,
    )?)?)
}

/// Export history as Markdown — the human-readable format.
///
/// Message bodies are fenced when they contain a line that would otherwise be
/// read as Markdown structure; the point of the export is that what the user
/// typed survives, not that it renders prettily.
pub fn export_markdown(conn: &Connection, filter: &ExportFilter) -> Result<String> {
    let export = collect_export(conn, filter)?;
    let mut out = String::new();
    out.push_str("# aibo history\n\n");
    out.push_str(&format!(
        "Exported {} · {} conversation(s) · schema v{}\n\n",
        iso8601(conn, export.exported_at)?,
        export.conversations.len(),
        export.schema_version
    ));

    for entry in &export.conversations {
        let conv = &entry.conversation;
        let title = conv.title.clone().unwrap_or_else(|| "Untitled".to_owned());
        out.push_str(&format!("## {title}\n\n"));
        out.push_str(&format!(
            "- Surface: `{}`\n",
            codec::surface_to_str(conv.surface)
        ));
        if let Some(app) = &conv.source_app {
            out.push_str(&format!("- App: `{app}`\n"));
        }
        out.push_str(&format!(
            "- Started: {}\n\n",
            iso8601(conn, conv.created_at)?
        ));

        for msg in &entry.messages {
            let role = codec::message_role_to_str(msg.role);
            let model = match (&msg.provider, &msg.model) {
                (Some(p), Some(m)) => format!(" · {p}/{m}"),
                (Some(p), None) => format!(" · {p}"),
                _ => String::new(),
            };
            out.push_str(&format!(
                "### {role}{model} · {}\n\n",
                iso8601(conn, msg.created_at)?
            ));
            out.push_str(&fence_if_needed(&msg.content));
            out.push_str("\n\n");
        }
    }
    Ok(out)
}

/// Format a unix second as ISO-8601 UTC.
///
/// SQLite does the calendar arithmetic. `aibo-store` has no date library and
/// does not need one for this.
fn iso8601(conn: &Connection, unix_seconds: i64) -> Result<String> {
    Ok(conn.query_row(
        "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ?1, 'unixepoch')",
        params![unix_seconds],
        |r| r.get(0),
    )?)
}

/// Wrap a body in a fence when it would otherwise be read as Markdown.
fn fence_if_needed(content: &str) -> String {
    let structural = content
        .lines()
        .any(|line| line.starts_with('#') || line.starts_with("```") || line.starts_with("---"));
    if structural {
        // Longer fence than anything inside, so nested fences cannot close it.
        let longest = content
            .lines()
            .filter(|l| l.starts_with("```"))
            .map(|l| l.chars().take_while(|c| *c == '`').count())
            .max()
            .unwrap_or(2);
        let fence = "`".repeat(longest.max(3) + 1);
        format!("{fence}\n{content}\n{fence}")
    } else {
        content.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Row mapping
// ---------------------------------------------------------------------------

/// Row mappers return `Result<Result<T>>`: the outer is `rusqlite`'s, the inner
/// carries the enum-parse failure, which `rusqlite::Error` cannot represent
/// without losing which column was wrong.
type Mapped<T> = rusqlite::Result<Result<T>>;

fn read_conversation(row: &rusqlite::Row<'_>) -> Mapped<Conversation> {
    let surface: String = row.get(1)?;
    Ok((|| {
        Ok(Conversation {
            id: row_uuid(row, 0)?,
            surface: codec::surface_from_str(&surface)?,
            source_app: row.get(2)?,
            created_at: row.get(3)?,
            updated_at: row.get(4)?,
            title: row.get(5)?,
        })
    })())
}

fn read_message(row: &rusqlite::Row<'_>) -> Mapped<StoredMessage> {
    let role: String = row.get(2)?;
    Ok((|| {
        Ok(StoredMessage {
            id: row_uuid(row, 0)?,
            conv_id: row_uuid(row, 1)?,
            role: codec::message_role_from_str(&role)?,
            content: row.get(3)?,
            provider: row.get(4)?,
            model: row.get(5)?,
            usage_in: row.get(6)?,
            usage_out: row.get(7)?,
            cost_micros: row.get(8)?,
            latency_ms: row.get(9)?,
            created_at: row.get(10)?,
        })
    })())
}

fn row_uuid(row: &rusqlite::Row<'_>, idx: usize) -> Result<Uuid> {
    row.get(idx).map_err(StoreError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn seeded() -> (Db, Uuid) {
        let db = Db::open_in_memory().expect("open");
        let conv = db
            .with_conn(|c| create_conversation(c, Surface::Ask, Some("com.apple.Safari")))
            .expect("conversation");
        db.with_conn(|c| {
            insert_message(
                c,
                conv,
                &NewMessage {
                    role: MessageRole::User,
                    content: "how do I open a panel".into(),
                    ..Default::default()
                },
            )
        })
        .expect("user message");
        db.with_conn(|c| {
            insert_message(
                c,
                conv,
                &NewMessage {
                    role: MessageRole::Assistant,
                    content: "press the hotkey".into(),
                    provider: Some("cerebras".into()),
                    model: Some("llama-3.3-70b".into()),
                    usage_in: Some(12),
                    usage_out: Some(4),
                    cost_micros: Some(37),
                    latency_ms: Some(180),
                },
            )
        })
        .expect("assistant message");
        (db, conv)
    }

    #[test]
    fn messages_round_trip() {
        let (db, conv) = seeded();
        let msgs = db.with_conn(|c| messages(c, conv)).expect("messages");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[1].cost_micros, Some(37));
    }

    #[test]
    fn inserting_a_message_bumps_the_conversation() {
        let (db, conv) = seeded();
        let c = db
            .with_conn(|c| get_conversation(c, conv))
            .expect("get")
            .expect("present");
        assert!(c.updated_at >= c.created_at);
        assert_eq!(c.surface, Surface::Ask);
    }

    #[test]
    fn deleting_a_conversation_cascades_to_its_messages() {
        let (db, conv) = seeded();
        db.with_conn(|c| delete_conversation(c, conv))
            .expect("delete");
        let remaining: i64 = db
            .with_conn(|c| Ok(c.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?))
            .expect("count");
        assert_eq!(remaining, 0, "ON DELETE CASCADE needs foreign_keys=ON");
    }

    #[test]
    fn json_export_contains_every_message() {
        let (db, _) = seeded();
        let json = db
            .with_conn(|c| export_json(c, &ExportFilter::default()))
            .expect("export");
        let parsed: Export = serde_json::from_str(&json).expect("reparse");
        assert_eq!(parsed.format, EXPORT_FORMAT);
        assert_eq!(parsed.conversations.len(), 1);
        assert_eq!(parsed.conversations[0].messages.len(), 2);
        assert!(json.contains("press the hotkey"));
    }

    #[test]
    fn markdown_export_is_readable_and_dated() {
        let (db, _) = seeded();
        let md = db
            .with_conn(|c| export_markdown(c, &ExportFilter::default()))
            .expect("export");
        assert!(md.starts_with("# aibo history"));
        assert!(md.contains("### user"));
        assert!(md.contains("### assistant · cerebras/llama-3.3-70b"));
        assert!(md.contains("how do I open a panel"));
        // ISO-8601, not a raw epoch.
        assert!(md.contains("T") && md.contains("Z"));
    }

    #[test]
    fn markdown_export_fences_bodies_that_look_like_markdown() {
        let db = Db::open_in_memory().expect("open");
        let conv = db
            .with_conn(|c| create_conversation(c, Surface::Transform, None))
            .expect("conversation");
        db.with_conn(|c| {
            insert_message(
                c,
                conv,
                &NewMessage {
                    role: MessageRole::User,
                    content: "# not a heading\n```\nfn main() {}\n```".into(),
                    ..Default::default()
                },
            )
        })
        .expect("message");

        let md = db
            .with_conn(|c| export_markdown(c, &ExportFilter::default()))
            .expect("export");
        assert!(md.contains("````\n# not a heading"));
    }

    #[test]
    fn export_filter_narrows_to_one_conversation() {
        let (db, conv) = seeded();
        db.with_conn(|c| create_conversation(c, Surface::Do, None))
            .expect("second conversation");
        let export = db
            .with_conn(|c| {
                collect_export(
                    c,
                    &ExportFilter {
                        conversation: Some(conv),
                        ..Default::default()
                    },
                )
            })
            .expect("export");
        assert_eq!(export.conversations.len(), 1);
        assert_eq!(export.conversations[0].conversation.id, conv);
    }
}
