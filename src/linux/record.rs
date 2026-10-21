//! Screen recording on Linux: `wf-recorder` (wlr-screencopy) writes `recording.mp4`, and the
//! small candidate frames for keyframe selection are decoded from it afterwards, so nothing
//! samples the screen while recording.

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::capture::{
    CANDIDATE_BUDGET_BYTES, CANDIDATE_EDGE, FRAME_SAMPLE_INTERVAL, ScreenRecording,
};
use crate::helper::FrameTimes;
use crate::model::{CaptureMode, InteractionEvent, RgbaFrame, Stroke, TargetFrame, TargetInfo};

/// How long the first frame may take before a silent screen is assumed (wf-recorder only
/// writes on damage).
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(3);
const STOP_WAIT: Duration = Duration::from_secs(15);

/// A running `wf-recorder` and the clock needed to place its video on the capture timeline.
/// Dropping it without `stop` ends the recorder too.
pub struct ScreenSession {
    child: Child,
    video: PathBuf,
    /// When the capture clock started; `duration_ms` is measured from here.
    started_at: Instant,
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

/// How long after a click or drag end the screen is sampled, so the keyframe shows its result.
const URGENT_DELAY_MS: u64 = 250;

/// Movie times to decode for the screen's reaction to each click and drag end, and for where
/// each ink stroke ended and each pin dropped: the time (capture clock) plus `URGENT_DELAY_MS`,
/// moved onto the movie clock by `video_offset_ms`, clamped to `video_duration_ms`, sorted and
/// deduplicated.
pub fn urgent_frame_times(
    events: &[InteractionEvent],
    strokes: &[Stroke],
    video_offset_ms: u64,
    video_duration_ms: u64,
) -> Vec<u64> {
    let mut times: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            InteractionEvent::Click { t, .. } => Some(*t),
            InteractionEvent::Drag { t1, .. } => Some(*t1),
            _ => None,
        })
        // Ink and pins: the screen with the mark just made. (Lasers leave nothing in a video.)
        .chain(strokes.iter().filter(|s| !s.laser).map(|s| s.t1))
        .map(|t| {
            (t + URGENT_DELAY_MS)
                .saturating_sub(video_offset_ms)
                .min(video_duration_ms)
        })
        .collect();
    times.sort_unstable();
    times.dedup();
    times
}

/// At most this many of `times` (sorted), spread evenly over them with the first and last kept.
pub fn thin_evenly(times: Vec<u64>, max: usize) -> Vec<u64> {
    if times.len() <= max {
        return times;
    }
    match max {
        0 => Vec::new(),
        1 => vec![times[0]],
        _ => (0..max)
            .map(|i| times[i * (times.len() - 1) / (max - 1)])
            .collect(),
    }
}

/// How many extra frames fit a quarter of the candidate budget.
fn urgent_frame_cap(frame_bytes: usize) -> usize {
    (CANDIDATE_BUDGET_BYTES / 4 / frame_bytes.max(1)).max(1)
}

/// Points `command` (a `wf-recorder`) at the target's output, or its area for a window or region.
pub(super) fn select_area(command: &mut Command, target: &TargetInfo) {
    match target.mode {
        CaptureMode::Window | CaptureMode::Region => {
            command.args(["-g", &geometry(target.frame)]);
        }
        CaptureMode::Display | CaptureMode::Device => {
            match crate::linux::hypr::monitor_name(target.display_id) {
                Some(name) => {
                    command.args(["-o", &name]);
                }
                None => crate::capture::scrawl_log(
                    "No monitor name for the target; wf-recorder uses its default output",
                ),
            }
        }
    }
}

fn spawn(target: &TargetInfo, video: &Path, codec: &str) -> Result<Child> {
    // A file left by an earlier attempt would pass the first-frame check.
    let _ = std::fs::remove_file(video);
    let mut command = Command::new("wf-recorder");
    command.args(["-y", "-c", codec]).arg("-f").arg(video);
    select_area(&mut command, target);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Could not start wf-recorder (install wf-recorder)")
}

/// The error for a wf-recorder that quit before writing, with its complaint.
fn quit_error(child: &mut Child, status: ExitStatus) -> anyhow::Error {
    let mut message = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut message);
    }
    anyhow::anyhow!("wf-recorder quit ({status}): {}", message.trim())
}

/// The start was cancelled (the capture was discarded while wf-recorder waited for its first
/// frame); the recorder is already stopped.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the recording was cancelled while starting")
    }
}

