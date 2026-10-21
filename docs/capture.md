# Capture: shortcuts, Mac recording, displays

Read this before changing `src/hold.rs`, `src/capture.rs`, `src/screens.rs` or the picker and capsule placement in `src/ui.rs`.

## Shortcuts (`src/hold.rs`, `is_capture_shortcut` in `src/capture.rs`)

`⌥⇧S` (or the shortcut set in Settings) works like `⌘Tab`. A tap (modifiers up within 250 ms) records the display under the pointer at once, auto-cropped, with a brief red edge glow from the overlay as the start cue, and the capsule springs open onto switch chips (Area · Window · Screen · iPhone if cabled; `hold::Reveal`, `REVEAL_*` constants at the top of `src/ui.rs`) until 3.5 s pass or the user clicks, scrolls or draws (never while hovered). A chip (`switch_recording`) restarts rather than retargets: Window/Area discard the few seconds and morph the capsule into the non-held picker, iPhone discards and starts the phone.

Held, the switcher opens on Screen; each further press of the key (S) steps Screen → Window → iPhone, a mouse drag selects an area, Esc cancels, and releasing the modifiers records the selection. While recording, a tap finishes, ignoring one within 400 ms of the start. The menu's **Choose What to Record…** opens the full picker.

Carbon hotkeys and the key event tap report presses through `emit_capture_shortcut` (deduplicated within 90 ms); the UI loop polls the modifiers and takes Esc from the key tap (`tick_switcher`, `take_escape`), so the release is seen whichever window has focus.

## Mac captures (`src/capture.rs`)

The sampler keeps only small candidates (480 px long edge, in a `CandidateBuffer` capped at 384 MB that thins evenly and halves its sampling rate when full, never dropping the start or click/stroke frames). At finish the chosen keyframes are decoded from `recording.mp4` at native size (2560 px Screen, 1280 px Window/Region, never above the display's own scale, so a 1x 1920 pt display records 1920 px), shifted by `videoOffsetMs` (when the movie started on the capture clock); if decoding fails the candidates are used. `scrawl capture-sim` drives this headless for memory and timing checks.

## More than one display (`src/screens.rs`)

Every rect and point is global top-left points (`CGEvent`, `CGDisplayBounds`, `SCWindow` frames, `TargetFrame`); displays left of or above the primary one are negative. `screens::flip` converts AppKit frames, `screens::current()` lists displays with menu bar, notch and `backingScaleFactor` (main thread), `screens::displays()` the CoreGraphics view (any thread).

A capture belongs to one display: a tap takes the one under the pointer, Window the one holding the window's centre, Area the one holding its centre (clamped to it; `sourceRect` is display-local via `screens::source_rect`); `TargetFrame.scale` is that display's.

The picker opens on the pointer's display and follows the pointer between displays (window, Metal overlay and capsule: `follow_pointer_display`) except while an area is dragged or iPhone is shown; the overlay `move_to`s the target's display when recording starts and follows the iPhone mirror if it's dragged. The capsule (`NativePill::place_on`) sits on the display in use, `PANEL_Y` below its top or lower for a taller menu bar or the notch (`screens::pill_origin`). Test with a `CGVirtualDisplay` (see `scripts/virtual-display.swift`), never on Kit's desk Mac.

## Linux ink and the stroke clock (`src/ink.rs`, `src/platform/linux/overlay/`)

Strokes and pins carry the capture clock (`telemetry.started_at`, the same as clicks); the bundle pipeline shifts them by `videoOffsetMs` with everything else, so a stroke's `t0`..`t1` is on the recording's timeline once rebased. Linux records `wf-recorder`'s composited output, so the ink overlay (a layer-shell surface) is in the video, unlike macOS, which keeps its overlay out and only draws ink on keyframes; keyframes are still redrawn from the strokes (numbers on pins, marks that already faded from the video) at a width above the video's, so the video's ink sits underneath them. `-g` (window and area targets) records only the target's rect, and the overlay's coordinates are the output's, so `scene::build` takes the target's rect in output points. `urgent_frame_times` adds a frame 250 ms after each stroke ends and each pin drops, when the ink is still at full opacity.
