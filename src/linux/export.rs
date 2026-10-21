//! `scrawl export` on Linux: the plan `crate::export` makes (crop, camera path, click ripples,
//! audio window), rendered with `ffmpeg` instead of `scrawl-helper`.
//!
//! Three ways to render it, cheapest that is exact:
//!
//! - **Direct**: no camera and no ripples. One `ffmpeg` does everything: crop, scale, constant
//!   frame rate, x264/x265, AAC.
//! - **Static**: no camera, but click ripples. A decoder `ffmpeg` crops and scales to the output
//!   size and sends raw RGB frames; Rust draws the ripples into them and hands them to an encoder
//!   `ffmpeg` that adds the audio.
//! - **Zoom**: the camera moves. The decoder sends the whole recording as raw RGB at constant
//!   frame rate, and Rust resamples each frame to the camera's rectangle for its time (the same
//!   linear interpolation of the plan's rows the macOS helper does), draws the ripples and sends
//!   it to the encoder.
//!
//! Why Rust samples the zoom frames instead of a `crop`+`scale` filter fed by `sendcmd`: the
//! camera rectangle is fractional and changes every frame, while `crop` rounds to whole pixels
//! (and its size is fixed at configuration), which makes a slow zoom or pan step and shimmer.
//! Here the rectangle is sampled at its exact position (bilinear when zooming in, an exact area
//! average when scaling down), and the ripples are drawn at the sampled geometry. Decode and encode
//! stay in ffmpeg, so the cost is a resample per frame split over a few threads, well below what
//! x264 needs.
//!
//! The ink is not drawn: on Linux it is composited on the screen that `wf-recorder` records, so it
//! is already in `recording.mp4`.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use anyhow::{Context, Result, bail, ensure};

use crate::autocrop::Rect;
use crate::export::ink::Ripple;
use crate::export::{Done, Plan};

/// Where exports land by default: `$XDG_VIDEOS_DIR/Scrawl`, else `~/Videos/Scrawl`.
pub fn default_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let asked = Command::new("xdg-user-dir")
        .arg("VIDEOS")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
    videos_dir(asked.as_deref(), &home).join("Scrawl")
}

/// The Videos folder from `xdg-user-dir VIDEOS`' output. It prints `$HOME` when the folder is not
/// configured, which would put exports loose in the home directory, so that counts as unset.
fn videos_dir(xdg_output: Option<&str>, home: &Path) -> PathBuf {
    xdg_output
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute() && dir != home)
        .unwrap_or_else(|| home.join("Videos"))
}

/// What the source's colors are tagged as, so the export can say the same of its own.
#[derive(Clone, Debug, Default, PartialEq)]
struct SourceColor {
    space: Option<String>,
    primaries: Option<String>,
    transfer: Option<String>,
    /// `tv` (limited) or `pc` (full).
    range: Option<String>,
}

impl SourceColor {
    fn probe(video: &Path) -> Self {
        let mut command = Command::new("ffprobe");
        command
            .args(["-v", "error", "-select_streams", "v:0", "-of", "default=nw=1"])
            .args([
                "-show_entries",
                "stream=color_space,color_primaries,color_transfer,color_range",
            ]);
        crate::linux::media::input(&mut command, video);
        let out = command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success());
        out.map(|out| Self::parse(&String::from_utf8_lossy(&out.stdout)))
            .unwrap_or_default()
    }

    fn parse(text: &str) -> Self {
        let mut color = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() || value == "unknown" || value == "unspecified" {
                continue;
            }
            let slot = match key.trim() {
                "color_space" => &mut color.space,
                "color_primaries" => &mut color.primaries,
                "color_transfer" => &mut color.transfer,
                "color_range" => &mut color.range,
                _ => continue,
            };
            *slot = Some(value.to_owned());
        }
        color
    }

    /// `limited` unless the source says it is full range.
    fn range(&self) -> &str {
        if self.range.as_deref() == Some("pc") {
            "pc"
        } else {
            "tv"
        }
    }

    /// Encoder options that tag the output like the source (only what the source says).
    fn tags(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        for (flag, value) in [
            ("-colorspace", &self.space),
            ("-color_primaries", &self.primaries),
            ("-color_trc", &self.transfer),
            ("-color_range", &self.range),
        ] {
            if let Some(value) = value {
                args.push(flag.into());
                args.push(value.into());
            }
        }
        args
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Direct,
    Static,
    Zoom,
}

fn mode(plan: &Plan) -> Mode {
    if plan.camera.is_some() {
        Mode::Zoom
    } else if plan.ink.ripples.is_empty() {
        Mode::Direct
    } else {
        Mode::Static
    }
}

fn seconds(ms: u64) -> String {
    format!("{:.3}", ms as f64 / 1_000.0)
}

/// `crop=` for the plan's crop rectangle in source pixels, or nothing for the whole frame.
/// `exact` keeps odd offsets in subsampled video.
fn crop_filter(crop: Rect, (vw, vh): (u32, u32)) -> Option<String> {
    let edge = |v: f32, max: u32| ((v * max as f32).round().max(0.0) as u32).min(max);
    let (x0, y0) = (edge(crop[0], vw), edge(crop[1], vh));
    let (x1, y1) = (edge(crop[2], vw), edge(crop[3], vh));
    let (w, h) = (x1.saturating_sub(x0).max(2), y1.saturating_sub(y0).max(2));
    let (x0, y0) = (x0.min(vw.saturating_sub(w)), y0.min(vh.saturating_sub(h)));
    if (w, h) == (vw, vh) {
        return None;
    }
    Some(format!("crop={w}:{h}:{x0}:{y0}:exact=1"))
}

