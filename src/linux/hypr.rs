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
use crate::windows::{ActivityLog, WindowEntry};

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
    /// 0 is the window focused last.
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i32,
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

/// One request on `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`.
fn hypr_json<T: DeserializeOwned>(command: &str) -> Result<T> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let instance =
        std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").context("Not running under Hyprland")?;
    let path = PathBuf::from(runtime).join("hypr").join(instance).join(".socket.sock");
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("Could not reach Hyprland at {}", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(format!("j/{command}").as_bytes())?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    serde_json::from_slice(&reply).with_context(|| format!("Invalid Hyprland reply to {command}"))
}

/// The cursor in global logical points.
pub fn cursor_position() -> Option<(f64, f64)> {
    hypr_json::<CursorPos>("cursorpos").ok().map(|c| (c.x, c.y))
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

/// What to record now; the first monitor's guess when Hyprland can't be asked.
pub fn discover_target(window_query: Option<&str>) -> TargetInfo {
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

/// The windows on `monitor`'s active workspace, front to back (floating above tiled, then by
/// focus), with bounds in points from the monitor's top-left.
pub fn window_entries(clients: &[Client], monitor: &Monitor) -> Vec<WindowEntry> {
    let mut visible: Vec<&Client> = clients
        .iter()
        .filter(|c| c.mapped && !c.hidden && c.workspace.id == monitor.active_workspace.id)
        .collect();
    visible.sort_by_key(|c| (!c.floating, c.focus_history_id));
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

/// Logs the cursor every tick and the window list every second or when focus moves, like the
/// macOS `windows::WindowLogger`. Like on macOS it is only started for display targets: `frame`
/// must be a monitor's `monitor_frame`, since windows are matched to that monitor by its origin
/// and their bounds are relative to it.
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
    loop {
        let finishing = stopping.load(Ordering::Acquire);
        let t = started_at.elapsed().as_millis() as u64;
        if let Ok(cursor) = hypr_json::<CursorPos>("cursorpos") {
            log.push_cursor(t, cursor.x - frame.x, cursor.y - frame.y, frame);
        }
        let front = hypr_json::<ActiveWindow>("activewindow").ok().map(|w| w.address);
        let due = !finishing
            && (last_snapshot.is_none_or(|s| s.elapsed() >= SNAPSHOT_INTERVAL)
                || front != last_front
                || request.swap(false, Ordering::AcqRel));
        if due {
            let began = Instant::now();
            if let (Ok(monitors), Ok(clients)) = (
                hypr_json::<Vec<Monitor>>("monitors"),
                hypr_json::<Vec<Client>>("clients"),
            ) && let Some(monitor) = monitors
                .iter()
                .find(|m| f64::from(m.x) == frame.x && f64::from(m.y) == frame.y)
            {
                log.push_windows(t, window_entries(&clients, monitor));
            }
            log.snapshot_calls += 1;
            log.snapshot_time += began.elapsed();
            last_snapshot = Some(Instant::now());
            last_front = front;
        }
        if finishing {
            return log;
        }
        thread::park_timeout(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitors() -> Vec<Monitor> {
        serde_json::from_slice(include_bytes!("fixtures/hypr-monitors.json")).unwrap()
    }

    fn clients() -> Vec<Client> {
        serde_json::from_slice(include_bytes!("fixtures/hypr-clients.json")).unwrap()
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
    fn bounds_are_relative_to_the_monitor() {
        let entries = window_entries(&clients(), &monitors()[1]);
        assert_eq!(entries[0].bounds, [10, 40, 1000, 800]);
    }
}
