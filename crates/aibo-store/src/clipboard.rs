//! Clipboard history with concealed/transient hygiene and 24h retention (§12).
//!
//! The rule that shapes this whole module, from §12:
//!
//! > **Concealed items are not recorded at all** — the `concealed` column exists
//! > to mark that *something* was skipped, never to hold the content. Writing a
//! > password into an encrypted database is still writing a password into a
//! > database.
//!
//! So [`record`] has exactly two behaviours. A concealed item produces a row
//! with `content = NULL` and `concealed = 1`, which is what lets the history
//! list say "an item from 1Password was skipped" instead of showing a gap. A
//! transient item produces no row at all — the source marked it usable now and
//! never persisted, and §12 honours that literally.
//!
//! Detection of the markers themselves (`org.nspasteboard.ConcealedType` /
//! `TransientType`, `ExcludeClipboardContentFromMonitorProcessing`, and the app
//! denylist) is the platform layer's job; by the time an item reaches here the
//! flags on `ClipboardItem` are authoritative.

use std::path::PathBuf;
use std::time::Duration;

use aibo_core::types::{ClipboardItem, ClipboardKind};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::codec;
use crate::error::Result;
use crate::now_unix;

/// Default retention: 24 hours (§12).
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// Default cap on retained items. §12 asks for a "capped count" without naming
/// one; this is the count, in one place, so it can be tuned by evidence.
pub const DEFAULT_MAX_ITEMS: usize = 200;

/// What [`record`] decided to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// The payload was stored.
    Stored(Uuid),
    /// A marker row was stored with no content, because the item was concealed.
    MarkedConcealed(Uuid),
    /// Nothing was stored: the item was transient, empty, or of a kind aibo
    /// does not handle.
    Skipped,
}

impl Recorded {
    /// The row id, when one was written.
    pub fn id(self) -> Option<Uuid> {
        match self {
            Recorded::Stored(id) | Recorded::MarkedConcealed(id) => Some(id),
            Recorded::Skipped => None,
        }
    }
}

/// A row from `clipboard_history`, as surfaced to the UI.
///
/// There is no `content` on concealed rows because there is no content stored;
/// the type makes that unrepresentable rather than relying on a convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Row id.
    pub id: Uuid,
    /// Payload kind.
    pub kind: ClipboardKind,
    /// The text payload. Always `None` for a concealed row.
    pub content: Option<String>,
    /// Referenced image or file payload. Empty for text and concealed rows.
    pub files: Vec<PathBuf>,
    /// The app that placed the item, when known.
    pub source_app: Option<String>,
    /// This row is a marker for a skipped secret.
    pub concealed: bool,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds at which retention expires it.
    pub expires_at: i64,
}

/// Record a clipboard observation, honouring §12's hygiene rules.
pub fn record(conn: &Connection, item: &ClipboardItem, retention: Duration) -> Result<Recorded> {
    // Transient means "usable now, never persisted". There is nothing to mark
    // either: the user did not lose an item, the source declined to offer one.
    if item.transient {
        return Ok(Recorded::Skipped);
    }
    let Some(kind) = codec::clipboard_kind_to_str(item.kind) else {
        return Ok(Recorded::Skipped);
    };

    let id = Uuid::now_v7();
    let created_at = now_unix();
    let retention_seconds = i64::try_from(retention.as_secs()).unwrap_or(i64::MAX);
    let expires_at = created_at.saturating_add(retention_seconds);

    // The one place the concealed rule is enforced. Note the content column is
    // bound to NULL, not to a redacted string: a redacted string is still a
    // decision made at render time, and this must be a decision made at write
    // time.
    let content: Option<String> = if item.concealed {
        None
    } else {
        match item.kind {
            ClipboardKind::Text => item.text.clone(),
            ClipboardKind::ImageRef | ClipboardKind::Files => {
                if item.files.is_empty() {
                    return Ok(Recorded::Skipped);
                }
                Some(serde_json::to_string(&item.files)?)
            }
            ClipboardKind::Unsupported | ClipboardKind::Empty => unreachable!("filtered above"),
        }
    };

    conn.execute(
        "INSERT INTO clipboard_history
           (id, kind, content, source_app, concealed, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            kind,
            content.as_deref(),
            item.source_app,
            i64::from(item.concealed),
            created_at,
            expires_at,
        ],
    )?;

    Ok(if item.concealed {
        Recorded::MarkedConcealed(id)
    } else {
        Recorded::Stored(id)
    })
}

