//! The SQLCipher database key and its recovery code (§12).
//!
//! §12 forces the choice up front: "decide now whether the key is device-bound
//! or user-recoverable — you cannot retrofit recoverability onto data already
//! encrypted with an unrecoverable key." The plan's recommendation, implemented
//! here, is **device-bound by default with an optional recovery code**.
//!
//! The recovery code *is* the key, Crockford-base32 encoded and grouped. That
//! is deliberate: wrapping the key under a passphrase would add a KDF, a
//! wrapping format and a second thing to lose, and would still leave the user
//! holding a string that decrypts their history. Printing the key itself is the
//! same security property with none of the moving parts, and it makes "restore
//! on a new machine" a pure decode.

use std::fmt;

use zeroize::Zeroize;

use crate::error::{Result, StoreError};

/// Length of a SQLCipher raw key, in bytes.
pub const KEY_LEN: usize = 32;

/// Crockford base32: no `I`, `L`, `O` or `U`, so a printed code cannot be
/// mistranscribed into a different valid code.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Characters per dash-separated group in a printed recovery code.
const GROUP: usize = 4;

/// The 32-byte whole-database encryption key (§12).
///
/// Zeroized on drop. `Debug` prints a placeholder — there is a redaction test
/// below, matching the one `aibo-core` keeps for `Credential`.
#[derive(Clone, PartialEq, Eq)]
pub struct DbKey([u8; KEY_LEN]);

impl DbKey {
    /// A fresh key from the OS CSPRNG.
    ///
    /// Note this does *not* use SQLite's `randomblob()`: SQLite's Windows VFS
    /// seeds its PRNG from the clock, process id and performance counter, which
    /// is not an acceptable source for a key that protects everything the user
    /// has ever typed.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; KEY_LEN];
        getrandom::fill(&mut bytes).map_err(|e| {
            StoreError::io(
                "<csprng>",
                std::io::Error::other(format!("OS CSPRNG unavailable: {e}")),
            )
        })?;
        Ok(Self(bytes))
    }

    /// Adopt raw key bytes, e.g. read back from the keychain.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw key bytes. Handle with the same care as the key itself.
    pub fn expose_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// The 64-character lower-case hex form SQLCipher's raw-key syntax wants.
    ///
    /// Returned inside [`zeroize::Zeroizing`] so the intermediate `String` does
    /// not outlive the statement that builds the `PRAGMA`.
    pub fn to_hex(&self) -> zeroize::Zeroizing<String> {
        let mut s = String::with_capacity(KEY_LEN * 2);
        for b in self.0 {
            s.push(char::from_digit((b >> 4) as u32, 16).expect("nibble"));
            s.push(char::from_digit((b & 0x0f) as u32, 16).expect("nibble"));
        }
        zeroize::Zeroizing::new(s)
    }

    /// Parse the 64-character hex form.
    pub fn from_hex(hex: &str) -> Result<Self> {
        let hex = hex.trim();
        if hex.len() != KEY_LEN * 2 {
            return Err(StoreError::RecoveryCode {
                reason: "hex key must be exactly 64 characters",
            });
        }
        let mut bytes = [0u8; KEY_LEN];
        for (i, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
            let hi = (chunk[0] as char)
                .to_digit(16)
                .ok_or(StoreError::RecoveryCode {
                    reason: "hex key contains a non-hex character",
                })?;
            let lo = (chunk[1] as char)
                .to_digit(16)
                .ok_or(StoreError::RecoveryCode {
                    reason: "hex key contains a non-hex character",
                })?;
            bytes[i] = ((hi << 4) | lo) as u8;
        }
        Ok(Self(bytes))
    }

    /// The printable recovery code: 52 Crockford-base32 characters in
    /// dash-separated groups of four.
    ///
    /// This is what §12's "an optional recovery code the user can print at
    /// setup" means. Show it exactly once, at setup, and never again.
    pub fn to_recovery_code(&self) -> zeroize::Zeroizing<String> {
        let raw = base32_encode(&self.0);
        let mut out = String::with_capacity(raw.len() + raw.len() / GROUP);
        for (i, c) in raw.chars().enumerate() {
            if i > 0 && i % GROUP == 0 {
                out.push('-');
            }
            out.push(c);
        }
        zeroize::Zeroizing::new(out)
    }

    /// Parse a recovery code back into a key.
    ///
    /// Dashes and whitespace are ignored, case is ignored, and Crockford's
    /// confusable set is folded (`I`/`L` → `1`, `O` → `0`) so a code read off
    /// paper round-trips.
    pub fn from_recovery_code(code: &str) -> Result<Self> {
        let bytes = base32_decode(code)?;
        if bytes.len() != KEY_LEN {
            return Err(StoreError::RecoveryCode {
                reason: "recovery code does not decode to a 32-byte key",
            });
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&bytes);
        Ok(Self(key))
    }
}

