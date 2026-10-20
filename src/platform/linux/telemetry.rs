//! What the coordinator shares with a running capture. On macOS the event tap fills it live;
//! here the evdev recorder (`linux::input::InputRecorder`) runs for each capture, from
//! `reset_for_session` to `finish_session`, and hands its events over at the end.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use super::ShortcutConfig;
use crate::capture::LiveOverlayEvent;
use crate::linux::input::InputRecorder;
use crate::model::{CaptureMode, InteractionEvent, Stroke, TargetFrame};

/// Where a capture's events go once `finish_session` has them, for the screen session that
/// decodes a frame after each click (`session::ScreenCaptureSession::stop`).
pub(super) type EventSlot = Arc<Mutex<Option<Vec<InteractionEvent>>>>;

pub struct SharedCaptureTelemetry {
    /// Interaction is being recorded.
    pub active: AtomicBool,
    pub selecting: AtomicBool,
    /// The record shortcut (`super::update_global_shortcut`).
    pub shortcut: Mutex<ShortcutConfig>,
    /// The capture clock's zero.
    pub started_at: Mutex<Instant>,
    pub target_frame: Mutex<TargetFrame>,
    /// The finished capture's events (empty while it runs: the recorder keeps them).
    pub events: Mutex<Vec<InteractionEvent>>,
    /// Ink strokes; none on Linux until the overlay (phase 5).
    pub strokes: Mutex<Vec<Stroke>>,
    pub live_ink: Mutex<Option<Stroke>>,
    /// Scrawl's own windows, left out of a macOS capture; `wf-recorder` records the output.
    pub excluded_window_ids: Arc<Mutex<Vec<u32>>>,
    pub passthrough_window_ids: Mutex<Vec<u32>>,
    /// Raised on a click or shortcut, so the window log takes a snapshot right away.
    pub window_log_request: Arc<AtomicBool>,
    /// macOS flashes an edge glow on a quick start; Linux has no overlay to draw it.
    pub start_glow: AtomicBool,
    pub ui_sender: flume::Sender<LiveOverlayEvent>,
    capture_mode: Mutex<CaptureMode>,
    picker_rect: Mutex<Option<([f32; 4], u8)>>,
    escape: AtomicBool,
    input: Mutex<Option<InputRecorder>>,
    session_events: Mutex<Option<EventSlot>>,
}

impl SharedCaptureTelemetry {
    pub fn new(ui_sender: flume::Sender<LiveOverlayEvent>) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicBool::new(false),
            selecting: AtomicBool::new(false),
            shortcut: Mutex::new(ShortcutConfig::default_record()),
            started_at: Mutex::new(Instant::now()),
            target_frame: Mutex::new(TargetFrame {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
                scale: 1.0,
            }),
            events: Mutex::new(Vec::new()),
            strokes: Mutex::new(Vec::new()),
            live_ink: Mutex::new(None),
            excluded_window_ids: Arc::new(Mutex::new(Vec::new())),
            passthrough_window_ids: Mutex::new(Vec::new()),
            window_log_request: Arc::new(AtomicBool::new(false)),
            start_glow: AtomicBool::new(false),
            ui_sender,
            capture_mode: Mutex::new(CaptureMode::Display),
            picker_rect: Mutex::new(None),
            escape: AtomicBool::new(false),
            input: Mutex::new(None),
            session_events: Mutex::new(None),
        })
    }

    /// The record shortcut, from wherever it was seen.
    pub fn emit_capture_shortcut(&self) {
        let _ = self.ui_sender.try_send(LiveOverlayEvent::CaptureShortcut);
    }

    pub fn matches_shortcut(&self, key_code: u16, cmd: bool, ctrl: bool, alt: bool, shift: bool) -> bool {
        self.shortcut
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .matches(key_code, cmd, ctrl, alt, shift)
    }

    pub fn set_capture_mode(&self, mode: CaptureMode) {
        *self.capture_mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    /// The picker opened: no capture is running.
    pub fn begin_picker_session(&self) {
        self.selecting.store(true, Ordering::Release);
        self.escape.store(false, Ordering::Release);
    }

    /// What the picker highlights (drawn by the overlay on macOS; phase 5 here).
    pub fn set_picker_rect(&self, rect: Option<([f32; 4], u8)>) {
        *self.picker_rect.lock().unwrap_or_else(PoisonError::into_inner) = rect;
    }

    /// A capture of `frame` starts now: the clock restarts and the input recorder starts.
    pub fn reset_for_session(&self, frame: TargetFrame) {
        let started_at = Instant::now();
        *self.started_at.lock().unwrap_or_else(PoisonError::into_inner) = started_at;
        *self.target_frame.lock().unwrap_or_else(PoisonError::into_inner) = frame;
        self.events.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.strokes.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.selecting.store(false, Ordering::Release);
        self.active.store(true, Ordering::Release);
        let recorder =
            InputRecorder::start(frame, started_at, Arc::clone(&self.window_log_request));
        // A recorder left from a capture that never finished stops here.
        if let Some(old) = self.input.lock().unwrap_or_else(PoisonError::into_inner).replace(recorder) {
            let _ = old.stop();
        }
    }

    /// The screen session that will want this capture's events.
    pub(super) fn hand_events_to(&self, slot: EventSlot) {
        *self.session_events.lock().unwrap_or_else(PoisonError::into_inner) = Some(slot);
    }

    /// Esc went down since the last look. Esc is only seen by Scrawl's own windows here.
    pub fn take_escape(&self) -> bool {
        self.escape.swap(false, Ordering::AcqRel)
    }

    /// Ends the capture's input recording: its events (also handed to the screen session) and
    /// no strokes.
    pub fn finish_session(&self) -> (Vec<InteractionEvent>, Vec<Stroke>) {
        self.active.store(false, Ordering::Release);
        let recorder = self.input.lock().unwrap_or_else(PoisonError::into_inner).take();
        let events = recorder.map(InputRecorder::stop).unwrap_or_default();
        *self.events.lock().unwrap_or_else(PoisonError::into_inner) = events.clone();
        if let Some(slot) = self.session_events.lock().unwrap_or_else(PoisonError::into_inner).take() {
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(events.clone());
        }
        (events, Vec::new())
    }
}
