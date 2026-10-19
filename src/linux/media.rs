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
