//! Hyprland through its IPC socket: what to record, which windows are on screen and where the
//! cursor is. Replies are the same JSON `hyprctl -j` prints, read without a process per call.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::capture::scrawl_log;
use crate::model::{CaptureMode, TargetFrame, TargetInfo};
use crate::screens::Screen;
use crate::windows::{ActivityLog, WindowEntry};

/// The capsule window's Wayland app id (`WindowOptions::app_id`), which Hyprland calls its class.
pub const PILL_CLASS: &str = "scrawl-pill";
/// The picker window's app id.
pub const PICKER_CLASS: &str = "scrawl-picker";
/// The review window's app id.
pub const REVIEW_CLASS: &str = "scrawl-review";
/// The Settings window's app id.
pub const SETTINGS_CLASS: &str = "scrawl-settings";
/// The app id of the window that keeps `scrawl app` running: gpui quits on Linux when its
/// last window closes, and the capsule closes between captures.
pub const KEEP_ALIVE_CLASS: &str = "scrawl-keep-alive";
/// The ink overlay's layer-shell namespace (`Overlay::new`'s `namespace`).
pub const INK_NAMESPACE: &str = "scrawl-ink";
/// Windows narrower or shorter than this (points) aren't offered by the picker.
const MIN_WINDOW: i32 = 80;

/// How often the cursor is read.
const TICK: Duration = Duration::from_millis(100);
/// How often the window list is read when the focused window hasn't changed.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Deserialize)]
pub struct WorkspaceRef {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    pub id: u32,
    pub name: String,
    /// Physical pixels.
    pub width: u32,
    pub height: u32,
    /// Logical (layout) coordinates.
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    #[serde(default)]
    pub transform: i32,
    pub focused: bool,
    pub active_workspace: WorkspaceRef,
    /// The special workspace (scratchpad) shown over the active one; id 0 when none is.
    #[serde(default)]
    pub special_workspace: Option<WorkspaceRef>,
    /// Space the bars keep, logical points: left, top, right, bottom.
    #[serde(default)]
    pub reserved: [f64; 4],
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub address: String,
    /// Logical layout coordinates.
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub class: String,
    pub title: String,
    pub workspace: WorkspaceRef,
    pub monitor: i64,
    pub mapped: bool,
    pub hidden: bool,
    pub floating: bool,
    /// Shown on every workspace of its monitor.
    #[serde(default)]
    pub pinned: bool,
    /// 0 is the window focused last.
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i32,
    #[serde(default)]
    pub pid: i32,
}

#[derive(Deserialize)]
struct CursorPos {
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
struct ActiveWindow {
    address: String,
}

/// One request on `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`, and its
/// reply.
fn hypr_request(request: &str) -> Result<Vec<u8>> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let instance =
        std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").context("Not running under Hyprland")?;
    let path = PathBuf::from(runtime).join("hypr").join(instance).join(".socket.sock");
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("Could not reach Hyprland at {}", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(request.as_bytes())?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    Ok(reply)
}

/// A query answered with the JSON `hyprctl -j` prints.
fn hypr_json<T: DeserializeOwned>(command: &str) -> Result<T> {
    let reply = hypr_request(&format!("j/{command}"))?;
    serde_json::from_slice(&reply).with_context(|| format!("Invalid Hyprland reply to {command}"))
}

/// A command (`eval …`, `keyword …`, `dispatch …`); `Err` with Hyprland's complaint unless it
/// answers `ok`.
fn hypr_command(command: &str) -> Result<()> {
    let reply = hypr_request(&format!("/{command}"))?;
    let reply = String::from_utf8_lossy(&reply);
    anyhow::ensure!(reply.trim() == "ok", "{}", reply.trim());
    Ok(())
}

/// Whether Hyprland's reply to an `eval` means it has no Lua `eval` at all (a Hyprland before
/// 0.53 answers "unknown request"; one that knows `eval` but parses a legacy config says so),
/// rather than that one Lua chunk failed (`error: …`).
fn eval_unsupported(reply: &str) -> bool {
    reply == "unknown request" || reply.contains("legacy parser")
}

/// Installs Scrawl's window rules for this Hyprland session (the user's config is never
/// edited): the capsule floats top-centre, the picker covers its monitor, the review and Settings
/// windows float centred. Lua (`eval`) first, `keyword windowrulev2` on Hyprland before 0.53.
pub fn install_window_rules(pill: [f32; 3], review: [f32; 2], settings: [f32; 2]) {
    let [width, height, top] = pill;
    let lua = [
        pill_rule(width, height, top),
        picker_rule(),
        review_rule(review[0], review[1]),
        settings_rule(settings[0], settings[1]),
        keep_alive_rule(),
        ink_layer_rule(),
    ];
    for rule in &lua {
        let Err(error) = hypr_command(&format!("eval {rule}")) else {
            continue;
        };
        let error = format!("{error:#}");
        if eval_unsupported(&error) {
            scrawl_log(format!("Hyprland has no Lua eval ({error}); trying windowrulev2"));
            return install_legacy_window_rules(pill, review, settings);
        }
        scrawl_log(format!("Hyprland rule (Lua) {rule:?}: {error}"));
    }
}

/// `install_window_rules` for a Hyprland without `eval`.
fn install_legacy_window_rules(pill: [f32; 3], review: [f32; 2], settings: [f32; 2]) {
    let [width, height, top] = pill;
    let legacy = legacy_pill_rules(width, height, top)
        .into_iter()
        .chain(legacy_window_rules(review, settings))
        .chain(legacy_keep_alive_rules());
    for rule in legacy {
        if let Err(error) = hypr_command(&format!("keyword windowrulev2 {rule}")) {
            scrawl_log(format!("Hyprland rule {rule:?}: {error:#}"));
        }
    }
    let layer = legacy_ink_layer_rule();
    if let Err(error) = hypr_command(&format!("keyword layerrule {layer}")) {
        scrawl_log(format!("Hyprland layer rule {layer:?}: {error:#}"));
    }
}

/// Gives the keyboard to the window at `address` (`0x…`).
pub fn focus_window(address: &str) {
    let lua = format!(
        "eval hl.dispatch(hl.dsp.focus({{ window = \"address:{address}\" }}))"
    );
    let Err(lua_error) = hypr_command(&lua) else {
        return;
    };
    scrawl_log(format!("Hyprland: Lua focus of {address}: {lua_error:#}; trying dispatch"));
    if let Err(error) = hypr_command(&format!("dispatch focuswindow address:{address}")) {
        scrawl_log(format!("Hyprland: could not focus {address}: {error:#}"));
    }
}

/// The commands that send the window of `class` to the top of the stack: Lua, and for a
/// Hyprland before 0.53 the dispatcher.
pub fn raise_commands(class: &str) -> [String; 2] {
    [
        format!(
            "eval hl.dispatch(hl.dsp.window.alter_zorder({{ mode = \"top\", window = \"class:^({class})$\" }}))"
        ),
        format!("dispatch alterzorder top,class:^({class})$"),
    ]
}

/// Sends the window of `class` to the top of the stack.
pub fn raise_window(class: &str) {
    let [lua, legacy] = raise_commands(class);
    let Err(lua_error) = hypr_command(&lua) else {
        return;
    };
    if let Err(error) = hypr_command(&legacy) {
        scrawl_log(format!(
            "Hyprland: could not raise {class}: {lua_error:#}; {error:#}"
        ));
    }
}

/// Whether the capsule has to be raised over the picker: both are mapped and the capsule was
/// mapped first (Hyprland lists windows in the order they mapped, which is their stacking
/// among floating windows of one kind).
pub fn pill_under_picker(clients: &[Client]) -> bool {
    let position = |class: &str| {
        clients
            .iter()
            .position(|c| c.class == class && c.mapped && !c.hidden)
    };
    matches!(
        (position(PILL_CLASS), position(PICKER_CLASS)),
        (Some(pill), Some(picker)) if pill < picker
    )
}

/// What `keep_pill_over_picker` does next.
#[derive(Debug, PartialEq, Eq)]
pub enum PillStep {
    /// No capsule, or it is already above the picker (a capsule that opens after the picker
    /// does).
    Done,
    /// The capsule is under the picker.
    Raise,
    /// A capsule is up and the picker isn't yet.
    Wait,
}

pub fn pill_step(clients: &[Client]) -> PillStep {
    let mapped = |class: &str| clients.iter().any(|c| c.class == class && c.mapped);
    if !mapped(PILL_CLASS) {
        PillStep::Done
    } else if pill_under_picker(clients) {
        PillStep::Raise
    } else if mapped(PICKER_CLASS) {
        PillStep::Done
    } else {
        PillStep::Wait
    }
}

/// The picker is opening: once it is mapped, a capsule that was already open (the Completed
/// one, say) goes back above the picker's dim. Returns at once when there is no capsule. Waits
/// for the picker, so run it off the UI thread.
pub fn keep_pill_over_picker() {
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(1_500) {
        match pill_step(&clients()) {
            PillStep::Done => return,
            PillStep::Raise => {
                raise_window(PILL_CLASS);
                return;
            }
            PillStep::Wait => std::thread::sleep(Duration::from_millis(15)),
        }
    }
    scrawl_log("Hyprland: the picker never mapped; the capsule was left where it was");
}

/// The focused window, if any.
pub fn active_window() -> Option<Client> {
    parse_active_window(&hypr_request("j/activewindow").ok()?)
}

/// Every window Hyprland knows (none when it can't be asked).
pub fn clients() -> Vec<Client> {
    hypr_json("clients").unwrap_or_default()
}

/// The windows the picker offers (`selectable_windows`), leaving out Scrawl's own.
pub fn list_selectable_windows() -> Vec<TargetInfo> {
    let (Ok(monitors), Ok(clients)) = (
        hypr_json::<Vec<Monitor>>("monitors"),
        hypr_json::<Vec<Client>>("clients"),
    ) else {
        return Vec::new();
    };
    selectable_windows(&monitors, &clients, std::process::id() as i32)
}

/// The app in front other than Scrawl (`front_app`).
pub fn frontmost_app_info() -> Option<(i32, String, Option<String>)> {
    front_app(
        active_window().as_ref(),
        &clients(),
        std::process::id() as i32,
    )
}

/// What to record: the newest window matching `window_query`, else the monitor under `point`
/// (`display_target_at`), named after the app in front. `focused_monitor_target` when there is
/// no point or no monitor list.
pub fn target_under_point(window_query: Option<&str>, point: Option<[f64; 2]>) -> TargetInfo {
    if window_query.is_some() {
        return focused_monitor_target(window_query);
    }
    let target = hypr_json::<Vec<Monitor>>("monitors")
        .ok()
        .and_then(|monitors| display_target_at(&monitors, point));
    let Some(mut target) = target else {
        return focused_monitor_target(None);
    };
    if let Some((_, class, _)) = frontmost_app_info() {
        target.app = Some(class);
    }
    target
}

/// Set once a cursor lookup failure has been logged, so a recording logs the first only.
static CURSOR_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);

