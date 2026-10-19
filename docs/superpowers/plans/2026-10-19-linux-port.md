# Scrawl on Linux (phases 1–2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `scrawl` builds and runs on Linux/Hyprland: reads bundles (phase 1) and records screen + voice into a full `.scrawl` bundle from a Hyprland key bind (phase 2).

**Architecture:** macOS-only modules are gated with `#[cfg(target_os = "macos")]`; `capture.rs` is split into a portable `capture/mod.rs` and `capture/mac.rs`. Linux backends live in `src/linux/` and shell out to system tools: `ffprobe`/`ffmpeg` (movie work the Swift helper does on macOS), `sherpa-onnx-ws` + Parakeet (transcription), Hyprland's IPC socket (target, windows, cursor), `wf-recorder` (screen), `wl-copy` (clipboard), `notify-send`.

**Tech Stack:** Rust 2024, clap, serde, cpal (already), `ab_glyph` and `tungstenite` (new, Linux only), libc (already).

**Spec:** `docs/superpowers/specs/2026-10-19-linux-port-design.md`

## Global Constraints

- The macOS build must keep compiling and behave exactly as before. Linux code goes behind `#[cfg(target_os = "linux")]` or in `src/linux/`. This machine cannot build for macOS, so macOS-side edits must be mechanical (moves and `cfg` attributes only) and listed in each commit message.
- **Never run `cargo` (build, check, test, add, fetch) without first telling the user and getting a go-ahead.** Every cargo command runs as `nice -n 19 ionice -c 3 cargo <cmd> -j 2` (tests add `-- --test-threads=2`). The commands below are written that way.
- New crates only: `ab_glyph`, `tungstenite` (both under `[target.'cfg(target_os = "linux")'.dependencies]`). Adding them needs network for `Cargo.lock`; say so when asking.
- Runtime tools: `ffmpeg`/`ffprobe`, `wf-recorder`, `wl-copy`, `notify-send`, `fc-match`, `hyprctl` socket, OpenWhispr's `sherpa-onnx-ws-linux-x64` and `~/.cache/openwhispr/parakeet-models/parakeet-tdt-0.6b-v3`.
- No automatic OpenCode delivery on Linux; clipboard only.
- Match the surrounding style: `//!` module docs, `///` on public items, `anyhow::{Context, Result, bail, ensure}`, `crate::capture::scrawl_log` for logging.
- Work on branch `linux-port` in `/home/ksm/scrawl`. Commit after each task with a `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` trailer.

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `src/capture.rs` → `src/capture/mac.rs` | move | ScreenCaptureKit, Carbon hotkeys, event taps (macOS) |
| `src/capture/mod.rs` | create | portable capture pieces: log, candidate buffer, `ScreenRecording`, `capture_size`, `write_bundle_artifacts` |
| `src/windows.rs` | modify | `cfg(macos)` on CGWindowList code; types stay portable |
| `src/main.rs` | modify | `cfg` on macOS modules/commands; `Toggle` command; Linux `record` arm |
| `src/phone.rs`, `src/audio.rs`, `src/helper.rs`, `src/handoff.rs` | modify | gate macOS bits; Linux clipboard; Linux helper backends; data dir |
| `src/render.rs` | modify | Linux text via `ab_glyph` + `fc-match` |
| `src/hex.rs` | modify | `ends_sentence` becomes `pub(crate)` |
| `src/linux/mod.rs` | create | `record()` orchestration, notifications, stop signals |
| `src/linux/media.rs` | create | ffprobe/ffmpeg: probe, frames, audio, shrink |
| `src/linux/parakeet.rs` | create | sherpa-onnx server + WebSocket client, tokens → words |
| `src/linux/hypr.rs` | create | Hyprland IPC: target, window/cursor logger |
| `src/linux/record.rs` | create | `wf-recorder` session, candidate frames |
| `src/linux/toggle.rs` | create | pidfile, `scrawl toggle` |
| `src/linux/fixtures/*.json` | create | real tool outputs for tests |
| `README.md` | modify | Linux section (deps, bind) |

---

### Task 1: Compile on Linux

**Files:**
- Move: `src/capture.rs` → `src/capture/mac.rs`
- Create: `src/capture/mod.rs`
- Modify: `src/main.rs`, `src/windows.rs`, `src/phone.rs`, `src/audio.rs`, `src/helper.rs`, `src/handoff.rs`, `src/autocrop_lab.rs` (only if needed)

**Interfaces:**
- Produces: `crate::capture::{scrawl_log, FRAME_SAMPLE_INTERVAL, CANDIDATE_EDGE, CANDIDATE_BUDGET_BYTES, CandidateBuffer, ScreenRecording, capture_size, write_bundle_artifacts}` on every OS; `crate::windows::{ActivityLog, WindowSnapshot, WindowEntry, CursorSample}` on every OS, with `ActivityLog::push_windows(&mut self, t: u64, windows: Vec<WindowEntry>) -> bool` and `push_cursor(&mut self, t: u64, x_pt: f64, y_pt: f64, frame: TargetFrame) -> bool`.

The current Linux `cargo check` fails with about 250 errors, all in macOS-only code. This task is driven by the compiler. Rules for every error:
1. If the item exists only to serve macOS UI or capture, put `#[cfg(target_os = "macos")]` on it, and on its callers if they are macOS-only too.
2. Never write a Linux stub that pretends to do a macOS feature. A Linux command arm that can't work yet returns `bail!` with a clear message.
3. Never change a macOS code path, only add `cfg` attributes or move code verbatim.

- [ ] **Step 1: Split `capture`**

```bash
cd /home/ksm/scrawl && mkdir -p src/capture && git mv src/capture.rs src/capture/mac.rs
```

Cut these items from `src/capture/mac.rs` and paste them, unchanged and with their doc comments, into a new `src/capture/mod.rs`:
- `FRAME_SAMPLE_INTERVAL`, `CANDIDATE_EDGE`, `CANDIDATE_EDGE_NO_VIDEO` (stays private but in `mod.rs`; `mac.rs` reaches it through `use super::*`), `CANDIDATE_BUDGET_BYTES`, `CandidateBuffer` with its `impl`, and `ScreenRecording` (old lines 21–110).
- `scrawl_log` (old lines 527–546).
- `capture_size` (old lines 2269–2277).
- `write_bundle_artifacts` (old lines 3597–3635).
- Any test in `mac.rs`'s `mod tests` that touches only these items (for example `CandidateBuffer` tests that call `len()`) goes into a `#[cfg(test)] mod tests` in `mod.rs`.

Head of `src/capture/mod.rs`:

```rust
//! The portable half of capture: candidate frames, the finished recording, writing a bundle and
//! the log. The recorder itself is platform code (`mac` here; `crate::linux` on Linux).

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::model::{RgbaFrame, TargetInfo};

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::*;
```

At the top of `src/capture/mac.rs`, after its own `use` lines, add `use super::*;`. Remove from `mac.rs` any `use` that only served the moved items, but only if the compiler warns that it is unused.

- [ ] **Step 2: Gate macOS-only modules in `src/main.rs`**

Put `#[cfg(target_os = "macos")]` on `mod ax; mod hold; mod icons; mod laser_overlay; mod native; mod onboarding; mod screens; mod settings; mod sparkle; mod ui;`. Then split the import at `main.rs:44`:

```rust
use crate::capture::write_bundle_artifacts;
#[cfg(target_os = "macos")]
use crate::capture::{ScreenCaptureSession, discover_target};
```

- [ ] **Step 3: Gate macOS commands in `src/main.rs`**

- Put `#[cfg(target_os = "macos")]` on the `Command` variants `App`, `Lab`, `Tune`, `SckProbe`, `Windows`, `Preview`, on their match arms, and on the helper enums only they use (`TuneCommand`, the `ui::LabScene` field, and so on).
- `None =>` (no subcommand): keep the current code under `cfg(macos)`. On Linux use:

```rust
#[cfg(not(target_os = "macos"))]
None => {
    use clap::CommandFactory;
    Cli::command().print_help()?;
    return Ok(());
}
```

  Use the parser struct's real name from `main.rs` in place of `Cli`.
- `Command::Record { iphone: true, .. }` and `Command::Record { iphone: false, .. }`: put `#[cfg(target_os = "macos")]` on both existing arms. Add a temporary Linux arm, which Task 9 replaces:

```rust
#[cfg(not(target_os = "macos"))]
Command::Record { .. } => anyhow::bail!("Recording on Linux arrives with `scrawl toggle`"),
```

- [ ] **Step 4: Gate the rest by compiler error**

Run (ask the user first): `cd /home/ksm/scrawl && nice -n 19 ionice -c 3 cargo check -j 2 2>&1 | grep -E '^(error|warning)' | sort | uniq -c | sort -rn | head -40`

Expected spots, all handled with rule 1:
- `src/windows.rs`: put `cfg(macos)` on `RawWindow`, `AppMeta`, `filter_windows`, `owned_window_at`, `raw_windows`, `dock_rect`, `snapshot`, `WindowLogger` (struct, impls, `Drop`), `fn run`, the interval/tick constants, and the tests that use them. `WindowSnapshot`, `WindowEntry`, `CursorSample`, `ActivityLog` and its methods stay portable.
- `src/phone.rs`: put `cfg(macos)` on `PhoneSession` and everything using `SharedCaptureTelemetry` or `LiveOverlayEvent` (live mirror, `start_ingest_server`, ingest-server items that need the UI channel), plus the matching `main.rs` arms (`Serve`, `Link`, `Devices`, `record --iphone`) if they stop compiling. `import`, `resolve_import`, `build_movie_bundle`, `MovieCapture` and `finalize_live_capture` must stay portable, because `scrawl import`/`reprocess` need them.
- `src/audio.rs:41`: wrap `crate::ui::start_metric(...)` in `#[cfg(target_os = "macos")]`.
- `src/helper.rs:101`: `#[cfg(target_os = "macos")]` on `permissions`.
- `src/handoff.rs:585`: `#[cfg(target_os = "macos")]` on `pill_style`.
- `src/main.rs` `Command::Mic { Watch }`: keep it if `mic::Watch` compiles (`mic.rs` already has a non-macOS branch).

Repeat the check until it reports 0 errors. Clear warnings caused by gating (unused imports or functions that only macOS uses) with `#[cfg(target_os = "macos")]` on the item. Never use `#[allow]`.

- [ ] **Step 5: Build and run the existing tests**

Run (ask first): `nice -n 19 ionice -c 3 cargo build -j 2 && nice -n 19 ionice -c 3 cargo test -j 2 -- --test-threads=2`

