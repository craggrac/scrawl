# Browser, terminal and project context (branch feat-browser-context)

Not in upstream. Linux-first; macOS keeps compiling (type-checked with an adapted copy of `mc.sh`, no diagnostics from scrawl).

## What is collected

Off by default: `"devContext": true` in `~/.config/scrawl/config.json`, or Settings > Recording > "Browser and project context" (same pattern as `videoExport`). Everything is on the capture clock, goes to `scrawl.json` as `devContext`, and shows as a `Context:` block at the end of `summary.md` (at most 12 timeline lines + `+N more in scrawl.json`, at most 3 repositories). `scrawl reprocess` keeps it.

| Part | How |
|-|-|
| Console errors/warnings, uncaught exceptions (top frame), failed requests (status >= 400, net errors, CORS blocks; not cancelled, not favicon), page URL changes | CDP, `src/linux/context/cdp.rs`. One WebSocket (`tungstenite`, already a dependency; no new crates) to a browser that already exposes DevTools. Event domains `Runtime`, `Log`, `Network` only, plus `Target` to find the tab. |
| Which tab | The page whose title the focused Hyprland window shows (window pid must be the pid listening on the debugging port). The tab last in front stays followed while a terminal has focus; other tabs are never attached. |
| Terminal cwd | Focused window pid -> `/proc` descendants with a controlling tty -> foreground process's cwd (`procs.rs`); only answered when all ttys agree (multi-tab terminals can't be told apart). `/` and `$HOME` ignored. Validated on a real pty. |
| localhost page -> project | `/proc/net/tcp{,6}` listening inode -> owning process -> cwd (`procs.rs`). `routing::listener_directory` shells out to macOS `lsof` and is not portable, so this is a Linux counterpart. |
| Repository | root, branch, head + subject, changed files (12 named, all counted) via hardened `git` (see Security). |
| Scrollback | Deliberately not captured. |

Caps: 40 console, 40 network, 20 pages, 4 repositories, strings 300 chars; repeats counted (`x3`). Redaction (`redact.rs`): URL credentials, query/fragment values with token-like names or shapes, 32+ char id-like path segments (not UUIDs), JWTs, provider key prefixes, `Authorization: Bearer ...`, `password=...` in messages -> `REDACTED`. Request headers/bodies are never read.

## CDP enablement story

Scrawl never launches, restarts or reconfigures a browser and never writes to a profile. It finds `DevToolsActivePort` files one or two folders under `~/.config` (Helium `net.imput.helium/`, Chrome/Chromium `google-chrome/`, Brave `BraveSoftware/Brave-Browser/`, Electron apps) or `"cdpPort"` in `config.json`, then validates with `/json/version`. Without a port it records nothing and logs once ("Dev context: no browser exposes DevTools ..."). To enable: start the browser with `--remote-debugging-port=9222` (or `=0`, the file is what Scrawl reads). README and the Settings caption state the tradeoff: any local process can then control the browser, including logged-in sessions. Also documented: Google Chrome 136+ ignores the flag for the default profile folder (needs a non-default `--user-data-dir`); I could not test this on Helium/Chrome (nothing visible may be opened), and Chromium's headless run uses its own scoped dir.

Helium on this machine currently exposes no port (no `DevToolsActivePort` in `~/.config/net.imput.helium`), so the live path against the user's browser was not exercised; the Hyprland side was checked read-only (Helium's window pid equals the browser main process, title is `<page title> - Helium`).

## Example (real headless Chromium, local page throwing errors; `cargo test -- --ignored chromium`)

```
Context:
· 0:02 page 127.0.0.1:42977/
· 0:02 console error: boom from console Error: inner (/:1:120)
· 0:02 console warning: careful (/:1:176)
· 0:04 POST /api/x -> 500        (query api_key=... redacted in scrawl.json)
· 0:04 GET /missing.json -> 404
· 0:04 uncaught ReferenceError: undefinedFunction is not defined (/:1:337)
· 0:05 page -> 127.0.0.1:42977/route/two     (?token=sekret redacted in scrawl.json)
· repo scrawl @ main 42be30e "Add tabs" (3 changed: M src/List.tsx, ?? notes.txt, +1)
```
The browser lines are real output of the Chromium test (the summary prints the Unicode arrow). The repo line is illustrative (from the rendering unit test); the collector test reads this worktree's real branch/head but asserts only that they exist.

## Finish-time impact

The collector is one worker thread (focused window every 250 ms, browser events as they come, git on short-lived threads). In the app its `stop()` runs in the same scope as the screen session's stop, so it adds nothing to the critical path; in `scrawl record` it is sequential but measured: `Dev context: ... stopped in 12 ms` / 30 ms (collector test, fake browser, real git; load average ~11). Worst case is `GIT_WAIT` 1.5 s only if a repository read is still running at stop (git is read once per repository, at first sight, normally long before). With no browser the loop idles at 40 ms. `git status` on /home/ksm/scrawl takes ~60 ms. The time is logged as `Dev context: ... stopped in N ms`; I did not run `scrawl record` (it notifies on screen).

## Security (coordinator's review finding, fixed)

The cwd comes from untrusted places, so a repository's config must not start programs. `git::git_raw` overrides every program-naming setting (fsmonitor, hooksPath, pager, diff.external, gpg, ssh, askpass), sets `GIT_CONFIG_NOSYSTEM`/`GIT_ATTR_NOSYSTEM`, uses `symbolic-ref` and `log --no-show-signature` for branch/head, and runs `status --no-renames --ignore-submodules` only when `git config --show-scope --get-regexp '^filter\.'` shows no local/worktree filter (clean filters run on stat-dirty files and can't be overridden generically); otherwise `changed` is omitted (`Project.changed: Option<u32>`). Test `a_repository_that_names_programs_in_its_config_gets_none_of_them_run` builds a repo whose config names a marker-touching program for each setting plus a `.gitattributes` filter, proves an ordinary `git status`/`log -p` starts them, and asserts Scrawl's read starts none.

## Tests

`cargo test`: 470 passed, 3 ignored (the ignored: this feature's real-Chromium test and two pre-existing). New tests: summary rendering, JSON round trip, caps/dedupe, CDP protocol state (focus following, replay filtering, console/exception/network handling, secrets), an in-process fake DevTools server over a real socket, whole-collector test (fake browser + focus + a localhost listener -> repository, stop < 500 ms), /proc parsing and process-tree logic on a fake `/proc`, real listener lookup, redaction, git (real temp repos, hostile config), Settings switch. `cargo test -- --ignored chromium` runs against a real headless Chromium (`SCRAWL_TEST_CHROMIUM` for the binary) and passed. No new compiler warnings.

## Unresolved / limits

- A tab still loading when DevTools connects keeps its address as its CDP title until it navigates (seen in headless Chromium 153), so the title match can miss it; stable tabs report real titles. Network events before attach (first ~300 ms) are lost; console messages are replayed with their real timestamps.
- Flatpak/snap browsers (different pid namespace) won't match the window pid.
- A terminal with several tabs in different folders gives no cwd; tmux gives the client's folder.
- Default is off; the user turns it on in Settings or config. `settings_window.rs` was touched (small), which the ink-tools branch may also edit.
- Fractional `at` paths for inline scripts read `/:line:col` (the page itself).