impl Drop for DbKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for DbKey {
    /// Prints a placeholder. The key must never reach a log line (§13).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DbKey(<redacted>)")
    }
}

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

fn base32_value(c: char) -> Option<u32> {
    let c = c.to_ascii_uppercase();
    match c {
        '0' | 'O' => Some(0),
        '1' | 'I' | 'L' => Some(1),
        '2'..='9' => c.to_digit(10),
        'A'..='H' => Some(c as u32 - 'A' as u32 + 10),
        'J' | 'K' => Some(c as u32 - 'J' as u32 + 18),
        'M' | 'N' => Some(c as u32 - 'M' as u32 + 20),
        'P'..='T' => Some(c as u32 - 'P' as u32 + 22),
        'V'..='Z' => Some(c as u32 - 'V' as u32 + 27),
        _ => None,
    }
}

fn base32_decode(code: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(KEY_LEN);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in code.chars() {
        if c == '-' || c.is_whitespace() {
            continue;
        }
        let v = base32_value(c).ok_or(StoreError::RecoveryCode {
            reason: "recovery code contains a character outside the alphabet",
        })?;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_is_not_all_zeroes_and_differs_each_time() {
        let a = DbKey::generate().expect("csprng");
        let b = DbKey::generate().expect("csprng");
        assert_ne!(a.expose_bytes(), &[0u8; KEY_LEN]);
        assert_ne!(a, b);
    }

    #[test]
    fn hex_round_trips() {
        let key = DbKey::generate().expect("csprng");
        let hex = key.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(DbKey::from_hex(&hex).expect("parse"), key);
    }

    #[test]
    fn recovery_code_round_trips() {
        let key = DbKey::generate().expect("csprng");
        let code = key.to_recovery_code();
        assert_eq!(DbKey::from_recovery_code(&code).expect("parse"), key);
    }

    #[test]
    fn recovery_code_tolerates_paper_transcription() {
        let key = DbKey::from_bytes([0x7f; KEY_LEN]);
        let code = key.to_recovery_code();
        let mangled = code.to_lowercase().replace('-', " ");
        assert_eq!(DbKey::from_recovery_code(&mangled).expect("parse"), key);
    }

    #[test]
    fn confusable_letters_fold() {
        // A user who wrote `O` for zero and `I` for one still gets their data.
        let a = DbKey::from_recovery_code(&"0".repeat(52)).expect("zeroes");
        let b = DbKey::from_recovery_code(&"O".repeat(52)).expect("letter o");
        assert_eq!(a, b);
    }

    #[test]
    fn short_or_junk_codes_are_rejected() {
        assert!(DbKey::from_recovery_code("ABCD-EFGH").is_err());
        assert!(DbKey::from_recovery_code(&"!".repeat(52)).is_err());
    }

    #[test]
    fn debug_redacts_the_key() {
        let key = DbKey::from_bytes([0xab; KEY_LEN]);
        let rendered = format!("{key:?}");
        assert_eq!(rendered, "DbKey(<redacted>)");
        assert!(!rendered.contains("ab"));
    }
}
