use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Application, Bounds, BoxShadow, Context, Div, Entity, FocusHandle, FontWeight,
    Image, ImageFormat, IntoElement, KeyDownEvent, Keystroke, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, SharedString, Size, TextRun, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, img, point,
    prelude::*, px, relative, rgb, rgba, size, svg,
};
use crate::audio::AudioRecorder;
use crate::capture::{LiveOverlayEvent, scrawl_log};
use crate::helper::PhoneDevice;
use crate::platform::{
    MetalLaserOverlay, PhoneSession, ScreenCaptureSession, SharedCaptureTelemetry,
    current_mouse_screen, discover_target, install_global_hotkeys, list_selectable_windows,
    record_external_frontmost_app, region_target, selectable_window_at,
    set_escape_hotkey_enabled, shortcut_from_key_and_mods, start_global_input_tap,
    update_global_shortcut,
};
use crate::model::{CaptureMode, ScrawlBundle, TargetFrame, TargetInfo, format_clock};
pub use crate::selection::SelectionMode;
use crate::sketch::timeline;

const SIDEBAR_BG: u32 = 0x111111;
const CANVAS_BG: u32 = 0x161616;
const SURFACE_BG: u32 = 0x1d1d1d;
const SURFACE_HOVER: u32 = 0x252525;
const SURFACE_SELECTED: u32 = 0x2e2e2e;
const LINE_COLOR: u32 = 0x2a2a2a;
const TEXT_COLOR: u32 = 0xf2f2f2;
const TEXT_SOFT: u32 = 0xd0d0d0;
const MUTED_COLOR: u32 = 0x969696;
const ACCENT_COLOR: u32 = 0x3b5cf6;
const ACCENT_HOVER: u32 = 0x4b6bff;
const INK_RED: u32 = 0xff3b30;
const POSITIVE_COLOR: u32 = 0x69d89f;

// Dynamic Island capsule geometry. GPUI boxes are border-box, so the hairline border is part of
// the inset: R_outer (20) = R_chip (16) + border (1) + pad (3), on every side.
const ISLAND_H: f32 = 40.0;
/// Width the recording pill opens from when a recording starts without the picker.
const ISLAND_OPEN_FROM_W: f32 = 120.0;
const ISLAND_RADIUS: f32 = ISLAND_H / 2.0;
const ISLAND_INSET: f32 = 4.0;
const ISLAND_PAD: f32 = ISLAND_INSET - 1.0;
const CHIP_H: f32 = ISLAND_H - 2.0 * ISLAND_INSET;
const CHIP_RADIUS: f32 = CHIP_H / 2.0;
const CHIP_PX: f32 = 12.0;
const CHIP_GAP: f32 = 2.0;
const DIVIDER_MX: f32 = 6.0;
const LEAD_TRAIL: f32 = 8.0;
const HUD_TEXT: f32 = 13.0;
// Selection-mode segments: line icon, then label.
const SEG_PX: f32 = 10.0;
const SEG_ICON: f32 = 14.0;
const SEG_ICON_GAP: f32 = 5.0;
const PILL_WINDOW_W: f32 = 520.0;
const PILL_WINDOW_H: f32 = 84.0;
/// The capsule window's top, below the top of its display.
const PILL_WINDOW_TOP: f32 = 34.0;
const REVIEW_WINDOW_W: f32 = 1120.0;
const REVIEW_WINDOW_H: f32 = 740.0;
/// Wayland app ids of Scrawl's windows: Hyprland's window rules match them (macOS ignores
/// them).
const PILL_APP_ID: &str = "scrawl-pill";
const PICKER_APP_ID: &str = "scrawl-picker";
const REVIEW_APP_ID: &str = "scrawl-review";

#[cfg(target_os = "macos")]
mod appkit;
mod lab;
pub(crate) mod motion;

#[cfg(target_os = "macos")]
use appkit::*;
#[cfg(target_os = "linux")]
use crate::platform::chrome::*;

pub use lab::{Options as LabOptions, Scene as LabScene, run as run_lab};
pub(crate) use motion::{TuneAction, tune_command};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Spring {
    /// Visual duration in seconds.
    response: f32,
    bounce: f32,
}

impl Spring {
    /// Kit's duration + bounce: ω = 2π / duration, ζ = 1 − bounce.
    const fn visual(duration: f32, bounce: f32) -> Self {
        Self {
            response: duration,
            bounce,
        }
    }
}

// The capsule's springs and durations live in `motion::Tune` (defaults in `Tune::DEFAULT`, live
// overrides from `~/.config/scrawl/tune.json` in debug builds; `scrawl tune`).
//
// Switch chips on a tap-started recording (`crate::hold::Reveal`). One shape: the recording pill
// grows into the chip row and folds back. The width leads the morph palette (.42/.16 open,
// .38/.08 close) on a slightly faster spring; the lead (dot + clock) stays pinned to the left
// edge, and each trailing item fades in as the moving edge uncovers it, on the same frame. The
// chips ↔ controls swap fades in place with a sine blur pulse (sharp at both ends) and no content
// scale, so the pinned lead never travels.

/// Which width spring the current morph uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Morph {
    /// A phase change (`Tune::island`).
    Phase,
    /// The switch chips opening (`Tune::reveal_open`).
    RevealOpen,
    /// The switch chips folding back (`Tune::reveal_close`).
    RevealClose,
    /// A switch chip turning the recording into the picker (`Tune::switch_width`).
    Switch,
}

impl Morph {
    fn spring(self, tune: &motion::Tune) -> Spring {
        match self {
            Self::Phase => tune.island,
            Self::RevealOpen => tune.reveal_open,
            Self::RevealClose => tune.reveal_close,
            Self::Switch => tune.switch_width,
        }
    }
}

/// Reduced motion: no width travel, the groups only cross-fade over this long.
const REDUCED_FADE_S: f32 = 0.18;
/// The picker's highlight moving from one mode to another cross-fades over this long.
const HIGHLIGHT_FADE_S: f32 = 0.18;

/// A switch chip turning the recording into the picker (`switch_recording`). The capsule stays
/// one shape: the chips are the picker's chips, so the picker row starts where they were and
/// slides to its place on `Tune::switch_slide` while the width springs on `Tune::switch_width`
/// (a touch faster, so it leads). The lead (dot and clock, frozen) and its divider ride along in
/// front of the row and leave under the capsule's left edge; Record, the divider and ✕ ride
/// behind it and fade in as the right edge uncovers them.
#[derive(Clone, Copy, Debug)]
struct SwitchMorph {
    /// The row's offset from its resting place and its velocity, in points. Measured by the
    /// first frame that draws it (text widths need the window).
    slide: Option<(f32, f32)>,
}

/// Advances a damped spring with fixed 4ms sub-steps so long frames stay stable.
fn spring_step(value: f32, velocity: f32, target: f32, dt: f32, spring: Spring) -> (f32, f32) {
    let omega = std::f32::consts::TAU / spring.response;
    let stiffness = omega * omega;
    let damping = 2.0 * (1.0 - spring.bounce) * omega;
    let steps = (dt / 0.004).ceil().max(1.0) as usize;
    let h = dt / steps as f32;
    let (mut x, mut v) = (value, velocity);
    for _ in 0..steps {
        v += ((target - x) * stiffness - v * damping) * h;
        x += v * h;
    }
    (x, v)
}

fn with_alpha(color: u32, alpha: f32) -> gpui::Rgba {
    rgba((color << 8) | (alpha.clamp(0.0, 1.0) * 255.0).round() as u32)
}

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

fn tabular_nums() -> gpui::FontFeatures {
    gpui::FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

fn tabular_font(weight: FontWeight) -> gpui::Font {
    gpui::Font {
        family: ".SystemUIFont".into(),
        features: tabular_nums(),
        fallbacks: None,
        weight,
        style: gpui::FontStyle::Normal,
    }
}

fn measure_text(window: &Window, text: &str, size: f32, weight: FontWeight) -> f32 {
    let mut font = window.text_style().font();
    font.weight = weight;
    font.features = tabular_nums();
    let run = TextRun {
        len: text.len(),
        font,
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        px(size),
        &[run],
        None,
    );
    f32::from(line.width).ceil()
}

fn chip_width(window: &Window, label: &str, weight: FontWeight) -> f32 {
    measure_text(window, label, HUD_TEXT, weight) + 2.0 * CHIP_PX
}

fn clock_template(clock: &str) -> String {
    clock
        .chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect()
}

#[cfg(target_os = "macos")]
#[allow(dead_code)]
fn completed_meta(_duration_ms: u64, frame_count: usize) -> String {
    format!(
        "· {} {} · {}",
        frame_count,
        if frame_count == 1 { "frame" } else { "frames" },
        crate::settings::paste_label()
    )
}

/// Lead text of the Completed island: local captures were copied (or sent, as `note` says);
/// iPhone ones count the inbox.
fn completed_label(inbox_count: usize, note: Option<&str>) -> String {
    match inbox_count {
        0 => note.unwrap_or("Copied").to_owned(),
        1 => "From iPhone".to_owned(),
        n => format!("{n} from iPhone"),
    }
}

/// How long a smart-routed capture waits, showing "Sending to …", before it is sent.
#[cfg(target_os = "macos")]
const SMART_SEND_UNDO: Duration = Duration::from_millis(3_000);

/// A session title short enough for the island.
fn island_title(title: &str) -> String {
    const MAX: usize = 24;
    if title.chars().count() > MAX {
        format!(
            "{}…",
            title.chars().take(MAX - 1).collect::<String>().trim_end()
        )
    } else {
        title.to_owned()
    }
}

/// The Completed island's lead once a local capture's send is done.
fn completed_delivery_note(delivery: &crate::delivery::Delivery, title: &str) -> String {
    match delivery {
        crate::delivery::Delivery::Sent { .. } => format!("Sent to {title}"),
        crate::delivery::Delivery::Queued { .. } => format!("Copied · couldn't reach {title}"),
        crate::delivery::Delivery::Saved => "Copied".to_owned(),
    }
}

/// Inner width of the Selecting row: mode segments, an optional Record chip, divider, and ✕.
fn selecting_row_width(label_widths: &[f32], record_width: Option<f32>) -> f32 {
    let segments: f32 = label_widths
        .iter()
        .map(|label| label + 2.0 * SEG_PX + SEG_ICON + SEG_ICON_GAP)
        .sum();
    let items = label_widths.len() + usize::from(record_width.is_some()) + 2;
    segments
        + record_width.unwrap_or(0.0)
        + 1.0
        + 2.0 * DIVIDER_MX
        + CHIP_H
        + (items - 1) as f32 * CHIP_GAP
}

fn segment_width(window: &Window, mode: SelectionMode) -> f32 {
    measure_text(window, mode.label(), HUD_TEXT, FontWeight::MEDIUM)
        + 2.0 * SEG_PX
        + SEG_ICON
        + SEG_ICON_GAP
}

/// Widths of the items after a Recording capsule's divider: the switch chips while the reveal
/// is open (`crate::hold::Reveal`), else Undo · Cancel · Finish.
fn recording_trail_widths(window: &Window, chips: bool, iphone: bool) -> Vec<f32> {
    if chips {
        SelectionMode::available(iphone)
            .iter()
            .map(|&mode| segment_width(window, mode))
            .collect()
    } else {
        vec![
            chip_width(window, "Undo", FontWeight::MEDIUM),
            chip_width(window, "Cancel", FontWeight::MEDIUM),
            chip_width(window, "Finish", FontWeight::SEMIBOLD),
        ]
    }
}

/// Trace names for a Recording capsule's trailing group, with each item's opacity.
fn trail_keys(
    chips: bool,
    iphone: bool,
    alphas: &[f32],
    prefix: &str,
) -> Vec<Option<(String, f32)>> {
    let names: Vec<String> = if chips {
        SelectionMode::available(iphone)
            .iter()
            .map(|m| format!("chip:{}", m.label()))
            .collect()
    } else {
        ["undo", "cancel", "finish"].map(String::from).to_vec()
    };
    names
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            Some((
                format!("{prefix}{n}"),
                alphas.get(i).copied().unwrap_or(1.0),
            ))
        })
        .collect()
}

/// Left edges of a trailing group's items, starting at `x0`.
fn trail_offsets(x0: f32, widths: &[f32]) -> Vec<f32> {
    widths
        .iter()
        .scan(x0, |x, w| {
            let left = *x;
            *x += w + CHIP_GAP;
            Some(left)
        })
        .collect()
}

