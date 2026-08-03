//! Connection open, PRAGMA key, integrity check and the blocking pool (§12).
//!
//! Open order matters and is not negotiable:
//!
//! 1. `PRAGMA key` — nothing can be read before the key is supplied.
//! 2. A trivial read, to *prove* the key. SQLCipher does not verify the key at
//!    `PRAGMA key` time; a wrong key surfaces later as `SQLITE_NOTADB`, and if
//!    we did not force it here it would surface halfway through a migration.
//! 3. `PRAGMA foreign_keys=ON` — off by default, and the §12 schema depends on
//!    `ON DELETE CASCADE`. It must be set outside a transaction, so it happens
//!    before migrations, not after.
//! 4. WAL, `synchronous=NORMAL`, `busy_timeout`.
//! 5. Integrity check.
//! 6. Migrations.
//!
//! All SQLite work runs on `spawn_blocking`, never on the UI thread (§6): see
//! [`Db::call`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::{Result, StoreError};
use crate::key::DbKey;
use crate::migrations;

/// How long a write waits for another connection's lock before giving up.
///
/// A second aibo instance is the realistic cause, and §13 wants that reported
/// rather than hung on.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How thorough the on-open integrity check is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum IntegrityCheck {
    /// `PRAGMA quick_check` — structural only, cheap enough for every launch.
    #[default]
    Quick,
    /// `PRAGMA integrity_check` — full, including index consistency. Used by
    /// the "something is wrong" path in settings, not on the hot launch path.
    Full,
    /// Skip. Only for tests.
    Skip,
}

/// An open, keyed, migrated database.
///
/// Cheap to clone: clones share one [`rusqlite::Connection`] behind a mutex.
/// One connection is the right shape here — the workload is a single desktop
/// user, and a pool under WAL would only trade lock contention for file
/// contention.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").field("path", &self.path).finish()
    }
}

impl Db {
    /// Open (creating if needed), key, verify, check and migrate the database.
    ///
    /// On a failed migration the connection is dropped and the pre-migration
    /// backup is put back before the error is returned, so the file the user is
    /// left with is the one that worked yesterday.
    pub fn open(path: impl AsRef<Path>, key: &DbKey) -> Result<Self> {
        Self::open_with(path, key, IntegrityCheck::default())
    }

    /// [`Db::open`] with an explicit integrity-check depth.
    pub fn open_with(path: impl AsRef<Path>, key: &DbKey, check: IntegrityCheck) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|source| StoreError::io(parent, source))?;
        }

        let mut conn = open_keyed(&path, key)?;
        run_integrity_check(&conn, check, &path)?;

        match migrations::migrate(&mut conn, Some(&path)) {
            Ok(_) => Ok(Self {
                conn: Arc::new(Mutex::new(conn)),
                path: Some(path),
            }),
            Err(err) => {
                // The restore cannot happen while SQLite holds the file.
                let backup = err.migration_backup().map(Path::to_path_buf);
                drop(conn);
                if let Some(backup) = backup {
                    migrations::restore_backup(&backup, &path)?;
                }
                Err(err)
            }
        }
    }

    /// An in-memory database, migrated and ready. Tests only.
    ///
    /// SQLCipher is a no-op for `:memory:`; this exercises the schema, the
    /// triggers and the queries, not the encryption.
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        migrations::migrate(&mut conn, None)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: None,
        })
    }

    /// The file backing this database, or `None` for `:memory:`.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Run a closure against the connection on the current thread.
    ///
    /// Prefer [`Db::call`] anywhere near the UI: this blocks (§6 forbids
    /// SQLite on the UI thread).
    pub fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        // A poisoned mutex means a previous query panicked. The connection is
        // still structurally fine — SQLite state is not corrupted by a panic in
        // a Rust closure — so recover rather than propagating the poison and
        // taking the tray down with it (§6).
        let mut guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard).map_err(|error| match error {
            StoreError::Sqlite(source) => {
                let path = self.path.as_deref().unwrap_or_else(|| Path::new(""));
                StoreError::from_sqlite(source, path, BUSY_TIMEOUT)
            }
            other => other,
        })
    }

    /// Run a closure against the connection on the blocking pool.
    ///
    /// This is the only form UI code should use.
    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.with_conn(f))
            .await
            .map_err(|_| StoreError::BlockingTask)?
    }

    /// Re-encrypt the whole database under a new key (§12: "plan key rotation /
    /// rekey now even if unused ... adding it later means touching every user's
    /// file").
    ///
    /// The caller must store `new` in credential storage **after** this returns `Ok`,
    /// and must be prepared for a crash in between — which is what the recovery
    /// code is for.
    pub fn rekey(&self, new: &DbKey) -> Result<()> {
        self.with_conn(|conn| {
            let hex = new.to_hex();
            conn.execute_batch(&format!("PRAGMA rekey = \"x'{}'\";", hex.as_str()))?;
            Ok(())
        })
    }

    /// Run an integrity check against the open database.
    pub fn integrity_check(&self, check: IntegrityCheck) -> Result<()> {
        let path = self.path.clone().unwrap_or_default();
        self.with_conn(|conn| run_integrity_check(conn, check, &path))
    }

    /// Fold the WAL back into the main file. Call before copying the file.
    pub fn checkpoint(&self) -> Result<()> {
        self.with_conn(|conn| migrations::checkpoint_truncate(conn))
    }

    /// The schema version currently recorded in the file.
    pub fn schema_version(&self) -> Result<i64> {
        self.with_conn(|conn| migrations::current_version(conn))
    }
}

