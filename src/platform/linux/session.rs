//! A screen capture with the macOS session's shape (`start` / `elapsed_ms` / `stop`), on
//! `wf-recorder` (`linux::record::ScreenSession`). Its candidate frames are decoded from the
//! video at the end, plus one after each click: those events come from the telemetry's input
//! recorder when the capture finishes.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use super::SharedCaptureTelemetry;
use super::telemetry::EventSlot;
use crate::capture::{ScreenRecording, scrawl_log};
use crate::linux::record;
use crate::model::TargetInfo;

pub struct ScreenCaptureSession {
    started_at: Instant,
    video: Option<PathBuf>,
    /// Starting `wf-recorder` waits for its first frame (up to seconds on a still screen), so
    /// it happens off the UI thread.
    recorder: Option<JoinHandle<Option<record::ScreenSession>>>,
    events: EventSlot,
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
    let events: EventSlot = Arc::new(Mutex::new(None));
    telemetry.hand_events_to(Arc::clone(&events));
    let recorder = video.clone().and_then(|path| {
        std::thread::Builder::new()
            .name("scrawl-wf-recorder".into())
            .spawn(move || {
                record::ScreenSession::start(&target, &path, started_at)
                    .inspect_err(|error| {
                        scrawl_log(format!("Screen recording failed to start: {error:#}"));
                        crate::linux::notify(&format!("Screen recording failed: {error:#}"));
                    })
                    .ok()
            })
            .ok()
    });
    ScreenCaptureSession {
        started_at,
        video,
        recorder,
        events,
    }
}

/// Ends a recording whose bundle is about to be deleted, without decoding anything (dropping
/// `wf-recorder`'s session stops it).
pub fn discard_screen_session(session: ScreenCaptureSession) {
    if let Some(recorder) = session.recorder {
        // It may still be waiting for its first frame; don't hold the UI for that.
        std::thread::spawn(move || drop(recorder.join()));
    }
}

impl ScreenCaptureSession {
    pub fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// Stops the recorder and decodes the candidate frames (after `finish_session`, which
    /// hands over the events). Without a video, one placeholder frame, as on macOS.
    pub fn stop(self) -> ScreenRecording {
        let duration_ms = self.elapsed_ms().max(100);
        let events = self
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        let stopped = self
            .recorder
            .and_then(|recorder| recorder.join().ok().flatten())
            .map(|recorder| recorder.stop(&events));
        match stopped {
            Some(Ok(recording)) => recording,
            failed => {
                if let Some(Err(error)) = failed {
                    scrawl_log(format!("Screen recording: {error:#}"));
                }
                let video = self
                    .video
                    .filter(|path| path.metadata().is_ok_and(|m| m.len() > 0));
                ScreenRecording {
                    frames: vec![crate::render::render_synthetic_demo_frame(1280, 800, 0, 0)],
                    video,
                    duration_ms,
                    video_offset_ms: 0,
                }
            }
        }
    }
}
