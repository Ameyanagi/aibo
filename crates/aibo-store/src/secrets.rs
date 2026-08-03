//! Credential storage for local files and legacy OS credential stores (§12).
//!
//! Two things live outside the encrypted database: provider credentials and the
//! database key itself. Production wiring stores both in credential files:
//! owner-only files on macOS and DPAPI-encrypted files on Windows.
//!
//! ## Legacy Windows Credential Manager support
//!
//! Windows Credential Manager caps a secret at 2560 bytes
//! (`CRED_MAX_CREDENTIAL_BLOB_SIZE`). Older `keyring::set_password` wiring
//! UTF-16-expanded strings first, reducing the practical ASCII limit to 1280.
//! This module uses `keyring` 4's raw-byte API instead: the measured 1652-byte
//! OAuth token fits, while a genuinely larger blob is routed to an injected
//! protected-file backend.
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
//! The Windows implementation lives in `aibo-platform` and includes a native
//! multi-kilobyte round-trip test.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
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

/// The legacy ASCII-character limit when using `keyring::set_password`.
pub const WINDOWS_MAX_ASCII_CHARS: usize = WINDOWS_CREDENTIAL_BLOB_MAX_BYTES / 2;

/// Maximum protected-file payload accepted on read.
///
/// Provider credentials should be measured in kilobytes. A bounded read keeps
/// a corrupt or attacker-replaced file from causing an unbounded allocation.
const MAX_PROTECTED_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Identifies this crate's protected-file envelope.
const PROTECTED_FILE_MAGIC: &[u8] = b"AIBO-SECRET\x00\x01";

/// The keychain account name for a provider's credential.
pub fn provider_account(provider: &str) -> String {
    format!("provider:{provider}")
}

/// Whether a secret fit through the legacy password-oriented keyring API.
///
/// New writes use [`raw_fits_in_credential_manager`] instead.
pub fn fits_in_credential_manager(secret: &str) -> bool {
    utf16_bytes(secret) <= WINDOWS_CREDENTIAL_BLOB_MAX_BYTES
}

/// Whether raw secret bytes fit in a Windows Credential Manager blob.
///
/// `keyring` 4 exposes a byte-oriented API, so new writes no longer need the
/// legacy UTF-16 doubling assumed by [`fits_in_credential_manager`].
pub fn raw_fits_in_credential_manager(secret: &[u8]) -> bool {
    secret.len() <= WINDOWS_CREDENTIAL_BLOB_MAX_BYTES
}

/// The byte size of a string encoded as UTF-16.
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
pub trait Protector: Send + Sync {
    /// Encrypt. The output is opaque and machine/user-bound.
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>>;
    /// Decrypt something [`Protector::protect`] produced.
    fn unprotect(&self, ciphertext: &[u8]) -> Result<Vec<u8>>;
}

/// No application-level encryption. Owner-only file permissions are the whole
/// protection.
///
/// This is suitable only when the caller deliberately accepts that every
/// process running as the same OS user can read the credential. Windows
/// production uses a DPAPI-backed [`Protector`] instead.
#[derive(Debug, Clone, Copy)]
pub struct PlaintextProtector {
    _private: (),
}

/// Explicit acknowledgement required to construct an owner-only plaintext
/// store through the supported API.
///
/// The marker makes plaintext storage difficult to enable accidentally in
/// wiring and straightforward to find in code review.
#[derive(Debug, Clone, Copy)]
pub struct OwnerOnlyPlaintext {
    _private: (),
}

impl OwnerOnlyPlaintext {
    /// Acknowledge that credentials will be readable by the current user and
    /// any process able to access their account.
    #[must_use]
    pub const fn acknowledge_risk() -> Self {
        Self { _private: () }
    }
}

impl PlaintextProtector {
    /// Construct the plaintext protector after an explicit risk
    /// acknowledgement.
    #[must_use]
    pub const fn owner_only(_acknowledgement: OwnerOnlyPlaintext) -> Self {
        Self { _private: () }
    }
}

