//! Screen recording on Linux: `wf-recorder` (wlr-screencopy) writes `recording.mp4`, and the
//! small candidate frames for keyframe selection are decoded from it afterwards, so nothing
//! samples the screen while recording.

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::capture::{
    CANDIDATE_BUDGET_BYTES, CANDIDATE_EDGE, FRAME_SAMPLE_INTERVAL, ScreenRecording,
};
use crate::helper::FrameTimes;
use crate::model::{CaptureMode, TargetFrame, TargetInfo};

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

fn spawn(target: &TargetInfo, video: &Path, codec: &str) -> Result<Child> {
    // A file left by an earlier attempt would pass the first-frame check.
    let _ = std::fs::remove_file(video);
    let mut command = Command::new("wf-recorder");
    command.args(["-y", "-c", codec]).arg("-f").arg(video);
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

/// Waits for the first bytes and returns their capture-clock time in ms; `Err` with
/// wf-recorder's complaint if it quit instead. On a screen that stays silent for
/// `FIRST_FRAME_WAIT` it returns the time the recorder was started.
fn wait_for_first_frame(child: &mut Child, video: &Path, started_at: Instant) -> Result<u64> {
    let spawned_ms = started_at.elapsed().as_millis() as u64;
    let deadline = Instant::now() + FIRST_FRAME_WAIT;
    while Instant::now() < deadline {
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
fn terminate(child: &mut Child) -> Result<ExitStatus> {
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
        let mut child = spawn(target, video, "h264_nvenc")?;
        let video_offset_ms = match wait_for_first_frame(&mut child, video, started_at) {
            Ok(ms) => ms,
            Err(error) => {
                crate::capture::scrawl_log(format!("NVENC unavailable, using x264: {error:#}"));
                let _ = child.kill();
                let _ = child.wait();
                child = spawn(target, video, "libx264")?;
                match wait_for_first_frame(&mut child, video, started_at) {
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

    /// Stops wf-recorder and decodes the candidate frames. `duration_ms` is the whole
    /// session on the capture clock (the video may be shorter: wf-recorder writes on damage).
    pub fn stop(mut self) -> Result<ScreenRecording> {
        let status = terminate(&mut self.child)?;
        let duration_ms = self.started_at.elapsed().as_millis() as u64;
        if !status.success() && status.signal() != Some(libc::SIGINT) && status.code() != Some(130)
        {
            crate::capture::scrawl_log(format!("wf-recorder exited with {status}"));
        }
        let unusable = || format!("The recording file {} was unusable", self.video.display());
        let probe = crate::helper::probe(&self.video).with_context(unusable)?;
        let (width, height) =
            crate::linux::media::scaled_size(probe.width, probe.height, CANDIDATE_EDGE as u32);
        let interval = sample_interval_ms(probe.duration_ms, width as usize * height as usize * 4);
        let mut frames = crate::helper::frames(
            &self.video,
            FrameTimes::Every(interval),
            CANDIDATE_EDGE as u32,
        )
        .with_context(unusable)?;
        if frames.is_empty() {
            bail!("No frames could be decoded from {}", self.video.display());
        }
        for frame in &mut frames {
            frame.t_ms += self.video_offset_ms;
        }
        Ok(ScreenRecording {
            frames,
            video: Some(self.video.clone()),
            duration_ms,
            video_offset_ms: self.video_offset_ms,
        })
    }
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
