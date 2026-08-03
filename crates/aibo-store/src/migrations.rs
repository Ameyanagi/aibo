//! Versioned migrations: backup first, run in a transaction, roll back on failure (§12).
//!
//! "Migrations exist from day one; retrofitting them onto a shipped paid app is
//! misery." The DDL below is copied from §12 verbatim, **including the three
//! FTS5 external-content triggers**. Those triggers are not optional: an
//! external-content FTS5 table does not maintain itself, and without them the
//! index silently goes stale and history search quietly stops finding recent
//! messages. `tests/fts_staleness.rs` fails if they are ever dropped.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::{Result, StoreError};

/// The schema version this build writes and expects.
pub const SCHEMA_VERSION: i64 = 2;

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

/// v2 — enforce the ownership relationships already represented by
/// `conv_id`/`tool_call_id`, including cleanup of legacy orphan rows.
///
/// Triggers avoid rebuilding tables containing user data. They provide the
/// same insert/update/delete guarantees as foreign keys for the existing v1
/// shape and migrate safely while `foreign_keys` is enabled.
const V2: &str = r#"
DELETE FROM tool_calls
 WHERE conv_id NOT IN (SELECT id FROM conversations);
DELETE FROM file_snapshots
 WHERE tool_call_id NOT IN (SELECT id FROM tool_calls);

CREATE TRIGGER tool_calls_bi BEFORE INSERT ON tool_calls
WHEN NOT EXISTS (SELECT 1 FROM conversations WHERE id = new.conv_id)
BEGIN
  SELECT RAISE(ABORT, 'tool_calls.conv_id has no conversation');
END;
CREATE TRIGGER tool_calls_bu BEFORE UPDATE OF conv_id ON tool_calls
WHEN NOT EXISTS (SELECT 1 FROM conversations WHERE id = new.conv_id)
BEGIN
  SELECT RAISE(ABORT, 'tool_calls.conv_id has no conversation');
END;
CREATE TRIGGER tool_calls_ad AFTER DELETE ON tool_calls BEGIN
  DELETE FROM file_snapshots WHERE tool_call_id = old.id;
END;
CREATE TRIGGER conversations_tool_calls_ad AFTER DELETE ON conversations BEGIN
  DELETE FROM tool_calls WHERE conv_id = old.id;
END;
CREATE TRIGGER file_snapshots_bi BEFORE INSERT ON file_snapshots
WHEN NOT EXISTS (SELECT 1 FROM tool_calls WHERE id = new.tool_call_id)
BEGIN
  SELECT RAISE(ABORT, 'file_snapshots.tool_call_id has no tool call');
END;
CREATE TRIGGER file_snapshots_bu BEFORE UPDATE OF tool_call_id ON file_snapshots
WHEN NOT EXISTS (SELECT 1 FROM tool_calls WHERE id = new.tool_call_id)
BEGIN
  SELECT RAISE(ABORT, 'file_snapshots.tool_call_id has no tool call');
END;
"#;

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: V1,
    },
    Migration {
        version: 2,
        sql: V2,
    },
];

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
    let mut statement = conn.prepare("SELECT version FROM schema_version ORDER BY rowid")?;
    let versions = statement
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    match versions.as_slice() {
        [] => Ok(0),
        [version] => Ok(*version),
        [first, ..] => Err(StoreError::HalfMigrated {
            at: *first,
            backup: None,
        }),
    }
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
        validate_current_schema(conn, from, None)?;
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
                backup: backup.clone(),
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
    validate_current_schema(conn, after, backup.clone())?;

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
    name.push(format!(".v{version}.{stamp}.{}.bak", uuid::Uuid::now_v7()));
    let backup = db_path.with_file_name(name);

    copy_file_create_new(db_path, &backup)?;
    if let Some(parent) = backup.parent() {
        sync_directory(parent)?;
    }
    Ok(backup)
}

/// Fold the WAL back into the main database file.
///
/// `wal_checkpoint` returns a row even when the database is not in WAL mode, so
/// the query is run for effect and the row discarded.
pub(crate) fn checkpoint_truncate(conn: &Connection) -> Result<()> {
    let (busy, _log_frames, _checkpointed): (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok((0, 0, 0)),
            other => Err(other),
        })?;
    if busy != 0 {
        return Err(StoreError::Locked {
            timeout: crate::db::BUSY_TIMEOUT,
        });
    }
    Ok(())
}

