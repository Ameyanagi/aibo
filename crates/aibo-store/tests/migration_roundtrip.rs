//! Migrations, backups and the key-loss recovery path, against a real
//! SQLCipher file in a temp directory (§12).

use aibo_core::types::{MessageRole, Surface};
use aibo_store::db::archive_unreadable;
use aibo_store::history::{
    ExportFilter, NewMessage, create_conversation, export_json, export_markdown, insert_message,
    messages,
};
use aibo_store::migrations::{SCHEMA_VERSION, current_version, migrate, restore_backup};
use aibo_store::search::search;
use aibo_store::{Db, DbKey, StoreError};
use rusqlite::Connection;
use std::path::Path;
use tempfile::TempDir;

fn seed(db: &Db, body: &str) {
    db.with_conn(|c| {
        let conv = create_conversation(c, Surface::Ask, Some("com.microsoft.VSCode"))?;
        insert_message(
            c,
            conv,
            &NewMessage {
                role: MessageRole::User,
                content: body.to_owned(),
                ..Default::default()
            },
        )?;
        Ok(())
    })
    .expect("seed");
}

fn message_count(db: &Db) -> i64 {
    db.with_conn(|c| Ok(c.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?))
        .expect("count")
}

#[test]
fn a_fresh_file_migrates_to_v1_and_survives_a_close_and_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("aibo.db");
    let key = DbKey::generate().expect("key");

    {
        let db = Db::open(&path, &key).expect("first open");
        assert_eq!(db.schema_version().expect("version"), SCHEMA_VERSION);
        seed(&db, "the encrypted round trip works");
    }

    let db = Db::open(&path, &key).expect("reopen");
    assert_eq!(db.schema_version().expect("version"), SCHEMA_VERSION);
    assert_eq!(message_count(&db), 1);
    // FTS survived the round trip too — the index lives inside the encrypted
    // file, which is the whole reason §12 chose whole-database encryption.
    assert_eq!(
        db.with_conn(|c| search(c, "encrypted", 10))
            .expect("search")
            .len(),
        1
    );
}

#[test]
fn reopening_an_already_current_database_runs_no_ddl() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("aibo.db");
    let key = DbKey::generate().expect("key");
    drop(Db::open(&path, &key).expect("open"));

    let mut conn = raw_open(&path, &key);
    let report = migrate(&mut conn, Some(&path)).expect("migrate");
    assert!(!report.changed());
    assert!(
        report.backup.is_none(),
        "a no-op migration must not litter the directory with backups"
    );
}

#[test]
fn a_failing_migration_rolls_back_and_leaves_the_data_intact() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("aibo.db");
    let key = DbKey::generate().expect("key");

    {
        let db = Db::open(&path, &key).expect("open");
        seed(&db, "data that must survive a failed migration");
    }

    // Rewind the recorded version while leaving the tables in place. The v1 DDL
    // will now fail on `CREATE TABLE conversations` — a faithful stand-in for
    // any step that fails halfway through a batch.
    let mut conn = raw_open(&path, &key);
    conn.execute("UPDATE schema_version SET version = 0", [])
        .expect("rewind");

    let err = migrate(&mut conn, Some(&path)).expect_err("must fail");
    assert!(
        matches!(err, StoreError::Migration { from: 0, .. }),
        "got {err:?}"
    );

    // The transaction rolled the whole batch back: the data is still there and
    // nothing was half-created.
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows, 1);
    assert_eq!(current_version(&conn).expect("version"), 0);
}

#[test]
fn a_backup_can_be_restored_over_a_damaged_file() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("aibo.db");
    let backup = dir.path().join("aibo.db.v1.bak");
    let key = DbKey::generate().expect("key");

    {
        let db = Db::open(&path, &key).expect("open");
        seed(&db, "the state we want back");
        // Fold the WAL in, then copy: a plain copy with an outstanding WAL is a
        // copy of a stale database.
        db.checkpoint().expect("checkpoint");
        std::fs::copy(&path, &backup).expect("backup");
    }

    {
        let db = Db::open(&path, &key).expect("reopen");
        seed(&db, "a change we want to undo");
        assert_eq!(message_count(&db), 2);
    }

    restore_backup(&backup, &path).expect("restore");

    let db = Db::open(&path, &key).expect("open restored");
    assert_eq!(message_count(&db), 1);
    let restored = db
        .with_conn(|c| search(c, "state we want back", 10))
        .expect("search");
    assert_eq!(restored.len(), 1);
}

