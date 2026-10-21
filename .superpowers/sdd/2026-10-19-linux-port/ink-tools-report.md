# Ink tools: undo, colours, arrows and boxes (report)

Status: DONE, with the live checks at the end. Branch `feat-ink-tools` (from `linux-port` 42be30e).

## Keys chosen and why

| Keys | Does | Why |
|---|---|---|
| Ctrl+Z (Ctrl alone) | removes the last non-laser stroke or pin | macOS has ⌘Z (`undo_stroke`, `LiveOverlayEvent::UndoStroke`) and its tap does not swallow it either, so the Linux analog is the same tradeoff: the focused app also gets Ctrl+Z. A "safer" combo (⇧+Backspace) would be inconsistent with macOS and still pass through to the app, so there is no gain. It only acts while a recording runs (the tracker exists only then). |
| ⇧ held, then 1 / 2 / 3 / 4 | red / yellow / green / blue for the strokes after | As asked. Not with Super (Hyprland has Super+⇧+digit binds). Tradeoff: the app also gets ⇧+digit (`!@#$`); documented. Persists for the recording only (`InkTracker::new` resets it). The tip dot shows the colour. |
| ⇧ + Ctrl, move | arrow from where the keys went down to the pointer | `hyprctl binds -j`: no bind on ⇧+Ctrl (mask 5) except Escape, none with the mouse. |
| ⇧ + Alt, move | box, pointer and start as opposite corners | mask 9 only has the recording shortcut (S). |

Figures need no mouse button, like freehand ink: a click would reach the app as Ctrl+Shift-click (browsers open links in a new window) or Alt-click. Ctrl and Alt together with ⇧, or Super with ⇧, still draw nothing until ⇧ is released (shortcuts). Changing the modifier or the colour while drawing ends the stroke and begins a new one. An Alt tap can show Firefox's menu bar (noted in README).

Undo takes the stroke out of the shared `strokes` list (the one `finish_session` hands to the bundle), so overlay and `scrawl.json`/marks/keyframes agree; a pin undone frees its number. Implemented in `InkTracker` rather than as a `LiveOverlayEvent` (the overlay plan's idea), since the list is already shared with the input thread; no UI hop.

## In the bundle

- `Stroke.color`: `"yellow" | "green" | "blue"`; absent = red (old bundles unchanged).
- `Stroke.shape`: `"arrow" | "box"`; absent = freehand. A figure's `pts` are its two ends (box: opposite corners), `dt` is ms since `t0`.
- `Mark.color` (absent = red), new `Shape::Box` (`"box"`).
- Marks: an explicit arrow stroke is a mark `arrow` with from/to ("drew an arrow from x 0.10, y 0.80 pointing to x 0.45, y 0.45"), a box is `box` with its corners ("boxed region x 0.20–0.60, y 0.30–0.70"), whatever freehand ink sits next to it; non-red adds " in green" before " over <element>". `agent_context` mark text got "boxed" and the colour too.

## Rendering

- Overlay raster: segment colour, max-coverage colour wins at a crossing, red keeps the shader's exact constants (`#ff3b30` at the centre, tests unchanged); the tip dot takes the picked colour. Figures use `sketch::figure_segments` (shaft + two head strokes at 28 degrees, head = 4 widths but at most half the arrow; four sides), the one geometry for every renderer.
- Keyframes/contact sheet (`render.rs`, shared): each stroke in its colour; figures via `figure_segments` with round caps; a figure whose second end is later than the keyframe is not shown.
- macOS export plan (`export/ink.rs`, `helper/Export.swift`): per-piece capsule polygons for figures (shown when finished), optional `color` on a plan stroke (`[r,g,b]`, absent = red). The Swift edit is 3 lines and is NOT compiled here (no Swift toolchain): please build the helper once.
- Skipped on purpose: pins stay red; typed on-screen text is deferred (needs keyboard focus, which the overlay deliberately never takes).

## Samples (rendered, not on screen)

`SCRAWL_INK_SAMPLES=/tmp/ink-samples cargo test ... writes_sample_frames_when_asked` writes
- `/tmp/ink-samples/overlay.png`: the overlay raster composited on a page-like background (red wave, yellow ellipse, green arrows, blue box, red pin, yellow tip dot);
- `/tmp/ink-samples/keyframe.png`: the keyframe renderer on the same strokes (same shapes and colours, numbered pin and badge).

## Performance (1080p, nice 19 with a sibling build running, so noisy)

- Existing case, two 200-segment strokes in opposite corners: median 1.7 ms, worst 1.9-2.6 ms (phase 5: 1.58 / 1.76).
- New: a full-screen diagonal arrow plus a 1152x486 box, repainted every frame: median 2.9-4.1 ms, worst 4.3-4.8 ms, damage 21% of the screen. Before the fix (a long diagonal binned its whole box, and scanned all of it) it was 6.1 ms median. Two changes: a segment bins only into tiles within its reach of the line (`segment_reaches`), and scans only a band of each row (`row_span`).
- Idle still makes no commits; figure points are two per stroke.

## Tests

`cargo test` 457 passed, 0 failed, 2 ignored (was 405 + the phase's later ones; +52 mine): tracker (pens, switching, colours, undo, wake), scene/raster (colours, tip, crossing, tile damage, timing), sketch (arrow/box marks, descriptions, geometry), render (colours, arrow head, hollow box, unfinished figure), export plan unchanged shape tests still pass. `mc.sh` adapted copy in `/tmp/inkmc` (original untouched, pointed at the worktree): clean on both bin and tests.

## Commits

1. shared model, classifier and renderers (macOS-side edits mechanical, listed in the message);
2. tracker: pens, colours, undo;
3. overlay: colours, figures, tile binning;
4. README and docs.

## Needs live testing by the user

1. Real keys under Hyprland: ⇧+Ctrl and ⇧+Alt feel (pressing the modifier at the start of the arrow; Ctrl pressed before ⇧ also works), that no Hyprland bind or app eats ⇧+Alt (some keyboards use Alt+Shift to switch layout: check `kb_options`), Firefox's Alt-tap menu bar.
2. Ctrl+Z side effect in the focused app during a recording (it also undoes there), and ⇧+1..4 typing `!@#$` into a focused text field.
3. The tip dot's colour change on a digit and its readability (it is small, as before).
4. Yellow on light pages (contrast), blue/green in the video (wf-recorder encodes the overlay: colour fidelity).
5. A real recording's `summary.md` and contact sheet with an arrow and a box; macOS export of such a bundle (helper build, figures drawn red/colour).
6. Pointer feel with figures (the arrow follows at the 8 ms poll like freehand ink).
