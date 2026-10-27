//! Microphone capture through PipeWire itself, not cpal's ALSA host. cpal went through
//! pipewire-alsa's `default` PCM, and opening it intermittently failed with "device not
//! available" (a recording with no audio at all). `pw-record` talks to PipeWire natively, so
//! PipeWire also does what scrawl once did by hand: downmix and resample (with a proper
//! filter) to the 16 kHz mono f32 that goes straight into the recorder's buffer.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::mic::{Input, Inputs};

/// Startup tries, and the pause between them: a node that is waking up or busy for a moment
/// gets another go before the recording is given up as silent.
const TRIES: u32 = 3;
const RETRY_PAUSE: Duration = Duration::from_millis(350);
/// How long a try waits for the first samples before assuming the node is just slow to wake.
const FIRST_DATA: Duration = Duration::from_millis(1_500);

/// The microphones PipeWire has now (`Audio/Source` nodes, so no monitors of outputs), by node
/// name, and the default's name.
pub fn inputs() -> Inputs {
    match Command::new("pw-dump").stdin(Stdio::null()).stderr(Stdio::null()).output() {
        Ok(output) if output.status.success() => parse_dump(&output.stdout),
        _ => Inputs::default(),
    }
}

/// `pw-dump`'s JSON: sources in order, and the `default.audio.source` metadata entry.
pub fn parse_dump(json: &[u8]) -> Inputs {
    let Ok(objects) = serde_json::from_slice::<Vec<serde_json::Value>>(json) else {
        return Inputs::default();
    };
    let mut inputs = Inputs::default();
    for object in &objects {
        let kind = object["type"].as_str().unwrap_or_default();
        if kind.ends_with(":Node") {
            let props = &object["info"]["props"];
            let class = props["media.class"].as_str().unwrap_or_default();
            if class != "Audio/Source" && class != "Audio/Source/Virtual" {
                continue;
            }
            let Some(id) = props["node.name"].as_str() else {
                continue;
            };
            let name = props["node.description"]
                .as_str()
                .or_else(|| props["node.nick"].as_str())
                .unwrap_or(id);
            inputs.devices.push(Input {
                id: id.to_owned(),
                name: name.to_owned(),
            });
        } else if kind.ends_with(":Metadata") {
            for entry in object["metadata"].as_array().into_iter().flatten() {
                if entry["key"] == "default.audio.source" {
                    if let Some(name) = entry["value"]["name"].as_str() {
                        inputs.default = Some(name.to_owned());
                    }
                }
            }
        }
    }
    // A default that is not a source we listed (a stale entry) is no default.
    if inputs.default.as_deref().is_some_and(|d| inputs.find(d).is_none()) {
        inputs.default = None;
    }
    inputs
}

/// Little-endian f32 bytes to samples, keeping a trailing partial sample in `carry` for the
/// next chunk. Non-finite values become silence.
pub fn decode_f32(carry: &mut Vec<u8>, chunk: &[u8], out: &mut Vec<f32>) {
    carry.extend_from_slice(chunk);
    let whole = carry.len() / 4 * 4;
    out.extend(carry[..whole].chunks_exact(4).map(|b| {
        let sample = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        if sample.is_finite() { sample } else { 0.0 }
    }));
    carry.drain(..whole);
}

enum Msg {
    Data,
    Eof,
}

/// A running `pw-record`; dropping it stops the recording and keeps what was heard.
pub struct Capture {
    child: Child,
    reader: Option<std::thread::JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}

impl Capture {
    /// Records `target` (a node name; `None` follows PipeWire's default) into `samples` at
    /// 16 kHz mono. A `pw-record` that exits before any audio is retried a few times.
    pub fn start(
        target: Option<&str>,
        samples: Arc<Mutex<Vec<f32>>>,
        error: Arc<Mutex<Option<String>>>,
    ) -> Result<Self> {
        let mut last = anyhow!("PipeWire capture did not start");
        for attempt in 1..=TRIES {
            match Self::attempt(target, &samples, &error) {
                Ok(capture) => return Ok(capture),
                Err(e) => {
                    crate::capture::scrawl_log(format!(
                        "Microphone: PipeWire capture try {attempt}/{TRIES} failed: {e:#}"
                    ));
                    last = e;
                }
            }
            if attempt < TRIES {
                std::thread::sleep(RETRY_PAUSE);
            }
        }
        Err(last)
    }

    fn attempt(
        target: Option<&str>,
        samples: &Arc<Mutex<Vec<f32>>>,
        error: &Arc<Mutex<Option<String>>>,
    ) -> Result<Self> {
        let mut command = Command::new("pw-record");
        command.args([
            "--raw",
            "--rate",
            "16000",
            "--channels",
            "1",
            "--format",
            "f32",
            "--quality",
            "10",
            "--latency",
            "50ms",
        ]);
        if let Some(target) = target {
            command.args(["--target", target]);
        }
        let mut child = command
            .arg("-")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Could not start pw-record (is PipeWire's pw-record installed?)")?;
        let mut stdout = child.stdout.take().context("No pw-record output")?;
        let mut stderr = child.stderr.take().context("No pw-record stderr")?;
        let stopping = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let reader = {
            let stopping = Arc::clone(&stopping);
            let samples = Arc::clone(samples);
            let error = Arc::clone(error);
            std::thread::Builder::new()
                .name("scrawl-pw-reader".into())
                .spawn(move || read_loop(&mut stdout, &samples, &error, &stopping, tx))
                .context("Could not spawn the PipeWire reader")?
        };
        let diagnostics = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = (&mut stderr).take(4_096).read_to_string(&mut text);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            text
        });
        let capture = Self {
            child,
            reader: Some(reader),
            stopping,
        };
        match rx.recv_timeout(FIRST_DATA) {
            Ok(Msg::Data) => Ok(capture),
            Err(RecvTimeoutError::Timeout) => {
                crate::capture::scrawl_log(
                    "Microphone: no samples yet from PipeWire; waiting for the node to wake",
                );
                Ok(capture)
            }
            Ok(Msg::Eof) | Err(RecvTimeoutError::Disconnected) => {
                drop(capture);
                let said = diagnostics.join().unwrap_or_default();
                bail!("pw-record ended without audio: {}", said.trim());
            }
        }
    }
}

