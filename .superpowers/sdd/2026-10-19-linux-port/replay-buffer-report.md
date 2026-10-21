# Replay buffer on Linux ("save what just happened")

Branch `feat-replay-buffer` (from `linux-port`). Not in upstream. Status: DONE for everything that can be
tested without the real screen and microphone; **live testing needed** (below).

## What it is

Opt-in (`"replayBuffer": true`, `"replaySeconds"` 5 to 60, default 30; Settings, Recording has the
switch). While `scrawl app` runs with it on:

- **Screen.** One `wf-recorder -m segment -f $XDG_RUNTIME_DIR/scrawl/replay/seg-%06d.ts` on the focused
  output: 2 s MPEG-TS segments, `h264_nvenc` (`preset=p1 tune=ll`, 3 Mb/s, max 6 Mb/s, `g=60`, `-b 0`)
  with the recorder's existing fall-back to x264 (`ultrafast zerolatency crf 28`, same caps). VFR as
  wf-recorder writes on damage, so a still screen costs nothing. The ring is tmpfs (memory), so an
  always-on recording writes nothing to the SSD. Closed segments older than window + 6 s are deleted
  every second (and the ring is capped at 256 MiB); the recorder gets `PR_SET_PDEATHSIG`.
- **Microphone.** The normal `AudioRecorder` (cpal), its sample vector capped to window + 6 s
  (`keep_last`), about 2.3 MB. New Linux-only methods `tail`, `keep_last`, `failed`.
- **Save** (`scrawl replay` / socket `replay` / tray "Save Last 30 s"): take the audio tail, SIGINT the
  recorder (flushes the open segment, so the last seconds are not lost to buffering), join the
  segments from the one the window starts in and make an mp4 by stream copy
  (`media::clip_segments`), cut the audio to the same span (front-padded with silence if the mic was
  heard for less), then `finalize_capture_bundle` (Parakeet, keyframes, contact sheet, summary), the
  clipboard and a notification, as `scrawl record` does. The summary note says it is a replay without
  ink/clicks. The buffer then starts over.
- The window log / cursor for the past: skipped (no rolling source exists).

### The clip cut (the one non-obvious part)

wf-recorder only writes when the screen changes, so a segment is not 2 s of wall time: after a still
minute one segment holds one picture and spans the minute. A plain `-ss ... -c copy` cannot cut that
(the MPEG-TS demuxer seeks forward to the next key frame and drops the held picture; an mp4 edit list
makes ffmpeg drop it too; both measured). Instead each segment's wall start is known from the file
times (a segment began when the one before it was closed: its mtime; the first, when the buffer
began), and `setts` rewrites the timestamps: `pts' = max(pts - start - skip, N * 1 ms)`. The segment's
first key frame stays the first picture, pictures before the window are squeezed into the first
milliseconds (they cannot be dropped, later ones depend on them), later ones keep their exact times.
Needs no B-frames (`-b 0`). Verified on CFR and on a held-picture stream (tests below); result is a
clean mp4 (first packet pts 0, key frame, no edit list effect) so downstream decoding/seeking is normal.
Pruning keeps a closed segment while its successor began inside the window, so the held picture of a
window that starts in a still stretch is never pruned.

## Interplay with a normal recording: pause the buffer

Both could run (NVENC session limit is not the issue: GeForce limit is at least 3 sessions, and
the driver here showed 0 sessions in use). Chosen instead: the buffer **stops (and discards its ring)
while a recording runs** and restarts when it ends. Reasons: two full-output screencopy+encode
pipelines and two mic streams for the same minutes double the load for no benefit (the recording
is the content); this card has been out of memory on NVENC before, and the recording's NVENC start
must not fight the buffer's for VRAM (a failed one falls back to x264, which costs the recording
quality/CPU); and one clip is always one uninterrupted stretch, which keeps the segment bookkeeping
(no gaps, one timeline) trivial. `set_recording` wakes the supervisor at once from the UI loop (about 8 ms
after the recording starts), so the buffer's recorder overlaps the new one only for its shutdown
(about 0.3 s). Same pause (and discard) while `hypr::session_locked()` (checked every 1 s), so nothing
from before a lock can be saved after it.

## Lifecycle / orphans

Quit, SIGTERM, SIGINT, SIGHUP: `replay::shutdown()` at the start of the quit sequence; the quit wait now also
waits for `replay::running()`. `PR_SET_PDEATHSIG(SIGINT)` is set only on the buffer's recorder: it is
spawned from the long-lived supervisor thread, which is what PDEATHSIG tracks. It is **not** applied
to the normal recording's `wf-recorder` (spawned from `scrawl-wf-recorder`, a thread that ends right
after start; PDEATHSIG there would kill it immediately). The known SIGKILL orphan of normal recordings
stays; moving that spawn onto a long-lived thread would fix it and is a separate change.