/// How much of an item at `x` (width `w`) the capsule's moving right edge has uncovered, eased:
/// trailing items fade in as the pill grows into them and out as it folds over them.
fn edge_alpha(inner_now: f32, x: f32, w: f32) -> f32 {
    let t = ((inner_now - x) / w.max(1.0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The switch chips' clock: the animation clock, so they stay open as long in slow motion
/// (`motion::time_scale`). The held switcher keeps the real clock: it times key presses.
fn reveal_clock_ms() -> u64 {
    (motion::now() * 1000.0) as u64
}

/// An item of the picker row sliding in from under the capsule's right edge (`outer`, the
/// shell's outer edge): invisible while the edge would still cut into its middle, opaque once
/// it is clear, so nothing shows half cut.
fn uncover_alpha(outer: f32, x: f32, w: f32) -> f32 {
    let inside = (outer - x) / w.max(1.0);
    let t = ((inside - 0.7) / 0.4).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Intrinsic outer width of the capsule for a phase, mirroring the layout in `HudPillWindow`.
/// `hold`: the held switcher, which always lists iPhone and records on release (no Record chip).
/// `chips`: a Recording capsule showing its switch chips.
fn island_content_width(
    window: &Window,
    phase: IslandPhase,
    clock: &str,
    iphone: bool,
    hold: bool,
    chips: bool,
    completed_note: Option<&str>,
) -> f32 {
    let divider = 1.0 + 2.0 * DIVIDER_MX;
    let inner = match phase {
        IslandPhase::Selecting { mode } => {
            let labels: Vec<f32> = SelectionMode::available(iphone || hold)
                .iter()
                .map(|m| measure_text(window, m.label(), HUD_TEXT, FontWeight::MEDIUM))
                .collect();
            let record = (mode.shows_record() && !hold)
                .then(|| chip_width(window, "Record", FontWeight::SEMIBOLD));
            selecting_row_width(&labels, record)
        }
        IslandPhase::Recording => {
            let trail = recording_trail_widths(window, chips, iphone);
            CHIP_H
                + measure_text(
                    window,
                    &clock_template(clock),
                    HUD_TEXT,
                    FontWeight::SEMIBOLD,
                )
                + LEAD_TRAIL
                + divider
                + trail.iter().sum::<f32>()
                + (trail.len() + 1) as f32 * CHIP_GAP
        }
        IslandPhase::Processing => {
            CHIP_H
                + 6.0
                + measure_text(window, "Processing…", HUD_TEXT, FontWeight::SEMIBOLD)
                + CHIP_PX
        }
        IslandPhase::Completed { inbox_count, .. } => {
            CHIP_H
                + 8.0
                + measure_text(
                    window,
                    &completed_label(inbox_count, completed_note),
                    HUD_TEXT,
                    FontWeight::SEMIBOLD,
                )
                + LEAD_TRAIL
                + divider
                + chip_width(window, "Paste", FontWeight::SEMIBOLD)
                + chip_width(window, "Inspect", FontWeight::MEDIUM)
                + CHIP_H
                + 3.0 * CHIP_GAP
        }
        IslandPhase::Hidden | IslandPhase::Dismissing => return ISLAND_H,
    };
    inner + 2.0 * ISLAND_INSET
}

/// The status menu's items (the tray on Linux), and the Linux control socket's `stop`.
#[derive(Clone, Debug, Eq, PartialEq)]
enum MenuAction {
    ToggleCapture,
    #[cfg(target_os = "macos")]
    ChooseTarget,
    OpenLatest,
    #[cfg(target_os = "macos")]
    OpenInbox(PathBuf),
    #[cfg(target_os = "macos")]
    ExportVideo,
    #[cfg(target_os = "macos")]
    Settings,
    #[cfg(target_os = "macos")]
    CheckForUpdates,
    /// Finish the recording if one runs (the control socket's `stop`).
    #[cfg(target_os = "linux")]
    Stop,
    /// Throw away the recording or the picker (the control socket's `cancel`).
    #[cfg(target_os = "linux")]
    Cancel,
    Quit,
}

/// Text beside the menu bar icon while a video export runs (`None` goes back to the inbox count).
#[cfg(target_os = "macos")]
static EXPORT_STATUS: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(target_os = "macos")]
static EXPORTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Exports the newest bundle to `~/Movies/Scrawl/<name>.mp4` off the main thread, reporting
/// through `EXPORT_STATUS`, then reveals the file in Finder.
#[cfg(target_os = "macos")]
fn start_video_export() {
    use std::sync::atomic::Ordering;
    if EXPORTING.swap(true, Ordering::SeqCst) {
        return;
    }
    let say = |text: Option<String>| {
        if let Ok(mut status) = EXPORT_STATUS.lock() {
            *status = text;
        }
    };
    say(Some("Exporting…".into()));
    let spawned = std::thread::Builder::new()
        .name("scrawl-export".into())
        .spawn(move || {
            let result = crate::handoff::load_bundle(std::path::Path::new("latest")).and_then(
                |(dir, bundle)| {
                    let options = crate::export::Options {
                        background: true,
                        ..Default::default()
                    };
                    crate::export::export(&dir, &bundle, &options, &mut |fraction| {
                        say(Some(format!("Exporting {}%", (fraction * 100.0) as u32)));
                    })
                },
            );
            match result {
                Ok(exported) => {
                    scrawl_log(format!("Exported {}", crate::export::describe(&exported)));
                    say(Some("Exported".into()));
                    let _ = std::process::Command::new("/usr/bin/open")
                        .arg("-R")
                        .arg(&exported.path)
                        .spawn();
                }
                Err(error) => {
                    scrawl_log(format!("Video export failed: {error:#}"));
                    say(Some("Export failed".into()));
                }
            }
            std::thread::sleep(Duration::from_secs(4));
            say(None);
            EXPORTING.store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        say(None);
        EXPORTING.store(false, Ordering::SeqCst);
    }
}

/// The content layer's scale: the capsule's, less a little per px of blur.
#[cfg(target_os = "macos")]
fn content_scale(scale: f32, blur_radius: f32, content_shrink: f32) -> f64 {
    let s = f64::from(scale.clamp(0.50, 1.15));
    (s * (1.0 - f64::from(blur_radius).min(5.0) * f64::from(content_shrink))).clamp(0.85, 1.06)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IslandPhase {
    Hidden,
    Selecting {
        mode: SelectionMode,
    },
    Recording,
    Processing,
    Completed {
        finished_at: Instant,
        frame_count: usize,
        duration_ms: u64,
        /// Unhandled iPhone captures including this one; 0 for a local capture.
        inbox_count: usize,
    },
    Dismissing,
}

impl IslandPhase {
    fn trace_name(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Selecting { .. } => "selecting",
            Self::Recording => "recording",
            Self::Processing => "processing",
            Self::Completed { .. } => "completed",
            Self::Dismissing => "dismissing",
        }
    }
}

enum CaptureSource {
    Screen {
        session: ScreenCaptureSession,
        audio: Option<AudioRecorder>,
        /// Screen mode only: which windows were where, for `scrawl autocrop`.
        windows: Option<crate::platform::WindowLogger>,
    },
    /// The helper's mirror window records the phone's native video and the narration.
    Phone(PhoneSession),
    /// `scrawl lab pill`: nothing is captured.
    Lab,
}

struct ActiveRecording {
    started: Instant,
    target: TargetInfo,
    bundle_dir: PathBuf,
    source: CaptureSource,
    _metal_overlay: Option<MetalLaserOverlay>,
}

impl ActiveRecording {
    fn elapsed_ms(&self) -> u64 {
        match &self.source {
            CaptureSource::Screen { session, .. } => session.elapsed_ms(),
            CaptureSource::Phone(session) => session.elapsed_ms(),
            CaptureSource::Lab => self.started.elapsed().as_millis() as u64,
        }
    }
}

/// Shared coordinator owned by the GPUI app.
pub struct ScrawlCoordinator {
    telemetry: Arc<SharedCaptureTelemetry>,
    active_recording: Option<ActiveRecording>,
    picker_metal_overlay: Option<MetalLaserOverlay>,
    native_pill: Option<crate::platform::NativePill>,
    iphone_preview: Option<PhoneSession>,
    island_phase: IslandPhase,
    island_prev_phase: IslandPhase,
    /// `motion::now()` when the phase last changed.
    island_phase_since: f64,
    island_dismissed_from: IslandPhase,
    island_width: f32,
    island_target_width: f32,
    island_velocity: f32,
    island_scale: f32,
    island_scale_velocity: f32,
    island_y: f32,
    island_y_velocity: f32,
    island_opacity: f32,
    /// Which width spring the current morph uses.
    island_morph: Morph,
    /// Switch chips on a tap-started recording (`crate::hold::Reveal`).
    reveal: crate::hold::Reveal,
    /// When the chips last opened or folded, for the in-place swap of the trailing group.
    reveal_since: f64,
    /// The trailing group fading out after that change: `Some(true)` the chips, `Some(false)` the
    /// controls, `None` when the capsule opened straight into its state (nothing to swap).
    reveal_outgoing: Option<bool>,
    /// The chips were open when the island last changed phase, so a Recording capsule that is
    /// blurring out or dismissing keeps them.
    reveal_prev_chips: bool,
    /// macOS Reduce motion, read when a reveal starts.
    reduced_motion: bool,
    switch: Option<SwitchMorph>,
    /// When the springs were last advanced (`step_island`).
    island_stepped: Instant,
    /// A new capsule window has drawn its first frame and not its second (which comes late):
    /// only its frames advance the springs, so the hitch slows the opening instead of eating it.
    island_await_frame: bool,
    /// The recording clock last shown, so the lead keeps its time while it leaves.
    last_elapsed_ms: u64,
    /// The highlighted mode before the last change and when it changed (`HIGHLIGHT_FADE_S`).
    highlight_prev: Option<SelectionMode>,
    highlight_since: f64,
    selectable_windows: Vec<TargetInfo>,
    selection_generation: u64,
    windows_loading: bool,
    selection_pointer: Option<[f32; 2]>,
    hovered_window: Option<TargetInfo>,
    animated_window_rect: Option<[f32; 4]>,
    selection_drag: Option<([f32; 2], [f32; 2])>,
    /// Global frame of the display the picker covers, highlighted in `SelectionMode::Screen`.
    display_rect: [f32; 4],
    paste_drag_start: Option<[f32; 2]>,
    recording_shortcut_key: bool,
    review_bundle: Option<(PathBuf, ScrawlBundle)>,
    review_status: Option<String>,
    selected_tab: ReviewTab,
    selected_keyframe: usize,
    /// A cabled iPhone, refreshed each time the picker opens.
    iphone: Option<PhoneDevice>,
    /// The bundle the Completed island is showing.
    completed_dir: Option<PathBuf>,
    /// The Completed island's lead text for a local capture sent to a session ("Sent to …"),
    /// instead of "Copied".
    completed_note: Option<String>,
    /// A smart-routed capture waiting out its undo window before it's sent; ✕ or Paste on the
    /// Completed island cancels it.
    pending_send: Option<PathBuf>,
    /// When this Mac last finished a capture, so ⌥⇧V prefers it over older inbox items.
    last_local_capture_ms: u64,
    inbox: crate::inbox::Inbox,
    /// The mode the last recording used; the picker opens on it.
    last_mode: SelectionMode,
    /// The picker opened on Area because the last recording was the iPhone and no phone had been
    /// seen yet: switch to the phone when discovery finds one, unless Kit picked a mode himself.
    open_iphone_when_found: bool,
    /// The welcome window, while it's open (`crate::onboarding`).
    #[cfg(target_os = "macos")]
    onboarding: Option<crate::onboarding::Session>,
    /// The Settings window, while it's open (`crate::settings`).
    #[cfg(target_os = "macos")]
    settings: Option<crate::settings::Session>,
    /// The last fresh read of what macOS has granted (at launch, then while onboarding is open).
    #[cfg(target_os = "macos")]
    grants: Option<crate::onboarding::Grants>,
    /// Holds the permissions window until the shortcut's modifiers are up.
    #[cfg(target_os = "macos")]
    gate: crate::onboarding::Gate,
    /// ⌥⇧S held as a ⌘Tab-style switcher (`crate::hold`).
    switcher: crate::hold::Switcher,
    /// The modifiers held when the shortcut went down; releasing any of them commits.
    hold_mods: u64,
    /// The open picker is the held switcher, not the menu bar's picker.
    hold_picker: bool,
    clock: Instant,
    /// `scrawl lab pill`: recordings are pretend (`CaptureSource::Lab`), the picker takes no
    /// input and opens no overlay, and nothing looks for windows or phones.
    lab: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewTab {
    ContactSheet,
    Keyframes,
}

impl ScrawlCoordinator {
    fn new(
        telemetry: Arc<SharedCaptureTelemetry>,
        initial_bundle: Option<(PathBuf, ScrawlBundle)>,
    ) -> Self {
        Self {
            telemetry,
            active_recording: None,
            picker_metal_overlay: None,
            native_pill: None,
            iphone_preview: None,
            island_phase: IslandPhase::Hidden,
            island_prev_phase: IslandPhase::Hidden,
            island_phase_since: motion::now(),
            island_dismissed_from: IslandPhase::Hidden,
            island_width: 205.0,
            island_target_width: 205.0,
            island_velocity: 0.0,
            island_scale: 0.92,
            island_scale_velocity: 0.0,
            island_y: -6.0,
            island_y_velocity: 0.0,
            island_opacity: 0.0,
            island_morph: Morph::Phase,
            reveal: crate::hold::Reveal::default(),
            reveal_since: motion::now(),
            reveal_outgoing: None,
            reveal_prev_chips: false,
            reduced_motion: false,
            switch: None,
            island_stepped: Instant::now(),
            island_await_frame: false,
            last_elapsed_ms: 0,
            highlight_prev: None,
            highlight_since: motion::now(),
            selectable_windows: Vec::new(),
            selection_generation: 0,
            windows_loading: false,
            selection_pointer: None,
            hovered_window: None,
            animated_window_rect: None,
            selection_drag: None,
            display_rect: [0.0, 0.0, 1440.0, 900.0],
            paste_drag_start: None,
            recording_shortcut_key: false,
            review_bundle: initial_bundle,
            review_status: None,
            selected_tab: ReviewTab::ContactSheet,
            selected_keyframe: 0,
            iphone: None,
            completed_dir: None,
            completed_note: None,
            pending_send: None,
            last_local_capture_ms: 0,
            inbox: crate::inbox::Inbox::load_default(),
            last_mode: SelectionMode::load_last_used(),
            open_iphone_when_found: false,
            #[cfg(target_os = "macos")]
            onboarding: None,
            #[cfg(target_os = "macos")]
            settings: None,
            #[cfg(target_os = "macos")]
            grants: None,
            #[cfg(target_os = "macos")]
            gate: crate::onboarding::Gate::default(),
            switcher: crate::hold::Switcher::default(),
            hold_mods: 0,
            hold_picker: false,
            clock: Instant::now(),
            lab: false,
        }
    }

    /// Remembers the mode a recording is starting in, so the next picker opens on it.
    fn remember_mode(&mut self, mode: SelectionMode) {
        if self.last_mode != mode {
            self.last_mode = mode;
            mode.save_last_used();
        }
    }

    /// Records a capture received from elsewhere; returns the unhandled count.
    #[cfg(target_os = "macos")]
    fn add_to_inbox(&mut self, dir: &std::path::Path, bundle: &ScrawlBundle) -> usize {
        let source = bundle.source.as_deref().unwrap_or("iphone");
        self.inbox.add(crate::inbox::InboxEntry::new(
            dir,
            bundle,
            source,
            crate::now_epoch_ms(),
        ));
        set_status_inbox_count(self.inbox.len());
        self.inbox.len()
    }

    /// Kit opened, copied, pasted or dismissed `dir`, so it leaves the inbox.
    fn mark_handled(&mut self, dir: &std::path::Path) {
        if self.inbox.remove(dir) {
            set_status_inbox_count(self.inbox.len());
        }
    }

    /// What ⌥⇧V pastes: the bundle on the Completed island, else the newest unhandled iPhone
    /// capture unless this Mac captured something since, else the latest bundle.
    #[cfg(target_os = "macos")]
    fn take_paste_target(&mut self) -> PathBuf {
        let showing = matches!(self.island_phase, IslandPhase::Completed { .. })
            .then(|| self.completed_dir.clone())
            .flatten();
        let dir = showing
            .or_else(|| {
                self.inbox
                    .newest()
                    .filter(|entry| entry.received_at_ms > self.last_local_capture_ms)
                    .map(|entry| entry.dir.clone())
            })
            .unwrap_or_else(|| PathBuf::from("latest"));
        self.mark_handled(&dir);
        dir
    }

    #[cfg(target_os = "macos")]
    pub fn paste_latest(&mut self, cx: &mut Context<Self>) {
        // Without Accessibility macOS drops the posted ⌘V (onboarding asks for it, but it can be
        // turned off later); bring the permissions window instead.
        if !crate::platform::accessibility_granted() {
            scrawl_log("⌥⇧V without Accessibility; asking for it");
            self.gate
                .defer(vec![crate::onboarding::Permission::Accessibility]);
            return;
        }
        let dir = self.take_paste_target();
        if matches!(self.island_phase, IslandPhase::Completed { .. }) {
            self.dismiss_island(cx);
        }
        std::thread::spawn(move || {
            if let Err(err) = crate::handoff::paste_bundle_image_and_text(&dir) {
                scrawl_log(format!("paste_bundle_image_and_text error: {err}"));
            }
        });
    }

    /// Looks for a cabled iPhone in the background; the picker shows "iPhone" when one is there.
    fn refresh_iphone(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |entity, cx| {
            let found = cx
                .background_executor()
                .spawn(async {
                    crate::platform::phone_devices()
                        .inspect_err(|error| scrawl_log(format!("iPhone discovery: {error:#}")))
                        .ok()
                        .and_then(|devices| devices.into_iter().next())
                })
                .await;
            let _ = entity.update(cx, |coord, cx| {
                if coord.iphone != found {
                    coord.iphone = found;
                    cx.notify();
                }
                if std::mem::take(&mut coord.open_iphone_when_found)
                    && coord.iphone.is_some()
                    && matches!(
                        coord.island_phase,
                        IslandPhase::Selecting {
                            mode: SelectionMode::Area | SelectionMode::Iphone
                        }
                    )
                {
                    coord.preview_iphone(cx);
                }
            });
        })
        .detach();
    }

    pub fn is_recording(&self) -> bool {
        self.active_recording.is_some()
    }

    pub fn is_processing(&self) -> bool {
        matches!(self.island_phase, IslandPhase::Processing)
    }

    pub fn is_selecting(&self) -> bool {
        matches!(self.island_phase, IslandPhase::Selecting { .. })
    }

    #[cfg(target_os = "macos")]
    pub fn open_onboarding(&mut self, flow: crate::onboarding::Flow) {
        // Drop the old window before the new one opens.
        self.onboarding = None;
        let sender = self.telemetry.ui_sender.clone();
        let events = move |event| {
            let _ = sender.send(LiveOverlayEvent::Onboarding(event));
        };
        match crate::onboarding::Session::open(flow, &self.shortcut_label(), events) {
            Ok(session) => self.onboarding = Some(session),
            Err(error) => scrawl_log(format!("onboarding could not open: {error:#}")),
        }
    }

    /// Opens Settings, or brings it forward on `pane`.
    #[cfg(target_os = "macos")]
    pub fn open_settings(&mut self, pane: Option<crate::settings::Pane>) {
        if let Some(session) = self.settings.as_mut() {
            session.show(pane);
            return;
        }
        let sender = self.telemetry.ui_sender.clone();
        let events = move |event| {
            let _ = sender.send(LiveOverlayEvent::Settings(event));
        };
        match crate::settings::Session::open(pane, self.grants.clone(), events) {
            Ok(mut session) => {
                session.sync(&crate::settings::AppHost {
                    telemetry: &self.telemetry,
                });
                self.settings = Some(session);
            }
            Err(error) => scrawl_log(format!("Settings could not open: {error:#}")),
        }
    }

    #[cfg(target_os = "linux")]
    /// Settings come to Linux later (phase 4 part 2).
    pub fn open_settings(&mut self) {
        scrawl_log("Settings aren't available on Linux yet");
    }

    #[cfg(target_os = "macos")]
    fn settings_event(&mut self, event: crate::settings::Event, cx: &mut Context<Self>) {
        use crate::settings::{Effect, Event};
        if let Event::Grants(grants) = &event {
            self.note_grants(grants);
        }
        let Some(session) = self.settings.as_mut() else {
            return;
        };
        let mut host = crate::settings::AppHost {
            telemetry: &self.telemetry,
        };
        match session.event(event, &mut host) {
            Effect::Close => self.settings = None,
            Effect::Tour => self.open_onboarding(crate::onboarding::Flow::tour(
                crate::onboarding::Step::Welcome,
                self.grants.clone(),
            )),
            Effect::Relaunch => {
                self.settings = None;
                crate::onboarding::relaunch();
                cx.quit();
            }
            _ => {}
        }
    }

    /// Keeps a fresh read of the grants and remembers what was granted. (The key tap follows
    /// Accessibility by itself, in the HID poller.)
    #[cfg(target_os = "macos")]
    fn note_grants(&mut self, grants: &crate::onboarding::Grants) {
        use crate::onboarding::Progress;
        let mut progress = Progress::load();
        if progress.note(grants) {
            progress.save();
        }
        self.grants = Some(grants.clone());
    }

    /// Required permissions that keep a capture from starting now.
    #[cfg(target_os = "macos")]
    fn missing_to_record(&mut self) -> Vec<crate::onboarding::Permission> {
        use crate::onboarding::{Permission, missing_to_record};
        let screen = crate::platform::screen_capture_usable(false);
        let accessibility = crate::platform::accessibility_granted();
        let missing = missing_to_record(self.grants.as_ref(), screen, accessibility);
        // The last read may be old; read again before standing in the way.
        if missing.contains(&Permission::Microphone)
            && let Some(fresh) = crate::onboarding::read_grants()
        {
            self.note_grants(&fresh);
            return missing_to_record(self.grants.as_ref(), screen, accessibility);
        }
        missing
    }

    /// Whether this may go on: with a required permission off it starts nothing, and the
    /// permissions window comes forward once the modifiers are up.
    #[cfg(target_os = "macos")]
    fn ready_to_record(&mut self) -> bool {
        if self.lab {
            return true;
        }
        let missing = self.missing_to_record();
        if missing.is_empty() {
            return true;
        }
        scrawl_log(format!(
            "Not recording: {missing:?} off; opening onboarding"
        ));
        self.gate.defer(missing);
        false
    }

    #[cfg(target_os = "linux")]
    /// Linux asks no permission for a capture.
    fn ready_to_record(&mut self) -> bool {
        true
    }

    /// Whether `permission`, asked for on first use (Camera, Accessibility), is on. If not, its
    /// window comes forward once the modifiers are up.
    #[cfg(target_os = "macos")]
    fn ready_for(&mut self, permission: crate::onboarding::Permission) -> bool {
        let granted = |g: &crate::onboarding::Grants| g.granted(permission);
        if self.lab || self.grants.as_ref().is_none_or(granted) {
            return true;
        }
        if let Some(fresh) = crate::onboarding::read_grants() {
            self.note_grants(&fresh);
            if granted(&fresh) {
                return true;
            }
        }
        scrawl_log(format!("{permission:?} is off; asking for it"));
        self.gate.defer(vec![permission]);
        false
    }

    /// Polled while the gate holds a request: shows the permissions window once ⌥⇧ is up.
    #[cfg(target_os = "macos")]
    pub fn tick_gate(&mut self) {
        use crate::onboarding::{Flow, Input, Mode, Permission};
        let Some(asks) = self.gate.tick(crate::platform::modifier_flags() != 0) else {
            return;
        };
        let required = asks.iter().any(|p| p.required());
        match self.onboarding.as_mut() {
            Some(session) if session.mode() == Mode::Tour && required => {
                session.input(Input::Blocked);
            }
            _ => {
                let asks = if required {
                    Permission::REQUIRED.to_vec()
                } else {
                    asks
                };
                self.open_onboarding(Flow::fix(asks, self.grants.clone()));
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn onboarding_event(&mut self, event: crate::onboarding::Event, cx: &mut Context<Self>) {
        use crate::onboarding::{Effect, Event, Progress};
        let effect = match event {
            Event::Launch(grants) => {
                self.note_grants(&grants);
                let mut progress = Progress::load();
                let plan = crate::onboarding::launch_plan(&progress, &grants);
                if std::mem::take(&mut progress.reopen_fix) {
                    progress.save();
                }
                if self.onboarding.is_none()
                    && let Some(flow) = plan
                {
                    self.open_onboarding(flow);
                }
                Effect::None
            }
            Event::Grants(grants) => {
                self.note_grants(&grants);
                self.onboarding
                    .as_mut()
                    .map_or(Effect::None, |session| session.grants(grants))
            }
            Event::Action(action) => self
                .onboarding
                .as_mut()
                .map_or(Effect::None, |session| session.action(&action)),
            Event::Exited => {
                self.onboarding = None;
                Effect::None
            }
        };
        match effect {
            Effect::Close | Effect::Settings => {
                if effect == Effect::Settings {
                    self.open_settings(None);
                }
                let mut progress = Progress::load();
                progress.seen = true;
                progress.resume = None;
                progress.save();
                self.onboarding = None;
            }
            Effect::Relaunch { resume } => {
                scrawl_log(format!(
                    "Screen Recording granted but not usable here; relaunching (resume {resume:?})"
                ));
                let mut progress = Progress::load();
                progress.relaunching(resume, crate::onboarding::unix_ms());
                progress.save();
                self.onboarding = None;
                crate::onboarding::relaunch();
                cx.quit();
            }
            _ => {}
        }
    }

    fn activity(&self) -> crate::hold::Activity {
        use crate::hold::Activity;
        if let Some(rec) = &self.active_recording {
            Activity::Recording {
                elapsed_ms: rec.started.elapsed().as_millis() as u64,
            }
        } else if self.is_processing() {
            Activity::Processing
        } else if self.is_selecting() && !self.switcher.is_switching() {
            Activity::Picker
        } else {
            Activity::Free
        }
    }

    fn clock_ms(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64
    }

    /// The capture shortcut went down (from Carbon, the event tap or the HID poller).
    pub fn press_shortcut(&mut self, cx: &mut Context<Self>) {
        let activity = self.activity();
        #[cfg(target_os = "macos")]
        if self.gate.is_pending() {
            return;
        }
        if !self.switcher.is_held() && activity == crate::hold::Activity::Free {
            // No switcher, picker or overlay before the permissions are on: they sit above
            // macOS's permission prompts and take the clicks meant for them.
            #[cfg(target_os = "macos")]
            let missing = self.missing_to_record();
            #[cfg(target_os = "macos")]
            if !self.gate.press(missing) {
                scrawl_log("Shortcut held back: a required permission is off");
                return;
            }
            self.hold_mods = crate::platform::modifier_flags();
            // An Esc from before this hold doesn't cancel it.
            self.telemetry.take_escape();
        }
        if let Some(mode) = self.selection_mode() {
            self.switcher.follow(mode);
        }
        let action = self.switcher.press(self.clock_ms(), activity);
        self.apply_switcher(action, cx);
    }

    /// Polled from the UI loop while the shortcut is held: watches for ⌥⇧ coming up and Esc,
    /// whichever app or window has focus.
    pub fn tick_switcher(&mut self, cx: &mut Context<Self>) {
        if !self.switcher.is_held() {
            return;
        }
        if self.switcher.is_switching() && !self.is_selecting() {
            // Closed some other way (✕, a click on a chip).
            self.switcher.reset();
            return;
        }
        let action = if self.telemetry.take_escape() {
            self.switcher.escape()
        } else {
            let mods = crate::platform::modifier_flags();
            let held = self.hold_mods != 0 && mods & self.hold_mods == self.hold_mods;
            self.switcher.tick(self.clock_ms(), held)
        };
        self.apply_switcher(action, cx);
    }

    fn apply_switcher(&mut self, action: crate::hold::Action, cx: &mut Context<Self>) {
        use crate::hold::Action;
        if action != Action::None {
            scrawl_log(format!("switcher -> {action:?}"));
        }
        match action {
            Action::None => {}
            Action::RecordScreen => self.record_screen_now(cx),
            Action::Open(mode) => {
                self.open_picker(Some(SelectionMode::Screen), true, cx);
                if mode != SelectionMode::Screen {
                    self.show_switcher_mode(mode, cx);
                }
            }
            Action::Show(mode) => self.show_switcher_mode(mode, cx),
            Action::Commit(mode) => self.commit_switcher(mode, cx),
            Action::Cancel => {
                if self.is_selecting() {
                    self.cancel_recording(cx);
                }
            }
            Action::RecordSelected => self.record_selected_mode(cx),
            Action::Finish => self.finish_recording(cx),
        }
    }

    /// The menu's Record Screen item, the review window's Record button and `app --record`.
    pub fn record_or_finish(&mut self, cx: &mut Context<Self>) {
        if self.is_recording() {
            self.finish_recording(cx);
        } else if self.is_selecting() {
            self.record_selected_mode(cx);
        } else if !self.is_processing() {
            self.record_screen_now(cx);
        }
    }

    fn show_switcher_mode(&mut self, mode: SelectionMode, cx: &mut Context<Self>) {
        if mode == SelectionMode::Iphone && self.iphone.is_some() {
            self.preview_iphone(cx);
        } else {
            self.set_selection_mode(mode, cx);
            // No phone yet: the badge says so, and the mirror opens if discovery finds one.
            self.open_iphone_when_found = mode == SelectionMode::Iphone;
        }
    }

    /// ⌥⇧ released: record what the switcher shows. An iPhone that isn't there or a drag too
    /// small for an area records nothing.
    fn commit_switcher(&mut self, mode: SelectionMode, cx: &mut Context<Self>) {
        match self.selection_mode().unwrap_or(mode) {
            SelectionMode::Screen => self.start_recording_full_screen(cx),
            SelectionMode::Window => {
                if self.hovered_window.is_none() {
                    if self.windows_loading {
                        self.selectable_windows = list_selectable_windows();
                        self.windows_loading = false;
                    }
                    self.hovered_window = current_mouse_screen()
                        .and_then(|[x, y]| {
                            selectable_window_at(
                                &self.selectable_windows,
                                f64::from(x),
                                f64::from(y),
                            )
                        })
                        .cloned();
                }
                match self.hovered_window.clone() {
                    Some(target) => self.start_recording_with_target(target, cx),
                    None => self.start_recording_full_screen(cx),
                }
            }
            SelectionMode::Iphone => {
                if self.iphone.is_some() || self.iphone_preview.is_some() {
                    self.start_recording_iphone(cx);
                } else {
                    self.cancel_recording(cx);
                }
            }
            SelectionMode::Area => match self
                .selection_drag
                .and_then(|(a, b)| area_rect(a, b, self.display_rect))
            {
                Some([x, y, w, h]) => {
                    let target = region_target(x, y, w, h, &self.selectable_windows);
                    self.start_recording_with_target(target, cx);
                }
                None => self.cancel_recording(cx),
            },
        }
    }

    /// Records the display under the cursor straight away (no picker), auto-cropped.
    pub fn record_screen_now(&mut self, cx: &mut Context<Self>) {
        let target = if self.lab {
            lab::target()
        } else {
            discover_target(None)
        };
        self.start_recording_with_target(target, cx);
        if self.is_recording() {
            self.show_reveal(cx);
        }
    }

    /// Activity that means the user got to work: clicks, drags, scrolls and strokes. Clicks on
    /// Scrawl's own windows (the capsule and its chips) are never recorded, so they don't count.
    fn reveal_activity(&self) -> usize {
        use std::sync::PoisonError;
        let t = &self.telemetry;
        t.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
            + t.strokes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len()
            + usize::from(
                t.live_ink
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_some(),
            )
    }

    /// The recording just started from a tap: the capsule opens onto the switch chips.
    fn show_reveal(&mut self, cx: &mut Context<Self>) {
        // The iPhone chip shows only while one is cabled (a USB registry read, no prompt).
        if !self.lab {
            self.refresh_iphone(cx);
        }
        self.reduced_motion = reduce_motion();
        let activity = self.reveal_activity();
        self.reveal.show(reveal_clock_ms(), activity);
        self.reveal_since = motion::now();
        self.reveal_outgoing = None;
        self.island_morph = Morph::RevealOpen;
        if self.reduced_motion {
            self.island_scale = 1.0;
            self.island_scale_velocity = 0.0;
            self.island_y = 0.0;
            self.island_y_velocity = 0.0;
        }
        self.sync_native_pill();
        cx.notify();
    }

    /// The chips fold back into the recording pill (timeout, work started, or a chip chosen).
    fn fold_reveal(&mut self) {
        if self.reveal.is_open() {
            self.reveal.close();
            self.play_fold();
        }
    }

    fn play_fold(&mut self) {
        self.reveal_since = motion::now();
        self.reveal_outgoing = Some(true);
        self.island_morph = Morph::RevealClose;
        self.sync_native_pill();
    }

    /// A switch chip on a tap-started recording. Screen keeps it. Anything else drops the few
    /// seconds recorded so far and starts over on the new target, exactly as if it had been
    /// chosen first: Window and Area morph the capsule into the picker (hover and click a window,
    /// or drag an area), iPhone goes straight into the phone's mirror.
    pub fn switch_recording(&mut self, mode: SelectionMode, cx: &mut Context<Self>) {
        if !self.is_recording() {
            return;
        }
        if mode == SelectionMode::Screen {
            self.fold_reveal();
            cx.notify();
            return;
        }
        scrawl_log(format!("switch_recording -> {mode:?}"));
        // The morph continues the chips, so it needs them on screen.
        let chips_shown = self.reveal.is_open();
        let Some((_, overlay)) = self.discard_recording() else {
            return;
        };
        if mode == SelectionMode::Iphone {
            // Recording → Recording: the chips fold into the pill as the phone starts.
            self.fold_reveal();
            drop(overlay);
            self.start_recording_iphone(cx);
            return;
        }
        // Window and Area: the chips stay up while the capsule morphs into the picker.
        // The recording's overlay becomes the picker's (moved if the picker opens on another
        // display), so the border never swaps windows (as the picker → recording path does).
        self.picker_metal_overlay = overlay;
        self.open_picker(Some(mode), false, cx);
        if self.is_selecting() && chips_shown && !self.reduced_motion {
            // The chips showed Screen; the picker shows `mode`.
            self.highlight_prev = Some(SelectionMode::Screen);
            self.highlight_since = motion::now();
            self.island_morph = Morph::Switch;
            self.switch = Some(SwitchMorph { slide: None });
        }
    }

    /// Stops the active recording and deletes its bundle, leaving the island as it is. Returns
    /// its target and overlay.
    fn discard_recording(&mut self) -> Option<(TargetInfo, Option<MetalLaserOverlay>)> {
        set_escape_hotkey_enabled(false);
        let rec = self.active_recording.take()?;
        let _ = self.telemetry.finish_session();
        match rec.source {
            CaptureSource::Screen { session, audio, .. } => {
                crate::platform::discard_screen_session(session);
                if let Some(audio) = audio {
                    let _ = audio.stop();
                }
            }
            CaptureSource::Phone(session) => session.cancel(),
            CaptureSource::Lab => return Some((rec.target, rec._metal_overlay)),
        }
        let _ = std::fs::remove_dir_all(rec.bundle_dir);
        Some((rec.target, rec._metal_overlay))
    }

    pub fn is_island_visible(&self) -> bool {
        !matches!(self.island_phase, IslandPhase::Hidden)
    }

    pub fn shortcut_label(&self) -> String {
        self.telemetry
            .shortcut
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .label
            .clone()
    }

    pub fn set_review_bundle(&mut self, bundle: (PathBuf, ScrawlBundle), status: Option<String>) {
        self.review_bundle = Some(bundle);
        self.review_status = status;
        self.selected_tab = ReviewTab::ContactSheet;
        self.selected_keyframe = 0;
    }

    fn sync_picker_overlay(&self) {
        let rect = match self.island_phase {
            IslandPhase::Selecting {
                mode: SelectionMode::Area,
            } => self.selection_drag.and_then(|(a, b)| {
                let [a, b] = [a, b].map(|p| clamp_point(p, self.display_rect));
                let w = (a[0] - b[0]).abs();
                let h = (a[1] - b[1]).abs();
                (w >= 1.0 && h >= 1.0).then_some(([a[0].min(b[0]), a[1].min(b[1]), w, h], 1))
            }),
            IslandPhase::Selecting {
                mode: SelectionMode::Window,
            } => self.hovered_window.as_ref().map(|t| {
                (
                    [
                        t.frame.x as f32,
                        t.frame.y as f32,
                        t.frame.width as f32,
                        t.frame.height as f32,
                    ],
                    2,
                )
            }),
            IslandPhase::Selecting {
                mode: SelectionMode::Screen,
            } => Some((self.display_rect, 2)),
            IslandPhase::Selecting {
                mode: SelectionMode::Iphone,
            } => self
                .iphone_preview
                .as_ref()
                .and_then(PhoneSession::rect)
                .map(|r| (r, 3)),
            _ => None,
        };
        self.telemetry.set_picker_rect(rect);
    }

    pub fn set_selection_mode(&mut self, mode: SelectionMode, cx: &mut Context<Self>) {
        self.open_iphone_when_found = false;
        if let IslandPhase::Selecting { mode: prev_mode } = self.island_phase {
            if prev_mode != mode {
                // Crisp tactile Dynamic Island scale micro-pulse when Space toggles camera/crosshair mode.
                self.island_scale_velocity += 1.25;
            }
            if mode != SelectionMode::Iphone
                && let Some(ref mut preview) = self.iphone_preview
            {
                preview.hide_preview();
            }
            self.island_phase = IslandPhase::Selecting { mode };
            self.selection_drag = None;
            if let Some(pos) = current_mouse_screen().or(self.selection_pointer) {
                self.selection_pointer = Some(pos);
                if mode == SelectionMode::Window {
                    self.hovered_window = selectable_window_at(
                        &self.selectable_windows,
                        f64::from(pos[0]),
                        f64::from(pos[1]),
                    )
                    .cloned();
                    self.animated_window_rect = self.hovered_window.as_ref().map(|w| {
                        [
                            w.frame.x as f32,
                            w.frame.y as f32,
                            w.frame.width as f32,
                            w.frame.height as f32,
                        ]
                    });
                } else {
                    self.animated_window_rect = None;
                }
            }
            self.sync_picker_overlay();
            cx.notify();
        }
    }

    /// Opens the live iPhone mirror window in preview mode so the user can see their phone before
    /// starting a recording. Clicking `iPhone` a second time or clicking `Record` starts recording.
    pub fn preview_iphone(&mut self, cx: &mut Context<Self>) {
        if self.is_recording() || self.is_processing() {
            return;
        }
        scrawl_log("preview_iphone invoked");
        // The mirror would raise the camera prompt under the picker, which takes its clicks.
        #[cfg(target_os = "macos")]
        if !self.ready_for(crate::onboarding::Permission::Camera) {
            self.cancel_recording(cx);
            return;
        }
        self.set_selection_mode(SelectionMode::Iphone, cx);
        if let Some(ref mut preview) = self.iphone_preview {
            preview.show_preview();
        } else {
            let dev_id = self.iphone.as_ref().map(|d| d.id.as_str());
            match PhoneSession::start_preview(dev_id, false, Some(Arc::clone(&self.telemetry))) {
                Ok(session) => {
                    self.iphone_preview = Some(session);
                }
                Err(error) => {
                    scrawl_log(format!("iPhone preview could not start: {error:#}"));
                    self.set_selection_mode(SelectionMode::Area, cx);
                    return;
                }
            }
        }
        self.sync_picker_overlay();
        cx.notify();
    }

    pub fn toggle_selection_mode(&mut self, cx: &mut Context<Self>) {
        if let IslandPhase::Selecting { mode } = self.island_phase {
            let next = match mode {
                SelectionMode::Area => SelectionMode::Window,
                SelectionMode::Window | SelectionMode::Screen | SelectionMode::Iphone => {
                    SelectionMode::Area
                }
            };
            self.set_selection_mode(next, cx);
        }
    }

    fn selection_mode(&self) -> Option<SelectionMode> {
        match self.island_phase {
            IslandPhase::Selecting { mode } => Some(mode),
            _ => None,
        }
    }

    /// Switches the picker to `mode`; choosing iPhone opens its live preview.
    pub fn choose_selection_mode(&mut self, mode: SelectionMode, cx: &mut Context<Self>) {
        if self.selection_mode().is_none() {
            return;
        }
        if mode == SelectionMode::Iphone {
            self.preview_iphone(cx);
        } else {
            self.set_selection_mode(mode, cx);
        }
    }

    /// A HUD segment click: picks the mode, or records when Screen/iPhone is already selected.
    pub fn click_selection_mode(&mut self, mode: SelectionMode, cx: &mut Context<Self>) {
        if self.selection_mode() == Some(mode) && mode.shows_record() {
            self.record_selected_mode(cx);
        } else {
            self.choose_selection_mode(mode, cx);
        }
    }

    /// Return, `s`, the capture shortcut, or Record: iPhone records the phone, anything else
    /// records the full screen.
    pub fn record_selected_mode(&mut self, cx: &mut Context<Self>) {
        match self.selection_mode() {
            Some(SelectionMode::Iphone) => self.start_recording_iphone(cx),
            Some(_) => self.start_recording_full_screen(cx),
            None => {}
        }
    }

    /// Picker keyboard map, shared by the picker window and the HUD pill. Returns whether the
    /// key was handled.
    /// ⌘Q and ⌘, in Scrawl's own windows: a menu bar (accessory) app has no main menu to
    /// carry them.
    pub fn handle_app_key(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let mods = keystroke.modifiers;
        if !mods.platform || mods.control || mods.alt || mods.shift {
            return false;
        }
        match keystroke.key.as_str() {
            "q" => cx.quit(),
            "," => {
                if self.is_selecting() {
                    self.switcher.reset();
                    self.cancel_recording(cx);
                }
                #[cfg(target_os = "macos")]
                self.open_settings(None);
                #[cfg(target_os = "linux")]
                self.open_settings();
            }
            _ => return false,
        }
        true
    }

    pub fn handle_selection_key(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let Some(mode) = self.selection_mode() else {
            return false;
        };
        let mods = keystroke.modifiers;
        // Esc cancels even with ⌥⇧ still held from the switcher.
        if keystroke.key == "escape" {
            self.switcher.reset();
            self.cancel_recording(cx);
            return true;
        }
        if self.handle_app_key(keystroke, cx) {
            return true;
        }
        // The shortcut itself, if it reaches a picker window instead of Carbon.
        if let Some(cfg) = shortcut_from_key_and_mods(
            &keystroke.key,
            mods.platform,
            mods.control,
            mods.alt,
            mods.shift,
        ) && self.telemetry.matches_shortcut(
            cfg.key_code as u16,
            mods.platform,
            mods.control,
            mods.alt,
            mods.shift,
        ) {
            self.telemetry.emit_capture_shortcut();
            return true;
        }
        if mods.platform || mods.control || mods.alt {
            return false;
        }
        let has_iphone = self.iphone.is_some();
        // The record shortcut's key ("S") also records on its own while the picker is up.
        let label = self.shortcut_label();
        let record_key = crate::platform::shortcut_parts(&label).1;
        match keystroke.key.as_str() {
            "space" | " " => self.toggle_selection_mode(cx),
            "escape" => self.cancel_recording(cx),
            "enter" => self.record_selected_mode(cx),
            key if key.eq_ignore_ascii_case(record_key) => self.record_selected_mode(cx),
            "tab" => self.choose_selection_mode(mode.cycled(!mods.shift, has_iphone), cx),
            "right" => self.choose_selection_mode(mode.cycled(true, has_iphone), cx),
            "left" => self.choose_selection_mode(mode.cycled(false, has_iphone), cx),
            key => match SelectionMode::from_digit(key, has_iphone) {
                Some(next) => self.choose_selection_mode(next, cx),
                None => return false,
            },
        }
        true
    }

    pub fn undo_stroke(&mut self, cx: &mut Context<Self>) {
        let mut strokes = self
            .telemetry
            .strokes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(idx) = strokes.iter().rposition(|s| !s.laser) {
            strokes.remove(idx);
        }
        drop(strokes);
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    pub fn request_immediate_frame(&self) {
        if let Some(ActiveRecording {
            source: CaptureSource::Screen { session, .. },
            ..
        }) = &self.active_recording
        {
            session.request_immediate_frame();
        }
    }

    /// Moves the island to `phase`, triggering a `.blurReplace` cross-transition on the inner content
    /// while the outer shell springs its width smoothly (`stiffness: 400, damping: 30` equivalent).
    fn set_island_phase(&mut self, phase: IslandPhase) {
        #[cfg(target_os = "macos")]
        use crate::onboarding::Input;
        #[cfg(target_os = "macos")]
        let capture = match (self.island_phase, phase) {
            (IslandPhase::Recording, IslandPhase::Recording) => None,
            (_, IslandPhase::Recording) => Some(Input::CaptureStarted),
            (IslandPhase::Recording | IslandPhase::Processing, IslandPhase::Completed { .. }) => {
                Some(Input::CaptureEnded { completed: true })
            }
            (
                IslandPhase::Recording | IslandPhase::Processing,
                IslandPhase::Hidden | IslandPhase::Dismissing,
            ) => Some(Input::CaptureEnded { completed: false }),
            _ => None,
        };
        #[cfg(target_os = "macos")]
        if let Some(input) = capture
            && let Some(session) = self.onboarding.as_mut()
        {
            session.input(input);
        }
        let changed = std::mem::discriminant(&self.island_phase) != std::mem::discriminant(&phase);
        if changed {
            // A new phase replaces the whole content; the chips go with it, and the outgoing
            // Recording content keeps showing them while it blurs away.
            self.reveal_prev_chips = self.reveal.is_open();
            self.reveal.close();
            self.reveal_outgoing = None;
            self.island_morph = Morph::Phase;
            self.switch = None;
            self.highlight_prev = None;
            if matches!(self.island_phase, IslandPhase::Hidden) {
                self.island_stepped = Instant::now();
            }
            self.island_prev_phase = self.island_phase;
            if matches!(phase, IslandPhase::Dismissing) {
                self.island_dismissed_from = self.island_phase;
            } else if matches!(self.island_phase, IslandPhase::Hidden) {
                self.island_scale = 0.92;
                self.island_scale_velocity = 0.0;
                self.island_y = -6.0;
                self.island_y_velocity = 0.0;
            }
            self.island_phase_since = motion::now();
            motion::trace_event(format!("phase:{}", phase.trace_name()));
        }
        let appearing = matches!(
            self.island_phase,
            IslandPhase::Hidden | IslandPhase::Dismissing
        );
        self.island_phase = phase;
        if crate::platform::native_pill() {
            if !matches!(phase, IslandPhase::Hidden) {
                self.ensure_native_pill();
            }
            if changed
                && (appearing
                    || matches!(
                        phase,
                        IslandPhase::Selecting { .. } | IslandPhase::Recording
                    ))
            {
                self.place_capsule();
            }
            self.sync_native_pill();
        }
    }

    /// Puts the capsule on the display in use: the one being recorded, the one the picker is on,
    /// else the one under the pointer.
    fn place_capsule(&self) {
        let Some(ref pill) = self.native_pill else {
            return;
        };
        let centre = |[x, y, w, h]: [f64; 4]| [x + w / 2.0, y + h / 2.0];
        let anchor = match self.island_phase {
            IslandPhase::Recording => self
                .active_recording
                .as_ref()
                .filter(|rec| rec.target.mode != CaptureMode::Device)
                .map(|rec| {
                    let f = rec.target.frame;
                    centre([f.x, f.y, f.width, f.height])
                }),
            IslandPhase::Selecting { .. } => Some(centre(self.display_rect.map(f64::from))),
            _ => None,
        };
        if let Some(point) = anchor.or_else(crate::screens::pointer) {
            pill.place_on(point);
        }
    }

    /// The iPhone's mirror window can be dragged to another display mid-recording: the ink
    /// overlay and the capsule go with it.
    fn follow_phone_mirror(&mut self) {
        let Some(ActiveRecording {
            source: CaptureSource::Phone(_),
            _metal_overlay: Some(overlay),
            ..
        }) = self.active_recording.as_mut()
        else {
            return;
        };
        let f = *self
            .telemetry
            .target_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if f.x <= -10_000.0 {
            return;
        }
        let centre = [f.x + f.width / 2.0, f.y + f.height / 2.0];
        if crate::screens::contains(overlay.screen(), centre)
            || !overlay.move_to([f.x, f.y, f.width, f.height])
        {
            return;
        }
        self.display_rect = overlay.screen().map(|v| v as f32);
        if let Some(ref pill) = self.native_pill {
            pill.place_on(centre);
        }
    }

    /// While picking, the picker (its window, overlay and the capsule) follows the pointer to
    /// whichever display it's on, unless an area is being dragged (an area stays on one display).
    fn follow_pointer_display(&mut self, pointer: [f32; 2], cx: &mut Context<Self>) {
        // The iPhone's mirror window stays where it is, and so does the highlight around it.
        if self.selection_drag.is_some()
            || self.lab
            || self.selection_mode() == Some(SelectionMode::Iphone)
        {
            return;
        }
        let p = pointer.map(f64::from);
        let current = self.display_rect.map(f64::from);
        if crate::screens::contains(current, p) {
            return;
        }
        let screens = crate::screens::current();
        let Some(i) = crate::screens::index_at(&screens, p) else {
            return;
        };
        let frame = screens[i].frame;
        if (0..4).all(|k| (frame[k] - current[k]).abs() < 1.0) {
            return;
        }
        scrawl_log(format!("picker follows the pointer to display {frame:?}"));
        self.display_rect = frame.map(|v| v as f32);
        if let Some(ref mut overlay) = self.picker_metal_overlay {
            overlay.move_to(frame);
        }
        if let Some(ns_win) = find_picker_ns_window() {
            move_window_to_screen(&ns_win, frame);
        }
        self.place_capsule();
        self.sync_picker_overlay();
        // The picker draws relative to `display_rect`; with the native capsule nothing else
        // redraws it, and a same-size display doesn't resize its window.
        cx.notify();
    }

    /// The picker's mouse-up, inside its window or not: ends an area drag.
    fn picker_mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        let [sx, sy] = current_mouse_screen()
            .unwrap_or([f32::from(event.position.x), f32::from(event.position.y)]);
        // The held switcher keeps the area until ⌥⇧ is released.
        if self.hold_picker {
            if self.selection_mode() == Some(SelectionMode::Area)
                && let Some((start, _)) = self.selection_drag
            {
                self.selection_drag = Some((start, [sx, sy]));
            } else {
                self.selection_drag = None;
            }
            self.sync_picker_overlay();
            cx.notify();
            return;
        }
        if let Some((start, _)) = self.selection_drag.take() {
            if let Some([x, y, w, h]) = area_rect(start, [sx, sy], self.display_rect) {
                let target = region_target(x, y, w, h, &self.selectable_windows);
                self.start_recording_with_target(target, cx);
            } else {
                self.sync_picker_overlay();
                cx.notify();
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn ensure_native_pill(&mut self) {
        if !crate::platform::native_pill() || self.native_pill.is_some() {
            return;
        }
        match crate::native::pill::NativePill::open() {
            Ok(pill) => {
                let num = pill.window_number();
                if num > 0 {
                    self.telemetry
                        .excluded_window_ids
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(num);
                }
                self.native_pill = Some(pill);
            }
            Err(err) => {
                scrawl_log(format!("NativePill::open error: {err}"));
            }
        }
    }

    #[cfg(target_os = "linux")]
    /// The native capsule is macOS only.
    pub(crate) fn ensure_native_pill(&mut self) {}

    #[cfg(target_os = "macos")]
    fn pill_snapshot(&self) -> crate::native::pill::PillSnapshot {
        use crate::native::pill::{PillSnapshot, PillStyle, model::Pres};
        let recording_ms = self
            .active_recording
            .as_ref()
            .map(ActiveRecording::elapsed_ms);
        let elapsed_ms = recording_ms.unwrap_or(self.last_elapsed_ms);
        let clock = format_clock(elapsed_ms);
        let has_iphone = self.iphone.is_some();
        let stroke_count = self
            .telemetry
            .strokes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|s| !s.laser)
            .count();

        let pres = match self.island_phase {
            IslandPhase::Hidden | IslandPhase::Dismissing => Pres::Hidden,
            IslandPhase::Selecting { mode } => Pres::Selecting {
                mode,
                hold: self.hold_picker,
                has_iphone,
            },
            IslandPhase::Recording => Pres::Recording {
                chips: self.reveal.is_open(),
                has_iphone,
                stroke_count,
            },
            IslandPhase::Processing => Pres::Processing,
            IslandPhase::Completed { inbox_count, .. } => Pres::Completed {
                label: completed_label(inbox_count, self.completed_note.as_deref()),
            },
        };
        PillSnapshot {
            pres,
            clock,
            reduced_motion: self.reduced_motion,
            style: PillStyle::current(),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn sync_native_pill(&mut self) {
        if self.native_pill.is_some() {
            let snap = self.pill_snapshot();
            if let Some(ref pill) = self.native_pill {
                pill.sync(snap);
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn sync_native_pill(&mut self) {}

    #[cfg(target_os = "macos")]
    fn apply_pill_action(&mut self, action: crate::native::pill::PillAction, cx: &mut Context<Self>) {
        use crate::native::pill::PillAction;
        match action {
            PillAction::SelectMode(mode) => self.click_selection_mode(mode, cx),
            PillAction::SwitchRecording(mode) => self.switch_recording(mode, cx),
            PillAction::RecordSelected => self.record_selected_mode(cx),
            PillAction::Undo => self.undo_stroke(cx),
            PillAction::Cancel => self.cancel_recording(cx),
            PillAction::Finish => self.finish_recording(cx),
            PillAction::Paste(target) => self.paste_completed_bundle(target, cx),
            PillAction::Inspect => self.inspect_completed_bundle(cx),
            PillAction::DismissCompleted => self.dismiss_completed_bundle(cx),
        }
    }

    #[cfg(target_os = "linux")]
    fn apply_pill_action(&mut self, action: crate::platform::PillAction, _cx: &mut Context<Self>) {
        match action {}
    }

    pub fn dismiss_island(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.island_phase,
            IslandPhase::Hidden | IslandPhase::Dismissing
        ) {
            return;
        }
        self.telemetry
            .selecting
            .store(false, std::sync::atomic::Ordering::Release);
        self.telemetry.set_picker_rect(None);
        self.picker_metal_overlay = None;
        self.iphone_preview = None;
        self.selection_drag = None;
        self.hovered_window = None;
        self.animated_window_rect = None;
        self.set_island_phase(IslandPhase::Dismissing);
        cx.defer(close_picker_window);
        cx.notify();
    }

    pub fn dismiss_completed_bundle(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = self.completed_dir.clone() {
            self.keep_pending_send(&dir);
            self.mark_handled(&dir);
        }
        self.dismiss_island(cx);
    }

    pub fn inspect_completed_bundle(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = self.completed_dir.clone() {
            self.mark_handled(&dir);
        }
        self.set_island_phase(IslandPhase::Hidden);
        self.island_opacity = 0.0;
        let entity = cx.entity().clone();
        cx.defer(move |cx| {
            close_capture_windows(cx);
            open_review_window(&entity, cx);
        });
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    pub fn paste_completed_bundle(
        &mut self,
        click_target: Option<[f32; 2]>,
        cx: &mut Context<Self>,
    ) {
        let dir = self
            .completed_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("latest"));
        self.keep_pending_send(&dir);
        self.dismiss_island(cx);
        if !crate::platform::accessibility_granted() {
            scrawl_log("Paste chip without Accessibility; asking for it");
            self.gate
                .defer(vec![crate::onboarding::Permission::Accessibility]);
            return;
        }
        self.mark_handled(&dir);
        std::thread::spawn(move || {
            if let Err(err) = crate::handoff::paste_bundle_at_target(&dir, click_target) {
                scrawl_log(format!("paste_bundle_at_target error: {err}"));
            }
        });
    }

    #[cfg(target_os = "linux")]
    /// The Paste chip on Linux: the capture is already on the clipboard; it leaves the inbox.
    pub fn paste_completed_bundle(
        &mut self,
        _click_target: Option<[f32; 2]>,
        cx: &mut Context<Self>,
    ) {
        let dir = self
            .completed_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("latest"));
        self.keep_pending_send(&dir);
        self.dismiss_island(cx);
        self.mark_handled(&dir);
        std::thread::spawn(move || {
            if let Err(err) = crate::handoff::copy_bundle_to_clipboard(&dir) {
                scrawl_log(format!("copy_bundle_to_clipboard error: {err:#}"));
            }
        });
    }

    /// Acts on smart routing's decision for a finished local capture (the clipboard already has
    /// it): a send waits out the undo window first; an offer is only named in the review window.
    #[cfg(target_os = "macos")]
    fn act_on_route(
        &mut self,
        dir: PathBuf,
        (judgement, url): Judged,
        cx: &mut Context<Self>,
    ) {
        use crate::routing::smart::Decision;
        match &judgement.decision {
            Decision::Send { .. } => {
                if let Some(dest) = crate::routing::smart::destination(&judgement, url.as_deref()) {
                    self.send_after_undo(dir, dest, cx);
                }
            }
            Decision::Offer { choices } => {
                let names: Vec<String> = choices
                    .iter()
                    .map(|c| island_title(c.candidate.label()))
                    .collect();
                self.review_status = Some(format!("Copied · for {}?", names.join(" or ")));
            }
            Decision::Keep { .. } => {}
        }
    }

    #[cfg(target_os = "linux")]
    fn act_on_route(&mut self, _dir: PathBuf, judged: Judged, _cx: &mut Context<Self>) {
        match judged {}
    }

    /// Shows "Sending to …" for `SMART_SEND_UNDO`, then records the destination and sends,
    /// unless ✕ or Paste cancelled it (nothing has left the Mac until then).
    #[cfg(target_os = "macos")]
    fn send_after_undo(
        &mut self,
        dir: PathBuf,
        dest: crate::model::Destination,
        cx: &mut Context<Self>,
    ) {
        self.pending_send = Some(dir.clone());
        self.completed_note = Some(format!("Sending to {}…", island_title(&dest.title)));
        cx.spawn(async move |entity, cx| {
            cx.background_executor().timer(SMART_SEND_UNDO).await;
            let _ = entity.update(cx, |coord, cx| {
                if coord.pending_send.as_deref() != Some(dir.as_path()) {
                    return;
                }
                coord.pending_send = None;
                if let Err(err) = crate::handoff::load_bundle(&dir).and_then(|(_, mut bundle)| {
                    crate::routing::record_destination(&dir, &mut bundle, dest.clone())
                }) {
                    scrawl_log(format!("routing: couldn't record the destination: {err:#}"));
                }
                coord.send_completed(dir, dest, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Cancels a smart send of `dir` that is still in its undo window.
    fn keep_pending_send(&mut self, dir: &std::path::Path) {
        if self.pending_send.as_deref() == Some(dir) {
            self.pending_send = None;
            self.completed_note = None;
            scrawl_log(format!(
                "routing (smart): kept {} on the clipboard",
                dir.display()
            ));
        }
    }

    /// Sends a finished local capture to the session it was routed to, and says how that went
    /// on the Completed island and in the review window.
    fn send_completed(
        &mut self,
        dir: PathBuf,
        dest: crate::model::Destination,
        cx: &mut Context<Self>,
    ) {
        let title = island_title(&dest.title);
        self.completed_note = Some(format!("Sending to {title}…"));
        let (sender, receiver) = flume::bounded(1);
        let send_dir = dir.clone();
        let spawned = std::thread::Builder::new()
            .name("scrawl-send".into())
            .spawn(move || {
                let _ = sender.send(crate::delivery::deliver(
                    &send_dir,
                    &dest.session,
                    &dest.title,
                    None,
                    crate::handoff::send_bundle_to_session,
                ));
            });
        if spawned.is_err() {
            self.completed_note = None;
            return;
        }
        cx.spawn(async move |entity, cx| {
            let Ok(delivery) = receiver.recv_async().await else {
                return;
            };
            let _ = entity.update(cx, |coord, cx| {
                if coord.completed_dir.as_deref() == Some(dir.as_path()) {
                    coord.completed_note = Some(completed_delivery_note(&delivery, &title));
                }
                if coord
                    .review_bundle
                    .as_ref()
                    .is_some_and(|(shown, _)| *shown == dir)
                {
                    coord.review_status = Some(delivery.status("Copied to clipboard"));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The UI loop's tick: advances the springs unless a new capsule window awaits its second
    /// frame (`island_await_frame`).
    pub fn island_tick(&mut self, cx: &mut Context<Self>) {
        if self.island_await_frame {
            // Ask for the frame that ends the wait.
            cx.notify();
        } else {
            self.step_island(cx);
        }
    }

    /// Advances the capsule's springs to now. The UI loop calls it every few ms to keep frames
    /// coming, and the capsule calls it as it draws, so every frame shows its own moment rather
    /// than the loop's last tick (up to 8 ms stale, which made the steps uneven).
    pub fn step_island(&mut self, cx: &mut Context<Self>) {
        if let Some(actions) = self.native_pill.as_ref().map(|p| p.drain_actions()) {
            for action in actions {
                self.apply_pill_action(action, cx);
            }
        }
        let tune = motion::tune();
        let real = self.island_stepped.elapsed().as_secs_f32();
        if real < 0.0002 {
            return;
        }
        self.island_stepped = Instant::now();
        // A late frame slows the motion rather than skipping it ahead (a new window's second
        // frame comes ~50 ms after its first).
        let dt = (real * motion::time_scale()).min(0.025);
        if let IslandPhase::Completed { finished_at, .. } = self.island_phase
            && finished_at.elapsed() >= Duration::from_millis(25_000)
        {
            self.set_island_phase(IslandPhase::Dismissing);
        }

        if let IslandPhase::Selecting { mode } = self.island_phase {
            if let Some(pos) = current_mouse_screen() {
                self.follow_pointer_display(pos, cx);
                self.selection_pointer = Some(pos);
                if mode == SelectionMode::Area
                    && let Some((start, _)) = self.selection_drag
                {
                    self.selection_drag = Some((start, pos));
                }
                if mode == SelectionMode::Window && !self.windows_loading {
                    self.hovered_window = selectable_window_at(
                        &self.selectable_windows,
                        f64::from(pos[0]),
                        f64::from(pos[1]),
                    )
                    .cloned();
                }
            }
            if mode == SelectionMode::Window
                && let Some(ref win) = self.hovered_window
            {
                let target_rect = [
                    win.frame.x as f32,
                    win.frame.y as f32,
                    win.frame.width as f32,
                    win.frame.height as f32,
                ];
                self.animated_window_rect = Some(target_rect);
            } else {
                self.animated_window_rect = None;
            }
            self.sync_picker_overlay();
        }
        self.follow_phone_mirror();

        // Keep layout width at `island_target_width` during open and dismiss so the capsule never
        // clips its inner chips; open/dismiss use a restrained 0.92 <-> 1.0 scale.
        let (target_w, target_scale, target_y, target_opacity, fade_rate) = match self.island_phase
        {
            IslandPhase::Hidden => return,
            IslandPhase::Dismissing => (self.island_target_width, 0.92, -6.0, 0.0, 18.0),
            _ => (self.island_target_width, 1.0, 0.0, 1.0, 20.0),
        };

        if self.reveal.is_open() {
            let activity = self.reveal_activity();
            let on_pill = self
                .native_pill
                .as_ref()
                .map_or_else(pointer_on_hud_pill, |p| p.pointer_on_pill());
            if self
                .reveal
                .tick(reveal_clock_ms(), on_pill, activity)
            {
                self.play_fold();
            }
        }

        let (width, velocity) = if self.reduced_motion && self.island_morph != Morph::Phase {
            // Reduce motion: the reveal's groups fade in place with no width travel; a shrink
            // waits for the outgoing chips to fade so none are clipped while visible.
            let fading =
                self.reveal_outgoing.is_some() && motion::age(self.reveal_since) < REDUCED_FADE_S;
            if fading && target_w < self.island_width {
                (self.island_width, 0.0)
            } else {
                (target_w, 0.0)
            }
        } else {
            spring_step(
                self.island_width,
                self.island_velocity,
                target_w,
                dt,
                self.island_morph.spring(&tune),
            )
        };
        let (scale, s_vel) = spring_step(
            self.island_scale,
            self.island_scale_velocity,
            target_scale,
            dt,
            tune.island_scale,
        );
        let (y_off, y_vel) = spring_step(
            self.island_y,
            self.island_y_velocity,
            target_y,
            dt,
            tune.island_scale,
        );
        if let Some(morph) = self.switch.as_mut()
            && let Some((x, v)) = morph.slide
        {
            let (x, v) = spring_step(x, v, 0.0, dt, tune.switch_slide);
            morph.slide = Some((x, v));
            if x.abs() < 0.2 && v.abs() < 2.0 {
                self.switch = None;
            }
        }
        self.island_width = width.clamp(120.0, PILL_WINDOW_W - 16.0);
        self.island_velocity = velocity;
        self.island_scale = scale.clamp(0.35, 1.18);
        self.island_scale_velocity = s_vel;
        self.island_y = y_off.clamp(-18.0, 14.0);
        self.island_y_velocity = y_vel;
        self.island_opacity += (target_opacity - self.island_opacity) * (fade_rate * dt).min(1.0);
        self.sync_native_pill();

        let dismiss_done = if let Some(ref pill) = self.native_pill {
            pill.is_fully_hidden()
        } else {
            self.island_opacity < 0.03
        };
        if matches!(self.island_phase, IslandPhase::Dismissing) && dismiss_done {
            self.set_island_phase(IslandPhase::Hidden);
            self.island_opacity = 0.0;
            cx.defer(close_capture_windows);
        }
        if self.native_pill.is_none() {
            cx.notify();
        }
    }

    /// The menu bar's picker, opening on the mode the last recording used.
    #[cfg(target_os = "macos")]
    pub fn begin_target_selection(&mut self, cx: &mut Context<Self>) {
        if self.ready_to_record() {
            self.open_picker(None, false, cx);
        }
    }

    /// Opens the picker on `mode`, else on the mode the last recording used. `hold`: it is the
    /// held switcher (`crate::hold`), which records on release.
    fn open_picker(&mut self, mode: Option<SelectionMode>, hold: bool, cx: &mut Context<Self>) {
        if self.is_recording() || self.is_processing() {
            return;
        }
        scrawl_log(format!(
            "open_picker invoked (mode: {mode:?}, hold: {hold})"
        ));
        self.hold_picker = hold;
        let started = Instant::now();
        self.recording_shortcut_key = false;
        self.selectable_windows.clear();
        self.hovered_window = None;
        self.animated_window_rect = None;
        self.selection_pointer = current_mouse_screen();
        // The picker opens on the display under the pointer and follows it there
        // (`follow_pointer_display`); Screen mode highlights all of it.
        let screens = crate::screens::current();
        if let Some(i) =
            crate::screens::pointer().and_then(|p| crate::screens::index_at(&screens, p))
        {
            self.display_rect = screens[i].frame.map(|v| v as f32);
        }
        if let Some(ref mut overlay) = self.picker_metal_overlay {
            overlay.move_to(self.display_rect.map(f64::from));
        }
        self.selection_generation += 1;
        self.windows_loading = !self.lab;
        if !self.lab {
            self.refresh_iphone(cx);
        }
        let generation = self.selection_generation;
        cx.spawn(async move |entity, cx| {
            if entity.read_with(cx, |coord, _| coord.lab).unwrap_or(true) {
                return;
            }
            let windows = cx
                .background_executor()
                .spawn(async { list_selectable_windows() })
                .await;
            let _ = entity.update(cx, |coord, cx| {
                // A previous picker request must not overwrite a reopened picker or recording.
                if coord.selection_generation != generation || !coord.is_selecting() {
                    return;
                }
                coord.windows_loading = false;
                let ptr = current_mouse_screen().or(coord.selection_pointer);
                coord.selection_pointer = ptr;
                coord.hovered_window = ptr
                    .and_then(|[x, y]| selectable_window_at(&windows, f64::from(x), f64::from(y)))
                    .cloned();
                if matches!(
                    coord.island_phase,
                    IslandPhase::Selecting {
                        mode: SelectionMode::Window
                    }
                ) {
                    coord.animated_window_rect = coord.hovered_window.as_ref().map(|w| {
                        [
                            w.frame.x as f32,
                            w.frame.y as f32,
                            w.frame.width as f32,
                            w.frame.height as f32,
                        ]
                    });
                }
                coord.selectable_windows = windows;
                coord.sync_picker_overlay();
                start_metric("picker_ready_ms", started);
                cx.notify();
            });
        })
        .detach();
        self.selection_drag = None;
        if matches!(
            self.island_phase,
            IslandPhase::Hidden | IslandPhase::Dismissing
        ) {
            // Enter near full size (0.92 -> 1.0) at full intrinsic layout width so inner buttons
            // are never clipped.
            self.island_width = self.island_target_width.max(195.0);
            self.island_velocity = 0.0;
            self.island_scale = 0.92;
            self.island_scale_velocity = 0.0;
            self.island_y = -6.0;
            self.island_y_velocity = 0.0;
            self.island_opacity = 0.0;
            self.island_phase = IslandPhase::Hidden;
        }
        // Open on the mode the last recording used. The phone needs a live preview, started once
        // the picker is up; Window starts on Area's crosshair until the window list arrives.
        let opening = match mode.unwrap_or(self.last_mode) {
            SelectionMode::Iphone => SelectionMode::Area,
            mode => mode,
        };
        self.set_island_phase(IslandPhase::Selecting { mode: opening });
        self.open_iphone_when_found = false;
        if mode.is_none() && self.last_mode == SelectionMode::Iphone {
            if self.iphone.is_some() {
                self.preview_iphone(cx);
            } else {
                self.open_iphone_when_found = true;
            }
        }
        self.telemetry.begin_picker_session();
        self.sync_picker_overlay();
        if self.picker_metal_overlay.is_none() && !self.lab {
            let [x, y, w, h] = self.display_rect.map(f64::from);
            let overlay = MetalLaserOverlay::open(Arc::clone(&self.telemetry), Some([x, y, w, h]))
                .map_err(|error| scrawl_log(format!("Metal overlay failed: {error}")))
                .ok();
            if let Some(ref ov) = overlay {
                self.telemetry
                    .excluded_window_ids
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(ov.window_number());
                self.telemetry
                    .passthrough_window_ids
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(ov.window_number());
            }
            self.picker_metal_overlay = overlay;
        }

        let entity = cx.entity().clone();
        cx.defer(move |cx| {
            close_review_window(cx);
            open_picker_windows(&entity, cx);
            start_metric("picker_open_ms", started);
        });
        cx.notify();
        start_metric("picker_prep_ms", started);
    }

    /// From the picker: the full screen it highlights.
    pub fn start_recording_full_screen(&mut self, cx: &mut Context<Self>) {
        let [x, y, w, h] = self.display_rect;
        let centre = [f64::from(x + w / 2.0), f64::from(y + h / 2.0)];
        let target = if self.lab {
            lab::target()
        } else {
            crate::platform::discover_target_at(None, Some(centre))
        };
        self.start_recording_with_target(target, cx);
    }

    pub fn start_recording_with_target(&mut self, target: TargetInfo, cx: &mut Context<Self>) {
        if self.is_recording() || self.is_processing() {
            return;
        }
        if self.lab {
            self.start_lab_recording(target, cx);
            return;
        }
        // No surprise prompt mid-capture: the permissions are granted in onboarding.
        if !self.ready_to_record() {
            self.cancel_recording(cx);
            return;
        }
        let started = Instant::now();
        scrawl_log(format!(
            "start_recording_with_target invoked: mode={:?} frame={:?}",
            target.mode, target.frame
        ));
        // Without the picker the start needs its own cue: the overlay flashes an edge glow.
        let quick = !self.is_selecting();
        self.telemetry.start_glow.store(
            quick && target.mode == CaptureMode::Display,
            std::sync::atomic::Ordering::Release,
        );
        if let IslandPhase::Selecting { mode } = self.island_phase
            && !self.hold_picker
        {
            self.remember_mode(mode);
        }
        self.recording_shortcut_key = false;
        self.iphone_preview = None;
        self.selection_drag = None;
        self.hovered_window = None;
        self.animated_window_rect = None;
        let id = crate::make_bundle_id();
        let root = std::env::var_os("SCRAWL_BENCH_START")
            .map(PathBuf::from)
            .unwrap_or_else(crate::handoff::default_captures_dir);
        let bundle_dir = root.join(format!("{id}.scrawl"));
        let _ = std::fs::create_dir_all(&bundle_dir);
        let video_path = bundle_dir.join("recording.mp4");

        self.telemetry.set_capture_mode(target.mode);
        self.telemetry.reset_for_session(target.frame);
        set_escape_hotkey_enabled(true);
        let stage = Instant::now();
        // Reuse the continuous 120Hz Metal overlay opened in `begin_target_selection` so the
        // cutout and border never swap windows or jump a single pixel when recording starts.
        // It moves to the target's display if that's another one (a window picked there).
        let f = target.frame;
        let target_rect = [f.x, f.y, f.width, f.height];
        if let Some(ref pill) = self.native_pill {
            pill.place_on([f.x + f.width / 2.0, f.y + f.height / 2.0]);
        }
        let metal_overlay = self.picker_metal_overlay.take().map(|mut overlay| {
            overlay.move_to(target_rect);
            overlay
        });
        let metal_overlay = metal_overlay.or_else(|| {
            let ov = MetalLaserOverlay::open(Arc::clone(&self.telemetry), Some(target_rect))
                .map_err(|error| scrawl_log(format!("Metal overlay failed: {error}")))
                .ok();
            if let Some(ref o) = ov {
                self.telemetry
                    .excluded_window_ids
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(o.window_number());
                self.telemetry
                    .passthrough_window_ids
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(o.window_number());
            }
            ov
        });
        start_metric("metal_open_ms", stage);
        let stage = Instant::now();
        let audio_recorder = AudioRecorder::start().ok();
        start_metric("audio_start_ms", stage);
        let stage = Instant::now();
        let screen_session = crate::platform::start_screen_session(
            target.clone(),
            Some(video_path),
            &self.telemetry,
        );
        start_metric("sck_start_ms", stage);
        let window_log = (target.mode == CaptureMode::Display).then(|| {
            let started_at = *self
                .telemetry
                .started_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::platform::WindowLogger::start(
                target.frame,
                started_at,
                Arc::clone(&self.telemetry.window_log_request),
            )
        });
        self.active_recording = Some(ActiveRecording {
            started: Instant::now(),
            target,
            bundle_dir,
            source: CaptureSource::Screen {
                session: screen_session,
                audio: audio_recorder,
                windows: window_log,
            },
            _metal_overlay: metal_overlay,
        });
        self.present_recording_island(cx);
        start_metric("recording_start_ms", started);
    }

    /// The island turns into the recording pill: from the picker it morphs, from nothing it opens
    /// from a compact capsule so the shell springs out to its width, Dynamic Island style.
    fn present_recording_island(&mut self, cx: &mut Context<Self>) {
        if matches!(self.island_phase, IslandPhase::Hidden) {
            self.island_width = ISLAND_OPEN_FROM_W;
            self.island_velocity = 0.0;
            self.island_scale = 0.92;
            self.island_scale_velocity = 0.0;
            self.island_y = -6.0;
            self.island_y_velocity = 0.0;
        }
        self.set_island_phase(IslandPhase::Recording);
        let entity = cx.entity().clone();
        cx.defer(move |cx| {
            close_review_window(cx);
            close_picker_window(cx);
            ensure_hud_pill_open(&entity, cx);
        });
        cx.notify();
    }

    /// `scrawl lab pill`: a pretend recording with no capture, microphone, files or overlay, so
    /// the capsule goes through exactly the states a real one does.
    fn start_lab_recording(&mut self, target: TargetInfo, cx: &mut Context<Self>) {
        if let IslandPhase::Selecting { mode } = self.island_phase {
            motion::trace_event(format!("record:{}", mode.label()));
        }
        self.iphone_preview = None;
        self.selection_drag = None;
        self.hovered_window = None;
        self.animated_window_rect = None;
        self.active_recording = Some(ActiveRecording {
            started: Instant::now(),
            target,
            bundle_dir: PathBuf::new(),
            source: CaptureSource::Lab,
            _metal_overlay: None,
        });
        self.present_recording_island(cx);
    }

    /// Mirrors the cabled iPhone in the helper's window and records its native video. The
    /// window's on-screen rect is the capture target, so the laser, ink, pins and clicks over it
    /// normalize straight into phone-screen coordinates (kept current as the window moves).
    pub fn start_recording_iphone(&mut self, cx: &mut Context<Self>) {
        if self.is_recording() || self.is_processing() {
            return;
        }
        scrawl_log("start_recording_iphone invoked");
        if self.lab {
            self.start_lab_recording(crate::phone::phone_target("iPhone", None, None, 0, 0), cx);
            return;
        }
        #[cfg(target_os = "macos")]
        if !self.ready_for(crate::onboarding::Permission::Camera) {
            self.cancel_recording(cx);
            return;
        }
        self.remember_mode(SelectionMode::Iphone);
        self.recording_shortcut_key = false;
        self.selection_drag = None;
        self.hovered_window = None;
        self.animated_window_rect = None;
        let bundle_dir = crate::handoff::default_captures_dir()
            .join(format!("{}.scrawl", crate::make_bundle_id()));
        let _ = std::fs::create_dir_all(&bundle_dir);
        let movie_path = bundle_dir.join("recording.mov");
        let existing_preview = self.iphone_preview.take();
        if let Some(dev) = existing_preview.as_ref().and_then(PhoneSession::device) {
            self.iphone = Some(dev);
        }
        let device = self.iphone.clone();
        let name = device
            .as_ref()
            .map_or("iPhone", |d| d.name.as_str())
            .to_owned();
        let target = crate::phone::phone_target(&name, None, None, 0, 0);
        let initial_frame = existing_preview
            .as_ref()
            .and_then(PhoneSession::rect)
            .map_or(
                TargetFrame {
                    x: -100_000.0,
                    y: -100_000.0,
                    width: 1.0,
                    height: 1.0,
                    scale: 2.0,
                },
                |[rx, ry, rw, rh]| TargetFrame {
                    x: f64::from(rx),
                    y: f64::from(ry),
                    width: f64::from(rw),
                    height: f64::from(rh),
                    scale: 2.0,
                },
            );
        self.telemetry.set_capture_mode(CaptureMode::Device);
        self.telemetry.reset_for_session(initial_frame);
        // Hold interaction telemetry inactive until the helper's `recording` event arrives.
        self.telemetry
            .active
            .store(false, std::sync::atomic::Ordering::Release);
        set_escape_hotkey_enabled(true);
        let session = if let Some(mut preview) = existing_preview {
            preview.begin_recording(&movie_path);
            preview
        } else {
            let dev_id = device.as_ref().map(|d| d.id.as_str());
            match PhoneSession::start(&movie_path, dev_id, Some(Arc::clone(&self.telemetry))) {
                Ok(session) => session,
                Err(error) => {
                    scrawl_log(format!("iPhone capture could not start: {error:#}"));
                    self.telemetry.finish_session();
                    let _ = std::fs::remove_dir_all(&bundle_dir);
                    self.dismiss_island(cx);
                    return;
                }
            }
        };
        // Over the mirror window's display (the pointer's until the window reports its rect).
        let mirror = (initial_frame.x > -10_000.0).then_some([
            initial_frame.x,
            initial_frame.y,
            initial_frame.width,
            initial_frame.height,
        ]);
        let metal_overlay = self.picker_metal_overlay.take().or_else(|| {
            MetalLaserOverlay::open(Arc::clone(&self.telemetry), mirror)
                .map_err(|error| scrawl_log(format!("Metal overlay failed: {error}")))
                .ok()
        });
        self.active_recording = Some(ActiveRecording {
            started: Instant::now(),
            target,
            bundle_dir,
            source: CaptureSource::Phone(session),
            _metal_overlay: metal_overlay,
        });
        self.present_recording_island(cx);
    }

    /// The mirror ended without being asked: keep what was recorded, or drop an empty capture.
    #[cfg(target_os = "macos")]
    fn phone_ended(&mut self, cx: &mut Context<Self>) {
        if self.iphone_preview.is_some() {
            self.iphone_preview = None;
            self.set_selection_mode(SelectionMode::Area, cx);
            return;
        }
        let recorded = match &self.active_recording {
            Some(ActiveRecording {
                source: CaptureSource::Phone(session),
                ..
            }) => session.has_recording(),
            _ => return,
        };
        if recorded {
            self.finish_recording(cx);
        } else {
            self.cancel_recording(cx);
        }
    }

    pub fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        scrawl_log("cancel_recording invoked");
        self.discard_recording();
        self.dismiss_island(cx);
    }

    pub fn finish_recording(&mut self, cx: &mut Context<Self>) {
        scrawl_log("finish_recording invoked");
        set_escape_hotkey_enabled(false);
        let Some(rec) = self.active_recording.take() else {
            return;
        };
        let (events, strokes) = self.telemetry.finish_session();
        let bundle_dir = rec.bundle_dir;
        let target = rec.target;
        let source = rec.source;
        // Drop the Metal overlay immediately on the main thread so the screen frame clears,
        // and morph the Dynamic Island into the compact spinning "Processing…" state while
        // media teardown, transcription, and PNG encoding run on a background worker.
        drop(rec._metal_overlay);
        self.set_island_phase(IslandPhase::Processing);
        self.completed_note = None;
        cx.notify();
        if matches!(source, CaptureSource::Lab) {
            let duration_ms = rec.started.elapsed().as_millis() as u64;
            let wait = Duration::from_secs_f32(lab::PROCESSING_S / motion::time_scale());
            cx.spawn(async move |entity, cx| {
                cx.background_executor().timer(wait).await;
                let _ = entity.update(cx, |coord, cx| {
                    coord.set_island_phase(IslandPhase::Completed {
                        finished_at: Instant::now(),
                        frame_count: 6,
                        duration_ms,
                        inbox_count: 0,
                    });
                    cx.notify();
                });
            })
            .detach();
            return;
        }

        // A capture that ends on a dev site goes to its project's session; work that out (reading
        // the browser tab, the port's process, the sessions) while the bundle is built.
        let routing = Routing::start(matches!(source, CaptureSource::Screen { .. }), &target);

        cx.spawn(async move |entity, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let (screen_session, audio_recorder, window_log) = match source {
                        CaptureSource::Screen {
                            session,
                            audio,
                            windows,
                        } => (session, audio, windows),
                        CaptureSource::Phone(session) => {
                            let finalized = session.stop().and_then(|recording| {
                                crate::phone::finalize_live_capture(
                                    &bundle_dir,
                                    recording,
                                    events,
                                    strokes,
                                    None,
                                )
                            });
                            if let Err(error) = &finalized {
                                scrawl_log(format!("iPhone capture failed: {error:#}"));
                            }
                            let duration_ms =
                                finalized.as_ref().map_or(0, |(_, b, _)| b.duration_ms);
                            return (finalized, duration_ms, None);
                        }
                        CaptureSource::Lab => {
                            return (
                                Err(anyhow::anyhow!("a lab recording has no bundle")),
                                0,
                                None,
                            );
                        }
                    };
                    let stop_started = Instant::now();
                    let (recording, recorded_audio, mut activity) = std::thread::scope(|s| {
                        let screen_handle = s.spawn(|| screen_session.stop());
                        let win_handle = s.spawn(|| {
                            window_log
                                .map(crate::platform::WindowLogger::stop)
                                .unwrap_or_default()
                        });
                        let audio = audio_recorder.and_then(|r| r.stop().ok());
                        (
                            screen_handle.join().unwrap(),
                            audio,
                            win_handle.join().unwrap_or_default(),
                        )
                    });
                    activity.stop_started = Some(stop_started);
                    activity.stop_ms = Some(stop_started.elapsed().as_millis() as u64);
                    let duration_ms = recording.duration_ms;
                    let mut finalized = crate::finalize_capture_bundle(
                        &bundle_dir,
                        target,
                        recording,
                        strokes,
                        events,
                        recorded_audio,
                        None,
                        activity,
                    );
                    let judged = routing.finish(&mut finalized);
                    (finalized, duration_ms, judged)
                })
                .await;

            let _ = entity.update(cx, |coord, cx| {
                let (finalized, duration_ms, judged) = result;
                let mut frame_count = 1;
                coord.completed_dir = None;
                if let Ok((dir, bundle, _)) = finalized {
                    frame_count = bundle.keyframes.len();
                    let _ = crate::handoff::copy_bundle_to_clipboard(&dir);
                    coord.completed_dir = Some(dir.clone());
                    coord.last_local_capture_ms = crate::now_epoch_ms();
                    if let Some(dest) = bundle.destination.clone() {
                        coord.send_completed(dir.clone(), dest, cx);
                    }
                    coord.set_review_bundle((dir.clone(), bundle), coord.completed_note.clone());
                    if let Some(judged) = judged {
                        coord.act_on_route(dir, judged, cx);
                    }
                }
                coord.set_island_phase(IslandPhase::Completed {
                    finished_at: Instant::now(),
                    frame_count,
                    duration_ms,
                    inbox_count: 0,
                });
                cx.notify();
            });
        })
        .detach();
    }
}

/// Smart routing's verdict on a finished capture, and the URL it was of.
#[cfg(target_os = "macos")]
type Judged = (crate::routing::smart::Judgement, Option<String>);
/// Linux captures stay on the clipboard: there is no verdict.
#[cfg(target_os = "linux")]
type Judged = std::convert::Infallible;

/// Where a finished screen capture goes (a dev site's session), worked out while its bundle is
/// built.
#[cfg(target_os = "macos")]
struct Routing {
    route: Option<std::thread::JoinHandle<Option<crate::model::Destination>>>,
    look: Option<std::thread::JoinHandle<Option<crate::routing::look::Live>>>,
}

#[cfg(target_os = "macos")]
impl Routing {
    /// `screen`: a screen capture (the only kind routed) of `target`.
    fn start(screen: bool, target: &TargetInfo) -> Self {
        let smart = crate::handoff::experiment(crate::routing::SMART_ROUTING);
        // A window capture is of that window's app, whatever is in front at the end.
        let front = screen
            .then(|| {
                (target.mode != CaptureMode::Window)
                    .then(crate::platform::frontmost_app_info)
                    .flatten()
                    .and_then(|(_, _, id)| id)
            })
            .flatten();
        let captured = target.bundle_id.clone();
        let route = (screen && !smart).then(|| {
            let (front, captured) = (front.clone(), captured.clone());
            std::thread::spawn(move || {
                crate::routing::route_capture(front.as_deref(), captured.as_deref())
            })
        });
        let look = (screen && smart).then(|| {
            std::thread::spawn(move || {
                crate::routing::look_capture(front.as_deref(), captured.as_deref())
            })
        });
        Self { route, look }
    }

    /// Records a plain route's destination in the bundle, or judges a smart one.
    fn finish(
        self,
        finalized: &mut anyhow::Result<(PathBuf, ScrawlBundle, String)>,
    ) -> Option<Judged> {
        if let Ok((dir, bundle, _)) = finalized
            && let Some(Ok(Some(dest))) = self.route.map(std::thread::JoinHandle::join)
            && let Err(err) = crate::routing::record_destination(dir, bundle, dest)
        {
            scrawl_log(format!("routing: couldn't record the destination: {err:#}"));
        }
        match (&*finalized, self.look.map(std::thread::JoinHandle::join)) {
            (Ok((dir, bundle, _)), Some(Ok(Some(live)))) => {
                Some(crate::routing::judge_capture(dir, bundle, live))
            }
            _ => None,
        }
    }
}

/// No automatic delivery on Linux: captures go to the clipboard only.
#[cfg(target_os = "linux")]
struct Routing;

#[cfg(target_os = "linux")]
impl Routing {
    fn start(_screen: bool, _target: &TargetInfo) -> Self {
        Self
    }

    fn finish(
        self,
        _finalized: &mut anyhow::Result<(PathBuf, ScrawlBundle, String)>,
    ) -> Option<Judged> {
        None
    }
}

pub struct HudPillWindow {
    coordinator: Entity<ScrawlCoordinator>,
    focus: FocusHandle,
    ns_window: Option<NativeWindow>,
    shell_layer: Option<ShellLayer>,
    /// Frames drawn so far, counting to 2.
    frames: u8,
}

impl Render for HudPillWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.ns_window.is_none() {
            self.ns_window = find_hud_pill_ns_window();
        }
        let tracing = motion::tracing();
        let frame = self.frames;
        if tracing && frame == 0 {
            motion::trace_skip_frame();
        } else if tracing {
            motion::trace_frame();
        }
        self.frames = self.frames.saturating_add(1).min(2);
        if frame == 0 {
            // A new window shows a few frames after it's asked for; its opening fade and blur
            // start from the frame that is seen, not from the request.
            self.coordinator.update(cx, |coord, _| {
                if coord.island_opacity < 0.98
                    && !matches!(coord.island_phase, IslandPhase::Dismissing)
                {
                    coord.island_opacity = 0.0;
                    coord.island_phase_since = motion::now();
                    coord.island_stepped = Instant::now();
                    coord.island_await_frame = true;
                }
            });
        }
        self.coordinator.update(cx, |coord, cx| {
            if frame == 1 {
                coord.island_await_frame = false;
            }
            if coord.is_island_visible() {
                coord.step_island(cx);
            }
        });
        let coord = self.coordinator.read(cx);
        let phase = coord.island_phase;
        let prev_phase = coord.island_prev_phase;
        let target_phase = match phase {
            IslandPhase::Dismissing => coord.island_dismissed_from,
            other => other,
        };
        let phase_age = motion::age(coord.island_phase_since);
        let width = coord.island_width;
        let scale = coord.island_scale;
        let y_off = coord.island_y;
        let opacity = coord.island_opacity.clamp(0.0, 1.0);
        let recording_ms = coord
            .active_recording
            .as_ref()
            .map(ActiveRecording::elapsed_ms);
        let elapsed_ms = recording_ms.unwrap_or(coord.last_elapsed_ms);
        let switch = coord
            .switch
            .filter(|_| matches!(phase, IslandPhase::Selecting { .. }));
        let island_velocity = coord.island_velocity;
        let highlight_prev = coord.highlight_prev;
        let highlight_p = ease_out_cubic(motion::age(coord.highlight_since) / HIGHLIGHT_FADE_S);
        let stroke_count = coord
            .telemetry
            .strokes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|s| !s.laser)
            .count();

        let clock = format_clock(elapsed_ms);
        let has_iphone = coord.iphone.is_some();
        let completed_note = coord.completed_note.clone();
        let hold = coord.hold_picker;
        // The Recording capsule's trailing group: switch chips or controls, and the swap between.
        let chips = if matches!(phase, IslandPhase::Recording) {
            coord.reveal.is_open()
        } else {
            coord.reveal_prev_chips
        };
        let reduced = coord.reduced_motion;
        let tune = motion::tune();
        let swap_age = motion::age(coord.reveal_since);
        let (enter_s, exit_s) = if reduced {
            (REDUCED_FADE_S, REDUCED_FADE_S * 0.8)
        } else {
            (tune.reveal_enter_s, tune.reveal_exit_s)
        };
        let swap = matches!(phase, IslandPhase::Recording)
            .then_some(coord.reveal_outgoing)
            .flatten()
            .filter(|_| swap_age < enter_s.max(exit_s));
        let target_width = island_content_width(
            window,
            target_phase,
            &clock,
            has_iphone,
            hold,
            chips,
            completed_note.as_deref(),
        );
        let snap_initial_width =
            phase_age < 0.025 && matches!(phase, IslandPhase::Selecting { .. }) && opacity < 0.35;
        let switch_layout = switch.map(|morph| {
            let clock_w = measure_text(
                window,
                &clock_template(&clock),
                HUD_TEXT,
                FontWeight::SEMIBOLD,
            );
            SwitchLayout::new(morph, width, target_width, clock_w)
        });
        self.coordinator.update(cx, |coord, _| {
            coord.island_target_width = target_width;
            if snap_initial_width {
                coord.island_width = target_width;
            }
            if recording_ms.is_some() {
                coord.last_elapsed_ms = elapsed_ms;
            }
            if let (Some(morph), Some(layout)) = (coord.switch.as_mut(), switch_layout)
                && morph.slide.is_none()
            {
                // The chips were pinned to the left edge, so they moved at half the width's
                // speed: the row starts there, at that speed.
                morph.slide = Some((layout.dx, -0.5 * island_velocity));
            }
        });
        let render_width = if snap_initial_width {
            target_width
        } else {
            width
        };

        // Two-beat `.blurReplace` cross-transition inside the springing shell:
        // - When morphing between two visible states (`has_prev`):
        //   * 0..55ms (exit): outgoing `prev_phase` stays mounted while blurring `0 -> 3.5px` and fading `1 -> 0.15`.
        //   * 55..140ms (enter): incoming `target_phase` mounts and unblurs `3.5 -> 0px` while fading `0.15 -> 1`.
        // - When opening/dismissing: subtle `3.5px -> 0px` blur over 110ms.
        // A switch chip's morph keeps its items, so nothing is replaced (`SwitchMorph`).
        let has_prev = !matches!(prev_phase, IslandPhase::Hidden | IslandPhase::Dismissing)
            && !matches!(phase, IslandPhase::Hidden | IslandPhase::Dismissing)
            && switch_layout.is_none();
        // Reduce motion keeps the cross-fade and drops the blur.
        let swap_blur = if reduced { 0.0 } else { 3.5 };
        let (content_phase, blur_radius, content_alpha) =
            if matches!(phase, IslandPhase::Dismissing) {
                (
                    target_phase,
                    ((1.0 - opacity) * 3.5).clamp(0.0, 3.5),
                    opacity,
                )
            } else if switch_layout.is_some() {
                (target_phase, 0.0, 1.0)
            } else if has_prev && phase_age < 0.055 {
                let t_out = (phase_age / 0.055).clamp(0.0, 1.0);
                (prev_phase, swap_blur * t_out, 1.0 - 0.85 * t_out)
            } else if has_prev && phase_age < 0.145 {
                let t_in = ease_out_cubic(((phase_age - 0.055) / 0.090).clamp(0.0, 1.0));
                (target_phase, swap_blur * (1.0 - t_in), 0.15 + 0.85 * t_in)
            } else if swap.is_some() && !reduced {
                // Chips ↔ controls in place: a sine blur pulse, sharp at both ends.
                let t = (swap_age / enter_s).clamp(0.0, 1.0);
                (
                    target_phase,
                    tune.reveal_blur_px * (std::f32::consts::PI * t).sin(),
                    1.0,
                )
            } else if !has_prev && matches!(target_phase, IslandPhase::Recording) {
                // Opening straight into Recording: the shell springs out from a compact capsule
                // with the lead pinned to its left edge, and each trailing item fades in as the
                // edge uncovers it (`edge_alpha`), so nothing waits for the shell.
                (target_phase, 0.0, 1.0)
            } else if !has_prev && opacity < 0.999 {
                // Opening: the content sharpens as the capsule fades in (both step-driven).
                (target_phase, swap_blur * (1.0 - opacity), 1.0)
            } else {
                (target_phase, 0.0, 1.0)
            };
        // The swap's blur leaves the content's scale alone so the pinned lead stays put.
        let content_shrink = if swap.is_some() { 0.0 } else { 0.015 };
        let is_completed = matches!(target_phase, IslandPhase::Completed { .. });

        if let Some(ref ns_win) = self.ns_window {
            apply_pill_layer_effect(
                ns_win,
                &mut self.shell_layer,
                render_width,
                ISLAND_H,
                scale,
                y_off,
                opacity,
                is_completed,
                blur_radius,
                content_shrink,
            );
        }
        let trace_xf = self.ns_window.as_ref().filter(|_| tracing).map(|ns_win| {
            trace_capsule(
                ns_win,
                render_width,
                scale,
                y_off,
                opacity,
                content_alpha,
                blur_radius,
                content_shrink,
            )
        });

        let c_paste_down = self.coordinator.clone();
        let c_paste_up = self.coordinator.clone();
        let c_paste_out = self.coordinator.clone();
        let c_inspect = self.coordinator.clone();
        let c_dismiss = self.coordinator.clone();
        let c_record = self.coordinator.clone();
        let c_sel_cancel = self.coordinator.clone();

        // Keep typography (`HUD_TEXT`) and spacing (`CHIP_GAP`) 100% constant so glyphs never
        // re-hint or jump; all scaling happens in the GPU CALayer transform.
        let row = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(CHIP_GAP))
            .text_size(px(HUD_TEXT))
            .whitespace_nowrap()
            .opacity(content_alpha);
        // Traces a row's children under these names (`SCRAWL_MOTION_TRACE`).
        let traced = |group: Div, keys: Vec<Option<(String, f32)>>| match trace_xf {
            Some(xf) => group.on_children_prepainted(motion::trace_children(keys, xf, "row")),
            None => group,
        };
        let named = |names: &[&str]| -> Vec<Option<(String, f32)>> {
            names.iter().map(|n| Some(((*n).to_owned(), 1.0))).collect()
        };

        // The highlight cross-fades from the mode it left.
        let highlight = |m: SelectionMode, mode: SelectionMode| -> f32 {
            match highlight_prev.filter(|prev| *prev != mode) {
                _ if m == mode && highlight_prev.is_none() => 1.0,
                Some(_) if m == mode => highlight_p,
                Some(prev) if m == prev => 1.0 - highlight_p,
                _ => f32::from(u8::from(m == mode)),
            }
        };
        // Per-item opacity of the picker row while it slides in (`SwitchMorph`).
        let selecting_alphas = |mode: SelectionMode| -> Vec<f32> {
            let Some(layout) = switch_layout else {
                return Vec::new();
            };
            let modes = SelectionMode::available(has_iphone || hold);
            let mut widths: Vec<f32> = modes.iter().map(|&m| segment_width(window, m)).collect();
            if mode.shows_record() && !hold {
                widths.push(chip_width(window, "Record", FontWeight::SEMIBOLD));
            }
            widths.extend([layout.divider_w, CHIP_H]);
            trail_offsets(layout.row_left(), &widths)
                .iter()
                .zip(&widths)
                .map(|(&x, &w)| uncover_alpha(layout.inner_now + ISLAND_INSET, x, w))
                .collect()
        };
        let ghost = switch_layout.map(|layout| {
            let pieces = layout.ghost_alphas();
            div()
                .absolute()
                .top(px(ISLAND_INSET))
                .left(px(ISLAND_INSET + layout.ghost_left()))
                .h(px(CHIP_H))
                .flex()
                .items_center()
                .gap(px(CHIP_GAP))
                .text_size(px(HUD_TEXT))
                .whitespace_nowrap()
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .pr(px(LEAD_TRAIL))
                        .child(
                            div()
                                .size(px(CHIP_H))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .opacity(pieces[0])
                                .child(div().size(px(8.0)).rounded_full().bg(rgb(INK_RED))),
                        )
                        .child(
                            div()
                                .w(px(layout.clock_w))
                                .font(tabular_font(FontWeight::SEMIBOLD))
                                .opacity(pieces[1])
                                .child(clock.clone()),
                        ),
                )
                .child(island_divider().opacity(pieces[2]))
        });
        let ghost = match (ghost, trace_xf) {
            (Some(g), Some(xf)) => {
                let lead_alpha = switch_layout.map_or(0.0, |l| l.ghost_alphas()[1]);
                let divider_alpha = switch_layout.map_or(0.0, |l| l.ghost_alphas()[2]);
                Some(g.on_children_prepainted(motion::trace_children(
                    vec![
                        Some(("lead".into(), lead_alpha)),
                        Some(("divider.lead".into(), divider_alpha)),
                    ],
                    xf,
                    "row",
                )))
            }
            (ghost, _) => ghost,
        };

        let capsule_content = match content_phase {
            IslandPhase::Selecting { mode } => {
                let alphas = selecting_alphas(mode);
                let alpha = |i: usize| alphas.get(i).copied().unwrap_or(1.0);
                let n_modes = SelectionMode::available(has_iphone || hold).len();
                let record = mode.shows_record() && !hold;
                let after = n_modes + usize::from(record);
                traced(
                    row.when_some(switch_layout, |row, layout| {
                        row.relative().left(px(layout.dx))
                    }),
                    SelectionMode::available(has_iphone || hold)
                        .iter()
                        .map(|m| m.label())
                        .map(|label| format!("chip:{label}"))
                        .chain(record.then(|| "record".to_owned()))
                        .chain(["divider.close".to_owned(), "close".to_owned()])
                        .enumerate()
                        .map(|(i, key)| Some((key, alpha(i))))
                        .collect(),
                )
                .children(
                    SelectionMode::available(has_iphone || hold)
                        .iter()
                        .enumerate()
                        .map(|(i, &segment)| {
                            let c_segment = self.coordinator.clone();
                            mode_segment_level(segment, highlight(segment, mode))
                                .opacity(alpha(i))
                                .on_click(move |_, _, cx| {
                                    c_segment.update(cx, |coord, cx| {
                                        coord.click_selection_mode(segment, cx)
                                    });
                                })
                        }),
                )
                .when(record, |row| {
                    row.child(
                        primary_chip("sel-record", "Record")
                            .opacity(alpha(n_modes))
                            .on_click(move |_, _, cx| {
                                c_record.update(cx, |coord, cx| coord.record_selected_mode(cx));
                            }),
                    )
                })
                .child(island_divider().opacity(alpha(after)))
                .child(
                    icon_circle_chip("sel-cancel", "✕")
                        .opacity(alpha(after + 1))
                        .on_click(move |_, _, cx| {
                            c_sel_cancel.update(cx, |coord, cx| coord.cancel_recording(cx));
                        }),
                )
            }
            IslandPhase::Completed { inbox_count, .. } => traced(
                row,
                named(&["done.lead", "divider.lead", "paste", "inspect", "close"]),
            )
            .child(
                island_lead()
                    .child(
                        div()
                            .size(px(CHIP_H))
                            .rounded_full()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgba(0x34c7592e))
                            .text_color(rgb(POSITIVE_COLOR))
                            .text_size(px(13.0))
                            .font_weight(FontWeight::BOLD)
                            .child("✓"),
                    )
                    .child(
                        div()
                            .ml(px(8.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT_COLOR))
                            .child(completed_label(inbox_count, completed_note.as_deref())),
                    ),
            )
            .child(island_divider())
            .child(
                primary_chip("hud-paste", "Paste")
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                        let start = current_mouse_screen();
                        c_paste_down.update(cx, |coord, _| {
                            coord.paste_drag_start = start;
                        });
                    })
                    .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                        c_paste_up.update(cx, |coord, cx| {
                            coord.paste_drag_start = None;
                            coord.paste_completed_bundle(None, cx);
                        });
                    })
                    .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
                        let end = current_mouse_screen();
                        c_paste_out.update(cx, |coord, cx| {
                            if let (Some(start), Some(drop_pt)) =
                                (coord.paste_drag_start.take(), end)
                            {
                                let dist = (drop_pt[0] - start[0]).hypot(drop_pt[1] - start[1]);
                                if dist >= 24.0 {
                                    coord.paste_completed_bundle(Some(drop_pt), cx);
                                }
                            }
                        });
                    }),
            )
            .child(
                pill_chip("hud-inspect", "Inspect", true).on_click(move |_, _, cx| {
                    c_inspect.update(cx, |coord, cx| coord.inspect_completed_bundle(cx));
                }),
            )
            .child(
                icon_circle_chip("hud-dismiss", "✕").on_click(move |_, _, cx| {
                    c_dismiss.update(cx, |coord, cx| coord.dismiss_completed_bundle(cx));
                }),
            ),
            IslandPhase::Recording => {
                // A soft ripple leaves the dot every 1.6s while the dot itself breathes.
                let cycle = 1.6;
                let ripple = (phase_age / cycle).fract();
                let breath = 0.5 + 0.5 * (std::f32::consts::TAU * phase_age / cycle).cos();
                let halo = 8.0 + 12.0 * ease_out_cubic(ripple);
                let clock_w = measure_text(
                    window,
                    &clock_template(&clock),
                    HUD_TEXT,
                    FontWeight::SEMIBOLD,
                );
                traced(
                    row,
                    vec![
                        Some(("lead".into(), 1.0)),
                        Some(("divider.lead".into(), 1.0)),
                        None,
                    ],
                )
                .child(
                    island_lead()
                        .child(
                            div()
                                .size(px(CHIP_H))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .size(px(halo))
                                        .rounded_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .bg(with_alpha(INK_RED, 0.34 * (1.0 - ripple).powi(2)))
                                        .child(
                                            div()
                                                .size(px(8.0))
                                                .rounded_full()
                                                .bg(with_alpha(INK_RED, 0.72 + 0.28 * breath)),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .w(px(clock_w))
                                .font(tabular_font(FontWeight::SEMIBOLD))
                                .child(clock),
                        ),
                )
                .child(island_divider())
                .child({
                    // The trailing slot: switch chips or controls. The lead is pinned to the
                    // shell's left edge (`justify_start` below), so each item's fade follows how
                    // far the springing right edge has uncovered it.
                    let trail_x0 = CHIP_H
                        + clock_w
                        + LEAD_TRAIL
                        + CHIP_GAP
                        + (1.0 + 2.0 * DIVIDER_MX)
                        + CHIP_GAP;
                    let inner_now = render_width - 2.0 * ISLAND_INSET;
                    let alphas = |group_chips: bool, fade: f32| -> (Vec<f32>, f32) {
                        let widths = recording_trail_widths(window, group_chips, has_iphone);
                        let alphas = trail_offsets(trail_x0, &widths)
                            .iter()
                            .zip(&widths)
                            .map(|(&x, &w)| fade * edge_alpha(inner_now, x, w))
                            .collect();
                        let total = widths.iter().sum::<f32>()
                            + widths.len().saturating_sub(1) as f32 * CHIP_GAP;
                        (alphas, total)
                    };
                    let enter = swap.map_or(1.0, |_| ease_out_cubic(swap_age / enter_s));
                    let (in_alphas, in_w) = alphas(chips, enter);
                    let outgoing = swap.and_then(|out_chips| {
                        let fade = 1.0 - ease_out_cubic(swap_age / exit_s);
                        (fade > 0.002).then(|| {
                            let (out_alphas, _) = alphas(out_chips, fade);
                            traced(
                                self.recording_trail(
                                    out_chips,
                                    has_iphone,
                                    stroke_count,
                                    &out_alphas,
                                    false,
                                ),
                                trail_keys(out_chips, has_iphone, &out_alphas, "out:"),
                            )
                        })
                    });
                    div()
                        .relative()
                        .flex()
                        .flex_none()
                        .items_center()
                        .h(px(CHIP_H))
                        .w(px(in_w))
                        .when_some(outgoing, |slot, group| {
                            slot.child(div().absolute().left(px(0.0)).top(px(0.0)).child(group))
                        })
                        .child(traced(
                            self.recording_trail(chips, has_iphone, stroke_count, &in_alphas, true),
                            trail_keys(chips, has_iphone, &in_alphas, ""),
                        ))
                })
            }
            IslandPhase::Processing => {
                // 120Hz 8-dot orbiting progress ring inside the leading 32px slot.
                let dots = (0..8).map(|i| {
                    let frac = i as f32 / 8.0;
                    let angle = frac * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
                    let turn = (phase_age * 2.1 - frac).rem_euclid(1.0);
                    let intensity = (1.0 - turn).powi(2);
                    let dot_r = 1.35 + 0.65 * intensity;
                    let cx_dot = 16.0 + 7.2 * angle.cos() - dot_r;
                    let cy_dot = 16.0 + 7.2 * angle.sin() - dot_r;
                    div()
                        .absolute()
                        .left(px(cx_dot))
                        .top(px(cy_dot))
                        .size(px(dot_r * 2.0))
                        .rounded_full()
                        .bg(with_alpha(TEXT_COLOR, 0.20 + 0.80 * intensity))
                });
                traced(row, named(&["processing"])).child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .pr(px(CHIP_PX))
                        .whitespace_nowrap()
                        .child(div().size(px(CHIP_H)).flex_none().relative().children(dots))
                        .child(
                            div()
                                .ml(px(6.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT_COLOR))
                                .child("Processing…"),
                        ),
                )
            }
            IslandPhase::Hidden | IslandPhase::Dismissing => row,
        };

        let has_shell = self.shell_layer.is_some();
        let c_keys = self.coordinator.clone();
        let pin_lead = matches!(content_phase, IslandPhase::Recording);

        div()
            .track_focus(&self.focus)
            // Clicking a chip can make the pill the key window; keep the picker's keyboard map.
            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                if c_keys.update(cx, |coord, cx| {
                    coord.handle_selection_key(&event.keystroke, cx)
                }) {
                    cx.stop_propagation();
                }
            })
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .opacity(opacity)
            .child(
                div()
                    .w(px(render_width))
                    .h(px(ISLAND_H))
                    .flex_none()
                    .p(px(if has_shell { ISLAND_INSET } else { ISLAND_PAD }))
                    .rounded(px(ISLAND_RADIUS))
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .when(pin_lead, |d| d.justify_start())
                    .when(!pin_lead, |d| d.justify_center())
                    .when(!has_shell, |d| {
                        d.bg(rgba(0x141416f5))
                            .border_1()
                            .border_color(if is_completed {
                                rgba(0x69d89f4d)
                            } else {
                                rgba(0xffffff1a)
                            })
                    })
                    .text_color(rgb(TEXT_COLOR))
                    .text_size(px(HUD_TEXT))
                    .whitespace_nowrap()
                    .relative()
                    .when_some(ghost, |d, ghost| d.child(ghost))
                    .child(capsule_content),
            )
    }
}

/// Where a switch chip's morph is this frame (`SwitchMorph`), in points from the capsule's
/// inner left edge.
#[derive(Clone, Copy, Debug)]
struct SwitchLayout {
    /// The picker row's offset from its resting place.
    dx: f32,
    clock_w: f32,
    lead_w: f32,
    divider_w: f32,
    /// The capsule's inner width now, and the picker row's width.
    inner_now: f32,
    inner_row: f32,
}

impl SwitchLayout {
    fn new(morph: SwitchMorph, width: f32, target_width: f32, clock_w: f32) -> Self {
        let lead_w = CHIP_H + clock_w + LEAD_TRAIL;
        let divider_w = 1.0 + 2.0 * DIVIDER_MX;
        let inner_row = target_width - 2.0 * ISLAND_INSET;
        let dx = morph.slide.map_or_else(
            || {
                // From the capsule's centre: where the chips were, pinned behind the lead, and
                // where the centred picker row rests.
                let chips = -width / 2.0 + ISLAND_INSET + lead_w + divider_w + 2.0 * CHIP_GAP;
                chips + inner_row / 2.0
            },
            |(x, _)| x,
        );
        Self {
            dx,
            clock_w,
            lead_w,
            divider_w,
            inner_now: width - 2.0 * ISLAND_INSET,
            inner_row,
        }
    }

    fn row_left(&self) -> f32 {
        (self.inner_now - self.inner_row) / 2.0 + self.dx
    }

    /// The leaving lead and divider sit just in front of the row, as they did in the pill.
    fn ghost_left(&self) -> f32 {
        self.row_left() - 2.0 * CHIP_GAP - self.divider_w - self.lead_w
    }

    /// Dot, clock and divider each fade out as they near the capsule's left edge and are gone
    /// when they reach it, so nothing is cut by a hard edge while it still shows.
    fn ghost_alphas(&self) -> [f32; 3] {
        // Each piece's left edge in the pill, from the shell's outer edge.
        let rests = [
            ISLAND_INSET + CHIP_H / 2.0 - 4.0,
            ISLAND_INSET + CHIP_H,
            ISLAND_INSET + self.lead_w + CHIP_GAP + DIVIDER_MX,
        ];
        rests.map(|rest| {
            let left = self.ghost_left() + rest;
            let t = (left / rest.min(40.0)).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        })
    }
}

impl HudPillWindow {
    /// One trailing group of the Recording capsule, each item faded by `alphas`: the switch chips
    /// (Screen selected, as a tap records the screen) or Undo · Cancel · Finish. An outgoing
    /// group isn't `interactive`.
    fn recording_trail(
        &self,
        chips: bool,
        has_iphone: bool,
        stroke_count: usize,
        alphas: &[f32],
        interactive: bool,
    ) -> Div {
        let alpha = |i: usize| alphas.get(i).copied().unwrap_or(1.0);
        let group = div().flex().flex_none().items_center().gap(px(CHIP_GAP));
        if chips {
            return group.children(SelectionMode::available(has_iphone).iter().enumerate().map(
                |(i, &mode)| {
                    let c_switch = self.coordinator.clone();
                    mode_segment(mode, mode == SelectionMode::Screen)
                        .opacity(alpha(i))
                        .when(interactive, |d| {
                            d.on_click(move |_, _, cx| {
                                c_switch.update(cx, |coord, cx| coord.switch_recording(mode, cx));
                            })
                        })
                },
            ));
        }
        let c_undo = self.coordinator.clone();
        let c_cancel = self.coordinator.clone();
        let c_finish = self.coordinator.clone();
        group
            .child(
                pill_chip("hud-undo", "Undo", stroke_count > 0)
                    .opacity(alpha(0))
                    .when(interactive && stroke_count > 0, |d| {
                        d.on_click(move |_, _, cx| {
                            c_undo.update(cx, |coord, cx| coord.undo_stroke(cx));
                        })
                    }),
            )
            .child(
                pill_chip("hud-cancel", "Cancel", true)
                    .opacity(alpha(1))
                    .when(interactive, |d| {
                        d.on_click(move |_, _, cx| {
                            c_cancel.update(cx, |coord, cx| coord.cancel_recording(cx));
                        })
                    }),
            )
            .child(
                primary_chip("hud-done", "Finish")
                    .opacity(alpha(2))
                    .when(interactive, |d| {
                        d.on_click(move |_, _, cx| {
                            c_finish.update(cx, |coord, cx| coord.finish_recording(cx));
                        })
                    }),
            )
    }
}

/// Leading status cluster: a 32px slot concentric with the capsule's left cap, then text.
/// Also acts as the drag handle for the floating island.
fn island_lead() -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .pr(px(LEAD_TRAIL))
        .whitespace_nowrap()
        .on_mouse_down(MouseButton::Left, |_, window, _| {
            window.start_window_move();
        })
}

fn island_divider() -> Div {
    div()
        .w(px(1.0))
        .h(px(16.0))
        .mx(px(DIVIDER_MX))
        .flex_none()
        .bg(rgba(0xffffff1a))
}

fn hud_chip(id: &'static str) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .h(px(CHIP_H))
        .px(px(CHIP_PX))
        .rounded(px(CHIP_RADIUS))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .whitespace_nowrap()
}

fn pill_chip(id: &'static str, label: &'static str, enabled: bool) -> gpui::Stateful<Div> {
    hud_chip(id)
        .font_weight(FontWeight::MEDIUM)
        .text_color(if enabled {
            rgb(TEXT_SOFT)
        } else {
            rgb(0x56565a)
        })
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(|s| s.bg(rgba(0xffffff14)).text_color(rgb(TEXT_COLOR)))
        })
        .child(label)
}

fn primary_chip(id: &'static str, label: &'static str) -> gpui::Stateful<Div> {
    hud_chip(id)
        .bg(rgb(ACCENT_COLOR))
        .text_color(rgb(TEXT_COLOR))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(ACCENT_HOVER)))
        .child(label)
}

fn icon_circle_chip(id: &'static str, icon: &'static str) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .size(px(CHIP_H))
        .rounded_full()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .text_size(px(12.0))
        .text_color(rgb(MUTED_COLOR))
        .cursor_pointer()
        .hover(|s| s.bg(rgba(0xffffff14)).text_color(rgb(TEXT_COLOR)))
        .child(icon)
}

