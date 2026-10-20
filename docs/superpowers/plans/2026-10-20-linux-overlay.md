# Linux phase 5: the ink overlay and the picker's dim and cutout

Goal: on Linux, holding ⇧ while recording draws the red marker (`#ff3b30`) on top of everything,
it is in the video, and the strokes land in the bundle exactly as on macOS (marks, keyframes,
`summary.md`); ⇧-click drops numbered pins. The target picker dims the screen with a cutout
around the hovered window, the screen or the dragged area, as the Metal overlay does on macOS.

Scope note (checked in the tree, not assumed): macOS has **no producer of laser strokes**.
`active_laser` is never assigned and nothing builds a `Stroke { laser: true }`; "laser" is the
overlay's name (`laser_overlay.rs`) and a dead branch in its shader. What the user sees is
⇧ + move = ink (README: "Capture Controls") and ⇧-click = pin. Phase 5 builds those two and
skips the laser branch (a stroke with `laser: true` is not drawn).

## Findings (spike, 2026-10-20, Hyprland 0.56.2, one 1920×1080@144 Hz output, scale 1)

Spike code is in `/home/ksm/scrawl-spikes/overlay-spike/` (`gpui_probe`, `layer_probe`,
`ipc_probe`, `evdev_probe`, `sdf_bench`), never in the repo. Visual checks were `grim` pixel
reads against a baseline grab and a `wf-recorder` clip (≤3 s, deleted).

**1. Can gpui 0.2.2 open a layer-shell surface? No.**
- `grep -ril 'layer_shell\|LayerShell\|wlr_layer\|zwlr'` over gpui-0.2.2: no hit. `WindowKind` is
  `Normal | PopUp | Floating`; `PopUp` is a plain `xdg_toplevel` (the Wayland backend only
  branches on `Floating`, for `set_parent`). gpui's Wayland backend has no sctk and no
  wlr-protocols crate.
- A layer-shell kind would be a fork of `wayland/window.rs` (72 uses of `toplevel`/`xdg_surface`)
  and `client.rs`, on the version macOS is pinned to. Rejected.

**Option (i), a fullscreen transparent pinned gpui window.** It works:
- Rule: `float, pin, no_focus, border_size 0, rounding 0, no_shadow, no_blur, no_anim, move 0 0,
  size monitor_w monitor_h`. Transparent background composites transparently: a `grim` diff
  against the baseline differed only at the drawn pixels (3 802 of 2 073 600).
- `wf-recorder` records it (the marker is in the video frame). Frame cadence 144 fps, gap p50
  6.9 ms, p99 7.9 ms. Pointer sample to paint (8 ms poll, newest sample's age when first
  painted): p50 2.6 ms, p99 5.7 ms.
- Input: the overlay received 0 pointer events while the pointer moved; the active window did not
  change; with a window under it, that window still got the motion and the focus (hover and focus
  pass through a `no_focus` window).
- Over a fullscreen window (a gpui window with `WindowBounds::Fullscreen`): still drawn on top,
  pinned floats are above fullscreen.
- **`rounding = 0` is needed.** The session has `decoration:rounding 10`; without the rule the
  four screen corners are not dimmed (pixels (0,0), (1,1), (1919,1079) unchanged, (3,3) dimmed).
  The existing `picker_rule` lacks it.
- Colour is off: `paint_path(rgb(0xff3b30))` reads back (219, 52, 43) over dark and (251, 59, 50)
  over blue, not (255, 59, 48).
- Costs: runtime rules per window class and per monitor (rules are session-only and static: the
  window cannot follow the target to another output without a rule change), it is a client in
  `hyprctl clients` and in task lists, and it is ordered by Hyprland's window rules, not by a
  protocol layer. Untested: how it stacks against a pinned float mapped later (the capsule).

**Option (ii), a native `wlr-layer-shell` surface on its own Wayland connection** (sctk 0.20
without default features, `shell::wlr_layer`, `wl_shm`):
- `Layer::Overlay`, anchored on all four edges, exclusive zone -1, keyboard interactivity none,
  **empty input region**. `hyprctl layers` shows it at level 3 (overlay); `configure` arrives with
  1920×1080.