/// The frame rate every ffmpeg in the pipeline is held to.
fn fps_filter(plan: &Plan) -> String {
    format!("fps={}", plan.fps)
}

/// `scale` for the RGB hand-off: the recording's YUV is read as BT.709 here and written back as
/// BT.709 by the encoder, so the round trip leaves the colors as recorded whatever the file says
/// (the encoder then tags the output like the source).
fn to_rgb(range: &str) -> String {
    format!(
        "scale=in_color_matrix=bt709:in_range={range}:out_range=pc:flags=bicubic,format=rgb24"
    )
}

fn to_yuv(range: &str) -> String {
    format!(
        "scale=in_color_matrix=bt709:in_range=pc:out_color_matrix=bt709:out_range={range}:flags=bicubic,format=yuv420p"
    )
}

fn base(command: &mut Command) {
    command.args(["-v", "error", "-nostdin", "-hide_banner", "-y"]);
}

/// The encoder's options: codec, audio and container, for everything after the inputs.
fn encode_args(plan: &Plan, color: &SourceColor) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    fn add(args: &mut Vec<OsString>, items: &[&str]) {
        args.extend(items.iter().map(OsString::from));
    }
    if plan.codec == "hevc" {
        add(&mut args, &["-c:v", "libx265", "-preset", "veryfast", "-crf", "25"]);
        add(&mut args, &["-tag:v", "hvc1", "-x265-params", "log-level=error"]);
    } else {
        add(&mut args, &["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"]);
        add(&mut args, &["-profile:v", "high"]);
    }
    add(&mut args, &["-pix_fmt", "yuv420p", "-g", &(plan.fps * 2).to_string()]);
    if plan.audio.is_some() && plan.audio_length_ms > 0 {
        add(&mut args, &["-c:a", "aac", "-b:a", "128k", "-ar", "48000"]);
    }
    args.extend(color.tags());
    add(&mut args, &["-movflags", "+faststart", "-f", "mp4"]);
    args.push(plan.out.into());
    args
}

/// The audio input: `audio.wav` from `audio_start_ms` for `audio_length_ms`.
fn audio_input(command: &mut Command, plan: &Plan) -> bool {
    let Some(audio) = plan.audio.filter(|_| plan.audio_length_ms > 0) else {
        return false;
    };
    command
        .args(["-ss", &seconds(plan.audio_start_ms)])
        .args(["-t", &seconds(plan.audio_length_ms)]);
    if audio == plan.video {
        // An iPhone movie's own track: the same locked-down input as the video.
        crate::linux::media::input(command, audio);
    } else {
        // `audio.wav`: only the local file protocol and the WAV demuxer, so a file that is really
        // a playlist (concat, HLS) can't make ffmpeg read others.
        let mut url = OsString::from("file:");
        url.push(audio);
        command
            .args(["-protocol_whitelist", "file", "-f", "wav"])
            .arg("-i")
            .arg(url);
    }
    true
}

/// The one-process encode for `Mode::Direct`.
fn direct_command(plan: &Plan, color: &SourceColor) -> Command {
    let mut command = Command::new("ffmpeg");
    base(&mut command);
    crate::linux::media::input(&mut command, plan.video);
    let audio = audio_input(&mut command, plan);
    let (cw, ch) = crop_size(plan);
    let mut chain: Vec<String> = Vec::new();
    chain.extend(crop_filter(plan.crop, plan.video_px));
    if (cw, ch) != (plan.width, plan.height) {
        chain.push(format!("scale={}:{}:flags=lanczos", plan.width, plan.height));
    }
    chain.push(fps_filter(plan));
    command.args(["-vf", &chain.join(",")]);
    command.args(["-map", "0:v:0"]);
    if audio {
        command.args(["-map", "1:a:0"]);
    }
    command.args(encode_args(plan, color));
    command
}

/// The crop's size in source pixels (what `crop_filter` cuts).
fn crop_size(plan: &Plan) -> (u32, u32) {
    let (vw, vh) = plan.video_px;
    match crop_filter(plan.crop, plan.video_px) {
        None => (vw, vh),
        Some(filter) => {
            let numbers: Vec<u32> = filter
                .trim_start_matches("crop=")
                .split([':', '='])
                .filter_map(|n| n.parse().ok())
                .collect();
            (numbers[0], numbers[1])
        }
    }
}

