//! The record shortcut as Scrawl's windows see it. The key that starts a recording from
//! anywhere is a Hyprland bind (`scrawl toggle`); this only recognises the same combination
//! when it reaches the picker or the review window, and names it in the macOS style the
//! capsule shows (`⌥⇧S`).

/// A shortcut: its key (`key_code`, see `shortcut_from_key_and_mods`) and its label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutConfig {
    pub key_code: u32,
    pub label: String,
}

impl ShortcutConfig {
    /// ⌥⇧S, as the README's `bind = ALT SHIFT, S, exec, scrawl toggle`.
    pub fn default_record() -> Self {
        Self {
            key_code: u32::from('S'),
            label: "⌥⇧S".to_owned(),
        }
    }

    /// Whether a key (`key_code`) with these modifiers is this shortcut.
    pub fn matches(&self, key_code: u16, cmd: bool, ctrl: bool, alt: bool, shift: bool) -> bool {
        u32::from(key_code) == self.key_code
            && crate::capture::shortcut_parts(&self.label).0
                == modifier_symbols(cmd, ctrl, alt, shift)
    }
}

/// The shortcut a GPUI key (`"s"`, `"space"`, `"f5"`) with these modifiers makes, if it can be
/// one: a letter, digit, Space or F1–F12, with ⌃, ⌥ or ⌘ unless it's a function key. The key
/// code is the label's character (`'S'`), `0x20` for Space and `0xF000 + n` for Fn.
pub fn shortcut_from_key_and_mods(
    key: &str,
    cmd: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Option<ShortcutConfig> {
    let lower = key.to_ascii_lowercase();
    let mut chars = lower.chars();
    let (key_code, key_label) = match (chars.next(), chars.next()) {
        (Some(ch), None) if ch.is_ascii_alphanumeric() => {
            let upper = ch.to_ascii_uppercase();
            (u32::from(upper), upper.to_string())
        }
        _ if lower == "space" || lower == " " => (0x20, "Space".to_owned()),
        _ => {
            let n: u32 = lower.strip_prefix('f')?.parse().ok()?;
            if !(1..=12).contains(&n) {
                return None;
            }
            (0xF000 + n, format!("F{n}"))
        }
    };
    let function_key = key_code >= 0xF000;
    if !cmd && !ctrl && !alt && !function_key {
        return None;
    }
    Some(ShortcutConfig {
        key_code,
        label: modifier_symbols(cmd, ctrl, alt, shift) + &key_label,
    })
}

/// Where a press of the capture shortcut was seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressSource {
    /// The evdev listener (`linux::hotkey`), which also sees the hold.
    Keys,
    /// `scrawl toggle` on the control socket, from the Hyprland bind (or a script).
    Socket,
}

/// One key press can arrive twice: the listener reads it from evdev and the Hyprland bind
/// runs `scrawl toggle`. A press from one source within this long after an unpaired press
/// from the other is that same press.
pub const SAME_PRESS_MS: u64 = 1_000;

/// Lets each physical press act once, whichever way it arrives first.
#[derive(Debug, Default)]
pub struct PressGate {
    /// The last press that acted, when, and whether its twin has been dropped already.
    last: Option<(PressSource, u64, bool)>,
}

impl PressGate {
    /// Whether a press from `source` at `now_ms` should act.
    pub fn accept(&mut self, source: PressSource, now_ms: u64) -> bool {
        if let Some((last, at, paired)) = &mut self.last
            && *last != source
            && !*paired
            && now_ms.saturating_sub(*at) < SAME_PRESS_MS
        {
            *paired = true;
            return false;
        }
        self.last = Some((source, now_ms, false));
        true
    }
}

/// Modifier symbols in the order macOS writes them: ⌃⌥⇧⌘.
fn modifier_symbols(cmd: bool, ctrl: bool, alt: bool, shift: bool) -> String {
    [(ctrl, '⌃'), (alt, '⌥'), (shift, '⇧'), (cmd, '⌘')]
        .into_iter()
        .filter_map(|(held, symbol)| held.then_some(symbol))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_with_modifiers_become_mac_style_labels() {
        let s = shortcut_from_key_and_mods("s", false, false, true, true).unwrap();
        assert_eq!(s.label, "⌥⇧S");
        assert_eq!(s.key_code, u32::from('S'));
        assert_eq!(s, ShortcutConfig::default_record());
        let all = shortcut_from_key_and_mods("k", true, true, true, true).unwrap();
        assert_eq!(all.label, "⌃⌥⇧⌘K");
        assert_eq!(
            shortcut_from_key_and_mods("space", false, true, false, false).map(|s| s.label),
            Some("⌃Space".to_owned())
        );
        // A function key needs no modifier; a plain letter isn't a shortcut.
        let f5 = shortcut_from_key_and_mods("f5", false, false, false, false).unwrap();
        assert_eq!((f5.label.as_str(), f5.key_code), ("F5", 0xF005));
        assert!(shortcut_from_key_and_mods("s", false, false, false, true).is_none());
        assert!(shortcut_from_key_and_mods("enter", false, true, false, false).is_none());
    }

    #[test]
    fn a_press_seen_by_both_the_keys_and_the_bind_acts_once() {
        use PressSource::{Keys, Socket};
        let mut gate = PressGate::default();
        // evdev first, the bind's `scrawl toggle` 30 ms later.
        assert!(gate.accept(Keys, 0));
        assert!(!gate.accept(Socket, 30));
        // Held ⌥⇧, S again: the next pair.
        assert!(gate.accept(Keys, 200));
        assert!(!gate.accept(Socket, 230));
        // The bind first this time: the listener's copy is the twin, and a later S counts.
        assert!(gate.accept(Socket, 5_000));
        assert!(!gate.accept(Keys, 5_010));
        assert!(gate.accept(Keys, 5_300));
        assert!(!gate.accept(Socket, 5_320));
        // Only one source (no bind, or no input group): every press acts.
        assert!(gate.accept(Socket, 9_000));
        assert!(gate.accept(Socket, 9_100));
        assert!(gate.accept(Keys, 20_000));
        assert!(gate.accept(Keys, 20_100));
        // A deliberate `scrawl toggle` well after a key press acts.
        assert!(gate.accept(Socket, 21_200));
    }

    #[test]
    fn a_shortcut_matches_its_key_and_exact_modifiers() {
        let record = ShortcutConfig::default_record();
        let s = u32::from('S') as u16;
        assert!(record.matches(s, false, false, true, true));
        assert!(!record.matches(s, false, false, true, false));
        assert!(!record.matches(s, true, false, true, true));
        assert!(!record.matches(u32::from('V') as u16, false, false, true, true));
    }
}
