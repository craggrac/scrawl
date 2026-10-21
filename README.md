# Scrawl

Show an agent a problem instead of describing it.

Press a shortcut (`⌥⇧S`) and Scrawl starts recording the screen. Talk, click around, draw on it, and press `⌥⇧S` again to finish. Scrawl records the screen, your voice, and your marks, then packs them into a compact bundle: a contact sheet of annotated keyframes (`contact-sheet.png`), individual full-resolution keyframes (`frames/frame-XX.png`), a time-aligned transcript and interaction timeline (`summary.md`), and a structured manifest (`scrawl.json`).

## Install

Scrawl needs an Apple silicon Mac on macOS 26 or later. Download the DMG from
[scrawl.kitlangton.com](https://scrawl.kitlangton.com), or use Homebrew:

```sh
brew install --cask kitlangton/tap/scrawl
```

The app updates itself (Sparkle), so Homebrew leaves updates to it.

## Linux (Hyprland)

A Linux build records from a Hyprland key bind. It needs `wf-recorder`, `ffmpeg`, `wl-clipboard`,
`libnotify` and fontconfig, plus [OpenWhispr](https://openwhispr.com) with its Parakeet
(`parakeet-tdt-0.6b-v3`) model downloaded: Scrawl runs OpenWhispr's `sherpa-onnx-ws` server for
transcription (`SCRAWL_SHERPA_WS` and `SCRAWL_PARAKEET_DIR` point elsewhere).

```sh
cargo build --release
ln -s "$PWD/target/release/scrawl" ~/.local/bin/scrawl
```

Bind the shortcut in Hyprland. With a Lua config (`hyprland.lua`, Hyprland 0.53 and later):

```lua
hl.bind("ALT + SHIFT + S", hl.dsp.exec_cmd("scrawl toggle"))
```

or in a `hyprland.conf`:

```
bind = ALT SHIFT, S, exec, scrawl toggle
```

Press it to start and again to stop. The bundle lands in `~/.local/share/scrawl/captures/` and its
summary on the clipboard; `scrawl send latest --session ses_…` delivers it to OpenCode. Not yet on
Linux: iPhone capture.

### The app

`scrawl app` is the long-running app with the capsule, the review window and a tray icon. Start it
with Hyprland, in `autostart.lua`:

```lua
hl.on("hyprland.start", function ()
    hl.exec_cmd("scrawl app")
end)
```

(or `exec-once = scrawl app` in a `hyprland.conf`). While it runs, the same `scrawl toggle` bind
drives it instead of starting a separate recorder, and a second `scrawl app` exits with a message.
It listens on `$XDG_RUNTIME_DIR/scrawl/ctl.sock` (owner-only); `toggle`, `stop`, `cancel`, `review`,
`settings` and `quit` are its commands, one per line.

- **Shortcut.** With read access to `/dev/input` (the `input` group, below), the app also sees the
  shortcut itself. Tap it to record the screen, or keep Alt+Shift held after the S to open the
  mode switcher (each further S steps to the next mode, letting go records it), and Esc closes
  the switcher or the picker. A press seen
  both there and through the bind counts once, and a press that only came through the bind
  (`scrawl toggle`) is always a tap. The shortcut recorded in the review window is saved in
  `~/.config/scrawl/config.json` (`linuxShortcuts`) and is what the listener and the capsule
  use, but Scrawl doesn't edit Hyprland's binds: change the bind to the same keys (for
  `SUPER + R`, `hl.bind("SUPER + R", hl.dsp.exec_cmd("scrawl toggle"))`). The listener reads keyboards only, compares each
  key with the shortcut and Esc and keeps nothing else, and ignores both while the screen is
  locked (Hyprland's `locked` state, so nothing typed into hyprlock reaches Scrawl).
- **Capsule.** `wf-recorder` records the whole output, so the capsule leaves the screen about
  1.5 s into a recording (it never lands in the capture) and comes back for processing and the
  result. Finish a recording with the shortcut, `scrawl toggle` or the tray.
- **Settings.** The tray's Settings…, `settings` on the socket and Super+, in Scrawl's windows open
  a window with General (launch at login, which writes `~/.config/autostart/scrawl.desktop` to run
  `scrawl app`, and the Record shortcut), Recording (whether Parakeet is ready), Storage (the size of
  the captures folder; nothing deletes old captures on Linux, so there is no retention setting),
  Agents (the skill command) and About. The Mac-only settings (permissions, OpenCode, the Paste
  shortcut, the transcriber choice, video export) aren't there. While it listens for a new Record
  shortcut the current one is ignored, from the keys and from `scrawl toggle`; the window class is
  `scrawl-settings`.
- **Tray.** Record or Stop, Open Latest Capture, Settings and Quit Scrawl. It needs a StatusNotifierItem
  host on the bar (waybar's `tray` module, Noctalia's bar); without one the app runs without it.
- **Quitting.** Quit Scrawl, `quit` on the socket, SIGTERM, SIGINT and SIGHUP all quit the same
  way: a running recording is thrown away and its `wf-recorder` stopped, then the socket goes.
  SIGKILL can't be caught: it leaves `wf-recorder` running (stop it with `pkill wf-recorder`) and
  a stale socket, which the next `scrawl app` replaces.
- **Housekeeping.** gpui ends an app whose last window closes, so the app keeps an empty 1×1
  window on the hidden special workspace `special:scrawl`.

### Ink, pins and the picker's dim

Hold **⇧** during a recording and move the pointer to draw in red (`#ff3b30`); ⇧-click drops a
numbered pin (a press that moves under 12 pt and lets go within 1 s; longer is ink). Ink holds for
0.65 s after you stop and fades over the next 0.65 s; pins stay. The strokes and pins are in the bundle
(`scrawl.json`, the marks in `summary.md`, redrawn on the keyframes) as on macOS. ⇧ with Super draws
nothing, and a ⇧ tap on the spot (a capital letter) leaves no mark. Linux adds three tools, none of
which is on macOS:

| Keys | What it does |
|---|---|
| hold **⇧**, press **1 / 2 / 3 / 4** | the strokes from then on are red / yellow / green / blue (the tip dot at the pointer shows the colour; it goes back to red at the next recording) |
| hold **⇧ + Ctrl** and move | an arrow from where you pressed the keys to the pointer (head at the pointer) |
| hold **⇧ + Alt** and move | a box with the pointer and where you pressed the keys as opposite corners |
| **Ctrl+Z** | takes back the last stroke or pin, from the overlay and from the bundle (macOS: ⌘Z) |

Arrows and boxes fade like ink and take the picked colour. Changing the modifier or the colour
while drawing ends the stroke and starts a new one (release Ctrl to go back to freehand). The
bundle's `scrawl.json` gets `"color": "yellow"`, `"green"` or `"blue"` on a stroke (absent is red)
and `"shape": "arrow"` or `"box"` (a figure's `pts` are its two ends, a box's opposite corners); the
marks come out as `arrow` ("drew an arrow from … pointing to …") and `box` ("boxed region …"), with
" in yellow" and so on when not red. Typed text on the screen isn't built: it needs keyboard focus,
which the overlay deliberately never takes.

- **The ink is in the video.** It is drawn by a `wlr-layer-shell` surface on the overlay layer
  (namespace `scrawl-ink`, no input region: clicks and hover go to the windows under it), and
  `wf-recorder` records the composited output, so the video shows the ink while it holds and fades.
  macOS leaves its overlay out of the capture; `wf-recorder` can't exclude a layer. The keyframes
  are redrawn from the strokes as before, a little bolder than the video's ink, so they cover it
  rather than doubling it. Nothing else of Scrawl's is drawn into a recording: no dim, no glow.
- **Needs `scrawl app`.** The ink is tracked and drawn by the app. A headless `scrawl record` or
  `scrawl toggle` with no app running draws no ink, and records no strokes or pins: clicks, drags
  and scrolling are noted as before.
- **Needs `zwlr_layer_shell_v1`.** Hyprland, sway and the other wlroots-protocol compositors have
  it; GNOME's Mutter doesn't, and there the log says "Metal overlay failed" and recordings go on
  without ink. The overlay sits over the output holding the target's centre. It maps nothing until
  there is ink and makes no commit while idle (`wf-recorder` writes a frame per commit).
- **The pointer comes from Hyprland** (`cursorpos` on its socket, every 8 ms while ⇧ is held), since
  evdev has no position. Only ⇧, Ctrl, Alt and Super presses, the keys 1 to 4 and Z with those held, and the mouse buttons
  are read for ink, and no key is kept, as for clicks above. Needs the `input` group like clicks do.
- **Evdev only watches, so the app gets the keys too.** Ctrl+Z is Scrawl's undo during a recording
  and also the focused app's (this is what macOS does with ⌘Z, too: its tap doesn't swallow it);
  pressing ⇧ + 1 to 4 also types `!@#$` there, and Ctrl+Z only acts while a recording runs. Scrawl
  takes nothing off the keyboard, which is also why the figure keys are ⇧ + Ctrl and ⇧ + Alt held while
  you move, with no click: a click would reach the app as Ctrl+Shift-click (a link in a new window
  in browsers). The combinations were picked to be free in a stock Hyprland setup (the only
  ⇧ + Alt bind is the recording shortcut's own; check yours with `hyprctl binds -j`), but a few apps
  react to an Alt tap (Firefox shows its menu bar when Alt is let go).
- **The picker draws its own dim** (`TargetPickerWindow`): the screen dimmed, a cutout over the
  hovered window, the screen or the dragged area, with a blue tint and border for a window or screen
  and a thin white one for an area. Its Hyprland rule has `rounding = 0` so the screen's corners dim.
- **Fractional scaling isn't supported yet.** The ink overlay draws at the integer scale the
  compositor reports; on a monitor scaled by a fraction (1.25, 1.5) it assumes scale 1, logs it once
  ("the output is scaled by …") and the ink is a little soft but in the right place. The
  picker's dim and the capsule come from GPUI, which handles it itself. Untested with more than one
  output, and where the picker and capsule land with more than one monitor.

While recording, Scrawl notes clicks, drags, scrolling and Ctrl/Super shortcuts (for example
`Ctrl+L`) from the input devices, and the keyframes around them say "clicked ..." or "scrolled
down". It never records typing: plain keys, and Shift or Alt combinations, are not looked at.
This needs read access to `/dev/input/event*`, which on most distributions means being in the
`input` group (`sudo usermod -aG input $USER`, then log in again). Without it recording still
works, just without these events. Touchpad taps and two-finger scrolling are not seen (libinput
synthesizes them above evdev); physical buttons and mouse wheels are.

## Agents

Install the agent skill for Claude Code, OpenCode, Codex and other agents:

```sh
bunx skills add kitlangton/scrawl
```

The `scrawl` command is the binary inside Scrawl.app. The Homebrew cask links it onto your PATH, or click
**Install** beside Command-line tool in Settings › Agents, which links it into `/opt/homebrew/bin`
or `/usr/local/bin` when you can write there, else `~/.local/bin`. The link always runs the app's
current copy, so it updates with the app.

With the skill, an agent that gets a Scrawl paste reads the contact sheet and transcript, opens the
keyframes the narration points at, and uses the command to look closer: `scrawl zoom <bundle> --at
0:12` (or `--at "a phrase you said"`) makes a dense sheet of frames around a moment, `scrawl
frames` pulls a range, `scrawl send` queues a capture into an OpenCode session, and `scrawl export`
makes a shareable `.mp4`. `scrawl skill` prints the skill without installing anything.

## Usage

```sh
# Launch the GPUI menu-bar and review app (tap ⌥⇧S to record, hold it to choose what to record)
cargo run -- app

# Generate a realistic multi-frame demo bundle and print its agent summary
cargo run -- demo --out /tmp/demo.scrawl

# Open a bundle (or `latest`) in the native GPUI review window
cargo run -- preview /tmp/demo.scrawl

# Print the agent summary or JSON manifest for a bundle
cargo run -- inspect /tmp/demo.scrawl
cargo run -- inspect /tmp/demo.scrawl --json

# Extract frames in a time window for an agent to look closer
cargo run -- frames /tmp/demo.scrawl --from 0:02 --to 0:07

# Copy contact-sheet.png + summary.md to the clipboard or send to OpenCode V2
cargo run -- send /tmp/demo.scrawl --clipboard
cargo run -- send /tmp/demo.scrawl --session ses_...

# Which session a capture of a dev site would go to (without sending)
cargo run -- route --url http://localhost:5173/

# Capture a cabled iPhone for 8 seconds from the terminal (mirror window + native video)
cargo run -- record --iphone --duration 8
cargo run -- devices

# Build a bundle from a ScrawlKit in-app recording (newest pair in ~/Downloads by default)
cargo run -- import
cargo run -- import ~/Downloads/20261013T164000.mp4 --timeline ~/Downloads/20261013T164000.json

# Rebuild an existing bundle with the current transcriber and keyframe rules
cargo run -- reprocess latest --out /tmp/again.scrawl

# Tune auto-crop on a Screen capture, live
cargo run -- autocrop latest --serve --open

# Export a bundle as a shareable .mp4 (crop to the activity, ink burned in, audio; --zoom for auto-zoom)
cargo run -- export latest --zoom --out /tmp/demo.mp4

# Clean up old captures now (the app does it daily), or keep one forever
cargo run -- clean --dry-run
cargo run -- pin latest
```

## Welcome and permissions

On first launch Scrawl opens a welcome window with four screens: a welcome, how ⌥⇧S works, one
screen with every permission Scrawl needs, and a first capture. Each permission is a row with a
button that shows the macOS prompt (or, once asked, opens System Settings) and a check that appears
by itself when macOS grants it (the window re-reads the grants every 0.5 s). Screen Recording is
first and its button is filled in; after that the next row's button is. Grant them in any order;
Continue unlocks once all three are on, and the screen is skipped when they already are.

| Permission | Used for | When it's asked for |
|---|---|---|
| Screen & System Audio Recording | Recording the screen (ScreenCaptureKit) | onboarding, required |
| Microphone | Your narration | onboarding, required |
| Accessibility ("Device Control and Data Access" on macOS 27) | The key event tap (⌘Z, Esc, S in the switcher, ⌘-shortcuts in the window log), and `⌥⇧V` and the Paste chip, which post ⌘V into the app in front | onboarding, required |
| Camera | The cabled iPhone mirror (macOS treats it as a camera) | the first time you pick iPhone |

Scrawl never asks for Input Monitoring. Clicks, drags and scrolls in other apps come from an AppKit
global mouse monitor (`NSEvent.addGlobalMonitorForEvents`), which needs no permission; ⇧ for ink
and the switcher's held ⌥⇧ come from polled modifier flags, which need none either; key presses
come from an active event tap, which Accessibility allows. macOS applies an Accessibility grant to
a running app at once (System Settings shows no "Quit & Reopen" for it), and Scrawl makes the tap
within half a second of it.

Screen Recording and Accessibility are lists you drag an app into, so their button skips Apple's
prompt: it opens the pane in System Settings, and a card flies from the button to just below the
Settings window (or, with no room on screen there, over the window's empty lower area), with its
Scrawl row lined up under the list. Drag that row into the list. The card follows the window,
steps aside when another app is in front, and shows "Scrawl is allowed" once a fresh process sees
the grant (a drop alone proves nothing). The row drags the `Scrawl.app` that contains the helper,
the bundle macOS grants. With Reduce Motion on, the card fades in place. Microphone and Camera
can't be dragged in, so they use the macOS prompt.

Camera is asked for in the same window, with just its row, when first needed. Speech recognition
isn't asked for: SpeechAnalyzer transcribes without it. The ⌥⇧S and ⌥⇧V hotkeys themselves need
no permission (Carbon `RegisterEventHotKey`).

Until the required permissions are on, ⌥⇧S starts nothing (no switcher, picker or overlay that
could sit over a macOS prompt): once the keys are released it brings the permissions window
forward.

A Screen Recording grant may not reach a running app, and macOS offers **Quit & Reopen** or
**Later** as soon as it's granted. Either way nobody reopens Scrawl by hand. If you pick **Quit &
Reopen**, macOS restarts Scrawl and the tour opens on the permissions screen again, Screen
Recording checked. If you pick **Later** (or nothing), Scrawl checks whether it can capture anyway
(a one-frame ScreenCaptureKit screenshot, tried only once a fresh helper reports the grant), and if
it can't, the row shows "Reopening Scrawl" for a second and Scrawl relaunches itself into the same
screen. It only does that once in two minutes (`relaunchedAt` in the config); if a fresh copy
still can't capture, the row's button becomes **Reopen Scrawl**. The relaunch waits for this copy
to exit and opens the app without `-n`, so if macOS's Quit & Reopen got there first there's still
one Scrawl. `scrawl sck-probe [--watch SECONDS]` and `scrawl-helper sck-probe` try the same
capture from the command line.

Progress is `"onboarding"` in `~/.config/scrawl/config.json`, including the screen the tour is on,
so it picks up there whatever quits Scrawl. Once seen, the tour only comes back from **Welcome
tour** in Settings › About, but a required permission that was granted and is gone again reopens the
permissions screen at launch. `SCRAWL_NO_ONBOARDING=1` skips it.

To replay it from scratch:

```sh
tccutil reset All dev.kitlangton.scrawl
jq 'del(.onboarding)' ~/.config/scrawl/config.json > /tmp/c.json && mv /tmp/c.json ~/.config/scrawl/config.json
# then quit and reopen Scrawl
```

Preview every screen without the app, including the permissions screen with none, some and all
granted and each on-demand sheet: `target/debug/scrawl-helper onboarding --render /tmp/onboarding
--icon assets/app-icon.png`.

## Settings

**Settings…** in the menu bar (⌘, while the menu is open) opens a small window with six panes:

- **General**: Launch at login, Send dev-site captures to OpenCode, and the two shortcuts, Record
  (`⌥⇧S`) and Paste last capture (`⌥⇧V`).
- **Recording**: the microphone, with its input level, the transcriber (Apple Speech, or Hex while
  it runs, with its model) and, under Experimental, Video export.
- **Storage**: how long recordings are kept, how much space the captures take, and Show in Finder.
- **Permissions**: the onboarding checklist, so a permission can be turned on later.
- **Agents**: Install or Uninstall the `scrawl` command (see [Agents](#agents)), and the skill's
  install command with Copy. A `scrawl` that belongs to another program is left alone.
- **About**: the version, Check for Updates…, links, and the welcome tour.

To change a shortcut, click its keys and press the new combination; `Esc` cancels. It needs ⌘, ⌥
or ⌃ (a function key can stand alone), and Scrawl turns down ⌘ with one key, combinations macOS
uses, the other Scrawl shortcut, and anything another app has already taken. The arrow beside the
heading puts the default back. While the recorder listens, Scrawl's own shortcuts are paused. They
are saved as `"shortcuts"` in `~/.config/scrawl/config.json`. The menu keeps Record Screen, Choose
What to Record…, Open Latest Capture, Export Last Capture as Video… (when on), Settings…, Check
for Updates… and Quit.

**Microphone** lists the connected inputs, with System default first, which follows whatever macOS
uses as it changes. The list updates as devices are plugged in or out. A chosen device is saved by
its CoreAudio UID as `"microphone": {"id", "name"}`; if it isn't connected when you record, Scrawl
records from the default instead and says so under the popup (and once in the log). The level
meter runs only while the Recording pane is on screen and Microphone is allowed. Cabled-iPhone
narration uses the same microphone. From the terminal, `scrawl mic list` shows the inputs and the
one recordings will use, `scrawl mic set <id|default>` chooses, and `scrawl mic watch` prints the
list again on every change.

Preview every pane, dark and light, with the recorder listening and a combination turned down:
`target/debug/scrawl-helper settings --render /tmp/settings --icon assets/app-icon.png`.

## Capture Controls

- **Tap `⌥⇧S`** (release within 0.25 s): record the display under the pointer straight away,
  auto-cropped (see below). Tap it again to finish; a tap within 0.4 s of the start is ignored, so a
  double press doesn't leave an empty capture. `⌃⌥⇧S` also works (only while `⌥⇧S` is the
  shortcut; see [Settings](#settings) to change it). As it starts, the island opens onto the modes (Area · Window ·
  **Screen** · iPhone when one is cabled) for about 3.5 s, or until you click, scroll or draw;
  hovering keeps them. Click Window or Area to retarget (the first seconds are dropped and the
  picker takes over: click a window or drag an area), iPhone to record the phone instead, or just
  carry on.
- **Hold `⌥⇧` after `S`**, like `⌘Tab`: after 0.25 s the switcher appears on **Screen**. Each
  further `S` steps Screen → Window → iPhone → Screen; Window highlights the window under the
  pointer. Dragging with the mouse selects an area instead. Release `⌥⇧` to record what's
  selected; `Esc` cancels. With no iPhone cabled, iPhone says so and releasing records nothing.
- **Choose What to Record…** in the menu bar: the full picker (Area, Window, Screen, iPhone).
- `⌥⇧V`: Paste both `contact-sheet.png` and `summary.md` into the active app or terminal (`⌃⌥⇧V` also works while it's the default)
- **Normal pointer**: Clicks and scrolls pass straight through to the app underneath
- **Hold `⇧` (Shift) + move**: Draw a persistent red marker stroke (`#ff3b30`) on screen that fades out after release
- **`⇧`-click**: Drop a numbered pin where you click (a press that moves under 12 pt and lets go within 1 s; longer is ink)
- **`⌘Z`**: Undo the last stroke
- **`Esc`**: Cancel the active selection or capture

While picking a target, the island shows the modes **Area**, **Window**, **Screen**, and **iPhone**
(when one is cabled). It opens on the mode you used last:

- `1`–`4` jump to a mode; `Tab`/`→` and `⇧Tab`/`←` step through them; `Space` toggles Area and Window
- Area: drag a region. Window: click a window. Screen and iPhone: `⏎`, `S`, `⌥⇧S`, **Record**, or
  clicking the selected mode again starts recording (`⏎`/`⌥⇧S` record the full screen from Area and
  Window too)

## Sending captures of a dev site to its session

A capture that ends with a browser in front showing a project's dev site goes straight to that
project's OpenCode session, the same way iPhone recordings do. It is still copied to the clipboard.
The island says "Sending to SCRAWL…", then "Sent to SCRAWL", or "Copied · couldn't reach SCRAWL"
when OpenCode didn't take it (the app retries it, as for iPhone recordings). Window captures use the
captured window's app instead of the one in front. Brave, Chrome, Arc, Edge, Vivaldi, Chromium, and
Safari work; macOS asks once whether Scrawl may control the browser, so it can read the tab's
address. If you decline, captures stay on the clipboard.

Scrawl picks the session in this order:

1. **The page registered itself.** Add one dev-only line to the page:

   ```html
   <script src="http://127.0.0.1:47777/sdk.js" data-session="ses_…"></script>
   ```

   or `data-directory="/abs/path/to/project"` to use that project's most recently active session.
   The script comes from Scrawl itself. It registers the tab, sends a heartbeat every 30 s while the
   tab is visible, and unregisters when the tab closes (a registration lasts 2 minutes after its
   last heartbeat). With Vite, inject it only in dev:

   ```ts
   // vite.config.ts
   plugins: [{
     name: "scrawl",
     apply: "serve",
     transformIndexHtml: () => [{
       tag: "script",
       attrs: { src: "http://127.0.0.1:47777/sdk.js", "data-directory": process.cwd() },
       injectTo: "head",
     }],
   }]
   ```

   The script does nothing unless the page is on `localhost`, `*.localhost`, `127.x`, or `[::1]`.
   Scrawl accepts a registration without a token only from a loopback origin calling from this Mac,
   and only for that origin. A page anywhere else (a `*.ts.net` dev host, say) must also set
   `data-token` to the ingest token (`ingestToken` in `~/.config/scrawl/config.json`).
2. **Zero setup for localhost.** If the tab is on a loopback host and no page registered, Scrawl
   finds the process listening on that port (`lsof`), its working directory and that directory's
   git root, and sends to the most recently active open root session in the repository (any folder
   in it). Outside a git repository only a session in that exact folder counts.
3. Otherwise the capture is copied to the clipboard only, as before.

The destination is recorded in `scrawl.json` (`destination: {session, title, via: "sdk" |
"localhost", url, directory}`) and on the second line of `summary.md`
(`For session: SCRAWL (ses_…) · page http://localhost:5173/`). `scrawl send` without `--session`
sends to it again, and refuses a different `--session` unless you pass `--reroute`.

To see where a capture of a URL would go, without sending anything:

```sh
scrawl route --url http://localhost:5173/
scrawl route --browser com.brave.Browser   # the browser's front tab
```

Turn it off with **Send dev-site captures to OpenCode** in Settings › General, or `"autoSend": false` in
`~/.config/scrawl/config.json`. `SCRAWL_NO_OPENCODE=1` also turns it off.

## Auto-crop (Screen captures)

Instead of picking an area up front, record the whole screen: tap `⌥⇧S`, or choose **Screen**.
Area and Window captures aren't cropped.
Scrawl works out where you were gesturing and crops each keyframe to it. While a Screen capture
records, Scrawl logs:

- **`windows`** in `scrawl.json`: the on-screen windows, front to back (id, app, bundle ID, title,
  bounds in capture points, layer, frontmost), including the menu bar and the Dock's strip. A
  snapshot is taken at the start, when a click, ink stroke or shortcut starts, when the frontmost
  app changes, at most every 500 ms while the cursor moves and every 2 s otherwise, and stored only
  when the list changed. Each snapshot costs about 0.5 ms. Titles stay in `scrawl.json`; they never
  go into `summary.md` or the clipboard.
- **`cursor`**: the cursor at up to 15 Hz while it moves, as `[t, x, y]` (normalized), so dwell
  counts.

When the capture finishes, each keyframe gets the slice of the timeline halfway to its neighbours,
and `src/autocrop.rs` weighs what happened in it: ink, clicks, drags and scrolls first, then where
the cursor rested and what changed on screen (a cursor wandering far from the ink is ignored). The
crop is then:

- **the window** holding the activity (two at most, for an arrow from one to the other), including
  a menu it opened and the touched part of the menu bar;
- or, when that window is most of the screen (maximized, full screen), **the activity inside it**
  with some context, its edges moved into nearby white space and past the ends of any lines of
  text they cross (up to the window's edge), so they don't cut words in half;
- or the previous crop, for a keyframe with only talk.

Neighbouring crops that mostly overlap become one, so the sheet doesn't jitter. A crop covering
most of the screen isn't worth it, and the keyframe stays whole.

The bundle then has:

- `frames/frame-XX.png` and `contact-sheet.png`: cropped from the full-resolution frames (Screen
  keyframes are decoded from the 2560 px wide recording), with the ink and badges redrawn at the crop's scale;
- `frames-full/frame-XX.png` and `contact-sheet-full.png`: the uncropped versions;
- one line in `summary.md`, e.g. `Cropped to: Ghostty + Safari (full frames in frames-full/)`;
- `autocrop` in `scrawl.json`: each keyframe's crop in points, its kind and reason, and the
  changed regions that fed it.

`scrawl zoom` crops its cells like the nearest keyframe (`--full` for the whole screen).
`scrawl reprocess <bundle> --out <dir>` re-runs it on an existing Screen capture from its video.
To turn it off, or change a setting, use `~/.config/scrawl/config.json`:

```json
{ "autocrop": false }
{ "autocrop": { "padding": 24, "minW": 480 } }
```

### Tuning

`scrawl autocrop <bundle|latest> --serve --open` opens a playground that runs the same Rust crop as
you move its sliders: each keyframe with its windows, activity, cursor heat, outliers and crop, the
cropped frame, and the cropped contact sheet. **Copy settings** gives the JSON for the config.
Without `--serve` it writes `autocrop/index.html` with the presets only.

The settings were tuned on simulated captures with hand-labelled ideal crops: one maximized
terminal, two windows side by side, an arrow from one window to another, a tiny badge in a huge
window, a menu-bar menu, talk-only keyframes, a dragged window, overlapping windows, thirty seconds
hopping between three windows, a stray cursor, a full-screen app, and a scrolling page.
`scripts/autocrop-corpus.mjs` renders them with Playwright (from `site/node_modules`), `scrawl
fixture` builds each into a real bundle through the capture pipeline, `scrawl autocrop-eval` scores
the crops (coverage of the ink and clicks, overlap with the ideal, context, legibility in the sheet,
stability; `--sweep` searches the settings), and `scripts/autocrop-gallery.mjs` renders a gallery to
judge them by eye:

```sh
bun scripts/autocrop-corpus.mjs /tmp/corpus
for s in /tmp/corpus/*/scenario.json; do
  scrawl fixture "$s" --out "/tmp/bundles/$(basename "$(dirname "$s")").scrawl"
done
scrawl autocrop-eval /tmp/bundles --json /tmp/scores.json   # add --sweep to search
bun scripts/autocrop-gallery.mjs /tmp/gallery /tmp/bundles --scores /tmp/scores.json
```

`autocrop-eval --export src/autocrop-corpus.json` refreshes the corpus the unit tests score.

## iPhone

Plug an iPhone in with a cable (unlocked, trusted) and the picker shows **iPhone** next to Area,
Window, and Screen. Choosing it opens a floating mirror of the
phone on the right of the screen and starts recording:

- The phone's own video is recorded natively (1320×2868 at up to 60 fps on an iPhone Pro Max) with
  your narration from the Mac's microphone, into the bundle as `recording.mov`. Keyframes are
  decoded from that video, never from a screenshot of the mirror. A minute after the capture the
  video is re-encoded to HEVC at the keyframes' 1600 px (see [Keeping captures small](#keeping-captures-small)).
- The laser, ink, pins, and clicks work over the mirror exactly as over a window. The mirror's
  on-screen rect is the capture target, so everything you draw is stored in phone-screen
  coordinates and lands in the right place on the phone keyframes. Move or resize the mirror
  freely; the mapping follows it.
- Press **M** with the mirror focused to flag a moment ("look here"); flags always get a keyframe
  and show as `⚑` in the summary.
- Finish from the island as usual. Unplugging the phone finishes the capture with what was recorded.

Phone bundles are ordinary bundles: `scrawl.json` adds `"source": "iphone"` and a `device` object
(name, capture `mirror` or `scrawlkit`, id, native size, screen points and scale, app and version
for SDK recordings), and the target's `mode` is `device`. Keyframes fit a 1600 px long edge
(736×1600 for a 1320×2868 phone); `scrawl frames` and `scrawl zoom` extract more frames from the
video when you need them.

### ScrawlKit (iOS)

`ScrawlKit` (`Sources/ScrawlKit`) is a Swift package (iOS 18+) for recording inside your own iOS app:
video of the app with narration, touches and freehand red strokes, markers, and app events on the
same clock, sent straight to Scrawl on your Mac.

Add this repo as a Swift package dependency (product `ScrawlKit`) and attach `.scrawl()` to your root
view:

```swift
import ScrawlKit

WindowGroup { ContentView().scrawl() }   // three-finger long press starts and stops
Scrawl.event("Done tapped")              // optional: what the app did, on the timeline
```

The host app needs `NSMicrophoneUsageDescription` in its Info.plist for narration (both capture
methods use the microphone). Set `Scrawl.shared.narration = false` to record without it.

`Scrawl.shared.captureMethod` picks how the video is made:

- **`.appFrames`** (default): ScrawlKit draws the app's own windows into an H.264 `.mp4` at 30 fps
  and 2× (880×1912 on a 440×956-point phone), with the microphone recorded alongside. There is no
  screen-recording prompt; the microphone asks once and iOS remembers the answer. If it's denied, the
  recording is video only and the timeline says `"audio": false`. The strip and ink are left out of
  the video (Scrawl on the Mac draws touches and strokes from the timeline), but so is
  anything the app doesn't draw itself: system alerts and permission prompts, and some layers (video
  players, camera previews, some Metal or backdrop effects) may be blank or look different. The
  system keyboard is drawn by another process, so its keys can't be captured: a plain panel labelled
  "Keyboard" stands in for it, following it as it slides (taps on the keyboard never reach the app,
  so they aren't on the timeline either). Sending the app to the background finishes and sends the
  recording.
- **`.replayKit`**: ReplayKit's in-app screen recording, the exact pixels on screen with the strip
  and the keyboard in them. iOS asks "Record screen and microphone?" unless you allowed it in the
  last ~8 minutes, and again after every reinstall.

The timeline records which was used (`"capture": "appFrames"` or `"replayKit"`). With either, the
context records the keyboard: `keyboard: visible` if it was up at the start, and
`keyboard → visible` / `keyboard → hidden` when it comes and goes.

On paired devices, the three-finger trigger turns off iOS's three-finger text-editing gestures in
the app (UIKit's `editingInteractionConfiguration` answers `.none` for the app's windows), because
the same press would otherwise open the Undo · Cut · Copy · Redo bar when a text field is focused.
The cost is that three-finger undo, redo, cut, copy, and paste don't work in that app on a paired
phone. Set `Scrawl.shared.suppressesSystemEditingGestures = false` to keep them. Unpaired devices
are unaffected.

While recording, a slim strip sits on the right edge (drag its dot to move it up or down or over to
the left edge; it remembers where you left it). From top to bottom: the recording dot (hold it to see
the elapsed time), Draw (tap to turn drawing red strokes on or off), Discard (with a few seconds to
Undo), and Send. Doing the three-finger trigger again also sends. The strip moves like liquid (the
"Liquid morph" variant of `site/public/prototype/strip`): it stretches out of a droplet, pours into
the button you tap to become a status circle, buds the "Sent to …", "Discarded · Undo" or "Mac didn't
answer · Share" bubble off on a neck, and stretches as you throw it. On iOS 26 it's Liquid Glass and
the neck is the glass itself blending; Reduce Motion cross-fades instead.

Drawn strokes are smooth and fade on their own:

- **Sampling.** Every coalesced touch is a sample, at the touch's own timestamp, so a busy main
  thread doesn't make strokes jagged. A one-euro filter (one cutoff from the 2D speed) takes out
  jitter as they come in, and a centered smoothing takes out the rest behind the tip without lag.
- **Look.** The line is a centripetal Catmull-Rom curve, 4.6 pt wide when drawn slowly and down to
  55% when fast (from speed, never pressure, like the Mac's ink), with tapered ends.
- **Drawing.** Finished stretches go into layers that never change, so each touch redraws only the
  live tip, which runs ahead through iOS's predicted touches. Predicted points are dropped on the
  next touch and never recorded.
- **Fading.** A stroke fades out (300 ms, ease-out) 1.5 s after the pen last lifted. Putting the pen
  down again before then holds every stroke on screen, so a circle and then an arrow fade together.
- **Timeline.** It gets the smoothed, unpredicted points, plus `fadeAt` (when the stroke began
  fading) on each stroke that faded during the recording. On keyframes the Mac draws a stroke from
  when it began until 1.2 s after it ended, or with the rest of its mark.

Touches aren't drawn on screen in either capture method. Each touch goes on the timeline (position,
phase, and the label of the element under it) and Scrawl on the Mac draws taps and swipes from it.

ScrawlKit stays completely inert on devices that haven't been configured (no gesture recognizers,
overlay window, network calls, files, or background `URLSession`). To pair your iPhone with this Mac, print a setup link (and
terminal QR code) and open it on the phone:

```sh
scrawl link --scheme updown [--session ses_…]
# updown://scrawl?url=http%3A%2F%2F100.64.0.12%3A47777&url=http%3A%2F%2F192.168.1.20%3A47777&token=…
```

`scrawl link` reads (or generates with mode `0600`) `ingestToken` in
`~/.config/scrawl/config.json` and defaults to this Mac's Tailscale and LAN IPs on port `47777`.
Opening `<scheme>://scrawl?reset=1` on the phone clears its Keychain config (`dev.kitlangton.scrawl`).
For cabled USB fallback via `devicectl` (`Documents/Scrawl`), list the app bundle IDs in
`~/.config/scrawl/config.json` under `"usbApps": ["com.kitlangton.UpDown"]`.

The Mac answers an upload once it has built the bundle and queued it into the OpenCode session (or
after up to 45 s), so the strip says what happened:

| On the Mac | Reply | The strip |
| --- | --- | --- |
| Queued into the session | `delivered: true, state: "sent"` | Sent to UP DOWN |
| Bundle saved, OpenCode didn't take it (retried from the Mac) | `delivered: false, state: "queued"` | On your Mac · Sends to UP DOWN when it can |
| Still building or sending when the wait ran out | `delivered: false, state: "processing"` | On your Mac · Sending to UP DOWN… |
| Bundle saved, no session for this app (or routing off) | `delivered: false, state: "saved"` | Saved on your Mac |
| Bundle couldn't be built | HTTP 500 | Mac didn't answer · Share |

A queued bundle gets `.deliver.json` and the Mac retries it every 15 s, backing off to 30 min, for
a day (then it's renamed `.deliver-failed.json`; both are logged to `~/Library/Logs/Scrawl/scrawl.log`).
The phone doesn't upload it again. Over USB the Mac writes the same reply to `<stamp>.delivery`
before `<stamp>.sent`. Replies without `delivered` (Macs from before this) read as sent.

You can also AirDrop the `.mp4` and `.json` pair to the Mac and run `scrawl import`:

- Touches become taps and swipes, drawn on keyframes as ripples and finger trails and listed in the
  review timeline. Each tap or swipe is a keyframe candidate, like a click.
- Markers become flags.
- App events (`Scrawl.event("Done tapped")`) appear in `summary.md` under the keyframe that was on
  screen when they happened (`  · 0:12 app: Done tapped`).

#### Destination and recovery

The original `timeline.json` stores the paired destination in its top-level `session` field.
`context.session`, when the app supplies it, describes the session being viewed and does not choose
the recipient. For example, recording OpenCode Settings while viewing another project's session
can still send to the paired OpenCode iOS coordinator. Automatic Mac ingest resolves the top-level
destination first; without one, it uses the app/session fallback in `resolve_opencode_session`.

When recovering a failed delivery, read that original top-level field and check whether the capture
already arrived before sending again. `scrawl send --session` explicitly overrides the recipient;
do not fill it from `context.session`. If there is no recorded destination and ownership is unclear,
ask before forwarding. Keep private-project endpoints and recipients unchanged.

#### Sending to an HTTPS endpoint

A setup link can point at a hosted endpoint instead of a Mac, for phones that aren't on your LAN or
tailnet (for example `ledger://scrawl?url=https%3A%2F%2Fproblems.example.com%2Fapi%2Fscrawl%2Fledger&token=pbcap_…`).
It speaks the same protocol as the Mac: `GET {url}/health` and `POST {url}/ingest` with a bearer
token, where the body is a 4-byte big-endian timeline length, the timeline JSON, then the MP4. When
any configured URL is `https`, recordings go through a durable outbox instead of the LAN path
(plain `http` URLs keep the direct POST plus USB fallback):

- **Saved before sending.** On Send, the framed payload and a small metadata file (target,
  `recordedAt`, attempts, last error) are written to `Application Support/ScrawlOutbox`. They're
  protected until the phone's first unlock, so uploads can run while it's locked, and excluded from
  backup. The recording never goes to `Documents/Scrawl` (it's staged in `tmp`) and isn't added to
  `Scrawl.shared.recordings`. An entry is deleted only after a 2xx.
- **Background upload.** Uploads are file uploads on a background `URLSession`
  (`dev.kitlangton.scrawl.outbox.<bundle ID>`), so they survive the app being suspended or killed
  and never load the movie into memory. The session is recreated whenever the app is configured, and
  it picks up uploads still in flight from an earlier launch.
- **Retries.** A 401 or 403 (the link was revoked), a 413 (too large) and any other 4xx except 408
  and 429 (the server rejected these exact bytes) are permanent: the entry stays on disk, marked
  failed, with Share. Anything else (offline, timeouts, 408, 429, 5xx) is retried
  with backoff (15 s, doubling up to 30 min), and immediately when the network comes back, the app
  comes to the foreground, or it launches. An entry never uploads twice at once. The server dedupes
  on person and `recordedAt`, so resending after a lost reply is safe. Opening a new setup link
  retries the entries that failed with the old one.
- **Strip.** While the upload runs on a working network (`NWPathMonitor` says `.satisfied`), the
  strip stays up as a small status circle with a progress ring, saying "Sending…" however long it
  takes. Touches on it reach the app. With no bytes going out for 30 seconds it says "Still
  sending…", never "Queued". When the upload lands it turns into "Sent to …" with a success haptic.
  "Queued · sends when online" (with a success haptic, since the recording is safe on the phone) is
  only for no network or a failed attempt waiting to retry; it leaves after a few seconds, and if a
  retry starts while it's up it goes back to "Sending…". If the upload lands after the strip has
  gone (a new recording took over, it was queued, or the app was in the background), the strip comes
  back briefly with "Sent to …", as soon as the app is in front. Permanent failures say "Scrawl link
  expired · Ask Kit for a new one", "Too large to send" or "Couldn't send", with Share, which shares
  copies of the recording and leaves the entry queued.
- **Revoked links.** The `/health` check at the start of a recording also goes to the HTTPS
  endpoint. A 401 there shows "Scrawl link expired" beside the strip, but the recording carries on
  and is kept in the outbox, marked failed, until the link is renewed.

The endpoint rejects bodies over 95 MB, so HTTPS recordings are smaller and capped:

| Capture | Video | Audio | Per second | Cap | At the cap |
| --- | --- | --- | --- | --- | --- |
| `.appFrames` to a Mac | 6 Mbps | 96 kbps | ≈ 0.76 MB | none | — |
| `.appFrames` to HTTPS | 3 Mbps | 96 kbps | ≈ 0.39 MB | 3:00 | ≈ 70 MB |
| `.replayKit` to HTTPS | not settable (est. 8–12 Mbps) | | ≈ 1–1.5 MB | 0:45 | ≈ 45–68 MB |

3 Mbps at 880×1912 and 30 fps is about 0.06 bits per pixel. That's plenty for app UI, which is mostly
static between scrolls and transitions, and the 25 MB of headroom covers the encoder overshooting its
average. Ten seconds before the cap, the strip shows "Length limit · Sending in 10 s" with a warning
haptic, then sends. A payload that's still over the limit is never uploaded: it's marked "Too large
to send" and kept for Share.

Apps with an app delegate can optionally forward background session events, so iOS can relaunch a
killed app to finish reporting uploads. Without this, finished uploads are picked up at the next
launch:

```swift
func application(_: UIApplication, handleEventsForBackgroundURLSession identifier: String,
                 completionHandler: @escaping () -> Void) {
  Scrawl.handleBackgroundURLSessionEvents(identifier: identifier, completionHandler: completionHandler)
}
```

#### App context

The app can say which screen it's on and add its own key/values, so the agent doesn't have to guess
from pixels. ScrawlKit sends only what the app passes in, plus its bundle ID, name, version, and build.

```swift
SettingsView().scrawlScreen("Settings › Experimental")   // while visible; restores the previous on disappear
SessionView().scrawlContext("session", session.id)       // follows the value; nil leaves it to enclosing views
Scrawl.screen("Onboarding")                              // or set either one directly (nil clears)
Scrawl.context("server", "studio")
```

The most recently appeared screen that is still visible wins, so pushed screens, sheets, and tabs
work. The timeline gets `shortVersion`, `context` (values at the start), `contextChanges`
(`[{"t", "key", "value"}]`, `null` when cleared), and `screens` (`[{"t", "name"}]`, the first at `0`).
The bundle keeps them as `context`, `contextChanges`, and `screens` spans (`{t0, t1, name}`), and
`summary.md` (and the prompt queued into OpenCode) shows them:

```
[scrawl 0:25 · iPhone · OpenCode 1.4 (354) · /…/20261014-101500.scrawl]
Screen: Sessions · server: studio · session: ses_3f…
- #1 (0:01) “So here.”
- #3 (0:08) ⟨Settings › Experimental⟩ [tapped “Back”] “Go back.”
  · 0:09 server → mini
```

### Headless ingest server (`scrawl serve`)

On an always-on Mac (such as a Mac Studio reachable over Tailscale), run `scrawl serve` to accept
authenticated ScrawlKit uploads, build `.scrawl` bundles, and route them straight into local
OpenCode sessions without running the GUI app:

```sh
scrawl serve [--port 47777] [--usb]
```

To run `scrawl serve` under `launchd`, copy `scripts/scrawl-serve.plist` into `~/Library/LaunchAgents`
and load it:

```sh
cp scripts/scrawl-serve.plist ~/Library/LaunchAgents/dev.kitlangton.scrawl.serve.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.kitlangton.scrawl.serve.plist
```

## Export as video

`scrawl export` turns a bundle into one shareable `.mp4`: H.264 (`--hevc` for HEVC) with AAC audio in
sync, the red ink, pins, laser and click ripples burned in at the times they happened, cropped to
where the activity was. On macOS it runs in `scrawl-helper` on AVFoundation (hardware decode and
encode, Core Image for the crop, Core Graphics for the ink), so nothing is decoded into Rust and
ffmpeg isn't needed. On Linux it runs on `ffmpeg` ([below](#export-on-linux)).

```sh
scrawl export latest                              # ~/Movies/Scrawl/<bundle>.mp4
scrawl export latest --out demo.mp4 --zoom        # with Screen Studio-style auto-zoom
scrawl export latest --crop full --size 1280      # whole capture, 1280 px long edge
scrawl export latest --crop 0.1,0.2,0.6,0.5       # x,y,w,h as fractions, or pixels of the video
```

- **Crop** (`--crop auto|full|x,y,w,h`): `auto`, the default, is auto-crop's whole-recording crop for
  Screen captures and everything for window, area and iPhone captures. The output keeps the crop's
  aspect with even sides, is capped at `--size` (1920) on its long edge, and is never scaled up.
- **Zoom** (`--zoom`): the camera eases to ~1.5–1.8× toward clicks, drags, marks and laser points,
  holds while activity stays nearby (2.5 s after the last), follows the cursor once it leaves a
  central safe zone, and eases back out. It never starts a zoom while the cursor is travelling fast.
  The path is a pure function of the bundle (`src/export/camera.rs`, critically damped springs,
  no bounce); the helper only renders each frame's rectangle.
- **Audio**: `audio.wav` starts where the video does (`videoOffsetMs`); a cabled iPhone's movie
  uses its own audio track. `--no-ink` and `--no-clicks` leave the annotations out, `--fps` sets the
  rate (default 60 for a 60 fps recording).
- **Menu**: with **Video export** on in Settings › Recording (`"videoExport": true` in
  `~/.config/scrawl/config.json`, default off), the menu shows **Export Last Capture as Video…**. It saves to `~/Movies/Scrawl/<name>.mp4`, shows progress
  beside the menu bar icon, and reveals the file in Finder. The command works without the flag.

### Export on Linux

`scrawl export` takes the same flags and makes the same picture (the crop and the zoom camera are
computed by the same code), rendered with `ffmpeg` and `ffprobe` (both already needed): H.264 with
`libx264` (`--hevc`: `libx265`, tagged `hvc1`) and AAC, `audio.wav` starting where the video does
(`videoOffsetMs`). `--crop`, `--size` (long edge, even sides, never scaled up), `--fps` and `--zoom`
all work. The output defaults to `$(xdg-user-dir VIDEOS)/Scrawl/<bundle>.mp4`, `~/Videos/Scrawl` when
there is no Videos folder.

- **Ink is already in the video.** The overlay draws it on the screen that `wf-recorder` records, so
  `recording.mp4` has it and the export does not draw it again. That also means `--no-ink` cannot take
  it out (it prints a note and exports the same); the ink is only left out of a recording made
  without the app, which had none. The auto-zoom still moves toward the marks.
- **Click ripples** are drawn by the export, at the times of the clicks, in the cropped and zoomed
  picture; `--no-clicks` leaves them out. Touch trails and the laser are iPhone and macOS things.
- **How it renders.** With no zoom and no ripples (no clicks in the recording, or `--no-clicks`) one
  `ffmpeg` does everything. Otherwise a decoding `ffmpeg` pipes raw frames at the output rate to Scrawl,
  which draws the ripples and, for `--zoom`, resamples each frame to the camera's rectangle for its
  time (exact fractional position: bilinear zooming in, area average scaling down; a `crop` filter
  would round to whole pixels and shimmer), and an encoding `ffmpeg` adds the audio. A 15 s 1080p
  recording takes about 5 s without zoom and 15 s with it on a busy desktop (about real time with zoom,
  about three times faster without).
- **Menu.** With `"videoExport": true` in `~/.config/scrawl/config.json` the tray menu has **Export
  Last Capture as Video…**. It runs at low priority, notifies when it starts and finishes (with the
  path), and the entry appears when the menu is next rebuilt (after a recording, or on the next start).

## Keeping captures small

Captures would otherwise pile up (mostly video). The app cleans up shortly after launch and then
daily, at low priority, and logs what it did:

- **Video, audio and full-size frames** (`recording.mp4`/`.mov`, `audio.wav`, `frames-full/`) go
  to the Trash after 7 days, as one `<bundle> media` folder. The prompt (`summary.md`,
  `scrawl.json`, `contact-sheet.png`, `frames/`) stays, so pasting and reading still work;
  `scrawl zoom` says the video was cleaned up.
- **Whole bundles** go to the Trash after 30 days.
- **Keep recordings** in Settings › Storage picks Video for 7 Days (bundles 30), Video for 30 Days
  (bundles 90), or Forever. It's stored as `"retention": {"mediaDays": 7, "bundleDays": 30}` in
  `~/.config/scrawl/config.json`; `null` keeps forever. The same pane shows how much the captures
  take and opens the folder in Finder.
- Never touched: a bundle under a day old or changed in the last day (a copy just arrived from
  another Mac, a zoom), one whose media is still copying (`.media-pending` there, `.media-sending`
  here; a marker over a day old is ignored), one being recorded or re-encoded, and one with a
  `.pinned` file (`scrawl pin`, `scrawl pin --off`).
- **iPhone video** from the cable is ~18 Mbps at 1320×2868. A minute after the capture (after any
  copy to another Mac finishes, and never during a recording) `scrawl-helper shrink` re-encodes it
  to HEVC at a 1600 px long edge, audio copied as is and every frame at its original time, and
  swaps it in place: a 4.5-minute capture goes from 750 MB to 72 MB in about 20 s. Daily passes
  shrink any native iPhone video that was missed.

`scrawl clean --dry-run` prints what a pass would do now.

## Transcription

Narration is transcribed on device by Apple's SpeechAnalyzer (macOS 26), which reports when every
word was spoken. The word timings drive two things:

- **Text per keyframe.** Each keyframe's text runs from the start of the clause in progress when
  its moment began (the action, the transition, or the spoken cue) until the next keyframe, so the
  transcript is split at sentence and clause boundaries, in order, with nothing repeated or lost.
- **Spoken cues.** Saying "now", "here", "look", or "watch" is a deliberate cue: if the screen
  changes around the word, that settled view becomes a keyframe; otherwise the frame as you said it.
  A cue only cuts a keyframe if its image differs from every keyframe already chosen, so talking
  over an unchanged screen still produces one frame. "This", "that", "these", "those", "see", and
  "there" count only when they coincide with a change, because they are usually filler. Speech
  pauses never cut keyframes.

### Apple or Hex

Apple's SpeechAnalyzer is the default and needs nothing. If you use Hex
(2.2 or later, running), Scrawl can transcribe with it instead and with whichever model Hex has
ready: Settings › Recording › **Transcription** → **Hex**, then pick a model under **Hex model**.
Or in
`~/.config/scrawl/config.json`:

```json
{ "transcriber": "hex", "hexModel": "parakeet_v3" }
```

`"transcriber"` is `"apple"` (default) or `"hex"`; `hexModel` defaults to the first ready one of
`parakeet_unified_en`, `parakeet_v3`, `parakeet_v2`, `whisper_large_v3_turbo`. Scrawl never
downloads or prepares a model: install it in Hex first.

- Hex is optional. When it isn't running, **Hex** is greyed out as "Hex (not running)" and Scrawl
  uses Apple, whatever the saved preference. If Hex fails on a recording, that recording falls back
  to Apple Speech (see `~/Library/Logs/Scrawl`).
- Hex returns text without word times, so Scrawl sends each stretch of speech separately and
  spreads its words across it by length. The transcript says `"source": "hex:<model>"` and
  `"estimated": true`; because those times are approximate, spoken cues ("now", "here") don't cut
  keyframes from a Hex transcript. Marks, clicks, laser and scene changes still do.
- `scrawl hex status` shows whether Hex is connected, its ready models and what the next recording
  will use. `scrawl reprocess <bundle> --out … --transcriber hex|apple` (also `scrawl import`) picks
  an engine for one run.

### Transcribing while recording (experiment)

```json
{ "experiments": { "liveTranscribe": true } }
```

With this in `~/.config/scrawl/config.json`, a Mac recording transcribed by Apple Speech streams the
microphone to `scrawl-helper transcribe-stream` as it records, so finishing only waits for the last
words (tens of milliseconds) instead of transcribing the whole recording. The streamed samples are
exactly the ones `audio.wav` holds, so word times are on the same clock. If the stream fails, the
speech model isn't installed yet, or it takes too long, the recording is transcribed afterwards as
usual; the log says which (`Live transcription: …`). Hex and iPhone recordings are unaffected.
`scrawl reprocess <bundle> --out … --bench 1 --live-speed 1` replays a bundle's audio through it.

## Helper

`scrawl-helper` is a small Swift executable built by `build.rs` from `helper/` and shipped inside
`Scrawl.app` next to `scrawl`. It owns the Swift-only or unbound Apple frameworks: SpeechAnalyzer,
CoreMediaIO/AVFoundation iPhone capture with the mirror window, AVFoundation movie decoding, and the
SwiftUI welcome window (`onboarding`).
Scrawl runs it as a subprocess (JSON on stdout; `stop`/`cancel` on stdin for the mirror). Run it
directly to test a piece in isolation:

```sh
target/debug/scrawl-helper devices
target/debug/scrawl-helper permissions
target/debug/scrawl-helper transcribe audio.wav
target/debug/scrawl-helper frames recording.mov --out-dir /tmp/f --at 1000,2000 --max-edge 1600
target/debug/scrawl-helper shrink recording.mov --out /tmp/small.mov --max-edge 1600
target/debug/scrawl-helper export plan.json   # the plan `scrawl export` writes (src/export)
target/debug/scrawl-helper permission-panel --permission screen-recording --preview   # the drag card, no grant polling
```

`permission-panel --permission screen-recording|accessibility [--from X,Y,W,H]`
(`--from` is the button's frame in global top-left points) prints JSON lines: `docked`, `dropped`,
then `granted` or `cancelled` with a `reason` (`back`, `settings-closed`, `no-settings-window`,
`parent`, `timeout`), and exits. On stdin it takes `cancel`, `back` and `granted`.
`permission-panel --check P` prints `{"granted":…}` from a fresh process. `permission-panel --plan --settings
X,Y,W,H --visible X,Y,W,H` prints where the card would dock (`below`, `inside`, `right`, `left` or
`clamped`); `scripts/check-permission-panel-dock.sh` checks the cases.

## Testing

```sh
taskpolicy -c utility cargo test -j 3 --offline -- --test-threads=2
./scripts/install-app.sh
```

- **iPhone:** connect a phone, hold `⌥⇧` and press `S` three times, release, talk, draw over the mirror, press M,
  Finish. Inspect the bundle's `summary.md`, `scrawl.json` (`source`, `device`), contact sheet, and
  frames.
- **ScrawlKit:** `scrawl import` on a recording and its timeline (or hand-write a timeline for any
  phone video), then check the ripples and trails on frames and the app-event lines in the summary.
  `swift test` runs the outbox tests on the Mac against a `URLProtocol` stub of the ingest endpoint
  (persisting, retry classification, resends, deletion only on 2xx, and what the strip says through
  slow uploads, no network, and retries).
- **Transcription:** `scrawl-helper transcribe <wav or movie>` prints words with timings;
  `scrawl reprocess <bundle> --out …` reruns transcription and keyframe selection on a real bundle.

## License

MIT. See [LICENSE](LICENSE).

### THIRD_PARTY

`helper/PermissionPanel.swift` adapts code and tuning from two MIT-licensed projects:

- [AskForPermission](https://github.com/riko2chen/AskForPermission), Copyright (c) 2026 Riko: the
  flight along a quadratic Bézier, waiting for the System Settings window to settle, following it
  through the window server, and the race that resolves a drop.
- [PermissionFlow](https://github.com/jaywcjlove/PermissionFlow), Copyright (c) 2026 小弟调调: the
  Privacy pane deep links and macOS 27's "Device Control and Data Access" name for Accessibility.

Both are under the MIT License: permission is hereby granted, free of charge, to any person
obtaining a copy of this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to use, copy, modify,
merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to
whom the Software is furnished to do so, subject to the following conditions: The above copyright
notice and this permission notice shall be included in all copies or substantial portions of the
Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED,
INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES
OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
