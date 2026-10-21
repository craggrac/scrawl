//! Blurring sensitive windows out of a capture (Linux only: Mac captures aren't touched).
//!
//! `redactApps` in `~/.config/scrawl/config.json` lists window patterns (`Patterns`). At finish,
//! before anything is decoded, summarised or copied, the windows that match are found in the
//! window log (`plan`), their titles are replaced with "(redacted)" there and on the target
//! (`scrub_titles`), and the places they covered are pixelated and darkened in `recording.mp4`
//! (`redact_video`). Keyframes, contact sheets and `scrawl export` are all decoded from that
//! video afterwards, so they carry the same blur and nothing else has to know about it.
//!
//! The video is rewritten in pieces: it is cut at keyframes, only the pieces a blur touches are
//! re-encoded and the rest are copied, then everything is joined again. A capture where no
//! window matches costs nothing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

use crate::linux::media;
use crate::model::{CaptureMode, TargetFrame, TargetInfo};
use crate::windows::WindowSnapshot;

/// What `redactApps` holds when the config doesn't say: the password managers. A password
/// manager in a recording that goes to an agent is a leak with nothing to gain from showing it.
/// Chat and banking windows depend on what is being recorded (the capture may be a bug in that
/// very app), so those are left to the user to add (`"whatsapp"`, `"title:*Bank*"`).
pub const DEFAULT_APPS: [&str; 5] = ["keepassxc", "bitwarden", "1password", "proton pass", "proton-pass"];

/// What a matched window's title becomes in `scrawl.json` and `summary.md`.
pub const REDACTED: &str = "(redacted)";

/// A pattern starting with this matches the window title instead of its class.
const TITLE_PREFIX: &str = "title:";

/// The patterns in effect: `configured` (`redactApps`) or `DEFAULT_APPS` when the config has none.
pub fn apps_or_default(configured: Option<Vec<String>>) -> Vec<String> {
    configured.unwrap_or_else(|| DEFAULT_APPS.iter().map(|app| (*app).to_owned()).collect())
}

/// Window patterns: a class pattern, or `title:` and a title pattern. Without `*` or `?` a
/// pattern matches when the text contains it, otherwise (a glob) when the whole text matches it;
/// case is ignored either way.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Patterns(Vec<Pattern>);

#[derive(Clone, Debug, PartialEq)]
struct Pattern {
    title: bool,
    text: Vec<char>,
    glob: bool,
}

impl Patterns {
    pub fn parse(entries: &[String]) -> Self {
        Self(
            entries
                .iter()
                .filter_map(|entry| {
                    let entry = entry.trim();
                    let title = entry
                        .get(..TITLE_PREFIX.len())
                        .is_some_and(|head| head.eq_ignore_ascii_case(TITLE_PREFIX));
                    let text = if title { entry[TITLE_PREFIX.len()..].trim() } else { entry };
                    (!text.is_empty()).then(|| Pattern {
                        title,
                        glob: text.contains(['*', '?']),
                        text: text.to_lowercase().chars().collect(),
                    })
                })
                .collect(),
        )
    }

    /// The configured patterns.
    pub fn configured() -> Self {
        Self::parse(&apps_or_default(crate::handoff::redact_apps()))
    }

    /// Whether a window with this class and title is one to hide.
    pub fn matches(&self, app: &str, title: &str) -> bool {
        let (app, title) = (app.to_lowercase(), title.to_lowercase());
        self.0.iter().any(|pattern| {
            let subject = if pattern.title { &title } else { &app };
            if pattern.glob {
                let subject: Vec<char> = subject.chars().collect();
                glob_matches(&pattern.text, &subject)
            } else {
                let needle: String = pattern.text.iter().collect();
                subject.contains(&needle)
            }
        })
    }
}

/// `pattern` (`*` any run, `?` any one character) against all of `text`.
fn glob_matches(pattern: &[char], text: &[char]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut resume: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                resume = Some((p, t));
                p += 1;
            }
            Some(&c) if c == '?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match resume {
                Some((star, at)) => {
                    resume = Some((star, at + 1));
                    p = star + 1;
                    t = at + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// Replaces the titles of the windows `patterns` match, in the window log and on the target.
pub fn scrub_titles(snapshots: &mut [WindowSnapshot], target: &mut TargetInfo, patterns: &Patterns) {
    for window in snapshots.iter_mut().flat_map(|s| s.windows.iter_mut()) {
        if !window.title.is_empty() && patterns.matches(&window.app, &window.title) {
            window.title = REDACTED.to_owned();
        }
    }
    if let (Some(app), Some(title)) = (&target.app, &mut target.window_title)
        && !title.is_empty()
        && patterns.matches(app, title)
    {
        *title = REDACTED.to_owned();
    }
}

// MARK: Where and when

/// Logical points around a window that are hidden with it (its border and shadow).
const MARGIN_PT: f64 = 8.0;
/// Time added to each side of a blur, for the video and the log not agreeing to the frame.
const TIME_PAD_MS: i64 = 300;
/// Hyprland slides a workspace in and out. A matched window that appears or disappears between
/// two snapshots may be moving across the screen from just before the second one until this
/// long after it, so its whole row and column are hidden then.
const SLIDE_BEFORE_MS: i64 = 150;
const SLIDE_AFTER_MS: i64 = 700;

/// One place to hide for a stretch of the video: `rect` is `[left, top, right, bottom]` as
/// fractions of the frame, the times are on the video's clock (`u64::MAX` is "to the end").
/// `solid` fills it with black instead of pixelating it (the big, short places of a slide).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Blur {
    pub rect: [f64; 4],
    pub start_ms: u64,
    pub end_ms: u64,
    pub solid: bool,
}