impl Protector for PlaintextProtector {
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        Ok(plaintext.to_vec())
    }

    fn unprotect(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        Ok(ciphertext.to_vec())
    }
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
        match self.entry(account)?.get_secret() {
            Ok(secret) => {
                let secret = Zeroizing::new(secret);
                let text = String::from_utf8(secret.to_vec()).map_err(|_| {
                    StoreError::Keychain(KeychainError {
                        service: self.service.clone(),
                        account: account.to_owned(),
                        kind: KeychainErrorKind::BadData,
                        detail: "keychain secret is not UTF-8".to_owned(),
                    })
                })?;
                Ok(Some(Zeroizing::new(text)))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_keyring(e, &self.service, account).into()),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        // Credential Manager applies its cap to the raw blob. `keyring` 4's
        // byte-oriented API avoids the older set_password UTF-16 expansion.
        #[cfg(windows)]
        if !raw_fits_in_credential_manager(secret.as_bytes()) {
            return Err(StoreError::SecretTooLarge {
                account: account.to_owned(),
                secret_bytes: secret.len(),
                limit: WINDOWS_CREDENTIAL_BLOB_MAX_BYTES,
            });
        }
        self.entry(account)?
            .set_secret(secret.as_bytes())
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
    /// The UTF-8 account bytes are hex encoded. Unlike lossy sanitisation this
    /// is both traversal-safe and collision-free (`a/b` and `a_b` cannot name
    /// the same credential).
    pub fn path_for(&self, account: &str) -> PathBuf {
        let mut encoded = String::with_capacity(account.len() * 2);
        for byte in account.as_bytes() {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
        }
        self.dir.join(format!("v2-{encoded}.secret"))
    }

    /// The path used by pre-v2 builds. It is consulted only for migration.
    fn legacy_path_for(&self, account: &str) -> Option<PathBuf> {
        // The old replacement scheme was not injective. Only consult it for
        // account namespaces whose accepted spelling maps one-to-one. In
        // particular, never let `provider:a/b` read or delete
        // `provider:a_b`'s credential.
        let safe_account = account == DB_KEY_ACCOUNT
            || account.strip_prefix("provider:").is_some_and(|provider| {
                !provider.is_empty()
                    && provider.chars().all(|character| {
                        character.is_ascii_alphanumeric()
                            || character == '.'
                            || character == '-'
                            || character == '_'
                    })
            });
        if !safe_account {
            return None;
        }
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
        Some(self.dir.join(format!("{safe}.secret")))
    }

    fn validate_account(&self, account: &str) -> Result<()> {
        // Most filesystems cap a component at 255 bytes. `v2-` + hex + suffix
        // consumes 10 bytes, leaving room for 120 account bytes.
        if account.is_empty() || account.len() > 120 {
            return Err(StoreError::io(
                self.path_for(account),
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "secret account must contain 1 to 120 UTF-8 bytes",
                ),
            ));
        }
        Ok(())
    }

    fn read_file(&self, path: &Path, account: &str) -> Result<Option<Zeroizing<String>>> {
        let mut file = match open_read_nofollow(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::io(path, e)),
        };
        let metadata = file.metadata().map_err(|e| StoreError::io(path, e))?;
        if !metadata.is_file() {
            return Err(StoreError::io(
                path,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "secret path is not a regular file",
                ),
            ));
        }
        if metadata.len() > MAX_PROTECTED_FILE_BYTES {
            return Err(StoreError::io(
                path,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "protected secret file is too large",
                ),
            ));
        }

        let mut stored = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
        (&mut file)
            .take(MAX_PROTECTED_FILE_BYTES + 1)
            .read_to_end(&mut stored)
            .map_err(|e| StoreError::io(path, e))?;
        if stored.len() as u64 > MAX_PROTECTED_FILE_BYTES {
            return Err(StoreError::io(
                path,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "protected secret file grew beyond its size limit",
                ),
            ));
        }
        let ciphertext = stored
            .strip_prefix(PROTECTED_FILE_MAGIC)
            // Pre-v2 files contained only protector output.
            .unwrap_or(&stored);
        let plaintext = Zeroizing::new(self.protector.unprotect(ciphertext)?);
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

    fn remove_if_present(&self, path: &Path) -> Result<bool> {
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StoreError::io(path, e)),
        }
    }
}