fn read_loop(
    stdout: &mut impl Read,
    samples: &Mutex<Vec<f32>>,
    error: &Mutex<Option<String>>,
    stopping: &AtomicBool,
    tx: Sender<Msg>,
) {
    let mut buffer = [0_u8; 16 * 1024];
    let mut carry = Vec::new();
    let mut decoded = Vec::new();
    let mut announced = false;
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                decoded.clear();
                decode_f32(&mut carry, &buffer[..count], &mut decoded);
                if let Ok(mut output) = samples.lock() {
                    output.extend_from_slice(&decoded);
                }
                if !announced && !decoded.is_empty() {
                    announced = true;
                    let _ = tx.send(Msg::Data);
                }
            }
        }
    }
    // While `start` still listens, it decides (and may retry); afterwards the end of the
    // stream, unless we asked for it, is a device that went away.
    if tx.send(Msg::Eof).is_err() && !stopping.load(Ordering::Acquire) {
        if let Ok(mut error) = error.lock() {
            *error = Some("PipeWire stopped delivering audio".to_owned());
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        // SIGTERM lets pw-record flush what it holds; a stuck one is killed.
        // SAFETY: plain signal to our own child.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_millis(500);
        while matches!(self.child.try_wait(), Ok(None)) {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_samples_carry_over_chunks() {
        let bytes: Vec<u8> = [0.5_f32, -1.0, f32::NAN]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let (mut carry, mut out) = (Vec::new(), Vec::new());
        decode_f32(&mut carry, &bytes[..6], &mut out);
        assert_eq!(out, [0.5]);
        decode_f32(&mut carry, &bytes[6..], &mut out);
        assert_eq!(out, [0.5, -1.0, 0.0]);
        assert!(carry.is_empty());
    }

    #[test]
    fn dump_lists_sources_and_the_default() {
        let json = br#"[
          {"type":"PipeWire:Interface:Node","info":{"props":{"media.class":"Audio/Sink","node.name":"out"}}},
          {"type":"PipeWire:Interface:Node","info":{"props":{"media.class":"Audio/Source","node.name":"alsa_input.usb","node.description":"HyperX"}}},
          {"type":"PipeWire:Interface:Node","info":{"props":{"media.class":"Audio/Source","node.name":"alsa_input.pci"}}},
          {"type":"PipeWire:Interface:Metadata","metadata":[
            {"key":"default.audio.sink","value":{"name":"out"}},
            {"key":"default.audio.source","value":{"name":"alsa_input.usb"}}]}
        ]"#;
        let inputs = parse_dump(json);
        assert_eq!(inputs.devices.len(), 2);
        assert_eq!(inputs.devices[0].name, "HyperX");
        assert_eq!(inputs.devices[1].name, "alsa_input.pci");
        assert_eq!(inputs.default.as_deref(), Some("alsa_input.usb"));
        assert_eq!(inputs.default_input().unwrap().name, "HyperX");
    }

    #[test]
    fn a_stale_default_is_dropped_and_junk_is_empty() {
        let json = br#"[{"type":"PipeWire:Interface:Metadata","metadata":[
            {"key":"default.audio.source","value":{"name":"gone"}}]}]"#;
        assert_eq!(parse_dump(json).default, None);
        assert!(parse_dump(b"not json").devices.is_empty());
    }

    /// Records a private null sink's monitor (never a real microphone):
    /// `cargo test -- --ignored pipewire_captures`.
    #[test]
    #[ignore]
    fn pipewire_captures_a_null_sink_at_16k_mono() {
        let load = Command::new("pactl")
            .args(["load-module", "module-null-sink", "sink_name=scrawl_test_sink"])
            .output()
            .unwrap();
        let module = String::from_utf8_lossy(&load.stdout).trim().to_owned();
        assert!(load.status.success(), "pactl needs PipeWire");
        std::thread::sleep(Duration::from_millis(300));
        let samples = Arc::new(Mutex::new(Vec::new()));
        let error = Arc::new(Mutex::new(None));
        let capture = Capture::start(
            Some("scrawl_test_sink.monitor"),
            Arc::clone(&samples),
            Arc::clone(&error),
        );
        let wav = std::env::temp_dir().join(format!("scrawl-tone-{}.wav", std::process::id()));
        let _ = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=f=440:d=2", "-ar", "48000"])
            .arg(&wav)
            .status();
        let _ = Command::new("pw-play")
            .args(["--target", "scrawl_test_sink"])
            .arg(&wav)
            .status();
        drop(capture);
        let _ = Command::new("pactl").args(["unload-module", &module]).status();
        let _ = std::fs::remove_file(&wav);
        let heard = samples.lock().unwrap().clone();
        assert!(error.lock().unwrap().is_none());
        // ~2 s tone + lead-in at 16 kHz.
        assert!(heard.len() > 28_000, "{} samples", heard.len());
        let peak = heard.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.01 && peak <= 1.0, "peak {peak}");
    }
}
