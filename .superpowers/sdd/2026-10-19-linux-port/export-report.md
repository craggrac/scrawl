# Linux `scrawl export` (branch linux-export)

## Design

The shared code (`src/export/`: crop choice, output size, camera path, audio window, ripple plan) is unchanged
in behaviour. `export::export` builds a `Plan` and hands it to `render()`: on macOS that writes the JSON plan and
runs `scrawl-helper export`; on Linux it calls `src/linux/export.rs`, which renders the plan with ffmpeg.

Three render paths, the cheapest that is exact:

| Path | When | How |
|-|-|-|
| Direct | no zoom, no ripples | one ffmpeg: crop (exact), lanczos scale, `fps`, libx264/libx265, AAC, audio `-ss videoOffsetMs` |
| Static | ripples, no zoom | decoder ffmpeg crops/scales to raw RGB; Rust draws ripples; encoder ffmpeg adds audio |
| Zoom | `--zoom` | decoder ffmpeg: constant-rate raw RGB at source size; Rust resamples each frame to the camera rectangle (linear interpolation of the plan rows, as `helper/Export.swift`), draws ripples; encoder ffmpeg |

Why Rust resamples the zoom instead of `sendcmd` + `crop` + `scale`: the camera rectangle is fractional and its
size changes every frame. `crop` rounds to whole pixels and its size is fixed at configuration (commands for w/h
exist but rounding remains), which makes a slow zoom step and shimmer when magnifying a 1080p source 1.8x.
`zoompan` has the same rounding. `libplacebo` would be exact but needs Vulkan (not guaranteed, GPU shared with
NVENC). The Rust sampler is bilinear at the exact fractional position when zooming in and an exact area average
when scaling down, is a pure function (unit tested), costs ~4.5 ms/frame at 1388x988 on 6 threads, and the same
code places the ripples at the sampled geometry. A frame is not resampled again when neither the source frame nor
the camera rectangle changed.

Colors: the YUV is read as BT.709 into RGB and written back as BT.709, so the round trip leaves the values as
recorded whatever the file's tags say (wf-recorder tags `bt470bg`); the output is tagged like the source.

## Flags

| Flag | Linux |
|-|-|
| `--crop auto/full/x,y,w,h` | works (shared code; static crop is done by ffmpeg `crop=...:exact=1`) |
| `--size` | works: long-edge cap, even sides, never upscaled |
| `--fps` | works (default 60 for a ~60 fps recording, else 30; the VFR recording becomes constant rate) |
| `--hevc` | libx265 `-crf 25`, `hvc1` tag |
| `--zoom` | works |
| `--no-clicks` | works (drops ripples; with no zoom this selects the one-process path) |
| `--no-ink` | accepted, no effect: the ink is already in `recording.mp4` (overlay is composited on the recorded screen). The CLI prints a note; README documents it |
| `--out` | works; default `$(xdg-user-dir VIDEOS)/Scrawl/<bundle>.mp4`, `~/Videos/Scrawl` if unset or equal to `$HOME` |

Not drawn on Linux: ink/pins (burned in), laser and touch trails (iPhone/macOS only; Linux records neither).

Menu: tray row "Export Last Capture as Video…" when `videoExport` is true in config.json
(`handoff::video_export` is now `cfg(any(macos, linux))`). Runs on a `scrawl-export` thread at niceness 19, notify-send
on start and on completion/failure (with the path). The row appears when the tray menu is next rebuilt (after a
record/stop, or the next start). The in-app path was not exercised live (no app/notifications were started);
it calls the same `export::export` as the CLI.

## Security (review finding)

Manifest `video`/`audio` names must be plain relative paths (`export::member`, rejects absolute, `..`, empty).
The WAV input is opened with `-protocol_whitelist file -f wav` (a playlist posing as audio.wav is not demuxed);
an iPhone movie's own audio and the video go through `media::input` (protocol and format whitelists).

## Measurements (test bundle scrawl-1792611765488-3c8f2: 15.3 s, 1920x1080 VFR, ink burned in, 0 clicks)

Machine load average 9-15 during the runs, everything under `nice -n 19 ionice -c 3`, so these are pessimistic.

| Variant | Result | Wall |
|-|-|-|
| `--crop auto` (default) | 1388x988 @ 60, 0.9 MB | 5.3 s |
| `--crop full` | 1920x1080 @ 60, 1.2 MB | 4.2 s |
| `--size 1280 --crop full` | 1280x720 | 4.3 s |
| `--zoom` | 1388x988, 2.0 MB | 14.6 s (first version 21 s: fps before RGB conversion, cached horizontal pass, repeat-frame reuse) |
| `--crop 0.25,0.25,0.5,0.5 --size 640` | 640x360 | 4.4 s |
| click bundle 1bd99d (12.4 s, 2 clicks): default (ripples) | 1920x1080 | 11.5 s |
| same, `--zoom` | 1920x1080 | 18.0 s |
| same, `--hevc --size 1280 --fps 30` | 1280x720 @ 30 | 12.9 s |

ffprobe on every output: h264 (hevc for the HEVC one) + aac, yuv420p, video `start_time` 0.000000 and audio
`start_time` 0.000000, `bt470bg` tag kept, video duration 15.350 s (source 15.342) / 12.400, audio 15.029 s
(= wav 15.70 s - videoOffsetMs 672) / 12.397. The 15.350 vs 15.029 gap is the bundle's own audio length.
Real lip-sync was not measured (no clap track); alignment follows `crop::audio_window`, as macOS.

## Frame checks (PNG extracted, read)

- Default crop: content matches the source region; burned-in red ink and the pin dots are visible, not doubled.
- `--zoom`: frames 0-2 s wide, then a zoomed view following the ink; text sharp, no visible stepping; returns to wide.
- Ripples (click at 9462 ms of the video clock): absent at 9.40 s, then a blue 30% disc with white and blue rings
  growing and fading at 9.50 / 9.60 / 9.75 s, centred on the click; in `--zoom` the ripple is scaled with the camera.

## Tests

New unit tests in `src/linux/export.rs` (xdg dir parsing, ffprobe color parsing, crop filter, mode selection,
command arguments incl. audio alignment and the locked-down audio input, camera interpolation, sampler
(copy identity, area average, exact-position bilinear), ripple lifetime) and an ffmpeg integration test rendering
all three paths on a generated clip (size, rate, duration, one audio stream, audio starts at ~0). `export::member`
test, tray row test. See the final run in the commit log message / task summary.

## Unresolved / notes

- The tray row's visibility follows the flag only when the menu is rebuilt.
- The in-app notification path was not run live (user rule: nothing visible).
- Zoom export is about real time at 1080p on a loaded machine; the resample is ~4.5 ms/frame and x264 plus
  decode dominate when the machine is idle.
- Frames are not resampled in the linear light domain (as the macOS helper).
