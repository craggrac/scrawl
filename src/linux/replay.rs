//! The replay buffer: with `"replayBuffer": true` in the config, `scrawl app` keeps the last
//! seconds of the focused output and the microphone, and `scrawl replay` (the control socket's
//! `replay`, the tray's "Save Last 30 s") saves them as a normal bundle.
//!
//! The screen is one `wf-recorder` writing 2 s MPEG-TS segments (its `segment` muxer) into
//! `$XDG_RUNTIME_DIR/scrawl/replay/`, which is memory (tmpfs): the SSD sees none of an
//! always-on recording, and the files that are older than the window plus a margin are deleted
//! as it goes. The microphone is the usual `AudioRecorder`, its samples capped to the window.
//! Saving stops the recorder (which flushes the open segment), copies the segments from the
//! one the window starts in into an mp4 without re-encoding (`media::clip_segments`), cuts
//! the audio to the same span, and runs the normal finish pipeline. The buffer then starts over.
//!
//! The buffer is off while a recording runs (two full-screen encodes and two microphone
//! streams for the same minutes, and a second NVENC session on a card that has been out of
//! memory) and while the session is locked; both throw away what it held, so a clip is
//! always one uninterrupted stretch and nothing from before a lock comes out after it.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};

use super::{control, hypr, media, record, toggle};
use crate::audio::{self, AudioRecorder, RecordedAudio};
use crate::capture::{ScreenRecording, scrawl_log};
use crate::handoff::{self, Config};
use crate::model::TargetInfo;

/// What the window is when `replaySeconds` is absent, and its bounds.
const DEFAULT_SECONDS: u64 = 30;
const MIN_SECONDS: u64 = 5;
const MAX_SECONDS: u64 = 60;
/// Kept beyond the window, so a save a moment late still has all of it.
const MARGIN: Duration = Duration::from_secs(6);
/// The most the segments may take, whatever the recorder's bitrate does.
const MAX_BYTES: u64 = 256 << 20;
/// How often the supervisor looks at the config, the lock and the files.
const TICK: Duration = Duration::from_secs(1);
/// Before a buffer that failed to start tries again.
const RETRY: Duration = Duration::from_secs(30);
const SAMPLE_RATE: usize = 16_000;
/// Where the segments go, `%06d` being the index the recorder counts up.
const SEGMENT_PATTERN: &str = "seg-%06d.ts";

/// How `wf-recorder` encodes the buffer: cheap and bounded, with a key frame every 60 frames
/// (a segment ends at the first one after 2 s) and no B-frames (`clip_segments` relies on it).
struct Encoder {
    codec: &'static str,
    params: &'static [&'static str],
}

const NVENC: Encoder = Encoder {
    codec: "h264_nvenc",
    params: &[
        "preset=p1",
        "tune=ll",
        "b=3000000",
        "maxrate=6000000",
        "bufsize=6000000",
        "g=60",
    ],
};

const X264: Encoder = Encoder {
    codec: "libx264",
    params: &[
        "preset=ultrafast",
        "tune=zerolatency",
        "crf=28",
        "maxrate=6000000",
        "bufsize=6000000",
        "g=60",
    ],
};

/// The window in seconds when the buffer is on: `replayBuffer` with `replaySeconds` (default 30)
/// held between 5 and 60.
fn window_from(config: &Config) -> Option<u64> {
    config.replay_buffer.unwrap_or(false).then(|| {
        config
            .replay_seconds
            .unwrap_or(DEFAULT_SECONDS)
            .clamp(MIN_SECONDS, MAX_SECONDS)
    })
}

/// The window in seconds if the buffer is turned on.
pub fn configured() -> Option<u64> {
    window_from(&handoff::load_config_from(&handoff::default_config_path()))
}

// MARK: Segments and what to keep

/// One segment file of the ring.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Segment {
    index: u64,
    path: PathBuf,
    /// When it was last written: for a closed segment, the moment the next one began.
    modified: SystemTime,
    bytes: u64,
}

/// The index in `seg-000012.ts`.
fn segment_index(name: &str) -> Option<u64> {
    name.strip_prefix("seg-")?
        .strip_suffix(".ts")?
        .parse()
        .ok()
}