Expected: the build passes. In the test run, only tests that depend on macOS may fail, for example tests calling `helper::` (they need `scrawl-helper`) or reading `~/Library`. Gate each of those with `#[cfg(target_os = "macos")]` on the test and add a one-line comment saying why. A failure in pure logic (`keyframes`, `sketch`, `render`, `routing`, `handoff`, `model`, `autocrop`, `agent_context`, `hex`, `zoom`, `export`) is a real bug: stop and report it.

- [ ] **Step 6: Smoke test**

Run: `SCRAWL_CAPTURES_DIR=/tmp/scrawl-caps ./target/debug/scrawl demo --out /tmp/demo.scrawl && ./target/debug/scrawl inspect /tmp/demo.scrawl | head -20 && ls /tmp/demo.scrawl`
Expected: the summary prints, and `contact-sheet.png`, `frames/`, `scrawl.json` and `summary.md` exist. The text in the contact sheet still uses the 5×7 fallback; Task 4 fixes that.

- [ ] **Step 7: Commit**

```bash
git add -A src && git commit -m "build: compile on Linux by gating macOS-only modules

capture.rs splits into capture/mod.rs (portable: log, candidates,
ScreenRecording, capture_size, write_bundle_artifacts) and capture/mac.rs
(moved verbatim). macOS edits are cfg attributes and verbatim moves only.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Linux data and log paths

**Files:**
- Modify: `src/handoff.rs:10-18`, `src/capture/mod.rs` (`scrawl_log`)

**Interfaces:**
- Produces: `handoff::default_captures_dir()` keeps its signature. It now returns `dirs::data_dir()/scrawl/captures`, which on macOS is the same path as before.

- [ ] **Step 1: Write the failing test** (in `handoff.rs`'s existing `#[cfg(test)] mod tests`)

```rust
#[test]
fn captures_live_in_the_platform_data_dir() {
    if std::env::var_os("SCRAWL_CAPTURES_DIR").is_some() {
        return;
    }
    let expected = dirs::data_dir().unwrap().join("scrawl/captures");
    assert_eq!(default_captures_dir(), expected);
}
```

- [ ] **Step 2: Run it to see it fail on Linux**

Run (ask first): `nice -n 19 ionice -c 3 cargo test -j 2 captures_live_in -- --test-threads=2`
Expected: FAIL, because the left side ends in `Library/Application Support/scrawl/captures`.

- [ ] **Step 3: Implement**

```rust
pub fn default_captures_dir() -> PathBuf {
    std::env::var_os("SCRAWL_CAPTURES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // `~/Library/Application Support` on macOS, `~/.local/share` on Linux.
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("scrawl/captures")
        })
}
```

In `scrawl_log` (`src/capture/mod.rs`), replace the `let Some(dir) = ...` line with:

```rust
    #[cfg(target_os = "macos")]
    let dir = dirs::home_dir().map(|home| home.join("Library/Logs/Scrawl"));
    #[cfg(not(target_os = "macos"))]
    let dir = dirs::state_dir().map(|state| state.join("scrawl"));
    let Some(dir) = dir else {
        return;
    };
```

- [ ] **Step 4: Run the test**

Same command as Step 2. Expected: PASS.

- [ ] **Step 5: Commit** with message `fix(linux): captures in the XDG data dir, log in the XDG state dir`, plus the trailer.

---

### Task 3: Movie work with ffmpeg

**Files:**
- Create: `src/linux/mod.rs`, `src/linux/media.rs`, `src/linux/fixtures/ffprobe.json`
- Modify: `src/main.rs` (add `#[cfg(target_os = "linux")] mod linux;`), `src/helper.rs`

**Interfaces:**
- Consumes: `crate::helper::{Probe, FrameTimes}` and `crate::retention::Shrunk` (existing).
- Produces, on Linux, re-exported from `helper` so callers don't change:
  - `media::probe(&Path) -> Result<Probe>`
  - `media::frames(&Path, FrameTimes, u32) -> Result<Vec<RgbaFrame>>`
  - `media::export_audio(&Path, &Path) -> Result<()>`
  - `media::shrink(&Path, &Path, u32) -> Result<Shrunk>`
  - `media::scaled_size(u32, u32, u32) -> (u32, u32)`

- [ ] **Step 1: Fixture.** Write `src/linux/fixtures/ffprobe.json`:

```json
{
    "programs": [],
    "stream_groups": [],
    "streams": [
        { "codec_type": "video", "width": 320, "height": 240, "avg_frame_rate": "30/1" },
        { "codec_type": "audio", "avg_frame_rate": "0/0" }
    ],
    "format": { "duration": "2.000000" }
}
```

- [ ] **Step 2: Write the failing tests** (in `src/linux/media.rs`; this step also creates the file, holding only the module doc and the tests below)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reads_size_rate_duration_and_audio() {
        let probe = parse_probe(include_bytes!("fixtures/ffprobe.json")).unwrap();
        assert_eq!(
            (probe.duration_ms, probe.width, probe.height, probe.audio_tracks),
            (2_000, 320, 240, 1)
        );
        assert!((probe.fps - 30.0).abs() < 1e-9);
    }

    #[test]
    fn scaled_size_caps_the_long_edge_and_stays_even() {
        assert_eq!(scaled_size(1920, 1080, 480), (480, 270));
        assert_eq!(scaled_size(1920, 1080, 0), (1920, 1080));
        assert_eq!(scaled_size(300, 200, 480), (300, 200));
        assert_eq!(scaled_size(1081, 1921, 480), (270, 480));
    }

    /// Needs `ffmpeg` on PATH: a 2 s test pattern, decoded both ways.
    #[test]
    fn decodes_frames_from_a_real_movie() {
        let dir = std::env::temp_dir().join(format!("scrawl-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let movie = dir.join("t.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30"])
            .args(["-t", "2", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&movie)
            .status()
            .unwrap();
        assert!(made.success());
        let every = frames(&movie, FrameTimes::Every(500), 160).unwrap();
        assert!((4..=5).contains(&every.len()), "{} frames", every.len());
        assert_eq!((every[0].width, every[0].height), (160, 120));
        assert_eq!(every[1].t_ms, 500);
        let at = frames(&movie, FrameTimes::At(&[100, 1_900]), 0).unwrap();
        assert_eq!(at.iter().map(|f| f.t_ms).collect::<Vec<_>>(), vec![100, 1_900]);
        assert_eq!(at[0].rgba.len(), 320 * 240 * 4);
        let wav = dir.join("a.wav");
        assert!(export_audio(&movie, &wav).is_err(), "no audio track to export");
        let _ = std::fs::remove_dir_all(dir);
    }
}
```

`src/linux/mod.rs` for now:

```rust
//! Linux backends for what macOS does with ScreenCaptureKit, AppKit and `scrawl-helper`.

pub mod media;
```

- [ ] **Step 3: Run the tests to see them fail**

Run (ask first): `nice -n 19 ionice -c 3 cargo test -j 2 linux::media -- --test-threads=2`
Expected: compile errors, because `parse_probe`, `scaled_size`, `frames` and `export_audio` do not exist yet.

- [ ] **Step 4: Implement `src/linux/media.rs`** (above the tests)

```rust
//! The movie work `scrawl-helper` does with AVFoundation on macOS, done with `ffprobe` and
//! `ffmpeg` on Linux. Frames come back as raw RGBA over stdout, so no temporary files.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::helper::{FrameTimes, Probe};
use crate::model::RgbaFrame;
use crate::retention::Shrunk;

#[derive(Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Deserialize)]
struct FfprobeStream {
    codec_type: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    avg_frame_rate: String,
}

#[derive(Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
}

pub fn parse_probe(json: &[u8]) -> Result<Probe> {
    let out: FfprobeOutput = serde_json::from_slice(json).context("Invalid ffprobe output")?;
    let video = out.streams.iter().find(|s| s.codec_type == "video");
    let seconds: f64 = out
        .format
        .and_then(|f| f.duration)
        .and_then(|d| d.parse().ok())
        .context("ffprobe found no duration")?;
    Ok(Probe {
        duration_ms: (seconds * 1_000.0).round() as u64,
        width: video.map_or(0, |v| v.width),
        height: video.map_or(0, |v| v.height),
        audio_tracks: out.streams.iter().filter(|s| s.codec_type == "audio").count() as u32,
        fps: video.map_or(0.0, |v| frame_rate(&v.avg_frame_rate)),
    })
}

/// `"30000/1001"` → 29.97; anything unreadable is 0 (unknown).
fn frame_rate(rate: &str) -> f64 {
    match rate.split_once('/') {
        Some((n, d)) => match (n.parse::<f64>(), d.parse::<f64>()) {
            (Ok(n), Ok(d)) if d > 0.0 => n / d,
            _ => 0.0,
        },
        None => rate.parse().unwrap_or(0.0),
    }
}

pub fn probe(movie: &Path) -> Result<Probe> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_entries"])
        .arg("format=duration:stream=codec_type,width,height,avg_frame_rate")
        .arg(movie)
        .output()
        .context("Could not run ffprobe (install ffmpeg)")?;
    ensure!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    parse_probe(&out.stdout)
}

/// The decoded size for `max_edge` (0 keeps native size), even on both sides as encoders want.
pub fn scaled_size(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let long = width.max(height).max(1);
    let (w, h) = if max_edge == 0 || long <= max_edge {
        (width, height)
    } else {
        let scale = |side: u32| (u64::from(side) * u64::from(max_edge) / u64::from(long)) as u32;
        (scale(width), scale(height))
    };
    (w.max(2) & !1, h.max(2) & !1)
}

