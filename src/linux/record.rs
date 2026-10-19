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
