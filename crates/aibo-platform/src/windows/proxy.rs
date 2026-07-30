//! Windows' own idea of how to reach the internet (§13).
//!
//! reqwest reads `HTTPS_PROXY` and friends from the environment and nothing
//! else. Windows does not set those: a managed machine is configured through
//! *Internet Settings*, which is where Group Policy, the Settings app and every
//! corporate agent write. So a machine that reaches the internet perfectly well
//! through a proxy looked, to aibo, like a machine with no route at all — and
//! §13 reported "offline", accurately and unhelpfully.
//!
//! What is read, from `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet
//! Settings`:
//!
//! * `ProxyEnable` (DWORD) — whether the manual proxy is on. Respected, because
//!   `ProxyServer` keeps its old value when the toggle is turned off, and using
//!   a disabled proxy is worse than using none.
//! * `ProxyServer` (string) — either a bare `host:port`, or a
//!   per-scheme list, `http=host:port;https=host:port`.
//! * `AutoConfigURL` (string) — a PAC script. **Not evaluated.** PAC is
//!   JavaScript whose result depends on the destination host, so honouring it
//!   would mean running a script per request. It is detected and logged, so a
//!   PAC-only network produces a diagnosable message rather than a silent
//!   failure.
//!
//! Read from `HKCU` rather than through `WinHttpGetIEProxyConfigForCurrentUser`
//! to stay on the registry surface this crate already uses, and because that
//! API's out-params must be freed with `GlobalFree` — a leak or a double-free
//! for a value read once at startup.

use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};
use windows::core::w;

/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings`.
const INTERNET_SETTINGS: windows::core::PCWSTR =
    w!(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings");

/// The proxy to use for HTTPS, or `None` to connect directly.
///
/// Never fails: an unreadable registry, a machine with no proxy and a
/// PAC-only configuration all mean "aibo has nothing better than a direct
/// connection", and returning an error would only give the caller the same
/// choice with more ceremony.
pub(crate) fn system_proxy() -> Option<String> {
    let key = open_internet_settings()?;

    let enabled = read_dword(key, w!("ProxyEnable")).unwrap_or(0) != 0;
    let server = read_string(key, w!("ProxyServer"));
    let pac = read_string(key, w!("AutoConfigURL"));

    // SAFETY: `key` came from `RegOpenKeyExW` and is not used after this.
    let _ = unsafe { RegCloseKey(key) };

    if let Some(pac) = pac.as_deref().filter(|value| !value.is_empty()) {
        // Logged rather than ignored: on a PAC-only network this is the single
        // fact that explains why nothing connects.
        tracing::warn!(
            pac,
            "Windows is configured with a PAC script, which aibo does not \
             evaluate; set HTTPS_PROXY if connections fail"
        );
    }

    if !enabled {
        return None;
    }

    let proxy = https_proxy_from(server.as_deref()?)?;
    Some(normalise(proxy))
}

/// Pick the HTTPS entry out of a `ProxyServer` value.
///
/// The value is either one address for every scheme, or a `;`-separated list of
/// `scheme=address`. In the list form the `https` entry is what matters and a
/// bare `socks=` entry must not be mistaken for it, which is why this looks for
/// the key rather than taking the first address it sees.
fn https_proxy_from(value: &str) -> Option<String> {
    if !value.contains('=') {
        let trimmed = value.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_owned());
    }
    let mut http = None;
    for entry in value.split(';') {
        let (scheme, address) = entry.split_once('=')?;
        let address = address.trim();
        if address.is_empty() {
            continue;
        }
        match scheme.trim().to_ascii_lowercase().as_str() {
            "https" => return Some(address.to_owned()),
            // Kept as a fallback: a proxy that only names `http` still almost
            // always accepts CONNECT for TLS, and trying it beats not trying.
            "http" => http = Some(address.to_owned()),
            _ => {}
        }
    }
    http
}

/// Give the address a scheme, which `reqwest::Proxy::all` requires.
///
/// `ProxyServer` holds a bare `host:port`; `http://` is correct even for HTTPS
/// traffic, because that describes how to talk to the *proxy* — the tunnel to
/// the destination is established with `CONNECT` inside it.
fn normalise(address: String) -> String {
    if address.contains("://") {
        address
    } else {
        format!("http://{address}")
    }
}

fn open_internet_settings() -> Option<HKEY> {
    let mut key = HKEY::default();
    // SAFETY: `INTERNET_SETTINGS` is a static wide string and `key` is a live
    // out-param that is only read when the call reports success.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            INTERNET_SETTINGS,
            None,
            KEY_READ,
            &mut key,
        )
    };
    status.is_ok().then_some(key)
}

fn read_dword(key: HKEY, name: windows::core::PCWSTR) -> Option<u32> {
    let mut value = 0u32;
    let mut size = u32::try_from(size_of::<u32>()).ok()?;
    let mut kind = REG_VALUE_TYPE::default();
    // SAFETY: the data pointer addresses `value`, whose size is passed in
    // `size`; both outlive the call.
    let status = unsafe {
        RegQueryValueExW(
            key,
            name,
            None,
            Some(&mut kind),
            Some(std::ptr::from_mut(&mut value).cast::<u8>()),
            Some(&mut size),
        )
    };
    status.is_ok().then_some(value)
}

fn read_string(key: HKEY, name: windows::core::PCWSTR) -> Option<String> {
    let mut size = 0u32;
    // SAFETY: asking only for the byte length, so no data pointer is passed.
    let status = unsafe { RegQueryValueExW(key, name, None, None, None, Some(&mut size)) };
    if status.is_err() || size == 0 {
        return None;
    }

    let mut bytes = vec![0u8; size as usize];
    // SAFETY: `bytes` is exactly `size` long, which is what the call above
    // reported, and it outlives the call.
    let status = unsafe {
        RegQueryValueExW(
            key,
            name,
            None,
            None,
            Some(bytes.as_mut_ptr()),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }

    // REG_SZ is UTF-16 and the stored length includes the terminator, so the
    // byte count can be odd or carry a trailing NUL; both are trimmed rather
    // than trusted.
    let wide: Vec<u16> = bytes[..size as usize]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    let value = String::from_utf16_lossy(&wide);
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list form is the one that matters on a managed machine, and picking
    /// the first address rather than the `https` one is the obvious bug.
    #[test]
    fn the_https_entry_wins_over_the_others() {
        assert_eq!(
            https_proxy_from("http=a:80;https=b:443;ftp=c:21").as_deref(),
            Some("b:443")
        );
        // A `socks` entry must never be handed to `Proxy::all` as if it were
        // an HTTP proxy.
        assert_eq!(https_proxy_from("socks=s:1080"), None);
    }

    #[test]
    fn http_stands_in_when_https_is_absent() {
        assert_eq!(https_proxy_from("http=a:80").as_deref(), Some("a:80"));
    }

    #[test]
    fn a_bare_address_applies_to_every_scheme() {
        assert_eq!(
            https_proxy_from("proxy:8080").as_deref(),
            Some("proxy:8080")
        );
        assert_eq!(https_proxy_from("  "), None);
    }

    /// `reqwest::Proxy::all` rejects a bare `host:port`, so the scheme is not
    /// cosmetic — without it every managed machine would log "unusable system
    /// proxy" and connect directly.
    #[test]
    fn an_address_without_a_scheme_gets_one() {
        assert_eq!(normalise("proxy:8080".to_owned()), "http://proxy:8080");
        assert_eq!(
            normalise("http://proxy:8080".to_owned()),
            "http://proxy:8080"
        );
    }
}
