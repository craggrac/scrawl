//! Clicks, drags, scrolls and shortcuts from the evdev devices in `/dev/input`, turned into the
//! same `InteractionEvent`s the macOS event tap records.
//!
//! Devices are read passively (never grabbed). evdev carries no absolute pointer position on
//! Wayland, so the cursor is asked of Hyprland when a gesture needs it. Only key chords with
//! Ctrl or Super held are kept; ordinary typing is dropped as it is read.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::capture::scrawl_log;
use crate::model::{InteractionEvent, TargetFrame};

const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const KEY_A: u16 = 30;

/// A press that moves less than this (points) before it is let go is a click, not a drag.
const CLICK_SLOP_PT: f64 = 12.0;
/// Cursor sampling period while a button is held.
const DRAG_SAMPLE_MS: u64 = 30;
/// Wheel events closer together than this share one cursor lookup.
const SCROLL_BURST_MS: u64 = 300;

/// One `input_event` without its kernel timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

/// The cursor in global logical points, if Hyprland can say.
pub type CursorSource<'a> = &'a mut dyn FnMut() -> Option<(f64, f64)>;

struct Press {
    t: u64,
    x: f32,
    y: f32,
    right: bool,
    path: Vec<[f32; 2]>,
    last_sample: u64,
}

/// Turns raw events into interaction events. Holds no device, so it is testable with synthetic
/// events and a fake cursor.
pub struct Gestures {
    frame: TargetFrame,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
    press: Option<Press>,
    scroll_at: Option<(u64, f64, f64)>,
}

