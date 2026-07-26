//! OS keychain access. Note the Windows 2560-byte credential cap (§12).
//!
//! Two things live outside the encrypted database: provider credentials and the
//! database key itself. Both go to the OS keychain — Keychain Services on
//! macOS, Credential Manager on Windows.
//!
//! ## The Windows cap, which decides the shape of this module
//!
//! §12, verbatim: "**Windows Credential Manager caps a secret at 2560 bytes**
//! (`CRED_MAX_CREDENTIAL_BLOB_SIZE`), and `keyring` UTF-16-doubles first — so
//! `set_password` tops out around **1280 ASCII characters**. A 32-byte database
//! key is fine; **a multi-kilobyte OAuth JWT is not**, and OpenAI hit exactly
//! this in Codex. Anything token-shaped needs either DPAPI-encrypted file
//! storage or chunking across entries. Decide before P1, since it changes the
//! storage interface."
//!
//! The decision is made here: **route by size and fall back to a
//! DPAPI-encrypted file**, not chunking. Chunking across entries means a
//! partial write leaves a credential that reassembles into garbage, and there
//! is no transaction across Credential Manager entries to prevent it. One file
//! with one atomic rename has a failure mode a user can understand.
//!
//! The DPAPI call itself is not here: `CryptProtectData` is `unsafe`, and this
//! crate is `#![forbid(unsafe_code)]`. [`Protector`] is the seam; the Windows
//! implementation belongs in `aibo-platform`.
//!
//! SPIKE: S8 — measure a real ChatGPT OAuth token against the 2560-byte cap and
//! confirm the DPAPI file path end-to-end on Windows before P1.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zeroize::Zeroizing;

use crate::error::{KeychainError, KeychainErrorKind, Result, StoreError};
use crate::key::DbKey;

/// Keychain service name. One service, one account per secret.
pub const KEYCHAIN_SERVICE: &str = "com.aibo.aibo";

/// Account holding the SQLCipher database key, hex-encoded.
pub const DB_KEY_ACCOUNT: &str = "database-key";

/// `CRED_MAX_CREDENTIAL_BLOB_SIZE`. The hard Windows limit, in bytes.
pub const WINDOWS_CREDENTIAL_BLOB_MAX_BYTES: usize = 2560;

/// The practical ASCII-character limit once `keyring` UTF-16-doubles: 1280.
pub const WINDOWS_MAX_ASCII_CHARS: usize = WINDOWS_CREDENTIAL_BLOB_MAX_BYTES / 2;

/// The keychain account name for a provider's credential.
pub fn provider_account(provider: &str) -> String {
    format!("provider:{provider}")
}

/// Whether a secret fits in Windows Credential Manager.
///
/// Counts UTF-16 code units and doubles, which is what the platform counts —
/// not `str::len()`. A Japanese token is over the cap sooner than its UTF-8
/// byte length suggests, and an astral-plane one sooner still.
pub fn fits_in_credential_manager(secret: &str) -> bool {
    utf16_bytes(secret) <= WINDOWS_CREDENTIAL_BLOB_MAX_BYTES
}

/// The size the platform will see, in bytes.
pub fn utf16_bytes(secret: &str) -> usize {
    secret.encode_utf16().count() * 2
}

/// Read/write/delete a named secret.
///
/// `get` returns `Ok(None)` for a missing entry: "the user has not configured
/// this provider" is a state, not a failure (§13).
pub trait SecretStore: Send + Sync {
    /// Fetch a secret.
    fn get(&self, account: &str) -> Result<Option<Zeroizing<String>>>;
    /// Store a secret, replacing any existing value.
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    /// Remove a secret. Removing a missing secret succeeds.
    fn delete(&self, account: &str) -> Result<()>;
}

/// Encrypt and decrypt bytes with an OS-bound facility.
///
/// On Windows this is DPAPI (`CryptProtectData` / `CryptUnprotectData`) scoped
/// to the current user, which is what makes the fallback file no weaker than
/// Credential Manager. Both calls are `unsafe`, so the implementation lives in
/// `aibo-platform` and is injected here.
///
/// SPIKE: S8 — confirm DPAPI round-trips a multi-kilobyte token, and decide
/// whether to pass an entropy parameter (a second factor kept in Credential
/// Manager, which does fit).
pub trait Protector: Send + Sync {
    /// Encrypt. The output is opaque and machine/user-bound.
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>>;
    /// Decrypt something [`Protector::protect`] produced.
    fn unprotect(&self, ciphertext: &[u8]) -> Result<Vec<u8>>;
}

// ---------------------------------------------------------------------------
// Keychain
// ---------------------------------------------------------------------------

