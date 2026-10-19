# Scrawl no Linux (Hyprland/Wayland): fases 1 e 2

## Objetivo

Rodar o Scrawl no Linux com Hyprland: gravar tela + voz por um atalho, transcrever com tempo por palavra
e gerar o mesmo bundle `.scrawl` (`contact-sheet.png`, `frames/`, `summary.md`, `scrawl.json`,
`recording.mp4`, `audio.wav`), entregue na área de transferência e no OpenCode.

O Mac continua compilando e funcionando igual. Toda mudança no Linux fica atrás de
`#[cfg(target_os = "linux")]` ou num módulo novo; código compartilhado só muda quando a mudança vale para os
dois sistemas (ex.: mover `write_bundle_artifacts` para fora de `capture.rs`).

## Abordagem

Ferramentas do sistema como backend (abordagem A). O Scrawl chama processos externos no lugar das APIs do
macOS:

| macOS | Linux | Dependência de runtime |
|---|---|---|
| ScreenCaptureKit (`capture.rs`) | `wf-recorder` (wlr-screencopy, suportado pelo Hyprland) | `wf-recorder` |
| `scrawl-helper` probe/frames/audio/shrink | `ffprobe`/`ffmpeg` | `ffmpeg` |
| SpeechAnalyzer | `whisper-cli` (whisper.cpp) com tempo por palavra | `whisper.cpp`, um modelo ggml |
| CGWindowList (`windows.rs`) | `hyprctl -j clients`, `activewindow`, `cursorpos`, `monitors` | Hyprland |
| NSPasteboard (`handoff.rs`) | `wl-copy` | `wl-clipboard` |
| hotkey Carbon | `bind` do Hyprland → `scrawl toggle` | — |

Rejeitada: tudo em processo (pipewire-rs + portal + whisper-rs). Mais portável entre desktops, mas build
pesado (whisper com CUDA) e muito mais código. Pode substituir um backend por vez no futuro, porque cada
backend fica atrás das mesmas funções que o Mac usa.

## Fora de escopo

iPhone (`phone.rs` ao vivo, `mirror`), ScrawlKit ao vivo, Sparkle, onboarding/permissões (TCC), pill, GPUI,
overlay de tinta, Settings, Hex, `export` em vídeo com zoom (fica para depois; `export` sem helper falha com
erro claro), colar automático (⌘V sintético). Fase 3 (cliques/teclas via `/dev/input`) e fase 4 (tinta ⇧ na
tela, pill, GPUI Linux) ganham specs próprias.

## Fase 1: compilar no Linux e ler bundles

Resultado: `cargo build` passa no Linux; `demo`, `inspect`, `frames`, `zoom`, `send`, `import`, `reprocess`,
`clean`, `pin`, `route`, `serve`, `skill` funcionam. Subcomandos só do Mac (`app`, `lab`, `tune`, `devices`,
`sck-probe`, `windows`, `preview`, `record --iphone`, `mic watch`, `hex`) somem do `clap` no Linux via
`#[cfg]`. Os que só dependem de arquivos (`capture-sim`, `autocrop`, `autocrop-eval`, `fixture`) ficam.

1. **Gating de módulos.** Em `main.rs`, os módulos que importam APIs do Mac ficam sob
   `#[cfg(target_os = "macos")]`: `ui`, `laser_overlay`, `native`, `sparkle`, `ax`, `settings`,
   `onboarding`, `screens`, `icons` (GPUI), `hold` (usa `ui::SelectionMode`) e `capture` (SCK/Carbon).
   O que o resto do código usa de `capture` sai para o módulo compartilhado `bundle_io.rs`: `scrawl_log` (usado por
   `inbox`, `retention`), `ScreenRecording`, `CANDIDATE_EDGE`, `CANDIDATE_BUDGET_BYTES`,
   `FRAME_SAMPLE_INTERVAL`, `CandidateBuffer`, `capture_size` (usados por `autocrop_lab`, que continua
   compilando no Linux) e as funções puras de gesto (`is_pin_click`, `ink_before`, `is_real_ink`).
   Critério: só move o que o Linux precisa para compilar.
