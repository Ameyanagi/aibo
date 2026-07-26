//! Application identity, and the AX-tree activation split that §8 flags as
//! High risk.
//!
//! > **Two different flags, keyed on app identity**: Chrome/Chromium honours
//! > `AXEnhancedUserInterface`; **Electron honours `AXManualAccessibility`**.
//! > Setting the wrong one returns `kAXErrorAttributeUnsupported`.
//!
//! Two further facts from the same row make this more than a lookup table:
//! Chrome's activation is **asynchronous**, so reading immediately after
//! setting returns an empty tree; and `AXEnhancedUserInterface` breaks window
//! positioning and makes resizing sluggish, which is why setting it from a tray
//! utility is user-hostile and gated behind
//! [`MacosConfig::allow_ax_tree_activation`].
//!
//! [`MacosConfig::allow_ax_tree_activation`]: super::MacosConfig::allow_ax_tree_activation

use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};

/// `AXEnhancedUserInterface` — the Chrome/Chromium flag. Not exported by
/// `accessibility-sys`, which only carries the documented constants.
pub(crate) const AX_ENHANCED_USER_INTERFACE: &str = "AXEnhancedUserInterface";

/// `AXManualAccessibility` — the Electron flag.
pub(crate) const AX_MANUAL_ACCESSIBILITY: &str = "AXManualAccessibility";

/// Which AX-tree activation attribute an application understands (§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AxActivation {
    /// Chrome, Chromium, Edge, Brave, Vivaldi, Arc — `AXEnhancedUserInterface`.
    ///
    /// Activation is asynchronous: the tree is empty for some period after the
    /// write. Do not read straight through.
    EnhancedUserInterface,
    /// Electron shells — `AXManualAccessibility`.
    ManualAccessibility,
    /// A native app, or one aibo does not recognise: touch nothing. Writing the
    /// wrong flag returns `kAXErrorAttributeUnsupported`, which is harmless but
    /// pointless.
    None,
}

/// Bundle identifiers of Chromium-family browsers.
///
/// Matched by exact identifier and by prefix, because Chrome ships channel
/// variants (`com.google.Chrome.canary`, `.beta`, `.dev`).
const CHROMIUM_PREFIXES: &[&str] = &[
    "com.google.Chrome",
    "org.chromium.Chromium",
    "com.microsoft.edgemac",
    "com.brave.Browser",
    "com.vivaldi.Vivaldi",
    "com.operasoftware.Opera",
    "company.thebrowser.Browser", // Arc
    "company.thebrowser.dia",
];

/// Bundle identifiers of Electron applications aibo is expected to meet.
///
/// SPIKE: S2 — this list is a starting point, not a verified matrix. The spike
/// must produce the real one across Safari, Chrome, VS Code, Slack, Word,
/// Notion and Terminal, plus `testapps/`.
const ELECTRON_PREFIXES: &[&str] = &[
    "com.microsoft.VSCode",
    "com.visualstudio.code.oss",
    "com.todesktop.230313mzl4w4u92", // Cursor
    "com.todesktop.",                // ToDesktop-packaged Electron apps generally
    "com.tinyspeck.slackmacgap",
    "notion.id",
    "md.obsidian",
    "com.hnc.Discord",
    "com.figma.Desktop",
    "com.spotify.client",
    "com.postmanlabs.mac",
    "com.linear",
];

/// Applications whose presence sets [`RouteInput::has_code`] (§4).
///
/// [`RouteInput::has_code`]: aibo_core::types::RouteInput
const CODE_APP_PREFIXES: &[&str] = &[
    "com.microsoft.VSCode",
    "com.visualstudio.code.oss",
    "com.todesktop.230313mzl4w4u92", // Cursor
    "com.jetbrains.",
    "com.apple.dt.Xcode",
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "dev.warp.Warp-Stable",
    "net.kovidgoyal.kitty",
    "com.github.wez.wezterm",
    "com.mitchellh.ghostty",
    "com.sublimetext.",
    "com.neovim.",
    "org.vim.MacVim",
    "com.zed.Zed",
    "dev.zed.Zed",
];

fn matches_any(identifier: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| identifier.starts_with(p))
}