impl std::error::Error for Cancelled {}

/// Whether a failed NVENC start is worth retrying on x264: not once the capture is cancelled.
fn retry_on_x264(error: &anyhow::Error) -> bool {
    !error.is::<Cancelled>()
}

/// Waits for the first bytes and returns their capture-clock time in ms; `Err` with
/// wf-recorder's complaint if it quit instead. On a screen that stays silent for
/// `FIRST_FRAME_WAIT` it returns the time the recorder was started.
pub(super) fn wait_for_first_frame(
    child: &mut Child,
    video: &Path,
    started_at: Instant,
    cancel: &AtomicBool,
) -> Result<u64> {
    let spawned_ms = started_at.elapsed().as_millis() as u64;
    let deadline = Instant::now() + FIRST_FRAME_WAIT;
    while Instant::now() < deadline {
        if cancel.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Cancelled.into());
        }
        if let Some(status) = child.try_wait()? {
            return Err(quit_error(child, status));
        }
        if video.metadata().is_ok_and(|m| m.len() > 0) {
            return Ok(started_at.elapsed().as_millis() as u64);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(status) = child.try_wait()? {
        return Err(quit_error(child, status));
    }
    crate::capture::scrawl_log(
        "wf-recorder: the screen showed no frame yet; starting the video at spawn time",
    );
    Ok(spawned_ms)
}

/// Ends a recorder: SIGINT lets it finish the file; after `STOP_WAIT` it is killed.
pub(super) fn terminate(child: &mut Child) -> Result<ExitStatus> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("wf-recorder did not stop; the recording may be cut");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl ScreenSession {
    /// Starts recording `target` into `video`, on NVENC when the GPU takes it, else x264.
    /// `started_at` is the capture clock's zero.
    pub fn start(target: &TargetInfo, video: &Path, started_at: Instant) -> Result<Self> {
        Self::start_cancellable(target, video, started_at, &AtomicBool::new(false))
    }

    /// `start`, giving up (with `Cancelled`, the recorder killed and reaped) as soon as
    /// `cancel` is set while it waits for the first frame; no x264 retry after that.
    pub fn start_cancellable(
        target: &TargetInfo,
        video: &Path,
        started_at: Instant,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let mut child = spawn(target, video, "h264_nvenc")?;
        let video_offset_ms = match wait_for_first_frame(&mut child, video, started_at, cancel) {
            Ok(ms) => ms,
            Err(error) if !retry_on_x264(&error) => return Err(error),
            Err(error) => {
                crate::capture::scrawl_log(format!("NVENC unavailable, using x264: {error:#}"));
                let _ = child.kill();
                let _ = child.wait();
                if cancel.load(Ordering::Acquire) {
                    return Err(Cancelled.into());
                }
                child = spawn(target, video, "libx264")?;
                match wait_for_first_frame(&mut child, video, started_at, cancel) {
                    Ok(ms) => ms,
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error);
                    }
                }
            }
        };
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
            started_at,
            video_offset_ms,
        })
    }

    /// Stops wf-recorder and returns the recording with no candidate frames yet: they are
    /// decoded from the video by `fill_candidates` while the narration is transcribed, since
    /// decoding is the slow part (seconds for a long capture) and nothing here needs it.
    /// `duration_ms` is the whole session on the capture clock (the video may be shorter:
    /// wf-recorder writes on damage).
    pub fn stop(mut self) -> Result<ScreenRecording> {
        let status = terminate(&mut self.child)?;
        let duration_ms = self.started_at.elapsed().as_millis() as u64;
        if !status.success() && status.signal() != Some(libc::SIGINT) && status.code() != Some(130)
        {
            crate::capture::scrawl_log(format!("wf-recorder exited with {status}"));
        }
        crate::helper::probe(&self.video)
            .with_context(|| format!("The recording file {} was unusable", self.video.display()))?;
        Ok(ScreenRecording {
            frames: Vec::new(),
            video: Some(self.video.clone()),
            duration_ms,
            video_offset_ms: self.video_offset_ms,
        })
    }
}

/// The placeholder frame for a capture whose video gave no frames, as on macOS.
pub fn placeholder_frame() -> RgbaFrame {
    crate::render::render_synthetic_demo_frame(1280, 800, 0, 0)
}

