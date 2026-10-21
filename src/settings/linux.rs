//! What Settings does differently on Linux: the one shortcut it has (Record, kept under
//! `"linuxShortcuts"` and listened to by `linux::hotkey`), launch at login as an XDG autostart
//! entry, and what the Recording pane says about Parakeet. Everything else (the `Panel`, the
//! view, the config writes) is the shared code in `super`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Host, LoginItem, ShortcutView};
pub use crate::platform::ShortcutConfig;
use crate::platform::{
    SharedCaptureTelemetry, save_record_shortcut, shortcut_from_key_and_mods, shortcut_parts,
};

// MARK: Shortcuts

/// Scrawl's shortcuts; only Record exists on Linux (paste into the app in front needs a
/// virtual keyboard Wayland doesn't offer).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShortcutKind {
    Record,
}

impl ShortcutKind {
    pub const ALL: [Self; 1] = [Self::Record];

    pub fn default_config(self) -> ShortcutConfig {
        match self {
            Self::Record => ShortcutConfig::default_record(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Record => "Record",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortcuts {
    pub record: ShortcutConfig,
}

impl Shortcuts {
    pub fn get(&self, kind: ShortcutKind) -> &ShortcutConfig {
        match kind {
            ShortcutKind::Record => &self.record,
        }
    }

    pub fn set(&mut self, kind: ShortcutKind, cfg: ShortcutConfig) {
        match kind {
            ShortcutKind::Record => self.record = cfg,
        }
    }
}

/// A key combination pressed in the recorder, as GPUI names it (`"s"`, `"f5"`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Pressed {
    pub key: String,
    /// Super, which a shortcut's label writes as ⌘.
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// A letter, number or Space with nothing but ⇧.
    NoModifier,
    /// A key the listener can't tell apart (punctuation, arrows, Enter).
    UnsupportedKey,
    /// The host turned it down.
    Refused,
}

impl Rejection {
    pub fn message(&self) -> String {
        match self {
            Self::NoModifier => "Hold ⌃, ⌥ or ⌘ with the key.".to_owned(),
            Self::UnsupportedKey => {
                "Scrawl can't use that key. Try a letter, a number, Space or F1 to F12.".to_owned()
            }
            Self::Refused => "Scrawl couldn't use that shortcut.".to_owned(),
        }
    }
}

/// Names a pressed combination as a shortcut, or says why it can't be one.
pub fn check(
    _kind: ShortcutKind,
    pressed: &Pressed,
    _shortcuts: &Shortcuts,
) -> Result<ShortcutConfig, Rejection> {
    let Pressed {
        key,
        cmd,
        ctrl,
        alt,
        shift,
    } = pressed;
    shortcut_from_key_and_mods(key, *cmd, *ctrl, *alt, *shift).ok_or_else(|| {
        // With every modifier a key that can be a shortcut at all still is one.
        if shortcut_from_key_and_mods(key, true, true, true, true).is_some() {
            Rejection::NoModifier
        } else {
            Rejection::UnsupportedKey
        }
    })
}

/// Saves the record shortcut (the default is removed, not written).
pub fn save_shortcut_in(
    path: &Path,
    kind: ShortcutKind,
    cfg: &ShortcutConfig,
) -> anyhow::Result<()> {
    match kind {
        ShortcutKind::Record => save_record_shortcut(path, cfg),
    }
}

pub fn shortcut_view(
    kind: ShortcutKind,
    cfg: &ShortcutConfig,
    recording: bool,
    error: Option<&String>,
) -> ShortcutView {
    let (modifiers, key) = shortcut_parts(&cfg.label);
    ShortcutView {
        kind,
        title: kind.name(),
        label: cfg.label.clone(),
        key: key.to_owned(),
        shift: modifiers.contains('⇧'),
        control: modifiers.contains('⌃'),
        option: modifiers.contains('⌥'),
        command: modifiers.contains('⌘'),
        is_default: *cfg == kind.default_config(),
        recording,
        error: error.cloned(),
    }
}

// MARK: Launch at login

/// The autostart entry's file name in `~/.config/autostart` (`$XDG_CONFIG_HOME/autostart`).
const AUTOSTART_FILE: &str = "scrawl.desktop";

fn autostart_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("autostart"))
}

/// A word of an `Exec` line, quoted when it has characters the format reserves.
fn exec_word(word: &str) -> String {
    const RESERVED: &str = " \t\n\"'\\><~|&;$*?#()`";
    if !word.contains(|c| RESERVED.contains(c)) && !word.contains('%') {
        return word.to_owned();
    }
    let mut out = String::from('"');
    for c in word.chars() {
        match c {
            '"' | '`' | '$' => {
                out.push('\\');
                out.push(c);
            }
            // A backslash is escaped in the quoting and again in the file's own syntax.
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The autostart entry that starts the app from `exe`.
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Scrawl\nComment=Screen recordings for coding agents\n\
         Exec={} app\nTerminal=false\n",
        exec_word(&exe.to_string_lossy())
    )
}

pub fn login_item_in(dir: &Path) -> LoginItem {
    if dir.join(AUTOSTART_FILE).is_file() {
        LoginItem::On
    } else {
        LoginItem::Off
    }
}

/// Writes (`on`) or removes the autostart entry in `dir`.
pub fn set_login_item_in(dir: &Path, exe: &Path, on: bool) -> anyhow::Result<()> {
    use anyhow::Context;
    let file = dir.join(AUTOSTART_FILE);
    if on {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Could not create {}", dir.display()))?;
        std::fs::write(&file, desktop_entry(exe))
            .with_context(|| format!("Could not write {}", file.display()))
    } else {
        match std::fs::remove_file(&file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("Could not remove {}", file.display()))
            }
            _ => Ok(()),
        }
    }
}