impl SecretStore for FileSecretStore {
    fn get(&self, account: &str) -> Result<Option<Zeroizing<String>>> {
        self.validate_account(account)?;
        if !validate_existing_directory(&self.dir)? {
            return Ok(None);
        }
        let path = self.path_for(account);
        if let Some(secret) = self.read_file(&path, account)? {
            return Ok(Some(secret));
        }
        match self.legacy_path_for(account) {
            Some(legacy) => self.read_file(&legacy, account),
            None => Ok(None),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        self.validate_account(account)?;
        ensure_secure_directory(&self.dir)?;
        let ciphertext = Zeroizing::new(self.protector.protect(secret.as_bytes())?);
        let path = self.path_for(account);
        if ciphertext.len() as u64
            > MAX_PROTECTED_FILE_BYTES.saturating_sub(PROTECTED_FILE_MAGIC.len() as u64)
        {
            return Err(StoreError::io(
                &path,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "protected secret exceeds the file-store size limit",
                ),
            ));
        }
        let (tmp, mut file) = create_random_temp_file(&path)?;
        let write_result: Result<()> = (|| {
            file.write_all(PROTECTED_FILE_MAGIC)
                .and_then(|()| file.write_all(&ciphertext))
                .and_then(|()| file.sync_all())
                .map_err(|e| StoreError::io(&tmp, e))?;
            drop(file);
            fs::rename(&tmp, &path).map_err(|e| StoreError::io(&path, e))?;
            sync_directory(&self.dir)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        write_result?;

        // A successful v2 write is the migration boundary. Delete the legacy
        // name only after the new file and directory entry are durable.
        if let Some(legacy) = self.legacy_path_for(account)
            && self.remove_if_present(&legacy)?
        {
            sync_directory(&self.dir)?;
        }
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<()> {
        self.validate_account(account)?;
        if !validate_existing_directory(&self.dir)? {
            return Ok(());
        }
        let mut removed = self.remove_if_present(&self.path_for(account))?;
        if let Some(legacy) = self.legacy_path_for(account) {
            removed |= self.remove_if_present(&legacy)?;
        }
        if removed {
            sync_directory(&self.dir)?;
        }
        Ok(())
    }
}

fn ensure_secure_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path).map_err(|e| StoreError::io(path, e))?;

        let metadata = fs::symlink_metadata(path).map_err(|e| StoreError::io(path, e))?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(StoreError::io(
                path,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "secret directory is not a real directory",
                ),
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|e| StoreError::io(path, e))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path).map_err(|e| StoreError::io(path, e))?;
        let metadata = fs::symlink_metadata(path).map_err(|e| StoreError::io(path, e))?;
        if !metadata.is_dir() {
            return Err(StoreError::io(
                path,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "secret path is not a directory",
                ),
            ));
        }
    }
    Ok(())
}

fn validate_existing_directory(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(StoreError::io(path, e)),
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(StoreError::io(
            path,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "secret directory is not a real directory",
            ),
        ));
    }
    Ok(true)
}

fn open_read_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow(&mut options);
    options.open(path)
}

