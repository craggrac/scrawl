# Scrawl no Linux (Hyprland/Wayland): fases 1 e 2

## Objetivo

Rodar o Scrawl no Linux com Hyprland: gravar tela + voz por um atalho, transcrever com tempo por palavra
e gerar o mesmo bundle `.scrawl` (`contact-sheet.png`, `frames/`, `summary.md`, `scrawl.json`,
`recording.mp4`, `audio.wav`), entregue na área de transferência (OpenCode por `scrawl send`).

O Mac continua compilando e funcionando igual. Código do Linux fica atrás de
`#[cfg(target_os = "linux")]` ou em `src/linux/`; código compartilhado só muda quando a mudança vale para os
dois sistemas (ex.: separar a parte portável de `capture.rs`).

## Abordagem

Ferramentas do sistema como backend (abordagem A). O Scrawl chama processos externos no lugar das APIs do
macOS:

| macOS | Linux | Dependência de runtime |
|---|---|---|
| ScreenCaptureKit (`capture.rs`) | `wf-recorder` (wlr-screencopy, suportado pelo Hyprland) | `wf-recorder` |
| `scrawl-helper` probe/frames/audio/shrink | `ffprobe`/`ffmpeg` | `ffmpeg` |
| SpeechAnalyzer | Parakeet TDT 0.6b v3 via `sherpa-onnx-ws` (o do OpenWhispr), tempo por token | OpenWhispr instalado (binário + modelo) |
| CGWindowList (`windows.rs`) | `hyprctl -j clients`, `activewindow`, `cursorpos`, `monitors` | Hyprland |
| NSPasteboard (`handoff.rs`) | `wl-copy` | `wl-clipboard` |
| hotkey Carbon | `bind` do Hyprland → `scrawl toggle` | — |

Rejeitada: tudo em processo (pipewire-rs + portal + whisper-rs/sherpa-rs). Mais portável entre desktops,
mas build pesado e muito mais código. Pode substituir um backend por vez no futuro, porque cada backend
fica atrás das mesmas funções que o Mac usa.

Transcrição verificada em 2026-10-19: `sherpa-onnx-ws-linux-x64 --model-type=nemo_transducer` com
`parakeet-tdt-0.6b-v3` devolve `text`, `tokens`, `timestamps` e `durations` (segundos por token; token que
começa com espaço abre palavra nova), em português correto.

## Fora de escopo

iPhone ao vivo (`PhoneSession`, `mirror`, servidor de ingest), Sparkle, onboarding/permissões (TCC), pill,
GPUI, overlay de tinta, Settings, Hex no Linux, `export` em vídeo (usa o helper Swift; no Linux falha com
erro claro), colar automático (⌘V sintético), transcrição ao vivo. Fase 3 (cliques/teclas via
`/dev/input`) e fase 4 (tinta ⇧ na tela, pill, GPUI Linux) ganham specs próprias.

## Fase 1: compilar no Linux e ler bundles

Resultado: `cargo build` passa no Linux; `demo`, `inspect`, `frames`, `zoom`, `send`, `import`, `reprocess`,
`clean`, `pin`, `route`, `serve`, `skill`, `capture-sim`, `autocrop`, `autocrop-eval`, `fixture` funcionam.
Subcomandos só do Mac (`app`, `lab`, `tune`, `devices`, `sck-probe`, `windows`, `preview`, `mic watch`,
`hex`, `link`) somem do `clap` no Linux via `#[cfg]`; `record --iphone` dá erro no Linux.

1. **Gating de módulos.** Em `main.rs`, `ui`, `laser_overlay`, `native`, `sparkle`, `ax`, `settings`,
   `onboarding`, `screens`, `icons` e `hold` ficam sob `#[cfg(target_os = "macos")]`.
2. **`capture` dividido.** `src/capture.rs` vira `src/capture/mod.rs` com só a parte portável
   (`scrawl_log`, `FRAME_SAMPLE_INTERVAL`, `CANDIDATE_EDGE`, `CANDIDATE_BUDGET_BYTES`, `CandidateBuffer`,
   `ScreenRecording`, `capture_size`, `write_bundle_artifacts`) e `src/capture/mac.rs` com o resto,
   declarado `#[cfg(target_os = "macos")] mod mac; pub use mac::*;`. Assim todo `crate::capture::scrawl_log`
   existente continua valendo.