/// The decoder for `Mode::Static` and `Mode::Zoom`: raw RGB frames at the plan's frame rate, at
/// the output size (static) or the recording's (zoom).
fn decoder_command(plan: &Plan, color: &SourceColor, mode: Mode) -> Command {
    let mut command = Command::new("ffmpeg");
    base(&mut command);
    crate::linux::media::input(&mut command, plan.video);
    let mut chain: Vec<String> = Vec::new();
    if mode == Mode::Static {
        chain.extend(crop_filter(plan.crop, plan.video_px));
        let (cw, ch) = crop_size(plan);
        if (cw, ch) != (plan.width, plan.height) {
            chain.push(format!(
                "scale={}:{}:flags=lanczos+accurate_rnd",
                plan.width, plan.height
            ));
        }
    }
    // The recording is variable frame rate (a frame per screen change). Scaled-down static frames
    // are cheap to convert, so they are converted before the rate is made constant; whole 1080p
    // frames are not, so the zoom's decoder makes the rate constant first and converts once per
    // output frame at most.
    if mode == Mode::Zoom {
        chain.push(fps_filter(plan));
        chain.push(to_rgb(color.range()));
    } else {
        chain.push(to_rgb(color.range()));
        chain.push(fps_filter(plan));
    }
    command
        .args(["-vf", &chain.join(",")])
        .args(["-an", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"]);
    command
}

/// The encoder for the piped modes: raw RGB frames on stdin, the audio, the output file.
fn encoder_command(plan: &Plan, color: &SourceColor) -> Command {
    let mut command = Command::new("ffmpeg");
    base(&mut command);
    command
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24"])
        .args(["-video_size", &format!("{}x{}", plan.width, plan.height)])
        .args(["-framerate", &plan.fps.to_string(), "-i", "-"]);
    let audio = audio_input(&mut command, plan);
    command
        .args(["-vf", &to_yuv(color.range())])
        .args(["-map", "0:v:0"]);
    if audio {
        command.args(["-map", "1:a:0"]);
    }
    command.args(encode_args(plan, color));
    command
}

/// The camera rectangle at `ms`: the plan's rows interpolated linearly, held before the first and
/// after the last (as `helper/Export.swift`'s `cameraRect`). Without rows, the crop.
fn camera_rect(keys: Option<&[[f64; 5]]>, crop: Rect, ms: f64) -> [f64; 4] {
    let Some(keys) = keys.filter(|k| !k.is_empty()) else {
        return crop.map(f64::from);
    };
    let row = |k: &[f64; 5]| [k[1], k[2], k[3], k[4]];
    let last = keys.len() - 1;
    if ms <= keys[0][0] {
        return row(&keys[0]);
    }
    if ms >= keys[last][0] {
        return row(&keys[last]);
    }
    // The last key at or before `ms`.
    let lo = keys.partition_point(|k| k[0] <= ms) - 1;
    let (a, b) = (&keys[lo], &keys[lo + 1]);
    let f = (ms - a[0]) / (b[0] - a[0]).max(1e-6);
    let mut out = [0.0; 4];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = a[i + 1] + (b[i + 1] - a[i + 1]) * f;
    }
    out
}

/// Source taps for each output position along one axis: `(source index, weight)` runs. Zooming in
/// (or 1:1) is linear interpolation between the two nearest source pixels at the exact fractional
/// position; scaling down averages the source over the output pixel's whole footprint.
struct Taps {
    index: Vec<u32>,
    weight: Vec<f32>,
    /// `offset[i]..offset[i + 1]` are output position `i`'s taps.
    offset: Vec<usize>,
}

impl Taps {
    fn new(origin: f64, span: f64, out_len: usize, src_len: usize) -> Self {
        let ratio = span / out_len as f64;
        let last = src_len as i64 - 1;
        let clamp = |i: i64| i.clamp(0, last) as u32;
        let mut taps = Self {
            index: Vec::new(),
            weight: Vec::new(),
            offset: vec![0],
        };
        for i in 0..out_len {
            let centre = origin + (i as f64 + 0.5) * ratio;
            if ratio <= 1.0 {
                let p = centre - 0.5;
                let i0 = p.floor();
                let f = (p - i0) as f32;
                taps.index.push(clamp(i0 as i64));
                taps.weight.push(1.0 - f);
                taps.index.push(clamp(i0 as i64 + 1));
                taps.weight.push(f);
            } else {
                let (lo, hi) = (centre - ratio / 2.0, centre + ratio / 2.0);
                for s in lo.floor() as i64..hi.ceil() as i64 {
                    let overlap = ((s + 1) as f64).min(hi) - (s as f64).max(lo);
                    if overlap > 0.0 {
                        taps.index.push(clamp(s));
                        taps.weight.push((overlap / ratio) as f32);
                    }
                }
            }
            taps.offset.push(taps.index.len());
        }
        taps
    }

    fn at(&self, i: usize) -> impl Iterator<Item = (usize, f32)> + '_ {
        (self.offset[i]..self.offset[i + 1]).map(|k| (self.index[k] as usize, self.weight[k]))
    }
}

