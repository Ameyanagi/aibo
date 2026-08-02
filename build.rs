//! Embed the Windows executable icon.
//!
//! Gated twice: the resource only makes sense when the *target* is Windows,
//! and the resource compiler (`rc.exe` / `windres`) only exists when the
//! *host* is Windows — the macOS runner's cross-`cargo check` of the Windows
//! target must not fail over a missing toolchain it never needed. A failure
//! to embed is a warning, never a broken build: an exe with a generic icon
//! ships; a build that dies over cosmetics does not.

fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/aibo.ico");
    let target_is_windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let host_is_windows = std::env::var("HOST")
        .map(|host| host.contains("windows"))
        .unwrap_or(false);
    if !(target_is_windows && host_is_windows) {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("packaging/windows/aibo.ico");
    if let Err(error) = resource.compile() {
        println!("cargo:warning=could not embed the Windows icon: {error}");
    }
}