/// The candidate frames of a recording `stop` left without any: those every
/// `sample_interval_ms`, plus one shortly after each click and drag in `events` and each stroke
/// end in `strokes`, on the capture clock. `frames` that are already there are returned as
/// they are, and so are they when there is no video; a video that gives no frames is logged
/// and leaves the placeholder.
pub fn fill_candidates(
    frames: Vec<RgbaFrame>,
    video: Option<&Path>,
    video_offset_ms: u64,
    events: &[InteractionEvent],
    strokes: &[Stroke],
) -> Vec<RgbaFrame> {
    let Some(video) = video.filter(|_| frames.is_empty()) else {
        return frames;
    };
    let began = Instant::now();
    match candidate_frames(video, video_offset_ms, events, strokes) {
        Ok(frames) => {
            crate::capture::scrawl_log(format!(
                "Candidate frames: {} decoded in {} ms",
                frames.len(),
                began.elapsed().as_millis()
            ));
            frames
        }
        Err(error) => {
            crate::capture::scrawl_log(format!("Screen recording: {error:#}"));
            vec![placeholder_frame()]
        }
    }
}

fn candidate_frames(
    video: &Path,
    video_offset_ms: u64,
    events: &[InteractionEvent],
    strokes: &[Stroke],
) -> Result<Vec<RgbaFrame>> {
    let unusable = || format!("The recording file {} was unusable", video.display());
    let probe = crate::helper::probe(video).with_context(unusable)?;
    let (width, height) =
        crate::linux::media::scaled_size(probe.width, probe.height, CANDIDATE_EDGE as u32);
    let frame_bytes = width as usize * height as usize * 4;
    let interval = sample_interval_ms(probe.duration_ms, frame_bytes);
    let urgent = thin_evenly(
        urgent_frame_times(events, strokes, video_offset_ms, probe.duration_ms),
        urgent_frame_cap(frame_bytes),
    );
    let edge = CANDIDATE_EDGE as u32;
    let mut frames = crate::linux::media::sampled_frames(video, interval, &urgent, edge)
        .or_else(|error| {
            // The sampled frames alone are still a keyframe selection.
            crate::capture::scrawl_log(format!("No frames after the clicks and ink: {error:#}"));
            crate::helper::frames(video, FrameTimes::Every(interval), edge)
        })
        .with_context(unusable)?;
    if frames.is_empty() {
        bail!("No frames could be decoded from {}", video.display());
    }
    for frame in &mut frames {
        frame.t_ms += video_offset_ms;
    }
    Ok(frames)
}