2. **Escrita do bundle.** `write_bundle_artifacts` (`capture.rs:3597`) e `ScreenRecording` vão para o mesmo
   `bundle_io.rs`. `finalize_capture_bundle` (`main.rs:785`) fica igual.
3. **Helper → ffmpeg.** `helper.rs` mantém a mesma API pública. No Linux, `probe`, `frames`
   (`FrameTimes::At` e `Every`, com `max_edge`), `export_audio` e `shrink` chamam `ffprobe`/`ffmpeg`
   (frames saem em `rawvideo rgba` por stdout, sem arquivos temporários). `permissions`, `devices`, `qr` e
   `transcribe` retornam erro "não suportado no Linux". Implementação em `src/helper_ffmpeg.rs`, escolhida
   por `cfg` dentro de `helper.rs`.
4. **Caminhos.** `default_captures_dir` usa `dirs::data_dir()/scrawl/captures` no Linux
   (`~/.local/share/scrawl/captures`); log em `dirs::state_dir()/scrawl`; exports em
   `dirs::video_dir()/Scrawl`. `SCRAWL_CAPTURES_DIR` continua valendo. No Mac nada muda.
5. **Texto no contact sheet.** `render.rs` tem fallback de largura chutada fora do Mac. No Linux:
   crate `ab_glyph` e a fonte do sistema achada por `fc-match -f '%{file}' 'sans:bold'` / `'sans'` (cache
   em `OnceLock`). Sem fonte achada, mantém o fallback atual.
6. **Área de transferência.** Stubs de `handoff.rs:185-195` passam a usar `wl-copy` (texto e
   `--type image/png`). Sem `wl-copy`, loga e segue.
7. **Navegador/`open`.** `routing.rs:169` (`osascript`) retorna nada no Linux; `open` vira `xdg-open`.

## Fase 2: gravar

Resultado: `bind = ALT SHIFT, S, exec, scrawl toggle` no `hyprland.conf` inicia e para uma gravação;
ao parar, o bundle é montado, transcrito, copiado (`wl-copy`) e entregue ao OpenCode pelo roteamento já
existente; `notify-send` avisa início, fim e erro.

### Componentes (todos em `src/linux/`)

- **`record.rs` — sessão de tela.** `ScreenSession::start(target, video_path)` lança
  `wf-recorder -o <monitor> -f recording.mp4 -c h264_nvenc` (cai para `libx264 -preset veryfast` se o
  encoder falhar no primeiro segundo), ou `-g "<x,y wxh>"` para janela. `stop()` manda SIGINT, espera
  sair, e devolve `ScreenRecording`: `video`, `duration_ms` pelo `ffprobe`, `video_offset_ms` = atraso
  medido entre o start e o primeiro frame (aprox. pelo tempo até o arquivo ganhar bytes), e `frames` =
  frames candidatos tirados do vídeo com `helper::frames(FrameTimes::Every(500ms), CANDIDATE_EDGE)`.
  O seletor de keyframes não muda: ele já trabalha sobre `ScreenRecording.frames`.
- **`hypr.rs` — janelas e cursor.** `WindowLogger` com a mesma interface do Mac
  (`start(frame, started_at, request)`, `stop() -> ActivityLog`): a cada 250 ms lê
  `hyprctl -j clients` + `activewindow` (vira `WindowSnapshot`/`WindowEntry`, deduplicado por
  `push_windows`) e `hyprctl -j cursorpos` (vira `CursorSample` por `push_cursor`).
  `discover_target(window)` usa `hyprctl -j monitors` (monitor focado) ou casa `--window` com título/classe
  em `clients`. Coordenadas: lógicas do Hyprland; escala do monitor vem de `monitors`.