- Composites transparently, exact colour (255, 59, 48), recorded by `wf-recorder`, drawn on top of
  a fullscreen window, focus unchanged. Hover reaches a window underneath: that window logged its
  first `mouse_move` inside the overlay's lifetime (epoch ms 589283 within 588275..593276).
  Clicks were not injected: the owner's pointer is live (see 2), so a `ydotool` click could land
  on his apps. The hit test is the same one that routes hover and focus, and an empty input
  region is protocol-defined passthrough.
- Frame cadence 143.5 fps (frame callbacks). Event-driven loop (`poll` on the Wayland fd plus an
  `eventfd`, commit only when something changed): 173 commits in 5 s, all of them the 1.3 s fade
  of one stroke, none while idle. Idle must not commit: `wf-recorder` writes on damage.
- A second Wayland connection next to gpui's in one process is ordinary; the spike builds gpui
  and sctk in one dependency graph (one wayland-client 0.31.15, wayland-protocols 0.31 and 0.32
  side by side).

**CPU rendering is enough, but not with tiny-skia.**

| Work (1920×1080, opt-level 2) | p50 | p99 |
|---|---|---|
| tiny-skia: one alpha `fill_rect` over the screen (a dim) | 30.8 ms | 35.8 ms |
| tiny-skia: 2000 per-segment AA strokes | 33.1 ms | 40.7 ms |
| tiny-skia: clear the pixmap | 0.8 ms | 1.4 ms |
| BGRA swizzle copy of the whole screen | 2.7 ms | 4.0 ms |
| Port of the shader's ink branch (per-segment box, max coverage): 150 segments | 0.64 ms | 0.75 ms |
| the same, 400 segments | 1.7 ms | 1.9 ms |
| the same, 2000 segments (a stress case; ink fades after 1.3 s) | 7.0 ms | 10.3 ms |
| compositing the dirty box, 150 / 400 / 2000 segments | 0.08 / 0.17 / 0.68 ms | |