/// Lets the next recording log its first cursor failure.
pub fn reset_cursor_error_log() {
    CURSOR_ERROR_LOGGED.store(false, Ordering::Release);
}

/// The cursor in global logical points. The first failure of a recording is logged.
pub fn cursor_position() -> Option<(f64, f64)> {
    match hypr_json::<CursorPos>("cursorpos") {
        Ok(c) => Some((c.x, c.y)),
        Err(error) => {
            if !CURSOR_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                scrawl_log(format!("Cursor position unavailable: {error:#}"));
            }
            None
        }
    }
}

#[derive(Deserialize)]
struct Keyboard {
    layout: String,
    #[serde(default)]
    main: bool,
}

#[derive(Deserialize)]
struct Devices {
    keyboards: Vec<Keyboard>,
}

/// The `layout` setting of the main keyboard, else the first.
fn main_keyboard_layout(devices: &Devices) -> Option<String> {
    devices
        .keyboards
        .iter()
        .find(|k| k.main)
        .or_else(|| devices.keyboards.first())
        .map(|k| k.layout.clone())
}

/// The keyboard layout Hyprland uses (`"us"`, `"br"`, ...), if it can say.
pub fn keyboard_layout() -> Option<String> {
    main_keyboard_layout(&hypr_json::<Devices>("devices").ok()?)
}

/// A monitor's area in logical points (rotated monitors swap sides).
pub fn monitor_frame(monitor: &Monitor) -> TargetFrame {
    let (width, height) = if monitor.transform % 2 == 1 {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    };
    TargetFrame {
        x: f64::from(monitor.x),
        y: f64::from(monitor.y),
        width: f64::from(width) / monitor.scale,
        height: f64::from(height) / monitor.scale,
        scale: monitor.scale,
    }
}

fn window_id(address: &str) -> u32 {
    u64::from_str_radix(address.trim_start_matches("0x"), 16).map_or(0, |a| a as u32)
}

/// The focused monitor, or the newest window whose title or class contains `window_query`.
pub fn pick_target(
    monitors: &[Monitor],
    clients: &[Client],
    window_query: Option<&str>,
) -> Result<TargetInfo> {
    let focused = monitors
        .iter()
        .find(|m| m.focused)
        .or(monitors.first())
        .context("Hyprland reports no monitors")?;
    let Some(query) = window_query else {
        return Ok(TargetInfo {
            mode: CaptureMode::Display,
            app: None,
            bundle_id: None,
            window_title: None,
            window_id: None,
            display_id: focused.id,
            frame: monitor_frame(focused),
        });
    };
    let needle = query.to_lowercase();
    let client = clients
        .iter()
        .filter(|c| c.mapped && !c.hidden)
        .filter(|c| c.title.to_lowercase().contains(&needle) || c.class.to_lowercase().contains(&needle))
        .min_by_key(|c| c.focus_history_id)
        .with_context(|| format!("No window matches {query:?}"))?;
    let monitor = monitors
        .iter()
        .find(|m| i64::from(m.id) == client.monitor)
        .unwrap_or(focused);
    Ok(TargetInfo {
        mode: CaptureMode::Window,
        app: Some(client.class.clone()),
        bundle_id: None,
        window_title: Some(client.title.clone()),
        window_id: Some(window_id(&client.address)),
        display_id: monitor.id,
        frame: TargetFrame {
            x: f64::from(client.at[0]),
            y: f64::from(client.at[1]),
            width: f64::from(client.size[0]),
            height: f64::from(client.size[1]),
            scale: monitor.scale,
        },
    })
}