/// One segment of the selection-mode control: line icon + label. Weight stays constant across
/// states so switching never shifts the measured capsule width (`selecting_row_width`).
fn mode_segment(mode: SelectionMode, active: bool) -> gpui::Stateful<Div> {
    mode_segment_level(mode, f32::from(u8::from(active)))
}

fn mix_rgb(from: u32, to: u32, t: f32) -> gpui::Rgba {
    let t = t.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let a = ((from >> shift) & 0xff) as f32;
        let b = ((to >> shift) & 0xff) as f32;
        ((a + (b - a) * t).round() as u32) << shift
    };
    rgb(channel(16) | channel(8) | channel(0))
}

/// A mode chip `level` of the way to selected, so the highlight can cross-fade between chips.
fn mode_segment_level(mode: SelectionMode, level: f32) -> gpui::Stateful<Div> {
    let group = mode.chip_id();
    let level = level.clamp(0.0, 1.0);
    let active = level > 0.5;
    hud_chip(group)
        .group(group)
        .px(px(SEG_PX))
        .gap(px(SEG_ICON_GAP))
        .font_weight(FontWeight::MEDIUM)
        .bg(with_alpha(0xffffff, 0x26 as f32 / 255.0 * level))
        .text_color(mix_rgb(TEXT_SOFT, TEXT_COLOR, level))
        .cursor_pointer()
        .hover(move |s| {
            s.bg(if active {
                rgba(0xffffff2e)
            } else {
                rgba(0xffffff14)
            })
            .text_color(rgb(TEXT_COLOR))
        })
        // `svg()` tints with its own text colour, so hover is mirrored through the group.
        .child(
            svg()
                .path(mode.icon())
                .size(px(SEG_ICON))
                .flex_none()
                .text_color(mix_rgb(MUTED_COLOR, TEXT_COLOR, level))
                .group_hover(group, |s| s.text_color(rgb(TEXT_COLOR))),
        )
        .child(mode.label())
}