fn create_random_temp_file(destination: &Path) -> Result<(PathBuf, File)> {
    let parent = destination.parent().ok_or_else(|| {
        StoreError::io(
            destination,
            io::Error::new(io::ErrorKind::InvalidInput, "secret path has no parent"),
        )
    })?;
    let stem = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("secret");

    for _ in 0..32 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|e| {
            StoreError::io(
                destination,
                io::Error::other(format!("could not randomize temporary filename: {e}")),
            )
        })?;
        let mut suffix = String::with_capacity(random.len() * 2);
        for byte in random {
            use std::fmt::Write as _;
            write!(&mut suffix, "{byte:02x}").expect("writing to String cannot fail");
        }
        let path = parent.join(format!(".{stem}.tmp-{suffix}"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        add_create_permissions(&mut options);
        add_nofollow(&mut options);
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(StoreError::io(&path, e)),
        }
    }

    Err(StoreError::io(
        destination,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique temporary secret file",
        ),
    ))
}

fn add_create_permissions(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    {
        let _ = options;
    }
}

fn add_nofollow(options: &mut OpenOptions) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Linux O_NOFOLLOW.
        options.custom_flags(0x20_000);
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Darwin O_NOFOLLOW.
        options.custom_flags(0x100);
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    {
        let _ = options;
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|dir| dir.sync_all())
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

/// Result of moving one secret between storage backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// The source did not contain this account.
    Missing,
    /// The destination was written, verified, then the source was removed.
    Migrated,
    /// Both stores held the same secret, so the redundant source was removed.
    RedundantSourceRemoved,
    /// Both stores held different secrets. Neither was changed.
    Conflict,
}

/// Safely move one account from `source` to `destination`.
///
/// The source is never deleted until the destination has returned the exact
/// value from a read-after-write. A conflicting destination is preserved and
/// reported for the caller to resolve rather than overwritten.
pub fn migrate_secret(
    account: &str,
    source: &dyn SecretStore,
    destination: &dyn SecretStore,
) -> Result<MigrationOutcome> {
    let Some(source_secret) = source.get(account)? else {
        return Ok(MigrationOutcome::Missing);
    };
    if let Some(destination_secret) = destination.get(account)? {
        if *destination_secret == *source_secret {
            source.delete(account)?;
            return Ok(MigrationOutcome::RedundantSourceRemoved);
        }
        return Ok(MigrationOutcome::Conflict);
    }

    destination.set(account, &source_secret)?;
    match destination.get(account)? {
        Some(stored) if *stored == *source_secret => {
            source.delete(account)?;
            Ok(MigrationOutcome::Migrated)
        }
        _ => Err(StoreError::Keychain(KeychainError {
            service: "secret-migration".to_owned(),
            account: account.to_owned(),
            kind: KeychainErrorKind::BadData,
            detail: "destination failed read-after-write verification".to_owned(),
        })),
    }
}

/// The storage interface the rest of aibo uses.
///
/// The primary backend can be an OS credential store or a file store. An
/// optional secondary backend supports the legacy Windows size-routing mode.
pub struct SecretStorage {
    primary: Box<dyn SecretStore>,
    secondary: Option<Box<dyn SecretStore>>,
    /// Raw byte limit used when this façade has a secondary backend.
    primary_limit: Option<usize>,
}

impl std::fmt::Debug for SecretStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStorage")
            .field("secondary_configured", &self.secondary.is_some())
            .finish_non_exhaustive()
    }
}

impl SecretStorage {
    /// Owner-only plaintext credential files.
    ///
    /// The directory is forced to `0700` and each file to `0600` on Unix.
    /// Contents are not application-encrypted.
    pub fn owner_only_plaintext_files(
        dir: impl Into<PathBuf>,
        acknowledgement: OwnerOnlyPlaintext,
    ) -> Self {
        tracing::warn!("using owner-only plaintext credential files");
        let protector = PlaintextProtector::owner_only(acknowledgement);
        let store = FileSecretStore::new(dir, Arc::new(protector));
        Self::single_backend(store)
    }

    /// One primary backend with no façade-level size routing.
    pub fn single_backend(primary: impl SecretStore + 'static) -> Self {
        Self {
            primary: Box::new(primary),
            secondary: None,
            primary_limit: None,
        }
    }

