# Redact: blur sensitive windows out of captures (Linux)

Branch `feat-redact` (from `linux-port`). Not in upstream. Module: `src/linux/redact.rs`.

## Rules

- Config `redactApps` in `~/.config/scrawl/config.json`: a list of patterns.
  - `zapfast`: case-insensitive part of the window class (what `hyprctl clients` calls `class`).
  - With `*` or `?`: a glob over the whole class (`org.gnome.*`).
  - `title:` in front: the same on the window title (`title:*bank*`), for a site in a browser (the
    whole browser window is hidden).
- Key absent: default list, the password managers `keepassxc`, `bitwarden`, `1password`, `proton pass`,
  `proton-pass`. Key present: exactly that list; `[]` turns it off.
- Why this default: a password manager in a recording an agent reads is a pure leak with nothing to gain
  from showing it, so it is safe to hide unasked. Chat and banking windows are what some captures are
  *about* (a bug in the WhatsApp client), so hiding them by default would silently break those; they are
  documented as additions (`"whatsapp"`, `"zapfast"`).
- A config value that doesn't parse is dropped by the existing lenient loader, which falls back to the
  default list (still protective).

## What is hidden, and how

Order in `finalize_capture_bundle` (Linux): `Redaction::prepare` first (plan from the window log, then
titles replaced), then `Redaction::apply` on the scope thread next to the transcription, before
`fill_candidates` decodes anything. Everything else (candidate frames, `frames/`, `frames-full/`, both
contact sheets, `scrawl export`) is decoded from the rewritten `recording.mp4`, so none of them needs its
own blur and none can miss it.

- Window log: every matched window's title becomes `(redacted)` in `scrawl.json` (`windows`), and
  `target.windowTitle` too (the only title `summary.md` prints). App names stay.
- Video: matched windows are pixelated (blocks of 1/45 of the height) and darkened, not blurred with a
  gaussian, so nothing can be read back. `ffmpeg` filter per region with
  `enable='between(t,a,b)'`, regions from the log's bounds.
- No matching window in the log: nothing is probed, read or rewritten (zero cost).
- Partial rewrite: the video is cut at keyframes (`-f segment -c copy`), only the pieces containing a
  region are re-encoded (`libx264 ultrafast -crf 24`, High profile via `-x264-params`, frames keep their
  exact times: `-fps_mode passthrough -enc_time_base demux`), the others stay as they are, and the pieces
  are joined with the concat demuxer into a new mp4 (audio stream copied if the file has one; wf-recorder
  writes none). The result is checked (same number of packets) before it replaces the original with a
  rename.
- Never an unblurred copy: pieces live under `$XDG_RUNTIME_DIR` (tmpfs; the video's folder if there is
  none) and are removed on drop; on any failure `recording.mp4` is **deleted** (fail closed; the log says
  so) and the bundle gets no video and a placeholder frame. Limits: wf-recorder writes the unblurred file
  while recording, so a capture that is cancelled or crashes before finish leaves it as it was.

### Window log, sampling and what the plan covers

The Linux logger (`hypr::run`) wakes every 100 ms and takes a snapshot every 1 s, or at once when the
focused window changes; snapshots are stored only when the list changes. A window can therefore move,
open or close between two snapshots without the log knowing where. `plan`:

- Each interval between snapshots i and i+1 hides every matched window of **both** (a window that opened
  or closed inside the interval is covered: one sample of padding on each side), a window present in both
  as one box over its path (hull), plus 8 pt around it (border, shadow) and 0.3 s each side for the video
  and the log not agreeing to the frame. Times are moved onto the video's clock (`videoOffsetMs`).
- Partly off screen: cut at the frame edge; fully off screen: ignored.
- Workspace switch / special workspace: Hyprland slides workspaces, and the log doesn't know where a window
  is mid-slide. When a matched window appears or disappears between two snapshots, its whole row and
  column are filled solid black from 0.15 s before to 0.7 s after the snapshot that noticed it.
  (Pixelating them doubled the re-encode cost; see below.)
- Found while on the real machine: ZapFast lives on `special:special` (a scratchpad). `window_entries`
  only listed the monitor's active workspace, so it would never have been in the log. It now also lists the
  special workspace shown over the monitor (`monitors[].specialWorkspace`) in front, and pinned windows.
  This is a (small, shared) change to the log autocrop uses too, and a fix for it.

## Decisions

- **macOS:** untouched. The new module is Linux-only (`pub mod redact` in `src/linux`), the only shared
  edits are cfg-gated or no-ops there: `Config.redact_apps` (a field), `StageTimings.redact_ms` (printed
  only when set), `main.rs` call under `#[cfg(target_os = "linux")]`. Mac recordings come out of Swift
  helper code that has no ffmpeg; the same plan logic could be reused later but nothing needs it now.
  `mc.sh` (adapted copy in /tmp pointing at this worktree) prints no scrawl diagnostics.
- **Window and region captures** are not redacted: the window log only runs for display captures, so there
  is nothing to go by. The target's own title is replaced if its class matches; the picture is not blurred
  (choosing that window is explicit).
