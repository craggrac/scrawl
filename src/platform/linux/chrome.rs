//! Scrawl's windows on Hyprland. GPUI can't place, pin or unfocus a Wayland window, so
//! session-only window rules do it (`linux::hypr::install_window_rules`, matched on each
//! window's app id), and the capsule draws its own shell (no `ShellLayer`). Giving the keys
//! back after the picker or the review window focuses the window that had them.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use gpui::{App, Entity};

use crate::linux::hypr;
use crate::ui::motion::Xf;
use crate::ui::{BundleReviewWindow, ScrawlCoordinator, TargetPickerWindow};

/// The platform window under one of Scrawl's GPUI windows: none to reach on Wayland.
pub enum NativeWindow {}

/// The capsule's Core Animation shell (macOS).
pub enum ShellLayer {}

/// How long `pointer_on_hud_pill`'s answer is reused.
const POINTER_CHECK: Duration = Duration::from_millis(100);
static LAST_POINTER_CHECK: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Hyprland's address of the window that had the keys before Scrawl took them.
static PREVIOUS_FRONT_WINDOW: Mutex<Option<String>> = Mutex::new(None);

/// GNOME's and KDE's reduce-motion settings aren't read yet.
pub fn reduce_motion() -> bool {
    false
}

/// The tray item comes with task 4.
pub fn set_status_inbox_count(_count: usize) {}

pub fn find_hud_pill_ns_window() -> Option<NativeWindow> {
    None
}

pub fn find_picker_ns_window() -> Option<NativeWindow> {
    None
}

/// Whether the pointer is over the capsule's window (its Hyprland client). The UI loop asks
/// every few ms while the switch chips show, so an answer is reused for `POINTER_CHECK`.
pub fn pointer_on_hud_pill() -> bool {
    let mut last = LAST_POINTER_CHECK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, on)) = *last
        && at.elapsed() < POINTER_CHECK
    {
        return on;
    }
    let on = crate::screens::pointer().is_some_and(|[x, y]| {
        hypr::clients().iter().any(|c| {
            c.class == hypr::PILL_CLASS
                && crate::screens::contains(
                    [
                        f64::from(c.at[0]),
                        f64::from(c.at[1]),
                        f64::from(c.size[0]),
                        f64::from(c.size[1]),
                    ],
                    [x, y],
                )
        })
    });
    *last = Some((Instant::now(), on));
    on
}

pub fn apply_pill_layer_effect(
    ns_window: &NativeWindow,
    _shell_slot: &mut Option<ShellLayer>,
    _pill_width: f32,
    _pill_height: f32,
    _scale: f32,
    _y_offset: f32,
    _opacity: f32,
    _is_completed: bool,
    _blur_radius: f32,
    _content_shrink: f32,
) {
    match *ns_window {}
}

pub fn trace_capsule(
    ns_window: &NativeWindow,
    _width: f32,
    _scale: f32,
    _y_off: f32,
    _opacity: f32,
    _content_alpha: f32,
    _blur_radius: f32,
    _content_shrink: f32,
) -> Xf {
    match *ns_window {}
}

pub fn move_window_to_screen(ns_win: &NativeWindow, _frame: [f64; 4]) {
    match *ns_win {}
}

/// The picker window's rule places it; its dim would cover a capsule that is already open, so
/// that one is raised over the picker once the picker is up.
pub fn configure_picker_window(
    _picker_coord: &Entity<ScrawlCoordinator>,
    _lab: bool,
    _cx: &mut App,
) {
    let spawned = std::thread::Builder::new()
        .name("scrawl-raise-pill".into())
        .spawn(hypr::keep_pill_over_picker);
    if let Err(error) = spawned {
        crate::capture::scrawl_log(format!("Could not raise the capsule over the picker: {error}"));
    }
}

pub fn bring_pill_forward(ns_win: &NativeWindow) {
    match *ns_win {}
}

pub fn configure_pill_window(
    ns_win: &NativeWindow,
    _shell_layer: &mut Option<ShellLayer>,
    _pill_coord: &Entity<ScrawlCoordinator>,
    _cx: &mut App,
) {
    match *ns_win {}
}

/// Called just before Scrawl takes the keys: remembers the focused window unless it is
/// Scrawl's own.
pub fn remember_front_app() {
    let Some(active) = hypr::active_window() else {
        return;
    };
    if active.pid != std::process::id() as i32 {
        *PREVIOUS_FRONT_WINDOW
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(active.address);
    }
}

/// Run (deferred) after any window closes: once neither a picker nor a review window is left
/// and one of Scrawl's windows still has the keys, the window that had them before gets them
/// back.
pub fn hand_back_keys(cx: &mut App) {
    if cx.windows().iter().any(|handle| {
        handle.downcast::<TargetPickerWindow>().is_some()
            || handle.downcast::<BundleReviewWindow>().is_some()
    }) {
        return;
    }
    let previous = PREVIOUS_FRONT_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let ours = hypr::active_window().is_none_or(|w| w.pid == std::process::id() as i32);
    if let Some(address) = previous
        && ours
    {
        hypr::focus_window(&address);
    }
}

/// `--screenshot`: Scrawl's largest window (the review window), with `grim`.
pub fn capture_own_window_screenshot(out_path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let own = std::process::id() as i32;
    let window = hypr::clients()
        .into_iter()
        .filter(|c| c.pid == own && c.mapped)
        .max_by_key(|c| i64::from(c.size[0]) * i64::from(c.size[1]))
        .context("Could not find a Scrawl window")?;
    let geometry = format!(
        "{},{} {}x{}",
        window.at[0], window.at[1], window.size[0], window.size[1]
    );
    let status = std::process::Command::new("grim")
        .args(["-g", &geometry])
        .arg(out_path)
        .status()
        .context("grim is required for --screenshot")?;
    anyhow::ensure!(status.success(), "grim failed");
    Ok(())
}