    /// The platform's OS-backed credential store.
    pub fn os_keychain() -> Self {
        Self::single_backend(Keychain::default())
    }

    /// Keychain plus a fallback for oversize secrets. This is the Windows
    /// legacy configuration, with a DPAPI-backed [`FileSecretStore`].
    pub fn with_oversize(
        primary: impl SecretStore + 'static,
        oversize: impl SecretStore + 'static,
    ) -> Self {
        Self {
            primary: Box::new(primary),
            secondary: Some(Box::new(oversize)),
            primary_limit: Some(WINDOWS_CREDENTIAL_BLOB_MAX_BYTES),
        }
    }

    /// Fetch a secret from wherever it was put.
    pub fn get(&self, account: &str) -> Result<Option<Zeroizing<String>>> {
        let primary = self.primary.get(account)?;
        let Some(secondary_store) = &self.secondary else {
            return Ok(primary);
        };
        let secondary = secondary_store.get(account)?;
        match (primary, secondary) {
            (None, None) => Ok(None),
            (Some(value), None) | (None, Some(value)) => Ok(Some(value)),
            (Some(primary), Some(secondary)) if *primary == *secondary => Ok(Some(primary)),
            (Some(_), Some(_)) => Err(StoreError::Keychain(KeychainError {
                service: "secret-storage".to_owned(),
                account: account.to_owned(),
                kind: KeychainErrorKind::BadData,
                detail: "conflicting values exist in primary and secondary storage".to_owned(),
            })),
        }
    }

    /// Store a secret, choosing the backend by size.
    ///
    /// The other backend's copy is removed, so a token that shrank below the
    /// cap (or grew above it) does not leave a stale second copy for `get` to
    /// resurrect later.
    pub fn set(&self, account: &str, secret: &str) -> Result<()> {
        let Some(primary_limit) = self.primary_limit else {
            return self.primary.set(account, secret);
        };
        if secret.len() <= primary_limit {
            self.primary.set(account, secret)?;
            if let Some(store) = &self.secondary {
                store.delete(account)?;
            }
            return Ok(());
        }
        match &self.secondary {
            Some(store) => {
                store.set(account, secret)?;
                self.primary.delete(account)
            }
            // Refuse rather than silently truncating or writing plaintext.
            None => Err(StoreError::SecretTooLarge {
                account: account.to_owned(),
                secret_bytes: secret.len(),
                limit: primary_limit,
            }),
        }
    }

