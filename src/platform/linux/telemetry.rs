//! What the coordinator shares with a running capture. On macOS the event tap fills it live;
//! here the evdev recorder (`linux::input::InputRecorder`) runs for each capture, from
//! `reset_for_session` to `finish_session`: it hands its events over at the end and draws the
//! ink live into `strokes` and `live_ink`, where the overlay and the UI read it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use super::ShortcutConfig;
use super::shortcut::{PressGate, PressSource};
use crate::capture::LiveOverlayEvent;
use crate::ink::{InkState, Wake};
use crate::linux::input::InputRecorder;
use crate::model::{CaptureMode, InteractionEvent, Stroke, TargetFrame};

/// What a finished capture's input left, for the screen session that decodes a frame after
/// each click and each stroke (`session::ScreenCaptureSession::stop`).
pub(super) struct Handover {
    pub events: Vec<InteractionEvent>,
    pub strokes: Vec<Stroke>,
}

/// Ends a running input recorder and returns its events.
type Stopper = Box<dyn FnOnce() -> Vec<InteractionEvent> + Send>;

/// Where a capture's input goes once `finish_session` has it.
pub(super) type EventSlot = Arc<Mutex<Option<Handover>>>;

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
    /// Finished ink strokes and pins, drawn by the input recorder (`crate::ink::InkTracker`).
    pub strokes: Arc<Mutex<Vec<Stroke>>>,
    /// The stroke ⇧ is drawing now.
    pub live_ink: Arc<Mutex<Option<Stroke>>>,
    /// ⇧ is held and drawing (the overlay's tip dot).
    pub shift_held: Arc<AtomicBool>,
    /// The pointer in global points as of the last look while ⇧ was held.
    pub pointer_screen: Arc<Mutex<[f32; 2]>>,
    /// Rings the ink overlay's loop when the ink changes; nothing until an overlay attaches.
    pub ink_wake: Arc<Wake>,
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
    /// The modifiers whose release ends the hold of the last accepted shortcut press (0: a tap).
    hold_mods: AtomicU64,
    /// Lets a key press seen by both the listener and the bind act once.
    press_gate: Mutex<(Instant, PressGate)>,
    input: Mutex<Option<Stopper>>,
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
            strokes: Arc::new(Mutex::new(Vec::new())),
            live_ink: Arc::new(Mutex::new(None)),
            shift_held: Arc::new(AtomicBool::new(false)),
            pointer_screen: Arc::new(Mutex::new([0.0, 0.0])),
            ink_wake: Arc::new(Wake::default()),
            excluded_window_ids: Arc::new(Mutex::new(Vec::new())),
            passthrough_window_ids: Mutex::new(Vec::new()),
            window_log_request: Arc::new(AtomicBool::new(false)),
            start_glow: AtomicBool::new(false),
            ui_sender,
            capture_mode: Mutex::new(CaptureMode::Display),
            picker_rect: Mutex::new(None),
            escape: AtomicBool::new(false),
            hold_mods: AtomicU64::new(0),
            press_gate: Mutex::new((Instant::now(), PressGate::default())),
            input: Mutex::new(None),
            session_events: Mutex::new(None),
        })
    }

    /// The ink's shared state, for the input recorder and the overlay.
    pub fn ink_state(&self) -> InkState {
        InkState {
            strokes: Arc::clone(&self.strokes),
            live: Arc::clone(&self.live_ink),
            shift_held: Arc::clone(&self.shift_held),
            pointer: Arc::clone(&self.pointer_screen),
            wake: Arc::clone(&self.ink_wake),
        }
    }

    /// The record shortcut, from wherever it was seen.
    pub fn emit_capture_shortcut(&self) {
        let _ = self.ui_sender.try_send(LiveOverlayEvent::CaptureShortcut);
    }

    /// The record shortcut as `source` saw it; the second sighting of one press is dropped.
    pub fn press_shortcut_from(&self, source: PressSource) {
        let accepted = {
            let mut gate = self
                .press_gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let now_ms = u64::try_from(gate.0.elapsed().as_millis()).unwrap_or(u64::MAX);
            gate.1.accept(source, now_ms)
        };
        if accepted {
            let hold = super::shortcut::hold_modifiers(
                source,
                super::modifier_flags(),
                &self
                    .shortcut
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .label,
            );
            self.hold_mods.store(hold, Ordering::Release);
            self.emit_capture_shortcut();
        }
    }

    /// The hold of the shortcut press being handled (`shortcut::hold_modifiers`).
    pub fn take_hold_mods(&self) -> u64 {
        self.hold_mods.swap(0, Ordering::AcqRel)
    }

    /// Esc went down, in whichever app (`linux::hotkey`).
    pub fn note_escape(&self) {
        self.escape.store(true, Ordering::Release);
    }

    pub fn matches_shortcut(
        &self,
        key_code: u16,
        cmd: bool,
        ctrl: bool,
        alt: bool,
        shift: bool,
    ) -> bool {
        self.shortcut
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .matches(key_code, cmd, ctrl, alt, shift)
    }

    pub fn set_capture_mode(&self, mode: CaptureMode) {
        *self
            .capture_mode
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = mode;
    }

    /// The picker opened: no capture is running.
    pub fn begin_picker_session(&self) {
        self.selecting.store(true, Ordering::Release);
        self.escape.store(false, Ordering::Release);
    }

    /// What the picker highlights. Nothing draws it from here: the GPUI picker paints its own
    /// dim from the same state (`picker_dim`).
    pub fn set_picker_rect(&self, rect: Option<([f32; 4], u8)>) {
        *self
            .picker_rect
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = rect;
    }

    /// A capture of `frame` starts now: the clock restarts and the input recorder starts.
    pub fn reset_for_session(&self, frame: TargetFrame) {
        self.begin_session(frame, |started_at| {
            let recorder = InputRecorder::start(
                frame,
                started_at,
                Arc::clone(&self.window_log_request),
                Some(self.ink_state()),
            );
            Box::new(move || recorder.stop())
        });
    }

    /// `reset_for_session` with the recorder's start given (`start` gets the capture clock's
    /// zero and returns how to stop what it started).
    fn begin_session(&self, frame: TargetFrame, start: impl FnOnce(Instant) -> Stopper) {
        // A recorder left from a capture that never finished stops first: its thread is still
        // writing the shared ink, which the new capture must start without.
        let old = self
            .input
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(stop) = old {
            let _ = stop();
        }
        let started_at = Instant::now();
        *self
            .started_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = started_at;
        *self
            .target_frame
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = frame;
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.strokes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        *self
            .live_ink
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.shift_held.store(false, Ordering::Release);
        self.selecting.store(false, Ordering::Release);
        self.active.store(true, Ordering::Release);
        *self.input.lock().unwrap_or_else(PoisonError::into_inner) = Some(start(started_at));
    }

    /// The screen session that will want this capture's events.
    pub(super) fn hand_events_to(&self, slot: EventSlot) {
        *self
            .session_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(slot);
    }

    /// Esc went down since the last look (`note_escape`).
    pub fn take_escape(&self) -> bool {
        self.escape.swap(false, Ordering::AcqRel)
    }

    /// Ends the capture's input recording: its events and the ink strokes (both also handed to
    /// the screen session, and the strokes cleared from the shared state).
    pub fn finish_session(&self) -> (Vec<InteractionEvent>, Vec<Stroke>) {
        self.active.store(false, Ordering::Release);
        let recorder = self
            .input
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        // The recorder's thread ends first: it keeps the stroke being drawn.
        let events = recorder.map(|stop| stop()).unwrap_or_default();
        *self
            .live_ink
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.shift_held.store(false, Ordering::Release);
        let strokes = std::mem::take(
            &mut *self.strokes.lock().unwrap_or_else(PoisonError::into_inner),
        );
        *self.events.lock().unwrap_or_else(PoisonError::into_inner) = events.clone();
        if let Some(slot) = self
            .session_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Handover {
                events: events.clone(),
                strokes: strokes.clone(),
            });
        }
        (events, strokes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(t0: u64, pin: Option<u32>) -> Stroke {
        Stroke {
            t0,
            t1: t0 + 100,
            fw: 1920,
            fh: 1080,
            on: None,
            pin,
            laser: false,
            pts: vec![100, 100, 0, 200, 200, 50],
        }
    }

    fn frame() -> TargetFrame {
        TargetFrame {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
            scale: 1.0,
        }
    }

    #[test]
    fn finishing_a_session_returns_its_strokes_and_hands_them_to_the_screen_session() {
        let (sender, _receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(sender);
        let slot: EventSlot = Arc::new(Mutex::new(None));
        telemetry.hand_events_to(Arc::clone(&slot));
        telemetry.strokes.lock().unwrap().push(stroke(10, None));
        telemetry.strokes.lock().unwrap().push(stroke(20, Some(1)));
        *telemetry.live_ink.lock().unwrap() = Some(stroke(30, None));
        telemetry.shift_held.store(true, Ordering::Release);

        let (events, strokes) = telemetry.finish_session();
        assert!(events.is_empty());
        assert_eq!(strokes.len(), 2);
        assert_eq!(strokes[1].pin, Some(1));
        // The shared state is clean for whatever draws next, and the screen session has them.
        assert!(telemetry.strokes.lock().unwrap().is_empty());
        assert!(telemetry.live_ink.lock().unwrap().is_none());
        assert!(!telemetry.shift_held.load(Ordering::Acquire));
        let handed = slot.lock().unwrap().take().expect("handed over");
        assert_eq!(handed.strokes, strokes);
    }

    /// A recorder that has stopped and left nothing.
    fn idle_recorder() -> Stopper {
        Box::new(Vec::new)
    }

    #[test]
    fn a_new_session_starts_without_the_last_ones_ink() {
        let (sender, _receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(sender);
        telemetry.strokes.lock().unwrap().push(stroke(10, None));
        *telemetry.live_ink.lock().unwrap() = Some(stroke(30, None));
        telemetry.shift_held.store(true, Ordering::Release);
        telemetry.begin_session(frame(), |_| idle_recorder());
        assert!(telemetry.strokes.lock().unwrap().is_empty());
        assert!(telemetry.live_ink.lock().unwrap().is_none());
        assert!(!telemetry.shift_held.load(Ordering::Acquire));
        assert!(telemetry.active.load(Ordering::Acquire));
        let (_, strokes) = telemetry.finish_session();
        assert!(strokes.is_empty());
    }

    #[test]
    fn a_recorder_left_running_is_stopped_before_the_ink_is_cleared() {
        let (sender, _receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(sender);
        // The old recorder's thread writes the stroke it was drawing as it stops.
        let (strokes, live) = (Arc::clone(&telemetry.strokes), Arc::clone(&telemetry.live_ink));
        telemetry.begin_session(frame(), move |_| {
            Box::new(move || {
                strokes.lock().unwrap().push(stroke(10, None));
                *live.lock().unwrap() = Some(stroke(20, None));
                Vec::new()
            })
        });
        telemetry.begin_session(frame(), |_| idle_recorder());
        assert!(telemetry.strokes.lock().unwrap().is_empty());
        assert!(telemetry.live_ink.lock().unwrap().is_none());
    }

    #[test]
    fn the_ink_state_shares_what_the_telemetry_shows() {
        let (sender, _receiver) = flume::unbounded();
        let telemetry = SharedCaptureTelemetry::new(sender);
        let state = telemetry.ink_state();
        state.strokes.lock().unwrap().push(stroke(1, None));
        assert_eq!(telemetry.strokes.lock().unwrap().len(), 1);
        state.shift_held.store(true, Ordering::Release);
        assert!(telemetry.shift_held.load(Ordering::Acquire));
    }
}