/// What to record now without a pointer: the newest window matching `window_query`, else the
/// focused monitor (`pick_target`); the first monitor's guess when Hyprland can't be asked.
pub fn focused_monitor_target(window_query: Option<&str>) -> TargetInfo {
    let asked = hypr_json::<Vec<Monitor>>("monitors").and_then(|monitors| {
        let clients = hypr_json::<Vec<Client>>("clients")?;
        pick_target(&monitors, &clients, window_query)
    });
    asked.unwrap_or_else(|error| {
        scrawl_log(format!("Hyprland: {error:#}; recording the default output"));
        TargetInfo {
            mode: CaptureMode::Display,
            app: None,
            bundle_id: None,
            window_title: None,
            window_id: None,
            display_id: 0,
            frame: TargetFrame { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0, scale: 1.0 },
        }
    })
}

/// The output name `wf-recorder -o` wants for a monitor id.
pub fn monitor_name(id: u32) -> Option<String> {
    hypr_json::<Vec<Monitor>>("monitors")
        .ok()?
        .into_iter()
        .find(|m| m.id == id)
        .map(|m| m.name)
}

/// The windows on `monitor` front to back: those of the special workspace shown over it (a
/// scratchpad), then floating above tiled, then by focus. That is its active workspace's
/// windows, the special workspace's, and pinned ones, with bounds in points from the
/// monitor's top-left.
pub fn window_entries(clients: &[Client], monitor: &Monitor) -> Vec<WindowEntry> {
    let special = monitor.special_workspace.as_ref().map_or(0, |w| w.id);
    let on_special = |c: &Client| special != 0 && c.workspace.id == special;
    let mut visible: Vec<&Client> = clients
        .iter()
        .filter(|c| {
            c.mapped
                && !c.hidden
                && (c.workspace.id == monitor.active_workspace.id
                    || on_special(c)
                    || (c.pinned && c.monitor == i64::from(monitor.id)))
        })
        .collect();
    visible.sort_by_key(|c| (!on_special(c), !c.floating, c.focus_history_id));
    visible
        .into_iter()
        .map(|c| WindowEntry {
            id: window_id(&c.address),
            app: c.class.clone(),
            bundle_id: None,
            regular: true,
            title: c.title.clone(),
            bounds: [c.at[0] - monitor.x, c.at[1] - monitor.y, c.size[0], c.size[1]],
            layer: 0,
            frontmost: c.focus_history_id == 0,
        })
        .collect()
}

/// The area `frame` and `other` share, in points squared (0 when they only touch).
fn overlap(frame: TargetFrame, other: TargetFrame) -> f64 {
    let width = (frame.x + frame.width).min(other.x + other.width) - frame.x.max(other.x);
    let height = (frame.y + frame.height).min(other.y + other.height) - frame.y.max(other.y);
    if width > 0.0 && height > 0.0 { width * height } else { 0.0 }
}

/// Whether a capture of `frame` is on at least one of `monitors`.
fn on_a_monitor(monitors: &[Monitor], frame: TargetFrame) -> bool {
    monitors.iter().any(|m| overlap(monitor_frame(m), frame) > 0.0)
}

/// The windows on screen for a capture of `frame`, with their bounds from the top-left of
/// `frame` (they may be negative, as `WindowEntry::bounds` allows), the ones that don't reach it
/// left out. Always translated, by zero when the capture starts at the monitor's corner: a
/// display capture of the second monitor, or an area the size of a monitor but offset, is not
/// the first monitor's picture.
///
/// A capture can cross monitors, so the windows of every monitor are collected and merged (the
/// first monitor's first; a window found twice once). Not only those of the monitors the capture
/// overlaps: a floating window belongs to the monitor holding its centre but is drawn wherever it
/// reaches, so one of a neighbouring monitor can hang into the capture.
pub fn capture_entries(clients: &[Client], monitors: &[Monitor], frame: TargetFrame) -> Vec<WindowEntry> {
    let (width, height) = (frame.width.round() as i32, frame.height.round() as i32);
    let mut entries: Vec<WindowEntry> = Vec::new();
    for monitor in monitors {
        let dx = (f64::from(monitor.x) - frame.x).round() as i32;
        let dy = (f64::from(monitor.y) - frame.y).round() as i32;
        for mut entry in window_entries(clients, monitor) {
            entry.bounds[0] += dx;
            entry.bounds[1] += dy;
            let [x, y, w, h] = entry.bounds;
            if x < width && y < height && x + w > 0 && y + h > 0 && !entries.iter().any(|e| e.id == entry.id) {
                entries.push(entry);
            }
        }
    }
    entries
}

/// One look at the windows on screen for a capture of `frame` (`capture_entries`), `None` when
/// Hyprland can't be asked or the capture is on no monitor. Two socket requests.
pub fn sample_windows(frame: TargetFrame) -> Option<Vec<WindowEntry>> {
    let monitors = hypr_json::<Vec<Monitor>>("monitors").ok()?;
    let clients = hypr_json::<Vec<Client>>("clients").ok()?;
    on_a_monitor(&monitors, frame).then(|| capture_entries(&clients, &monitors, frame))
}

/// `sample_windows` for `frame`, as a closure a thread can own.
pub fn sample_windows_for(frame: TargetFrame) -> impl Fn() -> Option<Vec<WindowEntry>> + Send + Sync + 'static {
    move || sample_windows(frame)
}

/// Whether `frame` is a whole monitor (as `WindowLogger` is meant for), when that can be told.
fn is_display(frame: TargetFrame) -> bool {
    hypr_json::<Vec<Monitor>>("monitors").map_or(true, |monitors| {
        monitors.iter().any(|m| {
            let shown = monitor_frame(m);
            (shown.x - frame.x).abs() < 1.0
                && (shown.y - frame.y).abs() < 1.0
                && (shown.width - frame.width).abs() < 1.0
                && (shown.height - frame.height).abs() < 1.0
        })
    })
}

/// The longest stretch without a good look that still counts as watched: a missed look or two
/// (a slow socket, a busy compositor), not more. The same as the replay ring's.
const MAX_UNWATCHED_MS: u64 = 3_000;

/// Whether the looks at the windows covered a recording from its start to its end.
#[derive(Debug, Default)]
struct Coverage {
    last_ok: u64,
    gaps: bool,
}

impl Coverage {
    /// A good look at `t` (ms from the start).
    fn saw(&mut self, t: u64) {
        self.gaps |= t.saturating_sub(self.last_ok) > MAX_UNWATCHED_MS;
        self.last_ok = t;
    }

    /// Whether anything from the start to `end` went unwatched.
    fn has_gaps(&self, end: u64) -> bool {
        self.gaps || end.saturating_sub(self.last_ok) > MAX_UNWATCHED_MS
    }
}

/// Logs the cursor every tick and the window list every second or when focus moves, like the
/// macOS `windows::WindowLogger`. For a display capture `frame` is the monitor's
/// `monitor_frame`; for a window or an area (started only so `redact` can hide windows in it) it
/// is that rectangle: the windows of the monitor holding it are logged with bounds from its
/// corner (`capture_entries`), and the cursor is not (the input recorder has it).
pub struct WindowLogger {
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<ActivityLog>>,
}

