//! FTS5 trigram search over messages (§12).
//!
//! §12 picks `tokenize='trigram'` because it "is the only built-in tokenizer
//! that works for CJK without a custom segmenter. Costs index size; correct for
//! a Japanese-using author."
//!
//! Trigram has one consequence the UI has to know about: **a query shorter than
//! three characters cannot be answered by the index at all**. Two-character
//! Japanese words are extremely common, so returning nothing would look like a
//! bug. Queries under [`MIN_TRIGRAM_LEN`] therefore fall back to a `LIKE` scan,
//! which is fine at a single user's data volume and is honest about being
//! unranked.
//!
//! The index is *external content* — the row data lives in `messages`, and the
//! three triggers in [`crate::migrations`] are the only thing keeping the two in
//! step. [`index_is_consistent`] and [`rebuild_index`] exist so that can be
//! checked and repaired rather than assumed.

use aibo_core::types::{MessageRole, Surface};
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::codec;
use crate::error::{Result, StoreError};

/// Shortest query the trigram index can serve.
pub const MIN_TRIGRAM_LEN: usize = 3;

/// How a hit was found — the UI shows unranked results differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HitSource {
    /// Matched through the FTS5 trigram index, ranked by bm25.
    Index,
    /// Matched by substring scan because the query was under
    /// [`MIN_TRIGRAM_LEN`]. Ordered by recency, not relevance.
    Scan,
}

/// One search result.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// The matching message.
    pub message_id: Uuid,
    /// Its conversation.
    pub conv_id: Uuid,
    /// The conversation's title, if it has one.
    pub conv_title: Option<String>,
    /// The conversation's surface.
    pub surface: Surface,
    /// Who wrote the matching message.
    pub role: MessageRole,
    /// An excerpt with the match delimited by `[` and `]`.
    pub snippet: String,
    /// Unix seconds.
    pub created_at: i64,
    /// bm25 score (lower is better) for [`HitSource::Index`], `0.0` otherwise.
    pub score: f64,
    /// How the hit was found.
    pub source: HitSource,
}