/// CleanShot-style floating camera cursor rendered at `(x, y)` in `SelectionMode::Window`.
fn camera_cursor_badge(x: f32, y: f32) -> Div {
    div()
        .absolute()
        .left(px(x - 20.0))
        .top(px(y - 20.0))
        .size(px(40.0))
        .rounded_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .bg(rgba(0x161619eb))
        .border_1()
        .border_color(rgba(0xffffff38))
        .shadow(vec![BoxShadow {
            color: gpui::hsla(0.0, 0.0, 0.0, 0.45),
            offset: point(px(0.0), px(3.0)),
            blur_radius: px(10.0),
            spread_radius: px(0.0),
        }])
        .child(
            // Top viewfinder hump
            div()
                .w(px(9.0))
                .h(px(2.5))
                .rounded_t(px(1.5))
                .bg(rgb(0xf5f5f7)),
        )
        .child(
            // Camera body + lens
            div()
                .w(px(21.0))
                .h(px(14.5))
                .rounded(px(3.5))
                .bg(rgb(0xf5f5f7))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .size(px(9.0))
                        .rounded_full()
                        .bg(rgb(0x161619))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(div().size(px(4.5)).rounded_full().bg(rgb(0xf5f5f7))),
                ),
        )
}

pub struct TargetPickerWindow {
    coordinator: Entity<ScrawlCoordinator>,
    focus: FocusHandle,
}

