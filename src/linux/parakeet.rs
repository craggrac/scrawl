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
/// Longest clip sent to the model; Parakeet degrades on long audio.
const MAX_CLIP_MS: u64 = 30_000;
/// Islands closer than this belong to one clip, if it stays within `MAX_CLIP_MS`.
const MERGE_GAP_MS: u64 = 700;
/// Islands shorter than this are clicks and breaths.
const MIN_ISLAND_MS: u64 = 120;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const CLIP_TIMEOUT: Duration = Duration::from_secs(120);

/// One offline result. `timestamps` and `durations` are seconds from the clip's start, one per
/// token; a token that begins with a space begins a word.
#[derive(Debug, Deserialize)]
pub struct SherpaResult {
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

/// Exact zeros throughout: a muted microphone, not a quiet room.
pub fn is_digital_silence(samples: &[f32]) -> bool {
    !samples.is_empty() && samples.iter().all(|&sample| sample == 0.0)
}

/// What Parakeet says for noise with no speech in it. A clip that comes back with nothing but
/// these words is dropped; the same words inside real speech stay.
const PHANTOM_WORDS: [&str; 7] = ["obrigado", "obrigada", "thank", "thanks", "you", "tchau", "bye"];

/// Whether a clip's words are only `PHANTOM_WORDS`.
pub fn is_phantom(words: &[WordTiming]) -> bool {
    !words.is_empty()
        && words.iter().all(|word| {
            let bare = word.text.trim_matches(|c: char| !c.is_alphanumeric());
            PHANTOM_WORDS.contains(&bare.to_lowercase().as_str())
        })
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
                text: phrase
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            });
            start = index + 1;
        }
    }
    segments
}

/// Splits `[start, end)` into chunks of at most `MAX_CLIP_MS`.
fn split_span(start: u64, end: u64) -> impl Iterator<Item = (u64, u64)> {
    (start..end)
        .step_by(MAX_CLIP_MS as usize)
        .map(move |from| (from, (from + MAX_CLIP_MS).min(end)))
}

/// Clips of at most 30 s: islands under 120 ms dropped, close ones (gap under 700 ms) merged
/// while the clip fits, longer ones split. No islands means the whole file in chunks, unless
/// it is too short to be worth sending.
pub fn plan_clips(islands: &[(u64, u64)], duration_ms: u64) -> Vec<(u64, u64)> {
    if islands.is_empty() {
        return if duration_ms >= MIN_WHOLE_MS {
            split_span(0, duration_ms).collect()
        } else {
            Vec::new()
        };
    }
    let mut clips: Vec<(u64, u64)> = Vec::new();
    for &(start, end) in islands {
        if end.saturating_sub(start) < MIN_ISLAND_MS {
            continue;
        }
        match clips.last_mut() {
            Some(last)
                if start.saturating_sub(last.1) < MERGE_GAP_MS && end - last.0 <= MAX_CLIP_MS =>
            {
                last.1 = end;
            }
            _ => clips.extend(split_span(start, end)),
        }
    }
    clips
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
        .or_else(|| {
            dirs::cache_dir().map(|c| c.join("openwhispr/parakeet-models").join(MODEL_NAME))
        })
        .filter(|dir| dir.join("tokens.txt").is_file())
}

/// A sherpa-onnx server of our own, stopped when dropped. It is started per recording, so no
/// model stays in memory between captures. `sherpa-onnx-ws` has no address option and binds
/// every interface, not just loopback; it lives only during one transcription, and a remote
/// peer could only submit its own audio, never read ours, since each connection gets only its
/// own result. Our client connects over loopback.
struct Server {
    child: Child,
    port: u16,
}

impl Server {
    /// Starts the server, once more on a fresh port if the reserved one was taken meanwhile.
    fn start(binary: &Path, model: &Path) -> Result<Self> {
        let mut attempt = 1;
        loop {
            match Self::spawn(binary, model) {
                Err(error) if attempt < 2 => {
                    crate::capture::scrawl_log(format!(
                        "sherpa-onnx start failed, retrying: {error:#}"
                    ));
                    attempt += 1;
                }
                result => return result,
            }
        }
    }

