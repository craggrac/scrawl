//! A screen capture with the macOS session's shape (`start` / `elapsed_ms` / `stop`), on
//! `wf-recorder` (`linux::record::ScreenSession`). Its candidate frames are decoded from the
//! video when the bundle is built (`linux::record::fill_candidates`), not when it stops.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use super::SharedCaptureTelemetry;
use crate::capture::{ScreenRecording, scrawl_log};
use crate::linux::record;
use crate::model::TargetInfo;

pub struct ScreenCaptureSession {
    started_at: Instant,
    video: Option<PathBuf>,
    /// Starting `wf-recorder` waits for its first frame (up to seconds on a still screen), so
    /// it happens off the UI thread.
    recorder: Option<JoinHandle<Option<record::ScreenSession>>>,
    /// Set by `discard_screen_session`: a start still waiting gives up at once.
    cancel: Arc<AtomicBool>,
    /// Counts this session in `recorders_running` until its recorder has stopped.
    running: Option<Running>,
}

/// Sessions whose `wf-recorder` may still be running.
static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// Whether any `wf-recorder` may still be running: quitting waits for them, since a recorder
/// outlives the app otherwise.
pub fn recorders_running() -> bool {
    RUNNING.load(Ordering::Acquire) > 0
}

/// Also holds cleanup off (`retention::Recording`) while the recorder may be running.
struct Running(#[allow(dead_code)] crate::retention::Recording);

impl Running {
    fn new() -> Self {
        RUNNING.fetch_add(1, Ordering::AcqRel);
        Self(crate::retention::Recording::start())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Starts recording `target` into `video` on the telemetry's capture clock (call after
/// `reset_for_session`). A recorder that won't start is logged and leaves a session with no
/// video, as a failed ScreenCaptureKit stream does on macOS.
pub fn start_screen_session(
    target: TargetInfo,
    video: Option<PathBuf>,
    telemetry: &Arc<SharedCaptureTelemetry>,
) -> ScreenCaptureSession {
    let started_at = *telemetry
        .started_at
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    let recorder = video.clone().and_then(|path| {
        std::thread::Builder::new()
            .name("scrawl-wf-recorder".into())
            .spawn(move || {
                record::ScreenSession::start_cancellable(&target, &path, started_at, &thread_cancel)
                    .inspect_err(|error| {
                        if error.is::<record::Cancelled>() {
                            scrawl_log("Screen recording cancelled while starting");
                        } else {
                            scrawl_log(format!("Screen recording failed to start: {error:#}"));
                            crate::linux::notify(&format!("Screen recording failed: {error:#}"));
                        }
                    })
                    .ok()
            })
            .inspect_err(|error| {
                scrawl_log(format!("Could not start the recorder thread: {error}"))
            })
            .ok()
    });
    let running = recorder.is_some().then(Running::new);
    ScreenCaptureSession {
        started_at,
        video,
        recorder,
        cancel,
        running,
    }
}

/// Ends a recording whose bundle is about to be deleted, without decoding anything (dropping
/// `wf-recorder`'s session stops it).
pub fn discard_screen_session(session: ScreenCaptureSession) {
    session.cancel.store(true, Ordering::Release);
    if let Some(recorder) = session.recorder {
        let running = session.running;
        // A recorder already running is stopped as it drops, off the UI thread.
        std::thread::spawn(move || {
            if recorder.join().is_err() {
                scrawl_log("The recorder thread panicked");
            }
            drop(running);
        });
    }
}

impl ScreenCaptureSession {
    pub fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// Stops the recorder. The candidate frames are left to the bundle build, which decodes
    /// them while the narration is transcribed. Without a video, one placeholder frame, as on
    /// macOS.
    pub fn stop(self) -> ScreenRecording {
        let duration_ms = self.elapsed_ms().max(100);
        let stopped = self
            .recorder
            .and_then(|recorder| {
                recorder
                    .join()
                    .inspect_err(|_| scrawl_log("The recorder thread panicked"))
                    .ok()
                    .flatten()
            })
            .map(record::ScreenSession::stop);
        match stopped {
            Some(Ok(recording)) => recording,
            failed => {
                if let Some(Err(error)) = failed {
                    scrawl_log(format!("Screen recording: {error:#}"));
                }
                // A video ffprobe can't read is no use to the bundle.
                let video = self.video.filter(|path| {
                    let readable = crate::helper::probe(path).is_ok();
                    if !readable {
                        let _ = std::fs::remove_file(path);
                    }
                    readable
                });
                ScreenRecording {
                    frames: vec![record::placeholder_frame()],
                    video,
                    duration_ms,
                    video_offset_ms: 0,
                }
            }
        }
    }
}