#[derive(Clone, Copy)]
struct Seen {
    id: u32,
    rect: [f64; 4],
}

fn hull(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// What to hide in the video: for each interval between two window snapshots, every matched
/// window of either snapshot (the window log only samples every second or when focus moves, so
/// a window that moved, opened or closed in between may be anywhere between), a moved window
/// as one box over its path, plus the slide described at `SLIDE_AFTER_MS`. Windows partly off
/// the screen are cut at its edge.
pub fn plan(
    snapshots: &[WindowSnapshot],
    patterns: &Patterns,
    frame: TargetFrame,
    video_offset_ms: u64,
) -> Vec<Blur> {
    let (width, height) = (frame.width.max(1.0), frame.height.max(1.0));
    let seen = |snapshot: &WindowSnapshot| -> Vec<Seen> {
        snapshot
            .windows
            .iter()
            .filter(|w| patterns.matches(&w.app, &w.title))
            .filter_map(|w| {
                let [x, y, w_, h] = w.bounds.map(f64::from);
                let rect = [
                    ((x - MARGIN_PT) / width).max(0.0),
                    ((y - MARGIN_PT) / height).max(0.0),
                    ((x + w_ + MARGIN_PT) / width).min(1.0),
                    ((y + h + MARGIN_PT) / height).min(1.0),
                ];
                (rect[2] > rect[0] && rect[3] > rect[1]).then_some(Seen { id: w.id, rect })
            })
            .collect()
    };
    let on_video = |t: u64| t as i64 - video_offset_ms as i64;
    let mut blurs = Vec::new();
    let mut push = |rect: [f64; 4], from: i64, to: i64, solid: bool| {
        if to >= 0 {
            blurs.push(Blur {
                rect,
                start_ms: from.max(0) as u64,
                end_ms: if to == i64::MAX { u64::MAX } else { to as u64 },
                solid,
            });
        }
    };
    for (i, snapshot) in snapshots.iter().enumerate() {
        let current = seen(snapshot);
        let following = snapshots.get(i + 1);
        let next = following.map(seen).unwrap_or_default();
        let from = if i == 0 { 0 } else { on_video(snapshot.t) - TIME_PAD_MS };
        let to = following.map_or(i64::MAX, |f| on_video(f.t) + TIME_PAD_MS);
        for window in &current {
            let rect = match next.iter().find(|n| n.id == window.id) {
                Some(later) => hull(window.rect, later.rect),
                None => window.rect,
            };
            push(rect, from, to, false);
        }
        for window in next.iter().filter(|n| !current.iter().any(|c| c.id == n.id)) {
            push(window.rect, from, to, false);
        }
        if let Some(following) = following {
            let changed = current
                .iter()
                .filter(|c| !next.iter().any(|n| n.id == c.id))
                .chain(next.iter().filter(|n| !current.iter().any(|c| c.id == n.id)));
            for window in changed {
                let [left, top, right, bottom] = window.rect;
                let (from, to) = (
                    on_video(following.t) - SLIDE_BEFORE_MS,
                    on_video(following.t) + SLIDE_AFTER_MS,
                );
                push([0.0, top, 1.0, bottom], from, to, true);
                push([left, 0.0, right, 1.0], from, to, true);
            }
        }
    }
    coalesce(blurs)
}

/// The same place over touching or overlapping times becomes one blur.
fn coalesce(mut blurs: Vec<Blur>) -> Vec<Blur> {
    blurs.sort_by(|a, b| {
        a.rect
            .iter()
            .zip(&b.rect)
            .map(|(x, y)| x.total_cmp(y))
            .find(|order| order.is_ne())
            .unwrap_or_else(|| a.start_ms.cmp(&b.start_ms))
    });
    let mut merged: Vec<Blur> = Vec::new();
    for blur in blurs {
        match merged.last_mut() {
            Some(last)
                if last.rect == blur.rect && last.solid == blur.solid && blur.start_ms <= last.end_ms =>
            {
                last.end_ms = last.end_ms.max(blur.end_ms);
            }
            _ => merged.push(blur),
        }
    }
    merged.sort_by_key(|b| b.start_ms);
    merged
}

// MARK: Rewriting the video

/// A pixel box (`x`, `y`, width, height; even, as 4:2:0 wants) over a span in seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Region {
    rect: [u32; 4],
    from: f64,
    to: f64,
    solid: bool,
}

fn region(blur: &Blur, (width, height): (u32, u32)) -> Option<Region> {
    let [left, top, right, bottom] = blur.rect;
    let edge = |fraction: f64, size: u32, up: bool| {
        let pixels = fraction * f64::from(size) / 2.0;
        let even = 2 * if up { pixels.ceil() } else { pixels.floor() } as u32;
        even.min(size)
    };
    let (x0, y0) = (edge(left, width, false), edge(top, height, false));
    let (x1, y1) = (edge(right, width, true), edge(bottom, height, true));
    (x1 > x0 && y1 > y0).then(|| Region {
        rect: [x0, y0, x1 - x0, y1 - y0],
        from: blur.start_ms as f64 / 1000.0,
        to: if blur.end_ms == u64::MAX { 1.0e9 } else { blur.end_ms as f64 / 1000.0 },
        solid: blur.solid,
    })
}