/// The segments in `dir`, oldest first. The `seg-%06d.ts` file `wf-recorder` makes by opening
/// its output name as it is is not one.
fn list_segments(dir: &Path) -> Vec<Segment> {
    let mut segments: Vec<Segment> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let index = segment_index(entry.file_name().to_str()?)?;
            let meta = entry.metadata().ok()?;
            Some(Segment {
                index,
                path: entry.path(),
                modified: meta.modified().ok()?,
                bytes: meta.len(),
            })
        })
        .collect();
    segments.sort_by_key(|s| s.index);
    segments
}

/// How many of the oldest `segments` may go: a closed one whose end is more than `keep` ago
/// (the open one, the newest, never), then more while the rest take more than `max_bytes`.
fn expired(segments: &[Segment], now: SystemTime, keep: Duration, max_bytes: u64) -> usize {
    let closed = segments.len().saturating_sub(1);
    let mut count = segments[..closed]
        .iter()
        .take_while(|s| {
            now.duration_since(s.modified)
                .is_ok_and(|age| age > keep)
        })
        .count();
    let mut bytes: u64 = segments[count..].iter().map(|s| s.bytes).sum();
    while count < closed && bytes > max_bytes {
        bytes -= segments[count].bytes;
        count += 1;
    }
    count
}

/// Where the window starts: `want` before `saved_at`, but not before the buffer began.
fn window_start(origin: SystemTime, saved_at: SystemTime, want: Duration) -> SystemTime {
    saved_at.checked_sub(want).map_or(origin, |t| t.max(origin))
}

/// The segment the window starts in (the last that began at or before it) and how far into it.
/// A segment begins when the one before it was closed; the first, at `origin`.
fn cut_point(
    origin: SystemTime,
    segments: &[Segment],
    window_start: SystemTime,
) -> Option<(usize, Duration)> {
    let began = |i: usize| if i == 0 { origin } else { segments[i - 1].modified };
    let first = (0..segments.len())
        .rev()
        .find(|&i| began(i) <= window_start)?;
    Some((first, window_start.duration_since(began(first)).ok()?))
}

/// `samples` as exactly `len` of them: the oldest dropped, or silence put in front when the
/// microphone has been heard for less than the window.
fn fit_audio(mut samples: Vec<f32>, len: usize) -> Vec<f32> {
    if samples.len() > len {
        samples.drain(..samples.len() - len);
    } else if samples.len() < len {
        let missing = len - samples.len();
        samples.splice(0..0, std::iter::repeat_n(0.0, missing));
    }
    samples
}

/// Copies `segments[first..]` into `joined`, end to end, and the mp4 `out` is made from that.
fn cut_video(segments: &[Segment], skip: Duration, joined: &Path, out: &Path) -> Result<()> {
    let mut file = std::fs::File::create(joined)
        .with_context(|| format!("Could not write {}", joined.display()))?;
    for segment in segments {
        let mut part = std::fs::File::open(&segment.path)
            .with_context(|| format!("Could not read {}", segment.path.display()))?;
        std::io::copy(&mut part, &mut file)?;
    }
    drop(file);
    media::clip_segments(joined, skip, out)
}

// MARK: The running buffer

/// A `wf-recorder` writing segments, the microphone, and what the ring has dropped so far.
struct Run {
    child: Child,
    dir: PathBuf,
    target: TargetInfo,
    /// When the oldest segment on disk began.
    origin: SystemTime,
    audio: Option<AudioRecorder>,
    /// When a microphone that isn't there may be tried again.
    audio_retry: Instant,
    /// The end of what `wf-recorder` said, for the log if it quits.
    said: Arc<Mutex<String>>,
}

/// Counts a buffer's recorder as running, for `running`.
static RECORDER_UP: AtomicBool = AtomicBool::new(false);

/// Whether a buffer's `wf-recorder` may still be running: quitting waits for it, as for a
/// recording's.
pub fn running() -> bool {
    RECORDER_UP.load(Ordering::Acquire)
}

/// A fresh, owner-only ring directory.
fn ring_dir() -> Result<PathBuf> {
    let runtime = toggle::runtime_dir().context("No per-user runtime or state directory")?;
    toggle::create_runtime_dir(&runtime)?;
    let dir = runtime.join("replay");
    let _ = std::fs::remove_dir_all(&dir);
    toggle::create_runtime_dir(&dir)?;
    Ok(dir)
}