- **`whisper.rs` — transcrição.** Novo `Engine::Whisper` em `transcribe.rs`, padrão no Linux. Roda
  `whisper-cli -m <modelo> -l auto -oj -ml 1 -sow -of <tmp> <audio.wav>` (wav já é gravado pelo `audio.rs`;
  reamostra para 16 kHz mono com `ffmpeg` se preciso) e converte cada segmento de uma palavra em
  `WordTiming { t0, t1, text }`, `estimated: false`. Modelo: `SCRAWL_WHISPER_MODEL`, senão
  `"whisper_model"` em `~/.config/scrawl/config.json`, senão
  `~/.local/share/scrawl/models/ggml-small.bin`. Binário: `whisper-cli`, senão `whisper-cpp`. Sem binário ou
  modelo: bundle sai sem transcrição e o log/notificação diz o que falta (mesmo comportamento de hoje sem
  áudio).
- **`toggle.rs` — liga/desliga.** `scrawl toggle`: se `$XDG_RUNTIME_DIR/scrawl/record.pid` aponta para um
  processo vivo, manda SIGINT e sai; senão inicia `scrawl record --until-stopped` desanexado (setsid) e
  grava o pid. `record --until-stopped` troca o `sleep(duration)` por espera até SIGINT/SIGTERM (handler
  com `libc` setando um `AtomicBool`); `--duration` continua existindo. Pid velho (processo morto) é
  ignorado e sobrescrito.
- **`record` em `main.rs`.** O braço `Command::Record { iphone: false }` passa a chamar
  `ScreenSession`/`WindowLogger` do sistema certo via `cfg`; o resto (`AudioRecorder`,
  `finalize_capture_bundle`) é o mesmo código. Depois do bundle, no Linux, copia para a área de
  transferência e chama a entrega existente (`handoff`/`delivery`), igual ao que o app do Mac faz ao parar.

### Fluxo

`⌥⇧S` → `scrawl toggle` → `scrawl record --until-stopped` → wf-recorder + cpal + hyprctl rodando →
`⌥⇧S` → SIGINT → para tudo → candidatos via ffmpeg → whisper → `finalize_capture_bundle` →
`wl-copy` + OpenCode → `notify-send`.

### Erros

- `wf-recorder` ausente ou sai na hora: aborta com mensagem clara e notificação; nada de bundle vazio.
- Sem mic (`cpal` falha): grava sem áudio, como hoje.
- Sem whisper/modelo: bundle sem transcrição + aviso com o comando para instalar.
- `hyprctl` falha (fora do Hyprland): grava o monitor inteiro sem log de janelas.
- `scrawl toggle` duplo rápido: o pidfile é escrito antes do recorder subir; o segundo toggle sempre para.

## Testes

- Os testes existentes dos módulos puros (`keyframes`, `sketch`, `render`, `routing`, `handoff`, `model`,
  `autocrop`, `agent_context`) precisam passar no Linux.
- Novos testes por parser, com saídas reais gravadas como fixture: `ffprobe` JSON → `Probe`; JSON do
  `whisper-cli -ml 1` → `TranscriptData`; `hyprctl -j clients`/`monitors`/`cursorpos` → snapshots e target;
  pidfile do toggle (vivo, morto, ausente).
- Fim a fim manual: `scrawl record --duration 8` e `scrawl toggle` duas vezes, depois abrir
  `contact-sheet.png` e `summary.md` do bundle (princípio 5 do `AGENTS.md`).
- Todo `cargo build`/`test` roda só depois de avisar, com `nice -n 19 ionice -c 3 cargo … -j 2`.

## Dependências novas

- Crate: `ab_glyph` (Linux). Nada mais.
- Sistema: `wf-recorder`, `ffmpeg`, `wl-clipboard`, `libnotify`, `whisper.cpp` (AUR `whisper.cpp` ou
  `whisper.cpp-cuda`) + modelo ggml multilíngue.