/// Open a connection and apply every PRAGMA §12 requires, in order.
fn open_keyed(path: &Path, key: &DbKey) -> Result<Connection> {
    let conn =
        Connection::open(path).map_err(|e| StoreError::from_sqlite(e, path, BUSY_TIMEOUT))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| StoreError::from_sqlite(e, path, BUSY_TIMEOUT))?;

    // Raw-key syntax: 64 hex characters inside `x'...'`, which skips SQLCipher's
    // KDF. Correct here because the key is already 32 uniformly random bytes —
    // there is no low-entropy passphrase for a KDF to stretch.
    //
    // The interpolation is injection-safe: `to_hex` only ever emits `[0-9a-f]`.
    {
        let hex = key.to_hex();
        conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", hex.as_str()))
            .map_err(|e| StoreError::from_sqlite(e, path, BUSY_TIMEOUT))?;
    }

    // Prove the key now. An empty file decrypts trivially (there is no header
    // yet); a populated file with the wrong key fails here as SQLITE_NOTADB,
    // which `from_sqlite` maps to `KeyLoss` rather than corruption.
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(|e| StoreError::from_sqlite(e, path, BUSY_TIMEOUT))?;

    // Off by default; the schema depends on ON DELETE CASCADE. Must be outside
    // a transaction, hence before migrations.
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|e| StoreError::from_sqlite(e, path, BUSY_TIMEOUT))?;

    // `journal_mode` returns the resulting mode as a row.
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .map_err(|e| StoreError::from_sqlite(e, path, BUSY_TIMEOUT))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(
            mode = %mode,
            "database did not accept WAL; falling back to whatever SQLite chose"
        );
    }

    conn.execute_batch("PRAGMA synchronous = NORMAL;")
        .map_err(|e| StoreError::from_sqlite(e, path, BUSY_TIMEOUT))?;

    Ok(conn)
}

fn run_integrity_check(conn: &Connection, check: IntegrityCheck, path: &Path) -> Result<()> {
    let sql = match check {
        IntegrityCheck::Skip => return Ok(()),
        IntegrityCheck::Quick => "PRAGMA quick_check(1)",
        IntegrityCheck::Full => "PRAGMA integrity_check(1)",
    };
    let report: String = conn
        .query_row(sql, [], |r| r.get(0))
        .map_err(|e| StoreError::from_sqlite(e, path, BUSY_TIMEOUT))?;
    if report.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(StoreError::Corrupt { detail: report })
    }
}

/// Archive an unreadable database and return where it went.
///
/// This is the "start fresh" half of §12's key-loss flow. The file is **moved,
/// never deleted**: an unreadable database is still the user's data, and a
/// recovery code found in a drawer next month should still work. Settings live
/// separately in plaintext TOML, so the app stays configured afterwards.
pub fn archive_unreadable(path: &Path) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    // Include a random component: start-fresh may be retried within one second,
    // and `rename` replaces an existing destination on Unix.
    name.push(format!(".unreadable.{stamp}.{}", uuid::Uuid::now_v7()));
    let archived = path.with_file_name(name);

    move_file_no_replace(path, &archived)?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = migrations::sidecar_path(path, suffix);
        if sidecar.exists() {
            let mut target = archived.file_name().unwrap_or_default().to_os_string();
            target.push(suffix);
            let target = archived.with_file_name(target);
            move_file_no_replace(&sidecar, &target)?;
        }
    }
    Ok(archived)
}