The overlay never draws a screen-wide dim (the picker's GPUI window does), so a hand port of
`laser_overlay.metal`'s ink, pin and tip-dot maths into a dirty-box rasteriser, with no new
dependency, is 4 µs per segment and looks the same as macOS. (tiny-skia is in the tree through
gpui's resvg but is 4× slower per segment and useless for a full-screen fill.)

**2. Pointer positions while ⇧ is held.**
- `j/cursorpos` over the IPC socket: 3 000 sequential requests, p50 **11 µs**, p95 19 µs, p99
  40 µs, max 2.6 ms. A 125 Hz loop held 125.0 Hz (gap p99 8.6 ms); a 1 000 Hz loop cost the probe
  2.0% of a core. Hyprland's own CPU moved 0–12% between runs with the same load (the owner's
  session is live), so no overhead shows above noise at 125 Hz. After an evdev `SYN`, the query
  answers in p50 116 µs (428 samples).
- evdev cannot give the position. Devices here: `libvirtualhid Mouse (Absolute)` (event25),
  `libvirtualhid Mouse` (rel), Touchscreen, Pen Tablet, `ydotoold virtual device` (rel) and a
  SteelSeries keyboard. The owner's pointer is **the Moonlight absolute device**: 387 `SYN`s in
  6 s, `ABS_X/Y` raw 0..65535, no `REL`. A `REL` sum is meaningless there, and for relative
  devices libinput acceleration happens in the compositor (a `ydotool` run summed REL (200, 0)
  while the cursor ended (-293, -84) away, the absolute device moving it concurrently). Drift
  against a true cursor could not be measured cleanly because of that, and does not need to be:
  accumulating evdev motion is rejected on principle.
- Recommendation: the compositor is the only source that agrees with what is on screen for every
  device (Moonlight's absolute and relative devices, `ydotoold`, a real mouse, a touchpad, a pen).
  **Poll `cursorpos` every 8 ms only while a pure ⇧ is held during a recording**: this is the
  macOS HID poller (8 ms sleep) with the IPC call in place of `NSEvent.mouseLocation`. No evdev
  motion is needed, so no `ABS`/`REL` device choice, and it works when the pointer is moved by
  `hyprctl` or a tablet. The existing evdev reader already tells when ⇧ goes down and up.

**3. Can the existing GPUI picker draw the dim itself? Yes.**
- A gpui path with two subpaths (full rect plus a rounded rect, `FillRule::EvenOdd`) painted in
  `rgba(0x00000052)` gives: outside pixel 29 → 21 (×0.72 = 1 − 0.32), inside the cutout unchanged,
  the rounded corner pixel dimmed, and a 1 px `rgba(0xffffffec)` inset stroke reads 209 on
  a dark pixel. Same rule needed: `rounding = 0` (above).
- The picker already is the full-output, focusable, pinned window that owns the pointer and keys
  (phase 4: chips clicked, Esc, area drag state `selection_drag`). It already computes the
  `highlight` rect for the badge. A layer-shell dim cannot work for the picker: the overlay layer
  is above every window, so it would dim the picker's own badge, camera cursor and chips; the
  picker needs input, which an empty-input-region surface does not give.
- On macOS the Metal overlay is opened by the picker and reused for the recording ("zero swap").
  On Linux nothing needs to carry over: the picker window closes, the ink overlay maps.

**Not tested, and why:** click injection through the overlay (live pointer, above); more than one
output and fractional scale (one output at scale 1); Hyprland older than 0.53; compositors other
than Hyprland (the code would work on any wlroots-protocol compositor); the capsule's stacking
against a later pinned float.

## Architecture

- **Two mechanisms, one per job.** The picker draws its own dim inside its GPUI window
  (`TargetPickerWindow`, Linux only: the macOS Metal overlay keeps drawing it there). Ink is a
  native layer-shell overlay thread, opened by the same `MetalLaserOverlay::open` call sites in
  `ui.rs`, so the coordinator's flow (open at the picker, `move_to` the target, drop at the end)
  is unchanged.
- **Ink is in the video, by design.** `wf-recorder` records the composited output, so the ink
  overlay is recorded (the goal asks for it; macOS excludes its overlay window from the capture
  and only draws ink onto keyframes). The strokes are also in the bundle and drawn onto the
  keyframes, as on macOS: a keyframe cut while the ink is still visible shows it twice at the
  same place (the same path, so it reads as one, slightly bolder at the edge).
- **Portable ink logic out of `capture/mac.rs`.** `is_pin_click`, `ink_span_pt`, `is_real_ink`,
  `ink_before` and their constants (`CLICK_SLOP_PT`, `PIN_MAX_PRESS_MS`, `MIN_INK_SPAN_PT`) move to
  a new `src/ink.rs` (mechanical: moves and `use` paths, tests move with them). New there:
  `InkTracker`, the state machine of `run_global_hid_poller` + `shift_down`/`shift_up`: pure
  ⇧ + not ctrl/alt/super + inside the target draws a `Stroke` (points `[x, y, dt]` in
  `0..=SCALE`, a point only when ≥ 1.5 pt from the last, at most 2 400), ⇧ held with ctrl/alt/super
  waits for the ⇧ release, release commits the stroke only if `is_real_ink`, a ⇧-press that lets
  go within 1 s and under 12 pt is a numbered pin (what was inked before the press is kept via
  `ink_before`). Same call shape as `Gestures`: `handle(t, raw, cursor)` and `tick(t, cursor)`.
- **Input.** `InputRecorder::run` feeds each evdev event to `Gestures` (clicks, as now: a
  ⇧-press is still never a click) and to `InkTracker`. While ⇧ is down it polls the
  device set with an 8 ms timeout (30 ms otherwise) and calls `tick`, which asks
  `hypr::cursor_position()`. The tracker writes into the telemetry's `strokes` and `live_ink`,
  which become `Arc<Mutex<…>>` so the recorder thread and the overlay thread share them
  (`self.telemetry.strokes.lock()` in `ui.rs` keeps compiling through `Arc` deref: undo and the
  pill's stroke count work unchanged). `finish_session` returns the strokes.
- **Overlay thread** (`platform::linux::overlay`, named `MetalLaserOverlay` in the seam so
  `ui.rs` is untouched): its own `wayland-client` connection, `Layer::Overlay`, namespace
  `scrawl-ink`, all four anchors, exclusive zone -1, keyboard none, **empty input region**,
  `wl_shm` pool of two buffers. `open` returns after the first `configure` (error text if there
  is no `zwlr_layer_shell_v1`, logged as today's "Metal overlay failed: …"). It maps nothing (no
  buffer) until `telemetry.active`, so opening it at the picker costs nothing visible. The loop
  polls the Wayland fd and an `eventfd` (the ink recorder writes it on every change; a 50 ms
  timeout also catches changes nobody signals, such as the Undo chip) and commits only when
  the picture changed: the dirty box of strokes, their 650 ms hold and 650 ms fade, the tip
  dot. Output: the wl_output whose logical rect holds the centre of `on` (as
  `pick_screen`), else the pointer's; `move_to` another output recreates the surface. Buffer
  scale `ceil(scale)` (sctk's preferred scale).
- **Scene and raster are pure.** `overlay/scene.rs` copies the stroke → segments/pins loop of
  `laser_overlay.rs::draw` (it is entangled with the `repr(C)` Metal structs, so it is copied,
  not moved, and carries a comment to stay in step; macOS is not edited for it) using
  `sketch::smooth_ink_points`. `overlay/raster.rs` ports the ink, pin (halo + fill) and tip-dot
  branches of `laser_overlay.metal` into a premultiplied BGRA canvas with damage tracking; the
  colours are `#ff3b30` and a 0.82 white halo.
- **Picker dim.** `platform/linux/picker_dim.rs`: `dim_shape(kind, rect, screen, fade)` returns
  the cutout, its corner radius (10 for windows and screens, kind 2; 0 for an area, kind 1; the
  phone is macOS only), the 0.32 × fade dim, the 0.13 tint inside a window (`#3b5cf6`) and the
  inset border (2 px blue `#5b7cfa` at 0.93 for windows, 1 px white at 0.92 for areas), exactly
  the shader's constants at `intro_ease = 0`. The picker paints it with one `canvas` before its
  badge. With no rect: the uniform 0.32 dim. `picker_rule` gains `rounding = 0` (and `rounding 0`
  in the legacy rules).
- **Nothing in the recording but ink.** No start glow, no dim during a recording (they would be
  recorded). Task 8 is the optional dim drawn only outside the recorded rectangle.
- **Z-order of the picker's chips.** The capsule must stay above the picker's dim: it is opened
  after the picker (`ensure_hud_pill_open`) and both are pinned floats. Task 2 checks it with the
  capsule already open; the fix, if needed, is `hyprctl` `alterzorder top` on the capsule.

## Safety net for macOS

macOS-side edits: `capture/mac.rs` (helpers move out, `use crate::ink::…`), `sketch.rs` (the
`cfg(any(target_os = "macos", test))` on `smooth_ink_points` and `slice_laser_window` becomes
`cfg(any(target_os = "macos", target_os = "linux", test))`), `capture/mod.rs`
(`LiveOverlayEvent::UndoStroke` loses its `cfg`). Each is listed in its commit. After every
task: `bash /home/ksm/scrawl-spikes/scrawl-macstub/mc.sh` must show no scrawl diagnostics.

## Dependency tradeoff (needs the owner's go-ahead: `Cargo.lock` needs the network)

`smithay-client-toolkit = { version = "0.20", default-features = false }` under
`[target.'cfg(target_os = "linux")'.dependencies]`. Without default features: no calloop, no
xkbcommon. New in the lock (checked by resolving the spike's graph against the repo's `Cargo.lock`): six
crates, all pure Rust: `smithay-client-toolkit`, `cursor-icon`, `wayland-csd-frame`,
`wayland-protocols-wlr`, `-misc` and `-experimental`. wayland-client, wayland-backend,
wayland-cursor, wayland-protocols 0.32, memmap2, xkeysym, rustix and libc are already there.
- Without sctk: `wayland-client` (already) + `wayland-protocols-wlr` and hand-written `Dispatch`
  impls and a memfd pool, ~250 more lines, one new crate instead of six. The plan takes sctk for
  the `LayerSurface`, `Shm`/`SlotPool`, `OutputState` and `Region` helpers; swapping later only
  touches `overlay/surface.rs`.
- Fallback with no new crate: option (i) above (a pinned, `no_focus`, `rounding = 0` gpui window
  with a rule per output). It works but has the costs listed; `scene.rs` and `raster.rs` do not
  apply (gpui paths), `ink.rs` and the input wiring do.

## Constraints

Same as the earlier phases: macOS build behaves as before (mechanical edits only, listed per
commit), every cargo command as `nice -n 19 ionice -c 3 cargo <cmd> -j 2` (tests add
`-- --test-threads=2`) and told to the owner first, no `#[allow]`, `//!`/`///` docs and
`crate::capture::scrawl_log` in the surrounding style, no gpui fork or git pin, the owner's running
`scrawl app` is replaced only when a task's verification needs it (it holds the instance lock; a
second app cannot run beside it, and `XDG_RUNTIME_DIR` cannot be changed without losing the
Wayland and Hyprland sockets), announced first, recordings ≤ 5 s and deleted, clipboard saved and restored, commits end with
`Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

Verification needs the owner's pointer: `hyprctl eval 'hl.dispatch(hl.dsp.cursor.move({ x = …,
y = … }))'` moves it (a `ydotool` click goes wherever it is). The owner uses Moonlight, whose
absolute device keeps repositioning the pointer, so every manual step below is run with the owner
away from the machine, and never with a `ydotool` click outside a window of ours.