/// Resamples `rect` (normalized to the source) of the RGB24 `src` into the RGB24 `out`, over
/// `threads` bands of rows.
fn resample(
    src: &[u8],
    (sw, sh): (usize, usize),
    rect: [f64; 4],
    out: &mut [u8],
    (ow, oh): (usize, usize),
    threads: usize,
) {
    let xs = Taps::new(rect[0] * sw as f64, (rect[2] - rect[0]) * sw as f64, ow, sw);
    let ys = Taps::new(rect[1] * sh as f64, (rect[3] - rect[1]) * sh as f64, oh, sh);
    let rows_per = oh.div_ceil(threads.max(1)).max(1);
    std::thread::scope(|scope| {
        for (band, chunk) in out.chunks_mut(rows_per * ow * 3).enumerate() {
            let (xs, ys) = (&xs, &ys);
            scope.spawn(move || {
                let first = band * rows_per;
                let rows = chunk.len() / (ow * 3);
                // The source rows this band reads, filtered horizontally once each.
                let taps = ys.offset[first]..ys.offset[first + rows];
                let lo = ys.index[taps.clone()].iter().copied().min().unwrap_or(0) as usize;
                let hi = ys.index[taps].iter().copied().max().unwrap_or(0) as usize;
                let mut across = vec![0.0_f32; (hi - lo + 1) * ow * 3];
                for (sy, filtered) in (lo..=hi).zip(across.chunks_exact_mut(ow * 3)) {
                    let line = &src[sy * sw * 3..(sy + 1) * sw * 3];
                    for (x, cell) in filtered.chunks_exact_mut(3).enumerate() {
                        let (mut r, mut g, mut b) = (0.0_f32, 0.0_f32, 0.0_f32);
                        for k in xs.offset[x]..xs.offset[x + 1] {
                            let at = xs.index[k] as usize * 3;
                            let w = xs.weight[k];
                            r += w * f32::from(line[at]);
                            g += w * f32::from(line[at + 1]);
                            b += w * f32::from(line[at + 2]);
                        }
                        cell.copy_from_slice(&[r, g, b]);
                    }
                }
                let mut acc = vec![0.0_f32; ow * 3];
                for (r, row) in chunk.chunks_exact_mut(ow * 3).enumerate() {
                    acc.fill(0.0);
                    for (sy, wy) in ys.at(first + r) {
                        let filtered = &across[(sy - lo) * ow * 3..(sy - lo + 1) * ow * 3];
                        for (a, v) in acc.iter_mut().zip(filtered) {
                            *a += wy * v;
                        }
                    }
                    for (byte, value) in row.iter_mut().zip(&acc) {
                        *byte = (value + 0.5).min(255.0) as u8;
                    }
                }
            });
        }
    });
}

/// Maps the capture's normalized coordinates to output pixels for one camera rectangle.
#[derive(Clone, Copy, Debug)]
struct View {
    source: (f64, f64),
    origin: (f64, f64),
    scale: (f64, f64),
}

impl View {
    fn new(source: (u32, u32), rect: [f64; 4], out: (u32, u32)) -> Self {
        let (sw, sh) = (f64::from(source.0), f64::from(source.1));
        Self {
            source: (sw, sh),
            origin: (rect[0] * sw, rect[1] * sh),
            scale: (
                f64::from(out.0) / ((rect[2] - rect[0]) * sw).max(1.0),
                f64::from(out.1) / ((rect[3] - rect[1]) * sh).max(1.0),
            ),
        }
    }

    fn point(&self, x: f32, y: f32) -> (f64, f64) {
        (
            (f64::from(x) * self.source.0 - self.origin.0) * self.scale.0,
            (f64::from(y) * self.source.1 - self.origin.1) * self.scale.1,
        )
    }

    fn unit(&self) -> f64 {
        (self.scale.0 + self.scale.1) / 2.0
    }
}

/// How long a click ripple lives.
const RIPPLE_MS: f64 = 520.0;

fn blend(frame: &mut [u8], at: usize, colour: [f32; 3], alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    for (byte, c) in frame[at..at + 3].iter_mut().zip(colour) {
        *byte = (f32::from(*byte) * (1.0 - alpha) + c * alpha + 0.5) as u8;
    }
}

/// Draws the ripples alive at `ms` into an RGB24 frame, as `helper/Export.swift` does: a 30%
/// blue disc growing and fading, with a white ring and a slightly larger blue ring.
fn draw_ripples(
    frame: &mut [u8],
    (ow, oh): (usize, usize),
    view: &View,
    ripples: &[Ripple],
    k: f32,
    ms: f64,
) {
    const BLUE: [f32; 3] = [0.0, 136.0, 255.0];
    const WHITE: [f32; 3] = [255.0, 255.0, 255.0];
    for ripple in ripples {
        let age = ms - ripple.t as f64;
        if !(0.0..RIPPLE_MS).contains(&age) {
            continue;
        }
        let p = age / RIPPLE_MS;
        let ease = 1.0 - (1.0 - p).powi(3);
        let unit = f64::from(k) * view.unit();
        let (cx, cy) = view.point(ripple.x, ripple.y);
        let radius = (9.0 + 24.0 * ease) * unit;
        let half = 1.75 * unit / 2.0;
        let outer = radius + 2.5 * unit;
        let fade = (1.0 - p) as f32;
        let reach = outer + half + 1.0;
        let x0 = ((cx - reach).floor().max(0.0)) as usize;
        let x1 = ((cx + reach).ceil().min(ow as f64)).max(0.0) as usize;
        let y0 = ((cy - reach).floor().max(0.0)) as usize;
        let y1 = ((cy + reach).ceil().min(oh as f64)).max(0.0) as usize;
        let cover = |edge: f64| edge.clamp(0.0, 1.0) as f32;
        for y in y0..y1 {
            for x in x0..x1 {
                let d = (x as f64 + 0.5 - cx).hypot(y as f64 + 0.5 - cy);
                let at = (y * ow + x) * 3;
                blend(frame, at, BLUE, cover(radius - d + 0.5) * 0.30 * fade);
                blend(frame, at, WHITE, cover(half - (d - radius).abs() + 0.5) * fade);
                blend(frame, at, BLUE, cover(half - (d - outer).abs() + 0.5) * 0.92 * fade);
            }
        }
    }
}