/// A stretch of the video between keyframes: copied as it is, or re-encoded with `regions`
/// (indices into the blurs) hidden. `end` is `None` for the last one.
#[derive(Debug, PartialEq)]
struct Segment {
    start: f64,
    end: Option<f64>,
    regions: Vec<usize>,
}

/// Cuts the video, whose keyframes are at `keyframes` (seconds, sorted), so that each region
/// lies inside a piece to re-encode: from the keyframe at or before it to the first one after
/// it, pieces that touch merging into one.
fn plan_segments(keyframes: &[f64], regions: &[Region]) -> Vec<Segment> {
    let Some(&first) = keyframes.first() else {
        return vec![Segment { start: 0.0, end: None, regions: (0..regions.len()).collect() }];
    };
    let mut spans: Vec<(f64, Option<f64>)> = regions
        .iter()
        .map(|r| {
            let start = keyframes.iter().rev().find(|&&k| k <= r.from).copied().unwrap_or(first);
            (start, keyframes.iter().find(|&&k| k > r.to).copied())
        })
        .collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, Option<f64>)> = Vec::new();
    for (start, end) in spans {
        match merged.last_mut() {
            Some((_, last_end)) if last_end.is_none_or(|e| start <= e) => {
                *last_end = last_end.zip(end).map(|(a, b)| a.max(b));
            }
            _ => merged.push((start, end)),
        }
    }
    let mut segments = Vec::new();
    let mut cursor = Some(first);
    for (start, end) in merged {
        if let Some(from) = cursor.filter(|&from| from < start) {
            segments.push(Segment { start: from, end: Some(start), regions: Vec::new() });
        }
        segments.push(Segment {
            start,
            end,
            regions: (0..regions.len())
                .filter(|&i| regions[i].from < end.unwrap_or(f64::MAX) && regions[i].to >= start)
                .collect(),
        });
        cursor = end;
    }
    if let Some(from) = cursor {
        segments.push(Segment { start: from, end: None, regions: Vec::new() });
    }
    segments
}

/// More pixelated regions than this in one piece are replaced by one box over all of them for
/// the whole piece: every one costs a copy of the frame.
const MAX_REGIONS: usize = 16;

/// The `-filter_complex` that pixelates and darkens `regions` (times relative to `origin`) of a
/// video `height` pixels tall, and fills the solid ones with black. Blocks of about 1/45 of the
/// height can't be read back, unlike a gaussian blur.
fn filter(regions: &[Region], origin: f64, height: u32) -> String {
    let (solid, mut windows): (Vec<Region>, Vec<Region>) = regions.iter().partition(|r| r.solid);
    if windows.len() > MAX_REGIONS {
        let all = windows.iter().fold(None, |all: Option<Region>, r| {
            Some(all.map_or(*r, |a| {
                let (x0, y0) = (a.rect[0].min(r.rect[0]), a.rect[1].min(r.rect[1]));
                let (x1, y1) = (
                    (a.rect[0] + a.rect[2]).max(r.rect[0] + r.rect[2]),
                    (a.rect[1] + a.rect[3]).max(r.rect[1] + r.rect[3]),
                );
                Region {
                    rect: [x0, y0, x1 - x0, y1 - y0],
                    from: a.from.min(r.from),
                    to: a.to.max(r.to),
                    solid: false,
                }
            }))
        });
        windows = all.into_iter().collect();
    }
    let when = |r: &Region| format!("enable='between(t,{:.3},{:.3})'", (r.from - origin).max(0.0), r.to - origin);
    let block = (height / 45).max(8);
    let mut graph = String::new();
    let mut last = "0:v".to_owned();
    if !windows.is_empty() {
        let sources: String = (0..windows.len()).map(|i| format!("[s{i}]")).collect();
        graph.push_str(&format!("[0:v]split={}[v0]{sources};", windows.len() + 1));
        for (i, r) in windows.iter().enumerate() {
            let [x, y, w, h] = r.rect;
            graph.push_str(&format!(
                "[s{i}]crop={w}:{h}:{x}:{y},scale={}:{}:flags=area,scale={w}:{h}:flags=neighbor,\
                 eq=brightness=-0.25[b{i}];[v{i}][b{i}]overlay={x}:{y}:{}[v{}];",
                (w / block).max(1),
                (h / block).max(1),
                when(r),
                i + 1,
            ));
        }
        last = format!("v{}", windows.len());
    }
    for (i, r) in solid.iter().enumerate() {
        let [x, y, w, h] = r.rect;
        graph.push_str(&format!("[{last}]drawbox={x}:{y}:{w}:{h}:black@1:t=fill:{}[d{i}];", when(r)));
        last = format!("d{i}");
    }
    graph.pop();
    graph.replace(&format!("[{last}]"), "[out]")
}

