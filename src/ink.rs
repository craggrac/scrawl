//! Ink: what a held ⇧ draws while recording, and the rules that decide which of it is kept. The
//! helpers are shared (macOS' event poller and the Linux input thread decide the same way); on
//! Linux `InkTracker` is the whole state machine that `run_global_hid_poller` and
//! `shift_down`/`shift_up` are on macOS: ⇧ + move draws a stroke, ⇧-click drops a numbered pin.

use crate::model::{SCALE, Stroke};

/// A press that moves less than this (points) before it's let go is a click, not a drag.
pub const CLICK_SLOP_PT: f32 = 12.0;
/// A ⇧-press held still for longer than this drops no pin.
pub const PIN_MAX_PRESS_MS: u64 = 1_000;
/// Ink spanning less than this (points) is a stationary ⇧ tap, not a stroke.
const MIN_INK_SPAN_PT: f32 = 6.0;

/// Whether a ⇧-press (`t`, global `x`, `y`) let go at `up` is a ⇧-click, which drops a pin.
pub fn is_pin_click(down: (u64, f64, f64), up: (u64, f64, f64)) -> bool {
    up.0.saturating_sub(down.0) <= PIN_MAX_PRESS_MS
        && ((up.1 - down.1).hypot(up.2 - down.2) as f32) < CLICK_SLOP_PT
}

/// How far (points) the ink reaches from where it started.
fn ink_span_pt(stroke: &Stroke) -> f32 {
    let Some(first) = stroke.pts.get(0..2) else {
        return 0.0;
    };
    stroke
        .pts
        .chunks_exact(3)
        .map(|p| {
            let dx = (p[0] - first[0]) as f32 / SCALE as f32 * stroke.fw as f32;
            let dy = (p[1] - first[1]) as f32 / SCALE as f32 * stroke.fh as f32;
            dx.hypot(dy)
        })
        .fold(0.0_f32, f32::max)
}

/// Whether finished ink is worth keeping as a stroke.
pub fn is_real_ink(stroke: &Stroke) -> bool {
    stroke.pts.len() >= 6 && ink_span_pt(stroke) >= MIN_INK_SPAN_PT
}

/// The part of live ink drawn before `t_ms`.
pub fn ink_before(mut stroke: Stroke, t_ms: u64) -> Stroke {
    let t0 = stroke.t0;
    let at = |p: &[i32]| t0.saturating_add(p[2].max(0) as u64);
    let keep = stroke
        .pts
        .chunks_exact(3)
        .take_while(|p| at(p) < t_ms)
        .count();
    stroke.pts.truncate(keep * 3);
    if let Some(last) = stroke.pts.chunks_exact(3).last() {
        stroke.t1 = at(last);
    }
    stroke
}

#[cfg(target_os = "linux")]
pub use tracker::{InkState, InkTracker, Wake};

#[cfg(target_os = "linux")]
mod tracker {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use super::{is_pin_click, is_real_ink, ink_before};
    use crate::linux::input::{
        BTN_LEFT, CursorSource, EV_KEY, EV_SYN, Modifiers, RawEvent, SYN_DROPPED,
    };
    use crate::model::{SCALE, Stroke, TargetFrame};

    /// A point is added to live ink only when this far (points) from the last one.
    const MIN_POINT_GAP_PT: f32 = 1.5;
    /// Points in one stroke; past it a stroke stops growing.
    const MAX_STROKE_POINTS: usize = 2_400;

    /// An `eventfd` the overlay waits on next to its Wayland connection; whoever changes the ink
    /// writes it. It does nothing until an overlay attaches one.
    #[derive(Default)]
    pub struct Wake(Mutex<Option<Arc<OwnedFd>>>);

