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