/// The packets of the video track: their times (seconds) and which of them are keyframes.
fn packets(video: &Path) -> Result<(usize, Vec<f64>)> {
    let mut command = Command::new("ffprobe");
    command.args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time,flags"]);
    command.args(["-of", "csv=p=0"]);
    media::input(&mut command, video);
    let out = media::run(command, "ffprobe", Duration::from_secs(60))?;
    let mut count = 0;
    let mut keyframes = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        count += 1;
        if let Some((time, flags)) = line.split_once(',')
            && flags.starts_with('K')
            && let Ok(time) = time.parse::<f64>()
        {
            keyframes.push(time);
        }
    }
    keyframes.sort_by(f64::total_cmp);
    Ok((count, keyframes))
}

/// A directory for the pieces that is removed when this is dropped. On tmpfs when there is one
/// (`$XDG_RUNTIME_DIR`), so the unblurred pieces never reach the disk.
struct Scratch(PathBuf);

impl Scratch {
    fn new(video: &Path) -> Result<Self> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
            .or_else(|| video.parent().map(Path::to_owned))
            .context("No folder for the redaction's pieces")?;
        let dir = base.join(format!("scrawl-redact-{}-{}", std::process::id(), crate::now_epoch_ms()));
        std::fs::create_dir_all(&dir).with_context(|| format!("Could not create {}", dir.display()))?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ffmpeg() -> Command {
    let mut command = Command::new("ffmpeg");
    command.args(["-v", "error", "-nostdin", "-y"]);
    command
}

/// How long ffmpeg may take on one step; hiding a window shouldn't wedge the finish.
const DEADLINE: Duration = Duration::from_secs(600);

/// Rewrites `video` with `blurs` hidden. The original is replaced only by a finished and
/// checked file.
fn blur_video(video: &Path, blurs: &[Blur]) -> Result<()> {
    let probe = media::probe(video)?;
    let size = (probe.width, probe.height);
    ensure!(size.0 > 0 && size.1 > 0, "the recording has no picture");
    let regions: Vec<Region> = blurs.iter().filter_map(|b| region(b, size)).collect();
    if regions.is_empty() {
        return Ok(());
    }
    let (packet_count, keyframes) = packets(video)?;
    let segments = plan_segments(&keyframes, &regions);
    let scratch = Scratch::new(video)?;
    let cut = |i: usize| scratch.0.join(format!("cut{i}.ts"));
    let done = |i: usize| scratch.0.join(format!("done{i}.ts"));

    // Cut at the keyframes where the pieces change; a cut is a copy.
    let times: Vec<String> =
        segments.iter().skip(1).map(|s| format!("{:.4}", s.start - 0.0005)).collect();
    let mut split = ffmpeg();
    media::input(&mut split, video);
    split
        .args(["-map", "0:v:0", "-c", "copy", "-an", "-f", "segment", "-segment_format", "mpegts"])
        .arg("-segment_times")
        .arg(if times.is_empty() { "1000000".to_owned() } else { times.join(",") })
        .arg(scratch.0.join("cut%d.ts"));
    media::run(split, "ffmpeg", DEADLINE)?;

    // Re-encode the pieces with something to hide, and list all of them in order.
    let mut list = String::new();
    for (i, segment) in segments.iter().enumerate() {
        ensure!(cut(i).exists(), "the recording could not be cut at its keyframes");
        let used = if segment.regions.is_empty() {
            cut(i)
        } else {
            let chosen: Vec<Region> = segment.regions.iter().map(|&r| regions[r]).collect();
            let mut encode = ffmpeg();
            encode
                .args(["-protocol_whitelist", "file", "-i"])
                .arg(cut(i))
                .arg("-filter_complex")
                .arg(filter(&chosen, segment.start, size.1))
                .args(["-map", "[out]", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "24"])
                // ultrafast alone writes Baseline; wf-recorder's video is High, and the joined
                // file has to say one thing about its profile.
                .args(["-x264-params", "cabac=1:8x8dct=1:bframes=2:partitions=i8x8,i4x4"])
                // Every frame at its own time, as wf-recorder wrote it.
                .args(["-fps_mode", "passthrough", "-enc_time_base", "demux", "-an", "-f", "mpegts"])
                .arg(done(i));
            media::run(encode, "ffmpeg", DEADLINE)?;
            done(i)
        };
        list.push_str(&format!("file '{}'\n", used.display().to_string().replace('\'', "'\\''")));
        if let Some(end) = segment.end {
            list.push_str(&format!("duration {:.6}\n", end - segment.start));
        }
    }
    let list_path = scratch.0.join("pieces.txt");
    std::fs::write(&list_path, list)?;

    // Join them again, with the original's audio if it had any.
    let joined = video.with_extension("redacting.mp4");
    let mut join = ffmpeg();
    join.args(["-protocol_whitelist", "file", "-f", "concat", "-safe", "0", "-i"]).arg(&list_path);
    if probe.audio_tracks > 0 {
        media::input(&mut join, video);
        join.args(["-map", "0:v", "-map", "1:a"]);
    }
    join.args(["-c", "copy", "-f", "mp4"]).arg(&joined);
    let checked = media::run(join, "ffmpeg", DEADLINE).and_then(|_| {
        let (count, _) = packets(&joined)?;
        ensure!(count == packet_count, "the rewritten recording has {count} frames, not {packet_count}");
        Ok(())
    });
    if let Err(error) = checked {
        let _ = std::fs::remove_file(&joined);
        return Err(error);
    }
    std::fs::rename(&joined, video).context("Could not replace the recording")
}

/// What `finalize_capture_bundle` needs of the redaction: the places to hide.
pub struct Redaction {
    blurs: Vec<Blur>,
}

impl Redaction {
    /// Finds what the config hides in the window log (before any title goes), then replaces the
    /// matched windows' titles in `snapshots` and `target`.
    pub fn prepare(
        snapshots: &mut [WindowSnapshot],
        target: &mut TargetInfo,
        video_offset_ms: u64,
    ) -> Self {
        let patterns = Patterns::configured();
        // Only display captures keep a window log; a window or region capture has none to go by.
        let blurs = if target.mode == CaptureMode::Display {
            plan(snapshots, &patterns, target.frame, video_offset_ms)
        } else {
            Vec::new()
        };
        scrub_titles(snapshots, target, &patterns);
        Self { blurs }
    }

