//! The tray icon — and the timing constraint that dictates where it is built (§6).
//!
//! §6, verbatim: "`tray-icon` requires the event loop to be *already running* —
//! not merely created — and on macOS the tray must be created on the main
//! thread. `iced_winit` runs your `boot` function **before** `event_loop.run_app`,
//! so the tray cannot be created there. Create it from the first `update` tick
//! instead, which runs on the main thread inside the loop."
//!
//! That is the whole reason this module exists as a separate unit with a
//! `create` function rather than being a line in `boot`: the natural place to
//! put it is the place where it does not work. [`crate::app::Aibo::update`]
//! calls [`create`] once, on its first tick, and never again.
//!
//! iced's own tray-icon integration PR is still open and unmerged (§6), so
//! event delivery is hand-wired: `tray-icon` and `muda` push onto their own
//! channels, and [`forward_events`] bridges them into the iced subscription.

use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::error::{Result, UiError};
use crate::i18n::{self, Key};

/// A command chosen from the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrayCommand {
    /// Show the panel — same effect as the hotkey, for users who have lost it
    /// to a conflict (§9). This is the reason the tray must survive a failed
    /// hotkey registration.
    OpenPanel,
    /// Focus the task window if a run is in flight (§6).
    ShowTasks,
    /// Open settings.
    OpenSettings,
    /// Quit. Shutdown must reap child processes — `codex app-server` and MCP
    /// servers must not outlive aibo (§6).
    Quit,
}

impl TrayCommand {
    /// Stable menu id. Stable because `muda` identifies items by id across
    /// rebuilds, and the tooltip/menu is rebuilt when the language changes.
    const fn id(self) -> &'static str {
        match self {
            TrayCommand::OpenPanel => "aibo.tray.panel",
            TrayCommand::ShowTasks => "aibo.tray.tasks",
            TrayCommand::OpenSettings => "aibo.tray.settings",
            TrayCommand::Quit => "aibo.tray.quit",
        }
    }

    /// Resolve a `muda` menu id back to a command.
    pub fn from_id(id: &MenuId) -> Option<Self> {
        const ALL: [TrayCommand; 4] = [
            TrayCommand::OpenPanel,
            TrayCommand::ShowTasks,
            TrayCommand::OpenSettings,
            TrayCommand::Quit,
        ];
        ALL.into_iter().find(|c| c.id() == id.0.as_str())
    }
}

/// What the tray icon is indicating.
///
/// §6: "an agent run outlives the panel … the tray icon indicates activity".
/// That is the tray's only job beyond being a menu, so it is the only state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrayState {
    /// No work in flight.
    #[default]
    Idle,
    /// At least one agent run is active.
    Busy,
    /// Something needs the user: an approval, or a revoked permission (§17).
    Attention,
}

/// The live tray icon and its menu.
///
/// Not `Send`: on macOS this owns an `NSStatusItem`, which is main-thread only.
/// It is held in [`crate::app::Aibo`], which lives on the event-loop thread.
pub struct Tray {
    icon: TrayIcon,
    state: TrayState,
}

impl std::fmt::Debug for Tray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tray")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// Build the tray icon and its menu.
///
/// **Call this from the first `update` tick, never from `boot`** — see the
/// module docs. On macOS it must additionally run on the main thread, which the
/// first `update` tick guarantees and a `tokio` task does not.
pub fn create() -> Result<Tray> {
    let menu = build_menu()?;
    let icon = TrayIconBuilder::new()
        .with_id("aibo.tray")
        .with_menu(Box::new(menu))
        .with_tooltip(i18n::t(Key::TrayTooltip))
        .with_icon(state_icon(TrayState::Idle))
        // macOS renders template images in the menu bar tint, so the icon
        // follows light/dark and the "reduce transparency" setting for free.
        .with_icon_as_template(cfg!(target_os = "macos"))
        // Left click shows the menu rather than opening the panel: the hotkey
        // is the way in, and a stray click should never capture context.
        .with_menu_on_left_click(true)
        .build()
        .map_err(|e| UiError::Tray(e.to_string()))?;

    Ok(Tray {
        icon,
        state: TrayState::Idle,
    })
}