## Tasks

### Task 1 — dependency and the picker rule (small, no behaviour change yet)

- Add the sctk line above; `Cargo.lock` update (network, announced). `cargo check` and tests
  still pass.
- `picker_rule` and the legacy rules gain `rounding = 0` / `rounding 0`; update
  `picker_covers_its_monitor_and_review_floats_centred` first (RED: asserts the new strings).
- Verify: `cargo test` and `mc.sh`; the live rule is exercised by Task 2's run.

### Task 2 — the picker draws its own dim and cutout

TDD (pure, `platform/linux/picker_dim.rs`):
- `dim_shape(2, window_rect, screen, 1.0)`: cutout = the rect, radius 10, dim 0.32, tint 0.13, border
  2 px, blue; `dim_shape(1, …)`: radius 0, no tint, 1 px white; no rect: whole-screen 0.32; `fade`
  0 → everything 0; rect clipped to the screen; sub-pixel rect → none.
- Geometry of the two-subpath fill (outer rect + rounded rect with `k = 0.5523 r` cubics).

Wiring: `TargetPickerWindow::render` paints it (Linux only, `cfg`) from `highlight`, `mode` and
`picker_fade` (the variable that is computed and unused today); `set_picker_rect` stays a no-op
store with its comment fixed ("the GPUI picker draws it itself").