impl Gestures {
    pub fn new(frame: TargetFrame) -> Self {
        Self {
            frame,
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
            press: None,
            scroll_at: None,
        }
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

    /// Like macOS `normalize_point`: `(p - origin) / size`, clamped to 0..1.
    fn normalize(&self, x: f64, y: f64) -> [f32; 2] {
        [
            ((x - self.frame.x) / self.frame.width.max(1.0)).clamp(0.0, 1.0) as f32,
            ((y - self.frame.y) / self.frame.height.max(1.0)).clamp(0.0, 1.0) as f32,
        ]
    }

    /// Feeds one raw event read at `t` ms; returns the interactions it completes.
    pub fn handle(
        &mut self,
        t: u64,
        raw: RawEvent,
        cursor: CursorSource<'_>,
    ) -> Vec<InteractionEvent> {
        match raw.kind {
            EV_KEY => self.key(t, raw, cursor),
            EV_REL => self.rel(t, raw, cursor),
            _ => Vec::new(),
        }
    }

    fn key(&mut self, t: u64, raw: RawEvent, cursor: CursorSource<'_>) -> Vec<InteractionEvent> {
        let down = raw.value != 0;
        match raw.code {
            BTN_LEFT | BTN_RIGHT => {
                let right = raw.code == BTN_RIGHT;
                if raw.value == 1 {
                    self.pointer_down(t, right, cursor);
                    Vec::new()
                } else if raw.value == 0 {
                    self.pointer_up(t, right, cursor).into_iter().collect()
                } else {
                    Vec::new()
                }
            }
            29 | 97 => {
                self.ctrl = down;
                Vec::new()
            }
            56 | 100 => {
                self.alt = down;
                Vec::new()
            }
            42 | 54 => {
                self.shift = down;
                Vec::new()
            }
            125 | 126 => {
                self.meta = down;
                Vec::new()
            }
            code if raw.value == 1 && (self.ctrl || self.meta) => key_name(code)
                .map(|name| InteractionEvent::Key {
                    t,
                    shortcut: self.chord(name),
                })
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn chord(&self, name: &str) -> String {
        let mut chord = String::new();
        for (held, label) in [
            (self.ctrl, "Ctrl+"),
            (self.alt, "Alt+"),
            (self.shift, "Shift+"),
            (self.meta, "Super+"),
        ] {
            if held {
                chord.push_str(label);
            }
        }
        chord.push_str(name);
        chord
    }

    fn pointer_down(&mut self, t: u64, right: bool, cursor: CursorSource<'_>) {
        self.press = None;
        let Some((gx, gy)) = cursor() else { return };
        if !self.contains(gx, gy) {
            return;
        }
        let [x, y] = self.normalize(gx, gy);
        self.press = Some(Press {
            t,
            x,
            y,
            right,
            path: Vec::new(),
            last_sample: t,
        });
    }

    fn pointer_up(
        &mut self,
        t: u64,
        right: bool,
        cursor: CursorSource<'_>,
    ) -> Option<InteractionEvent> {
        if self.press.as_ref().is_none_or(|p| p.right != right) {
            return None;
        }
        let press = self.press.take()?;
        let end = cursor()
            .map(|(x, y)| self.normalize(x, y))
            .or_else(|| press.path.last().copied())
            .unwrap_or([press.x, press.y]);
        let dist = f64::from((end[0] - press.x) * self.frame.width.max(1.0) as f32)
            .hypot(f64::from((end[1] - press.y) * self.frame.height.max(1.0) as f32));
        Some(if dist < CLICK_SLOP_PT {
            InteractionEvent::Click {
                t: press.t,
                x: press.x,
                y: press.y,
                button: Some(if right { "right" } else { "left" }.into()),
                label: None,
                role: None,
            }
        } else {
            InteractionEvent::Drag {
                t: press.t,
                t1: t,
                x0: press.x,
                y0: press.y,
                x1: end[0],
                y1: end[1],
                label: None,
                button: None,
                path: press.path,
            }
        })
    }

    fn rel(&mut self, t: u64, raw: RawEvent, cursor: CursorSource<'_>) -> Vec<InteractionEvent> {
        // The `_HI_RES` twins are ignored so a notch is not counted twice.
        let (dx, dy) = match raw.code {
            // Toward the user is negative on evdev and scrolls the page down.
            REL_WHEEL => (0.0, -(raw.value as f32)),
            REL_HWHEEL => (raw.value as f32, 0.0),
            _ => return Vec::new(),
        };
        if raw.value == 0 {
            return Vec::new();
        }
        let (gx, gy) = match self.scroll_at {
            Some((last, x, y)) if t.saturating_sub(last) < SCROLL_BURST_MS => (x, y),
            _ => match cursor() {
                Some(p) => p,
                None => return Vec::new(),
            },
        };
        self.scroll_at = Some((t, gx, gy));
        if !self.contains(gx, gy) {
            return Vec::new();
        }
        let [x, y] = self.normalize(gx, gy);
        vec![InteractionEvent::Scroll {
            t,
            x,
            y,
            dx,
            dy,
            label: None,
        }]
    }

    /// Records the cursor while a button is held, at most every `DRAG_SAMPLE_MS`.
    pub fn tick(&mut self, t: u64, cursor: CursorSource<'_>) {
        let due = self
            .press
            .as_ref()
            .is_some_and(|p| t.saturating_sub(p.last_sample) >= DRAG_SAMPLE_MS);
        if !due {
            return;
        }
        let point = cursor().map(|(x, y)| self.normalize(x, y));
        if let Some(press) = self.press.as_mut() {
            press.last_sample = t;
            if let Some(point) = point {
                press.path.push(point);
            }
        }
    }
}

/// US name of an evdev key code, for shortcuts; `None` for keys we do not name.
pub fn key_name(code: u16) -> Option<&'static str> {
    const ROW_Q: [&str; 10] = ["Q", "W", "E", "R", "T", "Y", "U", "I", "O", "P"];
    const ROW_A: [&str; 9] = ["A", "S", "D", "F", "G", "H", "J", "K", "L"];
    const ROW_Z: [&str; 7] = ["Z", "X", "C", "V", "B", "N", "M"];
    const DIGITS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];
    const F: [&str; 24] = [
        "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14",
        "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
    ];
    let c = usize::from(code);
    Some(match code {
        1 => "Esc",
        2..=11 => DIGITS[c - 2],
        12 => "-",
        13 => "=",
        14 => "Backspace",
        15 => "Tab",
        16..=25 => ROW_Q[c - 16],
        26 => "[",
        27 => "]",
        28 | 96 => "Enter",
        30..=38 => ROW_A[c - 30],
        39 => ";",
        40 => "'",
        41 => "`",
        43 => "\\",
        44..=50 => ROW_Z[c - 44],
        51 => ",",
        52 => ".",
        53 => "/",
        57 => "Space",
        59..=68 => F[c - 59],
        87 => "F11",
        88 => "F12",
        102 => "Home",
        103 => "Up",
        104 => "PageUp",
        105 => "Left",
        106 => "Right",
        107 => "End",
        108 => "Down",
        109 => "PageDown",
        111 => "Delete",
        183..=194 => F[c - 183 + 12],
        _ => return None,
    })
}

// ---- Devices ----------------------------------------------------------------------------

/// `EVIOCGBIT(EV_KEY, len)`: `_IOC(_IOC_READ, 'E', 0x20 + EV_KEY, len)`.
fn eviocgbit_key(len: usize) -> libc::c_ulong {
    (2 << 30) | ((len as libc::c_ulong) << 16) | (u32::from(b'E') << 8) as libc::c_ulong | (0x20 + 1)
}

/// Whether `code` is set in a little-endian capability bitmap.
pub fn bit_set(bits: &[u8], code: u16) -> bool {
    bits.get(usize::from(code / 8))
        .is_some_and(|byte| byte & (1 << (code % 8)) != 0)
}

/// A pointer (has `BTN_LEFT`) or a keyboard (has `KEY_A`).
fn wanted(file: &File) -> bool {
    let mut bits = [0u8; 96];
    let rc = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            eviocgbit_key(bits.len()) as _,
            bits.as_mut_ptr(),
        )
    };
    rc >= 0 && (bit_set(&bits, BTN_LEFT) || bit_set(&bits, KEY_A))
}