/// Search message history.
///
/// `query` is treated as a literal phrase: FTS5 operator syntax (`AND`, `*`,
/// `-`, `NEAR`) is escaped away rather than exposed, because a stray `-` in a
/// user's search box turning into a syntax error is not a feature.
pub fn search(conn: &Connection, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.chars().count() < MIN_TRIGRAM_LEN {
        return scan(conn, trimmed, limit);
    }

    let mut stmt = conn.prepare(
        "SELECT m.id, m.conv_id, c.title, c.surface, m.role,
                snippet(messages_fts, 0, '[', ']', '…', 12),
                m.created_at, bm25(messages_fts)
         FROM messages_fts
         JOIN messages m ON m.rowid = messages_fts.rowid
         JOIN conversations c ON c.id = m.conv_id
         WHERE messages_fts MATCH ?1
         ORDER BY bm25(messages_fts)
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![as_fts_phrase(trimmed), limit as i64], |row| {
        let surface: String = row.get(3)?;
        let role: String = row.get(4)?;
        Ok((|| -> Result<SearchHit> {
            Ok(SearchHit {
                message_id: row.get(0)?,
                conv_id: row.get(1)?,
                conv_title: row.get(2)?,
                surface: codec::surface_from_str(&surface)?,
                role: codec::message_role_from_str(&role)?,
                snippet: row.get(5)?,
                created_at: row.get(6)?,
                score: row.get(7)?,
                source: HitSource::Index,
            })
        })())
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// The sub-trigram fallback: an unranked substring scan, newest first.
fn scan(conn: &Connection, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.conv_id, c.title, c.surface, m.role, m.content, m.created_at
         FROM messages m
         JOIN conversations c ON c.id = m.conv_id
         WHERE m.content LIKE ?1 ESCAPE '\\'
         ORDER BY m.created_at DESC
         LIMIT ?2",
    )?;
    let pattern = format!("%{}%", escape_like(query));
    let rows = stmt.query_map(params![pattern, limit as i64], |row| {
        let surface: String = row.get(3)?;
        let role: String = row.get(4)?;
        let content: String = row.get(5)?;
        Ok((|| -> Result<SearchHit> {
            Ok(SearchHit {
                message_id: row.get(0)?,
                conv_id: row.get(1)?,
                conv_title: row.get(2)?,
                surface: codec::surface_from_str(&surface)?,
                role: codec::message_role_from_str(&role)?,
                snippet: excerpt(&content, query),
                created_at: row.get(6)?,
                score: 0.0,
                source: HitSource::Scan,
            })
        })())
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// Whether the external-content index still agrees with `messages`.
///
/// FTS5 reports inconsistency as an error, not a row, so this catches it. A
/// `false` here means one of the §12 triggers was lost or a write bypassed
/// them; the fix is [`rebuild_index`].
pub fn index_is_consistent(conn: &Connection) -> Result<bool> {
    match conn.execute(
        "INSERT INTO messages_fts(messages_fts) VALUES('integrity-check')",
        [],
    ) {
        Ok(_) => Ok(true),
        Err(rusqlite::Error::SqliteFailure(ffi, _))
            if ffi.code == rusqlite::ErrorCode::DatabaseCorrupt =>
        {
            Ok(false)
        }
        Err(e) => Err(StoreError::Sqlite(e)),
    }
}

/// Rebuild the FTS index from `messages`.
///
/// The repair for a stale index. Cheap enough at this data volume to run
/// unconditionally when [`index_is_consistent`] says no.
pub fn rebuild_index(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO messages_fts(messages_fts) VALUES('rebuild')",
        [],
    )?;
    Ok(())
}

/// Wrap a user query as a single FTS5 phrase, doubling embedded quotes.
///
/// Everything else — `*`, `-`, `NEAR`, `AND` — loses its meaning inside the
/// quotes, which is the intent.
fn as_fts_phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// Escape `LIKE` wildcards for the `ESCAPE '\'` form used by [`scan`].
fn escape_like(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for c in query.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A short window of `content` around the first occurrence of `needle`,
/// delimited the same way `snippet()` delimits.
fn excerpt(content: &str, needle: &str) -> String {
    const CONTEXT: usize = 32;
    let Some(byte_idx) = content.find(needle) else {
        return content.chars().take(CONTEXT * 2).collect();
    };
    // Work in chars so the window never splits a multi-byte character.
    let char_idx = content[..byte_idx].chars().count();
    let start = char_idx.saturating_sub(CONTEXT);
    let chars: Vec<char> = content.chars().collect();
    let needle_len = needle.chars().count();
    let end = (char_idx + needle_len + CONTEXT).min(chars.len());

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..char_idx]);
    out.push('[');
    out.extend(&chars[char_idx..char_idx + needle_len]);
    out.push(']');
    out.extend(&chars[char_idx + needle_len..end]);
    if end < chars.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::history::{NewMessage, create_conversation, insert_message};

    fn seeded() -> Db {
        let db = Db::open_in_memory().expect("open");
        db.with_conn(|c| {
            let conv = create_conversation(c, Surface::Ask, None)?;
            for body in [
                "the quick brown fox",
                "日本語のテキストも検索できる",
                "unrelated chatter",
            ] {
                insert_message(
                    c,
                    conv,
                    &NewMessage {
                        content: body.to_owned(),
                        ..Default::default()
                    },
                )?;
            }
            Ok(())
        })
        .expect("seed");
        db
    }

    // `insert_message` needs `&mut Connection`; `with_conn` hands one out.
    #[test]
    fn finds_ascii_substrings() {
        let db = seeded();
        let hits = db.with_conn(|c| search(c, "brown", 10)).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, HitSource::Index);
        assert!(hits[0].snippet.contains('['));
    }

    #[test]
    fn finds_japanese_substrings_which_is_why_the_tokenizer_is_trigram() {
        let db = seeded();
        let hits = db.with_conn(|c| search(c, "テキスト", 10)).expect("search");
        assert_eq!(hits.len(), 1, "trigram must index CJK without a segmenter");
    }

    #[test]
    fn short_queries_fall_back_to_a_scan_instead_of_returning_nothing() {
        let db = seeded();
        let hits = db.with_conn(|c| search(c, "日本", 10)).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, HitSource::Scan);
        assert!(hits[0].snippet.contains("[日本]"));
    }

    #[test]
    fn fts_operators_in_the_query_do_not_error() {
        let db = seeded();
        for query in ["fox OR bogus", "-quick", "brown*", "\"unbalanced"] {
            db.with_conn(|c| search(c, query, 10))
                .unwrap_or_else(|e| panic!("query {query:?} should not error: {e}"));
        }
    }

    #[test]
    fn like_wildcards_are_literal_in_the_scan_path() {
        let db = Db::open_in_memory().expect("open");
        db.with_conn(|c| {
            let conv = create_conversation(c, Surface::Ask, None)?;
            insert_message(
                c,
                conv,
                &NewMessage {
                    content: "100% done".into(),
                    ..Default::default()
                },
            )?;
            insert_message(
                c,
                conv,
                &NewMessage {
                    content: "nothing here".into(),
                    ..Default::default()
                },
            )?;
            Ok(())
        })
        .expect("seed");

        let hits = db.with_conn(|c| search(c, "0%", 10)).expect("search");
        assert_eq!(hits.len(), 1, "`%` must not act as a wildcard");
    }

    #[test]
    fn the_index_starts_consistent() {
        let db = seeded();
        assert!(db.with_conn(|c| index_is_consistent(c)).expect("check"));
    }

    #[test]
    fn empty_query_returns_nothing_rather_than_everything() {
        let db = seeded();
        assert!(
            db.with_conn(|c| search(c, "   ", 10))
                .expect("search")
                .is_empty()
        );
    }
}
