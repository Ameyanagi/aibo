//! Versioned migrations: backup first, run in a transaction, roll back on failure (§12).
//!
//! "Migrations exist from day one; retrofitting them onto a shipped paid app is
//! misery." The DDL below is copied from §12 verbatim, **including the three
//! FTS5 external-content triggers**. Those triggers are not optional: an
//! external-content FTS5 table does not maintain itself, and without them the
//! index silently goes stale and history search quietly stops finding recent
//! messages. `tests/fts_staleness.rs` fails if they are ever dropped.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::{Result, StoreError};

/// The schema version this build writes and expects.
pub const SCHEMA_VERSION: i64 = 1;

/// One forward migration step. There is no `down`: migrating down would drop
/// columns, and dropping columns is losing data.
struct Migration {
    /// The version the database is at *after* this step.
    version: i64,
    /// DDL, executed as one batch inside the migration transaction.
    sql: &'static str,
}

/// v1 — §12 in full.
const V1: &str = r#"
CREATE TABLE conversations (
  id           BLOB PRIMARY KEY,          -- uuid v7, time-sortable
  surface      TEXT NOT NULL,             -- complete|transform|ask|do
  source_app   TEXT,                      -- bundle id / exe name
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  title        TEXT
);
CREATE INDEX idx_conv_updated ON conversations(updated_at DESC);

CREATE TABLE messages (
  id           BLOB PRIMARY KEY,
  conv_id      BLOB NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  role         TEXT NOT NULL,             -- system|user|assistant|tool
  content      TEXT NOT NULL,
  provider     TEXT, model TEXT,
  usage_in     INTEGER, usage_out INTEGER,
  cost_micros  INTEGER,                   -- §14 spend meter
  latency_ms   INTEGER,
  created_at   INTEGER NOT NULL
);
CREATE INDEX idx_msg_conv ON messages(conv_id, created_at);

CREATE VIRTUAL TABLE messages_fts USING fts5(
  content, content='messages', content_rowid='rowid', tokenize='trigram'
);

-- REQUIRED. An external-content FTS5 table does NOT maintain itself; SQLite
-- makes the application responsible for consistency. Without these the index
-- silently goes stale and history search quietly stops finding recent messages.
CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, content)
    VALUES('delete', old.rowid, old.content);
END;
CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, content)
    VALUES('delete', old.rowid, old.content);
  INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TABLE clipboard_history (
  id           BLOB PRIMARY KEY,
  kind         TEXT NOT NULL,             -- text|image_ref|files
  content      TEXT,
  source_app   TEXT,
  concealed    INTEGER NOT NULL DEFAULT 0,-- never surfaced, never sent
  created_at   INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL           -- default now + 24h
);
CREATE INDEX idx_clip_expiry ON clipboard_history(expires_at);

CREATE TABLE tool_calls (
  id BLOB PRIMARY KEY, conv_id BLOB NOT NULL, tier INTEGER NOT NULL,
  name TEXT NOT NULL, args TEXT, result TEXT, approved INTEGER,
  duration_ms INTEGER, created_at INTEGER NOT NULL
);

CREATE TABLE permissions (
  scope TEXT PRIMARY KEY,                 -- "mcp:github:create_issue", "shell:git"
  decision TEXT NOT NULL,                 -- allow|deny|ask
  decided_at INTEGER NOT NULL
);

CREATE TABLE file_snapshots (             -- undo for tier 3 writes
  id BLOB PRIMARY KEY, tool_call_id BLOB NOT NULL,
  path TEXT NOT NULL, before BLOB, created_at INTEGER NOT NULL
);

CREATE TABLE actions (                    -- saved custom actions: the #1 request
  id BLOB PRIMARY KEY,                    -- for every Transform-class tool
  name TEXT NOT NULL, verb TEXT,          -- optional trigger word
  prompt TEXT NOT NULL,
  role TEXT, provider TEXT, model TEXT,   -- optional pinned binding
  app_scope TEXT,                         -- optional: only in this app
  hotkey TEXT,                            -- optional direct binding
  sort_order INTEGER NOT NULL
);

CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE schema_version (version INTEGER NOT NULL);
"#;

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: V1,
}];

/// What [`migrate`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Version found on open. `0` means an empty file.
    pub from: i64,
    /// Version after migrating.
    pub to: i64,
    /// The pre-migration file copy. Kept on success too, so a migration that
    /// completed but produced bad data is still recoverable.
    pub backup: Option<PathBuf>,
}

impl MigrationReport {
    /// Whether any DDL actually ran.
    pub fn changed(&self) -> bool {
        self.from != self.to
    }
}

/// Read `schema_version`. `0` means the table does not exist yet.
pub fn current_version(conn: &Connection) -> Result<i64> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type='table' AND name='schema_version'",
            [],
            |_| Ok(true),
        )
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(false),
            other => Err(other),
        })?;
    if !exists {
        return Ok(0);
    }
    let version: i64 = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
            r.get(0)
        })
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(0),
            other => Err(other),
        })?;
    Ok(version)
}