    /// Remove a secret from both backends.
    pub fn delete(&self, account: &str) -> Result<()> {
        self.primary.delete(account)?;
        if let Some(store) = &self.secondary {
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

    /// Migrate an account into this configured storage, with read-after-write
    /// verification before the old copy is removed.
    pub fn migrate_from(
        &self,
        account: &str,
        source: &dyn SecretStore,
    ) -> Result<MigrationOutcome> {
        migrate_secret(account, source, self)
    }
}

impl SecretStore for SecretStorage {
    fn get(&self, account: &str) -> Result<Option<Zeroizing<String>>> {
        SecretStorage::get(self, account)
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        SecretStorage::set(self, account, secret)
    }

    fn delete(&self, account: &str) -> Result<()> {
        SecretStorage::delete(self, account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

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
            if self.enforce_cap && !raw_fits_in_credential_manager(secret.as_bytes()) {
                return Err(StoreError::SecretTooLarge {
                    account: account.to_owned(),
                    secret_bytes: secret.len(),
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

    #[derive(Default)]
    struct LyingDestination {
        writes: Mutex<usize>,
    }

    #[derive(Clone, Default)]
    struct DeleteFailingStore {
        entries: Arc<Mutex<HashMap<String, String>>>,
        fail_delete: Arc<AtomicBool>,
    }

    impl SecretStore for DeleteFailingStore {
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
            self.entries
                .lock()
                .expect("lock")
                .insert(account.to_owned(), secret.to_owned());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<()> {
            if self.fail_delete.load(Ordering::SeqCst) {
                return Err(StoreError::io(
                    "<test-delete>",
                    io::Error::other("injected delete failure"),
                ));
            }
            self.entries.lock().expect("lock").remove(account);
            Ok(())
        }
    }

    impl SecretStore for LyingDestination {
        fn get(&self, _account: &str) -> Result<Option<Zeroizing<String>>> {
            Ok(None)
        }

        fn set(&self, _account: &str, _secret: &str) -> Result<()> {
            *self.writes.lock().expect("lock") += 1;
            Ok(())
        }

        fn delete(&self, _account: &str) -> Result<()> {
            Ok(())
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
    fn raw_keyring_api_avoids_legacy_utf16_expansion() {
        let token = "x".repeat(1652);
        assert!(!fits_in_credential_manager(&token));
        assert!(raw_fits_in_credential_manager(token.as_bytes()));

        let dir = tempfile::tempdir().expect("tempdir");
        let fallback =
            FileSecretStore::new(dir.path().join("fallback"), Arc::new(ReversingProtector));
        let fallback_path = fallback.path_for("token");
        let storage = SecretStorage::with_oversize(FakeKeychain::capped(), fallback);
        storage.set("token", &token).expect("store in primary");
        assert!(!fallback_path.exists());
        assert_eq!(*storage.get("token").expect("get").expect("present"), token);
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
        let storage = SecretStorage::single_backend(FakeKeychain::capped());
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
        let storage = SecretStorage::single_backend(FakeKeychain::default());
        let (key, created) = storage.db_key_or_create().expect("create");
        assert!(created);
        let (again, created_again) = storage.db_key_or_create().expect("load");
        assert!(!created_again);
        assert_eq!(key, again);
    }

    #[test]
    fn owner_only_file_storage_round_trips_without_a_platform_credential_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let credentials = dir.path().join("credentials");
        let storage = SecretStorage::owner_only_plaintext_files(
            &credentials,
            OwnerOnlyPlaintext::acknowledge_risk(),
        );
        let token = format!("header.{}.signature", "x".repeat(4000));

        storage.set("provider:codex", &token).expect("store");
        assert_eq!(
            *storage
                .get("provider:codex")
                .expect("read")
                .expect("present"),
            token
        );
        storage.delete("provider:codex").expect("delete");
        assert!(
            storage
                .get("provider:codex")
                .expect("read after delete")
                .is_none()
        );
    }

    #[test]
    fn account_names_cannot_escape_the_secret_directory() {
        let store = FileSecretStore::new("/tmp/aibo-secrets", Arc::new(ReversingProtector));
        let path = store.path_for("../../etc/passwd");
        assert_eq!(path.parent(), Some(Path::new("/tmp/aibo-secrets")));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("v2-2e2e2f2e2e2f6574632f706173737764.secret")
        );
    }

    #[test]
    fn account_file_names_do_not_collide() {
        let store = FileSecretStore::new("/tmp/aibo-secrets", Arc::new(ReversingProtector));
        assert_ne!(store.path_for("a/b"), store.path_for("a_b"));
    }

    #[test]
    fn legacy_file_is_read_and_removed_on_next_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileSecretStore::new(dir.path().join("secrets"), Arc::new(ReversingProtector));
        ensure_secure_directory(&store.dir).expect("directory");
        let legacy_path = store
            .legacy_path_for("provider:old")
            .expect("supported legacy account");
        let legacy_ciphertext: Vec<u8> = b"old-secret".iter().rev().copied().collect();
        fs::write(&legacy_path, legacy_ciphertext).expect("legacy write");

        assert_eq!(
            *store
                .get("provider:old")
                .expect("legacy read")
                .expect("present"),
            "old-secret"
        );
        store
            .set("provider:old", "new-secret")
            .expect("migrating write");
        assert!(!legacy_path.exists());
        assert_eq!(
            *store.get("provider:old").expect("read").expect("present"),
            "new-secret"
        );
    }

    #[test]
    fn ambiguous_legacy_names_are_never_read_or_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileSecretStore::new(dir.path().join("secrets"), Arc::new(ReversingProtector));
        ensure_secure_directory(&store.dir).expect("directory");
        let legitimate = store
            .legacy_path_for("provider:a_b")
            .expect("safe legacy account");
        let ciphertext: Vec<u8> = b"other-provider-secret".iter().rev().copied().collect();
        fs::write(&legitimate, ciphertext).expect("legacy write");

        assert!(store.get("provider:a/b").expect("read").is_none());
        store.delete("provider:a/b").expect("delete");
        assert!(legitimate.exists(), "colliding account must not delete it");
    }

    #[test]
    fn conflicting_backends_fail_closed_after_cleanup_failure() {
        let primary = DeleteFailingStore::default();
        let control = primary.clone();
        let secondary = FakeKeychain::default();
        let storage = SecretStorage::with_oversize(primary, secondary);
        storage.set("token", "old").expect("small value");

        control.fail_delete.store(true, Ordering::SeqCst);
        storage
            .set("token", &"n".repeat(WINDOWS_CREDENTIAL_BLOB_MAX_BYTES + 1))
            .expect_err("cleanup fails");
        let err = storage
            .get("token")
            .expect_err("must not return stale primary");
        assert!(matches!(
            err,
            StoreError::Keychain(KeychainError {
                kind: KeychainErrorKind::BadData,
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn protected_files_are_created_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let secret_dir = dir.path().join("secrets");
        let store = FileSecretStore::new(&secret_dir, Arc::new(ReversingProtector));
        store.set("token", "secret").expect("write");

        let dir_mode = fs::metadata(&secret_dir)
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(store.path_for("token"))
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        assert!(fs::read_dir(&secret_dir).expect("read dir").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_secret_files_are_not_followed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let secret_dir = dir.path().join("secrets");
        let store = FileSecretStore::new(&secret_dir, Arc::new(ReversingProtector));
        ensure_secure_directory(&secret_dir).expect("directory");
        let target = dir.path().join("target");
        fs::write(&target, b"terces").expect("target");
        symlink(&target, store.path_for("token")).expect("symlink");

        assert!(store.get("token").is_err());
    }

    #[test]
    fn migration_verifies_before_deleting_source() {
        let source = FakeKeychain::default();
        source.set("token", "secret").expect("source write");
        let destination = FakeKeychain::default();

        assert_eq!(
            migrate_secret("token", &source, &destination).expect("migration"),
            MigrationOutcome::Migrated
        );
        assert!(source.get("token").expect("source read").is_none());
        assert_eq!(
            *destination
                .get("token")
                .expect("destination read")
                .expect("present"),
            "secret"
        );
    }

    #[test]
    fn failed_migration_verification_preserves_source() {
        let source = FakeKeychain::default();
        source.set("token", "secret").expect("source write");
        let destination = LyingDestination::default();

        assert!(migrate_secret("token", &source, &destination).is_err());
        assert_eq!(
            *source.get("token").expect("source read").expect("present"),
            "secret"
        );
    }

    #[test]
    fn migration_conflict_preserves_both_values() {
        let source = FakeKeychain::default();
        source.set("token", "old").expect("source write");
        let destination = FakeKeychain::default();
        destination.set("token", "new").expect("destination write");

        assert_eq!(
            migrate_secret("token", &source, &destination).expect("migration"),
            MigrationOutcome::Conflict
        );
        assert_eq!(
            *source.get("token").expect("source read").expect("present"),
            "old"
        );
        assert_eq!(
            *destination
                .get("token")
                .expect("destination read")
                .expect("present"),
            "new"
        );
    }
}