impl Render for TargetPickerWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let coord = self.coordinator.read(cx);
        let mode = match coord.island_phase {
            IslandPhase::Selecting { mode } => mode,
            _ => SelectionMode::Area,
        };
        let drag = coord.selection_drag;
        let hovered = coord.hovered_window.clone();
        let anim_rect = coord.animated_window_rect;
        let pointer = coord.selection_pointer;
        let picker_fade = ease_out_cubic(motion::age(coord.island_phase_since) / 0.15);

        let c_down = self.coordinator.clone();
        let c_move = self.coordinator.clone();
        let c_up = self.coordinator.clone();
        let c_key = self.coordinator.clone();

        let win_bounds = window.bounds();
        // The picker covers one display; pointer, windows and areas are global points.
        let origin = [coord.display_rect[0], coord.display_rect[1]];
        let screen = [
            f32::from(win_bounds.size.width),
            f32::from(win_bounds.size.height),
        ];

        let iphone_rect = coord.iphone_preview.as_ref().and_then(PhoneSession::rect);
        let iphone_name = coord
            .iphone_preview
            .as_ref()
            .and_then(PhoneSession::device)
            .or_else(|| coord.iphone.clone())
            .map_or_else(|| "iPhone".to_string(), |d| d.name);
        let hold = coord.hold_picker;
        let shortcut = coord.shortcut_label();
        let (hold_mods, hold_key) = crate::platform::shortcut_parts(&shortcut);
        let no_phone = hold && iphone_rect.is_none() && coord.iphone.is_none();

        // Selection rect in window-local coordinates, clipped to the screen, plus its badge.
        let highlight: Option<([f32; 4], String)> = match mode {
            SelectionMode::Area => drag.map(|(a, b)| {
                let [a, b] = [a, b].map(|p| clamp_point(p, coord.display_rect));
                let w = (a[0] - b[0]).abs().max(1.0);
                let h = (a[1] - b[1]).abs().max(1.0);
                (
                    [a[0].min(b[0]) - origin[0], a[1].min(b[1]) - origin[1], w, h],
                    format!("{} × {}", w.round() as i32, h.round() as i32),
                )
            }),
            SelectionMode::Window => hovered.map(|t| {
                let title = match (&t.app, &t.window_title) {
                    (Some(app), Some(win)) if !win.is_empty() && win != app => {
                        format!("{app} — {win}")
                    }
                    (Some(app), _) => app.clone(),
                    _ => format!(
                        "{} × {}",
                        t.frame.width.round() as i32,
                        t.frame.height.round() as i32
                    ),
                };
                let [rx, ry, rw, rh] = anim_rect.unwrap_or([
                    t.frame.x as f32,
                    t.frame.y as f32,
                    t.frame.width as f32,
                    t.frame.height as f32,
                ]);
                ([rx - origin[0], ry - origin[1], rw, rh], title)
            }),
            SelectionMode::Screen if hold => Some((
                [0.0, 0.0, screen[0], screen[1]],
                format!(
                    "Full screen · {hold_key} to switch · drag for an area · release {hold_mods} to record"
                ),
            )),
            SelectionMode::Screen => Some((
                [0.0, 0.0, screen[0], screen[1]],
                "Full screen · Click to record".to_string(),
            )),
            SelectionMode::Iphone if no_phone => Some((
                [0.0, 0.0, screen[0], screen[1]],
                "No iPhone connected".to_string(),
            )),
            SelectionMode::Iphone => iphone_rect.map(|[rx, ry, rw, rh]| {
                let action = if hold {
                    format!("release {hold_mods} to record")
                } else {
                    "Click to record".to_string()
                };
                (
                    [rx - origin[0], ry - origin[1], rw, rh],
                    format!("{iphone_name} · {action}"),
                )
            }),
        }
        .map(|(rect, label)| (clip_rect(rect, screen), label));

        let badge = highlight.as_ref().map(|(rect, label)| {
            let max_text = (screen[0] - 2.0 * BADGE_MARGIN - 2.0 * BADGE_PX - 2.0).min(480.0);
            let measured_label = if mode == SelectionMode::Area {
                clock_template(label)
            } else {
                label.clone()
            };
            let text_w =
                measure_text(window, &measured_label, BADGE_TEXT, FontWeight::MEDIUM).min(max_text);
            let size = [text_w + 2.0 * BADGE_PX + 2.0, BADGE_H];
            let anchor = match mode {
                SelectionMode::Iphone if no_phone => BadgeAnchor::Center,
                SelectionMode::Area | SelectionMode::Iphone => BadgeAnchor::BelowTrailing,
                SelectionMode::Window | SelectionMode::Screen => BadgeAnchor::Center,
            };
            (
                place_badge(*rect, size, screen, anchor),
                text_w,
                label.clone(),
            )
        });

        // The 120Hz `MetalLaserOverlay` draws the backdrop dim, selection cutout, window tint,
        // and inset border continuously across both `Selecting` and `Recording` so there is zero
        // window swap or border jump on recording start. `TargetPickerWindow` only provides the
        // hit-test surface, cursor style, camera cursor follower, and tabular dimension badge.
        let _ = picker_fade;
        let mut root = div()
            .id("scrawl-target-picker")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .when(mode == SelectionMode::Area, |d| d.cursor_crosshair())
            .when(
                matches!(mode, SelectionMode::Iphone | SelectionMode::Screen),
                |d| d.cursor_pointer(),
            )
            .when(mode == SelectionMode::Window, |d| {
                d.cursor(gpui::CursorStyle::None)
            });

        if mode == SelectionMode::Window
            && let Some([px_screen, py_screen]) = pointer
        {
            let cx_local = px_screen - origin[0];
            let cy_local = py_screen - origin[1];
            root = root.child(camera_cursor_badge(cx_local, cy_local));
        }

        root.when_some(badge, |d, ([bx, by], text_w, label)| {
            d.child(
                div()
                    .absolute()
                    .left(px(bx))
                    .top(px(by))
                    .h(px(BADGE_H))
                    .px(px(BADGE_PX))
                    .rounded(px(BADGE_H / 2.0))
                    .flex()
                    .items_center()
                    .bg(rgba(0x1c1c1ef2))
                    .border_1()
                    .border_color(rgba(0xffffff21))
                    .shadow(vec![BoxShadow {
                        color: gpui::hsla(0.0, 0.0, 0.0, 0.3),
                        offset: point(px(0.0), px(2.0)),
                        blur_radius: px(8.0),
                        spread_radius: px(0.0),
                    }])
                    .text_size(px(BADGE_TEXT))
                    .font(tabular_font(FontWeight::MEDIUM))
                    .text_color(rgb(TEXT_COLOR))
                    .child(div().w(px(text_w)).truncate().child(label)),
            )
        })
        .on_key_down(move |event: &KeyDownEvent, _, cx| {
            // Unhandled, gpui hands the key back to AppKit, which offers it again (as a key
            // equivalent, keyDown, or input method command) and this runs up to three times:
            // Space and the arrows then toggle or cycle back to where they started.
            if c_key.update(cx, |coord, cx| {
                coord.handle_selection_key(&event.keystroke, cx)
            }) {
                cx.stop_propagation();
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            move |event: &MouseDownEvent, _window, cx| {
                let [sx, sy] = current_mouse_screen()
                    .unwrap_or([f32::from(event.position.x), f32::from(event.position.y)]);
                c_down.update(cx, |coord, cx| {
                    let cur_mode = match coord.island_phase {
                        IslandPhase::Selecting { mode } => mode,
                        _ => return,
                    };
                    // The held switcher: a drag selects an area; releasing ⌥⇧ records.
                    if coord.hold_picker {
                        coord.selection_drag = Some(([sx, sy], [sx, sy]));
                        if cur_mode == SelectionMode::Area {
                            coord.sync_picker_overlay();
                            cx.notify();
                        }
                        return;
                    }
                    match cur_mode {
                        SelectionMode::Area => {
                            coord.selection_drag = Some(([sx, sy], [sx, sy]));
                            coord.sync_picker_overlay();
                            cx.notify();
                        }
                        SelectionMode::Iphone => {
                            if let Some([rx, ry, rw, rh]) =
                                coord.iphone_preview.as_ref().and_then(PhoneSession::rect)
                                && sx >= rx
                                && sx <= rx + rw
                                && sy >= ry
                                && sy <= ry + rh
                            {
                                coord.start_recording_iphone(cx);
                            } else {
                                coord.set_selection_mode(SelectionMode::Area, cx);
                                coord.selection_drag = Some(([sx, sy], [sx, sy]));
                                coord.sync_picker_overlay();
                                cx.notify();
                            }
                        }
                        SelectionMode::Window => {
                            if coord.windows_loading {
                                return;
                            }
                            if let Some(target) = selectable_window_at(
                                &coord.selectable_windows,
                                f64::from(sx),
                                f64::from(sy),
                            )
                            .cloned()
                            {
                                coord.start_recording_with_target(target, cx);
                            } else {
                                coord.start_recording_full_screen(cx);
                            }
                        }
                        SelectionMode::Screen => coord.start_recording_full_screen(cx),
                    }
                });
            },
        )
        .on_mouse_move(move |event: &MouseMoveEvent, _window, cx| {
            let [sx, sy] = current_mouse_screen()
                .unwrap_or([f32::from(event.position.x), f32::from(event.position.y)]);
            let dragging = event.pressed_button == Some(MouseButton::Left);
            c_move.update(cx, |coord, cx| {
                coord.selection_pointer = Some([sx, sy]);
                let cur_mode = match coord.island_phase {
                    IslandPhase::Selecting { mode } => mode,
                    _ => return,
                };
                if coord.hold_picker
                    && dragging
                    && let Some((start, _)) = coord.selection_drag
                {
                    let moved = (sx - start[0]).hypot(sy - start[1]) >= 6.0;
                    if cur_mode != SelectionMode::Area && moved {
                        let action = coord.switcher.drag();
                        coord.apply_switcher(action, cx);
                    }
                    if coord.selection_mode() == Some(SelectionMode::Area) {
                        coord.selection_drag = Some((start, [sx, sy]));
                        coord.sync_picker_overlay();
                        cx.notify();
                    }
                    return;
                }
                match cur_mode {
                    SelectionMode::Area => {
                        if let Some((start, _)) = coord.selection_drag {
                            coord.selection_drag = Some((start, [sx, sy]));
                            coord.sync_picker_overlay();
                            cx.notify();
                        }
                    }
                    SelectionMode::Iphone => {
                        coord.sync_picker_overlay();
                    }
                    SelectionMode::Screen => {}
                    SelectionMode::Window => {
                        let found = coord
                            .selectable_windows
                            .iter()
                            .find(|w| {
                                let f = w.frame;
                                f64::from(sx) >= f.x
                                    && f64::from(sx) <= f.x + f.width
                                    && f64::from(sy) >= f.y
                                    && f64::from(sy) <= f.y + f.height
                            })
                            .cloned();
                        if coord.hovered_window.as_ref().and_then(|w| w.window_id)
                            != found.as_ref().and_then(|w| w.window_id)
                        {
                            coord.hovered_window = found;
                            coord.sync_picker_overlay();
                            cx.notify();
                        }
                    }
                }
            });
        })
        .on_mouse_up(MouseButton::Left, {
            let c_up = c_up.clone();
            move |event: &MouseUpEvent, _window, cx| {
                c_up.update(cx, |coord, cx| coord.picker_mouse_up(event, cx));
            }
        })
        // A drag released past the picker's display (on another one) ends there too, at the
        // display's edge: the window that saw the press gets the release, outside its bounds.
        .on_mouse_up_out(
            MouseButton::Left,
            move |event: &MouseUpEvent, _window, cx| {
                c_up.update(cx, |coord, cx| coord.picker_mouse_up(event, cx));
            },
        )
    }
}