/// Every readable pointer or keyboard under `/dev/input`; the others are skipped.
fn open_devices() -> Vec<File> {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
        })
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(path)
                .ok()
        })
        .filter(wanted)
        .collect()
}

/// `struct input_event` is a `timeval` followed by `u16 type`, `u16 code`, `i32 value`.
const EVENT_SIZE: usize = std::mem::size_of::<libc::timeval>() + 8;

/// Splits a read into raw events (the kernel timestamp is dropped).
pub fn decode_events(buf: &[u8]) -> Vec<RawEvent> {
    const TIME: usize = std::mem::size_of::<libc::timeval>();
    buf.chunks_exact(EVENT_SIZE)
        .map(|e| RawEvent {
            kind: u16::from_ne_bytes([e[TIME], e[TIME + 1]]),
            code: u16::from_ne_bytes([e[TIME + 2], e[TIME + 3]]),
            value: i32::from_ne_bytes([e[TIME + 4], e[TIME + 5], e[TIME + 6], e[TIME + 7]]),
        })
        .collect()
}

/// Waits up to `timeout_ms` for any device to have data; the indexes of the readable ones.
fn poll_readable(devices: &[File], timeout_ms: i32) -> Vec<usize> {
    let mut fds: Vec<libc::pollfd> = devices
        .iter()
        .map(|d| libc::pollfd {
            fd: d.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    if rc <= 0 {
        return Vec::new();
    }
    fds.iter()
        .enumerate()
        .filter(|(_, f)| f.revents != 0)
        .map(|(i, _)| i)
        .collect()
}

/// Reads what is pending on one device. `None` when the device is gone.
fn drain(device: &mut File) -> Option<Vec<RawEvent>> {
    let mut out = Vec::new();
    let mut buf = [0u8; EVENT_SIZE * 64];
    loop {
        match device.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => out.extend(decode_events(&buf[..n])),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Some(out),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// Records gestures on a thread until stopped.
pub struct InputRecorder {
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<Vec<InteractionEvent>>>,
}

impl InputRecorder {
    /// `request` is raised on each click, drag and shortcut so the window log snapshots then.
    pub fn start(frame: TargetFrame, started_at: Instant, request: Arc<AtomicBool>) -> Self {
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let worker = thread::Builder::new()
            .name("scrawl-input".into())
            .spawn(move || run(frame, started_at, &request, &stop))
            .ok();
        Self { stopping, worker }
    }

    /// Ends the thread and returns the events in time order.
    pub fn stop(mut self) -> Vec<InteractionEvent> {
        self.stopping.store(true, Ordering::Release);
        let mut events = self
            .worker
            .take()
            .and_then(|w| w.join().ok())
            .unwrap_or_default();
        events.sort_by_key(InteractionEvent::timestamp_ms);
        scrawl_log(format!("Input: {} events recorded", events.len()));
        events
    }
}

fn run(
    frame: TargetFrame,
    started_at: Instant,
    request: &AtomicBool,
    stopping: &AtomicBool,
) -> Vec<InteractionEvent> {
    let mut devices = open_devices();
    if devices.is_empty() {
        scrawl_log("Input: no readable /dev/input device (is the user in the input group?)");
        return Vec::new();
    }
    let mut gestures = Gestures::new(frame);
    let mut events = Vec::new();
    let mut cursor = || super::hypr::cursor_position();
    while !stopping.load(Ordering::Acquire) {
        let mut gone = Vec::new();
        for index in poll_readable(&devices, DRAG_SAMPLE_MS as i32) {
            let Some(raws) = drain(&mut devices[index]) else {
                gone.push(index);
                continue;
            };
            for raw in raws {
                let t = started_at.elapsed().as_millis() as u64;
                for event in gestures.handle(t, raw, &mut cursor) {
                    if !matches!(event, InteractionEvent::Scroll { .. }) {
                        request.store(true, Ordering::Release);
                    }
                    events.push(event);
                }
            }
        }
        for index in gone.into_iter().rev() {
            devices.swap_remove(index);
        }
        gestures.tick(started_at.elapsed().as_millis() as u64, &mut cursor);
        if devices.is_empty() {
            break;
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

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
        RawEvent { kind: EV_KEY, code, value }
    }

    fn rel(code: u16, value: i32) -> RawEvent {
        RawEvent { kind: EV_REL, code, value }
    }

    /// Feeds events with a cursor the test moves between them.
    struct Rig {
        gestures: Gestures,
        at: Cell<Option<(f64, f64)>>,
        out: Vec<InteractionEvent>,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                gestures: Gestures::new(frame()),
                at: Cell::new(Some((600.0, 300.0))),
                out: Vec::new(),
            }
        }

        fn feed(&mut self, t: u64, raw: RawEvent) {
            let at = &self.at;
            let mut cursor = || at.get();
            self.out.extend(self.gestures.handle(t, raw, &mut cursor));
        }

        fn tick(&mut self, t: u64) {
            let at = &self.at;
            let mut cursor = || at.get();
            self.gestures.tick(t, &mut cursor);
        }
    }

    #[test]
    fn left_click_inside_is_normalized() {
        let mut rig = Rig::new();
        rig.feed(1000, key(BTN_LEFT, 1));
        rig.at.set(Some((603.0, 302.0)));
        rig.feed(1080, key(BTN_LEFT, 0));
        assert_eq!(
            rig.out,
            vec![InteractionEvent::Click {
                t: 1000,
                x: 0.5,
                y: 0.5,
                button: Some("left".into()),
                label: None,
                role: None,
            }]
        );
    }

    #[test]
    fn right_click_names_the_button() {
        let mut rig = Rig::new();
        rig.feed(10, key(BTN_RIGHT, 1));
        rig.feed(60, key(BTN_RIGHT, 0));
        assert!(matches!(
            &rig.out[..],
            [InteractionEvent::Click { button: Some(b), .. }] if b == "right"
        ));
    }

    #[test]
    fn long_movement_is_a_drag_with_a_path() {
        let mut rig = Rig::new();
        rig.feed(100, key(BTN_LEFT, 1));
        rig.at.set(Some((700.0, 300.0)));
        rig.tick(140);
        rig.at.set(Some((1100.0, 550.0)));
        rig.feed(300, key(BTN_LEFT, 0));
        assert_eq!(
            rig.out,
            vec![InteractionEvent::Drag {
                t: 100,
                t1: 300,
                x0: 0.5,
                y0: 0.5,
                x1: 1.0,
                y1: 1.0,
                label: None,
                button: None,
                path: vec![[0.6, 0.5]],
            }]
        );
    }

    #[test]
    fn eleven_points_is_still_a_click_and_twelve_a_drag() {
        for (dist, drag) in [(11.0, false), (12.0, true)] {
            let mut rig = Rig::new();
            rig.feed(0, key(BTN_LEFT, 1));
            rig.at.set(Some((600.0 + dist, 300.0)));
            rig.feed(50, key(BTN_LEFT, 0));
            assert_eq!(matches!(rig.out[0], InteractionEvent::Drag { .. }), drag);
        }
    }

    #[test]
    fn press_outside_the_target_is_dropped() {
        let mut rig = Rig::new();
        rig.at.set(Some((50.0, 300.0)));
        rig.feed(0, key(BTN_LEFT, 1));
        rig.at.set(Some((600.0, 300.0)));
        rig.feed(50, key(BTN_LEFT, 0));
        assert!(rig.out.is_empty());
    }

    #[test]
    fn wheel_toward_the_user_reads_scrolled_down() {
        let mut rig = Rig::new();
        rig.feed(5, rel(REL_WHEEL, -1));
        let [event] = &rig.out[..] else { panic!("{:?}", rig.out) };
        assert_eq!(crate::sketch::describe_event(event), "scrolled down");
        rig.out.clear();
        rig.feed(500, rel(REL_WHEEL, 1));
        assert_eq!(crate::sketch::describe_event(&rig.out[0]), "scrolled up");
    }

    #[test]
    fn hi_res_wheel_is_ignored_and_horizontal_sets_dx() {
        let mut rig = Rig::new();
        rig.feed(5, rel(0x0b, -120));
        rig.feed(5, rel(0x0c, 120));
        assert!(rig.out.is_empty());
        rig.feed(6, rel(REL_HWHEEL, 2));
        assert!(matches!(
            rig.out[..],
            [InteractionEvent::Scroll { dx, dy, .. }] if dx == 2.0 && dy == 0.0
        ));
    }

    #[test]
    fn scroll_burst_asks_the_cursor_once_and_outside_is_dropped() {
        let mut rig = Rig::new();
        let calls = Cell::new(0);
        let mut cursor = || {
            calls.set(calls.get() + 1);
            Some((600.0, 300.0))
        };
        for t in [0, 40, 80] {
            rig.out.extend(rig.gestures.handle(t, rel(REL_WHEEL, -1), &mut cursor));
        }
        assert_eq!((calls.get(), rig.out.len()), (1, 3));
        let mut rig = Rig::new();
        rig.at.set(Some((5.0, 5.0)));
        rig.feed(0, rel(REL_WHEEL, -1));
        assert!(rig.out.is_empty());
    }

    #[test]
    fn ctrl_shift_k_is_a_shortcut() {
        let mut rig = Rig::new();
        for (code, v) in [(29, 1), (42, 1), (37, 1), (37, 0), (42, 0), (29, 0)] {
            rig.feed(7, key(code, v));
        }
        assert_eq!(
            rig.out,
            vec![InteractionEvent::Key { t: 7, shortcut: "Ctrl+Shift+K".into() }]
        );
    }

    #[test]
    fn super_enter_and_modifier_order() {
        let mut rig = Rig::new();
        rig.feed(1, key(125, 1));
        rig.feed(2, key(28, 1));
        rig.feed(3, key(28, 0));
        rig.feed(4, key(125, 0));
        for code in [42, 56, 29, 125] {
            rig.feed(5, key(code, 1));
        }
        rig.feed(6, key(2, 1));
        let shortcuts: Vec<_> = rig
            .out
            .iter()
            .map(|e| match e {
                InteractionEvent::Key { shortcut, .. } => shortcut.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(shortcuts, ["Super+Enter", "Ctrl+Alt+Shift+Super+1"]);
    }

    #[test]
    fn typing_modifiers_alone_and_repeats_are_not_events() {
        let mut rig = Rig::new();
        for (code, v) in [(37, 1), (37, 0), (42, 1), (37, 1), (37, 0), (42, 0), (56, 1), (37, 1)] {
            rig.feed(1, key(code, v));
        }
        rig.feed(2, key(56, 0));
        rig.feed(2, key(29, 1));
        rig.feed(2, key(29, 2));
        rig.feed(2, key(37, 2));
        assert!(rig.out.is_empty());
    }

    #[test]
    fn key_names() {
        assert_eq!(key_name(30), Some("A"));
        assert_eq!(key_name(44), Some("Z"));
        assert_eq!(key_name(11), Some("0"));
        assert_eq!(key_name(2), Some("1"));
        assert_eq!(key_name(59), Some("F1"));
        assert_eq!(key_name(88), Some("F12"));
        assert_eq!(key_name(183), Some("F13"));
        assert_eq!(key_name(194), Some("F24"));
        assert_eq!(key_name(57), Some("Space"));
        assert_eq!(key_name(109), Some("PageDown"));
        assert_eq!(key_name(53), Some("/"));
        assert_eq!(key_name(0), None);
        assert_eq!(key_name(29), None);
    }

    #[test]
    fn decodes_input_event_records() {
        let time = std::mem::size_of::<libc::timeval>();
        let mut buf = vec![0u8; EVENT_SIZE];
        buf[time..time + 2].copy_from_slice(&EV_REL.to_ne_bytes());
        buf[time + 2..time + 4].copy_from_slice(&REL_WHEEL.to_ne_bytes());
        buf[time + 4..time + 8].copy_from_slice(&(-1i32).to_ne_bytes());
        assert_eq!(decode_events(&buf), vec![rel(REL_WHEEL, -1)]);
        assert!(decode_events(&buf[..EVENT_SIZE - 1]).is_empty());
    }

    #[test]
    fn capability_bits() {
        let mut bits = [0u8; 96];
        bits[usize::from(BTN_LEFT / 8)] |= 1 << (BTN_LEFT % 8);
        assert!(bit_set(&bits, BTN_LEFT));
        assert!(!bit_set(&bits, KEY_A));
        assert!(!bit_set(&bits[..4], BTN_LEFT));
    }
}