/// Unexpired, non-concealed items, newest first.
///
/// Concealed rows are excluded here as well as at write time. §12: "never
/// surfaced, never sent" — the marker exists for the count and the explanation,
/// not for the picker.
pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<HistoryEntry>> {
    let limit = sql_limit(limit)?;
    let mut stmt = conn.prepare(
        "SELECT id, kind, content, source_app, concealed, created_at, expires_at
         FROM clipboard_history
         WHERE concealed = 0 AND expires_at > ?1
         ORDER BY created_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![now_unix(), limit], read_entry)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// One entry by id, concealed rows included so the UI can explain a gap.
pub fn get(conn: &Connection, id: Uuid) -> Result<Option<HistoryEntry>> {
    conn.query_row(
        "SELECT id, kind, content, source_app, concealed, created_at, expires_at
         FROM clipboard_history WHERE id = ?1",
        params![id],
        read_entry,
    )
    .optional()?
    .transpose()
}

/// How many concealed items were skipped and are still within retention.
pub fn concealed_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM clipboard_history WHERE concealed = 1 AND expires_at > ?1",
        params![now_unix()],
        |r| r.get(0),
    )?)
}

/// Delete everything past its expiry. Run on a timer and on open.
pub fn purge_expired(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM clipboard_history WHERE expires_at <= ?1",
        params![now_unix()],
    )?)
}

/// Trim to the newest `max` items, expired or not.
pub fn enforce_cap(conn: &Connection, max: usize) -> Result<usize> {
    let max = sql_limit(max)?;
    Ok(conn.execute(
        "DELETE FROM clipboard_history WHERE id NOT IN (
             SELECT id FROM clipboard_history ORDER BY created_at DESC, id DESC LIMIT ?1
         )",
        params![max],
    )?)
}

/// The one-click purge §12 requires.
pub fn purge_all(conn: &Connection) -> Result<usize> {
    Ok(conn.execute("DELETE FROM clipboard_history", [])?)
}

type Mapped<T> = rusqlite::Result<Result<T>>;

fn read_entry(row: &rusqlite::Row<'_>) -> Mapped<HistoryEntry> {
    let kind: String = row.get(1)?;
    let concealed: i64 = row.get(4)?;
    let stored_content: Option<String> = row.get(2)?;
    Ok((|| {
        let kind = codec::clipboard_kind_from_str(&kind)?;
        let (content, files) = if concealed != 0 {
            (None, Vec::new())
        } else {
            match kind {
                ClipboardKind::Text => (stored_content, Vec::new()),
                ClipboardKind::ImageRef | ClipboardKind::Files => {
                    let files = match stored_content {
                        Some(payload) => serde_json::from_str(&payload)?,
                        None => Vec::new(),
                    };
                    (None, files)
                }
                ClipboardKind::Unsupported | ClipboardKind::Empty => (None, Vec::new()),
            }
        };
        Ok(HistoryEntry {
            id: row.get(0)?,
            kind,
            // Belt and braces: even if a row somehow carried content with
            // `concealed = 1`, it does not leave this function.
            content,
            files,
            source_app: row.get(3)?,
            concealed: concealed != 0,
            created_at: row.get(5)?,
            expires_at: row.get(6)?,
        })
    })())
}

