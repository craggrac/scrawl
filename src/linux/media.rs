//! The movie work `scrawl-helper` does with AVFoundation on macOS, done with `ffprobe` and
//! `ffmpeg` on Linux. Frames come back as raw RGBA over stdout, so no temporary files.

use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
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
        audio_tracks: out
            .streams
            .iter()
            .filter(|s| s.codec_type == "audio")
            .count() as u32,
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

/// Adds `movie` as the input, locked down: only the local `file` protocol and movie containers
/// are allowed, so a crafted playlist can't make ffmpeg fetch URLs or read other files, and the
/// `file:` prefix keeps a path starting with `-` or `proto:` from being misread.
fn input(command: &mut Command, movie: &Path) {
    let mut url = OsString::from("file:");
    url.push(movie);
    command
        .args(["-protocol_whitelist", "file"])
        .args(["-format_whitelist", "mov,mp4,m4a,3gp,3g2,mj2,matroska,webm"])
        .arg("-i")
        .arg(url);
}

pub fn probe(movie: &Path) -> Result<Probe> {
    let mut command = Command::new("ffprobe");
    command
        .args(["-v", "error", "-print_format", "json", "-show_entries"])
        .arg("format=duration:stream=codec_type,width,height,avg_frame_rate");
    input(&mut command, movie);
    let out = run(command, "ffprobe", Duration::from_secs(30))?;
    parse_probe(&out.stdout)
}

/// Runs `command` to completion, failing with its stderr when it exits non-zero and killing it
/// when it outlives `deadline`. Both pipes are drained on threads so a chatty child can't block.
fn run(mut command: Command, name: &str, deadline: Duration) -> Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Could not run {name} (install ffmpeg)"))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let Some(status) = status else {
        bail!("{name} timed out after {} s", deadline.as_secs());
    };
    ensure!(
        status.success(),
        "{name} failed: {}",
        String::from_utf8_lossy(&stderr).trim()
    );
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// The decoded size for `max_edge` (0 keeps native size). Native sizes are returned as they are;
/// a scaled size is even on both sides as encoders want.
pub fn scaled_size(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let long = width.max(height).max(1);
    if max_edge == 0 || long <= max_edge {
        return (width, height);
    }
    let scale =
        |side: u32| ((u64::from(side) * u64::from(max_edge) / u64::from(long)) as u32).max(2) & !1;
    (scale(width), scale(height))
}

