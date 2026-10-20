//! macOS: ScreenCaptureKit, the CoreGraphics event tap and AppKit, from `crate::capture` and
//! friends.

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use crate::capture::{
    ScreenCaptureSession, SharedCaptureTelemetry, accessibility_granted, current_mouse_screen,
    discover_target, discover_target_at, frontmost_app_info, install_global_hotkeys,
    list_selectable_windows, modifier_flags, record_external_frontmost_app, region_target,
    screen_capture_usable, selectable_window_at, set_escape_hotkey_enabled,
    shortcut_from_key_and_mods, shortcut_parts, start_global_input_tap, update_global_shortcut,
};
pub use crate::laser_overlay::MetalLaserOverlay;
use crate::model::TargetInfo;
pub use crate::phone::PhoneSession;
pub use crate::windows::WindowLogger;

/// The review window's button that opens a bundle's folder, and what it says once it has.
pub const SHOW_IN_FILE_MANAGER: &str = "Show in Finder";
pub const OPENED_IN_FILE_MANAGER: &str = "Opened in Finder";

/// Starts recording `target` (into `video`, if given), leaving Scrawl's own windows out.
pub fn start_screen_session(
    target: TargetInfo,
    video: Option<PathBuf>,
    telemetry: &SharedCaptureTelemetry,
) -> ScreenCaptureSession {
    ScreenCaptureSession::start(target, video, Arc::clone(&telemetry.excluded_window_ids))
}

/// Ends a recording whose bundle is about to be deleted.
pub fn discard_screen_session(session: ScreenCaptureSession) {
    let _ = session.stop();
}

/// Opens `dir` in Finder.
pub fn open_in_file_manager(dir: &Path) {
    let _ = std::process::Command::new("/usr/bin/open").arg(dir).spawn();
}