/// Starts `wf-recorder` on `target` with `encoder`, which ends with the app: `PR_SET_PDEATHSIG`
/// stops it even when scrawl is killed (as its thread, which this is called on for as long as
/// the recorder runs, ends).
fn spawn(target: &TargetInfo, dir: &Path, encoder: &Encoder) -> Result<Child> {
    let mut command = Command::new("wf-recorder");
    command.args(["-y", "-m", "segment", "-c", encoder.codec, "-b", "0"]);
    for param in encoder.params {
        command.args(["-p", param]);
    }
    command.arg("-f").arg(dir.join(SEGMENT_PATTERN));
    record::select_area(&mut command, target);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT);
            Ok(())
        });
    }
    command
        .spawn()
        .context("Could not start wf-recorder (install wf-recorder)")
}

impl Run {
    /// Starts the ring on the focused output, on NVENC when the GPU takes it, else x264.
    fn start() -> Result<Self> {
        let target = hypr::focused_monitor_target(None);
        let dir = ring_dir()?;
        let first = dir.join("seg-000000.ts");
        let never = AtomicBool::new(false);
        let mut started_with = None;
        let mut failure = None;
        for encoder in [&NVENC, &X264] {
            let started = Instant::now();
            let mut attempt = spawn(&target, &dir, encoder)?;
            match record::wait_for_first_frame(&mut attempt, &first, started, &never) {
                Ok(_) => {
                    started_with = Some((attempt, encoder.codec));
                    break;
                }
                Err(error) => {
                    scrawl_log(format!("Replay buffer: {} failed: {error:#}", encoder.codec));
                    let _ = attempt.kill();
                    let _ = attempt.wait();
                    failure = Some(error);
                }
            }
        }
        let Some((mut child, codec)) = started_with else {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(failure.unwrap_or_else(|| anyhow::anyhow!("no encoder")));
        };
        RECORDER_UP.store(true, Ordering::Release);
        // Read to the end, or wf-recorder would die of SIGPIPE on its next message; the last
        // of it is what the log gets if it quits.
        let said = Arc::new(Mutex::new(String::new()));
        if let Some(mut stderr) = child.stderr.take() {
            let said = Arc::clone(&said);
            std::thread::spawn(move || {
                let mut chunk = [0_u8; 1024];
                while let Ok(n @ 1..) = stderr.read(&mut chunk) {
                    let mut said = said.lock().unwrap_or_else(PoisonError::into_inner);
                    said.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    let excess = said.len().saturating_sub(2_000);
                    if excess > 0 {
                        let cut = (excess..).find(|&i| said.is_char_boundary(i)).unwrap_or(0);
                        said.drain(..cut);
                    }
                }
            });
        }
        scrawl_log(format!("Replay buffer: recording with {codec}"));
        Ok(Self {
            child,
            dir,
            target,
            origin: SystemTime::now(),
            audio: AudioRecorder::start().ok(),
            audio_retry: Instant::now() + RETRY,
            said,
        })
    }