const BADGE_H: f32 = 22.0;
const BADGE_PX: f32 = 8.0;
const BADGE_TEXT: f32 = 12.0;
const BADGE_GAP: f32 = 8.0;
const BADGE_MARGIN: f32 = 8.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BadgeAnchor {
    /// Just outside the bottom-right corner (Area drags), flipping above or inside near edges.
    BelowTrailing,
    /// Centered over the target (Window hover).
    Center,
}

/// The smallest area the picker records, points a side (`screens::MIN_AREA` is the floor
/// after trimming).
const MIN_AREA_SIDE: f32 = 36.0;

/// A point dragged past the picker's display (global points), pulled back onto its edge: an
/// area belongs to the display the drag started on, which the picker stays on while dragging.
fn clamp_point([x, y]: [f32; 2], [dx, dy, dw, dh]: [f32; 4]) -> [f32; 2] {
    [x.clamp(dx, dx + dw), y.clamp(dy, dy + dh)]
}

/// A dragged area in global points, kept on the picker's `display` (so the cutout, the size
/// badge and the recording agree), if it's big enough to record.
fn area_rect(a: [f32; 2], b: [f32; 2], display: [f32; 4]) -> Option<[f64; 4]> {
    let [a, b] = [a, b].map(|p| clamp_point(p, display));
    let w = (a[0] - b[0]).abs();
    let h = (a[1] - b[1]).abs();
    (w >= MIN_AREA_SIDE && h >= MIN_AREA_SIDE)
        .then(|| [a[0].min(b[0]), a[1].min(b[1]), w, h].map(f64::from))
}

fn clip_rect([x, y, w, h]: [f32; 4], [sw, sh]: [f32; 2]) -> [f32; 4] {
    let x0 = x.clamp(0.0, sw);
    let y0 = y.clamp(0.0, sh);
    let x1 = (x + w).clamp(0.0, sw);
    let y1 = (y + h).clamp(0.0, sh);
    [x0, y0, x1 - x0, y1 - y0]
}

/// Positions a badge of `size` relative to `rect`, keeping it fully on screen.
fn place_badge(
    [x, y, w, h]: [f32; 4],
    [bw, bh]: [f32; 2],
    [sw, sh]: [f32; 2],
    anchor: BadgeAnchor,
) -> [f32; 2] {
    let (bx, by) = match anchor {
        BadgeAnchor::BelowTrailing => {
            let below = y + h + BADGE_GAP;
            let above = y - BADGE_GAP - bh;
            let by = if below + bh <= sh - BADGE_MARGIN {
                below
            } else if above >= BADGE_MARGIN {
                above
            } else {
                y + h - BADGE_GAP - bh
            };
            (x + w - bw, by)
        }
        BadgeAnchor::Center => (x + (w - bw) / 2.0, y + (h - bh) / 2.0),
    };
    let max_x = (sw - BADGE_MARGIN - bw).max(BADGE_MARGIN);
    let max_y = (sh - BADGE_MARGIN - bh).max(BADGE_MARGIN);
    [
        bx.clamp(BADGE_MARGIN, max_x).round(),
        by.clamp(BADGE_MARGIN, max_y).round(),
    ]
}

pub struct BundleReviewWindow {
    coordinator: Entity<ScrawlCoordinator>,
    focus: FocusHandle,
    cached_sheet_path: Option<PathBuf>,
    cached_sheet_image: Option<Arc<Image>>,
    cached_frames: Vec<Option<Arc<Image>>>,
}

impl BundleReviewWindow {
    fn new(coordinator: Entity<ScrawlCoordinator>, focus: FocusHandle) -> Self {
        Self {
            coordinator,
            focus,
            cached_sheet_path: None,
            cached_sheet_image: None,
            cached_frames: Vec::new(),
        }
    }

    fn ensure_images(&mut self, dir: &std::path::Path, bundle: &ScrawlBundle) {
        let sheet_path = dir.join(&bundle.contact_sheet);
        if self.cached_sheet_path.as_ref() != Some(&sheet_path) {
            self.cached_sheet_path = Some(sheet_path.clone());
            self.cached_sheet_image = std::fs::read(&sheet_path)
                .ok()
                .map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)));
            self.cached_frames = bundle
                .keyframes
                .iter()
                .map(|kf| {
                    let bytes = std::fs::read(dir.join(&kf.file)).ok()?;
                    Some(Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
                })
                .collect();
        }
    }
}

