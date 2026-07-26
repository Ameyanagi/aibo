//! The native probe that runs inside `iced::window::run`.
//!
//! Everything here executes on iced's event-loop thread. On macOS that is the
//! main thread, which is a hard requirement for AppKit — the probe checks it
//! with `MainThreadMarker::new()` and reports the answer rather than assuming.

use iced::window::Window;
// `HasWindowHandle` itself is reached through `iced::window::Window`'s
// supertrait bound — the crate is still a direct dependency because that is the
// version-unification claim S1 is testing.
use raw_window_handle::RawWindowHandle;

/// How the vibrancy surface is inserted into the native view hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Don't touch vibrancy at all — isolates the level / all-Spaces half.
    None,
    /// `window_vibrancy::apply_vibrancy` verbatim: the effect view becomes a
    /// **subview** of iced's own view.
    Subview,
    /// Reparent so the effect view is a true **sibling below** iced's view.
    Sibling,
}

impl Strategy {
    /// Parse `--strategy <none|subview|sibling>`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "subview" => Some(Self::Subview),
            "sibling" => Some(Self::Sibling),
            _ => None,
        }
    }

    /// Human name used in the report.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Subview => "subview",
            Self::Sibling => "sibling",
        }
    }
}

/// A flat `key value` report. Deliberately not structured: a spike's output is
/// read by a human once and then thrown away.
#[derive(Debug, Clone, Default)]
pub struct Report {
    rows: Vec<(String, String)>,
}

impl Report {
    /// Record one `key value` row.
    pub fn push(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.rows.push((key.into(), value.into()));
    }

    /// Print the whole report to stdout, aligned.
    pub fn print(&self) {
        println!("\n--- S1 report ---------------------------------------------");
        let width = self.rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        for (k, v) in &self.rows {
            println!("{k:<width$}  {v}");
        }
        println!("-----------------------------------------------------------");
        println!(
            "The vibrancy question is NOT answered by this text. Look at the\n\
             window. Everything above only proves the calls were accepted."
        );
    }

    /// One-line summary for the on-screen label.
    pub fn verdict_line(&self) -> String {
        let get = |k: &str| {
            self.rows
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or("?")
        };
        format!(
            "handle={} vibrancy={} level={} all_spaces={}",
            get("handle.kind"),
            get("vibrancy.applied"),
            get("level.readback"),
            get("collection_behavior.can_join_all_spaces")
        )
    }
}

/// Run every S1 probe against the window iced just handed us.
pub fn run(window: &dyn Window, strategy: Strategy) -> Report {
    let mut r = Report::default();
    r.push("strategy", strategy.name());
    r.push(
        "thread.is_main",
        // Not authoritative on its own — the macOS branch asks AppKit directly.
        format!("{}", std::thread::current().name().unwrap_or("<unnamed>")),
    );

    let handle = match window.window_handle() {
        Ok(h) => {
            r.push("handle.ok", "true");
            h
        }
        Err(e) => {
            r.push("handle.ok", "false");
            r.push("handle.error", format!("{e}"));
            return r;
        }
    };

    match handle.as_raw() {
        RawWindowHandle::AppKit(appkit) => {
            r.push("handle.kind", "AppKit");
            r.push("handle.ns_view", format!("{:p}", appkit.ns_view.as_ptr()));
            #[cfg(target_os = "macos")]
            macos::probe(appkit.ns_view, window, strategy, &mut r);
        }
        RawWindowHandle::Win32(win32) => {
            r.push("handle.kind", "Win32");
            r.push("handle.hwnd", format!("{:#x}", win32.hwnd.get()));
            windows_probe(window, strategy, &mut r);
        }
        other => {
            r.push("handle.kind", format!("{other:?}"));
            r.push(
                "handle.unsupported",
                "S1 only covers macOS and Windows (§8)".to_string(),
            );
        }
    }

    r
}