#[test]
fn a_newer_schema_version_is_refused_rather_than_downgraded() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("aibo.db");
    let key = DbKey::generate().expect("key");
    drop(Db::open(&path, &key).expect("open"));

    {
        let conn = raw_open(&path, &key);
        conn.execute("UPDATE schema_version SET version = 7", [])
            .expect("bump");
        drop(conn);
    }

    let err = Db::open(&path, &key).expect_err("must refuse");
    assert!(
        matches!(
            err,
            StoreError::SchemaTooNew {
                found: 7,
                supported: 1
            }
        ),
        "got {err:?}"
    );
    assert_eq!(err.recovery(), Some(aibo_store::Recovery::UpgradeApp));
}

#[test]
fn key_loss_is_loud_and_both_recovery_paths_work() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("aibo.db");
    let key = DbKey::generate().expect("key");
    let code = key.to_recovery_code().as_str().to_owned();

    {
        let db = Db::open(&path, &key).expect("open");
        seed(&db, "history worth recovering");
    }

    // A device without the key — a restored backup on a new machine.
    let wrong = DbKey::generate().expect("key");
    let err = Db::open(&path, &wrong).expect_err("must fail loudly");
    assert!(matches!(err, StoreError::KeyLoss { .. }), "got {err:?}");
    assert_eq!(
        err.recovery(),
        Some(aibo_store::Recovery::RecoveryCodeOrStartFresh)
    );

    // Path 1: the printed recovery code.
    let recovered = DbKey::from_recovery_code(&code).expect("recovery code");
    let db = Db::open(&path, &recovered).expect("recovered open");
    assert_eq!(message_count(&db), 1);
    drop(db);

    // Path 2: start fresh. The unreadable file is archived, never deleted.
    let archived = archive_unreadable(&path).expect("archive");
    assert!(archived.exists());
    assert!(!path.exists());
    let fresh = Db::open(&path, &wrong).expect("fresh database");
    assert_eq!(message_count(&fresh), 0);
    assert_eq!(fresh.schema_version().expect("version"), SCHEMA_VERSION);
}

#[test]
fn export_survives_the_round_trip_in_both_formats() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("aibo.db");
    let key = DbKey::generate().expect("key");

    {
        let db = Db::open(&path, &key).expect("open");
        seed(&db, "what if I stop paying");
    }

    let db = Db::open(&path, &key).expect("reopen");
    let json = db
        .with_conn(|c| export_json(c, &ExportFilter::default()))
        .expect("json");
    let markdown = db
        .with_conn(|c| export_markdown(c, &ExportFilter::default()))
        .expect("markdown");

    assert!(json.contains("what if I stop paying"));
    assert!(json.contains("\"schema_version\": 1"));
    assert!(markdown.contains("what if I stop paying"));
    assert!(markdown.contains("com.microsoft.VSCode"));

    // And the message the export claims is the message that is stored.
    let conv = db
        .with_conn(|c| {
            Ok(
                c.query_row("SELECT id FROM conversations LIMIT 1", [], |r| {
                    r.get::<_, uuid::Uuid>(0)
                })?,
            )
        })
        .expect("conversation id");
    let stored = db.with_conn(|c| messages(c, conv)).expect("messages");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].content, "what if I stop paying");
}

/// Open the file the way [`aibo_store::Db`] does, without migrating, so tests
/// can inspect and perturb it.
fn raw_open(path: &Path, key: &DbKey) -> Connection {
    let conn = Connection::open(path).expect("open");
    conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", key.to_hex().as_str()))
        .expect("key");
    conn.execute_batch("PRAGMA foreign_keys = ON;").expect("fk");
    conn
}