impl Render for BundleReviewWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let stage_w = (f32::from(window.bounds().size.width) - 320.0 - 44.0).max(320.0);
        let coord_read = self.coordinator.read(cx);
        let state = coord_read.review_bundle.clone();
        let status = coord_read.review_status.clone();
        let tab = coord_read.selected_tab;
        let selected_kf = coord_read.selected_keyframe;
        let shortcut_label = coord_read.shortcut_label();
        let recording_shortcut = coord_read.recording_shortcut_key;

        let Some((dir, bundle)) = state else {
            let c_start = self.coordinator.clone();
            let c_demo = self.coordinator.clone();
            let c_keydown = self.coordinator.clone();
            return div()
                .id("scrawl-review-empty")
                .track_focus(&self.focus)
                .on_key_down(move |event: &KeyDownEvent, window, cx| {
                    let handled = c_keydown
                        .update(cx, |coord, cx| coord.handle_app_key(&event.keystroke, cx));
                    let key = event.keystroke.key.as_str();
                    if !handled
                        && (key == "escape"
                            || (event.keystroke.modifiers.platform
                                && key.eq_ignore_ascii_case("w")))
                    {
                        window.remove_window();
                    } else if !handled {
                        return;
                    }
                    cx.stop_propagation();
                })
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .bg(rgb(CANVAS_BG))
                .text_color(rgb(TEXT_COLOR))
                .child(
                    div()
                        .text_size(px(16.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Scrawl"),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            hint_button("empty-record", "Record", Some(shortcut_label), true)
                                .on_click(move |_, _, cx| {
                                    c_start.update(cx, |coord, cx| coord.record_or_finish(cx));
                                }),
                        )
                        .child(
                            action_button("empty-demo", "Load Demo Bundle", false).on_click(
                                move |_, _, cx| {
                                    if let Ok((dir, bundle, _)) = crate::generate_demo_bundle(None)
                                    {
                                        c_demo.update(cx, |coord, cx| {
                                            coord.set_review_bundle(
                                                (dir, bundle),
                                                Some("Generated demo bundle".into()),
                                            );
                                            cx.notify();
                                        });
                                    }
                                },
                            ),
                        ),
                )
                .into_any_element();
        };

        self.ensure_images(&dir, &bundle);

        let target_label = match (&bundle.target.app, &bundle.target.window_title) {
            (Some(app), Some(win)) if !win.is_empty() && win != app => {
                format!("{app} — “{win}”")
            }
            (Some(app), _) => app.clone(),
            _ => format!("Display {}", bundle.target.display_id),
        };
        let timeline = timeline(&bundle);

        let c_keydown = self.coordinator.clone();
        let c_shortcut = self.coordinator.clone();
        let c_copy = self.coordinator.clone();
        let c_send = self.coordinator.clone();
        let c_tab_sheet = self.coordinator.clone();
        let c_tab_frames = self.coordinator.clone();
        let copy_dir = dir.clone();
        let send_dir = dir.clone();

        div()
            .id("scrawl-review-root")
            .track_focus(&self.focus)
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                let mods = event.keystroke.modifiers;
                let handled = c_keydown.update(cx, |coord, cx| {
                    if coord.recording_shortcut_key {
                        if key == "escape" {
                            coord.recording_shortcut_key = false;
                            cx.notify();
                            return true;
                        }
                        if let Some(cfg) = shortcut_from_key_and_mods(
                            key,
                            mods.platform,
                            mods.control,
                            mods.alt,
                            mods.shift,
                        ) {
                            update_global_shortcut(&coord.telemetry, cfg);
                            coord.recording_shortcut_key = false;
                            cx.notify();
                            return true;
                        }
                    }
                    if coord.handle_app_key(&event.keystroke, cx) {
                        return true;
                    }
                    if let Some(cfg) = shortcut_from_key_and_mods(
                        key,
                        mods.platform,
                        mods.control,
                        mods.alt,
                        mods.shift,
                    ) && coord.telemetry.matches_shortcut(
                        cfg.key_code as u16,
                        mods.platform,
                        mods.control,
                        mods.alt,
                        mods.shift,
                    ) {
                        coord.telemetry.emit_capture_shortcut();
                        return true;
                    }
                    false
                });
                if !handled && (key == "escape" || (mods.platform && key.eq_ignore_ascii_case("w")))
                {
                    window.remove_window();
                } else if !handled {
                    return;
                }
                cx.stop_propagation();
            })
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(CANVAS_BG))
            .text_color(rgb(TEXT_COLOR))
            .child(
                div()
                    .h(px(48.0))
                    .pl(px(78.0))
                    .pr_4()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .bg(rgb(SIDEBAR_BG))
                    .border_b_1()
                    .border_color(rgb(LINE_COLOR))
                    .on_mouse_down(MouseButton::Left, |_, window, _| {
                        window.start_window_move();
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                // Segmented control: R_outer 8 = R_tab 6 + border 1 + pad 1.
                                div()
                                    .flex_none()
                                    .h(px(28.0))
                                    .p(px(1.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .rounded(px(8.0))
                                    .bg(rgb(CANVAS_BG))
                                    .border_1()
                                    .border_color(rgb(LINE_COLOR))
                                    .child(
                                        tab_chip(
                                            "tab-contact-sheet",
                                            "Contact Sheet",
                                            tab == ReviewTab::ContactSheet,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                c_tab_sheet.update(cx, |coord, cx| {
                                                    coord.selected_tab = ReviewTab::ContactSheet;
                                                    cx.notify();
                                                });
                                            },
                                        ),
                                    )
                                    .child(
                                        tab_chip(
                                            "tab-keyframes",
                                            "Keyframes",
                                            tab == ReviewTab::Keyframes,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                c_tab_frames.update(cx, |coord, cx| {
                                                    coord.selected_tab = ReviewTab::Keyframes;
                                                    cx.notify();
                                                });
                                            },
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .text_color(rgb(MUTED_COLOR))
                                    .child(format!(
                                        "{} · {} · {} {}",
                                        target_label,
                                        format_clock(bundle.duration_ms),
                                        bundle.keyframes.len(),
                                        if bundle.keyframes.len() == 1 {
                                            "keyframe"
                                        } else {
                                            "keyframes"
                                        }
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                hint_button(
                                    "btn-shortcut",
                                    if recording_shortcut {
                                        "Press shortcut…"
                                    } else {
                                        "Shortcut"
                                    },
                                    (!recording_shortcut).then_some(shortcut_label),
                                    false,
                                )
                                .on_click(move |_, _, cx| {
                                    c_shortcut.update(cx, |coord, cx| {
                                        coord.recording_shortcut_key =
                                            !coord.recording_shortcut_key;
                                        cx.notify();
                                    });
                                }),
                            )
                            .child(
                                action_button(
                                    "btn-reveal",
                                    crate::platform::SHOW_IN_FILE_MANAGER,
                                    false,
                                )
                                .on_click(
                                    move |_, _, cx| {
                                        crate::platform::open_in_file_manager(&send_dir);
                                        c_send.update(cx, |coord, cx| {
                                            coord.review_status =
                                                Some(crate::platform::OPENED_IN_FILE_MANAGER.into());
                                            cx.notify();
                                        });
                                    },
                                ),
                            )
                            .child(
                                action_button(
                                    "btn-copy",
                                    if status.as_deref() == Some("Copied ✓") {
                                        "Copied"
                                    } else {
                                        "Copy"
                                    },
                                    true,
                                )
                                .on_click(move |_, _, cx| {
                                    let res = crate::handoff::copy_bundle_to_clipboard(&copy_dir);
                                    c_copy.update(cx, |coord, cx| {
                                        if res.is_ok() {
                                            coord.mark_handled(&copy_dir);
                                        }
                                        coord.review_status = Some(match res {
                                            Ok(()) => "Copied ✓".into(),
                                            Err(e) => format!("Copy failed: {e}"),
                                        });
                                        cx.notify();
                                    });
                                }),
                            ),
                    ),
            )
            .child(
                // Main Workspace: Stage on Left + Timeline Sidebar on Right
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .id("review-stage-scroll")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .overflow_x_hidden()
                            .p_5()
                            .child(match tab {
                                ReviewTab::ContactSheet => self.render_contact_sheet_stage(stage_w),
                                ReviewTab::Keyframes => {
                                    self.render_keyframes_stage(&bundle, selected_kf, stage_w)
                                }
                            }),
                    )
                    .child(
                        div()
                            .id("review-timeline-sidebar")
                            .w(px(320.0))
                            .h_full()
                            .flex_none()
                            .overflow_y_scroll()
                            .overflow_x_hidden()
                            .bg(rgb(SIDEBAR_BG))
                            .border_l_1()
                            .border_color(rgb(LINE_COLOR))
                            .px_4()
                            .py(px(18.0))
                            .flex()
                            .flex_col()
                            .child(sidebar_heading("Transcript"))
                            .child(
                                div()
                                    .p_3()
                                    .rounded(px(8.0))
                                    .bg(rgb(SURFACE_BG))
                                    .border_1()
                                    .border_color(rgb(LINE_COLOR))
                                    .text_size(px(13.0))
                                    .line_height(relative(1.45))
                                    .map(|d| {
                                        let text = bundle.transcript.text.trim();
                                        if text.is_empty() {
                                            d.text_color(rgb(MUTED_COLOR)).child("No speech")
                                        } else {
                                            d.text_color(rgb(TEXT_COLOR)).child(format!("“{text}”"))
                                        }
                                    }),
                            )
                            .child(div().mt(px(20.0)).child(sidebar_heading("Timeline")))
                            .child(
                                div().mx(px(-8.0)).flex().flex_col().gap(px(1.0)).children(
                                    timeline
                                        .into_iter()
                                        .enumerate()
                                        .map(|(i, line)| timeline_row(i, &line)),
                                ),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl BundleReviewWindow {
    fn render_contact_sheet_stage(&self, stage_w: f32) -> AnyElement {
        if let Some(ref image) = self.cached_sheet_image {
            div()
                .w_full()
                .min_w_0()
                .flex()
                .justify_center()
                .child(
                    img(Arc::clone(image))
                        .w(px(stage_w))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(rgb(LINE_COLOR)),
                )
                .into_any_element()
        } else {
            div()
                .p_6()
                .text_color(rgb(MUTED_COLOR))
                .child("Contact sheet image unavailable.")
                .into_any_element()
        }
    }

    fn render_keyframes_stage(
        &self,
        bundle: &ScrawlBundle,
        selected_kf: usize,
        stage_w: f32,
    ) -> AnyElement {
        // Card: border 1 + pad 8 around the frame, so R_card 12 = R_frame 3 + 9.
        let card_img_w = (stage_w - 18.0).max(240.0);
        let cards = bundle
            .keyframes
            .iter()
            .enumerate()
            .map(|(idx, kf)| {
                let is_selected = idx == selected_kf;
                let frame_img = self.cached_frames.get(idx).and_then(Clone::clone);
                let c_select = self.coordinator.clone();
                div()
                    .id(("kf-card", idx))
                    .p(px(8.0))
                    .rounded(px(12.0))
                    .bg(rgb(SURFACE_BG))
                    .border_1()
                    .border_color(if is_selected {
                        rgb(ACCENT_COLOR)
                    } else {
                        rgb(LINE_COLOR)
                    })
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .when(!is_selected, |d| d.hover(|s| s.border_color(rgb(0x3a3a3a))))
                    .on_click(move |_, _, cx| {
                        c_select.update(cx, |coord, cx| {
                            coord.selected_keyframe = idx;
                            cx.notify();
                        });
                    })
                    .when_some(frame_img, |d, image| {
                        d.child(img(image).w(px(card_img_w)).rounded(px(3.0)))
                    })
                    .child(
                        div()
                            .px(px(4.0))
                            .pb(px(2.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(12.5))
                            .child(
                                div()
                                    .h(px(20.0))
                                    .px(px(7.0))
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .rounded(px(10.0))
                                    .bg(rgb(CANVAS_BG))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(11.5))
                                    .text_color(rgb(TEXT_COLOR))
                                    .child(format!("#{} · {}", kf.index, format_clock(kf.t))),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(rgb(TEXT_SOFT))
                                    .child(kf.event.clone()),
                            ),
                    )
                    .when_some(kf.said.clone(), |d, said| {
                        d.child(
                            div()
                                .px(px(4.0))
                                .pb(px(2.0))
                                .text_size(px(12.0))
                                .line_height(relative(1.4))
                                .text_color(rgb(MUTED_COLOR))
                                .child(format!("“{said}”")),
                        )
                    })
            })
            .collect::<Vec<_>>();

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .children(cards)
            .into_any_element()
    }
}

fn sidebar_heading(label: &'static str) -> Div {
    div()
        .mb(px(8.0))
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED_COLOR))
        .child(label)
}

/// Splits a `sketch::timeline` line ("0:03 · Clicked…", "0:04 ✎ Circle…", "0:05 “said”") into
/// a time column and a body, flagging annotation marks.
fn split_timeline_line(line: &str) -> (&str, &str, bool) {
    let (time, rest) = line.split_once(' ').unwrap_or(("", line));
    if let Some(body) = rest.strip_prefix("✎ ") {
        (time, body, true)
    } else {
        (time, rest.strip_prefix("· ").unwrap_or(rest), false)
    }
}

fn timeline_row(index: usize, line: &str) -> gpui::Stateful<Div> {
    let (time, body, is_mark) = split_timeline_line(line);
    div()
        .id(("timeline-row", index))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .flex()
        .items_start()
        .gap(px(10.0))
        .text_size(px(12.5))
        .line_height(relative(1.4))
        .hover(|s| s.bg(rgb(SURFACE_BG)))
        .child(
            div()
                .w(px(32.0))
                .flex_none()
                .text_color(rgb(MUTED_COLOR))
                .child(time.to_string()),
        )
        .when(is_mark, |d| {
            d.child(div().flex_none().text_color(rgb(INK_RED)).child("✎"))
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(if is_mark {
                    rgb(TEXT_COLOR)
                } else {
                    rgb(TEXT_SOFT)
                })
                .child(body.to_string()),
        )
}

fn tab_chip(id: &'static str, label: &'static str, selected: bool) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .h(px(24.0))
        .px(px(10.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .when(selected, |d| {
            d.bg(rgb(SURFACE_SELECTED)).shadow(vec![BoxShadow {
                color: gpui::hsla(0.0, 0.0, 0.0, 0.25),
                offset: point(px(0.0), px(1.0)),
                blur_radius: px(2.0),
                spread_radius: px(0.0),
            }])
        })
        .text_color(if selected {
            rgb(TEXT_COLOR)
        } else {
            rgb(MUTED_COLOR)
        })
        .when(!selected, |d| {
            d.cursor_pointer().hover(|s| s.text_color(rgb(TEXT_SOFT)))
        })
        .child(label)
}

fn button_base(id: &'static str, primary: bool) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .h(px(28.0))
        .px(px(12.0))
        .rounded(px(7.0))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.0))
        .whitespace_nowrap()
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .border_1()
        .border_color(if primary {
            rgb(ACCENT_COLOR)
        } else {
            rgb(LINE_COLOR)
        })
        .bg(if primary {
            rgb(ACCENT_COLOR)
        } else {
            rgb(SURFACE_BG)
        })
        .text_color(rgb(TEXT_COLOR))
        .cursor_pointer()
        .hover(move |s| {
            if primary {
                s.bg(rgb(ACCENT_HOVER)).border_color(rgb(ACCENT_HOVER))
            } else {
                s.bg(rgb(SURFACE_HOVER))
            }
        })
}

fn action_button(id: &'static str, label: &str, primary: bool) -> gpui::Stateful<Div> {
    button_base(id, primary).child(SharedString::from(label.to_string()))
}

/// Button with a trailing muted hint, e.g. a keyboard shortcut.
fn hint_button(
    id: &'static str,
    label: &str,
    hint: Option<String>,
    primary: bool,
) -> gpui::Stateful<Div> {
    button_base(id, primary)
        .child(SharedString::from(label.to_string()))
        .when_some(hint, |d, hint| {
            d.child(
                div()
                    .text_color(if primary {
                        rgba(0xffffffb3)
                    } else {
                        rgb(MUTED_COLOR)
                    })
                    .child(hint),
            )
        })
}

/// The GPUI display that is the screen at `frame` (global points), else the primary one.
fn display_at(frame: [f32; 4], cx: &App) -> Option<std::rc::Rc<dyn gpui::PlatformDisplay>> {
    let screens = crate::screens::displays();
    let id = crate::screens::index_for_rect(&screens, frame.map(f64::from)).map(|i| screens[i].id);
    id.and_then(|id| cx.displays().into_iter().find(|d| u32::from(d.id()) == id))
        .or_else(|| cx.primary_display())
}

fn open_picker_windows(coord_entity: &Entity<ScrawlCoordinator>, cx: &mut App) {
    close_picker_window(cx);
    let screen = coord_entity.read(cx).display_rect;
    let display = display_at(screen, cx);
    let display_bounds = Bounds {
        origin: Point::default(),
        size: size(px(screen[2]), px(screen[3])),
    };
    let display_id = display.as_ref().map(|d| d.id());
    let lab = coord_entity.read(cx).lab;

    let picker_coord = coord_entity.clone();
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: display_bounds.size,
            })),
            display_id,
            titlebar: None,
            focus: !lab,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            app_id: Some(PICKER_APP_ID.into()),
            ..Default::default()
        },
        move |window, cx| {
            configure_picker_window(&picker_coord, lab, cx);
            let focus = cx.focus_handle();
            if !lab {
                window.focus(&focus);
            }
            cx.new(|cx| {
                cx.observe(&picker_coord, |_, _, cx| cx.notify()).detach();
                TargetPickerWindow {
                    coordinator: picker_coord,
                    focus,
                }
            })
        },
    );
    // A menu bar (accessory) app isn't active when its menu or capsule is used, and keys only
    // reach the picker while it is. The held switcher takes its keys from the key tap instead,
    // so it leaves the app in front alone.
    if !lab && !coord_entity.read(cx).switcher.is_held() {
        remember_front_app();
        cx.activate(true);
    }

    ensure_hud_pill_open(coord_entity, cx);
}

fn ensure_hud_pill_open(coord_entity: &Entity<ScrawlCoordinator>, cx: &mut App) {
    if crate::platform::native_pill() {
        let _ = coord_entity.update(cx, |coord, _| {
            coord.ensure_native_pill();
            coord.sync_native_pill();
        });
        return;
    }
    for handle in cx.windows() {
        if let Some(pill) = handle.downcast::<HudPillWindow>() {
            let _ = pill.update(cx, |view, _, cx| {
                if let Some(ref ns_win) = view.ns_window {
                    bring_pill_forward(ns_win);
                }
                cx.notify();
            });
            return;
        }
    }
    // The display in use: the picker's or recording's, else the pointer's.
    let coord = coord_entity.read(cx);
    let in_use = match (&coord.active_recording, coord.island_phase) {
        (Some(rec), _) if rec.target.mode != CaptureMode::Device => {
            let f = rec.target.frame;
            Some([f.x, f.y, f.width, f.height].map(|v| v as f32))
        }
        (_, IslandPhase::Selecting { .. }) => Some(coord.display_rect),
        _ => None,
    };
    let near =
        in_use.or_else(|| crate::screens::pointer().map(|[x, y]| [x as f32, y as f32, 1.0, 1.0]));
    let display = near.map_or_else(|| cx.primary_display(), |r| display_at(r, cx));
    let display_bounds = display.as_ref().map_or_else(
        || Bounds {
            origin: Point::default(),
            size: size(px(1440.0), px(900.0)),
        },
        |d| d.bounds(),
    );
    let display_id = display.as_ref().map(|d| d.id());

    // Transparent Dynamic Island host positioned cleanly below the 37pt macOS menu bar
    let pill_size: Size<Pixels> = size(px(PILL_WINDOW_W), px(PILL_WINDOW_H));
    let pill_bounds = Bounds {
        origin: point(
            (display_bounds.size.width - pill_size.width) * 0.5,
            px(PILL_WINDOW_TOP),
        ),
        size: pill_size,
    };
    let pill_coord = coord_entity.clone();
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(pill_bounds)),
            display_id,
            titlebar: None,
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: true,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            app_id: Some(PILL_APP_ID.into()),
            ..Default::default()
        },
        move |window, cx| {
            let ns_window = find_hud_pill_ns_window();
            let mut shell_layer = None;
            if let Some(ref ns_win) = ns_window {
                configure_pill_window(ns_win, &mut shell_layer, &pill_coord, cx);
            }
            let focus = cx.focus_handle();
            window.focus(&focus);
            cx.new(|cx| {
                cx.observe(&pill_coord, |_, _, cx| cx.notify()).detach();
                HudPillWindow {
                    coordinator: pill_coord,
                    focus,
                    ns_window,
                    shell_layer,
                    frames: 0,
                }
            })
        },
    );
}

fn close_picker_window(cx: &mut App) {
    for handle in cx.windows() {
        if handle.downcast::<TargetPickerWindow>().is_some() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    }
}

fn close_capture_windows(cx: &mut App) {
    for handle in cx.windows() {
        if handle.downcast::<HudPillWindow>().is_some()
            || handle.downcast::<TargetPickerWindow>().is_some()
        {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    }
}

fn close_review_window(cx: &mut App) {
    for handle in cx.windows() {
        if handle.downcast::<BundleReviewWindow>().is_some() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    }
}

/// Opens the review window in front with the keys. A menu bar (accessory) app isn't active when
/// its menu is used, and the cooperative `NSApp.activate()` may leave the window behind the app
/// in front, so it activates ignoring other apps once the window exists.
fn open_review_window(coord_entity: &Entity<ScrawlCoordinator>, cx: &mut App) {
    for handle in cx.windows() {
        if let Some(review) = handle.downcast::<BundleReviewWindow>() {
            remember_front_app();
            cx.activate(true);
            let _ = review.update(cx, |_, window, cx| {
                window.activate_window();
                cx.notify();
            });
            return;
        }
    }
    let bounds = Bounds::centered(None, size(px(REVIEW_WINDOW_W), px(REVIEW_WINDOW_H)), cx);
    let coord_clone = coord_entity.clone();
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: None,
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.0), px(16.0))),
            }),
            focus: true,
            show: true,
            kind: WindowKind::Normal,
            is_resizable: true,
            window_background: WindowBackgroundAppearance::Opaque,
            app_id: Some(REVIEW_APP_ID.into()),
            ..Default::default()
        },
        move |window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus);
            cx.new(|cx| {
                cx.observe(&coord_clone, |_, _, cx| cx.notify()).detach();
                BundleReviewWindow::new(coord_clone, focus)
            })
        },
    );
    remember_front_app();
    cx.activate(true);
}

pub(crate) fn start_metric(name: &str, started: Instant) {
    if std::env::var_os("SCRAWL_BENCH_START").is_some() {
        eprintln!(
            "START_METRIC {name}={:.6}",
            started.elapsed().as_secs_f64() * 1_000.0
        );
    }
}

impl ScrawlCoordinator {
    /// An event from the input monitor, the phone, the helper windows or the ingest server.
    /// Returns whether the capsule's window should open (a capture arrived from the iPhone).
    fn handle_event(&mut self, ev: LiveOverlayEvent, cx: &mut Context<Self>) -> bool {
        match ev {
            LiveOverlayEvent::CaptureShortcut => self.press_shortcut(cx),
            #[cfg(target_os = "macos")]
            LiveOverlayEvent::PasteLatestShortcut => {
                if !self.is_recording() && !self.is_selecting() {
                    self.paste_latest(cx);
                }
            }
            #[cfg(target_os = "macos")]
            LiveOverlayEvent::UndoStroke => {
                self.undo_stroke(cx);
            }
            #[cfg(target_os = "macos")]
            LiveOverlayEvent::CancelCapture => {
                self.cancel_recording(cx);
            }
            #[cfg(target_os = "macos")]
            LiveOverlayEvent::RequestSnapshot => {
                self.request_immediate_frame();
                cx.notify();
            }
            #[cfg(target_os = "macos")]
            LiveOverlayEvent::PhoneEnded => self.phone_ended(cx),
            #[cfg(target_os = "macos")]
            LiveOverlayEvent::Onboarding(event) => self.onboarding_event(event, cx),
            #[cfg(target_os = "macos")]
            LiveOverlayEvent::Settings(event) => self.settings_event(event, cx),
            #[cfg(target_os = "macos")]
            LiveOverlayEvent::IngestedBundle {
                dir,
                bundle,
                status,
            } => {
                let frame_count = bundle.keyframes.len();
                let duration_ms = bundle.duration_ms;
                let inbox_count = self.add_to_inbox(&dir, &bundle);
                self.completed_dir = Some(dir.clone());
                self.set_review_bundle((dir, *bundle), status);
                self.set_island_phase(IslandPhase::Completed {
                    finished_at: Instant::now(),
                    frame_count,
                    duration_ms,
                    inbox_count,
                });
                cx.notify();
                return true;
            }
        }
        false
    }
}

/// Real startup paths, without registering hotkeys or publishing personal captures.
#[cfg(target_os = "macos")]
pub fn run_start_benchmark() {
    let app = Application::new().with_assets(crate::icons::Assets);
    app.run(|cx: &mut App| {
        let quick_stop = std::env::var("SCRAWL_BENCH_QUICK_STOP").as_deref() == Ok("1");
        let (sender, _receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(sender);
        let coordinator = cx.new(|_| ScrawlCoordinator::new(telemetry, None));
        cx.spawn(async move |cx| {
            for run in 0..8 {
                coordinator.update(cx, |coord, cx| coord.begin_target_selection(cx)).unwrap();
                // Allow deferred picker creation and async discovery to settle.
                cx.background_executor().timer(Duration::from_millis(600)).await;
                coordinator.update(cx, |coord, cx| {
                    let stage = Instant::now();
                    assert!(!coord.windows_loading, "Picker discovery did not finish");
                    let target = region_target(100.0, 100.0, 480.0, 320.0, &coord.selectable_windows);
                    start_metric("target_discovery_ms", stage);
                    coord.start_recording_with_target(target, cx);
                }).unwrap();
                if !quick_stop {
                    cx.background_executor().timer(Duration::from_millis(1_500)).await;
                }
                coordinator.update(cx, |coord, cx| {
                    let rec = coord.active_recording.take().expect("Recording did not start");
                    assert!(rec._metal_overlay.is_some(), "Metal overlay failed");
                    coord.telemetry.finish_session();
                    let CaptureSource::Screen { session, audio, .. } = rec.source else {
                        unreachable!("The benchmark records the screen")
                    };
                    let (recording, audio) = std::thread::scope(|scope| {
                        let screen = scope.spawn(|| session.stop());
                        let audio = audio.expect("Microphone failed to start").stop().expect("Microphone failed to stop");
                        (screen.join().unwrap(), audio)
                    });
                    if !quick_stop {
                        assert!(recording.video.is_some(), "MP4 missing");
                        assert!(audio.sample_count > 0, "Microphone returned no samples");
                        // Candidates are small (480 px long edge); the MP4 keeps full size.
                        assert!(recording.frames.iter().any(|f| f.width == 480 && f.height == 320), "No real SCK frames (synthetic fallback only)");
                    }
                    let wav = hound::WavReader::new(std::io::Cursor::new(&audio.wav_bytes)).unwrap();
                    assert_eq!(wav.duration() as usize, audio.sample_count);
                    std::fs::write(rec.bundle_dir.join("audio.wav"), &audio.wav_bytes).unwrap();
                    println!("START_RUN {}", serde_json::json!({"run": run, "frames": recording.frames.len(), "first_frame_ms": recording.frames[0].t_ms, "audio_samples": audio.sample_count, "duration_ms": recording.duration_ms, "video_offset_ms": recording.video_offset_ms}));
                    coord.set_island_phase(IslandPhase::Hidden);
                    close_capture_windows(cx);
                }).unwrap();
            }
            cx.update(|cx| cx.quit()).unwrap();
        }).detach();
    });
}

/// Runs the GPUI desktop app, optionally opening an initial `.scrawl` bundle in the Review window
/// or starting recording immediately.
#[cfg(target_os = "macos")]
pub fn run_app(
    initial_bundle: Option<(PathBuf, ScrawlBundle)>,
    start_recording_immediately: bool,
    start_iphone_immediately: bool,
    screenshot_out: Option<PathBuf>,
) {
    let app = Application::new().with_assets(crate::icons::Assets);
    app.run(move |cx: &mut App| {
        scrawl_log(format!("run_app started pid={}", std::process::id()));
        stay_out_of_dock();
        // ⌘W, Esc, the close button or Scrawl itself: every way a window closes ends up here.
        cx.on_window_closed(|cx| cx.defer(hand_back_keys)).detach();
        let (ui_sender, ui_receiver) = flume::unbounded();
        crate::phone::start_ingest_server(ui_sender.clone());
        let telemetry = SharedCaptureTelemetry::new(ui_sender);
        install_global_hotkeys(&telemetry);
        start_global_input_tap(Arc::clone(&telemetry));
        crate::sparkle::start();
        crate::retention::start();
        crate::hex::watch();
        let menu_rx = install_status_bar();

        // Onboarding opens on first launch, or for a permission that went away.
        if screenshot_out.is_none() && std::env::var_os("SCRAWL_NO_ONBOARDING").is_none() {
            let sender = telemetry.ui_sender.clone();
            std::thread::spawn(move || {
                if let Some(grants) = crate::onboarding::read_grants() {
                    let event = crate::onboarding::Event::Launch(grants);
                    let _ = sender.send(LiveOverlayEvent::Onboarding(event));
                }
            });
        }
        let has_initial = initial_bundle.is_some();
        let coordinator = cx.new(|_| ScrawlCoordinator::new(telemetry, initial_bundle));
        set_status_inbox_count(coordinator.read(cx).inbox.len());

        if has_initial {
            open_review_window(&coordinator, cx);
        }
        if start_recording_immediately {
            let coord_clone = coordinator.clone();
            coord_clone.update(cx, |coord, cx| coord.record_or_finish(cx));
        }
        if start_iphone_immediately {
            coordinator.update(cx, |coord, cx| coord.start_recording_iphone(cx));
        }

        if let Some(out_path) = screenshot_out {
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(650))
                    .await;
                let window_num = cx
                    .update(|_| largest_window_to_front())
                    .ok()
                    .flatten();
                let _ = cx
                    .background_executor()
                    .spawn(async move { capture_own_window_screenshot(window_num, &out_path) })
                    .await;
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        }

        let loop_coord = coordinator.clone();
        cx.spawn(async move |cx| {
            let mut shown_status: Option<String> = None;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(8))
                    .await;
                let mut should_quit = false;
                let _ = cx.update(|cx| {
                    let status = EXPORT_STATUS.lock().ok().and_then(|s| s.clone());
                    if status != shown_status {
                        match &status {
                            Some(text) => set_status_text(text),
                            None => set_status_inbox_count(loop_coord.read(cx).inbox.len()),
                        }
                        shown_status = status;
                    }
                    if let Some(ref rx) = menu_rx {
                        while let Ok(action) = rx.try_recv() {
                            should_quit |= act_on_menu(action, &loop_coord, cx);
                        }
                    }

                    let mut should_open_pill = false;
                    while let Ok(ev) = ui_receiver.try_recv() {
                        should_open_pill |= loop_coord.update(cx, |coord, cx| coord.handle_event(ev, cx));
                    }
                    if should_open_pill {
                        ensure_hud_pill_open(&loop_coord, cx);
                    }

                    motion::poll_tune();
                    if loop_coord.read(cx).switcher.is_held() {
                        loop_coord.update(cx, |coord, cx| coord.tick_switcher(cx));
                    } else if loop_coord.read(cx).telemetry.take_escape()
                        && loop_coord.read(cx).is_selecting()
                    {
                        // The picker may not have the keys (another app is active), but the
                        // key tap sees Esc anyway.
                        loop_coord.update(cx, |coord, cx| coord.cancel_recording(cx));
                    }
                    if loop_coord.read(cx).gate.is_pending() {
                        loop_coord.update(cx, |coord, _| coord.tick_gate());
                    }
                    if loop_coord.read(cx).is_island_visible() {
                        record_external_frontmost_app();
                        loop_coord.update(cx, |coord, cx| coord.island_tick(cx));
                    }
                });
                if should_quit {
                    let _ = cx.update(|cx| cx.quit());
                    break;
                }
            }
        })
        .detach();
    });
}

