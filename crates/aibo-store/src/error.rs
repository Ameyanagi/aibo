//! The store's failure model (§12, §13).
//!
//! Every persistence failure is one variant of [`StoreError`]. The three that
//! §12 calls out by name — "database is locked", corrupt, half-migrated — are
//! variants rather than strings precisely so the UI can offer a *designed*
//! recovery path instead of a stack trace; see [`Recovery`].
//!
//! `aibo-store` is a library, so it uses `thiserror`. Conversion into the
//! domain-level `AiboError` goes through `AiboError::Internal`, which §13 says
//! is never rendered raw.

use std::path::{Path, PathBuf};
use std::time::Duration;

use aibo_core::AiboError;
use thiserror::Error;

/// Convenience alias used throughout this crate.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Why an OS keychain call failed, collapsed from `keyring`'s error enum.
///
/// The `keyring` error type is not stored directly: `keyring` 4.x is a facade
/// over `keyring-core` whose `Error::Ambiguous` variant carries live `Entry`
/// handles, which would drag store errors' auto-trait bounds along with it
/// (§12 note on the 4.x rewrite). Collapsing at the boundary keeps
/// [`StoreError`] plainly `Send + Sync + 'static`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeychainErrorKind {
    /// No such entry. Not an error for `get`, which maps it to `Ok(None)`.
    NoEntry,
    /// A platform length limit was exceeded — on Windows this is the
    /// 2560-byte `CRED_MAX_CREDENTIAL_BLOB_SIZE` cap (§12).
    TooLong,
    /// The store exists but could not be accessed: locked keychain, denied
    /// prompt, no Secret Service session.
    NoAccess,
    /// The stored blob was not in the expected format.
    BadData,
    /// Anything else the platform reported.
    Platform,
}

/// A failed OS keychain operation.
#[derive(Debug, Error)]
#[error("keychain {kind:?} for `{service}/{account}`: {detail}")]
pub struct KeychainError {
    /// Keychain service name.
    pub service: String,
    /// Keychain account name. Never the secret.
    pub account: String,
    /// Collapsed cause.
    pub kind: KeychainErrorKind,
    /// Platform-supplied detail. Never contains the secret.
    pub detail: String,
}

