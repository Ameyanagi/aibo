//! Clipboard save / restore, and the one question §20 asks about it:
//! **does save/restore round-trip?**
//!
//! The honest answer this module is built to expose is "not in general". §12's
//! clipboard-hygiene section and §20's note on exclusion markers both assume the
//! product will be writing to the user's pasteboard, and `arboard` can only see
//! the flavours it knows about. A save/restore that round-trips *text* while
//! silently destroying an image, RTF, or a promised file reference is a
//! data-loss bug that no automated check here can catch — which is why
//! [`Snapshot::flavours`] records what was on the pasteboard *before*, so the
//! operator can see what got dropped.

use anyhow::{Context, Result};

/// What was on the clipboard before the spike touched it.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The plain-text flavour, if any. **The only thing that can be restored.**
    pub text: Option<String>,
    /// Every type identifier present, for the operator to compare against what
    /// survives. macOS only; empty elsewhere.
    pub flavours: Vec<String>,
    /// `NSPasteboard.changeCount` at snapshot time. macOS only.
    pub change_count: Option<i64>,
}

impl Snapshot {
    /// Did the restore put back everything that was there?
    ///
    /// Deliberately conservative: it answers "was text the *only* thing on the
    /// pasteboard", because if it was not, the restore is lossy no matter what
    /// the text comparison says.
    pub fn restore_can_be_lossless(&self) -> bool {
        self.flavours.iter().all(|f| {
            f.starts_with("public.utf8-plain-text")
                || f.starts_with("public.plain-text")
                || f.starts_with("NSStringPboardType")
                || f.starts_with("public.utf16")
        })
    }
}

/// Read the clipboard, recording enough to tell whether a restore is honest.
pub fn snapshot() -> Result<Snapshot> {
    let text = match arboard::Clipboard::new() {
        Ok(mut clipboard) => clipboard.get_text().ok(),
        Err(error) => {
            return Err(anyhow::anyhow!("cannot open the clipboard: {error}"));
        }
    };
    Ok(Snapshot {
        text,
        flavours: platform::flavours(),
        change_count: platform::change_count(),
    })
}

/// Put `text` on the clipboard.
pub fn set(text: &str) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("cannot open the clipboard")?;
    clipboard
        .set_text(text.to_owned())
        .context("set_text failed")
}

/// Put back what [`snapshot`] saved.
///
/// Returns whether anything was restored at all. An empty original clipboard is
/// not restorable through `arboard` — there is no "make it empty again" — which
/// is itself worth knowing before the product promises to leave the clipboard
/// untouched.
pub fn restore(snapshot: &Snapshot) -> Result<bool> {
    match &snapshot.text {
        Some(text) => {
            set(text)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Read back the plain-text flavour.
pub fn get() -> Result<Option<String>> {
    let mut clipboard = arboard::Clipboard::new().context("cannot open the clipboard")?;
    Ok(clipboard.get_text().ok())
}

/// The current `NSPasteboard.changeCount`, on macOS.
pub fn change_count() -> Option<i64> {
    platform::change_count()
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2_app_kit::NSPasteboard;

    /// `NSPasteboard.changeCount`.
    ///
    /// §8's selection fallback is *"synthetic ⌘C + pasteboard `changeCount`
    /// poll"*, and §20 warns that the `org.nspasteboard.*` exclusion markers must
    /// be written in the **same `declareTypes:owner:` transaction** or
    /// `changeCount` bumps twice and clipboard managers capture the first item.
    /// Watching this number is how either claim gets tested.
    pub fn change_count() -> Option<i64> {
        let pasteboard = NSPasteboard::generalPasteboard();
        Some(pasteboard.changeCount() as i64)
    }

    /// Every type identifier currently on the general pasteboard.
    pub fn flavours() -> Vec<String> {
        let pasteboard = NSPasteboard::generalPasteboard();
        let Some(types) = pasteboard.types() else {
            return Vec::new();
        };
        types.iter().map(|t| t.to_string()).collect()
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    /// Windows has `GetClipboardSequenceNumber`, which is the direct analogue.
    ///
    /// SPIKE: S4 — not wired up. §20 also requires the Windows exclusion markers
    /// `CanIncludeInClipboardHistory` and `CanUploadToCloudClipboard`, and warns
    /// they need a **serialised DWORD 0**, not a bare presence marker. Both are
    /// part of the Windows half of this spike.
    pub fn change_count() -> Option<i64> {
        None
    }

    /// SPIKE: S4 — `EnumClipboardFormats` is the Windows equivalent.
    pub fn flavours() -> Vec<String> {
        Vec::new()
    }
}