    /// Looks after the ring: drops what is out of the window, keeps the microphone going and
    /// its samples to the window. `Err` when the recorder quit.
    fn tend(&mut self, seconds: u64) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            let said = self.said.lock().unwrap_or_else(PoisonError::into_inner);
            bail!("wf-recorder quit ({status}): {}", said.trim());
        }
        let keep = Duration::from_secs(seconds) + MARGIN;
        let segments = list_segments(&self.dir);
        let gone = if segments.is_empty() {
            0
        } else {
            expired(&segments, SystemTime::now(), keep, MAX_BYTES)
        };
        if gone > 0 {
            self.origin = segments[gone - 1].modified;
            for segment in &segments[..gone] {
                let _ = std::fs::remove_file(&segment.path);
            }
        }
        if self.audio.as_ref().is_some_and(AudioRecorder::failed) {
            self.audio = None;
        }
        if self.audio.is_none() && Instant::now() >= self.audio_retry {
            self.audio = AudioRecorder::start().ok();
            self.audio_retry = Instant::now() + RETRY;
        }
        if let Some(audio) = &self.audio {
            let keep = keep.as_secs() as usize * SAMPLE_RATE;
            audio.keep_last(keep, 5 * SAMPLE_RATE);
        }
        Ok(())
    }

    /// Ends the buffer and cuts the last `seconds` of it into `bundle_dir`.
    fn save(mut self, bundle_dir: &Path, seconds: u64) -> Result<Saved> {
        let saved_at = SystemTime::now();
        let start = window_start(self.origin, saved_at, Duration::from_secs(seconds));
        let window = saved_at.duration_since(start).unwrap_or_default();
        // Before the recorder stops, so the microphone's end is the moment of the save.
        let heard = self
            .audio
            .as_ref()
            .map(|audio| audio.tail(window.as_secs_f64().ceil() as usize * SAMPLE_RATE));
        let began = Instant::now();
        record::terminate(&mut self.child)?;
        let segments = list_segments(&self.dir);
        let (first, skip) = cut_point(self.origin, &segments, start)
            .filter(|_| window >= Duration::from_secs(1))
            .context("The replay buffer holds nothing yet")?;
        let video = bundle_dir.join("recording.mp4");
        cut_video(&segments[first..], skip, &self.dir.join("joined.ts"), &video)?;
        crate::helper::probe(&video)
            .with_context(|| format!("The replay clip {} was unusable", video.display()))?;
        let samples = (window.as_secs_f64() * SAMPLE_RATE as f64).round() as usize;
        let audio = heard
            .map(|heard| fit_audio(heard, samples))
            .filter(|heard| heard.iter().any(|&s| s != 0.0))
            .map(|heard| audio::encode_wav_16k(&heard).and_then(audio::recorded_audio_from_wav))
            .transpose()?;
        if audio.is_none() {
            scrawl_log("Replay buffer: no sound in the window (no microphone, or muted)");
        }
        scrawl_log(format!(
            "Replay buffer: saved {:.1} s ({} segments from #{}) in {} ms",
            window.as_secs_f64(),
            segments.len() - first,
            segments[first].index,
            began.elapsed().as_millis()
        ));
        Ok(Saved {
            target: self.target.clone(),
            recording: ScreenRecording {
                frames: Vec::new(),
                video: Some(video),
                duration_ms: window.as_millis() as u64,
                video_offset_ms: 0,
            },
            audio,
        })
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Err(error) = record::terminate(&mut self.child) {
            scrawl_log(format!("Replay buffer: {error:#}"));
        }
        RECORDER_UP.store(false, Ordering::Release);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What a save hands to the finish pipeline.
pub struct Saved {
    pub target: TargetInfo,
    pub recording: ScreenRecording,
    pub audio: Option<RecordedAudio>,
}

// MARK: The supervisor

/// Why the buffer is not running although it is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pause {
    Recording,
    Locked,
}

/// What the buffer should be doing: nothing, waiting, or buffering `n` seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    Off,
    Paused(Pause),
    On(u64),
}

/// `seconds` is the configured window, if on. `locked` is only asked when it matters.
fn want(seconds: Option<u64>, recording: bool, locked: impl FnOnce() -> bool) -> Want {
    match seconds {
        None => Want::Off,
        Some(_) if recording => Want::Paused(Pause::Recording),
        Some(_) if locked() => Want::Paused(Pause::Locked),
        Some(seconds) => Want::On(seconds),
    }
}

/// Where the buffer is, for answering a `replay` that can't be served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Off,
    Paused(Pause),
    Failed,
    Buffering,
}

impl Status {
    /// The reason a save can't happen, if it can't.
    fn refusal(self) -> Option<&'static str> {
        match self {
            Self::Buffering => None,
            Self::Off => Some("the replay buffer is off (Settings, Recording)"),
            Self::Paused(Pause::Recording) => {
                Some("a recording is running; the replay buffer starts again after it")
            }
            Self::Paused(Pause::Locked) => Some("the screen is locked, so the replay buffer is paused"),
            Self::Failed => Some("the replay buffer could not start (see the Scrawl log)"),
        }
    }
}

static STATUS: Mutex<Status> = Mutex::new(Status::Off);
static RECORDING: AtomicBool = AtomicBool::new(false);
static MESSAGES: OnceLock<Sender<Message>> = OnceLock::new();

