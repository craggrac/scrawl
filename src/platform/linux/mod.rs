//! Linux (Hyprland): the surface `crate::ui` calls, on the phase 2/3 recorders (`wf-recorder`,
//! the microphone, evdev input, the Hyprland window log) and Hyprland's IPC socket. What Linux
//! doesn't have yet (the ink overlay, the iPhone mirror, the native capsule) are stand-ins that
//! can't be built, so the paths using them are never taken.

pub mod chrome;
pub mod overlay;
pub mod picker_dim;
mod session;
mod shortcut;
mod standins;
mod telemetry;
pub mod tray;

use std::path::Path;

pub use self::session::{
    ScreenCaptureSession, discard_screen_session, recorders_running, start_screen_session,
};
pub use self::shortcut::{
    PressSource, ShortcutConfig, save_in as save_record_shortcut, shortcut_from_key_and_mods,
};
pub use self::overlay::MetalLaserOverlay;
pub use self::standins::{NativePill, PhoneSession, PillAction};
pub use self::telemetry::SharedCaptureTelemetry;
use crate::capture::scrawl_log;
pub use crate::capture::{selectable_window_at, shortcut_parts};
pub use crate::linux::context::Collector as ContextCollector;
pub use crate::linux::hypr::{
    WindowLogger, frontmost_app_info, list_selectable_windows,
    target_under_point as discover_target_at,
};
use crate::model::TargetInfo;

/// The review window's button that opens a bundle's folder, and what it says once it has.
pub const SHOW_IN_FILE_MANAGER: &str = "Show in Files";
pub const OPENED_IN_FILE_MANAGER: &str = "Opened in Files";

/// Opens `dir` in the file manager.
pub fn open_in_file_manager(dir: &Path) {
    let mut child = match std::process::Command::new("xdg-open").arg(dir).spawn() {
        Ok(child) => child,
        Err(error) => return scrawl_log(format!("xdg-open {}: {error}", dir.display())),
    };
    // Reaped off the UI thread, so it doesn't stay a zombie.
    let spawned = std::thread::Builder::new()
        .name("xdg-open".into())
        .spawn(move || match child.wait() {
            Ok(status) if !status.success() => scrawl_log(format!("xdg-open: {status}")),
            Ok(_) => {}
            Err(error) => scrawl_log(format!("xdg-open: {error}")),
        });
    if let Err(error) = spawned {
        scrawl_log(format!("xdg-open: could not wait for it: {error}"));
    }
}

/// The pointer in global points.
pub fn current_mouse_screen() -> Option<[f32; 2]> {
    crate::screens::pointer().map(|[x, y]| [x as f32, y as f32])
}

/// The newest window matching `window_query`, else the monitor under the pointer.
pub fn discover_target(window_query: Option<&str>) -> TargetInfo {
    discover_target_at(window_query, crate::screens::pointer())
}

/// An area dragged in the picker, as a region target (`hypr::region_target_on`).
pub fn region_target(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    windows: &[TargetInfo],
) -> TargetInfo {
    crate::linux::hypr::region_target_on(
        &crate::screens::displays(),
        [x, y, width, height],
        windows,
        frontmost_app_info(),
    )
}

/// Nothing to remember: `frontmost_app_info` reads Hyprland's focus history.
pub fn record_external_frontmost_app() {}

/// The modifiers held now, as `linux::hotkey` last saw them (0 when it isn't running, so a
/// held shortcut then counts as a tap).
pub fn modifier_flags() -> u64 {
    crate::linux::hotkey::modifier_bits()
}

/// Esc comes from `start_global_input_tap`'s listener whenever it runs.
pub fn set_escape_hotkey_enabled(_enabled: bool) {}

/// The capture shortcut is a Hyprland bind (`scrawl toggle`), not a hotkey Scrawl registers.
pub fn install_global_hotkeys(_telemetry: &std::sync::Arc<SharedCaptureTelemetry>) {}

/// Listens to the evdev keyboards for the app's lifetime: the capture shortcut's presses (with
/// the modifiers, so holding it opens the switcher) and Esc. Clicks and keys for a capture are
/// recorded separately, per capture (`linux::input::InputRecorder`).
pub fn start_global_input_tap(telemetry: std::sync::Arc<SharedCaptureTelemetry>) {
    use crate::linux::hotkey::{self, KeyAction};
    // The shortcut saved from the review window, which the bind should match.
    *telemetry
        .shortcut
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = shortcut::load_saved();
    let matcher = std::sync::Arc::clone(&telemetry);
    hotkey::start(
        move |code, cmd, ctrl, alt, shift| matcher.matches_shortcut(code, cmd, ctrl, alt, shift),
        move |action| {
            // The listener reads the keyboards under the lock screen too; what is typed into
            // hyprlock is not for Scrawl. Asked only when the shortcut or Esc goes down.
            if action == KeyAction::None || crate::linux::hypr::session_locked() {
                return;
            }
            match action {
                KeyAction::Shortcut => telemetry.press_shortcut_from(PressSource::Keys),
                KeyAction::Escape => telemetry.note_escape(),
                KeyAction::None => {}
            }
        },
    );
}

/// The record shortcut recorded in the review window. It is saved in the config file and is
/// what the listener matches (and the capsule names); the key that starts a recording from
/// anywhere is the Hyprland bind, which Scrawl doesn't edit, so that has to match.
pub fn update_global_shortcut(telemetry: &SharedCaptureTelemetry, config: ShortcutConfig) {
    scrawl_log(format!(
        "Record shortcut is now {}; change the Hyprland bind to match",
        config.label
    ));
    if let Err(error) = shortcut::save_in(&crate::handoff::default_config_path(), &config) {
        scrawl_log(format!("Saving the record shortcut failed: {error:#}"));
    }
    *telemetry
        .shortcut
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
}

/// The native (Metal) capsule is macOS only; the GPUI one draws on Linux.
pub fn native_pill() -> bool {
    false
}

/// No iPhone mirror on Linux.
pub fn phone_devices() -> anyhow::Result<Vec<crate::helper::PhoneDevice>> {
    Ok(Vec::new())
}
