# Slow stop and end-loaded keyframes (user test scrawl-1792611765488-3c8f2)

## Bug 1: stop took 4.2 s

**Root cause.** `wf-recorder` is not slow: SIGINT to exit measured 20 ms (x264 flush, idle desktop) and
320 ms (x264, niced, active screen). The 4.2 s was `linux::record::ScreenSession::stop` decoding the
candidate frames for keyframe selection before returning, serially:

1. `Every(220)` pass over the whole movie (1834 frames at 1080p): 1.65 s.
2. One `ffmpeg -ss` per click/stroke end (16 here, 4 at a time). Each seek decodes from the previous key
   frame (GOP about 250 frames), so the 16 seeks decode more frames than the whole movie: 2.3 s.
3. Three `ffprobe` runs, about 50 ms each.

Running 1 and 2 concurrently gains nothing (both CPU bound: 1.65 + 2.3 s alone, 4.0 s together).
No fixed sleep, no ignored signal, no mux/remux step. The video tail is kept: the recorder is still
SIGINT'ed and waited for (flush) before anything is probed.

**Fix** (one commit).
- `linux::media::sampled_frames`: the grid frames and the urgent frames come from one pass. `ffprobe`
  lists the frame times, Rust picks frame numbers (the frame an `fps` tick would hold; the first frame at
  or after an urgent time), and one `ffmpeg` `select` writes exactly those. Verified pixel-identical to the
  old Every+At output on the real bundle (86/86 frames) and on a test pattern; 3.7 s becomes 1.85 s.
  `select` is built in groups of 32 terms (a flat sum of more than about 100 fails to parse).
- `ScreenSession::stop` now only stops the recorder and probes the file. The candidates are decoded by
  `linux::record::fill_candidates`, called from `finalize_capture_bundle` inside the existing
  thread scope, next to the transcription (the same overlap `scrawl reprocess` already does). Linux-only
  `cfg` block plus returning `frames` from the scope closure in `main.rs` (mechanical on macOS). The
  placeholder-frame fallback is kept; if the one-pass decode fails, the plain `Every` pass is tried.
- Dropped the screen session's event hand-over (`Handover`, `EventSlot`, `hand_events_to`): it only existed
  to give `stop` the events, and `finalize` has them.

**Measured** (same bundle, own harness, niced; transcription is Parakeet on this machine).

| | stop | stop to done |
|-|-|-|
| before (app log) | 4219 ms | 9245 ms |
| before (replayed) | 3.83-3.95 s | 7.34-7.45 s |
| one-pass decode only | 1.82-1.88 s | 5.30-5.38 s |
| one-pass decode, overlapped with transcription | 0.0 s of decoding in stop | 4.78-4.83 s |

A real `wf-recorder` run (x264, because NVENC was out of memory at test time): terminate 320 ms + probe
60 ms. Keyframes of the replay are identical before and after: 13432, 13743, 14752 ms.
The overlap gains less than the sum because decode and transcription compete for the same cores
(transcribe went from 3.4 s to 6.4 s while the decode ran); that is why the one-pass decode matters.

Not measured: the audio stop (it opens the microphone) and the window logger (the app log shows it done
within 46 ms of finish). The audio and window threads run in parallel with the screen stop.

## Bug 2: all keyframes at the end

**Verdict: the shared algorithm doing what it is designed to do on this input. Not changed.**

- The capture is one static screen: the only recorded event is the stop shortcut (a key), no click, no
  drag, no scroll. 16 strokes (9 ink, 7 pins) from 3575 ms to 14842 ms.
- `keyframes::group_marks` (README "Ink, pins"; tests `groups_close_at_a_click_or_too_much_ink`,
  `same_screen_merges_stay_balanced`): marks drawn on the same screen with no click between them share
  one keyframe, taken at the last mark's end (`nearest_frame(last.t1)`), with all their ink redrawn on it.
  A group closes at `GROUP_STROKES = 12` strokes, `GROUP_MS = 20 s`, a click or drag, or when the screen
  around the ink changes by `SAME_SCREEN` (5%).
- Here: strokes 1-12 form group 1 (the 12-stroke cap), keyframe at t1 = 13347 -> frame 13432. Strokes
  13-15 form group 2 (frame 13743). Stroke 16 stays apart from them (screen differs by at least 5%
  around its box), frame 14752. The budget for a 15.9 s capture is 6, so nothing is merged further.
- Text: "Speech before the first keyframe belongs to it", and each keyframe's text runs until the next
  keyframe's start, so everything said until about 13 s lands on #1.
- Evidence it is not Linux timing: the video offset is right (ink first appears in the movie between
  2.9 and 3.2 s; stroke t0 3575 - offset 672 = 2903 ms). Reprocessing a copy gives the same frames.
  Selection on subsets of strokes behaves per the rules (strokes 1-12 alone: one keyframe at 13432;
  13-15 alone: one at 13743; 13-16: two). The one Linux difference, the ink being in the video, does not
  drive this: with every red ink pixel masked out of the candidate frames (macOS-like frames), all 16
  strokes give the same keyframes (13432, 13743, 14752). It does change some other splits (strokes 2-16:
  3 keyframes with ink in the video, 2 masked), always toward more keyframes, never fewer.
- Not a transcript or decode window problem: candidates cover the whole video (86 frames, 672 ms to
  15.3 s).

If keyframes spread over the whole drawing time are wanted, that is a change to the shared grouping
rules (for example a smaller stroke cap or splitting a group when its words exceed some size), which
this task was told not to make.
