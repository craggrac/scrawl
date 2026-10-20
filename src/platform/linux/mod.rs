//! Linux (Hyprland): the surface `crate::ui` calls, on the phase 2/3 recorders (`wf-recorder`,
//! the microphone, evdev input, the Hyprland window log) and Hyprland's IPC socket. What Linux
//! doesn't have yet (the ink overlay, the iPhone mirror, the native capsule) are stand-ins that
//! can't be built, so the paths using them are never taken.

pub mod chrome;
mod session;
mod shortcut;
mod standins;
mod telemetry;

use std::path::Path;

pub use self::session::{ScreenCaptureSession, discard_screen_session, start_screen_session};
pub use self::shortcut::{ShortcutConfig, shortcut_from_key_and_mods};
pub use self::standins::{MetalLaserOverlay, NativePill, PhoneSession, PillAction};
pub use self::telemetry::SharedCaptureTelemetry;
use crate::capture::scrawl_log;
pub use crate::capture::{selectable_window_at, shortcut_parts};
pub use crate::linux::hypr::{
    WindowLogger, discover_target_at, frontmost_app_info, list_selectable_windows,
};
use crate::model::TargetInfo;

/// The review window's button that opens a bundle's folder, and what it says once it has.
pub const SHOW_IN_FILE_MANAGER: &str = "Show in Files";
pub const OPENED_IN_FILE_MANAGER: &str = "Opened in Files";

/// Opens `dir` in the file manager.
pub fn open_in_file_manager(dir: &Path) {
    if let Err(error) = std::process::Command::new("xdg-open").arg(dir).spawn() {
        scrawl_log(format!("xdg-open {}: {error}", dir.display()));
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

/// No modifier state is read yet, so a held shortcut counts as released at once: the capture
/// shortcut is a tap that records the screen (hold-to-switch needs the evdev key-up, task 4).
pub fn modifier_flags() -> u64 {
    0
}

/// Esc reaches Scrawl's own windows only.
pub fn set_escape_hotkey_enabled(_enabled: bool) {}

/// The capture shortcut is a Hyprland bind (`scrawl toggle`), not a hotkey Scrawl registers.
pub fn install_global_hotkeys(_telemetry: &std::sync::Arc<SharedCaptureTelemetry>) {}

/// Input is recorded per capture (`linux::input::InputRecorder`), not by a global tap.
pub fn start_global_input_tap(_telemetry: std::sync::Arc<SharedCaptureTelemetry>) {}

/// The record shortcut recorded in the review window. It only renames the shortcut here: the
/// key itself is the Hyprland bind, which Scrawl doesn't edit.
pub fn update_global_shortcut(telemetry: &SharedCaptureTelemetry, config: ShortcutConfig) {
    scrawl_log(format!(
        "Record shortcut is now {}; change the Hyprland bind to match",
        config.label
    ));
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