/// Does what a status menu item (or a control socket command) asks; `true` for Quit.
fn act_on_menu(action: MenuAction, coord_entity: &Entity<ScrawlCoordinator>, cx: &mut App) -> bool {
    match action {
        MenuAction::ToggleCapture => {
            coord_entity.update(cx, |coord, cx| coord.record_or_finish(cx));
        }
        #[cfg(target_os = "macos")]
        MenuAction::ChooseTarget => {
            coord_entity.update(cx, |coord, cx| {
                if !coord.is_recording()
                    && !coord.is_processing()
                    && !coord.is_selecting()
                {
                    coord.begin_target_selection(cx);
                }
            });
        }
        MenuAction::OpenLatest => {
            if let Ok((dir, bundle)) =
                crate::handoff::load_bundle(std::path::Path::new("latest"))
            {
                coord_entity.update(cx, |coord, cx| {
                    coord.mark_handled(&dir);
                    coord.set_review_bundle((dir, bundle), None);
                    cx.notify();
                });
                open_review_window(coord_entity, cx);
            }
        }
        #[cfg(target_os = "macos")]
        MenuAction::OpenInbox(path) => {
            let loaded = crate::handoff::load_bundle(&path).ok();
            let opened = loaded.is_some();
            coord_entity.update(cx, |coord, cx| {
                coord.mark_handled(&path);
                if let Some(bundle) = loaded {
                    coord.set_review_bundle(bundle, None);
                }
                cx.notify();
            });
            if opened {
                open_review_window(coord_entity, cx);
            }
        }
        #[cfg(target_os = "macos")]
        MenuAction::ExportVideo => start_video_export(),
        #[cfg(target_os = "macos")]
        MenuAction::Settings => {
            coord_entity.update(cx, |coord, _| coord.open_settings(None));
        }
        #[cfg(target_os = "macos")]
        MenuAction::CheckForUpdates => {
            crate::sparkle::check_for_updates();
        }
        #[cfg(target_os = "linux")]
        MenuAction::Stop => {
            coord_entity.update(cx, |coord, cx| {
                if coord.is_recording() {
                    coord.finish_recording(cx);
                }
            });
        }
        #[cfg(target_os = "linux")]
        MenuAction::Cancel => {
            coord_entity.update(cx, |coord, cx| {
                if coord.is_recording() || coord.is_selecting() {
                    coord.cancel_recording(cx);
                }
            });
        }
        MenuAction::Quit => return true,
    }
    false
}

/// `scrawl app` on Linux, beyond the windows: the control socket (`scrawl toggle` and the
/// other commands), the tray, and the shortcut listener. Commands and tray clicks arrive as
/// `MenuAction`s, except `toggle`, which is a press of the capture shortcut.
#[cfg(target_os = "linux")]
fn start_linux_app(
    listener: std::os::unix::net::UnixListener,
    telemetry: &Arc<SharedCaptureTelemetry>,
) -> (
    flume::Receiver<MenuAction>,
    Option<crate::platform::tray::Tray>,
) {
    use crate::linux::control::Command;
    use crate::platform::tray::{Tray, TrayItem};
    let (menu_tx, menu_rx) = flume::unbounded();
    start_global_input_tap(Arc::clone(telemetry));
    let socket_telemetry = Arc::clone(telemetry);
    let socket_tx = menu_tx.clone();
    crate::linux::control::serve(listener, move |command| {
        let action = match command {
            Command::Toggle => {
                socket_telemetry.press_shortcut_from(crate::platform::PressSource::Socket);
                return Ok(());
            }
            Command::Stop => MenuAction::Stop,
            Command::Cancel => MenuAction::Cancel,
            Command::Review => MenuAction::OpenLatest,
            Command::Quit => MenuAction::Quit,
        };
        socket_tx
            .send(action)
            .map_err(|_| "Scrawl is quitting".to_owned())
    });
    let shortcut = telemetry
        .shortcut
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .label
        .clone();
    let tray = Tray::start(shortcut, move |item| {
        let _ = menu_tx.send(match item {
            TrayItem::Record => MenuAction::ToggleCapture,
            TrayItem::OpenLatest => MenuAction::OpenLatest,
            TrayItem::Quit => MenuAction::Quit,
        });
    });
    (menu_rx, tray)
}

/// An empty 1×1 window that is always open while `scrawl app` runs on Linux: gpui's Wayland
/// client ends the app when its last window closes, and the capsule closes between captures
/// (and during one). Hyprland keeps it on a hidden special workspace (`hypr::keep_alive_rule`).
#[cfg(target_os = "linux")]
struct KeepAliveWindow;

#[cfg(target_os = "linux")]
impl Render for KeepAliveWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

#[cfg(target_os = "linux")]
fn open_keep_alive_window(cx: &mut App) {
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1.0), px(1.0)),
            })),
            titlebar: None,
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            app_id: Some(crate::linux::hypr::KEEP_ALIVE_CLASS.into()),
            ..Default::default()
        },
        |_, cx| cx.new(|_| KeepAliveWindow),
    );
    if let Err(error) = opened {
        scrawl_log(format!("Keep-alive window: {error:#}; Scrawl quits with its last window"));
    }
}

/// How long quitting waits for a cancelled recording's `wf-recorder` to stop.
#[cfg(target_os = "linux")]
const QUIT_WAIT: Duration = Duration::from_secs(6);

/// How long the capsule shows after a Linux recording starts: `wf-recorder` records the whole
/// output, so the capsule then leaves the screen (finishing is by the shortcut) and comes back
/// for Processing and Completed.
#[cfg(any(target_os = "linux", test))]
const LINUX_PILL_RECORDING_SHOWN_MS: u64 = 1_500;

/// What the capsule's window should do on Linux, given the island's phase, how long the
/// recording has run, and whether the window is open.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PillWindowStep {
    Keep,
    Close,
    Open,
}

#[cfg(any(target_os = "linux", test))]
fn pill_window_step(phase: IslandPhase, recording_ms: Option<u64>, open: bool) -> PillWindowStep {
    match phase {
        IslandPhase::Recording
            if open && recording_ms.is_some_and(|ms| ms >= LINUX_PILL_RECORDING_SHOWN_MS) =>
        {
            PillWindowStep::Close
        }
        IslandPhase::Processing | IslandPhase::Completed { .. } if !open => PillWindowStep::Open,
        _ => PillWindowStep::Keep,
    }
}

/// Closes the capsule's window once a Linux screen recording has run
/// `LINUX_PILL_RECORDING_SHOWN_MS`, so it stays out of the capture, and opens it again for the
/// result. A lab recording captures nothing, so its capsule stays.
#[cfg(target_os = "linux")]
fn step_linux_pill_window(coord_entity: &Entity<ScrawlCoordinator>, cx: &mut App) {
    let coord = coord_entity.read(cx);
    let recording_ms = coord.active_recording.as_ref().and_then(|recording| {
        matches!(recording.source, CaptureSource::Screen { .. })
            .then(|| u64::try_from(recording.started.elapsed().as_millis()).unwrap_or(u64::MAX))
    });
    let pill = cx
        .windows()
        .into_iter()
        .find(|handle| handle.downcast::<HudPillWindow>().is_some());
    match pill_window_step(coord.island_phase, recording_ms, pill.is_some()) {
        PillWindowStep::Keep => {}
        PillWindowStep::Close => {
            if let Some(handle) = pill {
                scrawl_log("Capsule hidden for the recording".to_string());
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
        }
        PillWindowStep::Open => ensure_hud_pill_open(coord_entity, cx),
    }
}

/// Runs the GPUI app on Linux: the capsule and the review window, driven by the capture
/// shortcut, with Hyprland rules placing the windows. The tray and the control socket that
/// routes `scrawl toggle` here come with task 4.
#[cfg(target_os = "linux")]
pub fn run_app(
    initial_bundle: Option<(PathBuf, ScrawlBundle)>,
    start_recording_immediately: bool,
    start_iphone_immediately: bool,
    screenshot_out: Option<PathBuf>,
) {
    // `scrawl app` itself, not a preview or a screenshot: one per user.
    let control = if initial_bundle.is_none() && screenshot_out.is_none() {
        match crate::linux::control::bind() {
            Ok((listener, path)) => Some((listener, path)),
            Err(error) => {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };
    let app = Application::new().with_assets(crate::icons::Assets);
    app.run(move |cx: &mut App| {
        scrawl_log(format!("run_app started pid={}", std::process::id()));
        crate::linux::hypr::install_window_rules(
            [PILL_WINDOW_W, PILL_WINDOW_H, PILL_WINDOW_TOP],
            [REVIEW_WINDOW_W, REVIEW_WINDOW_H],
        );
        cx.on_window_closed(|cx| cx.defer(hand_back_keys)).detach();
        let (ui_sender, ui_receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(ui_sender);
        install_global_hotkeys(&telemetry);
        let (socket_path, menu_rx, mut tray) = match control {
            Some((listener, path)) => {
                open_keep_alive_window(cx);
                let (menu_rx, tray) = start_linux_app(listener, &telemetry);
                (Some(path), Some(menu_rx), tray)
            }
            None => (None, None, None),
        };
        let has_initial = initial_bundle.is_some();
        let coordinator = cx.new(|_| ScrawlCoordinator::new(telemetry, initial_bundle));
        set_status_inbox_count(coordinator.read(cx).inbox.len());

        if has_initial {
            open_review_window(&coordinator, cx);
        }
        if start_recording_immediately {
            coordinator.update(cx, |coord, cx| coord.record_or_finish(cx));
        }
        if start_iphone_immediately {
            coordinator.update(cx, |coord, cx| coord.start_recording_iphone(cx));
        }
        if let Some(out_path) = screenshot_out {
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(650))
                    .await;
                let shot = cx
                    .background_executor()
                    .spawn(async move { capture_own_window_screenshot(&out_path) })
                    .await;
                if let Err(error) = shot {
                    scrawl_log(format!("Screenshot failed: {error:#}"));
                }
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        }

        let loop_coord = coordinator.clone();
        cx.spawn(async move |cx| {
            let mut quitting: Option<Instant> = None;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(8))
                    .await;
                let _ = cx.update(|cx| {
                    if let Some(rx) = &menu_rx {
                        while let Ok(action) = rx.try_recv() {
                            if act_on_menu(action, &loop_coord, cx) && quitting.is_none() {
                                // A recording is thrown away and its recorder stopped first,
                                // or `wf-recorder` would outlive the app.
                                loop_coord.update(cx, |coord, cx| {
                                    if coord.is_recording() {
                                        coord.cancel_recording(cx);
                                    }
                                });
                                quitting = Some(Instant::now());
                            }
                        }
                    }
                    if let Some(tray) = tray.as_mut() {
                        let coord = loop_coord.read(cx);
                        tray.set_recording(coord.is_recording());
                        tray.set_shortcut(
                            &coord
                                .telemetry
                                .shortcut
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .label,
                        );
                    }
                    let mut should_open_pill = false;
                    while let Ok(ev) = ui_receiver.try_recv() {
                        should_open_pill |= loop_coord.update(cx, |coord, cx| coord.handle_event(ev, cx));
                    }
                    if should_open_pill {
                        ensure_hud_pill_open(&loop_coord, cx);
                    }
                    step_linux_pill_window(&loop_coord, cx);
                    motion::poll_tune();
                    if loop_coord.read(cx).switcher.is_held() {
                        loop_coord.update(cx, |coord, cx| coord.tick_switcher(cx));
                    } else if loop_coord.read(cx).telemetry.take_escape()
                        && loop_coord.read(cx).is_selecting()
                    {
                        loop_coord.update(cx, |coord, cx| coord.cancel_recording(cx));
                    }
                    if loop_coord.read(cx).is_island_visible() {
                        record_external_frontmost_app();
                        loop_coord.update(cx, |coord, cx| coord.island_tick(cx));
                    }
                });
                if quitting.is_some_and(|since| {
                    !crate::platform::recorders_running() || since.elapsed() >= QUIT_WAIT
                }) {
                    if let Some(path) = &socket_path {
                        let _ = std::fs::remove_file(path);
                    }
                    let _ = cx.update(|cx| cx.quit());
                    break;
                }
            }
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn app_ids_are_the_classes_the_hyprland_rules_match() {
        use crate::linux::hypr;
        assert_eq!(PILL_APP_ID, hypr::PILL_CLASS);
        assert_eq!(PICKER_APP_ID, hypr::PICKER_CLASS);
        assert_eq!(REVIEW_APP_ID, hypr::REVIEW_CLASS);
    }

    #[test]
    fn the_linux_capsule_leaves_the_recording_and_returns_for_the_result() {
        use PillWindowStep::{Close, Keep, Open};
        let rec = IslandPhase::Recording;
        // Shown while the recording starts, then off screen.
        assert_eq!(pill_window_step(rec, Some(400), true), Keep);
        assert_eq!(pill_window_step(rec, Some(1_500), true), Close);
        assert_eq!(pill_window_step(rec, Some(9_000), false), Keep);
        // Back for the result.
        assert_eq!(pill_window_step(IslandPhase::Processing, None, false), Open);
        let done = IslandPhase::Completed {
            finished_at: Instant::now(),
            frame_count: 3,
            duration_ms: 4_000,
            inbox_count: 0,
        };
        assert_eq!(pill_window_step(done, None, false), Open);
        assert_eq!(pill_window_step(done, None, true), Keep);
        // Picking opens its own window; dismissing or hidden never reopens one.
        let picking = IslandPhase::Selecting {
            mode: SelectionMode::Area,
        };
        assert_eq!(pill_window_step(picking, None, false), Keep);
        assert_eq!(pill_window_step(IslandPhase::Dismissing, None, false), Keep);
        assert_eq!(pill_window_step(IslandPhase::Hidden, None, false), Keep);
    }

    #[test]
    fn island_geometry_is_concentric() {
        assert_eq!(ISLAND_RADIUS, CHIP_RADIUS + ISLAND_INSET);
        assert_eq!(ISLAND_PAD + 1.0, ISLAND_INSET);
        assert_eq!(CHIP_H + 2.0 * ISLAND_INSET, ISLAND_H);
        assert!(ISLAND_H <= PILL_WINDOW_H);
    }

    #[test]
    fn a_held_drag_needs_room_for_an_area() {
        let laptop = [0.0, 0.0, 1728.0, 1117.0];
        assert_eq!(
            area_rect([100.0, 80.0], [40.0, 20.0], laptop),
            Some([40.0, 20.0, 60.0, 60.0])
        );
        assert_eq!(area_rect([0.0, 0.0], [30.0, 200.0], laptop), None);
    }

    /// Kit's review: a drag that starts at x -150 on a left display and overshoots to x 160 on
    /// the laptop keeps to the left display, so it's recorded there (`screens::area_on_display`).
    #[test]
    fn an_area_drag_stays_on_the_display_it_started_on() {
        let left = [-1920.0, 37.0, 1920.0, 1080.0];
        assert_eq!(
            area_rect([-150.0, 200.0], [160.0, 400.0], left),
            Some([-150.0, 200.0, 150.0, 200.0])
        );
        // Past its bottom-left corner too.
        assert_eq!(
            area_rect([-1800.0, 1000.0], [-2100.0, 1300.0], left),
            Some([-1920.0, 1000.0, 120.0, 117.0])
        );
        // What's left on the display can be too small to record.
        assert_eq!(area_rect([-20.0, 200.0], [400.0, 400.0], left), None);
        let screens = [
            crate::screens::Screen {
                id: 1,
                frame: [0.0, 0.0, 1728.0, 1117.0],
                visible: [0.0, 0.0, 1728.0, 1117.0],
                scale: 2.0,
                safe_top: 0.0,
            },
            crate::screens::Screen {
                id: 2,
                frame: left.map(f64::from),
                visible: left.map(f64::from),
                scale: 1.0,
                safe_top: 0.0,
            },
        ];
        let area = area_rect([-150.0, 200.0], [160.0, 400.0], left).unwrap();
        let (i, rect) = crate::screens::area_on_display(&screens, area).unwrap();
        assert_eq!((screens[i].id, rect), (2, area));
    }

    #[test]
    fn selection_modes_cycle_in_hud_order() {
        use SelectionMode::{Area, Iphone, Screen, Window};
        let walk = |has_iphone: bool, forward: bool| {
            let mut mode = Area;
            let mut seen = Vec::new();
            for _ in 0..5 {
                mode = mode.cycled(forward, has_iphone);
                seen.push(mode);
            }
            seen
        };
        assert_eq!(walk(true, true), [Window, Screen, Iphone, Area, Window]);
        assert_eq!(walk(true, false), [Iphone, Screen, Window, Area, Iphone]);
        assert_eq!(walk(false, true), [Window, Screen, Area, Window, Screen]);
        assert_eq!(walk(false, false), [Screen, Window, Area, Screen, Window]);
        // An unplugged phone leaves the picker on a mode that is no longer listed.
        assert_eq!(Iphone.cycled(true, false), Area);
        assert_eq!(Iphone.cycled(false, false), Screen);
    }

    #[test]
    fn digits_pick_modes_by_hud_position() {
        use SelectionMode::{Area, Iphone, Screen, Window};
        let pick = |key, has_iphone| SelectionMode::from_digit(key, has_iphone);
        assert_eq!(pick("1", true), Some(Area));
        assert_eq!(pick("2", true), Some(Window));
        assert_eq!(pick("3", false), Some(Screen));
        assert_eq!(pick("4", true), Some(Iphone));
        assert_eq!(pick("4", false), None);
        assert_eq!(pick("0", true), None);
        assert_eq!(pick("5", true), None);
        assert_eq!(pick("a", true), None);
    }

    #[test]
    fn selecting_row_width_mirrors_layout() {
        let segment = |label: f32| label + 2.0 * SEG_PX + SEG_ICON + SEG_ICON_GAP;
        let divider = 1.0 + 2.0 * DIVIDER_MX;
        // Three segments, divider, ✕: four gaps between five items.
        assert_eq!(
            selecting_row_width(&[30.0, 50.0, 45.0], None),
            segment(30.0) + segment(50.0) + segment(45.0) + divider + CHIP_H + 4.0 * CHIP_GAP
        );
        // A fourth segment and the Record chip each add their width and one gap.
        assert_eq!(
            selecting_row_width(&[30.0, 50.0, 45.0, 42.0], Some(70.0)),
            selecting_row_width(&[30.0, 50.0, 45.0], None) + segment(42.0) + 70.0 + 2.0 * CHIP_GAP
        );
        // The widest row (generous label estimates) still fits the pill window.
        let widest = selecting_row_width(&[34.0, 56.0, 50.0, 46.0], Some(2.0 * CHIP_PX + 52.0));
        assert!(
            widest + 2.0 * ISLAND_INSET <= PILL_WINDOW_W - 16.0,
            "{widest}"
        );
    }

    #[test]
    fn island_spring_settles_without_large_overshoot() {
        let (mut x, mut v) = (200.0_f32, 0.0_f32);
        let mut peak = x;
        for _ in 0..90 {
            (x, v) = spring_step(x, v, 300.0, 1.0 / 60.0, motion::Tune::DEFAULT.island);
            peak = peak.max(x);
        }
        assert!((x - 300.0).abs() < 0.5, "settled at {x}");
        assert!(peak < 300.0 + 100.0 * 0.05, "overshoot peak {peak}");
    }

    #[test]
    fn reveal_springs_lead_the_morph_palette_and_settle() {
        let t = motion::Tune::DEFAULT;
        // Width leads morph .42/.16 open and .38/.08 close on a slightly faster spring.
        assert!(t.reveal_open.response < 0.42);
        assert!(t.reveal_close.response < 0.38);
        assert!(t.reveal_close.response < t.reveal_open.response);
        assert!((t.reveal_open.bounce - 0.16).abs() < 1e-6);
        assert!(t.reveal_exit_s < t.reveal_enter_s && t.reveal_exit_s >= 0.75 * t.reveal_enter_s);
        assert!(t.reveal_blur_px <= 4.0);
        for (spring, from, to) in [
            (t.reveal_open, ISLAND_OPEN_FROM_W, 430.0),
            (t.reveal_close, 430.0, 330.0),
        ] {
            let (mut x, mut v) = (from, 0.0_f32);
            for _ in 0..60 {
                (x, v) = spring_step(x, v, to, 1.0 / 60.0, spring);
            }
            assert!((x - to).abs() < 0.5, "settled at {x}");
        }
    }

    #[test]
    fn trailing_items_fade_in_as_the_edge_uncovers_them() {
        let offsets = trail_offsets(100.0, &[50.0, 60.0]);
        assert_eq!(offsets, vec![100.0, 100.0 + 50.0 + CHIP_GAP]);
        assert_eq!(edge_alpha(100.0, 100.0, 50.0), 0.0);
        assert!((edge_alpha(125.0, 100.0, 50.0) - 0.5).abs() < 1e-6);
        assert_eq!(edge_alpha(150.0, 100.0, 50.0), 1.0);
        assert_eq!(edge_alpha(400.0, 100.0, 50.0), 1.0);
    }

    #[test]
    fn a_switch_chip_morph_starts_where_the_pill_was_and_rests_centred() {
        let (width, target, clock_w) = (336.0, 298.0, 35.0);
        let start = SwitchLayout::new(SwitchMorph { slide: None }, width, target, clock_w);
        // The lead is still pinned to the left edge and the chips start behind its divider.
        assert!(start.ghost_left().abs() < 1e-3, "{}", start.ghost_left());
        let chips_x = CHIP_H + clock_w + LEAD_TRAIL + 2.0 * CHIP_GAP + 1.0 + 2.0 * DIVIDER_MX;
        assert!((start.row_left() - chips_x).abs() < 1e-3);
        assert_eq!(start.ghost_alphas(), [1.0, 1.0, 1.0]);
        // At rest the picker row is centred in the capsule, the lead past its edge and gone.
        let rest = SwitchLayout::new(
            SwitchMorph {
                slide: Some((0.0, 0.0)),
            },
            target,
            target,
            clock_w,
        );
        assert!(rest.row_left().abs() < 1e-3);
        assert_eq!(rest.ghost_alphas(), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn island_spring_is_stable_for_long_frames() {
        let (x, _) = spring_step(200.0, 0.0, 300.0, 0.05, motion::Tune::DEFAULT.island);
        assert!(x > 200.0 && x < 300.0);
    }

    #[test]
    fn area_badge_sits_below_and_flips_near_edges() {
        let screen = [1440.0, 900.0];
        let badge = [90.0, BADGE_H];
        let [bx, by] = place_badge(
            [100.0, 100.0, 300.0, 200.0],
            badge,
            screen,
            BadgeAnchor::BelowTrailing,
        );
        assert_eq!([bx, by], [310.0, 308.0]);

        let [_, by] = place_badge(
            [100.0, 600.0, 300.0, 290.0],
            badge,
            screen,
            BadgeAnchor::BelowTrailing,
        );
        assert_eq!(by, 600.0 - BADGE_GAP - BADGE_H);

        let [bx, by] = place_badge(
            [0.0, 0.0, 40.0, 900.0],
            badge,
            screen,
            BadgeAnchor::BelowTrailing,
        );
        assert_eq!(bx, BADGE_MARGIN);
        assert!(by + BADGE_H <= 900.0 - BADGE_MARGIN);
    }

    #[test]
    fn window_badge_centers_and_clamps_on_screen() {
        let screen = [1440.0, 900.0];
        let [bx, by] = place_badge(
            [200.0, 100.0, 600.0, 400.0],
            [200.0, BADGE_H],
            screen,
            BadgeAnchor::Center,
        );
        assert_eq!([bx, by], [400.0, 289.0]);

        let clipped = clip_rect([-300.0, -50.0, 400.0, 200.0], screen);
        assert_eq!(clipped, [0.0, 0.0, 100.0, 150.0]);
    }

    #[test]
    fn timeline_lines_split_into_time_and_body() {
        assert_eq!(
            split_timeline_line("0:04 ✎ Circle around button — frame #2"),
            ("0:04", "Circle around button — frame #2", true)
        );
        assert_eq!(
            split_timeline_line("0:05 · Clicked Save"),
            ("0:05", "Clicked Save", false)
        );
        assert_eq!(
            split_timeline_line("0:01 “hello”"),
            ("0:01", "“hello”", false)
        );
    }

    #[test]
    fn clock_template_is_width_stable() {
        assert_eq!(clock_template("12:34"), "00:00");
        #[cfg(target_os = "macos")]
        assert_eq!(completed_meta(8_000, 1), "· 1 frame · ⌥⇧V");
        assert_eq!(completed_label(0, None), "Copied");
        assert_eq!(completed_label(0, Some("Sent to SCRAWL")), "Sent to SCRAWL");
        assert_eq!(completed_label(1, Some("Sent to SCRAWL")), "From iPhone");
        assert_eq!(completed_label(3, None), "3 from iPhone");
        assert_eq!(island_title("SCRAWL"), "SCRAWL");
        assert_eq!(
            island_title("Make Mac captures route to the site session"),
            "Make Mac captures route…"
        );
        let queued = crate::delivery::Delivery::Queued {
            title: "SCRAWL".into(),
            error: "offline".into(),
        };
        assert_eq!(
            completed_delivery_note(&queued, "SCRAWL"),
            "Copied · couldn't reach SCRAWL"
        );
        let sent = crate::delivery::Delivery::Sent {
            title: "SCRAWL".into(),
        };
        assert_eq!(completed_delivery_note(&sent, "SCRAWL"), "Sent to SCRAWL");
    }
}