impl Drop for ScreenSession {
    fn drop(&mut self) {
        if let Err(error) = terminate(&mut self.child) {
            crate::capture::scrawl_log(format!("{error:#}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancelled_start_stops_the_recorder_at_once() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let cancel = AtomicBool::new(true);
        let began = Instant::now();
        let error = wait_for_first_frame(
            &mut child,
            Path::new("/nonexistent/scrawl-test.mp4"),
            Instant::now(),
            &cancel,
        )
        .unwrap_err();
        assert!(error.is::<Cancelled>(), "{error:#}");
        assert!(began.elapsed() < Duration::from_millis(500));
        // Killed and reaped.
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn only_an_uncancelled_failure_falls_back_to_x264() {
        assert!(retry_on_x264(&anyhow::anyhow!("NVENC: no device")));
        assert!(!retry_on_x264(&anyhow::Error::new(Cancelled)));
    }

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

    fn click(t: u64) -> InteractionEvent {
        InteractionEvent::Click {
            t,
            x: 0.0,
            y: 0.0,
            button: None,
            label: None,
            role: None,
        }
    }

    fn drag(t: u64, t1: u64) -> InteractionEvent {
        InteractionEvent::Drag {
            t,
            t1,
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
            label: None,
            button: None,
            path: Vec::new(),
        }
    }

    #[test]
    fn urgent_times_follow_clicks_and_drag_ends_on_the_movie_clock() {
        let events = [drag(1_000, 2_000), click(5_000), click(300)];
        // Offset 100: movie time = capture time + 250 - 100.
        assert_eq!(
            urgent_frame_times(&events, &[], 100, 60_000),
            vec![450, 2_150, 5_150]
        );
    }

    fn stroke(t0: u64, t1: u64, pin: Option<u32>) -> Stroke {
        Stroke {
            t0,
            t1,
            fw: 1920,
            fh: 1080,
            on: None,
            pin,
            laser: false,
            color: Default::default(),
            shape: None,
            pts: vec![100, 100, 0],
        }
    }

    #[test]
    fn urgent_times_include_where_each_stroke_ended_and_each_pin() {
        let strokes = [
            stroke(2_000, 3_500, None),
            stroke(5_000, 5_040, Some(1)),
            // Ink that ended where a click's frame already is.
            stroke(100, 900, None),
        ];
        let events = [click(900)];
        // Offset 100: movie time = capture time + 250 - 100.
        assert_eq!(
            urgent_frame_times(&events, &strokes, 100, 60_000),
            vec![1_050, 3_650, 5_190]
        );
        // A laser stroke leaves no ink in the video.
        let laser = Stroke {
            laser: true,
            ..stroke(1, 2, None)
        };
        assert!(urgent_frame_times(&[], &[laser], 0, 10_000).is_empty());
    }

    #[test]
    fn urgent_times_are_clamped_deduplicated_and_ignore_other_events() {
        let scroll = InteractionEvent::Scroll {
            t: 10,
            x: 0.0,
            y: 0.0,
            dx: 0.0,
            dy: 1.0,
            label: None,
        };
        let events = [click(9_900), click(9_950), scroll, click(0)];
        assert_eq!(
            urgent_frame_times(&events, &[], 500, 9_680),
            vec![0, 9_650, 9_680]
        );
        assert!(urgent_frame_times(&[], &[], 0, 10_000).is_empty());
    }

    #[test]
    fn thinning_keeps_the_ends_and_spreads_evenly() {
        let times: Vec<u64> = (0..100).collect();
        assert_eq!(thin_evenly(times.clone(), 200), times);
        let thin = thin_evenly(times.clone(), 5);
        assert_eq!(thin, vec![0, 24, 49, 74, 99]);
        assert_eq!(thin_evenly(times.clone(), 1), vec![0]);
        assert!(thin_evenly(times, 0).is_empty());
    }

    #[test]
    fn the_urgent_cap_is_a_quarter_of_the_budget() {
        let frame = 480 * 270 * 4;
        assert!(urgent_frame_cap(frame) * frame <= CANDIDATE_BUDGET_BYTES / 4);
        assert!(urgent_frame_cap(frame) > 100);
        assert_eq!(urgent_frame_cap(usize::MAX), 1);
    }

    /// A 3 s test pattern movie in a fresh temp directory (needs `ffmpeg`).
    fn test_movie(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("scrawl-record-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let movie = dir.join("t.mp4");
        let made = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=30",
            ])
            .args(["-t", "3", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&movie)
            .status()
            .unwrap();
        assert!(made.success());
        (dir, movie)
    }

    #[test]
    fn candidates_are_decoded_on_the_capture_clock_with_one_after_each_click() {
        let (dir, movie) = test_movie("fill");
        let offset = 500;
        // Movie time 1 650 + 250 = 1 900 is between two sampled frames (220 ms apart).
        let frames = fill_candidates(
            Vec::new(),
            Some(&movie),
            offset,
            &[click(1_900 + offset - 250)],
            &[],
        );
        assert!(frames.len() > 10, "{} frames", frames.len());
        assert!(frames.windows(2).all(|w| w[0].t_ms <= w[1].t_ms));
        assert_eq!(frames[0].t_ms, offset);
        assert!(
            frames.iter().any(|f| f.t_ms == 1_900 + offset),
            "no frame after the click"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn candidates_already_there_or_without_a_video_are_left_alone() {
        let kept = vec![RgbaFrame::new(7, 2, 2, vec![0; 16])];
        let frames = fill_candidates(kept, Some(Path::new("/nonexistent/t.mp4")), 0, &[], &[]);
        assert_eq!(frames.iter().map(|f| f.t_ms).collect::<Vec<_>>(), vec![7]);
        assert!(fill_candidates(Vec::new(), None, 0, &[], &[]).is_empty());
    }

    #[test]
    fn a_video_that_cannot_be_decoded_leaves_the_placeholder_frame() {
        let dir = std::env::temp_dir().join(format!("scrawl-record-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let movie = dir.join("t.mp4");
        std::fs::write(&movie, b"not a movie").unwrap();
        let frames = fill_candidates(Vec::new(), Some(&movie), 0, &[], &[]);
        assert_eq!(frames.len(), 1);
        assert_eq!((frames[0].width, frames[0].height), (1280, 800));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn geometry_is_wf_recorders_format() {
        let frame = TargetFrame {
            x: 1930.0,
            y: 40.4,
            width: 999.6,
            height: 800.0,
            scale: 1.25,
        };
        assert_eq!(geometry(frame), "1930,40 1000x800");
    }
}