/// Bring `conn` up to [`SCHEMA_VERSION`].
///
/// The §12 contract, in order:
///
/// 1. A **file backup first**, whenever there is something to lose. The WAL is
///    truncated into the main file before copying, because a plain copy taken
///    while a WAL is outstanding is a copy of a stale database.
/// 2. Every step inside **one transaction**, so SQLite rolls the DDL back
///    itself if a statement fails — SQLite DDL is transactional, which is what
///    makes "half-migrated" a torn-file case rather than a routine one.
/// 3. On failure the error carries the backup path. The caller must drop the
///    connection before calling [`restore_backup`]; a restore cannot happen
///    while SQLite still holds the file open.
///
/// `db_path` is `None` for an in-memory database: there is no file to lose.
pub fn migrate(conn: &mut Connection, db_path: Option<&Path>) -> Result<MigrationReport> {
    let from = current_version(conn)?;
    if from > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: from,
            supported: SCHEMA_VERSION,
        });
    }
    if from == SCHEMA_VERSION {
        return Ok(MigrationReport {
            from,
            to: from,
            backup: None,
        });
    }

    // Nothing in an empty database is worth backing up.
    let backup = match db_path {
        Some(path) if from > 0 => Some(backup_file(conn, path)?),
        _ => None,
    };

    let tx = conn.transaction().map_err(|source| StoreError::Migration {
        from,
        to: SCHEMA_VERSION,
        backup: backup.clone(),
        source,
    })?;

    for step in MIGRATIONS.iter().filter(|m| m.version > from) {
        let applied = tx.execute_batch(step.sql).and_then(|()| {
            if step.version == 1 {
                tx.execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    [step.version],
                )
                .map(|_| ())
            } else {
                tx.execute("UPDATE schema_version SET version = ?1", [step.version])
                    .map(|_| ())
            }
        });
        if let Err(source) = applied {
            // Dropping the transaction rolls it back; be explicit anyway.
            let _ = tx.rollback();
            return Err(StoreError::Migration {
                from,
                to: step.version,
                backup,
                source,
            });
        }
    }

    tx.commit().map_err(|source| StoreError::Migration {
        from,
        to: SCHEMA_VERSION,
        backup: backup.clone(),
        source,
    })?;

    // A committed transaction that did not land the expected schema means the
    // file is torn under us; §12 wants that named rather than papered over.
    let after = current_version(conn)?;
    if after != SCHEMA_VERSION {
        return Err(StoreError::HalfMigrated {
            at: after,
            backup: backup.clone(),
        });
    }

    Ok(MigrationReport {
        from,
        to: SCHEMA_VERSION,
        backup,
    })
}

/// Copy the database file aside before migrating.
fn backup_file(conn: &Connection, db_path: &Path) -> Result<PathBuf> {
    checkpoint_truncate(conn)?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let version = current_version(conn)?;
    let mut name = db_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".v{version}.{stamp}.bak"));
    let backup = db_path.with_file_name(name);

    fs::copy(db_path, &backup).map_err(|source| StoreError::io(&backup, source))?;
    Ok(backup)
}

/// Fold the WAL back into the main database file.
///
/// `wal_checkpoint` returns a row even when the database is not in WAL mode, so
/// the query is run for effect and the row discarded.
pub(crate) fn checkpoint_truncate(conn: &Connection) -> Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(()),
            other => Err(other),
        })?;
    Ok(())
}

/// Put a pre-migration backup back in place.
///
/// The caller **must** have dropped every [`rusqlite::Connection`] to
/// `db_path` first. The stale `-wal` and `-shm` sidecars are removed, because a
/// WAL from the failed migration applied on top of the restored main file would
/// re-apply exactly the change that failed.
pub fn restore_backup(backup: &Path, db_path: &Path) -> Result<()> {
    fs::copy(backup, db_path).map_err(|source| StoreError::io(db_path, source))?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(db_path, suffix);
        if sidecar.exists() {
            fs::remove_file(&sidecar).map_err(|source| StoreError::io(&sidecar, source))?;
        }
    }
    Ok(())
}

/// `foo.db` → `foo.db-wal`. SQLite appends, it does not replace the extension.
pub(crate) fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    db_path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_migrates_to_current() {
        let mut conn = Connection::open_in_memory().expect("open");
        let report = migrate(&mut conn, None).expect("migrate");
        assert_eq!(report.from, 0);
        assert_eq!(report.to, SCHEMA_VERSION);
        assert!(report.changed());
        assert_eq!(current_version(&conn).expect("version"), SCHEMA_VERSION);
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrate(&mut conn, None).expect("first");
        let second = migrate(&mut conn, None).expect("second");
        assert!(!second.changed());
        assert!(second.backup.is_none());
    }

    #[test]
    fn v1_creates_the_three_fts_triggers() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrate(&mut conn, None).expect("migrate");
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type='trigger' ORDER BY name")
            .expect("prepare");
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");
        assert_eq!(names, vec!["messages_ad", "messages_ai", "messages_au"]);
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_downgraded() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrate(&mut conn, None).expect("migrate");
        conn.execute("UPDATE schema_version SET version = 99", [])
            .expect("bump");
        let err = migrate(&mut conn, None).expect_err("should refuse");
        assert!(matches!(
            err,
            StoreError::SchemaTooNew {
                found: 99,
                supported: 1
            }
        ));
    }

    #[test]
    fn sidecar_paths_append_rather_than_replace() {
        assert_eq!(
            sidecar_path(Path::new("/tmp/aibo.db"), "-wal"),
            PathBuf::from("/tmp/aibo.db-wal")
        );
    }
}