    fn spawn(binary: &Path, model: &Path) -> Result<Self> {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
            .local_addr()?
            .port();
        let file = |flag: &str, name: &str| format!("--{flag}={}", model.join(name).display());
        let child = Command::new(binary)
            .env("LD_LIBRARY_PATH", binary.parent().unwrap_or(Path::new(".")))
            .arg(file("tokens", "tokens.txt"))
            .arg(file("encoder", "encoder.int8.onnx"))
            .arg(file("decoder", "decoder.int8.onnx"))
            .arg(file("joiner", "joiner.int8.onnx"))
            .args([
                "--model-type=nemo_transducer",
                "--num-threads=4",
                "--num-work-threads=1",
            ])
            .arg(format!("--port={port}"))
            .arg("--log-file=/dev/null")
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Could not start {}", binary.display()))?;
        // Owned by a `Server` first, so even the early return below kills and reaps the child.
        let mut server = Self { child, port };
        let stderr = server
            .child
            .stderr
            .take()
            .context("No sherpa-onnx stderr")?;
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
            Err(_) => bail!(
                "sherpa-onnx did not start within {} s",
                STARTUP_TIMEOUT.as_secs()
            ),
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
    let mut transcript = TranscriptData {
        source: "parakeet".into(),
        model: Some(MODEL_NAME.into()),
        ..Default::default()
    };
    if is_digital_silence(&samples) {
        return Ok(transcript);
    }
    // No islands (too quiet to find speech by energy): Parakeet hears the whole thing.
    let clips = plan_clips(&audio::speech_islands(&samples), duration_ms);
    if clips.is_empty() {
        return Ok(transcript);
    }
    let server = Server::start(&binary, &model)?;
    let total = clips.len();
    let mut failed = 0;
    for (start, end) in clips {
        let from_ms = start.saturating_sub(CLIP_PAD_MS);
        let from = (from_ms * SAMPLES_PER_MS) as usize;
        let to = (((end + CLIP_PAD_MS) * SAMPLES_PER_MS) as usize).min(samples.len());
        if from >= to {
            continue;
        }
        match server.transcribe(&samples[from..to]) {
            Ok(result) => {
                let clip_words = words(&result, from_ms);
                if is_phantom(&clip_words) {
                    crate::capture::scrawl_log(format!(
                        "Parakeet heard only noise at {start} ms; dropped its words"
                    ));
                } else {
                    transcript.words.extend(clip_words);
                }
            }
            Err(error) => {
                failed += 1;
                crate::capture::scrawl_log(format!(
                    "Parakeet skipped the clip at {start} ms: {error:#}"
                ));
            }
        }
    }
    if failed == total {
        bail!("Parakeet failed on all {total} clips");
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
            [
                "Olha",
                "esse",
                "botão",
                "aqui,",
                "ele",
                "não",
                "salva",
                "o",
                "formulário",
                "quando",
                "eu",
                "clico."
            ]
        );
        assert_eq!((words[0].t0, words[0].t1), (1_000, 1_320));
        assert_eq!((words[2].t0, words[2].t1), (1_640, 1_880));
        assert_eq!((words[3].t0, words[3].t1), (1_880, 2_280));
        assert_eq!((words[11].t0, words[11].t1), (4_360, 4_920));
    }

    #[test]
    fn a_missing_duration_ends_at_the_next_token() {
        let result = SherpaResult {
            tokens: vec![" a".into(), " b".into()],
            timestamps: vec![0.5, 1.0],
            durations: vec![],
        };
        let words = words(&result, 0);
        assert_eq!((words[0].t0, words[0].t1), (500, 1_000));
        assert_eq!((words[1].t0, words[1].t1), (1_000, 1_080));
    }

    fn spoken(texts: &[&str]) -> Vec<WordTiming> {
        texts
            .iter()
            .map(|text| WordTiming {
                t0: 0,
                t1: 0,
                text: (*text).to_owned(),
            })
            .collect()
    }

    #[test]
    fn a_clip_of_only_thanks_is_phantom() {
        assert!(is_phantom(&spoken(&["Obrigado.", "Obrigado."])));
        assert!(is_phantom(&spoken(&["Thank", "you."])));
        assert!(!is_phantom(&spoken(&["Obrigado", "pela", "ajuda."])));
        assert!(!is_phantom(&[]));
    }

    #[test]
    fn only_exact_zeros_are_digital_silence() {
        assert!(is_digital_silence(&[0.0; 160]));
        assert!(!is_digital_silence(&[0.0, 0.0, 0.0001]));
        assert!(!is_digital_silence(&[]));
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

    #[test]
    fn close_islands_merge_within_the_cap() {
        let clips = plan_clips(&[(1_000, 2_000), (2_500, 3_000), (10_000, 11_000)], 20_000);
        assert_eq!(clips, [(1_000, 3_000), (10_000, 11_000)]);
    }

    #[test]
    fn merging_never_passes_30_seconds() {
        let clips = plan_clips(&[(0, 20_000), (20_300, 40_000)], 40_000);
        assert_eq!(clips, [(0, 20_000), (20_300, 40_000)]);
        assert!(clips.iter().all(|(s, e)| e - s <= MAX_CLIP_MS));
    }

    #[test]
    fn a_long_island_splits_into_30_second_chunks() {
        let clips = plan_clips(&[(5_000, 70_000)], 80_000);
        assert_eq!(clips, [(5_000, 35_000), (35_000, 65_000), (65_000, 70_000)]);
    }

    #[test]
    fn tiny_islands_are_dropped() {
        assert_eq!(
            plan_clips(&[(0, 100), (5_000, 6_000)], 10_000),
            [(5_000, 6_000)]
        );
    }

    #[test]
    fn quiet_audio_becomes_whole_file_chunks() {
        assert_eq!(
            plan_clips(&[], 65_000),
            [(0, 30_000), (30_000, 60_000), (60_000, 65_000)]
        );
        assert!(plan_clips(&[], 300).is_empty());
    }

    /// Loads the 0.6B model (~1 GB RAM, a few seconds): `cargo test -- --ignored parakeet`.
    #[test]
    #[ignore]
    fn parakeet_transcribes_spoken_portuguese() {
        let dir = std::env::temp_dir().join(format!("scrawl-parakeet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("speech.wav");
        let spoken = std::process::Command::new("espeak-ng")
            .args(["-v", "pt-br", "-w"])
            .arg(&raw)
            .arg("Olha esse botão aqui.")
            .status()
            .expect("espeak-ng is needed for this test");
        assert!(spoken.success());
        // espeak writes 22 kHz; `decode_wav_16k` only takes 16 kHz mono s16.
        let wav = dir.join("speech16.wav");
        let converted = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(&raw)
            .args(["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"])
            .arg(&wav)
            .status()
            .expect("ffmpeg is needed for this test");
        assert!(converted.success());
        let transcript = transcribe(&wav).unwrap();
        assert_eq!(transcript.source, "parakeet");
        assert!(
            transcript.text.to_lowercase().contains("botão"),
            "{}",
            transcript.text
        );
        assert!(!transcript.estimated && !transcript.words.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