fn build_menu() -> Result<Menu> {
    let menu = Menu::new();
    let items: [&dyn muda::IsMenuItem; 6] = [
        &MenuItem::with_id(
            TrayCommand::OpenPanel.id(),
            i18n::t(Key::TrayOpenPanel),
            true,
            None,
        ),
        &MenuItem::with_id(
            TrayCommand::ShowTasks.id(),
            i18n::t(Key::TrayTasks),
            true,
            None,
        ),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(
            TrayCommand::OpenSettings.id(),
            i18n::t(Key::TraySettings),
            true,
            None,
        ),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(TrayCommand::Quit.id(), i18n::t(Key::TrayQuit), true, None),
    ];
    menu.append_items(&items)
        .map_err(|e| UiError::Tray(e.to_string()))?;
    Ok(menu)
}

impl Tray {
    /// The current indicator state.
    pub fn state(&self) -> TrayState {
        self.state
    }

    /// Update the indicator. Cheap and idempotent — safe to call from every
    /// `update` tick that changes run state.
    pub fn set_state(&mut self, state: TrayState) {
        if self.state == state {
            return;
        }
        self.state = state;
        let _ = self.icon.set_icon(Some(state_icon(state)));
        let tooltip = match state {
            TrayState::Idle => i18n::t(Key::TrayTooltip),
            TrayState::Busy | TrayState::Attention => i18n::t(Key::TrayBusy),
        };
        let _ = self.icon.set_tooltip(Some(tooltip));
    }

    /// Rebuild the menu after a language change, preserving the ids.
    pub fn relocalise(&self) -> Result<()> {
        let menu = build_menu()?;
        self.icon.set_menu(Some(Box::new(menu)));
        let _ = self.icon.set_tooltip(Some(i18n::t(Key::TrayTooltip)));
        Ok(())
    }
}

/// Install a process-wide handler forwarding tray-menu activations to `sink`.
///
/// Called once, next to [`crate::hotkey::forward_events`], so both shell event
/// sources reach the iced subscription by the same route.
pub fn forward_events(sink: impl Fn(TrayCommand) + Send + Sync + 'static) {
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(command) = TrayCommand::from_id(&event.id) {
            sink(command);
        }
    }));
}

/// Build the platform tray glyph.
///
/// Windows uses the existing app artwork so the notification-area entry is a
/// recognisable aibo icon on both light and dark taskbars. macOS keeps a
/// template glyph because AppKit supplies the correct menu-bar tint itself.
#[cfg(target_os = "windows")]
fn state_icon(state: TrayState) -> Icon {
    let rgba = windows_icon_rgba(state);
    Icon::from_rgba(rgba, 32, 32).expect("bundled Windows tray icon is well-formed")
}

#[cfg(target_os = "windows")]
fn windows_icon_rgba(state: TrayState) -> Vec<u8> {
    const SIZE: u32 = 32;
    let source = image::load_from_memory_with_format(
        include_bytes!("../../../assets-src/aibo-icon-1024.png"),
        image::ImageFormat::Png,
    )
    .expect("bundled aibo icon is a valid PNG");
    let mut rgba = source
        .resize_exact(SIZE, SIZE, image::imageops::FilterType::Lanczos3)
        .to_rgba8()
        .into_raw();

    let badge = match state {
        TrayState::Idle => None,
        // The UI palette's success and danger colours. The dark outline keeps
        // the badge separate from both the artwork and either taskbar theme.
        TrayState::Busy => Some([0x5A, 0xD1, 0x9A, 0xFF]),
        TrayState::Attention => Some([0xE5, 0x53, 0x4B, 0xFF]),
    };
    if let Some(fill) = badge {
        paint_badge(&mut rgba, SIZE, fill);
    }
    rgba
}