- **Settings:** GPUI 0.2.2 in this tree has no text input in the Settings window (the only controls are
  switches, keycap recorder, buttons). Per the brief it is config-only: General › "Blurred windows" shows the
  effective list (read only, truncated) and a caption on how to edit `redactApps`. No input built.
- `dev_context` (browser console/page titles) is not filtered by these patterns.
- Not done: the narration is not touched; NVENC was not used (OOM seen earlier on this machine; x264 only).

## Finish-time cost (measured)

Test machine: 12 threads, **load average 8-18 from other agents' builds**, everything under `nice -n 19`,
so wall times are pessimistic; CPU seconds (rusage of the ffmpeg children) are the honest figure. Input: a
real 15.3 s 1080p wf-recorder capture (1834 frames, 8 keyframes, GOPs of 1.9-2.7 s), a 900x600 window,
log samples every second.

| scenario | pieces re-encoded | CPU s | wall s (loaded) |
|---|---|---|---|
| no matching window | none, nothing probed | 0 | 0 |
| window visible 4 s (5 s of video re-encoded: GOP padding, sample padding) | 1 piece | 24.0 | 7.3 |
| window visible 1 s | 1 piece | 13.1 | 4.5 |
| window there the whole recording | everything | 42.0 | 9.5 |

(Per frame re-encoded about 22 ms of CPU. Before filling the slide bands solid: 36.6 and 20.3 CPU s for the
first two rows; `veryfast` instead of `ultrafast`: 49 and 26. A first, whole-file approach was 41-56 CPU s
for every scenario.) Through the real `finalize_capture_bundle` on a copy of that bundle with the window
visible 4 s: `finish timings: ... redact 8535ms · ... total 12191ms` (loaded, niced; the total less the
redaction, about 3.7 s, is what the same run costs without a matching window). The redaction runs on the scope thread
next to the transcription (Parakeet, ~4-7 s on this machine), so on a real capture the narration hides
part of it, but the keyframe decoding waits for it. A long capture with a window there the whole time is
the expensive case: about 22 ms of CPU per frame, e.g. 40 CPU s per 15 s. The video is blurred
synchronously (before the clipboard is set); it was not made asynchronous because the summary and frames
must not exist before it.

## Samples (real 1080p capture copy, KeePassXC "Bank logins - KeePassXC" fake window 4-9 s)

`/home/ksm/scrawl-redact-samples/`:

- `contact-sheet-full.png`, `contact-sheet.png`, `frames-full-03.png`, `frames-03.png`: from the
  `finalize_capture_bundle` run; `video-4.5s.png` / `video-9.5s.png`: frames of the redacted video;
  `scrawl.json` / `summary.md`: the title is `(redacted)` (`grep "Bank logins"` finds nothing);
  `recording-redacted.mp4`.
- The window shows black because the UI behind it is dark; a light theme gives pixelated grey blocks
  (verified on a test pattern in the unit test). The black column in keyframe 2 is the slide band.

## Tests

- 22 in `linux::redact::tests` (patterns, glob, defaults, title scrubbing, plan: whole-time window, video
  clock + padding, moved window, slide bands, off-screen cut, no match; region pixels; segment planning;
  filter strings; with `ffmpeg`: only the touched pieces rewritten (frames in the copied pieces
  byte-identical to the original, region darkened and blocky, rest within lossy tolerance, solid band black,
  audio kept, packet count equal, no leftover file), whole-recording window, nothing to hide leaves the
  file untouched, a broken recording is deleted).
- `hypr`: a shown special workspace and pinned windows are on screen. `settings`: the view shows the
  configured list or the defaults.
- `nice -n 19 ionice -c 3 cargo test -j 2 -- --test-threads=2`: 527 passed, 3 ignored (as before).

## Commits

See `git log feat-redact`: special workspace in the window log; the redaction module + config + finalize
hook; Settings read-only list; solid slide bands; README and this report.

## Needs live testing

- A real recording with the ZapFast scratchpad toggled over a display capture: the special workspace
  window really appears in the log (`hypr::window_entries` against a live Hyprland), the blur lines up
  with the window, the slide band is enough for Hyprland's `specialWorkspace` animation.
- Offset between the window log's clock and the video (0.3 s padding is a guess for wf-recorder's first
  frame latency).
- A password manager (KeePassXC class `org.keepassxc.KeePassXC`) and the exact class names of Bitwarden,
  1Password and Proton Pass desktop apps (the defaults are substrings, not verified on the machine).
- Finish time on an idle machine and on a long (60 s+) capture with the window visible throughout.
- Playback of the joined `recording.mp4` in the player the user actually uses (parameter sets are
  in-band in the pieces; ffmpeg decodes it, other players untested).

## Follow-up: window and area captures, replay clips (branch `linux-port`)

Closes the two limits listed above ("Window and region captures are not redacted").