/// Runs ffmpeg with `filter` and splits its raw RGBA stdout into `width`×`height` frames.
/// A watchdog kills ffmpeg after `DECODE_DEADLINE`, which ends the stdout read.
fn decode(
    movie: &Path,
    seek_ms: Option<u64>,
    filter: &str,
    (width, height): (u32, u32),
    limit: Option<usize>,
) -> Result<Vec<Vec<u8>>> {
    const DECODE_DEADLINE: Duration = Duration::from_secs(300);
    let mut command = Command::new("ffmpeg");
    command.args(["-v", "error", "-nostdin"]);
    if let Some(ms) = seek_ms {
        command.args(["-ss", &format!("{:.3}", ms as f64 / 1_000.0)]);
    }
    input(&mut command, movie);
    command.args(["-an", "-vf", filter]);
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
    let mut stderr = child.stderr.take().context("No ffmpeg stderr")?;
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let child = Arc::new(Mutex::new(child));
    let timed_out = Arc::new(AtomicBool::new(false));
    let (done, wait) = mpsc::channel::<()>();
    let watchdog = {
        let (child, timed_out) = (Arc::clone(&child), Arc::clone(&timed_out));
        std::thread::spawn(move || {
            if wait.recv_timeout(DECODE_DEADLINE) == Err(mpsc::RecvTimeoutError::Timeout) {
                timed_out.store(true, Ordering::SeqCst);
                let _ = child.lock().map(|mut child| child.kill());
            }
        })
    };
    let size = width as usize * height as usize * 4;
    let mut frames = Vec::new();
    let read = loop {
        let mut frame = vec![0; size];
        match stdout.read_exact(&mut frame) {
            Ok(()) => frames.push(frame),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    drop(done);
    let _ = watchdog.join();
    let status = child
        .lock()
        .map_err(|_| anyhow::anyhow!("ffmpeg handle poisoned"))?
        .wait()?;
    let stderr = stderr.join().unwrap_or_default();
    ensure!(
        !timed_out.load(Ordering::SeqCst),
        "ffmpeg timed out after {} s",
        DECODE_DEADLINE.as_secs()
    );
    read?;
    ensure!(status.success(), "ffmpeg failed: {}", stderr.trim());
    Ok(frames)
}

/// Decodes movie frames into memory; `max_edge` 0 keeps native size.
pub fn frames(movie: &Path, times: FrameTimes, max_edge: u32) -> Result<Vec<RgbaFrame>> {
    let probe = probe(movie)?;
    ensure!(
        probe.width > 0 && probe.height > 0,
        "{} has no video",
        movie.display()
    );
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
                                    let rgba =
                                        decode(movie, Some(t.min(last)), scale, size, Some(1))?
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

/// Mixes a movie's audio to 16 kHz mono WAV, aligned to the movie's start: a late-starting audio
/// track is padded with silence rather than shifted earlier.
pub fn export_audio(movie: &Path, wav: &Path) -> Result<()> {
    let mut command = Command::new("ffmpeg");
    command.args(["-v", "error", "-nostdin", "-y"]);
    input(&mut command, movie);
    command
        .args(["-vn", "-af", "aresample=async=1:first_pts=0"])
        .args(["-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le"])
        .arg(wav);
    run(command, "ffmpeg (audio export)", Duration::from_secs(120))?;
    Ok(())
}

/// Re-encodes `movie` into `out` as HEVC with its long edge at most `max_edge` and the audio
/// copied, at the lowest CPU priority.
pub fn shrink(movie: &Path, out: &Path, max_edge: u32) -> Result<Shrunk> {
    let started = Instant::now();
    let before = std::fs::metadata(movie)?.len();
    let probe = probe(movie)?;
    let (width, height) = scaled_size(probe.width, probe.height, max_edge);
    // HEVC needs even dimensions even when the movie is not scaled down.
    let (width, height) = (width.max(2) & !1, height.max(2) & !1);
    let mut command = Command::new("nice");
    command.args(["-n", "19", "ffmpeg", "-v", "error", "-nostdin", "-y"]);
    input(&mut command, movie);
    command
        .args(["-vf", &format!("scale={width}:{height}")])
        .args([
            "-c:v", "libx265", "-preset", "fast", "-crf", "28", "-tag:v", "hvc1", "-c:a", "copy",
        ])
        .arg(out);
    run(command, "ffmpeg (shrink)", Duration::from_secs(1800))?;
    Ok(Shrunk {
        before,
        after: std::fs::metadata(out)?.len(),
        width,
        height,
        ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reads_size_rate_duration_and_audio() {
        let probe = parse_probe(include_bytes!("fixtures/ffprobe.json")).unwrap();
        assert_eq!(
            (
                probe.duration_ms,
                probe.width,
                probe.height,
                probe.audio_tracks
            ),
            (2_000, 320, 240, 1)
        );
        assert!((probe.fps - 30.0).abs() < 1e-9);
    }

    #[test]
    fn scaled_size_caps_the_long_edge_and_stays_even() {
        assert_eq!(scaled_size(1920, 1080, 480), (480, 270));
        assert_eq!(scaled_size(1920, 1080, 0), (1920, 1080));
        assert_eq!(scaled_size(300, 200, 480), (300, 200));
        assert_eq!(scaled_size(301, 201, 0), (301, 201));
        assert_eq!(scaled_size(301, 201, 480), (301, 201));
        assert_eq!(scaled_size(1081, 1921, 480), (270, 480));
    }

    /// Needs `ffmpeg` on PATH: a 2 s test pattern, decoded both ways.
    #[test]
    fn decodes_frames_from_a_real_movie() {
        let dir = std::env::temp_dir().join(format!("scrawl-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let movie = dir.join("t.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=30",
            ])
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
        assert_eq!(
            at.iter().map(|f| f.t_ms).collect::<Vec<_>>(),
            vec![100, 1_900]
        );
        assert_eq!(at[0].rgba.len(), 320 * 240 * 4);
        let wav = dir.join("a.wav");
        assert!(
            export_audio(&movie, &wav).is_err(),
            "no audio track to export"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Audio that starts 0.5 s after the video must be padded, not shifted to the start.
    #[test]
    fn exported_audio_keeps_a_late_start() {
        let dir = std::env::temp_dir().join(format!("scrawl-media-late-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let movie = dir.join("late.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=10",
            ])
            .args([
                "-itsoffset",
                "0.5",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
            ])
            .args([
                "-t", "2", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac",
            ])
            .arg(&movie)
            .status()
            .unwrap();
        assert!(made.success());
        let wav = dir.join("a.wav");
        export_audio(&movie, &wav).unwrap();
        // 16 kHz mono s16le: 32 000 bytes a second after a 44-byte header.
        let seconds = (std::fs::metadata(&wav).unwrap().len().saturating_sub(44)) as f64 / 32_000.0;
        assert!(seconds >= 1.9, "{seconds} s of audio");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A playlist saved as `.mp4` must not be followed.
    #[test]
    fn probe_refuses_a_playlist_posing_as_a_movie() {
        let dir = std::env::temp_dir().join(format!("scrawl-media-hls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.ts");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=10",
            ])
            .args([
                "-t", "1", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-f", "mpegts",
            ])
            .arg(&real)
            .status()
            .unwrap();
        assert!(made.success());
        let playlist = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nfile://{}\n#EXT-X-ENDLIST\n",
            real.display()
        );
        // The extension is what lets ffmpeg's own sniffing accept a playlist.
        for name in ["fake.mp4", "fake.m3u8"] {
            let fake = dir.join(name);
            std::fs::write(&fake, &playlist).unwrap();
            assert!(probe(&fake).is_err(), "{name} was followed");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
