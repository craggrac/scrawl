# Pin fade (Linux ink overlay)

Problem: a shift-click pin stayed on screen until the capture ended.

Change (Linux overlay only; macOS untouched):
- scene.rs: `Pin` has `alpha`. `build` gives a pin `finished_alpha(now - t1)` (hold 650 ms, fade 650 ms) and drops it afterwards. `animating` now counts pins, so the overlay repaints while a pin holds or fades, then goes idle.
- raster.rs: pin fill and halo are both scaled by `alpha`.
- An empty scene already clears: surface.rs redraws when the scene changes and the raster diff damages the vanished pixels (tested).
- Strokes/marks/keyframes/summary (`pin 1 at ...`) and undo are untouched; only the on-screen scene changes.

Tests: pin alpha full at drop and at 650 ms, 0.25 mid-fade, absent at 1300 ms; scene with only an old pin is empty; animating true while a pin fades, false after; raster draws a half-alpha pin and halo, then clears on an empty scene. Full suite 592 pass; `strokes_in_opposite_corners...` is a timing test that flaked under load and passes singly. mc.sh copy clean (first run hit a gpui_media bindings copy on a fresh target dir; second run clean). `cargo build` ok.

Risks: pin now disappears 1.3 s after drop on screen (intended); recording still has it. Pin alpha is based on t1, so an undo-free pin from an earlier capture clock is gone on a resumed overlay.