impl WindowLogger {
    pub fn start(frame: TargetFrame, started_at: Instant, request: Arc<AtomicBool>) -> Self {
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let worker = thread::Builder::new()
            .name("scrawl-window-log".into())
            .spawn(move || run(frame, started_at, &request, &stop))
            .ok();
        Self { stopping, worker }
    }

    pub fn stop(mut self) -> ActivityLog {
        self.stopping.store(true, Ordering::Release);
        let log = self
            .worker
            .take()
            .and_then(|w| {
                w.thread().unpark();
                w.join().ok()
            })
            .unwrap_or_default();
        scrawl_log(format!(
            "Window log: {} snapshots taken, {} stored; {} cursor samples",
            log.snapshot_calls,
            log.windows.len(),
            log.cursor.len()
        ));
        log
    }
}

fn run(
    frame: TargetFrame,
    started_at: Instant,
    request: &AtomicBool,
    stopping: &AtomicBool,
) -> ActivityLog {
    let mut log = ActivityLog::default();
    let mut last_snapshot: Option<Instant> = None;
    let mut last_front: Option<String> = None;
    let with_cursor = is_display(frame);
    let mut coverage = Coverage::default();
    loop {
        let finishing = stopping.load(Ordering::Acquire);
        let t = started_at.elapsed().as_millis() as u64;
        if with_cursor && let Ok(cursor) = hypr_json::<CursorPos>("cursorpos") {
            log.push_cursor(t, cursor.x - frame.x, cursor.y - frame.y, frame);
        }
        let front = hypr_json::<ActiveWindow>("activewindow").ok().map(|w| w.address);
        let due = !finishing
            && (last_snapshot.is_none_or(|s| s.elapsed() >= SNAPSHOT_INTERVAL)
                || front != last_front
                || request.swap(false, Ordering::AcqRel));
        if due {
            let began = Instant::now();
            if let Some(entries) = sample_windows(frame) {
                coverage.saw(t);
                log.push_windows(t, entries);
            }
            log.snapshot_calls += 1;
            log.snapshot_time += began.elapsed();
            last_snapshot = Some(Instant::now());
            last_front = front;
        }
        if finishing {
            log.window_gaps = coverage.has_gaps(t);
            return log;
        }
        thread::park_timeout(TICK);
    }
}

/// Every monitor as a `screens::Screen` (none when Hyprland can't be asked).
pub fn screens() -> Vec<Screen> {
    hypr_json::<Vec<Monitor>>("monitors")
        .map(|monitors| screens_from(&monitors))
        .unwrap_or_default()
}

/// The monitors as `screens::Screen`s: `visible` is the frame less the bars' reserved space,
/// and the monitor at the origin comes first (the primary display on macOS).
pub fn screens_from(monitors: &[Monitor]) -> Vec<Screen> {
    let mut out: Vec<Screen> = monitors
        .iter()
        .map(|m| {
            let f = monitor_frame(m);
            let frame = [f.x, f.y, f.width, f.height];
            let [left, top, right, bottom] = m.reserved;
            Screen {
                id: m.id,
                frame,
                visible: [
                    frame[0] + left,
                    frame[1] + top,
                    (frame[2] - left - right).max(1.0),
                    (frame[3] - top - bottom).max(1.0),
                ],
                scale: m.scale,
                safe_top: 0.0,
            }
        })
        .collect();
    out.sort_by_key(|s| !(s.frame[0] == 0.0 && s.frame[1] == 0.0));
    out
}

/// The windows the picker offers, front to back: mapped windows on a monitor's active
/// workspace, at least `MIN_WINDOW` a side, not Scrawl's own (`own_pid`).
pub fn selectable_windows(monitors: &[Monitor], clients: &[Client], own_pid: i32) -> Vec<TargetInfo> {
    let mut visible: Vec<(&Client, &Monitor)> = clients
        .iter()
        .filter(|c| c.mapped && !c.hidden && c.pid != own_pid)
        .filter(|c| c.size[0] >= MIN_WINDOW && c.size[1] >= MIN_WINDOW)
        .filter_map(|c| {
            let monitor = monitors.iter().find(|m| i64::from(m.id) == c.monitor)?;
            (c.workspace.id == monitor.active_workspace.id).then_some((c, monitor))
        })
        .collect();
    visible.sort_by_key(|(c, _)| (!c.floating, c.focus_history_id));
    visible
        .into_iter()
        .map(|(c, monitor)| TargetInfo {
            mode: CaptureMode::Window,
            app: Some(c.class.clone()).filter(|s| !s.is_empty()),
            bundle_id: None,
            window_title: Some(c.title.clone()).filter(|s| !s.is_empty()),
            window_id: Some(window_id(&c.address)),
            display_id: monitor.id,
            frame: TargetFrame {
                x: f64::from(c.at[0]),
                y: f64::from(c.at[1]),
                width: f64::from(c.size[0]),
                height: f64::from(c.size[1]),
                scale: monitor.scale,
            },
        })
        .collect()
}

/// The monitor under `point` (else the nearest, else the focused one) as a display target.
pub fn display_target_at(monitors: &[Monitor], point: Option<[f64; 2]>) -> Option<TargetInfo> {
    let screens = screens_from(monitors);
    let monitor = match point.and_then(|p| crate::screens::index_at(&screens, p)) {
        Some(index) => monitors.iter().find(|m| m.id == screens[index].id)?,
        None => monitors.iter().find(|m| m.focused).or(monitors.first())?,
    };
    Some(TargetInfo {
        mode: CaptureMode::Display,
        app: None,
        bundle_id: None,
        window_title: None,
        window_id: None,
        display_id: monitor.id,
        frame: monitor_frame(monitor),
    })
}

/// `j/locked`'s reply (`{"locked": true}`): whether the session lock (hyprlock) is up.
pub fn parse_locked(reply: &[u8]) -> Option<bool> {
    #[derive(Deserialize)]
    struct Locked {
        locked: bool,
    }
    serde_json::from_slice::<Locked>(reply)
        .ok()
        .map(|reply| reply.locked)
}

/// Whether the screen is locked; `false` when Hyprland can't say.
pub fn session_locked() -> bool {
    hypr_request("j/locked")
        .ok()
        .and_then(|reply| parse_locked(&reply))
        .unwrap_or(false)
}

/// `activewindow`'s reply; `{}` when nothing has focus.
pub fn parse_active_window(reply: &[u8]) -> Option<Client> {
    serde_json::from_slice(reply).ok()
}

/// The app in front other than Scrawl: the focused window, else the one focused most recently.
/// `(pid, class, None)`: Wayland apps have no bundle id.
pub fn front_app(
    active: Option<&Client>,
    clients: &[Client],
    own_pid: i32,
) -> Option<(i32, String, Option<String>)> {
    active
        .filter(|c| c.pid != own_pid)
        .or_else(|| {
            clients
                .iter()
                .filter(|c| c.mapped && c.pid != own_pid)
                .min_by_key(|c| c.focus_history_id)
        })
        .map(|c| (c.pid, c.class.clone(), None))
}

