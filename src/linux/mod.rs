//! Linux backends for what macOS does with ScreenCaptureKit, AppKit and `scrawl-helper`, and
//! the headless recording flow a Hyprland key bind drives through `scrawl toggle`.

pub mod control;
pub mod hypr;
pub mod input;
pub mod media;
pub mod parakeet;
pub mod record;
pub mod toggle;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::audio::{self, AudioRecorder};
use crate::capture::scrawl_log;
use crate::handoff;
use crate::model::CaptureMode;

/// How long the "Recording" notification stays: it lands in the first frames.
const START_NOTICE_MS: u32 = 1_500;

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

/// After the stop, so a second Ctrl-C in a terminal kills scrawl itself.
fn restore_default_signals() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
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
    notify_for(text, None);
}

/// A notification that closes after `ms`, for the one shown while recording: it is on screen,
/// so it lands in the capture, which macOS keeps its own UI out of.
fn notify_for(text: &str, ms: Option<u32>) {
    let mut command = std::process::Command::new("notify-send");
    command.args(["-a", "Scrawl"]);
    if let Some(ms) = ms {
        command.args(["-t", &ms.to_string(), "-h", "int:transient:1"]);
    }
    let _ = command.args(["Scrawl", text]).spawn();
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
    let bundle_dir = out.unwrap_or_else(|| {
        handoff::default_captures_dir().join(format!("{}.scrawl", crate::make_bundle_id()))
    });
    let created = !bundle_dir.exists();
    std::fs::create_dir_all(&bundle_dir)?;
    capture_into(&bundle_dir, window, duration, until_stopped, note, created).inspect_err(|error| {
        scrawl_log(format!("Recording failed: {error:#}"));
    })
}

fn capture_into(
    bundle_dir: &Path,
    window: Option<&str>,
    duration: u64,
    until_stopped: bool,
    note: Option<String>,
    created: bool,
) -> Result<PathBuf> {
    let target = hypr::focused_monitor_target(window);
    let started_at = Instant::now();
    let audio = AudioRecorder::start().ok();
    let video = bundle_dir.join("recording.mp4");
    let session = match record::ScreenSession::start(&target, &video, started_at) {
        Ok(session) => session,
        Err(error) => {
            // Nothing was recorded: no empty bundle for `latest` to find, and a directory the
            // user gave us stays theirs.
            if created {
                let _ = std::fs::remove_dir_all(bundle_dir);
            } else {
                let _ = std::fs::remove_file(&video);
            }
            return Err(error);
        }
    };
    // From here the recording is never deleted, however later steps fail.
    let finish = || -> Result<PathBuf> {
        let request = Arc::new(AtomicBool::new(false));
        let window_log = (target.mode == CaptureMode::Display).then(|| {
            hypr::WindowLogger::start(target.frame, started_at, Arc::clone(&request))
        });
        let input = input::InputRecorder::start(target.frame, started_at, request);
        notify_for(
            &if until_stopped {
                "Recording. Press the shortcut again to stop.".to_owned()
            } else {
                format!("Recording for {} s.", duration.max(1))
            },
            Some(START_NOTICE_MS),
        );
        wait_for_stop(duration, until_stopped);
        restore_default_signals();
        toggle::clear_pidfile();
        // The input thread stops first: the screen session needs its events to pick frames.
        let events = input.stop();
        let recording = session.stop(&events)?;
        let activity = window_log.map(hypr::WindowLogger::stop).unwrap_or_default();
        let recorded_audio = audio.and_then(|a| {
            a.stop()
                .inspect_err(|error| scrawl_log(format!("Audio: {error:#}")))
                .ok()
        });
        if recorded_audio
            .as_ref()
            .and_then(|a| audio::decode_wav_16k(&a.wav_bytes).ok())
            .is_some_and(|samples| parakeet::is_digital_silence(&samples))
        {
            scrawl_log("Microphone: recorded only digital silence (muted?)");
            notify("The microphone recorded only silence. Is it muted?");
        }
        let (dir, _bundle, summary) = crate::finalize_capture_bundle(
            bundle_dir,
            target,
            recording,
            Vec::new(),
            events,
            recorded_audio,
            note,
            activity,
        )?;
        print!("{summary}");
        Ok(dir)
    };
    finish().with_context(|| {
        format!(
            "The recording is kept in {}",
            video.display()
        )
    })
}
