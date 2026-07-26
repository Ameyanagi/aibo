//! `aibo-store` — local-only encrypted persistence (§12).
//!
//! Whole-database encryption via SQLCipher, so FTS5 indexes live *inside* the
//! encrypted file and history search works without a plaintext index. All
//! SQLite work runs on `spawn_blocking`, never on the UI thread (§6).
//!
//! Non-negotiables from §12: `PRAGMA foreign_keys=ON` (the schema depends on
//! `ON DELETE CASCADE`), migrations inside a transaction with a file backup
//! taken first, an integrity check on open, and designed paths for "locked",
//! "corrupt" and "half-migrated".
//!
//! # Opening a database
//!
//! ```no_run
//! use aibo_store::{Db, Keychain, SecretStorage};
//!
//! # fn main() -> aibo_store::Result<()> {
//! let secrets = SecretStorage::keychain_only(Keychain::default());
//! let (key, is_new) = secrets.db_key_or_create()?;
//! if is_new {
//!     // Show the recovery code exactly once, at setup (§12).
//!     let _code = key.to_recovery_code();
//! }
//! let db = Db::open("/path/to/aibo.db", &key)?;
//! # let _ = db;
//! # Ok(())
//! # }
//! ```
//!
//! When `Db::open` returns [`StoreError::KeyLoss`], §12 requires the failure to
//! be loud and the recovery designed rather than improvised: see
//! [`Recovery`], [`DbKey::from_recovery_code`] and [`db::archive_unreadable`].
//!
//! # What is deliberately *not* stored here
//!
//! - Provider credentials — OS keychain, see [`secrets`].
//! - Settings — plaintext TOML, so the app is still configured after a
//!   "start fresh" (§12).
//! - Concealed clipboard content — never written at all, see [`clipboard`].

#![forbid(unsafe_code)]

use std::time::{SystemTime, UNIX_EPOCH};

pub mod clipboard;
pub mod codec;
pub mod db;
pub mod error;
pub mod history;
pub mod key;
pub mod migrations;
pub mod search;
/// Named `secrets` rather than `keyring` so the module does not shadow the
/// `keyring` crate at the crate root.
pub mod secrets;

pub use db::{Db, IntegrityCheck};
pub use error::{KeychainError, KeychainErrorKind, Recovery, Result, StoreError};
pub use key::DbKey;
pub use migrations::SCHEMA_VERSION;
pub use secrets::{Keychain, Protector, SecretStorage, SecretStore};

/// The current time, as the unix seconds every `*_at` column in §12 holds.
///
/// Clamped at the epoch rather than panicking: a machine with a clock set
/// before 1970 should produce a wrong timestamp, not a dead tray icon (§6).
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