/// An area dragged in the picker as a region target: on the display holding its centre, trimmed
/// to it and at least `MIN_AREA` a side (`screens::area_on_display`), named after the window
/// under its centre, else the app in front.
pub fn region_target_on(
    screens: &[Screen],
    area: [f64; 4],
    windows: &[TargetInfo],
    front: Option<(i32, String, Option<String>)>,
) -> TargetInfo {
    let [x, y, width, height] = area;
    let hit = windows.iter().find(|w| {
        let f = w.frame;
        crate::screens::contains([f.x, f.y, f.width, f.height], [x + width / 2.0, y + height / 2.0])
    });
    let (display_id, scale, rect) = crate::screens::area_on_display(screens, area).map_or_else(
        || {
            let min = crate::screens::MIN_AREA;
            (0, 1.0, [x, y, width.max(min), height.max(min)])
        },
        |(i, rect)| (screens[i].id, screens[i].scale, rect),
    );
    TargetInfo {
        mode: CaptureMode::Region,
        app: hit
            .and_then(|w| w.app.clone())
            .or_else(|| front.as_ref().map(|(_, name, _)| name.clone())),
        bundle_id: None,
        window_title: hit.and_then(|w| w.window_title.clone()),
        window_id: None,
        display_id,
        frame: TargetFrame {
            x: rect[0],
            y: rect[1],
            width: rect[2],
            height: rect[3],
            scale,
        },
    }
}

/// The session-only Hyprland rule (Lua, Hyprland 0.53+) that floats the capsule's window
/// `width`×`height` points, `top` points below the top of its monitor, centred, pinned to every
/// workspace. It is not focused when it appears, but it must stay focusable: Hyprland sends no
/// pointer events to a `no_focus` window, so its chips could not be clicked.
pub fn pill_rule(width: f32, height: f32, top: f32) -> String {
    format!(
        "hl.window_rule({{ match = {{ class = \"^({PILL_CLASS})$\" }}, float = true, pin = true, \
         no_initial_focus = true, border_size = 0, no_shadow = true, no_blur = true, \
         move = {{ \"(monitor_w-{width})/2\", \"{top}\" }}, size = {{ \"{width}\", \"{height}\" }} }})"
    )
}

/// The same rules for Hyprland before 0.53 (`keyword windowrulev2`).
pub fn legacy_pill_rules(width: f32, height: f32, top: f32) -> Vec<String> {
    [
        "float".to_owned(),
        "pin".to_owned(),
        "noinitialfocus".to_owned(),
        "bordersize 0".to_owned(),
        "noshadow".to_owned(),
        "noblur".to_owned(),
        format!("move ((monitor_w-{width})/2) {top}"),
        format!("size {width} {height}"),
    ]
    .into_iter()
    .map(|rule| format!("{rule}, class:^({PILL_CLASS})$"))
    .collect()
}

/// The picker's window covers its whole monitor, pinned, with nothing drawn around it. Its
/// corners are square too: the session's rounding would leave the four screen corners undimmed.
pub fn picker_rule() -> String {
    format!(
        "hl.window_rule({{ match = {{ class = \"^({PICKER_CLASS})$\" }}, float = true, pin = true, \
         border_size = 0, rounding = 0, no_shadow = true, no_blur = true, no_anim = true, \
         move = {{ \"0\", \"0\" }}, size = {{ \"monitor_w\", \"monitor_h\" }} }})"
    )
}

/// A window of `class` floats centred at `width`×`height` points instead of tiling. It is an
/// ordinary window otherwise: focusable, with the session's border and shadow.
fn centered_rule(class: &str, width: f32, height: f32) -> String {
    format!(
        "hl.window_rule({{ match = {{ class = \"^({class})$\" }}, float = true, center = true, \
         size = {{ \"{width}\", \"{height}\" }} }})"
    )
}

/// The review window, centred.
pub fn review_rule(width: f32, height: f32) -> String {
    centered_rule(REVIEW_CLASS, width, height)
}

/// The Settings window, centred.
pub fn settings_rule(width: f32, height: f32) -> String {
    centered_rule(SETTINGS_CLASS, width, height)
}

/// The keep-alive window lives on a hidden special workspace, unfocusable, 1×1.
pub fn keep_alive_rule() -> String {
    format!(
        "hl.window_rule({{ match = {{ class = \"^({KEEP_ALIVE_CLASS})$\" }}, float = true, \
         no_focus = true, workspace = \"special:scrawl silent\", size = {{ \"1\", \"1\" }} }})"
    )
}

/// `keep_alive_rule` for Hyprland before 0.53.
pub fn legacy_keep_alive_rules() -> Vec<String> {
    ["float", "nofocus", "workspace special:scrawl silent", "size 1 1"]
        .map(|rule| format!("{rule}, class:^({KEEP_ALIVE_CLASS})$"))
        .into()
}

/// The ink overlay's layer maps without an animation: the first stroke would otherwise be
/// drawn into a fading-in layer, and so would the video.
pub fn ink_layer_rule() -> String {
    format!(
        "hl.layer_rule({{ match = {{ namespace = \"^({INK_NAMESPACE})$\" }}, no_anim = true }})"
    )
}

/// `ink_layer_rule` for Hyprland before 0.53.
pub fn legacy_ink_layer_rule() -> String {
    format!("noanim, ^({INK_NAMESPACE})$")
}

/// `centered_rule` for Hyprland before 0.53.
fn legacy_centered_rules(class: &str, width: f32, height: f32) -> [String; 3] {
    ["float".to_owned(), "center".to_owned(), format!("size {width} {height}")]
        .map(|rule| format!("{rule}, class:^({class})$"))
}