/// Decide which activation attribute — if any — this bundle identifier wants.
pub fn ax_activation_for(identifier: &str) -> AxActivation {
    if matches_any(identifier, CHROMIUM_PREFIXES) {
        AxActivation::EnhancedUserInterface
    } else if matches_any(identifier, ELECTRON_PREFIXES) {
        AxActivation::ManualAccessibility
    } else {
        AxActivation::None
    }
}

/// Is this a code editor / terminal, for routing purposes (§4)?
pub fn is_code_app(identifier: &str) -> bool {
    matches_any(identifier, CODE_APP_PREFIXES)
}

/// Apps whose clipboard contents are never recorded, regardless of markers
/// (§12 "plus an app denylist").
const CLIPBOARD_DENYLIST_PREFIXES: &[&str] = &[
    "com.1password.",
    "com.agilebits.onepassword",
    "com.bitwarden.desktop",
    "com.lastpass.",
    "in.sinew.Enpass-Desktop",
    "com.dashlane.",
    "org.keepassxc.keepassxc",
    "com.apple.keychainaccess",
    "com.apple.Passwords",
];

/// Should clipboard content sourced from this app be treated as concealed?
pub fn is_clipboard_denylisted(identifier: &str) -> bool {
    matches_any(identifier, CLIPBOARD_DENYLIST_PREFIXES)
}

/// The frontmost application's `(pid, bundle identifier, localised name)`.
///
/// `NSWorkspace` answers from cached state maintained by the window server, so
/// unlike an AX read this cannot block on a hung target — which is what makes
/// it legal in the §8 step-1 synchronous snapshot.
pub(crate) fn frontmost_application() -> Option<(i32, String, String)> {
    let workspace = NSWorkspace::sharedWorkspace();
    let app = workspace.frontmostApplication()?;
    let pid = app.processIdentifier();
    let bundle = app
        .bundleIdentifier()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let name = app
        .localizedName()
        .map(|s| s.to_string())
        .unwrap_or_default();
    Some((pid, bundle, name))
}

/// `(bundle identifier, localised name)` for a **specific** pid.
///
/// This is the identity lookup the deferred capture path uses (§7, §8): by the
/// time capture runs the panel is frontmost, so asking
/// [`frontmost_application`] would answer "aibo" for every field it fills in.
/// Like `frontmost_application` it reads `NSWorkspace`'s cached state and
/// cannot block on the target.
pub(crate) fn application_identity(pid: i32) -> Option<(String, String)> {
    let app = running_application_for_pid(pid)?;
    let bundle = app
        .bundleIdentifier()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let name = app
        .localizedName()
        .map(|s| s.to_string())
        .unwrap_or_default();
    Some((bundle, name))
}

/// The `NSRunningApplication` for a pid, if the process still exists.
pub(crate) fn running_application_for_pid(pid: i32) -> Option<Retained<NSRunningApplication>> {
    NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .find(|app| app.processIdentifier() == pid)
}

/// Bring an application to the front.
///
/// `ActivateAllWindows` is deliberate: after aibo's panel took focus, the user
/// expects the *window* they were editing back, not merely the app's menu bar.
/// The caller must still confirm the activation landed — see
/// [`Worker::restore_focus`].
///
/// [`Worker::restore_focus`]: super::worker::Worker::restore_focus
pub(crate) fn activate(app: &NSRunningApplication) -> bool {
    app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_and_electron_get_different_flags() {
        assert_eq!(
            ax_activation_for("com.google.Chrome.canary"),
            AxActivation::EnhancedUserInterface
        );
        assert_eq!(
            ax_activation_for("com.microsoft.VSCode"),
            AxActivation::ManualAccessibility
        );
        assert_eq!(ax_activation_for("com.apple.Safari"), AxActivation::None);
    }

    #[test]
    fn edge_is_chromium_not_electron() {
        assert_eq!(
            ax_activation_for("com.microsoft.edgemac"),
            AxActivation::EnhancedUserInterface
        );
    }

    #[test]
    fn password_managers_are_denylisted() {
        assert!(is_clipboard_denylisted("com.1password.1password"));
        assert!(!is_clipboard_denylisted("com.apple.Safari"));
    }
}
