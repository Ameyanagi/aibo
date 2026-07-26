//! `aibo-platform` — the OS integration layer, and the only crate that is
//! allowed to contain `#[cfg(target_os)]` (§7).
//!
//! Implementations of [`PlatformBackend`] are **channel handles to a dedicated
//! platform thread**, never the platform objects themselves: `uiautomation`'s
//! types are `!Send` and `!Sync` (apartment-threaded COM) and macOS AX blocks
//! for seconds against a busy app. The per-call timeouts from §8 belong on that
//! thread's request loop.
//!
//! [`PlatformBackend`]: aibo_core::traits::PlatformBackend

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;