#[cfg(target_os = "windows")]
fn paint_badge(rgba: &mut [u8], size: u32, fill: [u8; 4]) {
    let centre = (size as i32 - 6, size as i32 - 6);
    for y in (centre.1 - 5)..=(centre.1 + 5) {
        for x in (centre.0 - 5)..=(centre.0 + 5) {
            let distance_squared = (x - centre.0).pow(2) + (y - centre.1).pow(2);
            let colour = if distance_squared <= 16 {
                Some(fill)
            } else if distance_squared <= 25 {
                Some([0x0C, 0x0D, 0x12, 0xFF])
            } else {
                None
            };
            if let Some(colour) = colour {
                let index = ((y as u32 * size + x as u32) * 4) as usize;
                rgba[index..index + 4].copy_from_slice(&colour);
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn state_icon(state: TrayState) -> Icon {
    const SIZE: u32 = 32;
    let rgba = template_icon_rgba(state, SIZE);
    Icon::from_rgba(rgba, SIZE, SIZE).expect("generated icon is well-formed")
}

/// A monochrome version of the aibo app mark for native template rendering.
///
/// Passing the full square app icon as a macOS template would reduce it to an
/// opaque rounded square. This keeps the recognisable amber rail and its two
/// horizon strokes as silhouette/alpha, letting AppKit tint it correctly for
/// either menu-bar appearance. Activity remains a deliberately small badge so
/// it does not turn aibo into an unrelated generic status circle.
#[cfg(not(target_os = "windows"))]
fn template_icon_rgba(state: TrayState, size: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let scale = size as f32 / 32.0;

    for y in 0..size {
        for x in 0..size {
            let px = (x as f32 + 0.5) / scale;
            let py = (y as f32 + 0.5) / scale;

            let rail = rounded_rect_coverage(px, py, 7.0, 16.0, 2.35, 11.0, 2.0);
            let upper = rounded_rect_coverage(px, py, 18.5, 10.0, 8.5, 1.45, 1.35) * 0.82;
            let lower = rounded_rect_coverage(px, py, 17.0, 21.0, 7.0, 1.45, 1.35) * 0.64;
            let mark = rail.max(upper).max(lower);

            let badge = match state {
                TrayState::Idle => 0.0,
                TrayState::Busy => {
                    let outer = circle_coverage(px, py, 26.0, 25.0, 3.75);
                    let inner = circle_coverage(px, py, 26.0, 25.0, 1.65);
                    outer * (1.0 - inner)
                }
                TrayState::Attention => circle_coverage(px, py, 26.0, 25.0, 3.75),
            };
            let alpha = (mark.max(badge) * 255.0).round() as u8;

            let index = ((y * size + x) * 4) as usize;
            // The RGB value is irrelevant to template rendering; white makes
            // the raw icon useful on other Unix status areas too.
            rgba[index] = 255;
            rgba[index + 1] = 255;
            rgba[index + 2] = 255;
            rgba[index + 3] = alpha;
        }
    }

    rgba
}

#[cfg(not(target_os = "windows"))]
fn rounded_rect_coverage(
    x: f32,
    y: f32,
    centre_x: f32,
    centre_y: f32,
    half_width: f32,
    half_height: f32,
    radius: f32,
) -> f32 {
    let qx = (x - centre_x).abs() - (half_width - radius);
    let qy = (y - centre_y).abs() - (half_height - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let inside = qx.max(qy).min(0.0);
    let signed_distance = outside + inside - radius;
    (0.75 - signed_distance).clamp(0.0, 1.0)
}

#[cfg(not(target_os = "windows"))]
fn circle_coverage(x: f32, y: f32, centre_x: f32, centre_y: f32, radius: f32) -> f32 {
    let distance = ((x - centre_x).powi(2) + (y - centre_y).powi(2)).sqrt();
    (radius + 0.75 - distance).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_round_trip() {
        for command in [
            TrayCommand::OpenPanel,
            TrayCommand::ShowTasks,
            TrayCommand::OpenSettings,
            TrayCommand::Quit,
        ] {
            let id = MenuId::new(command.id());
            assert_eq!(TrayCommand::from_id(&id), Some(command));
        }
        assert_eq!(TrayCommand::from_id(&MenuId::new("nope")), None);
    }

    #[test]
    fn the_generated_icon_is_valid_at_every_state() {
        for state in [TrayState::Idle, TrayState::Busy, TrayState::Attention] {
            let _ = state_icon(state);
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn the_template_icon_uses_the_aibo_mark_and_distinct_status_badges() {
        let idle = template_icon_rgba(TrayState::Idle, 32);
        let busy = template_icon_rgba(TrayState::Busy, 32);
        let attention = template_icon_rgba(TrayState::Attention, 32);

        let alpha = |pixels: &[u8], x: usize, y: usize| pixels[(y * 32 + x) * 4 + 3];
        assert_eq!(alpha(&idle, 0, 0), 0, "template corners stay transparent");
        assert!(
            alpha(&idle, 7, 16) > 240,
            "the vertical aibo rail is present"
        );
        assert!(alpha(&idle, 18, 10) > 160, "the upper horizon is present");
        assert!(alpha(&idle, 17, 21) > 120, "the lower horizon is present");
        assert_ne!(idle, busy, "busy adds a status ring");
        assert_ne!(busy, attention, "attention fills the status badge");
    }
}