/// Runs ffmpeg with `filter` and splits its raw RGBA stdout into `width`×`height` frames.
fn decode(
    movie: &Path,
    seek_ms: Option<u64>,
    filter: &str,
    (width, height): (u32, u32),
    limit: Option<usize>,
) -> Result<Vec<Vec<u8>>> {
    let mut command = Command::new("ffmpeg");
    command.args(["-v", "error", "-nostdin"]);
    if let Some(ms) = seek_ms {
        command.args(["-ss", &format!("{:.3}", ms as f64 / 1_000.0)]);
    }
    command.arg("-i").arg(movie).args(["-an", "-vf", filter]);
    if let Some(limit) = limit {
        command.args(["-frames:v", &limit.to_string()]);
    }
    command.args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Could not run ffmpeg (install ffmpeg)")?;
    let mut stdout = child.stdout.take().context("No ffmpeg stdout")?;
    let size = width as usize * height as usize * 4;
    let mut frames = Vec::new();
    loop {
        let mut frame = vec![0; size];
        match stdout.read_exact(&mut frame) {
            Ok(()) => frames.push(frame),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
        }
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(frames)
}

/// Decodes movie frames into memory; `max_edge` 0 keeps native size.
pub fn frames(movie: &Path, times: FrameTimes, max_edge: u32) -> Result<Vec<RgbaFrame>> {
    let probe = probe(movie)?;
    ensure!(probe.width > 0 && probe.height > 0, "{} has no video", movie.display());
    let size = scaled_size(probe.width, probe.height, max_edge);
    let scale = format!("scale={}:{}:flags=area", size.0, size.1);
    match times {
        FrameTimes::Every(ms) => {
            let ms = ms.max(1);
            let filter = format!("fps=1000/{ms},{scale}");
            Ok(decode(movie, None, &filter, size, None)?
                .into_iter()
                .enumerate()
                .map(|(i, rgba)| RgbaFrame::new(i as u64 * ms, size.0, size.1, rgba))
                .collect())
        }
        FrameTimes::At(times) => {
            // A seek at or past the end decodes nothing; aim just before it.
            let last = probe.duration_ms.saturating_sub(50);
            let chunk = times.len().div_ceil(4).max(1);
            std::thread::scope(|scope| -> Result<Vec<RgbaFrame>> {
                let workers: Vec<_> = times
                    .chunks(chunk)
                    .map(|chunk| {
                        let scale = &scale;
                        scope.spawn(move || -> Result<Vec<RgbaFrame>> {
                            chunk
                                .iter()
                                .map(|&t| {
                                    let rgba = decode(movie, Some(t.min(last)), scale, size, Some(1))?
                                        .pop()
                                        .with_context(|| format!("No frame at {t} ms"))?;
                                    Ok(RgbaFrame::new(t, size.0, size.1, rgba))
                                })
                                .collect()
                        })
                    })
                    .collect();
                let mut out = Vec::with_capacity(times.len());
                for worker in workers {
                    out.extend(
                        worker
                            .join()
                            .map_err(|_| anyhow::anyhow!("Frame decode worker panicked"))??,
                    );
                }
                Ok(out)
            })
        }
    }
}

/// Mixes a movie's audio to 16 kHz mono WAV.
pub fn export_audio(movie: &Path, wav: &Path) -> Result<()> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(movie)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le"])
        .arg(wav)
        .output()
        .context("Could not run ffmpeg (install ffmpeg)")?;
    ensure!(
        out.status.success(),
        "ffmpeg could not export audio: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Re-encodes `movie` into `out` as HEVC with its long edge at most `max_edge` and the audio
/// copied, at the lowest CPU priority.
pub fn shrink(movie: &Path, out: &Path, max_edge: u32) -> Result<Shrunk> {
    let started = Instant::now();
    let before = std::fs::metadata(movie)?.len();
    let probe = probe(movie)?;
    let (width, height) = scaled_size(probe.width, probe.height, max_edge);
    let output = Command::new("nice")
        .args(["-n", "19", "ffmpeg", "-v", "error", "-nostdin", "-y", "-i"])
        .arg(movie)
        .args(["-vf", &format!("scale={width}:{height}")])
        .args(["-c:v", "libx265", "-preset", "fast", "-crf", "28", "-tag:v", "hvc1", "-c:a", "copy"])
        .arg(out)
        .output()
        .context("Could not run ffmpeg (install ffmpeg)")?;
    ensure!(
        output.status.success(),
        "ffmpeg could not shrink: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(Shrunk {
        before,
        after: std::fs::metadata(out)?.len(),
        width,
        height,
        ms: started.elapsed().as_millis() as u64,
    })
}
```

The `export_audio` test expects an error for a movie with no audio track: `ffmpeg` fails with "Output file does not contain any stream". If `Shrunk` has private fields, make them `pub` (they are already `pub` at `retention.rs:492`).

- [ ] **Step 5: Route `helper` to it on Linux.** In `src/helper.rs`, put `#[cfg(not(target_os = "linux"))]` on the existing `probe`, `shrink`, `export_audio`, `frames` and on the private `FrameFile` struct, then add:

```rust
#[cfg(target_os = "linux")]
pub use crate::linux::media::{export_audio, frames, probe, shrink};
```

In `src/main.rs`, next to the other `mod` lines, add `#[cfg(target_os = "linux")] mod linux;`.

- [ ] **Step 6: Run the tests**

Same command as Step 3. Expected: 3 PASS.

- [ ] **Step 7: Check `reprocess` end to end**

Run: `./target/debug/scrawl import /tmp/scrawl-probe/t.mp4 --out /tmp/import.scrawl 2>&1 | tail -5; ls /tmp/import.scrawl/frames | head`
`/tmp/scrawl-probe/t.mp4` is a 2 s test pattern made while planning. If it is gone, recreate it with the `ffmpeg` line from the test.
Expected: the bundle builds with frames. Transcription says it failed (the helper is missing), and Task 6 fixes that.

- [ ] **Step 8: Commit** with message `feat(linux): probe, frames, audio and shrink through ffmpeg`, plus the trailer.

---

### Task 4: Real fonts in the contact sheet

**Files:**
- Modify: `Cargo.toml`, `src/render.rs:1574-1720`

**Interfaces:**
- Produces: `text_width` / `draw_text` keep their signatures. On Linux they draw with the system sans font.

- [ ] **Step 1: Add the crate.** Ask first and mention that it needs the network: `nice -n 19 cargo add ab_glyph --target 'cfg(target_os = "linux")'`

- [ ] **Step 2: Write the failing test** (in `render.rs`'s existing test module)

```rust
#[cfg(target_os = "linux")]
#[test]
fn linux_text_uses_a_real_font() {
    let (width, height, pixels) =
        linux_text::mask("Scrawl ação", 16.0, false).expect("fc-match found no sans font");
    assert!(width > 40 && height >= 16, "{width}x{height}");
    assert!(pixels.iter().any(|&p| p > 200));
    // The 5×7 fallback gives every character the same width; a real font doesn't.
    assert!(text_width("WWWW", 16.0, true) > text_width("iiii", 16.0, true) * 1.5);
}
```

- [ ] **Step 3: Run to see it fail.** Ask first: `nice -n 19 ionice -c 3 cargo test -j 2 linux_text -- --test-threads=2`. Expected: compile error, because `linux_text` does not exist.

- [ ] **Step 4: Implement.** After `mod native_text { … }`, add:

```rust
#[cfg(target_os = "linux")]
mod linux_text {
    use std::sync::OnceLock;

    use ab_glyph::{Font, FontVec, Glyph, PxScale, ScaleFont, point};

    fn load(pattern: &str) -> Option<FontVec> {
        let out = std::process::Command::new("fc-match")
            .args(["-f", "%{file}", pattern])
            .output()
            .ok()?;
        let path = String::from_utf8(out.stdout).ok()?;
        FontVec::try_from_vec(std::fs::read(path.trim()).ok()?).ok()
    }

    /// The system's sans font, found once by fontconfig.
    fn font(bold: bool) -> Option<&'static FontVec> {
        static REGULAR: OnceLock<Option<FontVec>> = OnceLock::new();
        static BOLD: OnceLock<Option<FontVec>> = OnceLock::new();
        if bold {
            BOLD.get_or_init(|| load("sans:bold"))
        } else {
            REGULAR.get_or_init(|| load("sans"))
        }
        .as_ref()
    }

    /// One line of glyphs from x = 0 with the baseline at the ascent, and its advance.
    fn layout(font: &FontVec, text: &str, size: f32) -> (Vec<Glyph>, f32) {
        let scaled = font.as_scaled(PxScale::from(size));
        let mut x = 0.0;
        let mut previous = None;
        let glyphs = text
            .chars()
            .map(|ch| {
                let id = scaled.glyph_id(ch);
                if let Some(previous) = previous {
                    x += scaled.kern(previous, id);
                }
                let glyph = id.with_scale_and_position(size, point(x, scaled.ascent()));
                x += scaled.h_advance(id);
                previous = Some(id);
                glyph
            })
            .collect();
        (glyphs, x)
    }

    pub fn width(text: &str, size: f32, bold: bool) -> Option<f32> {
        Some(layout(font(bold)?, text, size).1)
    }

    /// A coverage mask with the same 2 px margin as the CoreText one.
    pub fn mask(text: &str, size: f32, bold: bool) -> Option<(usize, usize, Vec<u8>)> {
        let font = font(bold)?;
        let scaled = font.as_scaled(PxScale::from(size));
        let (glyphs, advance) = layout(font, text, size);
        let width = advance.ceil().max(1.0) as usize + 4;
        let height =
            (scaled.ascent() - scaled.descent() + scaled.line_gap()).ceil().max(1.0) as usize + 4;
        let mut pixels = vec![0_u8; width * height];
        for glyph in glyphs {
            let Some(outline) = font.outline_glyph(glyph) else {
                continue;
            };
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                let px = bounds.min.x as i64 + i64::from(gx) + 2;
                let py = bounds.min.y as i64 + i64::from(gy) + 2;
                if (0..width as i64).contains(&px) && (0..height as i64).contains(&py) {
                    let cell = &mut pixels[py as usize * width + px as usize];
                    *cell = (*cell).max((coverage.clamp(0.0, 1.0) * 255.0) as u8);
                }
            });
        }
        Some((width, height, pixels))
    }
}

#[cfg(target_os = "macos")]
use native_text::mask as text_mask;
#[cfg(target_os = "linux")]
use linux_text::mask as text_mask;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn text_mask(_text: &str, _size: f32, _bold: bool) -> Option<(usize, usize, Vec<u8>)> {
    None
}
```

In `text_width`, after the macOS block, add:

```rust
    #[cfg(target_os = "linux")]
    if let Some(width) = linux_text::width(text, size, bold) {
        return width;
    }
```

In `draw_text`, replace `#[cfg(target_os = "macos")] if let Some((width, height, pixels)) = native_text::mask(text, size, bold) {` with an unconditional `if let Some((width, height, pixels)) = text_mask(text, size, bold) {`. The body stays the same.

- [ ] **Step 5: Run the test.** Same command as Step 3. Expected: PASS.

- [ ] **Step 6: Look at it.** Run `./target/debug/scrawl demo --out /tmp/demo2.scrawl >/dev/null`, then read `/tmp/demo2.scrawl/contact-sheet.png` with the Read tool. Expected: badges such as `#1 0:02` show smooth text, not block letters.

- [ ] **Step 7: Commit** with message `feat(linux): contact-sheet text with the system font via ab_glyph`, plus the trailer. Include `Cargo.toml` and `Cargo.lock`.

---

### Task 5: Clipboard and `open` on Linux

**Files:**
- Modify: `src/handoff.rs:99-195`, `src/main.rs` (`Autocrop` arm, `std::process::Command::new("open")`)

**Interfaces:**
- Produces: `handoff::copy_bundle_to_clipboard(&Path) -> Result<()>` works on Linux. It copies `summary.md`, which names the contact sheet and frames by absolute path.

- [ ] **Step 1: Implement.** In `handoff.rs`, change `#[cfg(target_os = "macos")]` on `copy_bundle_to_clipboard` to `#[cfg(any(target_os = "macos", target_os = "linux"))]`. Change the `#[cfg(not(target_os = "macos"))]` stub of `copy_bundle_to_clipboard` to `#[cfg(not(any(target_os = "macos", target_os = "linux")))]`. Then add:

```rust
#[cfg(target_os = "linux")]
pub fn copy_string_to_clipboard(content: &str) -> Result<()> {
    wl_copy(&[], content.as_bytes())
}

#[cfg(target_os = "linux")]
pub fn copy_png_to_clipboard(png_bytes: &[u8]) -> Result<()> {
    wl_copy(&["--type", "image/png"], png_bytes)
}

/// `wl-copy` reads stdin, then forks to serve the selection, so it returns at once. Its
/// stdout/stderr must not be our pipes, or the forked server would hold them open.
#[cfg(target_os = "linux")]
fn wl_copy(args: &[&str], bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("wl-copy")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Could not run wl-copy (install wl-clipboard)")?;
    child
        .stdin
        .take()
        .context("No wl-copy stdin")?
        .write_all(bytes)?;
    ensure!(child.wait()?.success(), "wl-copy failed");
    Ok(())
}
```

In the `main.rs` `Autocrop` arm:

```rust
                let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                std::process::Command::new(opener).arg(&page).status()?;
```

- [ ] **Step 2: Build and check by hand.** Ask first: `nice -n 19 ionice -c 3 cargo build -j 2`. Then run `./target/debug/scrawl send /tmp/demo2.scrawl --clipboard && wl-paste | head -5`.
Expected: `Copied … to clipboard`, and `wl-paste` prints the start of `summary.md`.

- [ ] **Step 3: Commit** with message `feat(linux): clipboard handoff with wl-copy; xdg-open for autocrop`, plus the trailer.

---

### Task 6: Transcription with Parakeet

**Files:**
- Create: `src/linux/parakeet.rs`, `src/linux/fixtures/sherpa.json`
- Modify: `Cargo.toml`, `src/linux/mod.rs`, `src/helper.rs`, `src/hex.rs:450` (`ends_sentence` → `pub(crate)`)

**Interfaces:**
- Consumes: `audio::decode_wav_16k(&[u8]) -> Result<Vec<f32>>`, `audio::speech_islands(&[f32]) -> Vec<(u64, u64)>` and `hex::plan_clips(&[(u64, u64)], u64) -> Vec<(u64, u64)>` (all existing), and `hex::ends_sentence(&str) -> bool` (made `pub(crate)` in this task).
- Produces:
  - `parakeet::transcribe(&Path) -> Result<TranscriptData>`, re-exported as `helper::transcribe` on Linux, so `transcribe.rs` stays unchanged.
  - `parakeet::words(&SherpaResult, u64) -> Vec<WordTiming>`
  - `parakeet::segments(&[WordTiming]) -> Vec<TranscriptSegment>`

- [ ] **Step 1: Add the crate.** Ask first and mention that it needs the network: `nice -n 19 cargo add tungstenite --target 'cfg(target_os = "linux")' --no-default-features -F handshake`

- [ ] **Step 2: Fixture.** Write `src/linux/fixtures/sherpa.json`, the real reply from the planning probe (espeak-ng pt-BR, "Olha esse botão aqui, …"):

```json
{"lang": "", "emotion": "", "event": "", "text": "Olha esse botão aqui, ele não salva o formulário quando eu clico.", "timestamps": [0.00, 0.24, 0.48, 0.64, 0.72, 0.80, 0.88, 1.12, 1.28, 1.44, 1.60, 1.76, 1.92, 2.16, 2.40, 2.56, 2.88, 3.20, 3.36, 3.52, 3.84], "durations": [0.08, 0.08, 0.16, 0.08, 0.08, 0.08, 0.24, 0.16, 0.16, 0.16, 0.16, 0.16, 0.08, 0.08, 0.16, 0.16, 0.16, 0.16, 0.16, 0.16, 0.08], "tokens":[" Ol", "ha", " esse", " b", "ot", "ão", " aqui", ",", " ele", " não", " sal", "va", " o", " form", "ul", "ário", " quando", " eu", " cli", "co", "."], "words": []}
```

- [ ] **Step 3: Write the failing tests** (at the bottom of the new `src/linux/parakeet.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> SherpaResult {
        serde_json::from_slice(include_bytes!("fixtures/sherpa.json")).unwrap()
    }

    #[test]
    fn tokens_join_into_timed_words_from_the_clip_offset() {
        let words = words(&fixture(), 1_000);
        let texts: Vec<_> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(
            texts,
            ["Olha", "esse", "botão", "aqui,", "ele", "não", "salva", "o", "formulário", "quando", "eu", "clico."]
        );
        assert_eq!((words[0].t0, words[0].t1), (1_000, 1_320));
        assert_eq!((words[2].t0, words[2].t1), (1_640, 1_880));
        assert_eq!((words[3].t0, words[3].t1), (1_880, 2_280));
        assert_eq!((words[11].t0, words[11].t1), (4_360, 4_920));
    }

    #[test]
    fn a_missing_duration_ends_at_the_next_token() {
        let result = SherpaResult {
            text: String::new(),
            tokens: vec![" a".into(), " b".into()],
            timestamps: vec![0.5, 1.0],
            durations: vec![],
        };
        let words = words(&result, 0);
        assert_eq!((words[0].t0, words[0].t1), (500, 1_000));
        assert_eq!((words[1].t0, words[1].t1), (1_000, 1_080));
    }

    #[test]
    fn segments_break_on_sentence_ends() {
        let mut all = words(&fixture(), 0);
        all.extend(words(&fixture(), 5_000));
        let segments = segments(&all);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].t1, 3_920);
        assert_eq!(segments[1].t0, 5_000);
        assert!(segments[0].text.starts_with("Olha esse botão aqui,"));
    }

    /// Loads the 0.6B model (~1 GB RAM, a few seconds): `cargo test -- --ignored parakeet`.
    #[test]
    #[ignore]
    fn parakeet_transcribes_spoken_portuguese() {
        let dir = std::env::temp_dir().join(format!("scrawl-parakeet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("speech.wav");
        let spoken = std::process::Command::new("espeak-ng")
            .args(["-v", "pt-br", "-w"])
            .arg(&wav)
            .arg("Olha esse botão aqui.")
            .status()
            .expect("espeak-ng is needed for this test");
        assert!(spoken.success());
        let transcript = transcribe(&wav).unwrap();
        assert_eq!(transcript.source, "parakeet");
        assert!(transcript.text.to_lowercase().contains("botão"), "{}", transcript.text);
        assert!(!transcript.estimated && !transcript.words.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
```

`espeak-ng -w` writes a 22 kHz WAV. `decode_wav_16k` must resample it. If it only accepts 16 kHz, convert first in the test with `ffmpeg -ar 16000 -ac 1`, and in `transcribe` do the same through a temp file when `decode_wav_16k` errors. Scrawl's own recordings are already 16 kHz.

Add `pub mod parakeet;` to `src/linux/mod.rs`.

- [ ] **Step 4: Run to see them fail.** Ask first: `nice -n 19 ionice -c 3 cargo test -j 2 linux::parakeet -- --test-threads=2`. Expected: compile errors, because `SherpaResult`, `words` and `segments` are missing.

- [ ] **Step 5: Implement** (above the tests). First make `fn ends_sentence` in `src/hex.rs` `pub(crate)`.

```rust
//! Transcription on Linux: NVIDIA Parakeet TDT 0.6B v3 through sherpa-onnx's offline WebSocket
//! server, the one OpenWhispr ships, with the model OpenWhispr downloaded. Each speech island
//! goes over as raw 16 kHz samples and comes back with a time per token, so word times are
//! measured, not spread like Hex's.

use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tungstenite::Message;

use crate::audio;
use crate::model::{TranscriptData, TranscriptSegment, WordTiming};

const MODEL_NAME: &str = "parakeet-tdt-0.6b-v3";
const SAMPLES_PER_MS: u64 = 16;
/// Speech just outside an island's energy edges, like `hex`'s clips.
const CLIP_PAD_MS: u64 = 200;
/// Shorter than this and too quiet for islands: nothing worth sending.
const MIN_WHOLE_MS: u64 = 480;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const CLIP_TIMEOUT: Duration = Duration::from_secs(120);

/// One offline result. `timestamps` and `durations` are seconds from the clip's start, one per
/// token; a token that begins with a space begins a word.
#[derive(Debug, Deserialize)]
pub struct SherpaResult {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tokens: Vec<String>,
    #[serde(default)]
    pub timestamps: Vec<f64>,
    #[serde(default)]
    pub durations: Vec<f64>,
}

/// Joins tokens into words timed from the clip's start `offset_ms`. A token without a duration
/// ends where the next one starts (or 80 ms after its own start, for the last).
pub fn words(result: &SherpaResult, offset_ms: u64) -> Vec<WordTiming> {
    let ms = |seconds: f64| offset_ms + (seconds.max(0.0) * 1_000.0).round() as u64;
    let mut words: Vec<WordTiming> = Vec::new();
    for (index, token) in result.tokens.iter().enumerate() {
        let Some(&start) = result.timestamps.get(index) else {
            break;
        };
        let end = result
            .durations
            .get(index)
            .map(|duration| start + duration)
            .or_else(|| result.timestamps.get(index + 1).copied())
            .unwrap_or(start + 0.08);
        let piece = token.trim();
        if piece.is_empty() {
            continue;
        }
        match words.last_mut() {
            Some(word) if !token.starts_with(' ') => {
                word.text.push_str(piece);
                word.t1 = ms(end);
            }
            _ => words.push(WordTiming {
                t0: ms(start),
                t1: ms(end),
                text: piece.to_owned(),
            }),
        }
    }
    words
}

/// Words grouped into sentences, as `hex::spread_words` groups its estimates.
pub fn segments(words: &[WordTiming]) -> Vec<TranscriptSegment> {
    let mut segments = Vec::new();
    let mut start = 0;
    for (index, word) in words.iter().enumerate() {
        if crate::hex::ends_sentence(&word.text) || index + 1 == words.len() {
            let phrase = &words[start..=index];
            segments.push(TranscriptSegment {
                t0: phrase[0].t0,
                t1: word.t1,
                text: phrase.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "),
            });
            start = index + 1;
        }
    }
    segments
}

/// `SCRAWL_SHERPA_WS`, else the server in the most recently installed OpenWhispr.
fn server_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SCRAWL_SHERPA_WS") {
        return Some(PathBuf::from(path));
    }
    let installs = dirs::home_dir()?.join(".local/opt/openwhispr");
    std::fs::read_dir(installs)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("resources/bin/sherpa-onnx-ws-linux-x64"))
        .filter(|path| path.is_file())
        .max_by_key(|path| path.metadata().and_then(|m| m.modified()).ok())
}

/// `SCRAWL_PARAKEET_DIR`, else the model OpenWhispr downloaded.
fn model_dir() -> Option<PathBuf> {
    std::env::var_os("SCRAWL_PARAKEET_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::cache_dir().map(|c| c.join("openwhispr/parakeet-models").join(MODEL_NAME)))
        .filter(|dir| dir.join("tokens.txt").is_file())
}

/// A sherpa-onnx server of our own on a free loopback port, stopped when dropped. It is
/// started per recording, so no model stays in memory between captures.
struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start(binary: &Path, model: &Path) -> Result<Self> {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port();
        let file = |flag: &str, name: &str| format!("--{flag}={}", model.join(name).display());
        let mut child = Command::new(binary)
            .env("LD_LIBRARY_PATH", binary.parent().unwrap_or(Path::new(".")))
            .arg(file("tokens", "tokens.txt"))
            .arg(file("encoder", "encoder.int8.onnx"))
            .arg(file("decoder", "decoder.int8.onnx"))
            .arg(file("joiner", "joiner.int8.onnx"))
            .args(["--model-type=nemo_transducer", "--num-threads=4", "--num-work-threads=1"])
            .arg(format!("--port={port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Could not start {}", binary.display()))?;
        let stderr = child.stderr.take().context("No sherpa-onnx stderr")?;
        let server = Self { child, port };
        let (ready, started) = std::sync::mpsc::channel();
        // Keeps draining stderr after the ready line so the pipe never fills.
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.contains("Listening on") {
                    let _ = ready.send(());
                }
            }
        });
        match started.recv_timeout(STARTUP_TIMEOUT) {
            Ok(()) => Ok(server),
            Err(_) => bail!("sherpa-onnx did not start within {} s", STARTUP_TIMEOUT.as_secs()),
        }
    }

    /// Sends one clip: `[i32 LE sample rate][i32 LE byte count][f32 LE samples]`, one JSON reply.
    fn transcribe(&self, samples: &[f32]) -> Result<SherpaResult> {
        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port))?;
        stream.set_read_timeout(Some(CLIP_TIMEOUT))?;
        let (mut socket, _) = tungstenite::client(format!("ws://127.0.0.1:{}/", self.port), stream)
            .map_err(|error| anyhow::anyhow!("sherpa-onnx handshake failed: {error}"))?;
        let mut message = Vec::with_capacity(8 + samples.len() * 4);
        message.extend_from_slice(&16_000_i32.to_le_bytes());
        message.extend_from_slice(&((samples.len() * 4) as i32).to_le_bytes());
        for sample in samples {
            message.extend_from_slice(&sample.to_le_bytes());
        }
        socket.send(Message::binary(message))?;
        let reply = loop {
            match socket.read()? {
                message @ Message::Text(_) => break message.to_text()?.to_owned(),
                Message::Close(_) => bail!("sherpa-onnx closed the connection without a result"),
                _ => {}
            }
        };
        let _ = socket.send(Message::text("Done"));
        let _ = socket.close(None);
        serde_json::from_str(&reply).context("Invalid sherpa-onnx result")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Parakeet over the recording's speech islands, with measured word times.
pub fn transcribe(wav: &Path) -> Result<TranscriptData> {
    let binary = server_binary()
        .context("sherpa-onnx-ws not found: install OpenWhispr or set SCRAWL_SHERPA_WS")?;
    let model = model_dir().with_context(|| {
        format!("{MODEL_NAME} not found: download it in OpenWhispr or set SCRAWL_PARAKEET_DIR")
    })?;
    let samples = audio::decode_wav_16k(&std::fs::read(wav)?)?;
    let duration_ms = samples.len() as u64 / SAMPLES_PER_MS;
    let mut clips = crate::hex::plan_clips(&audio::speech_islands(&samples), duration_ms);
    if clips.is_empty() && duration_ms >= MIN_WHOLE_MS {
        // Too quiet to find speech by energy: let Parakeet hear the whole thing.
        clips.push((0, duration_ms));
    }
    let mut transcript = TranscriptData {
        source: "parakeet".into(),
        model: Some(MODEL_NAME.into()),
        ..Default::default()
    };
    if clips.is_empty() {
        return Ok(transcript);
    }
    let server = Server::start(&binary, &model)?;
    for (start, end) in clips {
        let from_ms = start.saturating_sub(CLIP_PAD_MS);
        let from = (from_ms * SAMPLES_PER_MS) as usize;
        let to = (((end + CLIP_PAD_MS) * SAMPLES_PER_MS) as usize).min(samples.len());
        if from >= to {
            continue;
        }
        let result = server.transcribe(&samples[from..to])?;
        transcript.words.extend(words(&result, from_ms));
    }
    transcript.segments = segments(&transcript.words);
    transcript.text = transcript
        .words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Ok(transcript)
}
```

If the installed `tungstenite` version lacks `Message::binary` / `Message::text`, use the variants (`Message::Binary(message.into())`, `Message::Text("Done".into())`). Check the version in `Cargo.lock`.

Then, in `src/helper.rs`, put `#[cfg(not(target_os = "linux"))]` on the Swift `transcribe` and its private `Transcript` struct, and add:

```rust
#[cfg(target_os = "linux")]
pub use crate::linux::parakeet::transcribe;
```

- [ ] **Step 6: Run the unit tests.** Same command as Step 4. Expected: 3 PASS, 1 ignored.

- [ ] **Step 7: Run the real-model test.** Tell the user it loads ~1 GB and takes a few seconds, then run: `nice -n 19 ionice -c 3 cargo test -j 2 parakeet_transcribes -- --ignored --test-threads=1`. Expected: PASS.

- [ ] **Step 8: Commit** with message `feat(linux): Parakeet transcription through OpenWhispr's sherpa-onnx server`, plus the trailer. Include `Cargo.toml` and `Cargo.lock`.

---

### Task 7: Hyprland target, windows and cursor

**Files:**
- Create: `src/linux/hypr.rs`, `src/linux/fixtures/hypr-monitors.json`, `src/linux/fixtures/hypr-clients.json`
- Modify: `src/linux/mod.rs` (`pub mod hypr;`)

**Interfaces:**
- Consumes: `windows::{ActivityLog, WindowEntry}` with `push_windows` / `push_cursor` (Task 1), and `model::{TargetInfo, TargetFrame, CaptureMode}`.
- Produces:
  - `hypr::discover_target(Option<&str>) -> TargetInfo`
  - `hypr::pick_target(&[Monitor], &[Client], Option<&str>) -> Result<TargetInfo>`
  - `hypr::window_entries(&[Client], &Monitor) -> Vec<WindowEntry>`
  - `hypr::monitor_name(u32) -> Option<String>`
  - `hypr::WindowLogger::{start(TargetFrame, Instant, Arc<AtomicBool>) -> Self, stop(self) -> ActivityLog}`

- [ ] **Step 1: Fixtures.** These are trimmed real outputs from this machine (Hyprland 0.56.2, one 1920×1080 monitor). `src/linux/fixtures/hypr-monitors.json`:

```json
[{"id": 0, "name": "DP-1", "width": 1920, "height": 1080, "x": 0, "y": 0, "scale": 1.0, "transform": 0, "focused": true, "activeWorkspace": {"id": 1, "name": "1"}},
 {"id": 1, "name": "HDMI-A-1", "width": 2560, "height": 1440, "x": 1920, "y": 0, "scale": 1.25, "transform": 0, "focused": false, "activeWorkspace": {"id": 5, "name": "5"}}]
```

`src/linux/fixtures/hypr-clients.json`:

```json
[{"address": "0x55a9e0d4b410", "at": [10, 52], "size": [1900, 1018], "class": "com.t3tools.T3Code", "title": "T3 Code", "workspace": {"id": 1, "name": "1"}, "monitor": 0, "mapped": true, "hidden": false, "floating": false, "focusHistoryID": 0, "pid": 1},
 {"address": "0x55a9e0aa0001", "at": [600, 300], "size": [700, 400], "class": "kitty", "title": "~/scrawl", "workspace": {"id": 1, "name": "1"}, "monitor": 0, "mapped": true, "hidden": false, "floating": true, "focusHistoryID": 3, "pid": 2},
 {"address": "0x55a9e0aa0002", "at": [0, 0], "size": [800, 600], "class": "steam", "title": "Steam", "workspace": {"id": 4, "name": "4"}, "monitor": 0, "mapped": true, "hidden": false, "floating": false, "focusHistoryID": 1, "pid": 3},
 {"address": "0x55a9e0aa0003", "at": [1930, 40], "size": [1000, 800], "class": "Helium", "title": "localhost:5173", "workspace": {"id": 5, "name": "5"}, "monitor": 1, "mapped": true, "hidden": false, "floating": false, "focusHistoryID": 2, "pid": 4}]
```

- [ ] **Step 2: Write the failing tests** (at the bottom of the new `src/linux/hypr.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn monitors() -> Vec<Monitor> {
        serde_json::from_slice(include_bytes!("fixtures/hypr-monitors.json")).unwrap()
    }

    fn clients() -> Vec<Client> {
        serde_json::from_slice(include_bytes!("fixtures/hypr-clients.json")).unwrap()
    }

    #[test]
    fn default_target_is_the_focused_monitor_in_points() {
        let target = pick_target(&monitors(), &clients(), None).unwrap();
        assert_eq!(target.mode, CaptureMode::Display);
        assert_eq!(target.display_id, 0);
        assert_eq!((target.frame.width, target.frame.height, target.frame.scale), (1920.0, 1080.0, 1.0));
    }

    #[test]
    fn scaled_monitor_frame_is_logical() {
        let frame = monitor_frame(&monitors()[1]);
        assert_eq!((frame.x, frame.width, frame.height), (1920.0, 2048.0, 1152.0));
    }

    #[test]
    fn window_query_matches_title_or_class() {
        let target = pick_target(&monitors(), &clients(), Some("5173")).unwrap();
        assert_eq!(target.mode, CaptureMode::Window);
        assert_eq!(target.app.as_deref(), Some("Helium"));
        assert_eq!(target.display_id, 1);
        assert_eq!((target.frame.x, target.frame.y, target.frame.width), (1930.0, 40.0, 1000.0));
        assert!(pick_target(&monitors(), &clients(), Some("nothing-like-this")).is_err());
    }

    #[test]
    fn entries_are_the_active_workspace_front_to_back() {
        let entries = window_entries(&clients(), &monitors()[0]);
        let apps: Vec<_> = entries.iter().map(|e| e.app.as_str()).collect();
        // Floating kitty draws above the tiled T3 Code; Steam is on another workspace.
        assert_eq!(apps, ["kitty", "com.t3tools.T3Code"]);
        assert!(entries[1].frontmost && !entries[0].frontmost);
        assert_eq!(entries[1].bounds, [10, 52, 1900, 1018]);
    }

    #[test]
    fn bounds_are_relative_to_the_monitor() {
        let entries = window_entries(&clients(), &monitors()[1]);
        assert_eq!(entries[0].bounds, [10, 40, 1000, 800]);
    }
}
```

Add `pub mod hypr;` to `src/linux/mod.rs`.

- [ ] **Step 3: Run to see them fail.** Ask first: `nice -n 19 ionice -c 3 cargo test -j 2 linux::hypr -- --test-threads=2`. Expected: compile errors.

- [ ] **Step 4: Implement** (above the tests)

```rust
//! Hyprland through its IPC socket: what to record, which windows are on screen and where the
//! cursor is. Replies are the same JSON `hyprctl -j` prints, read without a process per call.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::capture::scrawl_log;
use crate::model::{CaptureMode, TargetFrame, TargetInfo};
use crate::windows::{ActivityLog, WindowEntry};

/// How often the cursor is read.
const TICK: Duration = Duration::from_millis(100);
/// How often the window list is read when the focused window hasn't changed.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Deserialize)]
pub struct WorkspaceRef {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    pub id: u32,
    pub name: String,
    /// Physical pixels.
    pub width: u32,
    pub height: u32,
    /// Logical (layout) coordinates.
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    #[serde(default)]
    pub transform: i32,
    pub focused: bool,
    pub active_workspace: WorkspaceRef,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub address: String,
    /// Logical layout coordinates.
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub class: String,
    pub title: String,
    pub workspace: WorkspaceRef,
    pub monitor: i64,
    pub mapped: bool,
    pub hidden: bool,
    pub floating: bool,
    /// 0 is the window focused last.
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i32,
}

#[derive(Deserialize)]
struct CursorPos {
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
struct ActiveWindow {
    address: String,
}

/// One request on `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`.
fn hypr_json<T: DeserializeOwned>(command: &str) -> Result<T> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let instance =
        std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").context("Not running under Hyprland")?;
    let path = PathBuf::from(runtime).join("hypr").join(instance).join(".socket.sock");
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("Could not reach Hyprland at {}", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(format!("j/{command}").as_bytes())?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    serde_json::from_slice(&reply).with_context(|| format!("Invalid Hyprland reply to {command}"))
}

/// A monitor's area in logical points (rotated monitors swap sides).
pub fn monitor_frame(monitor: &Monitor) -> TargetFrame {
    let (width, height) = if monitor.transform % 2 == 1 {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    };
    TargetFrame {
        x: f64::from(monitor.x),
        y: f64::from(monitor.y),
        width: f64::from(width) / monitor.scale,
        height: f64::from(height) / monitor.scale,
        scale: monitor.scale,
    }
}

fn window_id(address: &str) -> u32 {
    u64::from_str_radix(address.trim_start_matches("0x"), 16).map_or(0, |a| a as u32)
}

/// The focused monitor, or the newest window whose title or class contains `window_query`.
pub fn pick_target(
    monitors: &[Monitor],
    clients: &[Client],
    window_query: Option<&str>,
) -> Result<TargetInfo> {
    let focused = monitors
        .iter()
        .find(|m| m.focused)
        .or(monitors.first())
        .context("Hyprland reports no monitors")?;
    let Some(query) = window_query else {
        return Ok(TargetInfo {
            mode: CaptureMode::Display,
            app: None,
            bundle_id: None,
            window_title: None,
            window_id: None,
            display_id: focused.id,
            frame: monitor_frame(focused),
        });
    };
    let needle = query.to_lowercase();
    let client = clients
        .iter()
        .filter(|c| c.mapped && !c.hidden)
        .filter(|c| c.title.to_lowercase().contains(&needle) || c.class.to_lowercase().contains(&needle))
        .min_by_key(|c| c.focus_history_id)
        .with_context(|| format!("No window matches {query:?}"))?;
    let monitor = monitors
        .iter()
        .find(|m| i64::from(m.id) == client.monitor)
        .unwrap_or(focused);
    Ok(TargetInfo {
        mode: CaptureMode::Window,
        app: Some(client.class.clone()),
        bundle_id: None,
        window_title: Some(client.title.clone()),
        window_id: Some(window_id(&client.address)),
        display_id: monitor.id,
        frame: TargetFrame {
            x: f64::from(client.at[0]),
            y: f64::from(client.at[1]),
            width: f64::from(client.size[0]),
            height: f64::from(client.size[1]),
            scale: monitor.scale,
        },
    })
}

/// What to record now; the first monitor's guess when Hyprland can't be asked.
pub fn discover_target(window_query: Option<&str>) -> TargetInfo {
    let asked = hypr_json::<Vec<Monitor>>("monitors").and_then(|monitors| {
        let clients = hypr_json::<Vec<Client>>("clients")?;
        pick_target(&monitors, &clients, window_query)
    });
    asked.unwrap_or_else(|error| {
        scrawl_log(format!("Hyprland: {error:#}; recording the default output"));
        TargetInfo {
            mode: CaptureMode::Display,
            app: None,
            bundle_id: None,
            window_title: None,
            window_id: None,
            display_id: 0,
            frame: TargetFrame { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0, scale: 1.0 },
        }
    })
}

/// The output name `wf-recorder -o` wants for a monitor id.
pub fn monitor_name(id: u32) -> Option<String> {
    hypr_json::<Vec<Monitor>>("monitors")
        .ok()?
        .into_iter()
        .find(|m| m.id == id)
        .map(|m| m.name)
}

/// The windows on `monitor`'s active workspace, front to back (floating above tiled, then by
/// focus), with bounds in points from the monitor's top-left.
pub fn window_entries(clients: &[Client], monitor: &Monitor) -> Vec<WindowEntry> {
    let mut visible: Vec<&Client> = clients
        .iter()
        .filter(|c| c.mapped && !c.hidden && c.workspace.id == monitor.active_workspace.id)
        .collect();
    visible.sort_by_key(|c| (!c.floating, c.focus_history_id));
    visible
        .into_iter()
        .map(|c| WindowEntry {
            id: window_id(&c.address),
            app: c.class.clone(),
            bundle_id: None,
            regular: true,
            title: c.title.clone(),
            bounds: [c.at[0] - monitor.x, c.at[1] - monitor.y, c.size[0], c.size[1]],
            layer: 0,
            frontmost: c.focus_history_id == 0,
        })
        .collect()
}

/// Logs the cursor every tick and the window list every second or when focus moves, like the
/// macOS `windows::WindowLogger`.
pub struct WindowLogger {
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<ActivityLog>>,
}

impl WindowLogger {
    pub fn start(frame: TargetFrame, started_at: Instant, request: Arc<AtomicBool>) -> Self {
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let worker = thread::Builder::new()
            .name("scrawl-window-log".into())
            .spawn(move || run(frame, started_at, &request, &stop))
            .ok();
        Self { stopping, worker }
    }

    pub fn stop(mut self) -> ActivityLog {
        self.stopping.store(true, Ordering::Release);
        let log = self
            .worker
            .take()
            .and_then(|w| {
                w.thread().unpark();
                w.join().ok()
            })
            .unwrap_or_default();
        scrawl_log(format!(
            "Window log: {} snapshots taken, {} stored; {} cursor samples",
            log.snapshot_calls,
            log.windows.len(),
            log.cursor.len()
        ));
        log
    }
}

fn run(
    frame: TargetFrame,
    started_at: Instant,
    request: &AtomicBool,
    stopping: &AtomicBool,
) -> ActivityLog {
    let mut log = ActivityLog::default();
    let mut last_snapshot: Option<Instant> = None;
    let mut last_front: Option<String> = None;
    loop {
        let finishing = stopping.load(Ordering::Acquire);
        let t = started_at.elapsed().as_millis() as u64;
        if let Ok(cursor) = hypr_json::<CursorPos>("cursorpos") {
            log.push_cursor(t, cursor.x - frame.x, cursor.y - frame.y, frame);
        }
        let front = hypr_json::<ActiveWindow>("activewindow").ok().map(|w| w.address);
        let due = !finishing
            && (last_snapshot.is_none_or(|s| s.elapsed() >= SNAPSHOT_INTERVAL)
                || front != last_front
                || request.swap(false, Ordering::AcqRel));
        if due {
            let began = Instant::now();
            if let (Ok(monitors), Ok(clients)) = (
                hypr_json::<Vec<Monitor>>("monitors"),
                hypr_json::<Vec<Client>>("clients"),
            ) && let Some(monitor) = monitors
                .iter()
                .find(|m| f64::from(m.x) == frame.x && f64::from(m.y) == frame.y)
            {
                log.push_windows(t, window_entries(&clients, monitor));
            }
            log.snapshot_calls += 1;
            log.snapshot_time += began.elapsed();
            last_snapshot = Some(Instant::now());
            last_front = front;
        }
        if finishing {
            return log;
        }
        thread::park_timeout(TICK);
    }
}
```

`frame.x == m.x` is exact on purpose: a display target's frame comes from `monitor_frame`, which copies the integer origin.

- [ ] **Step 5: Run the tests.** Same command as Step 3. Expected: 5 PASS.

- [ ] **Step 6: Commit** with message `feat(linux): Hyprland target, window list and cursor log`, plus the trailer.

---

### Task 8: Screen recording with wf-recorder

**Files:**
- Create: `src/linux/record.rs`
- Modify: `src/linux/mod.rs` (`pub mod record;`)

**Interfaces:**
- Consumes: `hypr::monitor_name(u32) -> Option<String>` (Task 7); `helper::{probe, frames, FrameTimes}` (Task 3); `capture::{ScreenRecording, FRAME_SAMPLE_INTERVAL, CANDIDATE_EDGE, CANDIDATE_BUDGET_BYTES}` (Task 1).
- Produces:
  - `record::ScreenSession::start(&TargetInfo, &Path, Instant) -> Result<Self>`
  - `ScreenSession::stop(self) -> Result<ScreenRecording>`
  - `record::sample_interval_ms(u64, usize) -> u64`
  - `record::geometry(TargetFrame) -> String`

- [ ] **Step 1: Check the encoder by hand.** Tell the user this records their screen for 3 s into `/tmp`, then run:
`timeout -s INT 3 wf-recorder -y -c h264_nvenc -f /tmp/scrawl-nvenc.mp4; ffprobe -v error -show_entries format=duration -of csv=p=0 /tmp/scrawl-nvenc.mp4; rm -f /tmp/scrawl-nvenc.mp4`
Expected: a duration near 3. If `wf-recorder` errors about the pixel format, retry with `-x yuv420p` and add `-x yuv420p` to the NVENC arguments in Step 4. If NVENC fails outright, the session falls back to `libx264`, so nothing else changes.

- [ ] **Step 2: Write the failing tests** (at the bottom of the new `src/linux/record.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_captures_sample_at_the_normal_interval() {
        assert_eq!(sample_interval_ms(10_000, 480 * 270 * 4), 220);
    }

    #[test]
    fn long_captures_sample_less_often_to_stay_in_budget() {
        let frame = 480 * 270 * 4;
        let interval = sample_interval_ms(30 * 60_000, frame);
        assert!(interval > 220);
        assert!((30 * 60_000 / interval + 1) as usize * frame <= CANDIDATE_BUDGET_BYTES);
    }

    #[test]
    fn geometry_is_wf_recorders_format() {
        let frame = TargetFrame { x: 1930.0, y: 40.4, width: 999.6, height: 800.0, scale: 1.25 };
        assert_eq!(geometry(frame), "1930,40 1000x800");
    }
}
```

Add `pub mod record;` to `src/linux/mod.rs`.

- [ ] **Step 3: Run to see them fail.** Ask first: `nice -n 19 ionice -c 3 cargo test -j 2 linux::record -- --test-threads=2`. Expected: compile errors.

- [ ] **Step 4: Implement** (above the tests)

```rust
//! Screen recording on Linux: `wf-recorder` (wlr-screencopy) writes `recording.mp4`, and the
//! small candidate frames for keyframe selection are decoded from it afterwards, so nothing
//! samples the screen while recording.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::capture::{CANDIDATE_BUDGET_BYTES, CANDIDATE_EDGE, FRAME_SAMPLE_INTERVAL, ScreenRecording};
use crate::helper::FrameTimes;
use crate::model::{CaptureMode, TargetFrame, TargetInfo};

/// How long the first frame may take before a silent screen is assumed (wf-recorder only
/// writes on damage).
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(3);
const STOP_WAIT: Duration = Duration::from_secs(15);

pub struct ScreenSession {
    child: Child,
    video: PathBuf,
    /// Capture-clock time of the video's first frame.
    video_offset_ms: u64,
}

/// `"x,y WxH"` in logical pixels, rounded.
pub fn geometry(frame: TargetFrame) -> String {
    format!(
        "{},{} {}x{}",
        frame.x.round() as i64,
        frame.y.round() as i64,
        frame.width.round() as i64,
        frame.height.round() as i64
    )
}

/// The candidate sampling step: `FRAME_SAMPLE_INTERVAL`, doubled until every frame of the
/// capture fits `CANDIDATE_BUDGET_BYTES` (the macOS `CandidateBuffer` thins the same way).
pub fn sample_interval_ms(duration_ms: u64, frame_bytes: usize) -> u64 {
    let mut interval = FRAME_SAMPLE_INTERVAL.as_millis() as u64;
    while (duration_ms / interval + 1) as usize * frame_bytes > CANDIDATE_BUDGET_BYTES {
        interval *= 2;
    }
    interval
}

fn spawn(target: &TargetInfo, video: &Path, codec: &str) -> Result<Child> {
    let mut command = Command::new("wf-recorder");
    command.args(["-y", "-c", codec]).arg("-f").arg(video);
    match target.mode {
        CaptureMode::Window | CaptureMode::Region => {
            command.args(["-g", &geometry(target.frame)]);
        }
        CaptureMode::Display | CaptureMode::Device => {
            if let Some(name) = crate::linux::hypr::monitor_name(target.display_id) {
                command.args(["-o", &name]);
            }
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Could not start wf-recorder (install wf-recorder)")
}

/// Waits for the first bytes; `Err` with wf-recorder's complaint if it quit instead.
fn wait_for_first_frame(child: &mut Child, video: &Path) -> Result<()> {
    let deadline = Instant::now() + FIRST_FRAME_WAIT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            let mut message = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut message);
            }
            bail!("wf-recorder quit ({status}): {}", message.trim());
        }
        if video.metadata().is_ok_and(|m| m.len() > 0) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

impl ScreenSession {
    /// Starts recording `target` into `video`, on NVENC when the GPU takes it, else x264.
    pub fn start(target: &TargetInfo, video: &Path, started_at: Instant) -> Result<Self> {
        let mut child = spawn(target, video, "h264_nvenc")?;
        if let Err(error) = wait_for_first_frame(&mut child, video) {
            crate::capture::scrawl_log(format!("NVENC unavailable, using x264: {error:#}"));
            child = spawn(target, video, "libx264")?;
            wait_for_first_frame(&mut child, video)?;
        }
        // Drain stderr for the rest of the recording: closing the pipe would kill wf-recorder
        // with SIGPIPE on its next message, and leaving it unread could fill it.
        if let Some(mut stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            });
        }
        Ok(Self {
            child,
            video: video.to_owned(),
            video_offset_ms: started_at.elapsed().as_millis() as u64,
        })
    }

    /// Stops wf-recorder (SIGINT lets it finish the file) and decodes the candidate frames.
    pub fn stop(mut self) -> Result<ScreenRecording> {
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGINT);
        }
        let deadline = Instant::now() + STOP_WAIT;
        while self.child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                bail!("wf-recorder did not stop; the recording may be cut");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let probe = crate::helper::probe(&self.video)?;
        let (width, height) =
            crate::linux::media::scaled_size(probe.width, probe.height, CANDIDATE_EDGE as u32);
        let interval = sample_interval_ms(probe.duration_ms, width as usize * height as usize * 4);
        let frames = crate::helper::frames(
            &self.video,
            FrameTimes::Every(interval),
            CANDIDATE_EDGE as u32,
        )?
        .into_iter()
        .map(|mut frame| {
            frame.t_ms += self.video_offset_ms;
            frame
        })
        .collect();
        Ok(ScreenRecording {
            frames,
            video: Some(self.video),
            duration_ms: self.video_offset_ms + probe.duration_ms,
            video_offset_ms: self.video_offset_ms,
        })
    }
}
```

- [ ] **Step 5: Run the tests.** Same command as Step 3. Expected: 3 PASS.

- [ ] **Step 6: Commit** with message `feat(linux): wf-recorder screen session with candidates decoded afterwards`, plus the trailer.

---

### Task 9: `record --until-stopped`, `scrawl toggle`, Linux record flow

**Files:**
- Create: `src/linux/toggle.rs`
- Modify: `src/linux/mod.rs`, `src/main.rs` (`Command::Record` gets `until_stopped`, new `Command::Toggle`, Linux `Record` arm), `README.md`

**Interfaces:**
- Consumes:
  - `hypr::{discover_target, WindowLogger}` (Task 7)
  - `record::ScreenSession` (Task 8)
  - `audio::AudioRecorder::{start() -> Result<Self>, stop(self) -> Result<RecordedAudio>}` (existing)
  - `crate::finalize_capture_bundle(...)` (existing, `main.rs:785`)
  - `handoff::{default_captures_dir, copy_bundle_to_clipboard}`
  - `crate::make_bundle_id()`
- Produces:
  - `linux::record(Option<&str>, u64, bool, Option<String>, Option<PathBuf>) -> Result<()>`
  - `toggle::toggle() -> Result<()>`
  - `toggle::decide(Option<&str>, impl Fn(i32) -> bool) -> Toggle`

- [ ] **Step 1: Write the failing tests** (at the bottom of the new `src/linux/toggle.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_recorder_is_stopped() {
        assert_eq!(decide(Some("4242\n"), |pid| pid == 4242), Toggle::Stop(4242));
    }

    #[test]
    fn a_dead_or_foreign_pid_starts_a_new_recording() {
        assert_eq!(decide(Some("4242"), |_| false), Toggle::Start);
    }

    #[test]
    fn no_or_garbled_pidfile_starts_a_new_recording() {
        assert_eq!(decide(None, |_| true), Toggle::Start);
        assert_eq!(decide(Some("abc"), |_| true), Toggle::Start);
        assert_eq!(decide(Some("0"), |_| true), Toggle::Start);
    }
}
```

Add `pub mod toggle;` to `src/linux/mod.rs`.

- [ ] **Step 2: Run to see them fail.** Ask first: `nice -n 19 ionice -c 3 cargo test -j 2 linux::toggle -- --test-threads=2`. Expected: compile errors.

- [ ] **Step 3: Implement `src/linux/toggle.rs`** (above the tests)

```rust
//! `scrawl toggle`: one command for a Hyprland key bind that starts a recording, or stops the
//! one running. The running recorder's pid lives in `$XDG_RUNTIME_DIR/scrawl/record.pid`.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

#[derive(Debug, PartialEq, Eq)]
pub enum Toggle {
    Stop(i32),
    Start,
}

pub fn pidfile() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("scrawl/record.pid")
}

/// Stop the pid in the file if `is_recorder` says it is still our recorder; else start one.
pub fn decide(contents: Option<&str>, is_recorder: impl Fn(i32) -> bool) -> Toggle {
    match contents.and_then(|c| c.trim().parse::<i32>().ok()) {
        Some(pid) if pid > 0 && is_recorder(pid) => Toggle::Stop(pid),
        _ => Toggle::Start,
    }
}

/// A recycled pid must never get our SIGINT: check it is `scrawl record --until-stopped`.
fn is_recorder(pid: i32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
        let args: Vec<&[u8]> = cmdline.split(|&b| b == 0).collect();
        args.contains(&b"record".as_slice()) && args.contains(&b"--until-stopped".as_slice())
    })
}

pub fn toggle() -> Result<()> {
    let path = pidfile();
    match decide(std::fs::read_to_string(&path).ok().as_deref(), is_recorder) {
        Toggle::Stop(pid) => {
            unsafe {
                libc::kill(pid, libc::SIGINT);
            }
            // The recorder clears the file itself; removing it now lets a quick third press
            // start a new recording while this one is still being compiled.
            let _ = std::fs::remove_file(&path);
        }
        Toggle::Start => {
            std::fs::create_dir_all(path.parent().context("pidfile has no parent")?)?;
            let child = Command::new(std::env::current_exe()?)
                .args(["record", "--until-stopped"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .context("Could not start scrawl record")?;
            std::fs::write(&path, child.id().to_string())?;
        }
    }
    Ok(())
}

/// Called by the recorder once it stops listening for SIGINT: forget our pid.
pub fn clear_pidfile() {
    let path = pidfile();
    let ours = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| c.trim().parse::<u32>().ok())
        == Some(std::process::id());
    if ours {
        let _ = std::fs::remove_file(path);
    }
}
```

- [ ] **Step 4: Implement `linux::record` in `src/linux/mod.rs`**

```rust
//! Linux backends for what macOS does with ScreenCaptureKit, AppKit and `scrawl-helper`, and
//! the headless recording flow a Hyprland key bind drives through `scrawl toggle`.

pub mod hypr;
pub mod media;
pub mod parakeet;
pub mod record;
pub mod toggle;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::audio::AudioRecorder;
use crate::capture::scrawl_log;
use crate::handoff;
use crate::model::CaptureMode;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

/// SIGINT and SIGTERM end the recording instead of the process.
fn install_stop_signals() {
    let handler = on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

/// Until a stop signal, or `duration` seconds unless `until_stopped`.
fn wait_for_stop(duration: u64, until_stopped: bool) {
    let deadline = (!until_stopped).then(|| Instant::now() + Duration::from_secs(duration.max(1)));
    while !STOP.load(Ordering::Acquire) && deadline.is_none_or(|d| Instant::now() < d) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn notify(text: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "Scrawl", "Scrawl", text])
        .spawn();
}

/// Records the focused monitor (or `window`) with the microphone into a `.scrawl` bundle, then
/// puts its summary on the clipboard. Delivery to OpenCode stays manual (`scrawl send`).
pub fn record(
    window: Option<&str>,
    duration: u64,
    until_stopped: bool,
    note: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    install_stop_signals();
    let result = record_bundle(window, duration, until_stopped, note, out);
    toggle::clear_pidfile();
    match &result {
        Ok(dir) => {
            let copied = handoff::copy_bundle_to_clipboard(dir);
            if let Err(error) = &copied {
                scrawl_log(format!("Clipboard: {error:#}"));
            }
            notify(&if copied.is_ok() {
                format!("Capture ready, summary copied\n{}", dir.display())
            } else {
                format!("Capture ready\n{}", dir.display())
            });
        }
        Err(error) => notify(&format!("Recording failed: {error:#}")),
    }
    result.map(|_| ())
}

fn record_bundle(
    window: Option<&str>,
    duration: u64,
    until_stopped: bool,
    note: Option<String>,
    out: Option<PathBuf>,
) -> Result<PathBuf> {
    let target = hypr::discover_target(window);
    let bundle_dir = out.unwrap_or_else(|| {
        handoff::default_captures_dir().join(format!("{}.scrawl", crate::make_bundle_id()))
    });
    std::fs::create_dir_all(&bundle_dir)?;
    let started_at = Instant::now();
    let audio = AudioRecorder::start().ok();
    let session = match record::ScreenSession::start(&target, &bundle_dir.join("recording.mp4"), started_at) {
        Ok(session) => session,
        Err(error) => {
            // No empty bundle for `latest` to find.
            let _ = std::fs::remove_dir_all(&bundle_dir);
            return Err(error);
        }
    };
    let window_log = (target.mode == CaptureMode::Display)
        .then(|| hypr::WindowLogger::start(target.frame, started_at, Arc::default()));
    notify("Recording. Press the shortcut again to stop.");
    wait_for_stop(duration, until_stopped);
    toggle::clear_pidfile();
    let recording = session.stop()?;
    let activity = window_log.map(hypr::WindowLogger::stop).unwrap_or_default();
    let recorded_audio = audio.and_then(|a| a.stop().ok());
    let (dir, _bundle, summary) = crate::finalize_capture_bundle(
        &bundle_dir,
        target,
        recording,
        Vec::new(),
        Vec::new(),
        recorded_audio,
        note,
        activity,
    )?;
    print!("{summary}");
    Ok(dir)
}
```

If `AudioRecorder::start` or `stop` have different signatures, match them; `main.rs:1488` and `main.rs:1506` show the current calls.

- [ ] **Step 5: Wire up `main.rs`**

Add to `Command::Record`:

```rust
        /// Record until SIGINT/SIGTERM (what `scrawl toggle` starts) instead of `--duration`.
        #[arg(long, conflicts_with = "iphone")]
        until_stopped: bool,
```

Add to both macOS `Record` arms a field `until_stopped`, and as their first line: `anyhow::ensure!(!until_stopped, "--until-stopped is Linux-only for now");`.

Replace the temporary Linux arm from Task 1 with:

```rust
        #[cfg(target_os = "linux")]
        Command::Record { iphone: true, .. } => anyhow::bail!("iPhone capture needs macOS"),
        #[cfg(target_os = "linux")]
        Command::Record {
            window,
            iphone: false,
            duration,
            until_stopped,
            note,
            out,
        } => linux::record(window.as_deref(), duration, until_stopped, note, out),
        #[cfg(target_os = "linux")]
        Command::Toggle => linux::toggle::toggle(),
```

Add the variant:

```rust
    /// Start a recording, or stop the running one (bind it to a key in Hyprland).
    #[cfg(target_os = "linux")]
    Toggle,
```

- [ ] **Step 6: Run the tests and the build.** Ask first: `nice -n 19 ionice -c 3 cargo test -j 2 linux:: -- --test-threads=2 && nice -n 19 ionice -c 3 cargo build -j 2`. Expected: every `linux::` test passes and the build succeeds.

- [ ] **Step 7: README.** Add a `## Linux (Hyprland)` section after `## Install`:

````markdown
## Linux (Hyprland)

A Linux build records from a Hyprland key bind. It needs `wf-recorder`, `ffmpeg`, `wl-clipboard`,
`libnotify` and fontconfig, plus [OpenWhispr](https://openwhispr.com) with its Parakeet
(`parakeet-tdt-0.6b-v3`) model downloaded: Scrawl runs OpenWhispr's `sherpa-onnx-ws` server for
transcription (`SCRAWL_SHERPA_WS` and `SCRAWL_PARAKEET_DIR` point elsewhere).

```sh
cargo build --release
ln -s "$PWD/target/release/scrawl" ~/.local/bin/scrawl
```

In `hyprland.conf`:

```
bind = ALT SHIFT, S, exec, scrawl toggle
```

Press it to start and again to stop. The bundle lands in `~/.local/share/scrawl/captures/` and its
summary on the clipboard; `scrawl send latest --session ses_…` delivers it to OpenCode. Not yet on
Linux: ink, clicks and keys in the timeline, the pill and menu-bar app, iPhone capture, video export.
````

- [ ] **Step 8: Commit** with message `feat(linux): scrawl toggle and record --until-stopped`, plus the trailer.

---

### Task 10: End-to-end check

**Files:** none, unless a bug turns up. A bug fix becomes its own commit with a test.

- [ ] **Step 1: Timed recording.** Tell the user it records the screen and microphone for 8 s and that they can talk during it. Run: `./target/debug/scrawl record --duration 8`
Expected: the summary prints. The bundle in `~/.local/share/scrawl/captures/<id>.scrawl` holds `recording.mp4`, `audio.wav`, `contact-sheet.png`, `frames/`, `scrawl.json` and `summary.md`.

- [ ] **Step 2: Inspect the bundle** (AGENTS.md principle 5)

Read `summary.md`, then `scrawl.json` (`transcript.source` = `parakeet`, `words` measured, `windows` and `cursor` present). Use the Read tool on `contact-sheet.png` and two `frames/*.png`. Check:
- the frames show the screen;
- the badges are legible;
- the keyframes are not all static duplicates;
- the transcript matches what was said.

Report anything noisy to the user, per AGENTS.md.

- [ ] **Step 3: Toggle flow.** Ask the user to add the bind (or run it by hand). Run `./target/debug/scrawl toggle`, wait 5 s, then run `./target/debug/scrawl toggle` again.
Expected:
- A "Recording…" notification appears, then "Capture ready, summary copied".
- `wl-paste | head -3` shows the summary.
- `$XDG_RUNTIME_DIR/scrawl/record.pid` is gone.

- [ ] **Step 4: Failure paths**
- `SCRAWL_PARAKEET_DIR=/nonexistent ./target/debug/scrawl record --duration 3`: the bundle is still built, `transcript.source` is `offline`, and the log names the missing model.
- No `wf-recorder`: run `mkdir -p /tmp/scrawl-nowf && for b in ffmpeg ffprobe notify-send wl-copy fc-match; do ln -sf "$(command -v $b)" /tmp/scrawl-nowf/; done && PATH=/tmp/scrawl-nowf ./target/debug/scrawl record --duration 3`. Expected: exit with `Could not start wf-recorder (install wf-recorder)`, a "Recording failed" notification, and no new directory in `~/.local/share/scrawl/captures/`.

- [ ] **Step 5: Full test run.** Ask first: `nice -n 19 ionice -c 3 cargo test -j 2 -- --test-threads=2`. Expected: all pass.

- [ ] **Step 6: Report.** Summarize for the user:
- what works;
- what the bundle looked like;
- macOS edits that could not be compiled here (list the files);
- which follow-up specs are open: input events (phase 3), ink overlay/pill (phase 4), Linux routing.
