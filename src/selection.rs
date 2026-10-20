//! The picker's capture modes (Area, Window, Screen, iPhone): their order in the capsule, labels,
//! icons, keys, and the one the last recording used.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionMode {
    Area,
    Window,
    /// The whole display the picker covers.
    Screen,
    Iphone,
}

impl SelectionMode {
    const ALL: [SelectionMode; 4] = [Self::Area, Self::Window, Self::Screen, Self::Iphone];

    fn key(self) -> &'static str {
        match self {
            Self::Area => "area",
            Self::Window => "window",
            Self::Screen => "screen",
            Self::Iphone => "iphone",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.key() == key.trim())
    }

    fn last_used_path() -> Option<PathBuf> {
        crate::handoff::default_captures_dir()
            .parent()
            .map(|dir| dir.join("last-mode"))
    }

    /// The mode the last recording used; the picker opens on it.
    pub(crate) fn load_last_used() -> Self {
        Self::last_used_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| Self::from_key(&text))
            .unwrap_or(Self::Area)
    }

    pub(crate) fn save_last_used(self) {
        if let Some(path) = Self::last_used_path() {
            let _ = std::fs::write(path, self.key());
        }
    }

    /// Modes shown in the HUD, in order; iPhone only while one is cabled.
    pub(crate) fn available(has_iphone: bool) -> &'static [SelectionMode] {
        if has_iphone {
            &Self::ALL
        } else {
            &Self::ALL[..3]
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Area => "Area",
            Self::Window => "Window",
            Self::Screen => "Screen",
            Self::Iphone => "iPhone",
        }
    }

    pub(crate) fn icon(self) -> &'static str {
        match self {
            Self::Area => crate::icons::AREA,
            Self::Window => crate::icons::WINDOW,
            Self::Screen => crate::icons::SCREEN,
            Self::Iphone => crate::icons::IPHONE,
        }
    }

    pub(crate) fn chip_id(self) -> &'static str {
        match self {
            Self::Area => "sel-area",
            Self::Window => "sel-window",
            Self::Screen => "sel-screen",
            Self::Iphone => "sel-iphone",
        }
    }

    /// Screen and iPhone need no pointer gesture, so the HUD offers a Record chip for them.
    pub(crate) fn shows_record(self) -> bool {
        matches!(self, Self::Screen | Self::Iphone)
    }

    /// `1`–`4` pick a mode by its position in the HUD.
    pub(crate) fn from_digit(key: &str, has_iphone: bool) -> Option<Self> {
        let index = key.parse::<usize>().ok()?.checked_sub(1)?;
        Self::available(has_iphone).get(index).copied()
    }

    /// The neighbouring mode in HUD order, wrapping at either end.
    pub(crate) fn cycled(self, forward: bool, has_iphone: bool) -> Self {
        let modes = Self::available(has_iphone);
        let Some(index) = modes.iter().position(|m| *m == self) else {
            return if forward {
                modes[0]
            } else {
                modes[modes.len() - 1]
            };
        };
        let next = if forward {
            (index + 1) % modes.len()
        } else {
            (index + modes.len() - 1) % modes.len()
        };
        modes[next]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_used_mode_keys_round_trip() {
        for mode in SelectionMode::ALL {
            assert_eq!(SelectionMode::from_key(mode.key()), Some(mode));
        }
        assert_eq!(
            SelectionMode::from_key("iphone\n"),
            Some(SelectionMode::Iphone)
        );
        assert_eq!(SelectionMode::from_key("fullscreen"), None);
    }
}