/// The OS keychain, via `keyring`.
#[derive(Debug, Clone)]
pub struct Keychain {
    service: String,
}

impl Default for Keychain {
    fn default() -> Self {
        Self::new(KEYCHAIN_SERVICE)
    }
}

impl Keychain {
    /// A keychain scoped to a service name.
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// The service name entries are filed under.
    pub fn service(&self) -> &str {
        &self.service
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, account)
            .map_err(|e| map_keyring(e, &self.service, account).into())
    }
}

impl SecretStore for Keychain {
    fn get(&self, account: &str) -> Result<Option<Zeroizing<String>>> {
        match self.entry(account)?.get_password() {
            Ok(secret) => Ok(Some(Zeroizing::new(secret))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_keyring(e, &self.service, account).into()),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        // Fail with the specific error rather than letting the platform report
        // a generic one — this is the exact case §12 says to design for.
        if !fits_in_credential_manager(secret) {
            return Err(StoreError::SecretTooLarge {
                account: account.to_owned(),
                utf16_bytes: utf16_bytes(secret),
                limit: WINDOWS_CREDENTIAL_BLOB_MAX_BYTES,
            });
        }
        self.entry(account)?
            .set_password(secret)
            .map_err(|e| map_keyring(e, &self.service, account).into())
    }

    fn delete(&self, account: &str) -> Result<()> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_keyring(e, &self.service, account).into()),
        }
    }
}

fn map_keyring(err: keyring::Error, service: &str, account: &str) -> KeychainError {
    let kind = match &err {
        keyring::Error::NoEntry => KeychainErrorKind::NoEntry,
        keyring::Error::TooLong(_, _) => KeychainErrorKind::TooLong,
        keyring::Error::NoStorageAccess(_) => KeychainErrorKind::NoAccess,
        keyring::Error::BadEncoding(_)
        | keyring::Error::BadDataFormat(_, _)
        | keyring::Error::BadStoreFormat(_) => KeychainErrorKind::BadData,
        _ => KeychainErrorKind::Platform,
    };
    KeychainError {
        service: service.to_owned(),
        account: account.to_owned(),
        kind,
        // `keyring`'s Display renders metadata only, never the secret.
        detail: err.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Protected file fallback
// ---------------------------------------------------------------------------

/// One protected file per secret, for payloads over the Windows cap.
///
/// Writes go to a temporary file and are renamed into place, so a crash leaves
/// either the old secret or the new one and never half of either.
pub struct FileSecretStore {
    dir: PathBuf,
    protector: Arc<dyn Protector>,
}

impl std::fmt::Debug for FileSecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSecretStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl FileSecretStore {
    /// A store writing protected files under `dir`.
    pub fn new(dir: impl Into<PathBuf>, protector: Arc<dyn Protector>) -> Self {
        Self {
            dir: dir.into(),
            protector,
        }
    }

    /// Where a given account's file lives.
    ///
    /// Account names are sanitised to `[A-Za-z0-9._-]`, so a provider id can
    /// never escape the directory.
    pub fn path_for(&self, account: &str) -> PathBuf {
        let safe: String = account
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.secret"))
    }
}

impl SecretStore for FileSecretStore {
    fn get(&self, account: &str) -> Result<Option<Zeroizing<String>>> {
        let path = self.path_for(account);
        let ciphertext = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::io(&path, e)),
        };
        let plaintext = Zeroizing::new(self.protector.unprotect(&ciphertext)?);
        let text = String::from_utf8(plaintext.to_vec()).map_err(|_| {
            StoreError::Keychain(KeychainError {
                service: self.dir.display().to_string(),
                account: account.to_owned(),
                kind: KeychainErrorKind::BadData,
                detail: "protected file did not decrypt to UTF-8".to_owned(),
            })
        })?;
        Ok(Some(Zeroizing::new(text)))
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        fs::create_dir_all(&self.dir).map_err(|e| StoreError::io(&self.dir, e))?;
        let ciphertext = self.protector.protect(secret.as_bytes())?;
        let path = self.path_for(account);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &ciphertext).map_err(|e| StoreError::io(&tmp, e))?;
        restrict_permissions(&tmp)?;
        fs::rename(&tmp, &path).map_err(|e| StoreError::io(&path, e))?;
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<()> {
        let path = self.path_for(account);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::io(&path, e)),
        }
    }
}