- **Window and area captures are redacted.** wf-recorder records both as a screen rectangle
  (`-g`), so what has to be hidden is the screen's matched windows over that rectangle. The window
  log now runs for them too, when the patterns are not empty (`redact::wants_window_log`; with
  `redactApps: []` nothing starts; `platform::logs_windows` is the seam, macOS keeps display-only).
  `hypr::sample_windows(frame)` finds the monitor by its corner (display) or the one holding the
  capture's centre, and `capture_entries` maps bounds into the capture's coordinates (the contract
  `WindowEntry::bounds` already documented: from the capture's corner, negative when hanging off),
  dropping windows that don't reach it. The existing `plan` then clips a matched window at the
  capture's edge, with margin, padding and slide bands unchanged. No cursor sampling for these (the
  input recorder has it). Display captures: unchanged (same monitor lookup, zero shift).
- **A window capture of a matching window** (class or `title:` pattern on the target) is filled solid
  black whole for its entire length, without relying on the log following the window. A region named
  after a window under its centre is not treated as that window.
- **Overlap with other windows** is the same plan: other matched windows overlapping the captured
  window's rectangle are cut in. Known over-blur: a matched window behind the captured one inside
  the rectangle is blurred too (the log has no occlusion test).
- **Cost** (measured on this Hyprland, read-only socket queries): `j/monitors` 0.046 ms and `j/clients`
  0.048 ms wall each (about 0.02 ms of client CPU), once a second, plus the `activewindow` focus
  check at 10 Hz that display captures already do. Nothing to speak of.
- **Not changed:** if Hyprland can't answer during an ordinary recording or area capture the log is
  empty and nothing is hidden (log line `Window log: 0 snapshots`). Only replay saves fail closed.
- **Tests:** `hypr` (window capture maps bounds, area found by centre, display unchanged);
  `redact` (area/window planning in capture coordinates, device none, own-window whole-frame blur
  and the region exception, with ffmpeg: whole-frame black on a window capture, a cut-in overlap in
  an area, the other regions untouched).

## Security fixes (automated review: three fail-open paths, plus the replay ring)

Supersedes the "Not changed" bullet of the follow-up above (an empty log no longer publishes).

1. **Multi-monitor coverage** (`hypr::capture_entries`, `sample_windows`). A capture is no longer
   assigned to one monitor (`monitor_for`, gone). The windows of every monitor are collected, put in the
   capture's coordinates and merged (dedup by window id); the capture must overlap at least one monitor
   (`on_a_monitor`) or the look fails (`None`). Every monitor's windows, not only those of monitors whose
   frame the capture overlaps, because a floating window homed on one monitor (by its centre) is drawn
   wherever it reaches and can hang into a capture on its neighbour. Display captures are one monitor's
   frame, so they could not span monitors (mirrored outputs aside); the `is_display` cursor check now
   requires the same origin and size.
2. **Coordinate mismatch.** The "monitor size equals frame size, return as is" shortcut is gone. The
   translation `monitor - frame` is always applied (zero at a matching origin), and the filter to windows
   that reach the capture runs for display captures too. Tests: an area 1920x1080 offset by 500, a
   display capture of the second monitor, a capture straddling the border, a neighbour's window hanging in.
3. **Empty or untrustworthy window log** (`redact::Redaction::prepare`, `hypr::Coverage`).
   - Patterns non-empty and the log has no snapshot at all (recording, window or area capture; the
     default list counts): the whole picture is filled solid black for the whole recording through the
     same rewrite path (so a failure still deletes the video), and `summary.md` gets the notice as its
     `Note:` line (user's note follows it), plus a `notify-send` (not in tests). Chosen over refusing or
     deleting like replay: replay can refuse because nothing has been produced yet and the buffer
     is lost, here the user has just recorded narration, strokes and events that carry no pixels and
     would be thrown away; a black picture with an explicit notice is the least information lost
     without a leak. Cost: one full re-encode (about 40 CPU s per 15 s of 1080p, a rare path).
   - `redactApps: []` is no redaction and no log, no cost (unchanged). A log that worked and saw no
     window is a log. Phone captures are skipped; a window capture of a matching window is black as
     before, without the notice.
   - Gaps: the logger records a failed look as nothing, so `Coverage` notes when more than 3 s (the
     ring's `MAX_GAP`) pass between good looks, before the first, or after the last until the stop;
     `ActivityLog::window_gaps`. With gaps every matched window seen anywhere (`held_everywhere`, shared
     with `replay_windows`) is blurred for the whole recording in the box that holds everywhere it was,
     added to the plan; the log in `scrawl.json` is kept as it is (not replaced by the held box).
     Seen nowhere means nothing to hide, as in replay.
   - The race of a startup config change (`[]` at start, patterns at finish) is closed too: no log,
     black picture.
4. **Replay ring ordering** (`window_ring`, separate commit). The final look and the background look now
   both happen and are stored under the log's lock; `record` drops a look older than the newest held;
   `slice` includes looks stored after the end, as if at the end (a window that appeared just before the
   save is blurred in the last interval). Tests: stale record, and a save racing a background look.

Tests: `cargo test` 587 passed (hypr: 6 new, `Coverage`; redact: no-log blackout for every kind of capture,
`[]` off, gaps, ffmpeg black video with audio and every frame kept; ring: 2 new).