/// Put a pre-migration backup back in place.
///
/// The caller **must** have dropped every [`rusqlite::Connection`] to
/// `db_path` first. The stale `-wal` and `-shm` sidecars are removed, because a
/// WAL from the failed migration applied on top of the restored main file would
/// re-apply exactly the change that failed.
pub fn restore_backup(backup: &Path, db_path: &Path) -> Result<()> {
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp_name = db_path.file_name().unwrap_or_default().to_os_string();
    temp_name.push(format!(".restore-{}", uuid::Uuid::now_v7()));
    let temp = db_path.with_file_name(temp_name);

    copy_file_create_new(backup, &temp)?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(db_path, suffix);
        if sidecar.exists()
            && let Err(error) = fs::remove_file(&sidecar)
        {
            let _ = fs::remove_file(&temp);
            return Err(StoreError::io(&sidecar, error));
        }
    }
    if let Err(error) = fs::rename(&temp, db_path) {
        let _ = fs::remove_file(&temp);
        return Err(StoreError::io(db_path, error));
    }
    sync_directory(parent)?;
    Ok(())
}

fn copy_file_create_new(source: &Path, destination: &Path) -> Result<()> {
    let mut input = File::open(source).map_err(|error| StoreError::io(source, error))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options
        .open(destination)
        .map_err(|error| StoreError::io(destination, error))?;
    let result = (|| {
        io::copy(&mut input, &mut output).map_err(|error| StoreError::io(destination, error))?;
        output
            .sync_all()
            .map_err(|error| StoreError::io(destination, error))?;
        let permissions = input
            .metadata()
            .map_err(|error| StoreError::io(source, error))?
            .permissions();
        fs::set_permissions(destination, permissions)
            .map_err(|error| StoreError::io(destination, error))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| StoreError::io(path, error))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn validate_current_schema(conn: &Connection, version: i64, backup: Option<PathBuf>) -> Result<()> {
    if version != SCHEMA_VERSION {
        return Err(StoreError::HalfMigrated {
            at: version,
            backup,
        });
    }

    const REQUIRED: &[(&str, &str)] = &[
        ("table", "conversations"),
        ("table", "messages"),
        ("table", "messages_fts"),
        ("table", "clipboard_history"),
        ("table", "tool_calls"),
        ("table", "permissions"),
        ("table", "file_snapshots"),
        ("table", "actions"),
        ("table", "settings"),
        ("table", "schema_version"),
        ("index", "idx_conv_updated"),
        ("index", "idx_msg_conv"),
        ("index", "idx_clip_expiry"),
        ("trigger", "messages_ai"),
        ("trigger", "messages_ad"),
        ("trigger", "messages_au"),
        ("trigger", "tool_calls_bi"),
        ("trigger", "tool_calls_bu"),
        ("trigger", "tool_calls_ad"),
        ("trigger", "conversations_tool_calls_ad"),
        ("trigger", "file_snapshots_bi"),
        ("trigger", "file_snapshots_bu"),
    ];
    for (kind, name) in REQUIRED {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type=?1 AND name=?2)",
            rusqlite::params![kind, name],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StoreError::HalfMigrated {
                at: version,
                backup: backup.clone(),
            });
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
    fn schema_creates_the_three_fts_triggers() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrate(&mut conn, None).expect("migrate");
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type='trigger' AND name LIKE 'messages_%' ORDER BY name",
            )
            .expect("prepare");
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");
        assert_eq!(names, vec!["messages_ad", "messages_ai", "messages_au"]);
    }

    #[test]
    fn current_version_with_missing_schema_object_is_rejected() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrate(&mut conn, None).expect("migrate");
        conn.execute_batch("DROP TRIGGER messages_ai")
            .expect("damage schema");
        let error = migrate(&mut conn, None).expect_err("must validate current schema");
        assert!(matches!(error, StoreError::HalfMigrated { .. }));
    }

    #[test]
    fn duplicate_schema_version_rows_are_rejected() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrate(&mut conn, None).expect("migrate");
        conn.execute(
            "INSERT INTO schema_version(version) VALUES (?1)",
            [SCHEMA_VERSION],
        )
        .expect("duplicate");
        assert!(matches!(
            current_version(&conn),
            Err(StoreError::HalfMigrated { .. })
        ));
    }

    #[test]
    fn tool_records_follow_their_owners() {
        let mut conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("PRAGMA foreign_keys=ON")
            .expect("foreign keys");
        migrate(&mut conn, None).expect("migrate");
        let conversation = vec![1_u8; 16];
        let tool = vec![2_u8; 16];
        let snapshot = vec![3_u8; 16];
        conn.execute(
            "INSERT INTO conversations(id,surface,created_at,updated_at)
             VALUES(?1,'do',1,1)",
            [&conversation],
        )
        .expect("conversation");
        conn.execute(
            "INSERT INTO tool_calls(id,conv_id,tier,name,created_at)
             VALUES(?1,?2,1,'test',1)",
            rusqlite::params![tool, conversation],
        )
        .expect("tool call");
        conn.execute(
            "INSERT INTO file_snapshots(id,tool_call_id,path,created_at)
             VALUES(?1,?2,'/tmp/a',1)",
            rusqlite::params![snapshot, tool],
        )
        .expect("snapshot");

        conn.execute("DELETE FROM conversations WHERE id=?1", [&conversation])
            .expect("delete conversation");
        let tools: i64 = conn
            .query_row("SELECT count(*) FROM tool_calls", [], |row| row.get(0))
            .expect("tools");
        let snapshots: i64 = conn
            .query_row("SELECT count(*) FROM file_snapshots", [], |row| row.get(0))
            .expect("snapshots");
        assert_eq!((tools, snapshots), (0, 0));
    }

    #[test]
    fn v1_to_v2_removes_existing_orphans_before_enforcing_ownership() {
        let mut conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(V1).expect("v1 schema");
        conn.execute("INSERT INTO schema_version(version) VALUES(1)", [])
            .expect("v1 marker");
        conn.execute(
            "INSERT INTO tool_calls(id,conv_id,tier,name,created_at)
             VALUES(x'01',x'02',1,'orphan',1)",
            [],
        )
        .expect("orphan tool");
        conn.execute(
            "INSERT INTO file_snapshots(id,tool_call_id,path,created_at)
             VALUES(x'03',x'01','/tmp/orphan',1)",
            [],
        )
        .expect("orphan snapshot");

        let report = migrate(&mut conn, None).expect("migrate to v2");
        assert_eq!((report.from, report.to), (1, SCHEMA_VERSION));
        let tools: i64 = conn
            .query_row("SELECT count(*) FROM tool_calls", [], |row| row.get(0))
            .expect("tools");
        let snapshots: i64 = conn
            .query_row("SELECT count(*) FROM file_snapshots", [], |row| row.get(0))
            .expect("snapshots");
        assert_eq!((tools, snapshots), (0, 0));
    }

    #[test]
    fn busy_checkpoint_is_not_reported_as_success() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("wal.db");
        let reader = Connection::open(&path).expect("reader");
        reader
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE values_(value INTEGER);
                 INSERT INTO values_ VALUES(1);",
            )
            .expect("setup");
        checkpoint_truncate(&reader).expect("initial checkpoint");
        reader.execute_batch("BEGIN").expect("begin reader");
        reader
            .query_row("SELECT value FROM values_", [], |row| row.get::<_, i64>(0))
            .expect("establish snapshot");

        let writer = Connection::open(&path).expect("writer");
        writer
            .execute("INSERT INTO values_ VALUES(2)", [])
            .expect("write WAL frame");
        assert!(matches!(
            checkpoint_truncate(&writer),
            Err(StoreError::Locked { .. })
        ));
        reader.execute_batch("ROLLBACK").expect("end reader");
    }

    #[test]
    fn backup_names_do_not_collide_within_one_second() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("database.db");
        let mut conn = Connection::open(&path).expect("open");
        migrate(&mut conn, Some(&path)).expect("migrate");
        let first = backup_file(&conn, &path).expect("first backup");
        let second = backup_file(&conn, &path).expect("second backup");
        assert_ne!(first, second);
        assert!(first.exists() && second.exists());
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
                supported: SCHEMA_VERSION
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
