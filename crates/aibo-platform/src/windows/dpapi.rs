//! Current-user DPAPI protection for oversized credential files.

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows_core::w;

use super::error::{WinResult, WindowsPlatformError};

/// Protects bytes with the Windows user's logon credential.
#[derive(Debug, Clone, Copy, Default)]
pub struct DpapiProtector;

impl DpapiProtector {
    /// Encrypt bytes for the current Windows user without displaying UI.
    pub fn protect(&self, plaintext: &[u8]) -> WinResult<Vec<u8>> {
        crypt(plaintext, Direction::Protect)
    }

    /// Decrypt bytes previously protected for the current Windows user.
    pub fn unprotect(&self, ciphertext: &[u8]) -> WinResult<Vec<u8>> {
        crypt(ciphertext, Direction::Unprotect)
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Protect,
    Unprotect,
}

fn crypt(input: &[u8], direction: Direction) -> WinResult<Vec<u8>> {
    let input_len = u32::try_from(input.len()).map_err(|_| {
        WindowsPlatformError::win32_bare(
            "DPAPI",
            "DPAPI input exceeds the Windows CRYPT_INTEGER_BLOB limit".to_owned(),
        )
    })?;
    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: input_len,
        // The API's historical signature is mutable, but neither operation
        // writes to the input buffer.
        pbData: input.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    // SAFETY: both blobs live for the call; `input_blob` names initialized
    // bytes; optional pointers are null. DPAPI allocates `output.pbData` with
    // LocalAlloc on success and it is copied, scrubbed, and freed below.
    let result = unsafe {
        match direction {
            Direction::Protect => CryptProtectData(
                &input_blob,
                w!("aibo protected secret"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            ),
            Direction::Unprotect => CryptUnprotectData(
                &input_blob,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            ),
        }
    };
    result.map_err(|error| {
        WindowsPlatformError::win32(
            match direction {
                Direction::Protect => "CryptProtectData",
                Direction::Unprotect => "CryptUnprotectData",
            },
            error,
        )
    })?;

    if output.cbData > 0 && output.pbData.is_null() {
        return Err(WindowsPlatformError::win32_bare(
            "DPAPI",
            "DPAPI returned a null output buffer",
        ));
    }

    let bytes = if output.cbData == 0 {
        Vec::new()
    } else {
        // SAFETY: DPAPI returned `cbData` initialized bytes at `pbData`.
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() }
    };
    if !output.pbData.is_null() {
        // SAFETY: the same allocation remains live and writable until
        // LocalFree.
        unsafe {
            std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
            let remaining = LocalFree(Some(HLOCAL(output.pbData.cast())));
            if !remaining.0.is_null() {
                return Err(WindowsPlatformError::win32_bare(
                    "LocalFree",
                    "refused the DPAPI output buffer",
                ));
            }
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trips_an_oversized_token() {
        let plaintext = vec![b'x'; 8 * 1024];
        let protector = DpapiProtector;
        let protected = protector.protect(&plaintext).expect("protect");
        assert_ne!(protected, plaintext);
        assert_eq!(
            protector.unprotect(&protected).expect("unprotect"),
            plaintext
        );
    }
}