enum Message {
    /// A recording started or ended: look at it now rather than at the next tick.
    Look,
    Save {
        bundle_dir: PathBuf,
        reply: Sender<Result<Saved>>,
    },
    Shutdown,
}

fn set_status(status: Status) {
    *STATUS.lock().unwrap_or_else(PoisonError::into_inner) = status;
}

/// Starts the supervisor thread (once). It owns the buffer, and with the setting off it does
/// nothing but look at the config every second.
pub fn start() {
    let (sender, receiver) = channel();
    if MESSAGES.set(sender).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("scrawl-replay".into())
        .spawn(move || supervise(&receiver));
    if let Err(error) = spawned {
        scrawl_log(format!("Replay buffer: could not start its thread: {error}"));
    }
}

/// A recording of the user's started or ended; the buffer is off while one runs.
pub fn set_recording(recording: bool) {
    if RECORDING.swap(recording, Ordering::AcqRel) != recording
        && let Some(sender) = MESSAGES.get()
    {
        let _ = sender.send(Message::Look);
    }
}

/// Ends the buffer, leaving no recorder behind; `running` says when it has.
pub fn shutdown() {
    if let Some(sender) = MESSAGES.get() {
        let _ = sender.send(Message::Shutdown);
    }
}

fn supervise(messages: &Receiver<Message>) {
    let mut run: Option<Run> = None;
    let mut seconds = DEFAULT_SECONDS;
    let mut retry_at = Instant::now();
    let mut told = false;
    loop {
        match messages.recv_timeout(TICK) {
            Ok(Message::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Message::Save { bundle_dir, reply }) => {
                let saved = match run.take() {
                    Some(run) => run.save(&bundle_dir, seconds),
                    None => Err(anyhow::anyhow!("The replay buffer is not recording")),
                };
                let _ = reply.send(saved);
                // Starts over from nothing at the next look.
                set_status(Status::Off);
                continue;
            }
            Ok(Message::Look) | Err(RecvTimeoutError::Timeout) => {}
        }
        let wanted = want(configured(), RECORDING.load(Ordering::Acquire), || {
            hypr::session_locked()
        });
        match wanted {
            Want::Off | Want::Paused(_) => {
                // What the buffer held goes with it.
                run = None;
                set_status(match wanted {
                    Want::Paused(pause) => Status::Paused(pause),
                    _ => Status::Off,
                });
                told = false;
            }
            Want::On(window) => {
                seconds = window;
                if run.is_none() && Instant::now() >= retry_at {
                    match Run::start() {
                        Ok(started) => {
                            run = Some(started);
                            told = false;
                        }
                        Err(error) => {
                            scrawl_log(format!("Replay buffer: could not start: {error:#}"));
                            if !told {
                                super::notify(&format!("The replay buffer could not start: {error:#}"));
                                told = true;
                            }
                            retry_at = Instant::now() + RETRY;
                        }
                    }
                }
                if let Some(running) = run.as_mut()
                    && let Err(error) = running.tend(seconds)
                {
                    scrawl_log(format!("Replay buffer: {error:#}"));
                    run = None;
                    retry_at = Instant::now() + RETRY;
                }
                set_status(if run.is_some() {
                    Status::Buffering
                } else {
                    Status::Failed
                });
            }
        }
    }
}

// MARK: Saving

/// Saves the last seconds as a bundle in the background and says so when it is on the
/// clipboard. `Err` at once when there is nothing to save (off, paused, not started).
pub fn save_in_background() -> Result<(), String> {
    let status = *STATUS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(reason) = status.refusal() {
        return Err(reason.to_owned());
    }
    std::thread::Builder::new()
        .name("scrawl-replay-save".into())
        .spawn(|| match save_bundle() {
            Ok(dir) => {
                let copied = handoff::copy_bundle_to_clipboard(&dir);
                if let Err(error) = &copied {
                    scrawl_log(format!("Clipboard: {error:#}"));
                }
                super::notify(&if copied.is_ok() {
                    format!("Replay saved, summary copied\n{}", dir.display())
                } else {
                    format!("Replay saved\n{}", dir.display())
                });
            }
            Err(error) => {
                scrawl_log(format!("Replay save failed: {error:#}"));
                super::notify(&format!("Saving the replay failed: {error:#}"));
            }
        })
        .map(drop)
        .map_err(|error| format!("could not start the save: {error}"))
}

