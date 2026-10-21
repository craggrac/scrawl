# Phase 4 part 2: the Settings window on Linux (report)

Status: DONE, visual verification pending (see below). Branch `linux-port`.

## What was built

- `src/settings.rs` became `src/settings/mod.rs`. `Panel`, `View`, `Action`, `Effect` and `Host`
  are shared; the macOS-only parts (SMAppService, Carbon shortcuts and their validation,
  permissions, Hex transcriber, retention, command-line tool, the helper `Session`) are behind
  `cfg(target_os = "macos")` (attributes only, no code moved; the mac tests module is mac-only).
  One new mac-side function, `check` (the old `validate` call, with the "other shortcut" lookup
  moved into it). `onboarding.rs`'s source-grep test points at `settings/mod.rs`.
- `src/settings/linux.rs`: Linux `ShortcutKind` (Record only), `Shortcuts`, `Pressed` (GPUI key
  plus modifiers), `check` (reuses `shortcut_from_key_and_mods`), `shortcut_view`, launch at login
  as `$XDG_CONFIG_HOME/autostart/scrawl.desktop` running `<current exe> app` (no helper existed;
  `Exec` is quoted per the desktop-entry spec), `ParakeetView`, and the `AppHost`. 9 tests.
- `src/ui/settings_window.rs`: the GPUI window (class `scrawl-settings`, 640x480, same colours and
  buttons as the review window). Panes: General (launch at login, Record shortcut recorder with
  keycaps, reset, Esc cancels, error caption), Recording (Parakeet ready / what is missing),
  Storage (captures size, Show in Files), Agents (skill command and Copy), About (version, links).
  Esc and Super+W close it; a recorder that loses the window's focus or a window that closes
  while listening gives the shortcut back.
- Recording a shortcut pauses the listener: `SharedCaptureTelemetry::pause_shortcuts`; while
  paused neither the evdev listener nor `scrawl toggle` acts (`press_shortcut_from`). Tested.
- Opening: tray "Settings…", `settings` on the control socket, Super+, in Scrawl's windows.
  Hyprland rule `settings_rule` (float, centre, 640x480; legacy rules too) shares a
  `centered_rule` with the review window.
- `linux::parakeet::missing()` (the existing start-up checks, factored into `requirements()`).
- README, `docs/onboarding-and-settings.md` updated.

## Decisions to know about

- **No retention setting on Linux.** `retention::start()` (the cleanup worker) and `trash` are
  macOS-only, so nothing enforces it; the default policy would also trash captures after 7/30
  days the moment someone enabled the worker. The Storage pane says captures stay until deleted.
  Porting it (worker, `gio trash`, a `Recording` guard) is a separate, deliberate change.
- **No Permissions/dependency pane.** There is no existing check function for wf-recorder, ffmpeg,
  wl-copy and so on (only Parakeet's `server_binary`/`model_dir`, now surfaced on Recording), so
  the pane is hidden rather than invented.
- **No microphone popup.** `mic::inputs()` through cpal/ALSA returns about 25 entries here
  (null, rate converters, hw/plughw duplicates), which is not a usable picker. The shared
  `microphone` view and action still exist; Linux just doesn't render them.
- Paste shortcut, OpenCode toggle, video export, Check for Updates, Tour: not shown.
- The Hyprland bind (`scrawl toggle`) swallows the current combo before the window sees it, so
  re-recording the combo that is already the bind isn't possible; any other combo works.

## Verification

- `cargo test`: 420 passed (410 before; +9 settings, +1 telemetry pause; tray/control/hypr tests
  extended in place). `bash scrawl-macstub/mc.sh`: clean (bin and tests). `cargo check`: the same
  5 old warnings.
- Visual: an early run of a second instance (isolated runtime dir and config) rendered General,
  Recording, Storage, Agents, About and the error caption correctly (grabs of the window only, in
  `/tmp/scrawl-vis/`). The owner then asked for no more windows on the session, so the recorder
  listening state, `settings` on the socket and the tray item have NOT been exercised live, and
  nothing was clicked. Needs a check after restarting `scrawl app`.
- The running `scrawl app` (pid 698280) was never touched; it needs a restart to get the window.