3. **`windows` dividido do mesmo jeito.** Tipos (`WindowSnapshot`, `WindowEntry`, `CursorSample`,
   `ActivityLog` e métodos) ficam portáveis; `raw_windows`, `snapshot`, `dock_rect`, `filter_windows`,
   `owned_window_at` e o `WindowLogger` do Mac ficam sob `cfg(macos)`.
4. **Outros usos do Mac.** `phone.rs`: `PhoneSession` e o servidor de ingest (usam `SharedCaptureTelemetry`
   e `LiveOverlayEvent`) ficam sob `cfg(macos)`; `import`, `build_movie_bundle`, `resolve_import`,
   `finalize_live_capture` ficam portáveis. `audio.rs`: a chamada `crate::ui::start_metric` fica sob
   `cfg(macos)`. `helper::permissions` fica sob `cfg(macos)`. `handoff::pill_style` fica sob `cfg(macos)`.
5. **Helper → ffmpeg.** `helper.rs` mantém a mesma API pública. No Linux, `probe`, `frames`
   (`FrameTimes::At` e `Every`, com `max_edge`), `export_audio` e `shrink` chamam `ffprobe`/`ffmpeg` (frames
   saem em `rawvideo rgba` pelo stdout). `devices` e `qr` retornam erro "não suportado no Linux";
   `transcribe` vai para o Parakeet (fase 2). Implementação em `src/linux/media.rs`, escolhida por `cfg`
   dentro de `helper.rs`.
6. **Caminhos.** Capturas em `dirs::data_dir()/scrawl/captures` (no Mac é o mesmo
   `~/Library/Application Support/scrawl/captures` de hoje; no Linux `~/.local/share/scrawl/captures`); log
   no Linux em `dirs::state_dir()/scrawl/scrawl.log`. `SCRAWL_CAPTURES_DIR` continua valendo.
7. **Texto no contact sheet.** Fora do Mac, `render.rs` usa largura chutada e glifos 5×7. No Linux: crate
   `ab_glyph` com a fonte do sistema achada por `fc-match -f '%{file}' 'sans:bold'` / `'sans'` (cache em
   `OnceLock`). Sem fonte, mantém o fallback atual.
8. **Área de transferência.** Os stubs de `handoff.rs:185-195` ganham versão Linux com `wl-copy`:
   `copy_bundle_to_clipboard` copia o `summary.md` como texto. Colar automático continua dando erro.
9. **`open`.** `autocrop --open` usa `xdg-open` no Linux. `routing::browser_tab_url` (`osascript`) já falha
   sem quebrar quando o binário não existe; fica como está.

## Fase 2: gravar

Resultado: `bind = ALT SHIFT, S, exec, scrawl toggle` no `hyprland.conf` inicia e para uma gravação; ao
parar, o bundle é montado e transcrito, o resumo vai para a área de transferência e `notify-send` avisa
início, fim e erro. Entrega automática ao OpenCode fica fora: o roteamento do Mac decide por pistas que não
existem aqui (aba do navegador via AppleScript, janela do terminal) e mandar para a sessão errada é pior
que não mandar; `scrawl send latest [--session …]` continua funcionando à mão. Roteamento no Linux ganha
spec própria.

### Componentes (em `src/linux/`)

- **`record.rs` — sessão de tela.** `ScreenSession::start(&target, &video, started_at) -> Result<Self>`
  lança `wf-recorder -o <monitor> -c h264_nvenc -f <video>` (sem NVENC: `-c libx264`)
  ou `-g "x,y wxh"` para janela. `stop()` manda SIGINT, espera sair e devolve `ScreenRecording`: `video`,
  `duration_ms` pelo `ffprobe`, `video_offset_ms` = ms entre o start e o primeiro byte no arquivo, `frames` =
  candidatos tirados do vídeo com `helper::frames(FrameTimes::Every(FRAME_SAMPLE_INTERVAL), CANDIDATE_EDGE)`
  com o tempo somado a `video_offset_ms`. O seletor de keyframes não muda.
- **`hypr.rs` — janelas, cursor e alvo.** `WindowLogger` com a interface do Mac (`start(frame, started_at,
  request)`, `stop() -> ActivityLog`): a cada 100 ms lê `hyprctl -j cursorpos` (vira `CursorSample` por
  `push_cursor`); a cada 1 s, ou quando a janela ativa muda, lê `hyprctl -j clients` + `activewindow` (vira
  `WindowEntry`s por `push_windows`). `discover_target(window)` usa `hyprctl -j monitors` (monitor focado)
  ou casa `--window` com título/classe em `clients`. Coordenadas lógicas do Hyprland; escala vem de
  `monitors`.