## Cost (synthetic source, no real screen or mic)

`ffmpeg testsrc2 1920x1080@30` (every pixel changes: a worst case for a desktop), 12 s real time,
`nice -n 19`, the buffer's encoder settings, segment muxer, tmpfs:

| | CPU (process) | GPU | rate / disk |
|-|-|-|-|
| source only (null encode) | 0.38 s / 12 s | - | - |
| NVENC `p1 ll` | about 0.65 s / 12 s of CPU beyond the source: about 6% of one core | SM 1%, encoder 6% | 3.1 Mb/s: 12 MB per 30 s, 24 MB per 60 s |
| x264 ultrafast zerolatency | 4.5 s / 12 s: 38% of one core (about 1.5 to 2 cores at 144 fps full motion) | - | 4 Mb/s |
| still screen | about 0 (nothing is written) | 0 | 0 |

Disk: tmpfs only, at most window + 6 s of segments (typically under 15 MB), cap 256 MiB. RAM: 64 KB/s
of audio. 2.4 GB tmpfs here. At 144 Hz a busy screen encodes at up to 144 fps on NVENC (still cheap); x264
fallback at that rate would be noticeable, only while the screen moves. No `-r` cap: wf-recorder's fps
filter fills the gap after a still stretch with duplicate frames (minutes of them in a burst).

## Save latency (measured)

- Join 14 segments (about 8 MB): 6.5 ms. Clip of 28 s of 1080p NVENC (10 MB): 117 ms.
- End to end harness (`ffmpeg -re` standing in for wf-recorder, 1080p, 2 s window): recorder stop + join + clip
  + probe: 245 to 285 ms. For a 30 s window expect about 0.4 s; the rest is the normal finish pipeline
  (transcription etc.), which runs after the buffer is already released.

## Tests

`cargo test`: 474 passed, 2 ignored (was 460 before + 14 here, the real-time one is `--ignored`); no new
warnings (the 6 in test builds are the old ones). `/tmp` copy of `mc.sh` pointed at this worktree:
clean (bin and tests).

New unit tests (`linux/replay.rs`): config window default/bounds, segment name parsing and listing
(stray `seg-%06d.ts` file wf-recorder creates is ignored), expiry (open segment never, clock going
back, byte cap), window start clamp, cut point incl. a segment that ran 36 s on a still screen and
after pruning, audio fitting, pause/lock decision order, save refusals; with ffmpeg: segments to clip
(CFR) and a window inside a still stretch (opens on the held picture, change at +3 s; a window after the
change squeezes the earlier pictures; decodes end to end). Also audio `tail/keep_last/failed`, tray row,
settings switch, control word. `a_ring_prunes_as_it_goes_and_a_save_cuts_the_window` (ignored, 11 s)
drives a real `Run` (prune, save, recorder stop, clip, duration) with ffmpeg as the recorder.

## Needs live testing (not done: no real screen or mic allowed)

1. **`wf-recorder -m segment` itself.** Read from wf-recorder 0.6.0 source: it accepts `-m` for any muxer,
   `-p` values reach the encoder options (`g`, `b`, `preset`...), a literal `seg-%06d.ts` file is created
   by its unconditional `avio_open` (harmless, ignored), `-y` skips the exists prompt. The same options
   were run through ffmpeg's CLI segment muxer with NVENC and x264 (key frames, concat, clip), but wf-recorder
   itself was not run. If it fails the log says `Replay buffer: h264_nvenc failed ...` then x264, and a
   notification says it could not start (retries every 30 s).
2. Whether Hyprland's screencopy keeps the idle cost at zero (damage) and the CPU/GPU in practice.
3. Microphone via cpal while idle; mic hot-unplug (retried every 30 s).
4. The Settings switch (one more row and a four-line caption in a 480 px pane: check it fits), the
   tray item, `scrawl replay` end to end, the notification, quit with the buffer running
   (`pgrep wf-recorder` empty), `kill -9` of the app (the buffer's recorder should go; check).
5. Video/audio alignment on a real clip (the video's wall start comes from file times, accurate to the
   encoder's latency: NVENC `ll` and x264 `zerolatency` are tens of ms; a frame-threaded encoder would be
   off by its delay).
6. The buffer follows the output focused when it started; with several monitors a focus change is not
   followed (single 1920x1080@144 here).

## Suggested Hyprland bind (not applied)

```lua
hl.bind("ALT + SHIFT + R", hl.dsp.exec_cmd("scrawl replay"))
```
`bind = ALT SHIFT, R, exec, scrawl replay`

## Privacy

Documented in the README section "Replay buffer": an always-on, opt-in recording; the microphone is
open and recorded (locally, in RAM) the whole time Scrawl runs, and the Noctalia privacy indicator
ignores scrawl and wf-recorder, so it will not show it.