/// Cuts the buffer into a new bundle and builds it.
fn save_bundle() -> Result<PathBuf> {
    let sender = MESSAGES.get().context("The replay buffer is not running")?;
    let bundle_dir =
        handoff::default_captures_dir().join(format!("{}.scrawl", crate::make_bundle_id()));
    std::fs::create_dir_all(&bundle_dir)?;
    let (reply, saved) = channel();
    let built = (|| {
        sender
            .send(Message::Save {
                bundle_dir: bundle_dir.clone(),
                reply,
            })
            .context("The replay buffer is not running")?;
        let saved = saved.recv().context("The replay buffer went away")??;
        let seconds = saved.recording.duration_ms / 1_000;
        let (dir, _bundle, _summary) = crate::finalize_capture_bundle(
            &bundle_dir,
            saved.target,
            saved.recording,
            Vec::new(),
            Vec::new(),
            saved.audio,
            Some(format!(
                "Saved from the replay buffer: the {seconds} seconds before the shortcut. \
                 Nothing was inked or pinned, and no clicks or keys were logged."
            )),
            crate::windows::ActivityLog::default(),
        )?;
        Ok(dir)
    })();
    if built.is_err() {
        // Nothing was kept of it.
        let _ = std::fs::remove_dir_all(&bundle_dir);
    }
    built
}

