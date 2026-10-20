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
            && crate::capture::shortcut_parts(&self.label).0 == modifier_symbols(cmd, ctrl, alt, shift)
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
    fn a_shortcut_matches_its_key_and_exact_modifiers() {
        let record = ShortcutConfig::default_record();
        let s = u32::from('S') as u16;
        assert!(record.matches(s, false, false, true, true));
        assert!(!record.matches(s, false, false, true, false));
        assert!(!record.matches(s, true, false, true, true));
        assert!(!record.matches(u32::from('V') as u16, false, false, true, true));
    }
}