/// Collects a child's stderr on a thread so a chatty ffmpeg can't block.
fn drain_stderr(child: &mut Child) -> std::thread::JoinHandle<String> {
    let stderr = child.stderr.take();
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = stderr {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    })
}

fn spawn(mut command: Command, name: &str, stdin: bool, stdout: bool) -> Result<Child> {
    command
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(if stdout { Stdio::piped() } else { Stdio::null() })
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Could not run {name} (install ffmpeg)"))
}

fn finish(mut child: Child, errors: std::thread::JoinHandle<String>, name: &str) -> Result<()> {
    let status = child.wait()?;
    let message = errors.join().unwrap_or_default();
    ensure!(status.success(), "{name} failed: {}", message.trim());
    Ok(())
}

/// Reads one whole frame; `false` at the end of the stream (a cut-off last frame is dropped, and
/// the decoder's exit status says why).
fn read_frame(input: &mut impl Read, buffer: &mut [u8]) -> Result<bool> {
    let mut filled = 0;
    while filled < buffer.len() {
        match input.read(&mut buffer[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("Could not read the decoded video"),
        }
    }
    Ok(true)
}

/// Renders `plan` with ffmpeg, calling `progress` with `0.0..=1.0` (piped modes only; a direct
/// encode reports when it is done).
pub(crate) fn render(
    plan: &Plan,
    background: bool,
    progress: &mut dyn FnMut(f32),
) -> Result<Done> {
    ensure!(plan.video_px.0 > 0 && plan.video_px.1 > 0, "The recording has no video");
    if background {
        // Utility priority: this thread's niceness is inherited by the ffmpeg processes it starts.
        // (Linux niceness is per thread; it is only raised on the export's own thread.)
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        }
    }
    let color = SourceColor::probe(plan.video);
    let result = match mode(plan) {
        Mode::Direct => {
            let mut child = spawn(direct_command(plan, &color), "ffmpeg", false, false)?;
            let errors = drain_stderr(&mut child);
            finish(child, errors, "ffmpeg")
        }
        mode => render_piped(plan, &color, mode, background, progress),
    };
    result?;
    let bytes = std::fs::metadata(plan.out)
        .with_context(|| format!("ffmpeg wrote no {}", plan.out.display()))?
        .len();
    ensure!(bytes > 0, "ffmpeg wrote an empty file");
    Ok(Done {
        duration_ms: plan.duration_ms,
        bytes,
    })
}

fn render_piped(
    plan: &Plan,
    color: &SourceColor,
    mode: Mode,
    background: bool,
    progress: &mut dyn FnMut(f32),
) -> Result<()> {
    let (ow, oh) = (plan.width as usize, plan.height as usize);
    let (sw, sh) = (plan.video_px.0 as usize, plan.video_px.1 as usize);
    let (iw, ih) = if mode == Mode::Zoom { (sw, sh) } else { (ow, oh) };
    // Resample bands: a few threads is plenty next to x264, and fewer when running in the background.
    let threads = std::thread::available_parallelism()
        .map_or(2, usize::from)
        .min(if background { 2 } else { 6 });

    let mut decoder = spawn(decoder_command(plan, color, mode), "ffmpeg (decode)", false, true)?;
    let decoder_errors = drain_stderr(&mut decoder);
    let mut encoder = match spawn(encoder_command(plan, color), "ffmpeg (encode)", true, false) {
        Ok(encoder) => encoder,
        Err(error) => {
            let _ = decoder.kill();
            let _ = decoder.wait();
            return Err(error);
        }
    };
    let encoder_errors = drain_stderr(&mut encoder);
    let mut source = decoder.stdout.take().context("No decoder output")?;
    let mut sink = encoder.stdin.take().context("No encoder input")?;

    // A writer thread keeps the encoder fed while the next frame is read and drawn.
    let (frames, queue) = mpsc::sync_channel::<Vec<u8>>(4);
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for frame in queue {
            sink.write_all(&frame)?;
        }
        Ok(())
    });

    let total = (plan.duration_ms * u64::from(plan.fps)).div_ceil(1_000).max(1);
    let camera = plan.camera.as_deref();
    let mut input = vec![0_u8; iw * ih * 3];
    // The recording holds a frame while the screen is still, and the camera holds while it rests,
    // so most frames repeat the last one: resampling is skipped when neither changed.
    let (mut seen, mut seen_rect, mut seen_out) = (Vec::new(), None, Vec::new());
    let mut count: u64 = 0;
    let rendered: Result<()> = (|| {
        while read_frame(&mut source, &mut input)? {
            let ms = count as f64 * 1_000.0 / f64::from(plan.fps);
            let rect = camera_rect(camera, plan.crop, ms);
            let mut frame = if mode == Mode::Zoom {
                if seen_rect != Some(rect) || seen != input {
                    seen_out.resize(ow * oh * 3, 0);
                    resample(&input, (sw, sh), rect, &mut seen_out, (ow, oh), threads);
                    seen.clone_from(&input);
                    seen_rect = Some(rect);
                }
                seen_out.clone()
            } else {
                input.clone()
            };
            if !plan.ink.ripples.is_empty() {
                let view = View::new(plan.video_px, rect, (plan.width, plan.height));
                draw_ripples(&mut frame, (ow, oh), &view, &plan.ink.ripples, plan.ink.k, ms);
            }
            if frames.send(frame).is_err() {
                // The writer stopped: the encoder died or closed its input.
                break;
            }
            count += 1;
            if count % 30 == 0 {
                progress((count as f32 / total as f32).min(0.99));
            }
        }
        Ok(())
    })();
    drop(frames);
    drop(source);
    let written = writer.join().unwrap_or_else(|_| Err(std::io::ErrorKind::Other.into()));

    if rendered.is_err() || (written.is_err() && count > 0) {
        let _ = decoder.kill();
    }
    let decoded = finish(decoder, decoder_errors, "ffmpeg (decode)");
    let encoded = finish(encoder, encoder_errors, "ffmpeg (encode)");
    // The encoder's message is the more useful one when the pipe broke.
    encoded?;
    rendered?;
    decoded?;
    if let Err(error) = written {
        bail!("Could not write to the encoder: {error}");
    }
    ensure!(count > 0, "ffmpeg decoded no frames");
    Ok(())
}