Verify with `ydotool` and `grim` (owner away; the owner's `scrawl app` holds the instance lock, so
this and every later run replaces it with the new build, announced, and restarts his afterwards):
hold ⌥⇧ (`ydotool key 56:1 42:1 31:1 31:0`, keep 56 and 42 down) → the picker opens on Screen, which
is a window-style cutout over the whole output (as macOS): the centre tinted blue ≈ 13%, a 2 px blue
border at the edges, the four screen corners dimmed ×0.68 (outside the radius-10 cutout; they are
only reachable because of `rounding = 0`), the capsule's chips above and not dimmed. Space → Window
mode and a `hyprctl` cursor move onto a window → the cutout is that window's rect, everything else
dimmed ×0.68; Area: `ydotool` button down and relative moves → the cutout follows the drag with the
badge `w × h`. The capsule opened before the picker (Choose target while it shows) stays on top.
Esc closes it (`ydotool key 1:1 1:0`).

### Task 3 — `src/ink.rs`: the helpers move, `InkTracker` (pure, tested)

- Move the helpers and constants and their tests out of `capture/mac.rs` (mechanical, listed in the
  commit); `mc.sh` clean.
- `InkTracker` TDD with synthetic `RawEvent`s and a fake cursor, as `Gestures`' rig:
  ⇧ down + movement inside the target → a live stroke whose points are normalized with `SCALE`
  and `dt` since the stroke start; a point under 1.5 pt from the last is not added; 2 400 points
  cap; leaving the target ends nothing but adds nothing; ⇧ + Ctrl (or Alt or Super) draws nothing
  and nothing until ⇧ is released; release commits only if `is_real_ink` (a stationary ⇧ tap, as a
  capital letter, is dropped); ⇧ + left press + release under 12 pt and 1 s → a pin numbered
  from the pins so far, with the earlier ink kept (`ink_before`) and the press jitter dropped;
  a longer or farther press is ink; a press from a different target frame is ignored;
  `finish` commits the live stroke; `shift_held` and the pointer for the tip dot are exposed.