pub fn login_item() -> LoginItem {
    autostart_dir().map_or(LoginItem::Unavailable, |dir| login_item_in(&dir))
}

pub fn set_login_item(on: bool) -> anyhow::Result<()> {
    let dir = autostart_dir().ok_or_else(|| anyhow::anyhow!("no config folder"))?;
    set_login_item_in(&dir, &std::env::current_exe()?, on)
}

// MARK: Parakeet

/// What the Recording pane says about the transcriber.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ParakeetView {
    /// The sherpa-onnx server and the model are both there.
    pub ready: bool,
    /// What is missing, or where it comes from.
    pub detail: String,
}

pub fn parakeet_view() -> ParakeetView {
    match crate::linux::parakeet::missing() {
        None => ParakeetView {
            ready: true,
            detail: "Parakeet TDT 0.6B v3 runs on this computer through OpenWhispr's \
                     sherpa-onnx and times each word."
                .to_owned(),
        },
        Some(missing) => ParakeetView {
            ready: false,
            detail: missing,
        },
    }
}

// MARK: The live window

/// The real listener and config file.
pub struct AppHost<'a> {
    pub telemetry: &'a SharedCaptureTelemetry,
}

impl Host for AppHost<'_> {
    fn shortcuts(&self) -> Shortcuts {
        Shortcuts {
            record: self
                .telemetry
                .shortcut
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    /// The listener and the capsule use it from now on; Hyprland's bind is not Scrawl's to edit.
    fn set_shortcuts(&mut self, shortcuts: &Shortcuts) -> Result<(), ShortcutKind> {
        crate::capture::scrawl_log(format!(
            "Record shortcut is now {}; change the Hyprland bind to match",
            shortcuts.record.label
        ));
        *self
            .telemetry
            .shortcut
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = shortcuts.record.clone();
        Ok(())
    }

    fn pause_shortcuts(&mut self, paused: bool) {
        self.telemetry.pause_shortcuts(paused);
    }

    fn config_path(&self) -> PathBuf {
        crate::handoff::default_config_path()
    }

    fn set_login_item(&mut self, on: bool) -> anyhow::Result<()> {
        set_login_item(on)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Action, Effect, Panel};
    use super::*;

    struct Fake {
        shortcuts: Shortcuts,
        path: PathBuf,
        paused: Vec<bool>,
        login: Vec<bool>,
    }

    impl Host for Fake {
        fn shortcuts(&self) -> Shortcuts {
            self.shortcuts.clone()
        }
        fn set_shortcuts(&mut self, shortcuts: &Shortcuts) -> Result<(), ShortcutKind> {
            self.shortcuts = shortcuts.clone();
            Ok(())
        }
        fn pause_shortcuts(&mut self, paused: bool) {
            self.paused.push(paused);
        }
        fn config_path(&self) -> PathBuf {
            self.path.clone()
        }
        fn set_login_item(&mut self, on: bool) -> anyhow::Result<()> {
            self.login.push(on);
            Ok(())
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "scrawl-settings-{name}-{}-{}",
                std::process::id(),
                crate::now_epoch_ms()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fake(dir: &TempDir) -> Fake {
        Fake {
            shortcuts: Shortcuts {
                record: ShortcutConfig::default_record(),
            },
            path: dir.0.join("config.json"),
            paused: Vec::new(),
            login: Vec::new(),
        }
    }

    fn pressed(key: &str, cmd: bool, ctrl: bool, alt: bool, shift: bool) -> Pressed {
        Pressed {
            key: key.to_owned(),
            cmd,
            ctrl,
            alt,
            shift,
        }
    }

    fn press(panel: &mut Panel, host: &mut Fake, pressed: Pressed) {
        panel.on(
            Action::Shortcut {
                kind: ShortcutKind::Record,
                pressed,
            },
            host,
        );
    }

    fn config(host: &Fake) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(&host.path).unwrap()).unwrap()
    }

    #[test]
    fn a_pressed_combination_names_a_shortcut_or_says_why_not() {
        let shortcuts = Shortcuts {
            record: ShortcutConfig::default_record(),
        };
        let check = |p: Pressed| check(ShortcutKind::Record, &p, &shortcuts);
        assert_eq!(
            check(pressed("r", true, false, false, false))
                .unwrap()
                .label,
            "⌘R"
        );
        assert_eq!(
            check(pressed("f5", false, false, false, false))
                .unwrap()
                .label,
            "F5"
        );
        // Letters and digits need ⌃, ⌥ or ⌘; ⇧ alone isn't enough.
        assert_eq!(
            check(pressed("r", false, false, false, true)),
            Err(Rejection::NoModifier)
        );
        // Keys the listener can't tell apart are no shortcut with any modifier.
        assert_eq!(
            check(pressed("enter", false, true, false, false)),
            Err(Rejection::UnsupportedKey)
        );
        assert_eq!(
            check(pressed("/", true, false, false, false)),
            Err(Rejection::UnsupportedKey)
        );
        assert!(Rejection::NoModifier.message().contains("Hold"));
    }

    #[test]
    fn the_recorder_pauses_the_listener_and_a_good_combo_is_saved_under_linux_shortcuts() {
        let dir = TempDir::new("record");
        let mut host = fake(&dir);
        std::fs::write(&host.path, r#"{"auto_copy": true}"#).unwrap();
        let mut panel = Panel::default();
        panel.on(
            Action::RecordShortcut {
                kind: ShortcutKind::Record,
            },
            &mut host,
        );
        assert_eq!(host.paused, [true]);
        assert!(panel.view(&host).shortcuts[0].recording);
        press(
            &mut panel,
            &mut host,
            pressed("r", true, false, false, false),
        );
        // Listening ended, the combo is live and saved, and the rest of the config stayed.
        assert_eq!(host.paused, [true, false]);
        assert_eq!(host.shortcuts.record.label, "⌘R");
        let saved = config(&host);
        assert_eq!(saved["linuxShortcuts"]["record"]["label"], "⌘R");
        assert_eq!(saved["auto_copy"], true);
        assert!(saved.get("shortcuts").is_none());
        let view = panel.view(&host);
        let record = &view.shortcuts[0];
        assert_eq!((record.label.as_str(), record.key.as_str()), ("⌘R", "R"));
        assert!(record.command && !record.shift && !record.is_default && !record.recording);
        // The reset arrow puts the default back, which isn't written.
        panel.on(
            Action::ResetShortcut {
                kind: ShortcutKind::Record,
            },
            &mut host,
        );
        assert_eq!(host.shortcuts.record, ShortcutConfig::default_record());
        assert!(config(&host).get("linuxShortcuts").is_none());
        assert!(panel.view(&host).shortcuts[0].is_default);
    }

    #[test]
    fn a_bad_combo_keeps_the_old_one_and_says_why_and_esc_cancels() {
        let dir = TempDir::new("bad");
        let mut host = fake(&dir);
        let mut panel = Panel::default();
        panel.on(
            Action::RecordShortcut {
                kind: ShortcutKind::Record,
            },
            &mut host,
        );
        press(
            &mut panel,
            &mut host,
            pressed("r", false, false, false, true),
        );
        assert_eq!(host.shortcuts.record, ShortcutConfig::default_record());
        assert_eq!(host.paused, [true, false]);
        let view = panel.view(&host);
        assert_eq!(
            view.shortcuts[0].error.as_deref(),
            Some(Rejection::NoModifier.message().as_str())
        );
        assert!(!view.shortcuts[0].recording);
        assert!(!host.path.exists());
        // Starting again clears the message; Esc ends listening without a change.
        panel.on(
            Action::RecordShortcut {
                kind: ShortcutKind::Record,
            },
            &mut host,
        );
        assert_eq!(panel.view(&host).shortcuts[0].error, None);
        panel.on(Action::CancelShortcut, &mut host);
        assert_eq!(host.paused, [true, false, true, false]);
        // A combo nobody asked to record is ignored.
        press(
            &mut panel,
            &mut host,
            pressed("r", true, false, false, false),
        );
        assert_eq!(host.shortcuts.record, ShortcutConfig::default_record());
    }

    #[test]
    fn closing_while_listening_gives_the_shortcut_back() {
        let dir = TempDir::new("close");
        let mut host = fake(&dir);
        let mut panel = Panel::default();
        panel.on(
            Action::RecordShortcut {
                kind: ShortcutKind::Record,
            },
            &mut host,
        );
        assert_eq!(panel.on(Action::Close, &mut host), Effect::Close);
        assert_eq!(host.paused, [true, false]);
        // Closing with nothing listening doesn't unpause what isn't paused.
        panel.on(Action::Close, &mut host);
        assert_eq!(host.paused, [true, false]);
    }

    #[test]
    fn the_toggles_write_their_keys_and_the_view_reads_them_back() {
        let dir = TempDir::new("toggles");
        let mut host = fake(&dir);
        std::fs::write(&host.path, r#"{"ingest_token": "secret"}"#).unwrap();
        let mut panel = Panel::default();
        let set = |panel: &mut Panel, host: &mut Fake, key: &str, value: serde_json::Value| {
            panel.on(
                Action::Set {
                    key: key.to_owned(),
                    value,
                },
                host,
            );
        };
        set(&mut panel, &mut host, "autoSend", false.into());
        set(&mut panel, &mut host, "launchAtLogin", true.into());
        set(&mut panel, &mut host, "launchAtLogin", false.into());
        assert_eq!(host.login, [true, false]);
        assert_eq!(config(&host)["autoSend"], false);
        assert_eq!(config(&host)["ingest_token"], "secret");
        assert!(!panel.view(&host).auto_send);
        // A key or value nobody offers changes nothing.
        set(&mut panel, &mut host, "autoSend", "no".into());
        set(&mut panel, &mut host, "nonsense", true.into());
        assert_eq!(config(&host).as_object().unwrap().len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&host.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn the_replay_buffer_switch_is_off_until_turned_on_and_written_under_its_key() {
        let dir = TempDir::new("replay");
        let mut host = fake(&dir);
        let mut panel = Panel::default();
        assert!(!panel.view(&host).replay_buffer);
        panel.on(
            Action::Set {
                key: "replayBuffer".to_owned(),
                value: true.into(),
            },
            &mut host,
        );
        assert_eq!(config(&host)["replayBuffer"], true);
        assert!(panel.view(&host).replay_buffer);
    }

    #[test]
    fn the_size_and_the_folder_are_the_panels_to_say() {
        let dir = TempDir::new("size");
        let host = fake(&dir);
        let mut panel = Panel::default();
        assert_eq!(panel.view(&host).captures_size, None);
        panel.set_captures_size(1_500_000_000);
        assert_eq!(panel.view(&host).captures_size.as_deref(), Some("1.5 GB"));
        assert_eq!(
            panel.on(Action::ShowCaptures, &mut fake(&dir)),
            Effect::ShowCaptures
        );
        let view = panel.view(&host);
        assert_eq!(view.skill_command, crate::settings::SKILL_COMMAND);
        assert!(!view.updates);
    }

    #[test]
    fn launch_at_login_is_an_autostart_entry_that_runs_the_app() {
        let dir = TempDir::new("autostart");
        let autostart = dir.0.join("autostart");
        let exe = Path::new("/home/me/.local/bin/scrawl");
        assert_eq!(login_item_in(&autostart), LoginItem::Off);
        // Off with nothing there is fine.
        set_login_item_in(&autostart, exe, false).unwrap();
        set_login_item_in(&autostart, exe, true).unwrap();
        assert_eq!(login_item_in(&autostart), LoginItem::On);
        let entry = std::fs::read_to_string(autostart.join("scrawl.desktop")).unwrap();
        assert!(entry.starts_with("[Desktop Entry]\nType=Application\n"));
        assert!(entry.contains("\nExec=/home/me/.local/bin/scrawl app\n"));
        assert!(entry.contains("\nTerminal=false\n"));
        set_login_item_in(&autostart, exe, false).unwrap();
        assert_eq!(login_item_in(&autostart), LoginItem::Off);
        assert!(!autostart.join("scrawl.desktop").exists());
    }

    #[test]
    fn an_exe_path_with_spaces_or_symbols_is_quoted_for_the_exec_line() {
        assert_eq!(exec_word("/usr/bin/scrawl"), "/usr/bin/scrawl");
        assert_eq!(exec_word("/my apps/scrawl"), "\"/my apps/scrawl\"");
        assert_eq!(exec_word("/a/$b/scrawl"), "\"/a/\\$b/scrawl\"");
        assert_eq!(exec_word("/a/100%/scrawl"), "\"/a/100%%/scrawl\"");
        assert_eq!(exec_word("/a\\b"), "\"/a\\\\\\\\b\"");
        assert!(
            desktop_entry(Path::new("/my apps/scrawl"))
                .contains("\nExec=\"/my apps/scrawl\" app\n")
        );
    }

    #[test]
    fn parakeet_says_what_is_missing() {
        let view = parakeet_view();
        // Whichever this machine has, the pane has something to say and agrees with itself.
        assert!(!view.detail.is_empty());
        assert_eq!(view.ready, crate::linux::parakeet::missing().is_none());
    }
}
