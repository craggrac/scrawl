//! Linux backends for what macOS does with ScreenCaptureKit, AppKit and `scrawl-helper`, and
//! the headless recording flow a Hyprland key bind drives through `scrawl toggle`.

pub mod hypr;
pub mod media;
pub mod parakeet;
pub mod record;
pub mod toggle;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::audio::AudioRecorder;
use crate::capture::scrawl_log;
use crate::handoff;
use crate::model::CaptureMode;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

/// SIGINT and SIGTERM end the recording instead of the process.
fn install_stop_signals() {
    let handler = on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

/// Until a stop signal, or `duration` seconds unless `until_stopped`.
fn wait_for_stop(duration: u64, until_stopped: bool) {
    let deadline = (!until_stopped).then(|| Instant::now() + Duration::from_secs(duration.max(1)));
    while !STOP.load(Ordering::Acquire) && deadline.is_none_or(|d| Instant::now() < d) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Desktop notification; failures (no `notify-send`) are ignored.
pub fn notify(text: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "Scrawl", "Scrawl", text])
        .spawn();
}

/// Records the focused monitor (or `window`) with the microphone into a `.scrawl` bundle, then
/// puts its summary on the clipboard. Delivery to OpenCode stays manual (`scrawl send`).
pub fn record(
    window: Option<&str>,
    duration: u64,
    until_stopped: bool,
    note: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    install_stop_signals();
    let result = record_bundle(window, duration, until_stopped, note, out);
    toggle::clear_pidfile();
    match &result {
        Ok(dir) => {
            let copied = handoff::copy_bundle_to_clipboard(dir);
            if let Err(error) = &copied {
                scrawl_log(format!("Clipboard: {error:#}"));
            }
            notify(&if copied.is_ok() {
                format!("Capture ready, summary copied\n{}", dir.display())
            } else {
                format!("Capture ready\n{}", dir.display())
            });
        }
        Err(error) => notify(&format!("Recording failed: {error:#}")),
    }
    result.map(|_| ())
}

fn record_bundle(
    window: Option<&str>,
    duration: u64,
    until_stopped: bool,
    note: Option<String>,
    out: Option<PathBuf>,
) -> Result<PathBuf> {
    let target = hypr::discover_target(window);
    let bundle_dir = out.unwrap_or_else(|| {
        handoff::default_captures_dir().join(format!("{}.scrawl", crate::make_bundle_id()))
    });
    std::fs::create_dir_all(&bundle_dir)?;
    let started_at = Instant::now();
    let audio = AudioRecorder::start().ok();
    let session = match record::ScreenSession::start(
        &target,
        &bundle_dir.join("recording.mp4"),
        started_at,
    ) {
        Ok(session) => session,
        Err(error) => {
            // No empty bundle for `latest` to find.
            let _ = std::fs::remove_dir_all(&bundle_dir);
            return Err(error);
        }
    };
    let window_log = (target.mode == CaptureMode::Display)
        .then(|| hypr::WindowLogger::start(target.frame, started_at, Arc::default()));
    notify("Recording. Press the shortcut again to stop.");
    wait_for_stop(duration, until_stopped);
    toggle::clear_pidfile();
    let recording = session.stop()?;
    let activity = window_log.map(hypr::WindowLogger::stop).unwrap_or_default();
    let recorded_audio = audio.and_then(|a| a.stop().ok());
    let (dir, _bundle, summary) = crate::finalize_capture_bundle(
        &bundle_dir,
        target,
        recording,
        Vec::new(),
        Vec::new(),
        recorded_audio,
        note,
        activity,
    )?;
    print!("{summary}");
    Ok(dir)
}