/// `scrawl replay`: asks the running app to save the buffer.
pub fn command() -> Result<()> {
    if control::send(control::Command::Replay)? {
        return Ok(());
    }
    bail!("Scrawl isn't running: the replay buffer lives in `scrawl app`")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + secs)
    }

    fn seg(index: u64, closed_at: u64, bytes: u64) -> Segment {
        Segment {
            index,
            path: PathBuf::from(format!("seg-{index:06}.ts")),
            modified: at(closed_at),
            bytes,
        }
    }

    #[test]
    fn the_window_is_off_by_default_and_held_to_its_bounds() {
        let with = |on: Option<bool>, seconds: Option<u64>| {
            window_from(&Config {
                replay_buffer: on,
                replay_seconds: seconds,
                ..Config::default()
            })
        };
        assert_eq!(with(None, Some(30)), None);
        assert_eq!(with(Some(false), None), None);
        assert_eq!(with(Some(true), None), Some(30));
        assert_eq!(with(Some(true), Some(45)), Some(45));
        assert_eq!(with(Some(true), Some(600)), Some(60));
        assert_eq!(with(Some(true), Some(0)), Some(5));
    }

    #[test]
    fn config_keys_are_read_from_json() {
        let config: Config =
            serde_json::from_str(r#"{"replayBuffer": true, "replaySeconds": 20}"#).unwrap();
        assert_eq!(window_from(&config), Some(20));
    }

    #[test]
    fn segment_files_are_told_from_other_files() {
        assert_eq!(segment_index("seg-000012.ts"), Some(12));
        assert_eq!(segment_index("seg-%06d.ts"), None);
        assert_eq!(segment_index("joined.ts"), None);
        assert_eq!(segment_index("seg-000012.ts.tmp"), None);
    }

    #[test]
    fn segments_are_listed_in_order_and_the_stray_output_is_skipped() {
        let dir = std::env::temp_dir().join(format!("scrawl-replay-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["seg-000010.ts", "seg-000002.ts", "seg-%06d.ts", "joined.ts"] {
            std::fs::write(dir.join(name), b"abc").unwrap();
        }
        let listed = list_segments(&dir);
        assert_eq!(listed.iter().map(|s| s.index).collect::<Vec<_>>(), vec![2, 10]);
        assert_eq!(listed[0].bytes, 3);
        assert!(list_segments(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn closed_segments_older_than_the_window_expire_and_the_open_one_never() {
        // Window plus margin: 36 s. Each segment closed when the next began.
        let keep = Duration::from_secs(36);
        let segments = [
            seg(0, 100, 10),
            seg(1, 102, 10),
            seg(2, 130, 10),
            seg(3, 132, 10),
            seg(4, 134, 10),
        ];
        // At 140 only the first two closed more than 36 s ago.
        assert_eq!(expired(&segments, at(140), keep, u64::MAX), 2);
        // At 10 000 everything is old, but the newest is still being written.
        assert_eq!(expired(&segments, at(10_000), keep, u64::MAX), 4);
        assert_eq!(expired(&segments[4..], at(10_000), keep, u64::MAX), 0);
        assert_eq!(expired(&segments, at(120), keep, u64::MAX), 0);
        // A clock that went back keeps them.
        assert_eq!(expired(&segments, at(50), keep, u64::MAX), 0);
    }

    #[test]
    fn a_ring_over_its_byte_cap_loses_its_oldest_but_keeps_the_open_segment() {
        let segments = [seg(0, 100, 60), seg(1, 102, 60), seg(2, 104, 60)];
        let keep = Duration::from_secs(1_000);
        assert_eq!(expired(&segments, at(110), keep, 200), 0);
        assert_eq!(expired(&segments, at(110), keep, 130), 1);
        assert_eq!(expired(&segments, at(110), keep, 10), 2);
    }

    #[test]
    fn the_window_starts_at_the_buffer_if_that_is_later() {
        let want = Duration::from_secs(30);
        assert_eq!(window_start(at(0), at(100), want), at(70));
        assert_eq!(window_start(at(90), at(100), want), at(90));
        // A save before the epoch plus the window can't go negative.
        assert_eq!(
            window_start(SystemTime::UNIX_EPOCH, SystemTime::UNIX_EPOCH, want),
            SystemTime::UNIX_EPOCH
        );
    }

    #[test]
    fn the_cut_is_in_the_segment_the_window_starts_in() {
        // Began at 100 (origin), 102, 104, 140: the third ran for 36 s on a still screen.
        let segments = [seg(0, 102, 1), seg(1, 104, 1), seg(2, 140, 1), seg(3, 142, 1)];
        let origin = at(100);
        assert_eq!(cut_point(origin, &segments, at(100)), Some((0, Duration::ZERO)));
        assert_eq!(
            cut_point(origin, &segments, at(103)),
            Some((1, Duration::from_secs(1)))
        );
        // In the long, still one: its first picture is what the screen showed.
        assert_eq!(
            cut_point(origin, &segments, at(120)),
            Some((2, Duration::from_secs(16)))
        );
        assert_eq!(
            cut_point(origin, &segments, at(141)),
            Some((3, Duration::from_secs(1)))
        );
        // Before anything was recorded.
        assert_eq!(cut_point(origin, &segments, at(50)), None);
        assert_eq!(cut_point(origin, &[], at(120)), None);
    }

    #[test]
    fn after_pruning_the_first_segment_began_when_the_dropped_one_closed() {
        // Segment 7 is the oldest left; the origin is when 6 closed.
        let segments = [seg(7, 106, 1), seg(8, 108, 1)];
        assert_eq!(
            cut_point(at(104), &segments, at(105)),
            Some((0, Duration::from_secs(1)))
        );
    }

    #[test]
    fn audio_is_fitted_to_the_window_from_its_end() {
        assert_eq!(fit_audio(vec![1.0, 2.0, 3.0, 4.0], 2), vec![3.0, 4.0]);
        // Heard for less than the window: the missing start is silence.
        assert_eq!(fit_audio(vec![1.0, 2.0], 4), vec![0.0, 0.0, 1.0, 2.0]);
        assert_eq!(fit_audio(vec![1.0], 1), vec![1.0]);
        assert!(fit_audio(Vec::new(), 0).is_empty());
    }

    #[test]
    fn the_buffer_pauses_for_a_recording_and_the_lock_and_asks_the_lock_last() {
        let never = || -> bool { panic!("the lock was asked") };
        assert_eq!(want(None, true, never), Want::Off);
        assert_eq!(want(Some(30), true, never), Want::Paused(Pause::Recording));
        assert_eq!(want(Some(30), false, || true), Want::Paused(Pause::Locked));
        assert_eq!(want(Some(30), false, || false), Want::On(30));
    }

    #[test]
    fn only_a_buffering_buffer_can_be_saved() {
        assert_eq!(Status::Buffering.refusal(), None);
        for status in [
            Status::Off,
            Status::Failed,
            Status::Paused(Pause::Recording),
            Status::Paused(Pause::Locked),
        ] {
            assert!(status.refusal().is_some(), "{status:?}");
        }
    }

    /// Writes `count` 2 s segments of a test pattern the way `wf-recorder`'s `segment` muxer
    /// does (key frame every 60 frames, no B-frames, MPEG-TS), and the movie they came from.
    /// With `still`, nothing changes for the first `still` seconds, so the first segment holds
    /// one picture for that long, as the recorder writes on a still screen.
    fn test_segments(name: &str, seconds: u32, still: u32) -> (PathBuf, Vec<Segment>) {
        let dir = std::env::temp_dir().join(format!("scrawl-replay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let select = format!("select='eq(n,0)+gte(t,{still})'");
        let made = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size=320x240:rate=30:duration={seconds}"))
            .args(["-vf", &select, "-fps_mode", "vfr"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-bf", "0", "-g", "60"])
            .args(["-f", "segment", "-segment_time", "2", "-segment_format", "mpegts"])
            .arg(dir.join(SEGMENT_PATTERN))
            .status()
            .unwrap();
        assert!(made.success());
        (dir.clone(), list_segments(&dir))
    }

    /// First and last frame times (s) of a movie, as ffprobe reads them.
    fn frame_span(movie: &Path) -> (f64, f64) {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v", "-show_entries"])
            .args(["frame=pts_time", "-of", "csv=p=0"])
            .arg(movie)
            .output()
            .unwrap();
        let times: Vec<f64> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split(',').next()?.trim().parse().ok())
            .collect();
        (times[0], *times.last().unwrap())
    }

    #[test]
    fn segments_become_a_clip_that_starts_where_the_window_does() {
        let (dir, segments) = test_segments("clip", 12, 0);
        assert!(segments.len() >= 5, "{} segments", segments.len());
        let out = dir.join("clip.mp4");
        let joined = dir.join("joined.ts");
        // 3 s into the segment run (a segment boundary falls at 2 and 4 s).
        cut_video(&segments, Duration::from_secs(3), &joined, &out).unwrap();
        let (first, last) = frame_span(&out);
        assert!(first < 0.01, "first frame at {first}");
        let probed = crate::helper::probe(&out).unwrap();
        // 12 s of video minus the 3 s skipped, to the frame.
        assert!(
            (probed.duration_ms as i64 - 9_000).abs() <= 100,
            "{} ms",
            probed.duration_ms
        );
        assert!((last - 8.97).abs() < 0.1, "last frame at {last}");
        // No cut at all keeps everything.
        cut_video(&segments, Duration::ZERO, &joined, &out).unwrap();
        assert!((crate::helper::probe(&out).unwrap().duration_ms as i64 - 12_000).abs() <= 100);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_window_inside_a_still_stretch_opens_on_the_picture_that_was_up() {
        // One picture for 6 s, then 6 s of change: the first segment spans both.
        let (dir, segments) = test_segments("still", 12, 6);
        let out = dir.join("clip.mp4");
        let joined = dir.join("joined.ts");
        cut_video(&segments, Duration::from_secs(3), &joined, &out).unwrap();
        let (first, last) = frame_span(&out);
        // The held picture opens the movie; the change comes 3 s in, not at once.
        assert!(first < 0.01, "first frame at {first}");
        let probed = crate::helper::probe(&out).unwrap();
        assert!((probed.duration_ms as i64 - 9_000).abs() <= 150, "{}", probed.duration_ms);
        assert!(last > 8.5, "last frame at {last}");
        let frames = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v", "-show_entries"])
            .args(["frame=pts_time", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        let times: Vec<f64> = String::from_utf8_lossy(&frames.stdout)
            .lines()
            .filter_map(|l| l.split(',').next()?.trim().parse().ok())
            .collect();
        assert!(times[1] > 2.9 && times[1] < 3.1, "second picture at {}", times[1]);
        // The window starts after the change: the pictures before it are squeezed to the start.
        cut_video(&segments, Duration::from_secs(8), &joined, &out).unwrap();
        let probed = crate::helper::probe(&out).unwrap();
        assert!((probed.duration_ms as i64 - 4_000).abs() <= 150, "{}", probed.duration_ms);
        // And the movie decodes end to end.
        let decoded = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args(["-f", "null", "-"])
            .status()
            .unwrap();
        assert!(decoded.success());
        let _ = std::fs::remove_dir_all(dir);
    }
}