- `sketch.rs` cfg widened (listed).

### Task 4 — wire the tracker into the Linux input thread

TDD: `SamplePacer` (pure: the poll timeout is 8 ms while ⇧ is held and 30 ms otherwise); recorder
with a scripted event source: `finish_session` returns the strokes, `reset_for_session` clears
them; `telemetry.strokes`/`live_ink` as `Arc<Mutex<…>>`, plus `shift_held`, `pointer_screen` and the
overlay wake (`eventfd`, a no-op until an overlay registers).

Also here: the urgent-frame candidates. `urgent_frame_times` gets the strokes: one frame at a
stroke's end and one at each pin, +250 ms like clicks, thinned and clamped as now (RED: the time
list).

Verify (no overlay yet, owner away): `scrawl app`, record; `ydotool key 42:1`, step the pointer
with `hyprctl … cursor.move` about every 10 ms along a square, `ydotool key 42:0`; then a ⇧
press-and-release without moving (pin) and a plain ⇧ tap (nothing); finish. `scrawl.json` has one
`strokes` entry without a `pin` field and ≈ the points sent, one pin, no other; `summary.md` names the mark and
the pin; the contact sheet's keyframe shows the red path and `#1`. Delete the bundle.

### Task 5 — scene and raster (pure, tested)

TDD (`overlay/scene.rs`, `overlay/raster.rs`):
- Scene: a finished ink stroke is visible at full alpha for 650 ms after `t1`, fades with
  `(1 − t)²` to 1 300 ms, then is dropped; a live stroke is always full; pins are 11.5 radius at
  the first point; the tip dot at the pointer only while ⇧ is held; laser strokes are skipped;
  `[nx, ny]` → target pixels with the target origin; more than the cap drops the oldest.
