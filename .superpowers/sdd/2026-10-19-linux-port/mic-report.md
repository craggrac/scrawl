# Mic capture and transcription quality (feat-mic-pw)

## Root cause (task A)
- /tmp/scrawl-app.log: three recordings in a row (1793052191, 1793052199, 1793052201) logged `Microphone startup failed: The requested audio device is not available`. cpal maps ALSA ENODEV/ENOENT/ENOTSUPP, or a zero buffer/period size or zero poll-fd count after `prepare()`, to `DeviceNotAvailable` (cpal 0.18.2 alsa/mod.rs lines ~397-403, 1716). The "default" PCM is pipewire-alsa, which returns those errors when its node connect races a suspended node (the USB source was SUSPENDED; the failures came ~35 min after the last use, and the next success was ~20 min later).
- NOT reproduced: reproducing needed opening the real mic, which was off limits. So this is a well-supported hypothesis, not proof. Also `AudioRecorder::start()` always returned Ok and the failure only surfaced at `stop()`, where callers did `.ok()` silently: the user got a capture with no audio and no message.
- Fix: bypass the ALSA shim entirely. A transient failure of the new path is also retried.

## Changes
- `src/linux/pwmic.rs` (new): `pw-record --raw --rate 16000 --channels 1 --format f32 --quality 10 --target <node> -`, stdout read into the same `samples` Vec. PipeWire does the downmix/resample (proper filter replaces the 3-tap box decimator). Up to 3 starts, 350 ms apart, when pw-record exits before any audio; no data within 1.5 s but alive is accepted (node waking). Later EOF sets the recorder `error` (so `failed()` works for the replay buffer). Drop sends SIGTERM, then kill after 500 ms, and joins the reader. `inputs()` parses `pw-dump` (Audio/Source nodes, `default.audio.source` metadata).
- `src/audio.rs`: cpal code gated to `not(linux)`; Linux `open_stream` uses `mic::inputs()` / `Pick` and resolves an explicit node name (the default included). API unchanged (start/stop/tail/keep_last/failed/live feed). New `stop_noting()` logs the failure and, on Linux, sends a notify-send "No microphone audio was recorded"; used by ui.rs and linux/mod.rs.
- `src/mic.rs`: Linux `inputs()` is PipeWire node name + description (so the mic choice maps to PipeWire nodes); cpal `device()` is non-Linux only. macOS untouched.
- `src/linux/parakeet.rs`: MERGE_GAP_MS 700 -> 2500, CLIP_PAD_MS 200 -> 400; `plan_clips_with`, `transcribe_clips` factored out; `SCRAWL_SHERPA_PROVIDER` env (default cpu); ignored harness test `ab_clip_plans`.

## A/B (48.5 s Portuguese capture -3, server on CPU, 4 threads)
Run: `SCRAWL_AB_WAV=<audio.wav> cargo test -- --ignored --nocapture ab_clip_plans`.
| plan | clips | words | time |
|---|---|---|---|
| current (gap 700, pad 200) | 8 | 168 | 4.6 s |
| gap 2500, pad 400 | 2 | 168 | 4.7 s |
| gap 4000, pad 400 | 2 | 168 | 5.3 s (same clips as 2500) |
| first 30 s as one clip | 1 | 109 | 3.5 s |

Differences current -> gap2500:
- "tá aqui não é pra ser um preview" -> "tá? Que não é pra ser um preview"
- "Se matar uma bosta aqui, que é torque, kimura, lustro" -> "Se ela tá uma bosta aqui que é torque, queimura, lustro"
- "eu queria meio que uma" -> "eu queria uma meio que uma"
- "pode gerar" -> "pode girar"
- Lost with gap2500: the trailing "E aí" (a 300 ms island); the current plan kept it.
Without ground truth, gap2500 is better on three phrases and worse on one trailing filler. Word times stay on the recording clock (clip offset + token times); first/last word 400/46520 ms vs 320/48290.

Timing: the 16.8 s in the live app was NOT transcription cost: here transcription takes ~4.6 s plus ~2.7 s server/model start. The app was concurrently decoding 220 candidate frames (13.4 s) on the same CPU.

CUDA: the OpenWhispr sherpa-onnx-ws build says "Please compile with -DSHERPA_ONNX_ENABLE_GPU=ON ... Fallback to cpu!"; no `libonnxruntime_providers_cuda` is installed. `--provider=cuda` is a silent CPU fallback (4.0 s vs 4.6 s is noise), so the default stays cpu. The env knob exists for a future GPU pack.

## Tests
- `cargo test`: 596 passed, 0 failed, 6 ignored (591 before + 3 pwmic + 2 plan tests). The ignored `pipewire_captures_a_null_sink_at_16k_mono` passes (private null sink + tone, sink unloaded afterwards; real mic never opened).
- Mac type-check (patched copy of mc.sh, /tmp/mc-mic.sh): clean. `cargo build`: ok.

## Risks
- Root cause is a hypothesis; the default-source path of pw-record was not run (explicit node name resolved from pw-dump instead, which is what the null-sink test exercises).
- Requires `pw-record` and `pw-dump` (pipewire tools); a missing binary now means no audio + a notification (no cpal fallback on Linux).
- notify-send during recording may appear in the capture; it is only sent at stop.
- Longer clips: slower first words for live feedback is not affected (post-recording); a missing trailing filler is possible.