/// `picker_rule`, `review_rule` and `settings_rule` for Hyprland before 0.53.
pub fn legacy_window_rules(review: [f32; 2], settings: [f32; 2]) -> Vec<String> {
    let picker = [
        "float",
        "pin",
        "bordersize 0",
        "rounding 0",
        "noshadow",
        "noblur",
        "noanim",
        "move 0 0",
        "size monitor_w monitor_h",
    ]
    .map(|rule| format!("{rule}, class:^({PICKER_CLASS})$"));
    picker
        .into_iter()
        .chain(legacy_centered_rules(REVIEW_CLASS, review[0], review[1]))
        .chain(legacy_centered_rules(SETTINGS_CLASS, settings[0], settings[1]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_missing_eval_falls_back_to_windowrulev2() {
        assert!(eval_unsupported("unknown request"));
        assert!(eval_unsupported("eval can't work with legacy parsers. Use keyword."));
        assert!(!eval_unsupported(
            "error: [string \"hl.window_rule(\"]:1: unexpected symbol near <eof>"
        ));
        assert!(!eval_unsupported("ok"));
    }

    fn monitors() -> Vec<Monitor> {
        serde_json::from_slice(include_bytes!("fixtures/hypr-monitors.json")).unwrap()
    }

    fn clients() -> Vec<Client> {
        serde_json::from_slice(include_bytes!("fixtures/hypr-clients.json")).unwrap()
    }

    #[test]
    fn keyboard_layout_is_the_main_keyboards() {
        let json = r#"{"mice":[],"keyboards":[
            {"address":"0x1","name":"power-button","layout":"us","main":false},
            {"address":"0x2","name":"kbd","layout":"br,us","main":true}],"tablets":[]}"#;
        let devices: Devices = serde_json::from_str(json).unwrap();
        assert_eq!(main_keyboard_layout(&devices).as_deref(), Some("br,us"));
        let none_main: Devices =
            serde_json::from_str(r#"{"keyboards":[{"layout":"de"}]}"#).unwrap();
        assert_eq!(main_keyboard_layout(&none_main).as_deref(), Some("de"));
        let empty: Devices = serde_json::from_str(r#"{"keyboards":[]}"#).unwrap();
        assert_eq!(main_keyboard_layout(&empty), None);
    }

    #[test]
    fn default_target_is_the_focused_monitor_in_points() {
        let target = pick_target(&monitors(), &clients(), None).unwrap();
        assert_eq!(target.mode, CaptureMode::Display);
        assert_eq!(target.display_id, 0);
        assert_eq!((target.frame.width, target.frame.height, target.frame.scale), (1920.0, 1080.0, 1.0));
    }

    #[test]
    fn scaled_monitor_frame_is_logical() {
        let frame = monitor_frame(&monitors()[1]);
        assert_eq!((frame.x, frame.width, frame.height), (1920.0, 2048.0, 1152.0));
    }

    #[test]
    fn window_query_matches_title_or_class() {
        let target = pick_target(&monitors(), &clients(), Some("5173")).unwrap();
        assert_eq!(target.mode, CaptureMode::Window);
        assert_eq!(target.app.as_deref(), Some("Helium"));
        assert_eq!(target.display_id, 1);
        assert_eq!((target.frame.x, target.frame.y, target.frame.width), (1930.0, 40.0, 1000.0));
        assert!(pick_target(&monitors(), &clients(), Some("nothing-like-this")).is_err());
    }

    #[test]
    fn entries_are_the_active_workspace_front_to_back() {
        let entries = window_entries(&clients(), &monitors()[0]);
        let apps: Vec<_> = entries.iter().map(|e| e.app.as_str()).collect();
        // Floating kitty draws above the tiled T3 Code; Steam is on another workspace.
        assert_eq!(apps, ["kitty", "com.t3tools.T3Code"]);
        assert!(entries[1].frontmost && !entries[0].frontmost);
        assert_eq!(entries[1].bounds, [10, 52, 1900, 1018]);
    }

    #[test]
    fn a_shown_special_workspace_and_pinned_windows_are_on_screen() {
        let mut monitor = monitors().remove(0);
        let mut all = clients();
        let mut extra = |class: &str, workspace: i64, pinned: bool| {
            all.push(
                serde_json::from_value(serde_json::json!({
                    "address": format!("0x{class}"), "at": [10, 52], "size": [1900, 1018],
                    "class": class, "title": "", "workspace": { "id": workspace, "name": "" },
                    "monitor": 0, "mapped": true, "hidden": false, "floating": pinned,
                    "pinned": pinned, "focusHistoryID": 9, "pid": 5,
                }))
                .unwrap(),
            );
        };
        extra("scratch", -99, false);
        extra("everywhere", 7, true);
        let apps = |monitor: &Monitor| -> Vec<String> {
            window_entries(&all, monitor).into_iter().map(|e| e.app).collect()
        };
        // Hidden scratchpad: its windows aren't on screen; a pinned one is on every workspace.
        assert_eq!(apps(&monitor), ["kitty", "everywhere", "com.t3tools.T3Code"]);
        monitor.special_workspace = Some(WorkspaceRef { id: -99 });
        assert_eq!(apps(&monitor), ["scratch", "kitty", "everywhere", "com.t3tools.T3Code"]);
        monitor.special_workspace = Some(WorkspaceRef { id: 0 });
        assert_eq!(apps(&monitor).len(), 3);
    }

    #[test]
    fn bounds_are_relative_to_the_monitor() {
        let entries = window_entries(&clients(), &monitors()[1]);
        assert_eq!(entries[0].bounds, [10, 40, 1000, 800]);
    }

    fn frame_at(x: f64, y: f64, width: f64, height: f64) -> TargetFrame {
        TargetFrame { x, y, width, height, scale: 1.0 }
    }

    fn seen(frame: TargetFrame) -> Vec<(String, [i32; 4])> {
        capture_entries(&clients(), &monitors(), frame).into_iter().map(|e| (e.app, e.bounds)).collect()
    }

    #[test]
    fn a_window_capture_logs_windows_from_its_own_corner_and_drops_those_it_does_not_reach() {
        // The capture of the kitty window: 600,300 700x400 on the first monitor.
        let shown = seen(frame_at(600.0, 300.0, 700.0, 400.0));
        let shown: Vec<_> = shown.iter().map(|(app, bounds)| (app.as_str(), *bounds)).collect();
        assert_eq!(shown, [("kitty", [0, 0, 700, 400]), ("com.t3tools.T3Code", [-590, -248, 1900, 1018])]);
        // An area at the top left that the floating kitty doesn't reach.
        let apps: Vec<_> = seen(frame_at(0.0, 0.0, 300.0, 200.0)).into_iter().map(|(app, _)| app).collect();
        assert_eq!(apps, ["com.t3tools.T3Code"]);
    }

    #[test]
    fn an_area_on_the_second_monitor_logs_that_monitors_windows() {
        let monitors = monitors();
        assert_eq!(seen(frame_at(1925.0, 20.0, 400.0, 300.0)), [("Helium".to_owned(), [5, 20, 1000, 800])]);
        assert!(on_a_monitor(&monitors, frame_at(1925.0, 20.0, 400.0, 300.0)));
        assert!(!on_a_monitor(&monitors, frame_at(9_000.0, 0.0, 10.0, 10.0)));
        // Touching a monitor's edge is not being on it.
        assert!(!on_a_monitor(&monitors, frame_at(-100.0, 0.0, 100.0, 100.0)));
    }

    #[test]
    fn a_capture_across_two_monitors_gets_the_windows_of_both() {
        // Straddling the two monitors (the border is at 1920): the first monitor's browser and the
        // second monitor's, each in the capture's coordinates, none twice.
        let both = seen(frame_at(1800.0, 20.0, 400.0, 300.0));
        assert_eq!(
            both,
            [
                ("com.t3tools.T3Code".to_owned(), [-1790, 32, 1900, 1018]),
                ("Helium".to_owned(), [130, 20, 1000, 800]),
            ]
        );
    }

    #[test]
    fn a_floating_window_of_a_neighbouring_monitor_that_hangs_into_the_capture_is_found() {
        // Homed on the first monitor but dragged over the border; the capture is on the second
        // monitor alone and a window of the monitor it is on would never have led to it.
        let mut all = clients();
        all[1].at = [1800, 300];
        let monitors = monitors();
        let frame = frame_at(1920.0, 0.0, 600.0, 600.0);
        let hanging: Vec<_> = capture_entries(&all, &monitors, frame)
            .into_iter()
            .filter(|e| e.app == "kitty")
            .map(|e| e.bounds)
            .collect();
        assert_eq!(hanging, [[-120, 300, 700, 400]]);
    }

    #[test]
    fn an_area_the_size_of_a_monitor_but_offset_is_translated() {
        // 1920x1080 like the first monitor but starting at 500,0: the size alone says nothing.
        let shown = seen(frame_at(500.0, 0.0, 1920.0, 1080.0));
        let by_app = |app: &str| shown.iter().find(|(a, _)| a == app).map(|(_, b)| *b);
        assert_eq!(by_app("com.t3tools.T3Code"), Some([-490, 52, 1900, 1018]));
        assert_eq!(by_app("kitty"), Some([100, 300, 700, 400]));
        assert_eq!(by_app("Helium"), Some([1430, 40, 1000, 800]));
    }

    #[test]
    fn a_display_capture_of_the_second_monitor_is_in_its_own_coordinates() {
        let monitors = monitors();
        assert_eq!(seen(monitor_frame(&monitors[1])), [("Helium".to_owned(), [10, 40, 1000, 800])]);
    }

    #[test]
    fn a_display_capture_logs_what_it_always_did() {
        let monitors = monitors();
        let frame = monitor_frame(&monitors[0]);
        assert_eq!(capture_entries(&clients(), &monitors, frame), window_entries(&clients(), &monitors[0]));
    }

    #[test]
    fn a_log_with_a_long_stretch_unwatched_has_gaps() {
        let mut coverage = Coverage::default();
        for t in [0, 1_000, 2_000, 4_500] {
            coverage.saw(t);
        }
        assert!(!coverage.has_gaps(6_000), "two missed looks are tolerated");
        assert!(coverage.has_gaps(9_000), "nothing for the last 4.5 s");
        coverage.saw(9_000);
        assert!(coverage.has_gaps(9_000), "the hole stays");
        // A log that never got a look is unwatched from the start, a late first look too.
        assert!(Coverage::default().has_gaps(10_000));
        assert!(!Coverage::default().has_gaps(1_000));
        let mut late = Coverage::default();
        late.saw(5_000);
        assert!(late.has_gaps(5_000));
    }

    #[test]
    fn screens_are_logical_less_the_bars_with_the_origin_first() {
        let screens = screens_from(&monitors());
        assert_eq!(screens.len(), 2);
        assert_eq!(screens[0].id, 0);
        assert_eq!(screens[0].frame, [0.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(screens[0].visible, [0.0, 42.0, 1920.0, 1038.0]);
        assert_eq!(screens[0].scale, 1.0);
        assert_eq!(screens[1].frame, [1920.0, 0.0, 2048.0, 1152.0]);
        assert_eq!(screens[1].visible, screens[1].frame);
        assert_eq!(screens[1].scale, 1.25);
        let mut reversed = monitors();
        reversed.reverse();
        assert_eq!(screens_from(&reversed)[0].id, 0);
    }

    #[test]
    fn selectable_windows_are_visible_others_front_to_back() {
        // Kitty is Scrawl itself here; Steam is on a hidden workspace.
        let windows = selectable_windows(&monitors(), &clients(), 2);
        let apps: Vec<_> = windows.iter().map(|w| w.app.as_deref().unwrap()).collect();
        assert_eq!(apps, ["com.t3tools.T3Code", "Helium"]);
        let helium = &windows[1];
        assert_eq!(helium.mode, CaptureMode::Window);
        assert_eq!(helium.window_title.as_deref(), Some("localhost:5173"));
        assert_eq!(helium.display_id, 1);
        assert_eq!(
            (helium.frame.x, helium.frame.y, helium.frame.width, helium.frame.height),
            (1930.0, 40.0, 1000.0, 800.0)
        );
        assert_eq!(helium.frame.scale, 1.25);
        assert!(helium.window_id.is_some());
        // Floating windows draw above tiled ones.
        let all = selectable_windows(&monitors(), &clients(), 99);
        assert_eq!(all[0].app.as_deref(), Some("kitty"));
        // Slivers are left out.
        let mut tiny = clients();
        tiny[1].size = [60, 400];
        assert!(selectable_windows(&monitors(), &tiny, 99).iter().all(|w| w.app.as_deref() != Some("kitty")));
    }

    #[test]
    fn display_target_is_the_monitor_under_the_point() {
        let right = display_target_at(&monitors(), Some([2000.0, 100.0])).unwrap();
        assert_eq!(right.mode, CaptureMode::Display);
        assert_eq!(right.display_id, 1);
        assert_eq!((right.frame.x, right.frame.width, right.frame.scale), (1920.0, 2048.0, 1.25));
        // Below the shorter monitor: the nearest one.
        assert_eq!(display_target_at(&monitors(), Some([100.0, 1100.0])).unwrap().display_id, 0);
        // No pointer: the focused monitor.
        assert_eq!(display_target_at(&monitors(), None).unwrap().display_id, 0);
        assert!(display_target_at(&[], None).is_none());
        // `screens_from` puts the origin monitor first; the answer is still by monitor id.
        let mut reversed = monitors();
        reversed.reverse();
        assert_eq!(display_target_at(&reversed, Some([2000.0, 100.0])).unwrap().display_id, 1);
        assert_eq!(display_target_at(&reversed, Some([10.0, 10.0])).unwrap().display_id, 0);
    }

    #[test]
    fn front_app_skips_scrawl_for_the_last_window_focused() {
        assert!(parse_active_window(b"{}").is_none());
        let kitty = clients().remove(1);
        let active = parse_active_window(&serde_json::to_vec(&serde_json::json!({
            "address": "0x1", "at": [0, 0], "size": [10, 10], "class": "kitty", "title": "t",
            "workspace": {"id": 1}, "monitor": 0, "mapped": true, "hidden": false,
            "floating": true, "focusHistoryID": 0, "pid": 7
        })).unwrap()).unwrap();
        assert_eq!((active.pid, active.class.as_str()), (7, "kitty"));
        assert_eq!(front_app(Some(&active), &clients(), 99), Some((7, "kitty".to_owned(), None)));
        // Scrawl (pid 2) has focus: the window focused before it.
        assert_eq!(
            front_app(Some(&kitty), &clients(), 2),
            Some((1, "com.t3tools.T3Code".to_owned(), None))
        );
        assert_eq!(front_app(None, &clients(), 2), Some((1, "com.t3tools.T3Code".to_owned(), None)));
        assert_eq!(front_app(None, &[], 2), None);
    }

    #[test]
    fn region_target_is_named_after_the_window_under_it_and_kept_on_one_display() {
        let screens = screens_from(&monitors());
        let windows = selectable_windows(&monitors(), &clients(), 99);
        let front = Some((1, "com.t3tools.T3Code".to_owned(), None));
        // Over Helium on the scaled monitor.
        let target = region_target_on(&screens, [2000.0, 100.0, 300.0, 200.0], &windows, front.clone());
        assert_eq!(target.mode, CaptureMode::Region);
        assert_eq!(target.app.as_deref(), Some("Helium"));
        assert_eq!(target.window_title.as_deref(), Some("localhost:5173"));
        assert_eq!(target.window_id, None);
        assert_eq!(target.display_id, 1);
        assert_eq!(
            (target.frame.x, target.frame.y, target.frame.width, target.frame.height, target.frame.scale),
            (2000.0, 100.0, 300.0, 200.0, 1.25)
        );
        // A sliver at the right edge grows to the smallest area, inside the display.
        let edge = region_target_on(&screens, [1915.0, 500.0, 4.0, 4.0], &[], front.clone());
        assert_eq!(edge.display_id, 0);
        assert_eq!((edge.frame.x, edge.frame.width, edge.frame.height), (1920.0 - 32.0, 32.0, 32.0));
        // Over no window: the app in front.
        assert_eq!(edge.app.as_deref(), Some("com.t3tools.T3Code"));
        assert_eq!(edge.window_title, None);
    }

    #[test]
    fn pill_rule_floats_the_capsule_top_centre() {
        assert_eq!(
            pill_rule(520.0, 84.0, 34.0),
            r#"hl.window_rule({ match = { class = "^(scrawl-pill)$" }, float = true, pin = true, no_initial_focus = true, border_size = 0, no_shadow = true, no_blur = true, move = { "(monitor_w-520)/2", "34" }, size = { "520", "84" } })"#
        );
        assert_eq!(
            legacy_pill_rules(520.0, 84.0, 34.0),
            [
                "float, class:^(scrawl-pill)$",
                "pin, class:^(scrawl-pill)$",
                "noinitialfocus, class:^(scrawl-pill)$",
                "bordersize 0, class:^(scrawl-pill)$",
                "noshadow, class:^(scrawl-pill)$",
                "noblur, class:^(scrawl-pill)$",
                "move ((monitor_w-520)/2) 34, class:^(scrawl-pill)$",
                "size 520 84, class:^(scrawl-pill)$",
            ]
        );
    }

    #[test]
    fn the_lock_state_is_read_from_hyprland() {
        assert_eq!(parse_locked(b"\n{\n    \"locked\": false\n}\n"), Some(false));
        assert_eq!(parse_locked(br#"{"locked": true}"#), Some(true));
        assert_eq!(parse_locked(b"unknown request"), None);
    }

    #[test]
    fn the_keep_alive_window_is_out_of_sight() {
        assert_eq!(
            keep_alive_rule(),
            r#"hl.window_rule({ match = { class = "^(scrawl-keep-alive)$" }, float = true, no_focus = true, workspace = "special:scrawl silent", size = { "1", "1" } })"#
        );
        assert_eq!(
            legacy_keep_alive_rules(),
            [
                "float, class:^(scrawl-keep-alive)$",
                "nofocus, class:^(scrawl-keep-alive)$",
                "workspace special:scrawl silent, class:^(scrawl-keep-alive)$",
                "size 1 1, class:^(scrawl-keep-alive)$",
            ]
        );
    }

    #[test]
    fn the_ink_layer_maps_without_an_animation() {
        assert_eq!(
            ink_layer_rule(),
            r#"hl.layer_rule({ match = { namespace = "^(scrawl-ink)$" }, no_anim = true })"#
        );
        assert_eq!(legacy_ink_layer_rule(), "noanim, ^(scrawl-ink)$");
    }

    #[test]
    fn picker_covers_its_monitor_and_review_and_settings_float_centred() {
        assert_eq!(
            picker_rule(),
            r#"hl.window_rule({ match = { class = "^(scrawl-picker)$" }, float = true, pin = true, border_size = 0, rounding = 0, no_shadow = true, no_blur = true, no_anim = true, move = { "0", "0" }, size = { "monitor_w", "monitor_h" } })"#
        );
        assert_eq!(
            review_rule(1120.0, 740.0),
            r#"hl.window_rule({ match = { class = "^(scrawl-review)$" }, float = true, center = true, size = { "1120", "740" } })"#
        );
        assert_eq!(
            settings_rule(640.0, 480.0),
            r#"hl.window_rule({ match = { class = "^(scrawl-settings)$" }, float = true, center = true, size = { "640", "480" } })"#
        );
        assert_eq!(
            legacy_window_rules([1120.0, 740.0], [640.0, 480.0]),
            [
                "float, class:^(scrawl-picker)$",
                "pin, class:^(scrawl-picker)$",
                "bordersize 0, class:^(scrawl-picker)$",
                "rounding 0, class:^(scrawl-picker)$",
                "noshadow, class:^(scrawl-picker)$",
                "noblur, class:^(scrawl-picker)$",
                "noanim, class:^(scrawl-picker)$",
                "move 0 0, class:^(scrawl-picker)$",
                "size monitor_w monitor_h, class:^(scrawl-picker)$",
                "float, class:^(scrawl-review)$",
                "center, class:^(scrawl-review)$",
                "size 1120 740, class:^(scrawl-review)$",
                "float, class:^(scrawl-settings)$",
                "center, class:^(scrawl-settings)$",
                "size 640 480, class:^(scrawl-settings)$",
            ]
        );
    }
    fn client(class: &str, mapped: bool) -> Client {
        serde_json::from_value(serde_json::json!({
            "address": format!("0x{class}"),
            "at": [0, 0],
            "size": [100, 100],
            "class": class,
            "title": "",
            "workspace": { "id": 1, "name": "1" },
            "monitor": 0,
            "mapped": mapped,
            "hidden": false,
            "floating": true,
            "focusHistoryID": 0,
            "pid": 1,
        }))
        .unwrap()
    }

    #[test]
    fn raising_the_capsule_is_alter_zorder_top_in_lua_and_in_the_old_dispatcher() {
        assert_eq!(
            raise_commands("scrawl-pill"),
            [
                r#"eval hl.dispatch(hl.dsp.window.alter_zorder({ mode = "top", window = "class:^(scrawl-pill)$" }))"#.to_owned(),
                "dispatch alterzorder top,class:^(scrawl-pill)$".to_owned(),
            ]
        );
    }

    #[test]
    fn the_capsule_is_raised_once_both_it_and_the_picker_are_mapped() {
        let both = [client("Helium", true), client("scrawl-pill", true), client("scrawl-picker", true)];
        assert!(pill_under_picker(&both));
        // A capsule that mapped after the picker is above it already.
        let after = [client("scrawl-picker", true), client("scrawl-pill", true)];
        assert!(!pill_under_picker(&after));
        // No capsule (it opens after the picker, so above it), or a picker not up yet.
        assert!(!pill_under_picker(&[client("scrawl-picker", true)]));
        assert!(!pill_under_picker(&[client("scrawl-pill", true), client("scrawl-picker", false)]));
        assert!(!pill_under_picker(&[client("scrawl-pill", false), client("scrawl-picker", true)]));
        assert!(!pill_under_picker(&[]));
    }

    #[test]
    fn without_a_capsule_the_raise_thread_ends_at_once() {
        // The picker not up yet is no reason to wait when there is nothing to raise.
        assert_eq!(pill_step(&[]), PillStep::Done);
        assert_eq!(pill_step(&[client("Helium", true)]), PillStep::Done);
        assert_eq!(pill_step(&[client("scrawl-pill", false)]), PillStep::Done);
        assert_eq!(pill_step(&[client("scrawl-picker", true)]), PillStep::Done);
    }

    #[test]
    fn a_capsule_waits_for_the_picker_then_is_raised_or_left() {
        assert_eq!(pill_step(&[client("scrawl-pill", true)]), PillStep::Wait);
        assert_eq!(
            pill_step(&[client("scrawl-pill", true), client("scrawl-picker", true)]),
            PillStep::Raise
        );
        assert_eq!(
            pill_step(&[client("scrawl-picker", true), client("scrawl-pill", true)]),
            PillStep::Done
        );
    }
}