/// Owner-only permissions on Unix. On Windows the DPAPI blob is already
/// user-bound, and ACLs are inherited from the app-support directory.
fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| StoreError::io(path, e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Size-routing façade
// ---------------------------------------------------------------------------

/// The storage interface the rest of aibo uses.
///
/// Small secrets (the 32-byte database key, an API key) go to the OS keychain.
/// Anything over the Windows cap goes to the protected file store. `get` checks
/// the keychain first and then the file store, so a token that grew past the
/// cap on a later refresh is still found.
pub struct SecretStorage {
    keychain: Box<dyn SecretStore>,
    oversize: Option<Box<dyn SecretStore>>,
}

impl std::fmt::Debug for SecretStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStorage")
            .field("oversize_configured", &self.oversize.is_some())
            .finish_non_exhaustive()
    }
}

impl SecretStorage {
    /// Keychain only. Correct on macOS, where Keychain Services has no
    /// comparable cap.
    pub fn keychain_only(keychain: impl SecretStore + 'static) -> Self {
        Self {
            keychain: Box::new(keychain),
            oversize: None,
        }
    }

    /// Keychain plus a fallback for oversize secrets. This is the Windows
    /// configuration, with a DPAPI-backed [`FileSecretStore`].
    pub fn with_oversize(
        keychain: impl SecretStore + 'static,
        oversize: impl SecretStore + 'static,
    ) -> Self {
        Self {
            keychain: Box::new(keychain),
            oversize: Some(Box::new(oversize)),
        }
    }

    /// Fetch a secret from wherever it was put.
    pub fn get(&self, account: &str) -> Result<Option<Zeroizing<String>>> {
        if let Some(found) = self.keychain.get(account)? {
            return Ok(Some(found));
        }
        match &self.oversize {
            Some(store) => store.get(account),
            None => Ok(None),
        }
    }

    /// Store a secret, choosing the backend by size.
    ///
    /// The other backend's copy is removed, so a token that shrank below the
    /// cap (or grew above it) does not leave a stale second copy for `get` to
    /// resurrect later.
    pub fn set(&self, account: &str, secret: &str) -> Result<()> {
        if fits_in_credential_manager(secret) {
            self.keychain.set(account, secret)?;
            if let Some(store) = &self.oversize {
                store.delete(account)?;
            }
            return Ok(());
        }
        match &self.oversize {
            Some(store) => {
                store.set(account, secret)?;
                self.keychain.delete(account)
            }
            // Refuse rather than silently truncating or writing plaintext.
            None => Err(StoreError::SecretTooLarge {
                account: account.to_owned(),
                utf16_bytes: utf16_bytes(secret),
                limit: WINDOWS_CREDENTIAL_BLOB_MAX_BYTES,
            }),
        }
    }

    /// Remove a secret from both backends.
    pub fn delete(&self, account: &str) -> Result<()> {
        self.keychain.delete(account)?;
        if let Some(store) = &self.oversize {
            store.delete(account)?;
        }
        Ok(())
    }

    /// The SQLCipher key, or `None` if this machine has never had one.
    pub fn db_key(&self) -> Result<Option<DbKey>> {
        match self.get(DB_KEY_ACCOUNT)? {
            Some(hex) => Ok(Some(DbKey::from_hex(&hex)?)),
            None => Ok(None),
        }
    }

    /// Store the SQLCipher key. 64 hex characters: 128 bytes as UTF-16, well
    /// inside the Windows cap.
    pub fn set_db_key(&self, key: &DbKey) -> Result<()> {
        self.set(DB_KEY_ACCOUNT, &key.to_hex())
    }