    impl Wake {
        /// Makes the eventfd to wait on. A second call replaces the first.
        pub fn attach(&self) -> std::io::Result<Arc<OwnedFd>> {
            let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let fd = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&fd));
            Ok(fd)
        }

        /// The overlay that attached `fd` is going away. A newer overlay's stays.
        pub fn detach(&self, fd: &Arc<OwnedFd>) {
            let mut current = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, fd)) {
                *current = None;
            }
        }

        pub fn notify(&self) {
            if let Some(fd) = &*self.0.lock().unwrap_or_else(PoisonError::into_inner) {
                let one = 1_u64;
                // A full counter (EAGAIN) already means "wake up".
                unsafe {
                    libc::write(fd.as_raw_fd(), (&raw const one).cast(), 8);
                }
            }
        }
    }

    /// The ink of a capture, shared by the input thread that draws it, the overlay that shows
    /// it and the UI (undo, the count of marks). Cloning shares everything.
    #[derive(Clone, Default)]
    pub struct InkState {
        /// Finished strokes and pins, oldest first.
        pub strokes: Arc<Mutex<Vec<Stroke>>>,
        /// The stroke ⇧ is drawing now.
        pub live: Arc<Mutex<Option<Stroke>>>,
        /// ⇧ is held and drawing (the overlay's tip dot).
        pub shift_held: Arc<AtomicBool>,
        /// The pointer in global points while ⇧ is held.
        pub pointer: Arc<Mutex<[f32; 2]>>,
        pub wake: Arc<Wake>,
    }

    /// Turns evdev key events and the cursor into ink, as macOS' HID poller does: while only ⇧
    /// is held and the pointer is inside the target it draws a stroke; ⇧ with ctrl, alt or
    /// super draws nothing until ⇧ is released; releasing ⇧ keeps the stroke if it is real ink;
    /// a ⇧-press let go on the spot is a numbered pin. Holds no device (`Gestures`' rig).
    pub struct InkTracker {
        frame: TargetFrame,
        state: InkState,
        mods: Modifiers,
        /// ⇧ went down with ctrl, alt or super: no ink until it is released.
        await_shift_release: bool,
        /// Only ⇧ was held at the last look.
        drawing: bool,
        /// A ⇧-press inside the target: when, and where (global points).
        press: Option<(u64, f64, f64)>,
        snapshot: bool,
        /// When the pointer was last asked for.
        last_look: Option<u64>,
    }

    impl InkTracker {
        pub fn new(frame: TargetFrame, state: InkState) -> Self {
            Self {
                frame,
                state,
                mods: Modifiers::default(),
                await_shift_release: false,
                drawing: false,
                press: None,
                snapshot: false,
                last_look: None,
            }
        }

        /// Forgets held modifiers and a pending press (a dropped device may have lost their
        /// release). Ink being drawn is ended by the next look.
        pub fn reset(&mut self) {
            self.mods = Modifiers::default();
            self.press = None;
        }

        /// Whether the window log should snapshot now: ink began or a pin dropped since the
        /// last call.
        pub fn take_snapshot_request(&mut self) -> bool {
            std::mem::take(&mut self.snapshot)
        }

        /// Only ⇧ is held, as of the last look: the pointer wants sampling often.
        pub fn drawing(&self) -> bool {
            self.drawing
        }

        /// ⇧ is held and drawing inside the target.
        #[cfg(test)]
        pub fn shift_held(&self) -> bool {
            self.state.shift_held.load(Ordering::Acquire)
        }

        /// The pointer (global points) as of the last look while ⇧ was held.
        #[cfg(test)]
        pub fn pointer(&self) -> [f32; 2] {
            *self
                .state
                .pointer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        }

        fn contains(&self, x: f64, y: f64) -> bool {
            let f = self.frame;
            f.width >= 16.0
                && f.height >= 16.0
                && x >= f.x
                && x <= f.x + f.width
                && y >= f.y
                && y <= f.y + f.height
        }

        /// Like macOS `normalize_point`.
        fn normalize(&self, x: f64, y: f64) -> (f32, f32, u32, u32) {
            let f = self.frame;
            (
                ((x - f.x) / f.width.max(1.0)).clamp(0.0, 1.0) as f32,
                ((y - f.y) / f.height.max(1.0)).clamp(0.0, 1.0) as f32,
                f.width.round().max(1.0) as u32,
                f.height.round().max(1.0) as u32,
            )
        }

        /// Feeds one raw event read at `t` ms.
        pub fn handle(&mut self, t: u64, raw: RawEvent, cursor: CursorSource<'_>) {
            match raw.kind {
                EV_KEY if raw.code == BTN_LEFT => match raw.value {
                    1 => self.press_down(t, cursor),
                    0 => self.press_up(t, cursor),
                    _ => {}
                },
                EV_KEY => {
                    if self.mods.set(raw.code, raw.value != 0) {
                        self.look(t, cursor);
                    }
                }
                EV_SYN if raw.code == SYN_DROPPED => self.reset(),
                _ => {}
            }
        }

        /// Looks at the pointer: the 8 ms poll while ⇧ is held (the thread wakes for every
        /// event of a fast mouse, but the pointer is only asked once per period, as macOS'
        /// poller sleeps), and at once when a modifier changes (`handle`).
        pub fn tick(&mut self, t: u64, cursor: CursorSource<'_>) {
            let period = crate::linux::input::SamplePacer::INK_MS as u64;
            if self
                .last_look
                .is_some_and(|last| t.saturating_sub(last) < period)
            {
                return;
            }
            self.look(t, cursor);
        }

        /// The capture ends: ink being drawn is kept.
        pub fn finish(&mut self) {
            self.press = None;
            self.drawing = false;
            self.state.shift_held.store(false, Ordering::Release);
            let live = self
                .state
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(stroke) = live
                && stroke.pts.len() >= 3
            {
                self.state
                    .strokes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(stroke);
            }
            self.state.wake.notify();
        }

        /// The left button went down with only ⇧ held, inside the target: a pin if it comes up
        /// as a click.
        fn press_down(&mut self, t: u64, cursor: CursorSource<'_>) {
            self.press = None;
            let only_shift =
                self.mods.shift() && !(self.mods.ctrl() || self.mods.alt() || self.mods.meta());
            if !only_shift {
                return;
            }
            if let Some((x, y)) = cursor()
                && self.contains(x, y)
            {
                self.press = Some((t, x, y));
            }
        }

        /// The left button came up. A ⇧-click drops the next numbered pin where it went down; a
        /// ⇧-drag is left to the ink being drawn.
        fn press_up(&mut self, t: u64, cursor: CursorSource<'_>) {
            let Some((t0, x0, y0)) = self.press.take() else {
                return;
            };
            let Some((x, y)) = cursor() else { return };
            if !is_pin_click((t0, x0, y0), (t, x, y)) {
                return;
            }
            let (nx, ny, fw, fh) = self.normalize(x0, y0);
            // ⇧ draws as it's held, so the live ink ends in the click's jitter: what was drawn
            // before the press is kept, and further ink starts a new stroke.
            let earlier = self
                .state
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .map(|ink| ink_before(ink, t0))
                .filter(is_real_ink);
            let mut strokes = self
                .state
                .strokes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            strokes.extend(earlier);
            let number = strokes.iter().filter(|s| s.pin.is_some()).count() as u32 + 1;
            strokes.push(Stroke {
                t0,
                t1: t,
                fw,
                fh,
                on: None,
                pin: Some(number),
                laser: false,
                pts: vec![
                    (nx * SCALE as f32).round() as i32,
                    (ny * SCALE as f32).round() as i32,
                    0,
                ],
            });
            drop(strokes);
            self.snapshot = true;
            self.state.wake.notify();
        }

        /// One look at the modifiers and the pointer: the body of macOS' poller loop.
        fn look(&mut self, t: u64, cursor: CursorSource<'_>) {
            let shift = self.mods.shift();
            let others = self.mods.ctrl() || self.mods.alt() || self.mods.meta();
            if !shift {
                self.await_shift_release = false;
            } else if others {
                self.await_shift_release = true;
            }
            let only_shift = shift && !others && !self.await_shift_release;
            if only_shift {
                self.last_look = Some(t);
                let Some((x, y)) = cursor() else { return };
                *self
                    .state
                    .pointer
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = [x as f32, y as f32];
                if self.contains(x, y) {
                    self.state.shift_held.store(true, Ordering::Release);
                    self.draw(t, x, y);
                }
                self.state.wake.notify();
            } else if self.drawing {
                self.state.shift_held.store(false, Ordering::Release);
                let live = self
                    .state
                    .live
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                // A stationary ⇧ tap (a capital letter typed) is not a stroke.
                if let Some(stroke) = live.filter(is_real_ink) {
                    self.state
                        .strokes
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(stroke);
                }
                self.state.wake.notify();
            }
            self.drawing = only_shift;
        }

        /// Adds the pointer's point to the live stroke.
        fn draw(&mut self, t: u64, x: f64, y: f64) {
            let (nx, ny, fw, fh) = self.normalize(x, y);
            let sx = (nx * SCALE as f32).round() as i32;
            let sy = (ny * SCALE as f32).round() as i32;
            let mut live = self
                .state
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if live.is_none() {
                self.snapshot = true;
            }
            let stroke = live.get_or_insert_with(|| Stroke {
                t0: t,
                t1: t,
                fw,
                fh,
                on: None,
                pin: None,
                laser: false,
                pts: Vec::new(),
            });
            let dt = t.saturating_sub(stroke.t0) as i32;
            let far_enough = stroke.pts.chunks_exact(3).last().is_none_or(|prev| {
                let dx = (sx - prev[0]) as f32 / SCALE as f32 * fw as f32;
                let dy = (sy - prev[1]) as f32 / SCALE as f32 * fh as f32;
                dx.hypot(dy) >= MIN_POINT_GAP_PT
            });
            if far_enough && stroke.pts.len() / 3 < MAX_STROKE_POINTS {
                stroke.pts.extend_from_slice(&[sx, sy, dt]);
                stroke.t1 = t;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::cell::Cell;

        use super::*;

        const SHIFT: u16 = 42;
        const RIGHT_SHIFT: u16 = 54;
        const CTRL: u16 = 29;
        const ALT: u16 = 56;
        const SUPER: u16 = 125;

        fn frame() -> TargetFrame {
            TargetFrame {
                x: 100.0,
                y: 50.0,
                width: 1000.0,
                height: 500.0,
                scale: 1.0,
            }
        }

        fn key(code: u16, value: i32) -> RawEvent {
            RawEvent {
                kind: EV_KEY,
                code,
                value,
            }
        }

        /// Feeds events with a cursor the test moves between them.
        struct Rig {
            tracker: InkTracker,
            state: InkState,
            at: Cell<Option<(f64, f64)>>,
        }

        impl Rig {
            fn new() -> Self {
                let state = InkState::default();
                Self {
                    tracker: InkTracker::new(frame(), state.clone()),
                    state,
                    at: Cell::new(Some((600.0, 300.0))),
                }
            }

            fn feed(&mut self, t: u64, raw: RawEvent) {
                let at = &self.at;
                let mut cursor = || at.get();
                self.tracker.handle(t, raw, &mut cursor);
            }

            fn tick(&mut self, t: u64) {
                let at = &self.at;
                let mut cursor = || at.get();
                self.tracker.tick(t, &mut cursor);
            }

            /// The pointer moves to `to` and the poll looks at `t`.
            fn move_to(&mut self, t: u64, to: (f64, f64)) {
                self.at.set(Some(to));
                self.tick(t);
            }

            fn live(&self) -> Option<Stroke> {
                self.state.live.lock().unwrap().clone()
            }

            fn strokes(&self) -> Vec<Stroke> {
                self.state.strokes.lock().unwrap().clone()
            }
        }

        /// A line along y = 300 from x = 600, one point per 10 points moved, 8 ms apart.
        fn draw_line(rig: &mut Rig, t0: u64, points: u32) -> u64 {
            let mut t = t0;
            for i in 0..points {
                rig.move_to(t, (600.0 + f64::from(i) * 10.0, 300.0));
                t += 8;
            }
            t
        }

        #[test]
        fn shift_and_movement_inside_draw_a_stroke_in_scale_units_with_the_time_since_it_began() {
            let mut rig = Rig::new();
            rig.feed(1_000, key(SHIFT, 1));
            rig.move_to(1_008, (600.0, 300.0));
            rig.move_to(1_016, (700.0, 300.0));
            rig.move_to(1_040, (700.0, 425.0));
            let live = rig.live().expect("a live stroke");
            assert_eq!((live.t0, live.t1), (1_000, 1_040));
            assert_eq!((live.fw, live.fh), (1_000, 500));
            assert_eq!(live.pin, None);
            // (600, 300) in a 1000x500 frame at (100, 50) is (0.5, 0.5); the first look was at
            // the key press itself.
            let s = crate::model::SCALE;
            assert_eq!(
                live.pts,
                vec![
                    s / 2, s / 2, 0,
                    s * 6 / 10, s / 2, 16,
                    s * 6 / 10, s * 3 / 4, 40,
                ]
            );
            assert!(rig.tracker.shift_held());
            assert_eq!(rig.tracker.pointer(), [700.0, 425.0]);
        }

        #[test]
        fn the_pointer_is_looked_at_once_per_poll_period_however_often_the_thread_wakes() {
            // A 1 kHz mouse wakes the input thread every millisecond.
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            let asked = Cell::new(0_u32);
            for t in 1..=100_u64 {
                // 20 points on every step, so every look that happens adds a point.
                let at = (110.0 + (t % 2) as f64 * 20.0, 60.0 + t as f64 * 4.0);
                rig.at.set(Some(at));
                let at = &rig.at;
                let mut cursor = || {
                    asked.set(asked.get() + 1);
                    at.get()
                };
                rig.tracker.tick(t, &mut cursor);
            }
            // Every 8 ms: 12 looks in 100 ms, and the press's own.
            assert!((11..=13).contains(&asked.get()), "asked {}", asked.get());
            let points = rig.live().unwrap().pts.len() / 3;
            assert!((12..=14).contains(&points), "{points} points");
        }

        #[test]
        fn a_modifier_change_looks_at_once_inside_the_period() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            draw_line(&mut rig, 8, 8);
            // 1 ms after the last look: ⇧ up still ends the ink then and there.
            rig.feed(8 * 8 + 9, key(SHIFT, 0));
            assert!(rig.live().is_none());
            assert_eq!(rig.strokes().len(), 1);
        }

        #[test]
        fn a_point_under_a_point_and_a_half_from_the_last_is_not_added() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            rig.move_to(8, (610.0, 300.0));
            rig.move_to(16, (610.5, 300.5));
            rig.move_to(24, (611.0, 300.0));
            let points = rig.live().unwrap().pts.len() / 3;
            // The press look (600), 610 and then 611 (1.0 from 610) is too close, too.
            assert_eq!(points, 2);
            rig.move_to(32, (611.6, 300.0));
            assert_eq!(rig.live().unwrap().pts.len() / 3, 3);
        }

        #[test]
        fn a_stroke_stops_at_two_thousand_four_hundred_points() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            for i in 0..2_600_u32 {
                // Every step is 20 points sideways, so each one is far enough.
                let at = (110.0 + f64::from(i % 2) * 20.0, 60.0 + f64::from(i % 400));
                rig.move_to(u64::from(i) * 8, at);
            }
            assert_eq!(rig.live().unwrap().pts.len() / 3, 2_400);
        }

        #[test]
        fn leaving_the_target_adds_nothing_and_ends_nothing() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            let t = draw_line(&mut rig, 8, 6);
            let before = rig.live().unwrap();
            rig.move_to(t, (50.0, 300.0));
            rig.move_to(t + 8, (60.0, 320.0));
            assert_eq!(rig.live().unwrap(), before);
            assert!(rig.strokes().is_empty());
            // The pointer is still followed for the tip dot.
            assert_eq!(rig.tracker.pointer(), [60.0, 320.0]);
            // Back inside, the same stroke goes on.
            rig.move_to(t + 16, (700.0, 400.0));
            assert_eq!(rig.live().unwrap().pts.len(), before.pts.len() + 3);
            assert!(rig.strokes().is_empty());
        }

        #[test]
        fn releasing_shift_keeps_real_ink_and_a_stationary_tap_leaves_none() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            let t = draw_line(&mut rig, 8, 8);
            rig.feed(t, key(SHIFT, 0));
            assert!(rig.live().is_none());
            assert!(!rig.tracker.shift_held());
            let strokes = rig.strokes();
            assert_eq!(strokes.len(), 1);
            assert_eq!(strokes[0].pts.len(), 8 * 3);

            // A capital letter: ⇧ down and up on the spot.
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            rig.move_to(8, (600.0, 300.0));
            rig.move_to(16, (601.0, 300.0));
            rig.feed(60, key(SHIFT, 0));
            assert!(rig.strokes().is_empty());
            assert!(rig.live().is_none());
        }

        #[test]
        fn shift_with_ctrl_alt_or_super_draws_nothing_until_shift_is_let_go() {
            for other in [CTRL, ALT, SUPER] {
                let mut rig = Rig::new();
                rig.feed(0, key(other, 1));
                rig.feed(5, key(SHIFT, 1));
                draw_line(&mut rig, 8, 6);
                assert!(rig.live().is_none(), "{other}");
                // Letting go of the other key is not enough...
                rig.feed(100, key(other, 0));
                draw_line(&mut rig, 108, 6);
                assert!(rig.live().is_none(), "{other} released, shift still down");
                // ...only ⇧'s own release (and a new press) is.
                rig.feed(200, key(SHIFT, 0));
                rig.feed(210, key(SHIFT, 1));
                draw_line(&mut rig, 218, 6);
                assert!(rig.live().is_some(), "{other}");
            }
        }

        #[test]
        fn a_modifier_added_to_shift_ends_the_stroke_being_drawn() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            let t = draw_line(&mut rig, 8, 8);
            rig.feed(t, key(CTRL, 1));
            assert!(rig.live().is_none());
            assert_eq!(rig.strokes().len(), 1);
            draw_line(&mut rig, t + 8, 6);
            assert_eq!(rig.strokes().len(), 1);
        }

        #[test]
        fn either_shift_key_counts_and_one_still_down_keeps_the_ink_going() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            rig.feed(2, key(RIGHT_SHIFT, 1));
            rig.feed(4, key(SHIFT, 0));
            draw_line(&mut rig, 8, 6);
            assert_eq!(rig.live().unwrap().pts.len() / 3, 6);
        }

        #[test]
        fn a_shift_click_drops_a_numbered_pin_and_keeps_the_ink_before_it() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            // A line, then the click's own jitter at its end.
            let t = draw_line(&mut rig, 8, 10);
            rig.at.set(Some((690.0, 300.0)));
            rig.feed(t + 100, key(BTN_LEFT, 1));
            rig.move_to(t + 120, (692.0, 301.0));
            rig.feed(t + 180, key(BTN_LEFT, 0));
            let strokes = rig.strokes();
            assert_eq!(strokes.len(), 2, "{strokes:?}");
            // The earlier ink stops before the press...
            assert_eq!(strokes[0].pin, None);
            assert!(strokes[0].t1 < t + 100);
            assert_eq!(strokes[0].pts.len() / 3, 10);
            // ...and the pin is where the press went down, numbered 1.
            let pin = &strokes[1];
            assert_eq!(pin.pin, Some(1));
            assert_eq!((pin.t0, pin.t1), (t + 100, t + 180));
            let s = crate::model::SCALE;
            assert_eq!(pin.pts, vec![s * 59 / 100, s / 2, 0]);
            assert!(rig.live().is_none(), "the jitter isn't a stroke");
            assert!(rig.tracker.take_snapshot_request());
        }

        #[test]
        fn pins_are_numbered_from_the_pins_so_far_and_a_still_shift_click_leaves_no_ink() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            for (i, x) in [200.0, 400.0, 800.0].into_iter().enumerate() {
                let t = 1_000 * (i as u64 + 1);
                rig.at.set(Some((x, 200.0)));
                rig.feed(t, key(BTN_LEFT, 1));
                rig.feed(t + 60, key(BTN_LEFT, 0));
            }
            let pins: Vec<_> = rig.strokes().iter().map(|s| s.pin).collect();
            assert_eq!(pins, vec![Some(1), Some(2), Some(3)]);
            // The poll at each click saw one point at most: no stroke of its own.
            assert!(rig.strokes().iter().all(|s| s.pts.len() == 3));
            // One pin undone in the UI: the next is numbered after the ones left.
            rig.state.strokes.lock().unwrap().remove(0);
            rig.feed(5_000, key(BTN_LEFT, 1));
            rig.feed(5_050, key(BTN_LEFT, 0));
            assert_eq!(rig.strokes().last().unwrap().pin, Some(3));
        }

        #[test]
        fn a_press_held_long_or_dragged_far_is_ink_not_a_pin() {
            for (up_at, up_to) in [(1_300_u64, (600.0, 300.0)), (200, (640.0, 300.0))] {
                let mut rig = Rig::new();
                rig.feed(0, key(SHIFT, 1));
                rig.feed(100, key(BTN_LEFT, 1));
                rig.move_to(100 + up_at / 2, (620.0, 300.0));
                rig.at.set(Some(up_to));
                rig.feed(100 + up_at, key(BTN_LEFT, 0));
                assert!(
                    rig.strokes().iter().all(|s| s.pin.is_none()),
                    "{up_at} {up_to:?}"
                );
                // ...and the ink is still being drawn.
                assert!(rig.live().is_some());
            }
        }

        #[test]
        fn a_press_outside_the_target_or_with_another_modifier_drops_no_pin() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            rig.at.set(Some((40.0, 40.0)));
            rig.feed(100, key(BTN_LEFT, 1));
            rig.feed(160, key(BTN_LEFT, 0));
            assert!(rig.strokes().is_empty());

            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            rig.feed(10, key(CTRL, 1));
            rig.feed(100, key(BTN_LEFT, 1));
            rig.feed(160, key(BTN_LEFT, 0));
            assert!(rig.strokes().is_empty());

            // A release with no press (the press was a plain click) is nothing.
            let mut rig = Rig::new();
            rig.feed(100, key(BTN_LEFT, 0));
            assert!(rig.strokes().is_empty());
        }

        #[test]
        fn finishing_keeps_the_stroke_being_drawn_and_clears_the_tip_dot() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            draw_line(&mut rig, 8, 3);
            rig.tracker.finish();
            assert!(rig.live().is_none());
            assert!(!rig.tracker.shift_held());
            assert_eq!(rig.strokes().len(), 1);
            // Nothing drawn, nothing kept.
            let mut rig = Rig::new();
            rig.tracker.finish();
            assert!(rig.strokes().is_empty());
        }

        #[test]
        fn a_pointer_nobody_can_read_draws_nothing_and_a_reset_forgets_modifiers() {
            let mut rig = Rig::new();
            rig.at.set(None);
            rig.feed(0, key(SHIFT, 1));
            rig.tick(8);
            assert!(rig.live().is_none());
            rig.at.set(Some((600.0, 300.0)));
            rig.tick(16);
            assert!(rig.live().is_some());
            // A dropped event: the ⇧ release may have been lost, so ⇧ isn't taken as held and
            // the ink ends at the next look.
            rig.feed(
                30,
                RawEvent {
                    kind: EV_SYN,
                    code: SYN_DROPPED,
                    value: 0,
                },
            );
            rig.move_to(40, (700.0, 300.0));
            assert!(rig.live().is_none());
            assert!(!rig.tracker.shift_held());
        }

        #[test]
        fn a_snapshot_is_asked_for_when_ink_begins() {
            let mut rig = Rig::new();
            rig.feed(0, key(SHIFT, 1));
            assert!(rig.tracker.take_snapshot_request());
            assert!(!rig.tracker.take_snapshot_request());
            draw_line(&mut rig, 8, 4);
            assert!(!rig.tracker.take_snapshot_request());
        }

        #[test]
        fn the_wake_does_nothing_until_an_overlay_attaches_and_then_counts_changes() {
            let state = InkState::default();
            state.wake.notify();
            let fd = state.wake.attach().unwrap();
            state.wake.notify();
            state.wake.notify();
            let mut count = 0_u64;
            let read = unsafe { libc::read(fd.as_raw_fd(), (&raw mut count).cast(), 8) };
            assert_eq!((read, count), (8, 2));
            // An overlay that was replaced can't detach its successor.
            let newer = state.wake.attach().unwrap();
            state.wake.detach(&fd);
            state.wake.notify();
            let woken = unsafe { libc::read(newer.as_raw_fd(), (&raw mut count).cast(), 8) };
            assert_eq!((woken, count), (8, 1));
            state.wake.detach(&newer);
            state.wake.notify();
            // Nothing new once detached (the fd is still ours to poll, and empty).
            let again = unsafe { libc::read(newer.as_raw_fd(), (&raw mut count).cast(), 8) };
            assert_eq!(again, -1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_still_shift_press_drops_no_pin() {
        let down = (1_000, 10.0, 10.0);
        let slop = f64::from(CLICK_SLOP_PT);
        assert!(is_pin_click(down, (1_150, 14.0, 13.0)));
        assert!(is_pin_click(down, (1_000 + PIN_MAX_PRESS_MS, 10.0, 10.0)));
        assert!(!is_pin_click(down, (2_500, 10.0, 10.0)));
        assert!(!is_pin_click(down, (1_100, 10.0, 10.0 + slop)));
    }
}