/// Move a file without ever replacing an existing recovery artifact.
///
/// The archive lives beside the database, so a hard link is on the same
/// filesystem. A crash between linking and unlinking leaves two recoverable
/// names instead of losing either copy.
fn move_file_no_replace(source: &Path, destination: &Path) -> Result<()> {
    fs::hard_link(source, destination).map_err(|error| StoreError::io(destination, error))?;
    if let Err(error) = fs::remove_file(source) {
        let _ = fs::remove_file(destination);
        return Err(StoreError::io(source, error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_creates_keys_and_migrates() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("aibo.db");
        let key = DbKey::generate().expect("key");
        let db = Db::open(&path, &key).expect("open");
        assert_eq!(
            db.schema_version().expect("version"),
            migrations::SCHEMA_VERSION
        );
        assert!(path.exists());
    }

    #[test]
    fn reopening_with_the_same_key_works_and_foreign_keys_stay_on() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("aibo.db");
        let key = DbKey::generate().expect("key");
        drop(Db::open(&path, &key).expect("first open"));

        let db = Db::open(&path, &key).expect("reopen");
        let fk: i64 = db
            .with_conn(|c| Ok(c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?))
            .expect("pragma");
        assert_eq!(fk, 1, "ON DELETE CASCADE in the §12 schema depends on this");
    }

    #[test]
    fn the_wrong_key_is_key_loss_not_corruption() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("aibo.db");
        drop(Db::open(&path, &DbKey::generate().expect("key")).expect("open"));

        let err = Db::open(&path, &DbKey::generate().expect("other key")).expect_err("must fail");
        assert!(
            matches!(err, StoreError::KeyLoss { .. }),
            "expected KeyLoss, got {err:?}"
        );
        assert_eq!(
            err.recovery(),
            Some(crate::error::Recovery::RecoveryCodeOrStartFresh)
        );
    }

    #[test]
    fn rekey_rotates_the_key_and_the_old_one_stops_working() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("aibo.db");
        let old = DbKey::generate().expect("key");
        let new = DbKey::generate().expect("key");

        let db = Db::open(&path, &old).expect("open");
        db.with_conn(|c| {
            c.execute("INSERT INTO settings (key, value) VALUES ('a', 'b')", [])?;
            Ok(())
        })
        .expect("write");
        db.rekey(&new).expect("rekey");
        drop(db);

        assert!(matches!(
            Db::open(&path, &old).expect_err("old key must fail"),
            StoreError::KeyLoss { .. }
        ));
        let db = Db::open(&path, &new).expect("new key opens");
        let value: String = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT value FROM settings WHERE key='a'", [], |r| r.get(0))?)
            })
            .expect("read");
        assert_eq!(value, "b");
    }

    #[test]
    fn archiving_moves_rather_than_deletes() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("aibo.db");
        drop(Db::open(&path, &DbKey::generate().expect("key")).expect("open"));

        let archived = archive_unreadable(&path).expect("archive");
        assert!(!path.exists());
        assert!(archived.exists());
    }

    #[test]
    fn repeated_archives_cannot_replace_each_other() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("aibo.db");
        fs::write(&path, b"first").expect("first file");
        let first = archive_unreadable(&path).expect("first archive");
        fs::write(&path, b"second").expect("second file");
        let second = archive_unreadable(&path).expect("second archive");

        assert_ne!(first, second);
        assert_eq!(fs::read(first).expect("first remains"), b"first");
        assert_eq!(fs::read(second).expect("second remains"), b"second");
    }

    #[test]
    fn runtime_busy_errors_are_normalized_to_locked() {
        let db = Db::open_in_memory().expect("open");
        let error = db
            .with_conn::<()>(|_| {
                Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    None,
                )))
            })
            .expect_err("busy");
        assert!(matches!(error, StoreError::Locked { .. }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn call_runs_on_the_blocking_pool() {
        let db = Db::open_in_memory().expect("open");
        let n: i64 = db
            .call(|c| Ok(c.query_row("SELECT 41 + 1", [], |r| r.get(0))?))
            .await
            .expect("call");
        assert_eq!(n, 42);
    }
}