/// Windows half of S1: acrylic via `window-vibrancy`.
///
/// Kept free of any direct `windows`-crate call on purpose — this file is only
/// ever compiled on macOS by the author of the spike, and an unverified raw
/// `DwmSetWindowAttribute` signature would break the Windows build for whoever
/// runs it next. `window_vibrancy::apply_acrylic` already sets
/// `DWMWA_SYSTEMBACKDROP_TYPE` = `DWMSBT_TRANSIENTWINDOW` on build 22621+,
/// which is exactly what §8 asks for.
fn windows_probe(window: &dyn Window, strategy: Strategy, r: &mut Report) {
    if strategy == Strategy::None {
        r.push("vibrancy.applied", "skipped");
    } else {
        // §8: use `DWMSBT_TRANSIENTWINDOW` (acrylic), not Mica, for a transient
        // palette; floor is build **22621**, not 22000.
        match window_vibrancy::apply_acrylic(window, None) {
            Ok(()) => r.push("vibrancy.applied", "true (acrylic)"),
            Err(e) => {
                r.push("vibrancy.applied", "false");
                r.push("vibrancy.error", format!("{e}"));
            }
        }
    }

    r.push("level.readback", "n/a (winit WS_EX_TOPMOST)");
    r.push("collection_behavior.can_join_all_spaces", "n/a (macOS only)");

    // SPIKE: S1 — three Windows-only questions this binary does NOT answer:
    //  1. `DWMWA_USE_IMMERSIVE_DARK_MODE` must be set explicitly (§8: windows
    //     default to light regardless of system setting). `window-vibrancy`
    //     does not set it.
    //  2. §8: acrylic "falls back to a neutral colour when the window
    //     deactivates" — verify against an always-inactive panel, which is
    //     precisely aibo's case.
    //  3. §8: `WS_EX_NOACTIVATE` and a text input are mutually exclusive, so
    //     confirm the panel can take keyboard focus at all with these flags.
    r.push(
        "windows.not_covered",
        "immersive_dark_mode, deactivated-acrylic fallback, WS_EX_NOACTIVATE+input",
    );
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::ptr::NonNull;

    use iced::window::Window;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObjectProtocol};
    use objc2::{ClassType, Message};
    use objc2_app_kit::{
        NSAutoresizingMaskOptions, NSColor, NSFloatingWindowLevel, NSPanel, NSView,
        NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
        NSVisualEffectView, NSWindow, NSWindowCollectionBehavior,
    };
    use objc2_foundation::MainThreadMarker;

    use super::{Report, Strategy};

    /// Objective-C class name of any objc2 object.
    fn class_name<T>(obj: &T) -> String {
        // SAFETY: every objc2 framework type is a `#[repr(C)]` newtype whose
        // base address is a valid Objective-C object pointer, so reinterpreting
        // `&T` as `&AnyObject` is exactly the identity the framework crates
        // already rely on for `Deref`.
        let any: &AnyObject = unsafe { &*(obj as *const T).cast::<AnyObject>() };
        any.class().name().to_string_lossy().into_owned()
    }

    /// Comma-separated class names of a view's immediate subviews — the direct
    /// evidence for where the effect view landed in the hierarchy.
    fn subview_classes(view: &NSView) -> String {
        let subviews = view.subviews();
        let mut names = Vec::new();
        for sub in subviews.iter() {
            names.push(class_name(&*sub));
        }
        if names.is_empty() {
            "<none>".to_string()
        } else {
            names.join(",")
        }
    }

    /// Everything S1 asks about a macOS window, in order.
    pub fn probe(
        ns_view: NonNull<c_void>,
        window: &dyn Window,
        strategy: Strategy,
        r: &mut Report,
    ) {
        let Some(mtm) = MainThreadMarker::new() else {
            // If this ever fires, `iced::window::run` does not run its closure
            // on the main thread and the whole S1 approach is dead — every
            // AppKit call below would be UB.
            r.push("appkit.main_thread", "FALSE — ABORTING, S1 FAILS");
            return;
        };
        r.push("appkit.main_thread", "true");

        // SAFETY: `ns_view` comes from `AppKitWindowHandle`, whose contract is
        // that it points at a live `NSView` owned by the window we are being
        // called about. We are on the main thread (checked above) and the view
        // cannot be deallocated while iced holds the window.
        let view: &NSView = unsafe { ns_view.cast::<NSView>().as_ref() };
        r.push("view.class", class_name(view));
        r.push("view.subviews.before", subview_classes(view));

        let Some(ns_window) = view.window() else {
            r.push("window.found", "false — the view has no NSWindow yet");
            return;
        };
        r.push("window.found", "true");
        r.push("window.class", class_name(&*ns_window));
        // §8: "A true non-activating `NSPanel` does not [work] — winit only
        // ever creates `NSWindow`". This line is the check for that claim.
        r.push(
            "window.is_nspanel",
            format!("{}", ns_window.isKindOfClass(NSPanel::class())),
        );
        r.push("window.is_opaque.before", format!("{}", ns_window.isOpaque()));
        r.push("window.style_mask", format!("{:#x}", ns_window.styleMask().0));
        r.push("window.level.before", format!("{}", ns_window.level()));

        // --- transparency ---------------------------------------------------
        // Vibrancy is a *behind-window* effect: an opaque window or an opaque
        // background colour hides it completely. iced's `transparent: true`
        // handles the surface; these two handle the NSWindow.
        ns_window.setOpaque(false);
        ns_window.setBackgroundColor(Some(&NSColor::clearColor()));
        r.push("window.is_opaque.after", format!("{}", ns_window.isOpaque()));

        // --- floating level -------------------------------------------------
        // §8: `Level::AlwaysOnTop` → `kCGFloatingWindowLevel`. We set it again
        // natively and read back, so the report distinguishes "iced asked" from
        // "AppKit accepted".
        ns_window.setLevel(NSFloatingWindowLevel);
        let level_after = ns_window.level();
        r.push("level.readback", format!("{level_after}"));
        r.push(
            "level.is_floating",
            format!("{}", level_after == NSFloatingWindowLevel),
        );

        // --- all Spaces -----------------------------------------------------
        // §9: "the panel must join all Spaces or it appears on the wrong
        // desktop when the user is in a fullscreen app." `CanJoinAllSpaces`
        // alone is not enough for the fullscreen case — `FullScreenAuxiliary`
        // is what lets the panel appear over another app's fullscreen Space,
        // and `Stationary` stops it sliding during Space transitions.
        let wanted = NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary;
        let before = ns_window.collectionBehavior();
        ns_window.setCollectionBehavior(before | wanted);
        let after = ns_window.collectionBehavior();
        r.push("collection_behavior.before", format!("{:#x}", before.0));
        r.push("collection_behavior.after", format!("{:#x}", after.0));
        r.push(
            "collection_behavior.can_join_all_spaces",
            format!(
                "{}",
                after.contains(NSWindowCollectionBehavior::CanJoinAllSpaces)
            ),
        );
        r.push(
            "collection_behavior.fullscreen_auxiliary",
            format!(
                "{}",
                after.contains(NSWindowCollectionBehavior::FullScreenAuxiliary)
            ),
        );
        r.push(
            "collection_behavior.stationary",
            format!("{}", after.contains(NSWindowCollectionBehavior::Stationary)),
        );

        // --- vibrancy -------------------------------------------------------
        match strategy {
            Strategy::None => {
                r.push("vibrancy.applied", "skipped");
            }
            Strategy::Subview => {
                // The off-the-shelf path. §16 wants a HUD-like material for a
                // transient palette; `Active` because the panel is frequently
                // the inactive window and `FollowsWindowActiveState` would grey
                // it out (the macOS analogue of §8's Windows acrylic warning).
                match window_vibrancy::apply_vibrancy(
                    window,
                    window_vibrancy::NSVisualEffectMaterial::HudWindow,
                    Some(window_vibrancy::NSVisualEffectState::Active),
                    Some(12.0),
                ) {
                    Ok(()) => r.push("vibrancy.applied", "true (subview)"),
                    Err(e) => {
                        r.push("vibrancy.applied", "false");
                        r.push("vibrancy.error", format!("{e}"));
                    }
                }
                r.push("view.subviews.after", subview_classes(view));
            }
            Strategy::Sibling => {
                apply_sibling_vibrancy(mtm, &ns_window, view, r);
            }
        }

        r.push(
            "note",
            "acceptance is VISUAL — see README 'How to read the result'".to_string(),
        );
    }

    /// Reparent so the `NSVisualEffectView` is a **sibling below** iced's view
    /// rather than a subview of it.
    ///
    /// This is the strategy `window-vibrancy` cannot express, and the one most
    /// likely to actually work: a layer-backed view's own layer is the parent
    /// of its subviews' layers, so a subview can never draw behind its host's
    /// `CAMetalLayer`. Siblings under a shared container can.
    fn apply_sibling_vibrancy(
        mtm: MainThreadMarker,
        ns_window: &NSWindow,
        iced_view: &NSView,
        r: &mut Report,
    ) {
        let content = ns_window.contentView();
        let is_content = content
            .as_deref()
            .is_some_and(|c| std::ptr::eq(c as *const NSView, iced_view as *const NSView));
        r.push("sibling.iced_view_is_contentview", format!("{is_content}"));

        // Retain iced's view before it is detached by `setContentView:`.
        let iced_view_owned: Retained<NSView> = iced_view.retain();
        let bounds = iced_view.bounds();

        let container = NSView::initWithFrame(mtm.alloc::<NSView>(), bounds);
        container.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let effect = NSVisualEffectView::initWithFrame(mtm.alloc::<NSVisualEffectView>(), bounds);
        effect.setMaterial(NSVisualEffectMaterial::HUDWindow);
        effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        // Always `Active`: an overlay panel is very often the inactive window,
        // and `FollowsWindowActiveState` would desaturate it exactly then.
        effect.setState(NSVisualEffectState::Active);
        effect.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        // Order matters: the effect view goes in first (bottom), iced's view on
        // top of it.
        ns_window.setContentView(Some(&container));
        container.addSubview(&effect);
        container.addSubview(&iced_view_owned);
        iced_view_owned.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        iced_view_owned.setFrame(container.bounds());

        r.push("vibrancy.applied", "true (sibling)");
        r.push("sibling.container.class", class_name(&*container));
        r.push("sibling.container.subviews", subview_classes(&container));
        r.push(
            "sibling.contentview.after",
            ns_window
                .contentView()
                .map(|v| class_name(&*v))
                .unwrap_or_else(|| "<none>".to_string()),
        );

        // SPIKE: S1 — reparenting winit's view is not something winit
        // documents or supports. Watch for: resize/scale-factor breakage, IME
        // (§9, S10) which is attached to the view's input context, and mouse
        // hit-testing. If `sibling` renders correctly but breaks input, the
        // real answer for aibo is §20's stated alternative — `iced_wgpu` plus a
        // custom shell — not this hack.
    }
}