fn sql_limit(limit: usize) -> Result<i64> {
    i64::try_from(limit).map_err(|_| crate::StoreError::InvalidLimit { value: limit })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn item(text: &str) -> ClipboardItem {
        ClipboardItem {
            kind: ClipboardKind::Text,
            text: Some(text.to_owned()),
            files: Vec::new(),
            concealed: false,
            transient: false,
            source_app: Some("com.apple.Safari".into()),
            sequence: 1,
            restorable: true,
        }
    }

    #[test]
    fn plain_text_is_recorded() {
        let db = Db::open_in_memory().expect("open");
        let recorded = db
            .with_conn(|c| record(c, &item("hello"), DEFAULT_RETENTION))
            .expect("record");
        assert!(matches!(recorded, Recorded::Stored(_)));

        let entries = db.with_conn(|c| recent(c, 10)).expect("recent");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content.as_deref(), Some("hello"));
        assert!(entries[0].files.is_empty());
    }

    #[test]
    fn file_references_round_trip_without_being_discarded() {
        let db = Db::open_in_memory().expect("open");
        let mut files = item("");
        files.kind = ClipboardKind::Files;
        files.text = None;
        files.files = vec![PathBuf::from("/tmp/one.txt"), PathBuf::from("/tmp/two.txt")];

        let recorded = db
            .with_conn(|c| record(c, &files, DEFAULT_RETENTION))
            .expect("record");
        assert!(matches!(recorded, Recorded::Stored(_)));
        let entries = db.with_conn(|c| recent(c, 10)).expect("recent");
        assert_eq!(entries[0].files, files.files);
        assert_eq!(entries[0].content, None);
    }

    #[test]
    fn a_reference_kind_without_a_reference_is_not_reported_as_stored() {
        let db = Db::open_in_memory().expect("open");
        let mut image = item("");
        image.kind = ClipboardKind::ImageRef;
        image.text = None;
        assert_eq!(
            db.with_conn(|c| record(c, &image, DEFAULT_RETENTION))
                .expect("record"),
            Recorded::Skipped
        );
    }

    #[test]
    fn a_concealed_item_is_marked_but_its_content_never_reaches_the_database() {
        let db = Db::open_in_memory().expect("open");
        let mut secret = item("hunter2");
        secret.concealed = true;
        secret.source_app = Some("com.1password.1password".into());

        let recorded = db
            .with_conn(|c| record(c, &secret, DEFAULT_RETENTION))
            .expect("record");
        let id = match recorded {
            Recorded::MarkedConcealed(id) => id,
            other => panic!("expected a concealed marker, got {other:?}"),
        };

        // Not in the picker.
        assert!(db.with_conn(|c| recent(c, 10)).expect("recent").is_empty());
        // Marked, so the UI can say something was skipped.
        assert_eq!(db.with_conn(|c| concealed_count(c)).expect("count"), 1);

        let entry = db
            .with_conn(|c| get(c, id))
            .expect("get")
            .expect("row present");
        assert!(entry.concealed);
        assert_eq!(entry.content, None);

        // The strongest form of the assertion: the bytes are not in the file.
        let raw: Option<String> = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT content FROM clipboard_history", [], |r| r.get(0))?)
            })
            .expect("raw read");
        assert_eq!(raw, None, "concealed content must never be written at all");
    }

    #[test]
    fn a_transient_item_leaves_no_row_at_all() {
        let db = Db::open_in_memory().expect("open");
        let mut transient = item("one-time code");
        transient.transient = true;
        assert_eq!(
            db.with_conn(|c| record(c, &transient, DEFAULT_RETENTION))
                .expect("record"),
            Recorded::Skipped
        );
        assert_eq!(db.with_conn(|c| concealed_count(c)).expect("count"), 0);
        let rows: i64 = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT count(*) FROM clipboard_history", [], |r| r.get(0))?)
            })
            .expect("count");
        assert_eq!(rows, 0);
    }

    #[test]
    fn empty_and_unsupported_kinds_are_skipped() {
        let db = Db::open_in_memory().expect("open");
        for kind in [ClipboardKind::Empty, ClipboardKind::Unsupported] {
            let mut it = item("");
            it.kind = kind;
            assert_eq!(
                db.with_conn(|c| record(c, &it, DEFAULT_RETENTION))
                    .expect("record"),
                Recorded::Skipped
            );
        }
    }

    #[test]
    fn expired_items_are_purged_and_hidden() {
        let db = Db::open_in_memory().expect("open");
        db.with_conn(|c| record(c, &item("stale"), Duration::ZERO))
            .expect("record");
        assert!(
            db.with_conn(|c| recent(c, 10)).expect("recent").is_empty(),
            "an expired item must not surface even before the purge runs"
        );
        assert_eq!(db.with_conn(|c| purge_expired(c)).expect("purge"), 1);
    }

    #[test]
    fn the_cap_keeps_the_newest_items() {
        let db = Db::open_in_memory().expect("open");
        for i in 0..5 {
            db.with_conn(|c| record(c, &item(&format!("item {i}")), DEFAULT_RETENTION))
                .expect("record");
        }
        db.with_conn(|c| enforce_cap(c, 3)).expect("cap");
        let entries = db.with_conn(|c| recent(c, 10)).expect("recent");
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn purge_all_clears_markers_too() {
        let db = Db::open_in_memory().expect("open");
        let mut secret = item("hunter2");
        secret.concealed = true;
        db.with_conn(|c| record(c, &secret, DEFAULT_RETENTION))
            .expect("record");
        db.with_conn(|c| record(c, &item("plain"), DEFAULT_RETENTION))
            .expect("record");
        assert_eq!(db.with_conn(|c| purge_all(c)).expect("purge"), 2);
        assert_eq!(db.with_conn(|c| concealed_count(c)).expect("count"), 0);
    }
}
