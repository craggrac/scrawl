# Retention on Linux + Portuguese spoken cues

## Task 1: Portuguese cues (src/keyframes.rs, spoken_cue)
- Strong: agora 58, aqui 56, olha/olhe/veja 58, ó 56. Weak: vê/ali/lá/isto 44; aí, isso, este/esta/esse/essa/estes/estas/esses/essas 42.
- Accents survive: trim/lowercase use Unicode-aware char::is_alphanumeric / to_lowercase.
- Filler words stay weak; unchanged screens still never get a duplicate Speech keyframe.
- Tests: accents, cue snaps to the change, no duplicate on an unchanged screen.

## Task 2: retention on Linux (src/retention.rs)
- trash() uses `gio trash -- <path>` on Linux (same success check).
- start/worker/shrink_now/Recording un-gated for Linux; lower_priority sets the thread nice to 19.
- retention::start() called in the Linux run_app, only for the real app (with the control socket).
- Recording guard: platform/linux/session.rs (Running, covers in-app recordings), linux/mod.rs record_bundle (scrawl record/toggle), linux/replay.rs save_bundle; the last two also `claim` the bundle dir.
- Shrink: run_pass passes shrink=false on Linux (iPhone-only; Mac-synced iPhone bundles are not re-encoded on Linux).
- Skipped: Keep Recordings UI in Settings (macOS-gated deeply); Linux uses the default Week preset or `"retention"` in config.json.
- Tests: trashing a missing path errors (gio); real Trash never touched.

## Behavior risk
Default policy (no config file): media to Trash after 7 days, bundle after 30. Dry run on the real captures: nothing now; with +2 days, media of the 10 bundles from 2026-10-20/21 (~100 MB) would go; prompt files stay. 
Bundles from 2026-10-26 are protected (< 1 day).