    /// Hides the places in `video`, which no frame is taken from before this is done. Nothing
    /// is rewritten when no window matched. If the rewrite fails the recording is deleted
    /// rather than kept unblurred. Returns the time it took when there was something to hide.
    pub fn apply(&self, video: Option<&Path>) -> Option<u64> {
        let video = video.filter(|_| !self.blurs.is_empty())?;
        let began = Instant::now();
        match blur_video(video, &self.blurs) {
            Ok(()) => {
                let ms = began.elapsed().as_millis() as u64;
                crate::capture::scrawl_log(format!(
                    "Redaction: {} places hidden in the recording in {ms} ms",
                    self.blurs.len()
                ));
                Some(ms)
            }
            Err(error) => {
                crate::capture::scrawl_log(format!(
                    "Redaction failed, so the recording was deleted: {error:#}"
                ));
                let _ = std::fs::remove_file(video);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::WindowEntry;

    fn patterns(entries: &[&str]) -> Patterns {
        Patterns::parse(&entries.iter().map(|e| (*e).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn a_plain_pattern_is_a_case_insensitive_part_of_the_class() {
        let p = patterns(&["keepassxc", " ZapFast "]);
        assert!(p.matches("org.keepassxc.KeePassXC", "Passwords - KeePassXC"));
        assert!(p.matches("zapfast", ""));
        assert!(!p.matches("firefox", "KeePassXC docs"), "the title isn't looked at");
        assert!(!patterns(&["", "  ", "title:"]).matches("anything", "at all"));
    }

    #[test]
    fn a_glob_matches_the_whole_class_and_title_patterns_the_title() {
        let p = patterns(&["org.gnome.*", "title:*bank*", "Title:Vault ?"]);
        assert!(p.matches("ORG.GNOME.Seahorse", ""));
        assert!(!p.matches("not.org.gnome.x", ""), "a glob has to match from the start");
        assert!(p.matches("firefox", "My Bank - Login"));
        assert!(p.matches("firefox", "vault 7"));
        assert!(!p.matches("firefox", "vault 77"));
        assert!(!p.matches("bank", "news"), "a title pattern ignores the class");
    }

    #[test]
    fn the_defaults_are_the_password_managers_and_a_config_list_replaces_them() {
        let defaults = Patterns::parse(&apps_or_default(None));
        for class in ["org.keepassxc.KeePassXC", "Bitwarden", "1Password", "Proton Pass", "proton-pass"] {
            assert!(defaults.matches(class, ""), "{class}");
        }
        assert!(!defaults.matches("zapfast", "") && !defaults.matches("kitty", "bitwarden"));
        assert_eq!(apps_or_default(Some(vec!["zapfast".into()])), ["zapfast"]);
        assert!(apps_or_default(Some(Vec::new())).is_empty());
    }

    fn window(id: u32, app: &str, title: &str, bounds: [i32; 4]) -> WindowEntry {
        WindowEntry {
            id,
            app: app.into(),
            bundle_id: None,
            regular: true,
            title: title.into(),
            bounds,
            layer: 0,
            frontmost: false,
        }
    }

    fn snapshot(t: u64, windows: Vec<WindowEntry>) -> WindowSnapshot {
        WindowSnapshot { t, windows }
    }

    fn frame() -> TargetFrame {
        TargetFrame { x: 0.0, y: 0.0, width: 1000.0, height: 500.0, scale: 1.0 }
    }

    fn target(app: Option<&str>, title: Option<&str>) -> TargetInfo {
        TargetInfo {
            mode: CaptureMode::Window,
            app: app.map(Into::into),
            bundle_id: None,
            window_title: title.map(Into::into),
            window_id: None,
            display_id: 0,
            frame: frame(),
        }
    }

    #[test]
    fn matched_titles_are_replaced_and_everything_else_stays() {
        let mut log = vec![snapshot(
            0,
            vec![
                window(1, "KeePassXC", "Bank logins", [0, 0, 10, 10]),
                window(2, "kitty", "ssh server", [0, 0, 10, 10]),
                window(3, "KeePassXC", "", [0, 0, 10, 10]),
            ],
        )];
        let mut shown = target(Some("keepassxc"), Some("Bank logins"));
        scrub_titles(&mut log, &mut shown, &patterns(&["keepassxc"]));
        let titles: Vec<_> = log[0].windows.iter().map(|w| (w.app.as_str(), w.title.as_str())).collect();
        assert_eq!(titles, [("KeePassXC", REDACTED), ("kitty", "ssh server"), ("KeePassXC", "")]);
        assert_eq!(shown.window_title.as_deref(), Some(REDACTED));
        assert_eq!(shown.app.as_deref(), Some("keepassxc"));
        let mut other = target(Some("kitty"), Some("ssh server"));
        scrub_titles(&mut log, &mut other, &patterns(&["keepassxc"]));
        assert_eq!(other.window_title.as_deref(), Some("ssh server"));
        // A title pattern hides the title it matched too.
        let mut tab = vec![snapshot(0, vec![window(4, "helium", "My Bank", [0, 0, 1, 1])])];
        scrub_titles(&mut tab, &mut other, &patterns(&["title:*bank"]));
        assert_eq!(tab[0].windows[0].title, REDACTED);
    }

    fn pass() -> Patterns {
        patterns(&["pass"])
    }

    fn hidden(app: &str, id: u32, bounds: [i32; 4]) -> WindowEntry {
        window(id, app, "secret", bounds)
    }

    #[test]
    fn a_window_there_the_whole_time_is_one_blur_with_a_margin() {
        let log = [
            snapshot(25, vec![hidden("pass", 1, [100, 50, 200, 100])]),
            snapshot(1_000, vec![hidden("pass", 1, [100, 50, 200, 100]), window(2, "kitty", "", [0, 0, 50, 50])]),
        ];
        let blurs = plan(&log, &pass(), frame(), 0);
        assert_eq!(
            blurs,
            [Blur { rect: [0.092, 0.084, 0.308, 0.316], start_ms: 0, end_ms: u64::MAX, solid: false }]
        );
    }

    #[test]
    fn times_are_on_the_videos_clock_and_padded_a_sample_either_side() {
        let log = [
            snapshot(0, vec![]),
            snapshot(1_000, vec![]),
            snapshot(3_000, vec![hidden("pass", 1, [0, 0, 100, 100])]),
            snapshot(5_000, vec![]),
            snapshot(9_000, vec![]),
        ];
        let blurs = plan(&log, &pass(), frame(), 500);
        let window: Vec<_> = blurs.iter().filter(|b| b.rect[3] < 0.5 && b.rect[2] < 0.5).collect();
        // Absent at 1 s, there at 3 s, gone at 5 s: the screen was sampled in between, so the
        // window may have opened right after the first sample and closed right before the next.
        // The video starts 0.5 s into the capture and each side gets 0.3 s.
        assert_eq!(window.len(), 1);
        assert_eq!((window[0].start_ms, window[0].end_ms), (1_000 - 500 - 300, 5_000 - 500 + 300));
    }

    #[test]
    fn a_window_that_moved_is_hidden_over_its_whole_path() {
        let log = [
            snapshot(0, vec![hidden("pass", 1, [0, 0, 100, 100])]),
            snapshot(1_000, vec![hidden("pass", 1, [500, 300, 100, 100])]),
            snapshot(2_000, vec![hidden("pass", 1, [500, 300, 100, 100])]),
        ];
        let blurs = plan(&log, &pass(), frame(), 0);
        let path = blurs.iter().find(|b| b.start_ms == 0 && b.end_ms == 1_300).unwrap();
        assert_eq!(path.rect, [0.0, 0.0, 0.608, 0.816]);
        let after = blurs.iter().find(|b| b.rect[0] > 0.4).unwrap();
        assert_eq!((after.start_ms, after.end_ms), (700, u64::MAX));
    }

    #[test]
    fn a_window_that_appears_or_leaves_is_hidden_along_its_row_and_column_for_the_slide() {
        let log = [
            snapshot(0, vec![hidden("pass", 1, [400, 200, 100, 100])]),
            snapshot(2_000, vec![]),
        ];
        let blurs = plan(&log, &pass(), frame(), 0);
        let row = blurs.iter().find(|b| b.rect[0] == 0.0 && b.rect[2] == 1.0).unwrap();
        let column = blurs.iter().find(|b| b.rect[1] == 0.0 && b.rect[3] == 1.0).unwrap();
        assert!(row.solid && column.solid);
        assert_eq!((row.start_ms, row.end_ms), (1_850, 2_700));
        assert_eq!((column.start_ms, column.end_ms), (1_850, 2_700));
        assert_eq!((row.rect[1], row.rect[3]), (0.384, 0.616));
        assert_eq!((column.rect[0], column.rect[2]), (0.392, 0.508));
    }

    #[test]
    fn a_window_partly_off_the_screen_is_cut_at_its_edge_and_one_fully_off_is_ignored() {
        let log = [snapshot(
            0,
            vec![hidden("pass", 1, [-300, -100, 400, 200]), hidden("pass", 2, [2_000, 0, 100, 100])],
        )];
        let blurs = plan(&log, &pass(), frame(), 0);
        assert_eq!(blurs.len(), 1);
        assert_eq!(blurs[0].rect, [0.0, 0.0, 0.108, 0.216]);
    }

    #[test]
    fn nothing_matching_plans_nothing() {
        let log = [snapshot(0, vec![window(1, "kitty", "pass", [0, 0, 100, 100])])];
        assert!(plan(&log, &pass(), frame(), 0).is_empty());
        assert!(plan(&[], &pass(), frame(), 0).is_empty());
    }

    #[test]
    fn regions_are_even_pixels_inside_the_frame() {
        let blur = Blur { rect: [0.0015, 0.0, 0.5003, 1.0], start_ms: 500, end_ms: u64::MAX, solid: false };
        let r = region(&blur, (1920, 1080)).unwrap();
        assert_eq!(r.rect, [2, 0, 960, 1080]);
        assert_eq!((r.from, r.to), (0.5, 1.0e9));
        let off = Blur { rect: [0.5, 0.5, 0.5, 0.9], start_ms: 0, end_ms: 1, solid: false };
        assert!(region(&off, (1920, 1080)).is_none());
    }

    fn at(from: f64, to: f64) -> Region {
        Region { rect: [0, 0, 10, 10], from, to, solid: false }
    }

    #[test]
    fn pieces_are_cut_at_the_keyframes_around_each_region() {
        let keyframes = [0.0, 2.0, 4.0, 6.0, 8.0, 10.0];
        let segments = plan_segments(&keyframes, &[at(4.5, 5.0), at(8.5, 9.0)]);
        let shape: Vec<_> = segments.iter().map(|s| (s.start, s.end, s.regions.clone())).collect();
        assert_eq!(
            shape,
            [
                (0.0, Some(4.0), vec![]),
                (4.0, Some(6.0), vec![0]),
                (6.0, Some(8.0), vec![]),
                (8.0, Some(10.0), vec![1]),
                (10.0, None, vec![]),
            ]
        );
    }

    #[test]
    fn regions_in_pieces_that_touch_and_ones_that_run_on_make_one_piece() {
        let keyframes = [0.0, 2.0, 4.0, 6.0];
        let both = plan_segments(&keyframes, &[at(0.5, 1.0), at(3.0, 3.5)]);
        let shape: Vec<_> = both.iter().map(|s| (s.start, s.end, s.regions.clone())).collect();
        assert_eq!(shape, [(0.0, Some(4.0), vec![0, 1]), (4.0, None, vec![])]);
        let touching = plan_segments(&keyframes, &[at(1.0, 2.0), at(3.0, 5.0)]);
        let shape: Vec<_> = touching.iter().map(|s| (s.start, s.end, s.regions.clone())).collect();
        assert_eq!(shape, [(0.0, Some(6.0), vec![0, 1]), (6.0, None, vec![])]);
        let to_the_end = plan_segments(&keyframes, &[at(0.0, 1.0e9)]);
        assert_eq!(to_the_end, [Segment { start: 0.0, end: None, regions: vec![0] }]);
        let no_keyframes = plan_segments(&[], &[at(1.0, 2.0)]);
        assert_eq!(no_keyframes, [Segment { start: 0.0, end: None, regions: vec![0] }]);
    }

    #[test]
    fn the_filter_enables_each_region_in_its_pieces_time() {
        let region = Region { rect: [152, 82, 216, 136], from: 2.7, to: 8.3, solid: false };
        let graph = filter(&[region], 2.0, 360);
        assert!(graph.starts_with("[0:v]split=2[v0][s0];[s0]crop=216:136:152:82,scale=27:17:flags=area"), "{graph}");
        assert!(graph.contains("overlay=152:82:enable='between(t,0.700,6.300)'[out]"), "{graph}");
        let many: Vec<Region> = (0..40).map(|i| Region { rect: [i * 4, 0, 4, 4], from: f64::from(i), to: 50.0, solid: false }).collect();
        let graph = filter(&many, 0.0, 1080);
        assert!(graph.starts_with("[0:v]split=2[v0][s0];[s0]crop=160:4:0:0"), "{graph}");
        assert!(graph.contains("between(t,0.000,50.000)"), "{graph}");
        // Solid places are only a box drawn, with no copy of the frame.
        let band = Region { rect: [0, 242, 1920, 616], from: 5.0, to: 6.0, solid: true };
        let graph = filter(&[band], 4.0, 1080);
        assert_eq!(graph, "[0:v]drawbox=0:242:1920:616:black@1:t=fill:enable='between(t,1.000,2.000)'[out]");
        let both = filter(&[region, band], 2.0, 360);
        assert!(both.contains("[v1]drawbox=0:242:1920:616:black@1:t=fill:enable='between(t,3.000,4.000)'[out]"), "{both}");
    }

    // MARK: With ffmpeg

    const W: usize = 640;
    const H: usize = 360;

    /// A 12 s test pattern with a keyframe every second (and B-frames), in a fresh directory.
    fn movie(name: &str, audio: bool) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("scrawl-redact-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let movie = dir.join("recording.mp4");
        let mut command = Command::new("ffmpeg");
        command.args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30"]);
        if audio {
            command.args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=16000"]);
        }
        command
            .args(["-t", "12", "-c:v", "libx264", "-preset", "ultrafast", "-bf", "2", "-g", "30"])
            .args(["-keyint_min", "30", "-sc_threshold", "0", "-pix_fmt", "yuv420p"]);
        if audio {
            command.args(["-c:a", "aac"]);
        }
        assert!(command.arg(&movie).status().unwrap().success());
        (dir, movie)
    }

    /// The luma of the frame shown at `seconds`.
    fn gray(movie: &Path, seconds: f64) -> Vec<u8> {
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-ss", &seconds.to_string(), "-i"])
            .arg(movie)
            .args(["-frames:v", "1", "-vf", "format=gray", "-f", "rawvideo", "-"])
            .output()
            .unwrap();
        assert_eq!(out.stdout.len(), W * H, "no frame at {seconds}");
        out.stdout
    }

    fn mean_difference(a: &[u8], b: &[u8], x: std::ops::Range<usize>, y: std::ops::Range<usize>) -> f64 {
        let mut total = 0.0;
        let mut count = 0.0;
        for row in y {
            for col in x.clone() {
                total += (f64::from(a[row * W + col]) - f64::from(b[row * W + col])).abs();
                count += 1.0;
            }
        }
        total / count
    }

    /// Whether every 8x8 block of the box is flat, as pixelated blocks are.
    fn blocky(frame: &[u8], x: usize, y: usize, blocks: usize) -> bool {
        (0..blocks).all(|bx| {
            (0..blocks).all(|by| {
                let values: Vec<u8> = (0..8)
                    .flat_map(|dy| (0..8).map(move |dx| (dx, dy)))
                    .map(|(dx, dy)| frame[(y + by * 8 + dy) * W + x + bx * 8 + dx])
                    .collect();
                values.iter().max().unwrap() - values.iter().min().unwrap() <= 8
            })
        })
    }

    fn log_with_a_window() -> Vec<WindowSnapshot> {
        vec![
            snapshot(0, vec![window(2, "kitty", "", [0, 0, 100, 100])]),
            snapshot(2_000, vec![window(2, "kitty", "", [0, 0, 100, 100])]),
            snapshot(3_000, vec![hidden("pass", 1, [160, 90, 200, 120])]),
            snapshot(6_000, vec![hidden("pass", 1, [160, 90, 200, 120])]),
            snapshot(8_000, vec![window(2, "kitty", "", [0, 0, 100, 100])]),
        ]
    }

    #[test]
    fn only_the_pieces_with_a_window_in_them_are_rewritten() {
        let (dir, video) = movie("pieces", true);
        let original = dir.join("original.mp4");
        std::fs::copy(&video, &original).unwrap();
        let frame = TargetFrame { x: 0.0, y: 0.0, width: W as f64, height: H as f64, scale: 1.0 };
        let blurs = plan(&log_with_a_window(), &pass(), frame, 0);
        assert!(!blurs.is_empty());
        let redaction = Redaction { blurs };
        assert!(redaction.apply(Some(&video)).is_some());

        // Copied as they were: before the window and after it (the pieces at 0-1 s, 9-12 s).
        // Each time is half a frame after a frame's, so a rounded timestamp can't pick another.
        for t in [0.0167, 0.9167, 9.2167, 11.5167] {
            assert_eq!(gray(&video, t), gray(&original, t), "{t}");
        }
        // Where the window is the picture is unreadable blocks; the rest of the frame stays.
        let (before, after) = (gray(&original, 4.5167), gray(&video, 4.5167));
        assert!(blocky(&after, 160, 170, 4));
        assert!(mean_difference(&before, &after, 168..352, 98..202) > 30.0, "darkened");
        assert!(mean_difference(&before, &after, 400..620, 10..80) < 6.0);
        // Still hidden after the sample at 6 s, and the audio is still there.
        let later = gray(&video, 7.0167);
        assert!(blocky(&later, 160, 170, 4));
        assert!(mean_difference(&later, &gray(&original, 7.0167), 160..352, 170..202) > 30.0);
        // The window left at 8 s: for the slide its whole row is black for a moment.
        let sliding = gray(&video, 8.2167);
        assert!(sliding[150 * W + 20] < 24 && sliding[150 * W + 600] < 24);
        assert!(gray(&original, 8.2167)[150 * W + 20] > 60);
        assert_eq!(media::probe(&video).unwrap().audio_tracks, 1);
        assert_eq!(packets(&video).unwrap().0, packets(&original).unwrap().0);
        assert!(!video.with_extension("redacting.mp4").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_window_there_from_start_to_end_rewrites_the_whole_recording() {
        let (dir, video) = movie("whole", false);
        let log = [snapshot(0, vec![hidden("pass", 1, [160, 90, 200, 120])])];
        let frame = TargetFrame { x: 0.0, y: 0.0, width: W as f64, height: H as f64, scale: 1.0 };
        let redaction = Redaction { blurs: plan(&log, &pass(), frame, 0) };
        assert!(redaction.apply(Some(&video)).is_some());
        for t in [0.2, 6.0, 11.8] {
            assert!(blocky(&gray(&video, t), 160, 170, 4), "{t}");
        }
        assert_eq!(packets(&video).unwrap().0, 360);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn with_nothing_to_hide_the_recording_is_left_alone() {
        let (dir, video) = movie("none", false);
        let before = std::fs::read(&video).unwrap();
        assert_eq!(Redaction { blurs: Vec::new() }.apply(Some(&video)), None);
        assert_eq!(std::fs::read(&video).unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_recording_that_cannot_be_rewritten_is_deleted_not_kept_unblurred() {
        let dir = std::env::temp_dir().join(format!("scrawl-redact-broken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("recording.mp4");
        std::fs::write(&video, b"not a movie").unwrap();
        let blur = Blur { rect: [0.0, 0.0, 0.5, 0.5], start_ms: 0, end_ms: u64::MAX, solid: false };
        assert_eq!(Redaction { blurs: vec![blur] }.apply(Some(&video)), None);
        assert!(!video.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