/// Every failure `aibo-store` can produce (§12).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Another connection (usually a second aibo instance) holds the write
    /// lock past `busy_timeout`.
    #[error("database is locked (waited {}ms)", timeout.as_millis())]
    Locked {
        /// How long the busy handler waited before giving up.
        timeout: Duration,
    },

    /// `PRAGMA integrity_check` did not return `ok`, or SQLite reported
    /// `SQLITE_CORRUPT`.
    #[error("database is corrupt: {detail}")]
    Corrupt {
        /// First line of the integrity report.
        detail: String,
    },

    /// The file exists but cannot be decrypted with the key we hold.
    ///
    /// §12 requires this to be **loud**: one sentence, then "restore with
    /// recovery code" or "start fresh". Never silently create a second
    /// database next to an unreadable one.
    #[error("the database at {} cannot be decrypted with the key from credential storage", path.display())]
    KeyLoss {
        /// The unreadable file.
        path: PathBuf,
    },

    /// The file was written by a newer build. Downgrade is not supported;
    /// migrating *down* would drop columns and therefore data.
    #[error("database schema is v{found}, this build supports v{supported}")]
    SchemaTooNew {
        /// Version found in `schema_version`.
        found: i64,
        /// Version this build knows.
        supported: i64,
    },

    /// A migration step failed. The transaction was rolled back; `backup`
    /// names the pre-migration copy if one was taken.
    #[error("migration v{from} → v{to} failed")]
    Migration {
        /// Version before the step.
        from: i64,
        /// Version the step was reaching for.
        to: i64,
        /// Pre-migration file copy, when one exists.
        backup: Option<PathBuf>,
        /// The SQLite failure.
        #[source]
        source: rusqlite::Error,
    },

    /// `schema_version` disagrees with the tables actually present — a crash
    /// between the DDL and the version bump, which the transaction is supposed
    /// to prevent but a torn file can still produce.
    #[error("database is half-migrated at v{at}")]
    HalfMigrated {
        /// The recorded version.
        at: i64,
        /// Pre-migration file copy, when one exists.
        backup: Option<PathBuf>,
    },

    /// An OS keychain operation failed.
    #[error(transparent)]
    Keychain(#[from] KeychainError),

    /// The secret does not fit in the platform credential store (§12).
    ///
    /// Windows caps a raw credential blob at 2560 bytes. Larger values need the
    /// DPAPI-protected file backend.
    #[error("secret for `{account}` is {secret_bytes} bytes, over the {limit}-byte platform cap")]
    SecretTooLarge {
        /// Keychain account name. Never the secret.
        account: String,
        /// Raw secret size.
        secret_bytes: usize,
        /// The platform cap.
        limit: usize,
    },

    /// A recovery code failed to parse or did not decode to 32 bytes.
    #[error("recovery code is not valid: {reason}")]
    RecoveryCode {
        /// Why it was rejected. Never echoes the code.
        reason: &'static str,
    },

    /// An unclassified SQLite failure.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    /// A filesystem failure: backup, archive, restore, export.
    #[error("i/o error on {}: {source}", path.display())]
    Io {
        /// The path being operated on.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// Export serialisation failed.
    #[error("serialisation failed")]
    Json(#[from] serde_json::Error),

    /// A value in the database is not one this build understands, e.g. a
    /// `surface` outside `complete|transform|ask|do`.
    #[error("column `{column}` holds unrecognised value {value:?}")]
    BadColumn {
        /// Column name.
        column: &'static str,
        /// The offending value.
        value: String,
    },

    /// The blocking database task panicked or was cancelled (§6: every task
    /// boundary catches panics, so this is reported rather than propagated).
    #[error("the blocking database task did not complete")]
    BlockingTask,
}

impl StoreError {
    /// Wrap an [`std::io::Error`] with the path it happened on.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        StoreError::Io {
            path: path.into(),
            source,
        }
    }

    /// Classify a raw `rusqlite` error into the variants §12 names.
    ///
    /// A wrong `PRAGMA key` surfaces as `SQLITE_NOTADB` ("file is not a
    /// database") because SQLCipher cannot read the header — that is key loss,
    /// not corruption, and gets a completely different recovery path.
    pub fn from_sqlite(err: rusqlite::Error, path: &Path, busy_timeout: Duration) -> Self {
        use rusqlite::ErrorCode;
        if let rusqlite::Error::SqliteFailure(ffi, ref msg) = err {
            let detail = msg.clone().unwrap_or_else(|| ffi.to_string());
            return match ffi.code {
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => StoreError::Locked {
                    timeout: busy_timeout,
                },
                ErrorCode::NotADatabase => StoreError::KeyLoss {
                    path: path.to_path_buf(),
                },
                ErrorCode::DatabaseCorrupt => StoreError::Corrupt { detail },
                _ => StoreError::Sqlite(err),
            };
        }
        StoreError::Sqlite(err)
    }

    /// The pre-migration backup this failure left behind, if any.
    ///
    /// `Db::open` uses this to restore the file after dropping the connection —
    /// the restore cannot happen while SQLite still holds the handle.
    pub fn migration_backup(&self) -> Option<&Path> {
        match self {
            StoreError::Migration { backup, .. } | StoreError::HalfMigrated { backup, .. } => {
                backup.as_deref()
            }
            _ => None,
        }
    }

    /// The designed recovery path for this failure (§12, §13).
    ///
    /// Returning `None` means "no user-facing recovery" — report it and stop.
    pub fn recovery(&self) -> Option<Recovery> {
        match self {
            StoreError::Locked { .. } => Some(Recovery::RetryLater),
            StoreError::KeyLoss { .. } => Some(Recovery::RecoveryCodeOrStartFresh),
            StoreError::Corrupt { .. } => Some(Recovery::StartFresh),
            StoreError::SchemaTooNew { .. } => Some(Recovery::UpgradeApp),
            StoreError::Migration { backup, .. } | StoreError::HalfMigrated { backup, .. } => {
                if backup.is_some() {
                    Some(Recovery::RestoreBackup)
                } else {
                    Some(Recovery::StartFresh)
                }
            }
            _ => None,
        }
    }
}

/// What the UI is allowed to offer the user for a given [`StoreError`] (§12).
///
/// These are the only four answers. "Start fresh" always *archives* the old
/// file rather than deleting it — an unreadable database is still the user's
/// data, and deleting it forecloses a later recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Recovery {
    /// Another process holds the lock; back off and retry.
    RetryLater,
    /// Offer "restore with recovery code", then "start fresh" (§12 key loss).
    RecoveryCodeOrStartFresh,
    /// Archive the unusable file and create a new database. Settings live in
    /// plaintext TOML, so the app stays configured afterwards (§12).
    StartFresh,
    /// A pre-migration backup exists; put it back.
    RestoreBackup,
    /// The file is from a newer build. Only a newer aibo can open it.
    UpgradeApp,
}

impl From<StoreError> for AiboError {
    /// Store failures reach the domain layer as `Internal`, which §13 renders
    /// as a generic message plus "copy diagnostics" — never raw.
    fn from(err: StoreError) -> Self {
        AiboError::Internal(Box::new(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_error_is_send_sync_static() {
        fn assert<T: Send + Sync + 'static>() {}
        assert::<StoreError>();
    }

    #[test]
    fn key_loss_offers_the_recovery_code_path() {
        let err = StoreError::KeyLoss {
            path: PathBuf::from("/tmp/aibo.db"),
        };
        assert_eq!(err.recovery(), Some(Recovery::RecoveryCodeOrStartFresh));
    }

    #[test]
    fn store_errors_convert_into_internal() {
        let err: AiboError = StoreError::Locked {
            timeout: Duration::from_secs(5),
        }
        .into();
        assert!(matches!(err, AiboError::Internal(_)));
    }
}
