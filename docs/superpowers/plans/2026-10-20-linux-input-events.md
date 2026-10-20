# Linux phase 3: clicks, drags, scroll and shortcuts from `/dev/input`

Goal: Linux bundles carry the same `InteractionEvent`s the macOS build records, so keyframe
selection, click ripples, autocrop and `summary.md` work at the same quality. Ink/⇧ strokes are
phase 5 and stay out of scope here.

## Design

- **Source:** evdev character devices `/dev/input/event*`, read passively (no `EVIOCGRAB`) with
  `libc` (already a dependency): `open(O_RDONLY | O_NONBLOCK | O_CLOEXEC)`, `poll`, `read` of
  `struct input_event` (`timeval`, `u16 type`, `u16 code`, `i32 value`; 24 bytes on 64-bit).
  No new crate. The user is in the `input` group. A device that can't be opened is skipped;
  no readable device means one log line and an empty event list, never a failed recording.
- **Device choice:** devices whose `EVIOCGBIT` says they have `BTN_LEFT` (pointers) or `KEY_A`
  (keyboards). Pure-logic tests do not touch real devices.
- **Time:** each event is stamped with `started_at.elapsed()` when read (the same `Instant` the
  screen session and `hypr::WindowLogger` use), in ms. Kernel timestamps are not used.
- **Position:** evdev has no absolute pointer position on Wayland. At button down, on drag
  samples (every ~30 ms while held) and at button up the recorder asks Hyprland for
  `cursorpos` (reuse `hypr.rs`'s request helper) and normalizes against `target.frame` exactly
  like `windows::push_cursor` / macOS `normalize_point`: `(p − origin) / size`, clamped 0..1.
  Presses that start outside the target are dropped (macOS `contains_screen_point`).
- **Click vs drag:** same rule as macOS `pointer_down`/`pointer_up`: movement under 12 logical
  points is a `Click { t: down time, button: "left"|"right" }`, more is a
  `Drag { t: down, t1: up, x0, y0, x1, y1, button: None, path }` (path = the sampled points).
  `label`/`role` stay `None` (they come from accessibility, phase 6).
- **Scroll:** `REL_WHEEL` / `REL_HWHEEL` (ignore the `_HI_RES` twins so a notch isn't counted
  twice). Physical direction must match macOS wording: turning the wheel toward the user
  (page moves down) must read "scrolled down" through `sketch.rs` `describe_event`/`direction`.
  Position = last known cursor (query `cursorpos` per scroll burst, not per notch). Scrolls
  outside the target are dropped.
- **Shortcuts (privacy):** only key-downs while Ctrl or Super is held become
  `Key { shortcut }`; ordinary typing is never stored, not even internally beyond the current
  modifier state. Format is Linux style, modifiers in fixed order:
  `Ctrl+Alt+Shift+Super+<Key>` (e.g. `Ctrl+Shift+K`, `Super+Enter`). Key names come from a
  static evdev keycode table (US names: letters, digits, F1–F24, Enter, Tab, Space, Backspace,
  Esc, Delete, arrows, Home/End/PageUp/PageDown, punctuation); unknown codes are skipped.
  Modifier-only presses and auto-repeat (`value == 2`) are not events.
- **Urgent frames:** the macOS sampler keeps an extra frame right after each click. Linux
  decodes candidates after the fact, so `ScreenSession::stop` gains the event times: it also
  decodes `FrameTimes::At` frames ~250 ms after each Click and each Drag's `t1` (clamped to the
  video), merged into the candidate list in time order without duplicates.
- **Window log:** each click/drag/shortcut sets the `request` flag already passed to
  `hypr::WindowLogger::start` so the window snapshot happens at that moment (macOS raises
  `window_log_request` the same way).
- **Wiring:** `src/linux/input.rs` exposes `InputRecorder::start(target.frame, started_at,
  request) -> InputRecorder` and `stop(self) -> Vec<InteractionEvent>` (a thread, stopped with
  an `AtomicBool` like `WindowLogger`). `capture_into` starts it with the window logger (for
  every capture mode, not only Display) and passes the events as the 5th argument of
  `finalize_capture_bundle`, in time order.

## Tasks

### Task 1 — `src/linux/input.rs`: evdev decoding and gesture logic (pure, tested)

TDD with synthetic `input_event` sequences and a fake cursor source (a closure or trait object
returning global logical points), no devices:
- left click inside target → one `Click` with normalized x/y, `button: "left"`, `t` = down time;
- right click → `button: "right"`;
- movement ≥ 12 pt → `Drag` with x0/y0/x1/y1, `t1`, path;
- press outside target → nothing;
- wheel toward user → `Scroll` that `describe_event` renders "scrolled down"; hi-res codes
  ignored; horizontal wheel sets `dx`;
- `Ctrl`+`Shift`+`K` → `Key { shortcut: "Ctrl+Shift+K" }`; plain typing, Shift-only typing,
  modifier-only presses and repeats → nothing;
- key name table spot checks.
Plus the device layer (open/poll/read, `EVIOCGBIT` filter) and the `InputRecorder` thread.

### Task 2 — wiring, urgent frames, window log, docs

- `capture_into` starts/stops `InputRecorder`, sets the window-log request on gestures, passes
  events to `finalize_capture_bundle`.
- `ScreenSession::stop(events)` adds the post-click candidate frames (unit test for the time
  list: +250 ms, clamped, deduplicated, sorted).
- README Linux section: what is recorded, the `input` group requirement, the privacy rule.
- Manual check with the user: record, click a few times, scroll, press Ctrl+L; the bundle's
  `scrawl.json` has the events and `summary.md` keyframes say "clicked …"/"scrolled down".

## Constraints

Same as the port plan: macOS build untouched (Linux code in `src/linux/` or behind cfg), every
cargo command as `nice -n 19 ionice -c 3 cargo <cmd> -j 2` (tests `-- --test-threads=2`), no
new crates, style of the surrounding code, no `#[allow]`, commits end with the trailer.