    /// Fetch the database key, generating and storing one on first run.
    ///
    /// The `bool` is `true` when a key was just created — the caller shows the
    /// recovery code exactly then, and never again (§12).
    pub fn db_key_or_create(&self) -> Result<(DbKey, bool)> {
        if let Some(key) = self.db_key()? {
            return Ok((key, false));
        }
        let key = DbKey::generate()?;
        self.set_db_key(&key)?;
        Ok((key, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An in-process stand-in for the OS keychain. It enforces the Windows cap
    /// on every platform, so the cap is exercised by CI everywhere rather than
    /// only on the one OS where it is real.
    #[derive(Default)]
    struct FakeKeychain {
        entries: Mutex<HashMap<String, String>>,
        enforce_cap: bool,
    }

    impl FakeKeychain {
        fn capped() -> Self {
            Self {
                entries: Mutex::new(HashMap::new()),
                enforce_cap: true,
            }
        }
    }

    impl SecretStore for FakeKeychain {
        fn get(&self, account: &str) -> Result<Option<Zeroizing<String>>> {
            Ok(self
                .entries
                .lock()
                .expect("lock")
                .get(account)
                .cloned()
                .map(Zeroizing::new))
        }
        fn set(&self, account: &str, secret: &str) -> Result<()> {
            if self.enforce_cap && !fits_in_credential_manager(secret) {
                return Err(StoreError::SecretTooLarge {
                    account: account.to_owned(),
                    utf16_bytes: utf16_bytes(secret),
                    limit: WINDOWS_CREDENTIAL_BLOB_MAX_BYTES,
                });
            }
            self.entries
                .lock()
                .expect("lock")
                .insert(account.to_owned(), secret.to_owned());
            Ok(())
        }
        fn delete(&self, account: &str) -> Result<()> {
            self.entries.lock().expect("lock").remove(account);
            Ok(())
        }
    }

    /// Stands in for DPAPI. It reverses the bytes — enough to prove the round
    /// trip goes through the [`Protector`] seam, and obviously not encryption,
    /// which is the point of keeping the real one in `aibo-platform`.
    struct ReversingProtector;

    impl Protector for ReversingProtector {
        fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
            Ok(plaintext.iter().rev().copied().collect())
        }
        fn unprotect(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
            Ok(ciphertext.iter().rev().copied().collect())
        }
    }

    #[test]
    fn the_documented_cap_matches_the_platform_constant() {
        assert_eq!(WINDOWS_CREDENTIAL_BLOB_MAX_BYTES, 2560);
        assert_eq!(WINDOWS_MAX_ASCII_CHARS, 1280);
        assert!(fits_in_credential_manager(&"a".repeat(1280)));
        assert!(!fits_in_credential_manager(&"a".repeat(1281)));
    }

    #[test]
    fn utf16_doubling_is_measured_not_assumed() {
        // Three UTF-8 bytes, one UTF-16 code unit, two bytes on Windows.
        assert_eq!(utf16_bytes("日"), 2);
        // Astral plane: a surrogate pair, four bytes on Windows.
        assert_eq!(utf16_bytes("𝄞"), 4);
        assert!(fits_in_credential_manager(&"日".repeat(1280)));
        assert!(!fits_in_credential_manager(&"𝄞".repeat(641)));
    }

    #[test]
    fn a_database_key_fits_comfortably() {
        let key = DbKey::generate().expect("key");
        assert!(fits_in_credential_manager(&key.to_hex()));
        assert_eq!(utf16_bytes(&key.to_hex()), 128);
    }

    #[test]
    fn an_oauth_sized_token_goes_to_the_file_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = SecretStorage::with_oversize(
            FakeKeychain::capped(),
            FileSecretStore::new(dir.path(), Arc::new(ReversingProtector)),
        );

        // A ~4 KB JWT: the shape §12 says Credential Manager cannot hold.
        let token = format!("eyJhbGciOiJSUzI1NiJ9.{}.sig", "x".repeat(4000));
        assert!(!fits_in_credential_manager(&token));

        let account = provider_account("chatgpt");
        storage.set(&account, &token).expect("store");
        let read = storage.get(&account).expect("read").expect("present");
        assert_eq!(*read, token);
    }

    #[test]
    fn without_a_fallback_an_oversize_secret_is_refused_not_truncated() {
        let storage = SecretStorage::keychain_only(FakeKeychain::capped());
        let err = storage
            .set("chatgpt", &"x".repeat(4000))
            .expect_err("must refuse");
        assert!(matches!(err, StoreError::SecretTooLarge { .. }));
    }

    #[test]
    fn shrinking_past_the_cap_does_not_leave_a_stale_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = SecretStorage::with_oversize(
            FakeKeychain::capped(),
            FileSecretStore::new(dir.path(), Arc::new(ReversingProtector)),
        );

        storage.set("token", &"x".repeat(4000)).expect("big");
        storage.set("token", "small").expect("small");
        assert_eq!(
            *storage.get("token").expect("read").expect("present"),
            "small"
        );

        storage.delete("token").expect("delete");
        assert!(storage.get("token").expect("read").is_none());
    }

    #[test]
    fn the_database_key_round_trips_through_storage() {
        let storage = SecretStorage::keychain_only(FakeKeychain::default());
        let (key, created) = storage.db_key_or_create().expect("create");
        assert!(created);
        let (again, created_again) = storage.db_key_or_create().expect("load");
        assert!(!created_again);
        assert_eq!(key, again);
    }

    #[test]
    fn account_names_cannot_escape_the_secret_directory() {
        let store = FileSecretStore::new("/tmp/aibo-secrets", Arc::new(ReversingProtector));
        assert_eq!(
            store.path_for("../../etc/passwd"),
            PathBuf::from("/tmp/aibo-secrets/.._.._etc_passwd.secret")
        );
    }
}