static EXPORTING: AtomicBool = AtomicBool::new(false);

/// The tray's "Export Last Capture as Video…": exports the newest bundle to the default folder on
/// a low-priority thread and tells the user with a notification, with the path.
pub fn start_latest_in_background() {
    use crate::capture::scrawl_log;
    if EXPORTING.swap(true, Ordering::SeqCst) {
        super::notify("Already exporting a video");
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("scrawl-export".into())
        .spawn(|| {
            super::notify("Exporting the last capture as a video…");
            let result = crate::handoff::load_bundle(Path::new("latest")).and_then(
                |(dir, bundle)| {
                    let options = crate::export::Options {
                        background: true,
                        ..Default::default()
                    };
                    crate::export::export(&dir, &bundle, &options, &mut |_| {})
                },
            );
            match result {
                Ok(exported) => {
                    scrawl_log(format!("Exported {}", crate::export::describe(&exported)));
                    super::notify(&format!("Video exported\n{}", exported.path.display()));
                }
                Err(error) => {
                    scrawl_log(format!("Video export failed: {error:#}"));
                    super::notify(&format!("Video export failed: {error:#}"));
                }
            }
            EXPORTING.store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        EXPORTING.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::ink::InkPlan;

    fn plan<'a>(ink: &'a InkPlan, out: &'a Path) -> Plan<'a> {
        Plan {
            video: Path::new("/v/recording.mp4"),
            out,
            audio: None,
            audio_start_ms: 0,
            audio_length_ms: 0,
            width: 960,
            height: 540,
            fps: 30,
            codec: "h264",
            duration_ms: 2_000,
            crop: [0.0, 0.0, 1.0, 1.0],
            camera: None,
            ink,
            video_px: (1920, 1080),
        }
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn videos_dir_follows_xdg_but_not_home() {
        let home = Path::new("/home/u");
        assert_eq!(videos_dir(Some("/data/Clips\n"), home), Path::new("/data/Clips"));
        assert_eq!(videos_dir(Some("/home/u\n"), home), Path::new("/home/u/Videos"));
        assert_eq!(videos_dir(Some(""), home), Path::new("/home/u/Videos"));
        assert_eq!(videos_dir(Some("relative"), home), Path::new("/home/u/Videos"));
        assert_eq!(videos_dir(None, home), Path::new("/home/u/Videos"));
    }

    #[test]
    fn source_color_is_read_from_ffprobe_and_unknowns_are_skipped() {
        let color = SourceColor::parse(
            "color_range=tv\ncolor_space=bt470bg\ncolor_transfer=unknown\ncolor_primaries=unknown\n",
        );
        assert_eq!(color.space.as_deref(), Some("bt470bg"));
        assert_eq!(color.range(), "tv");
        assert_eq!(color.tags().len(), 4, "colorspace and range, with their flags");
        assert_eq!(SourceColor::parse("color_range=pc\n").range(), "pc");
        assert_eq!(SourceColor::default().range(), "tv");
        assert!(SourceColor::default().tags().is_empty());
    }

    #[test]
    fn the_crop_filter_cuts_whole_pixels_inside_the_frame() {
        assert_eq!(crop_filter([0.0, 0.0, 1.0, 1.0], (1920, 1080)), None);
        assert_eq!(
            crop_filter([0.25, 0.25, 0.75, 0.5], (1920, 1080)).as_deref(),
            Some("crop=960:270:480:270:exact=1")
        );
        // A rectangle a hair over the edge still lands inside it.
        assert_eq!(
            crop_filter([0.5, 0.0, 1.0004, 1.0], (1920, 1080)).as_deref(),
            Some("crop=960:1080:960:0:exact=1")
        );
    }

    #[test]
    fn the_mode_follows_the_camera_and_the_ripples() {
        let none = InkPlan::default();
        let out = PathBuf::from("/o.mp4");
        let mut p = plan(&none, &out);
        assert_eq!(mode(&p), Mode::Direct);
        p.camera = Some(vec![[0.0, 0.0, 0.0, 1.0, 1.0]]);
        assert_eq!(mode(&p), Mode::Zoom);
        let ripples = InkPlan {
            ripples: vec![Ripple { x: 0.5, y: 0.5, t: 0 }],
            ..Default::default()
        };
        assert_eq!(mode(&plan(&ripples, &out)), Mode::Static);
    }

    #[test]
    fn the_direct_command_crops_scales_holds_the_rate_and_aligns_the_audio() {
        let ink = InkPlan::default();
        let out = PathBuf::from("/o.partial.mp4");
        let audio = PathBuf::from("/v/audio.wav");
        let mut p = plan(&ink, &out);
        p.audio = Some(&audio);
        p.audio_start_ms = 672;
        p.audio_length_ms = 15_000;
        p.crop = [0.25, 0.25, 0.75, 0.75];
        let a = args(&direct_command(&p, &SourceColor::default()));
        let joined = a.join(" ");
        assert!(joined.contains("-ss 0.672 -t 15.000"), "{joined}");
        assert!(joined.contains("crop=960:540:480:270:exact=1,fps=30"), "{joined}");
        assert!(joined.contains("-map 0:v:0 -map 1:a:0"), "{joined}");
        assert!(joined.contains("libx264") && joined.contains("-c:a aac"), "{joined}");
        assert!(joined.ends_with("/o.partial.mp4"), "{joined}");
        p.codec = "hevc";
        let hevc = args(&direct_command(&p, &SourceColor::default())).join(" ");
        assert!(hevc.contains("libx265") && hevc.contains("hvc1"), "{hevc}");
    }

    #[test]
    fn the_audio_input_is_a_locked_down_wav_unless_it_is_the_movie() {
        let ink = InkPlan::default();
        let out = PathBuf::from("/o.mp4");
        let wav = PathBuf::from("/b/audio.wav");
        let mut p = plan(&ink, &out);
        p.audio = Some(&wav);
        p.audio_length_ms = 1_000;
        let joined = args(&direct_command(&p, &SourceColor::default())).join(" ");
        assert!(
            joined.contains("-protocol_whitelist file -f wav -i file:/b/audio.wav"),
            "{joined}"
        );
        // The movie's own audio goes through the same whitelist as the video.
        p.audio = Some(p.video);
        let movie = args(&direct_command(&p, &SourceColor::default())).join(" ");
        assert!(!movie.contains("-f wav"), "{movie}");
        assert_eq!(movie.matches("-format_whitelist").count(), 2, "{movie}");
    }

    #[test]
    fn the_piped_commands_hand_over_rgb_at_the_right_size() {
        let ink = InkPlan::default();
        let out = PathBuf::from("/o.mp4");
        let p = plan(&ink, &out);
        let color = SourceColor::default();
        let still = args(&decoder_command(&p, &color, Mode::Static)).join(" ");
        assert!(still.contains("scale=960:540:flags=lanczos"), "{still}");
        assert!(still.contains("format=rgb24,fps=30"), "{still}");
        let zoom = args(&decoder_command(&p, &color, Mode::Zoom)).join(" ");
        assert!(!zoom.contains("scale=960"), "the zoom decoder keeps the source size: {zoom}");
        assert!(zoom.contains("fps=30,scale=in_color_matrix"), "constant rate first: {zoom}");
        let encode = args(&encoder_command(&p, &color)).join(" ");
        assert!(encode.contains("-video_size 960x540 -framerate 30 -i -"), "{encode}");
        assert!(!encode.contains("-map 1:a"), "no audio input: {encode}");
    }

    #[test]
    fn the_camera_is_interpolated_between_rows_and_held_outside_them() {
        let keys = [[0.0, 0.0, 0.0, 1.0, 1.0], [1_000.0, 0.2, 0.2, 0.6, 0.6]];
        let crop = [0.0, 0.0, 1.0, 1.0];
        assert_eq!(camera_rect(Some(&keys), crop, -5.0), [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(camera_rect(Some(&keys), crop, 2_000.0), [0.2, 0.2, 0.6, 0.6]);
        let mid = camera_rect(Some(&keys), crop, 500.0);
        assert!((mid[0] - 0.1).abs() < 1e-9 && (mid[2] - 0.8).abs() < 1e-9);
        assert_eq!(camera_rect(None, [0.1, 0.1, 0.9, 0.9], 5.0), [0.1f32 as f64, 0.1f32 as f64, 0.9f32 as f64, 0.9f32 as f64]);
    }

    #[test]
    fn taps_are_normalized_and_one_to_one_is_a_copy() {
        for (span, out) in [(8.0, 8), (8.0, 3), (2.5, 9), (5.0, 2)] {
            let taps = Taps::new(1.0, span, out, 16);
            for i in 0..out {
                let sum: f32 = taps.at(i).map(|(_, w)| w).sum();
                assert!((sum - 1.0).abs() < 1e-4, "{span}->{out} at {i}: {sum}");
            }
        }
        let src: Vec<u8> = (0..4 * 3 * 3).map(|i| (i * 7 % 251) as u8).collect();
        let mut out = vec![0; src.len()];
        resample(&src, (4, 3), [0.0, 0.0, 1.0, 1.0], &mut out, (4, 3), 2);
        assert_eq!(out, src);
    }

    #[test]
    fn scaling_down_averages_and_zooming_in_interpolates_at_the_exact_position() {
        // 4x1 gray ramp 0, 100, 200, 40: two output pixels average pairs.
        let src: Vec<u8> = [0, 100, 200, 40].iter().flat_map(|&v| [v; 3]).collect();
        let mut out = vec![0; 2 * 3];
        resample(&src, (4, 1), [0.0, 0.0, 1.0, 1.0], &mut out, (2, 1), 1);
        assert_eq!(out, [50, 50, 50, 120, 120, 120]);
        // The middle half (pixels 1 and 2) at 4 pixels wide: linear between the neighbours at
        // each output pixel's exact position.
        let mut zoomed = vec![0; 4 * 3];
        resample(&src, (4, 1), [0.25, 0.0, 0.75, 1.0], &mut zoomed, (4, 1), 1);
        let reds: Vec<u8> = zoomed.chunks(3).map(|p| p[0]).collect();
        assert_eq!(reds, [75, 125, 175, 160]);
    }

    #[test]
    fn a_ripple_grows_fades_and_leaves_the_rest_of_the_frame_alone() {
        let (w, h) = (200usize, 100usize);
        let view = View::new((200, 100), [0.0, 0.0, 1.0, 1.0], (200, 100));
        let ripples = [Ripple { x: 0.5, y: 0.5, t: 1_000 }];
        let blank = vec![40_u8; w * h * 3];

        let mut before = blank.clone();
        draw_ripples(&mut before, (w, h), &view, &ripples, 1.0, 999.0);
        assert_eq!(before, blank, "nothing before the click");

        let mut early = blank.clone();
        draw_ripples(&mut early, (w, h), &view, &ripples, 1.0, 1_050.0);
        let at = (50 * w + 100) * 3;
        assert!(early[at + 2] > early[at], "blue at the centre: {:?}", &early[at..at + 3]);
        assert_eq!(&early[..3], &blank[..3], "the corner is untouched");

        let mut after = blank.clone();
        draw_ripples(&mut after, (w, h), &view, &ripples, 1.0, 1_520.0);
        assert_eq!(after, blank, "gone after 520 ms");
    }

    fn have_ffmpeg() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn make_inputs(dir: &Path) -> (PathBuf, PathBuf) {
        let video = dir.join("recording.mp4");
        let wav = dir.join("audio.wav");
        let made = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=24:duration=2"])
            .args(["-vf", "format=yuv420p", "-c:v", "libx264"])
            .arg(&video)
            .status()
            .unwrap();
        assert!(made.success());
        let made = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=3:sample_rate=16000"])
            .args(["-ac", "1", "-c:a", "pcm_s16le"])
            .arg(&wav)
            .status()
            .unwrap();
        assert!(made.success());
        (video, wav)
    }

    fn probed(path: &Path) -> (crate::helper::Probe, f64) {
        let probe = crate::helper::probe(path).unwrap();
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=start_time", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        let start = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(f64::NAN);
        (probe, start)
    }

    #[test]
    fn all_three_renderers_make_an_mp4_with_the_asked_size_rate_and_audio() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("scrawl-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (video, wav) = make_inputs(&dir);
        let ripples = InkPlan {
            k: 1.0,
            ripples: vec![Ripple { x: 0.5, y: 0.5, t: 500 }],
            ..Default::default()
        };
        let none = InkPlan::default();
        for (name, ink, camera) in [
            ("direct", &none, None),
            ("static", &ripples, None),
            ("zoom", &ripples, Some(vec![[0.0, 0.0, 0.0, 1.0, 1.0], [1_000.0, 0.25, 0.25, 0.75, 0.75]])),
        ] {
            let out = dir.join(format!("{name}.mp4"));
            let p = Plan {
                video: &video,
                out: &out,
                audio: Some(&wav),
                audio_start_ms: 500,
                audio_length_ms: 1_500,
                width: 160,
                height: 90,
                fps: 30,
                codec: "h264",
                duration_ms: 2_000,
                crop: [0.0, 0.0, 1.0, 1.0],
                camera,
                ink,
                video_px: (320, 180),
            };
            let mut last = 0.0;
            let done = render(&p, false, &mut |f| last = f).unwrap();
            assert!(done.bytes > 0, "{name}");
            let (probe, audio_start) = probed(&out);
            assert_eq!((probe.width, probe.height), (160, 90), "{name}");
            assert!((probe.duration_ms as i64 - 2_000).abs() < 100, "{name}: {}", probe.duration_ms);
            assert_eq!(probe.audio_tracks, 1, "{name}");
            assert!(audio_start.abs() < 0.05, "{name}: audio starts at {audio_start}");
            assert!(last <= 1.0, "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