- **`parakeet.rs` — transcrição.** No Linux, `helper::transcribe(wav)` chama o Parakeet, então
  `transcribe.rs` não muda (o motor "apple" significa "o do sistema"). Sobe
  `sherpa-onnx-ws-linux-x64 --model-type=nemo_transducer --port=<livre>` com `LD_LIBRARY_PATH` na pasta do
  binário, espera `Listening on` no stderr, corta o áudio em ilhas de fala (`audio::speech_islands` +
  `hex::plan_clips`, como o Hex já faz), manda cada clip pelo WebSocket (`[i32 LE sample_rate][i32 LE
  bytes][f32 samples]`, resposta JSON, depois `Done`), junta tokens em palavras e soma o início do clip.
  `WordTiming` medido (`estimated: false`), `source: "parakeet"`. Binário: `SCRAWL_SHERPA_WS`, senão
  `~/.local/opt/openwhispr/*/resources/bin/sherpa-onnx-ws-linux-x64`. Modelo: `SCRAWL_PARAKEET_DIR`, senão
  `~/.cache/openwhispr/parakeet-models/parakeet-tdt-0.6b-v3`. Faltou algum: bundle sem transcrição, log diz
  o que falta. Crate nova: `tungstenite` (sem TLS).
- **`toggle.rs` — liga/desliga.** `scrawl toggle`: se `$XDG_RUNTIME_DIR/scrawl/record.pid` aponta para um
  processo vivo, manda SIGINT e sai; senão inicia `scrawl record --until-stopped` desanexado (`setsid`) e
  grava o pid. `record --until-stopped` espera SIGINT/SIGTERM (handler com `libc` setando um `AtomicBool`) no
  lugar do `sleep(duration)`. Pid velho (processo morto) é ignorado e sobrescrito. O próprio `record` apaga o
  pidfile ao terminar.
- **`linux::record` (em `src/linux/mod.rs`).** No Linux, o braço `Command::Record { iphone: false }` chama
  `linux::record(window, duration, until_stopped, note, out)`, que junta `hypr::discover_target`,
  `AudioRecorder`, `ScreenSession`, `hypr::WindowLogger` e o mesmo `finalize_capture_bundle` do Mac; depois
  `copy_bundle_to_clipboard` e `notify-send`. O braço do Mac não muda.

### Fluxo

`⌥⇧S` → `scrawl toggle` → `scrawl record --until-stopped` → wf-recorder + cpal + hyprctl rodando →
`⌥⇧S` → SIGINT → para tudo → candidatos via ffmpeg → Parakeet → `finalize_capture_bundle` →
`wl-copy` → `notify-send`.

### Erros

- `wf-recorder` ausente ou sai no primeiro segundo: aborta com mensagem clara e notificação, sem bundle.
- Sem mic (`cpal` falha): grava sem áudio, como hoje.
- Sem Parakeet: bundle sem transcrição (`source: "offline"`, como hoje quando o SpeechAnalyzer falha) + log.
- `hyprctl` falha (fora do Hyprland): grava o primeiro monitor de `wf-recorder` sem log de janelas.
- `scrawl toggle` duplo rápido: o pidfile é escrito pelo `toggle` logo após o spawn; o segundo toggle para.

## Testes

- Os testes existentes dos módulos portáveis precisam passar no Linux.
- Novos testes com saídas reais como fixture: JSON do `ffprobe` → `Probe`; JSON do sherpa → palavras;
  `hyprctl -j clients`/`monitors`/`cursorpos` → entradas e alvo; pidfile do toggle (vivo, morto, ausente).
- Fim a fim manual: `scrawl record --duration 8` e `scrawl toggle` duas vezes, depois abrir
  `contact-sheet.png` e `summary.md` do bundle (princípio 5 do `AGENTS.md`).
- Todo `cargo build`/`test` roda só depois de avisar o usuário, com `nice -n 19 ionice -c 3 cargo … -j 2`.

## Dependências novas

- Crates (só Linux): `ab_glyph`, `tungstenite` (`default-features = false, features = ["handshake"]`).
- Sistema: `wf-recorder`, `ffmpeg`, `wl-clipboard`, `libnotify`, `fontconfig`, OpenWhispr (já instalado).