- Raster: a horizontal segment's centre pixel is (255, 59, 48, 255) premultiplied BGRA; one pixel
  past `r + 0.7` is transparent; width modulation (`w` 0.55 → radius 1.24, 1.15 → 2.59); a pin's
  halo ring is white at 0.82 and its fill red; the tip dot radius; the dirty box is the union of the
  segment boxes (and the previous frame's, so erasing works); an empty scene clears only the
  previous box; overlapping strokes keep the max coverage, not a sum.
- Timing test as a bench-style assertion with a generous bound (400 segments under 20 ms in a debug
  build), so the CPU claim in the findings stays checked.

### Task 6 — the layer surface (`platform::linux::overlay`)

- Pure: `pick_output(outputs, on, pointer)` (centre of `on`, else most overlap, else the pointer's,
  else the first) with `screens::index_for_rect`'s rules; buffer scale choice.
- The thread as in Architecture; `open`, `move_to`, `screen`, `window_number` (0: nothing excludes
  a window on Linux), `Drop` destroys the surface and joins. The stand-in `MetalLaserOverlay` is
  replaced by it in the seam. Error cases: no layer shell, the connection closing (the thread
  logs and ends; recording goes on without ink).
- Verify (owner away): start `scrawl app`, `scrawl toggle`; `hyprctl layers` lists namespace
  `scrawl-ink` at level 3 during the capture (and not after it); `ydotool key 42:1` and cursor steps draw a
  red path; `grim` reads (255, 59, 48) on the line and the baseline elsewhere (a diff against a
  grab before it); the active window is unchanged; `wf-recorder` (the recording itself): extract a
  frame at the ink time with `ffmpeg -ss … -frames:v 1` and find the red; 1.3 s after the last
  move the path is gone; with the pointer still and no stroke, `WAYLAND_DEBUG=1` on the overlay
  shows no commits (idle). A window of ours underneath still gets the hover (the spike's `victim`).
  Stop; the surface is gone.

### Task 7 — shared glue and docs

- `LiveOverlayEvent::UndoStroke` ungated, and Linux's listener sends it on Ctrl+Z (no other
  modifier) while a capture is active, like ⌘Z (the key also reaches the app: nothing is
  grabbed). `RequestSnapshot` stays macOS (Linux decodes frames afterwards, Task 4).
- README Linux section: ⇧ ink and pins, ink is in the video, picker dim, Hyprland needs
  `zwlr_layer_shell_v1` (any wlroots-protocol compositor); remove "ink" from "Not yet on Linux".
- A note in `docs/capture.md` or the Linux section on the stroke clock and `-g` recording.

### Task 8 (optional, owner to decide) — the recording-time dim outside the target

macOS keeps a 0.22 dim outside a window/area target while recording. On Linux it can be drawn on the
same overlay **outside** the `-g` rectangle `wf-recorder` records, so it is never in the video:
the dim as four rects around the target, a 1 px border just outside. `dim_shape` gets the
recording variant (0.22, no tint, no border inside); not for full-screen targets (macOS
fades a 0.32 dim there for 0.15 s; the start glow would be recorded, so neither is done).
Verify: window recording; `grim` shows the dim outside; the clip's frame has none.

### Task 9 — end-to-end check with the owner

Owner at the machine with a pointer and a keyboard (not Moonlight): record a window, hold ⇧ and
draw a circle and an underline around something, ⇧-click twice, Ctrl+Z once, finish with the
bind. Check: ink drawn live and above a fullscreen window and the capsule; clicks and hover
through it; the bundle's contact sheet and `summary.md` have the marks and pins; the video has
the ink for about 1.3 s after each stroke. Picker: area drag, window hover, Esc. Repeat with the
owner's Moonlight pointer for latency feel. Delete the bundles.

## Risks

- **Fractional scale and several outputs are untested** (one output at scale 1 here).
  `ceil(scale)` buffer scale is a plan, not a measurement; Hyprland's `wl_output.scale` and the
  `preferred_buffer_scale` path need a look on a scaled monitor before release.
- **Click passthrough was inferred, not injected** (hover and focus pass; the input region is
  empty). Task 6's verification adds a click on a window of ours with the owner away.
- **Ink in the video** changes what a Linux bundle's frames look like against macOS (and doubles
  on a keyframe cut inside the fade). Needs the owner's decision (below).
- **8 ms polling** puts the ink about one poll plus one frame (≈ 15 ms) behind the pointer, as on
  macOS (120 Hz poller). Measured parts: 11 µs per query, 6.9 ms frames. The end-to-end feel,
  Moonlight included, is Task 9's.
- **Hyprland-only position source** (`cursorpos`); the layer-shell part is not. A sway/other port
  would need a position source and nothing else.
- The scene copy (`overlay/scene.rs`) can drift from `laser_overlay.rs`; the comment and the
  shared constants test are the only guard.
- Runtime rules added by the spike for classes `scrawl-spike-*` live in the running session until
  Hyprland restarts; they match nothing else.
