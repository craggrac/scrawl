//! The capture shortcut and Esc while `scrawl app` runs, read from the evdev devices like
//! `input::InputRecorder` (passively, never grabbed). The Hyprland bind only reports a press;
//! this also sees the modifiers come up, which is what holding the shortcut to open the mode
//! switcher needs (`modifier_bits`, behind `platform::modifier_flags`).
//!
//! Privacy: a key is only compared against the shortcut and Esc as it is read. Nothing but
//! the four modifier states is kept, and no other key leaves this module.

use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::input::{
    EV_KEY, EV_SYN, Layout, Modifiers, RawEvent, SYN_DROPPED, drain, key_name, open_devices,
    poll_readable,
};
use crate::capture::scrawl_log;

/// `modifier_bits`: Super (⌘ in the shortcut's label), Ctrl, Alt, Shift.
pub const MOD_SUPER: u64 = 1;
pub const MOD_CTRL: u64 = 1 << 1;
pub const MOD_ALT: u64 = 1 << 2;
pub const MOD_SHIFT: u64 = 1 << 3;

const KEY_ESC: u16 = 1;

/// What a key press means here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    None,
    /// The capture shortcut went down (not a repeat).
    Shortcut,
    /// Esc went down without Ctrl or Super.
    Escape,
}

/// The shortcut's key code (`ShortcutConfig::key_code`: the letter or digit, `0x20` for
/// Space, `0xF000 + n` for Fn) of an evdev key, if it can be one.
pub fn shortcut_key(code: u16, layout: Layout) -> Option<u16> {
    let name = key_name(code, layout)?;
    if name == "Space" {
        return Some(0x20);
    }
    if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u16>().ok()) {
        return (1..=12).contains(&n).then_some(0xF000 + n);
    }
    match name.as_bytes() {
        [c] if c.is_ascii_alphanumeric() => Some(u16::from(*c)),
        _ => None,
    }
}

/// Turns raw events into shortcut presses and Esc, tracking only the modifiers.
#[derive(Debug, Default)]
pub struct HotkeyKeys {
    mods: Modifiers,
    layout: Layout,
}

impl HotkeyKeys {
    pub fn new(layout: Layout) -> Self {
        Self {
            mods: Modifiers::default(),
            layout,
        }
    }

    /// The modifiers held now, as `MOD_*` bits.
    pub fn modifier_bits(&self) -> u64 {
        [
            (self.mods.meta(), MOD_SUPER),
            (self.mods.ctrl(), MOD_CTRL),
            (self.mods.alt(), MOD_ALT),
            (self.mods.shift(), MOD_SHIFT),
        ]
        .into_iter()
        .filter(|(held, _)| *held)
        .fold(0, |bits, (_, bit)| bits | bit)
    }

    /// Forget the modifiers (a device went away or events were dropped).
    pub fn reset(&mut self) {
        self.mods = Modifiers::default();
    }

    /// `is_shortcut(key_code, super, ctrl, alt, shift)` is `ShortcutConfig::matches`.
    pub fn handle(
        &mut self,
        raw: RawEvent,
        is_shortcut: impl Fn(u16, bool, bool, bool, bool) -> bool,
    ) -> KeyAction {
        if raw.kind == EV_SYN && raw.code == SYN_DROPPED {
            self.reset();
            return KeyAction::None;
        }
        if raw.kind != EV_KEY || self.mods.set(raw.code, raw.value != 0) || raw.value != 1 {
            return KeyAction::None;
        }
        let m = &self.mods;
        if shortcut_key(raw.code, self.layout)
            .is_some_and(|code| is_shortcut(code, m.meta(), m.ctrl(), m.alt(), m.shift()))
        {
            KeyAction::Shortcut
        } else if raw.code == KEY_ESC && !m.ctrl() && !m.meta() {
            KeyAction::Escape
        } else {
            KeyAction::None
        }
    }
}

/// The modifiers the listener last saw held (`MOD_*`); 0 when it isn't running.
static MODIFIERS: AtomicU64 = AtomicU64::new(0);

/// See `MODIFIERS`.
pub fn modifier_bits() -> u64 {
    MODIFIERS.load(Ordering::Acquire)
}

/// How often the device list is read again, for keyboards plugged in later.
const RESCAN: Duration = Duration::from_secs(10);

/// Listens for the app's lifetime on a thread: `on_key` gets each shortcut press and Esc.
pub fn start(
    is_shortcut: impl Fn(u16, bool, bool, bool, bool) -> bool + Send + 'static,
    on_key: impl Fn(KeyAction) + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name("scrawl-hotkey".into())
        .spawn(move || listen(&is_shortcut, &on_key));
    if let Err(error) = spawned {
        scrawl_log(format!("Shortcut listener: could not start: {error}"));
    }
}

/// Which `/dev/input` node a file is.
fn device_number(file: &File) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    file.metadata().ok().map(|meta| meta.rdev())
}

fn listen(is_shortcut: &dyn Fn(u16, bool, bool, bool, bool) -> bool, on_key: &dyn Fn(KeyAction)) {
    let layout =
        super::hypr::keyboard_layout().map_or(Layout::Us, |setting| Layout::from_setting(&setting));
    let mut keys = HotkeyKeys::new(layout);
    let mut devices: Vec<File> = Vec::new();
    let mut scanned: Option<Instant> = None;
    let mut warned = false;
    loop {
        if scanned.is_none_or(|at| at.elapsed() >= RESCAN) {
            // Only devices not open yet are added: reopening one could lose a modifier's
            // release, and the switcher would think it still held.
            let open: Vec<u64> = devices.iter().filter_map(device_number).collect();
            devices.extend(
                open_devices()
                    .into_iter()
                    .filter(|file| device_number(file).is_some_and(|n| !open.contains(&n))),
            );
            scanned = Some(Instant::now());
            if devices.is_empty() && !warned {
                warned = true;
                scrawl_log(
                    "Shortcut listener: no readable /dev/input device (is the user in the \
                     input group?); holding the shortcut won't open the switcher",
                );
            }
        }
        if devices.is_empty() {
            std::thread::sleep(RESCAN);
            continue;
        }
        let mut gone = Vec::new();
        for index in poll_readable(&devices, 500) {
            let Some(raws) = drain(&mut devices[index]) else {
                gone.push(index);
                continue;
            };
            for raw in raws {
                let action = keys.handle(raw, is_shortcut);
                MODIFIERS.store(keys.modifier_bits(), Ordering::Release);
                if action != KeyAction::None {
                    on_key(action);
                }
            }
        }
        if !gone.is_empty() {
            keys.reset();
            MODIFIERS.store(0, Ordering::Release);
            for index in gone.into_iter().rev() {
                devices.swap_remove(index);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u16, value: i32) -> RawEvent {
        RawEvent {
            kind: EV_KEY,
            code,
            value,
        }
    }

    const LEFT_ALT: u16 = 56;
    const LEFT_SHIFT: u16 = 42;
    const LEFT_CTRL: u16 = 29;
    const S: u16 = 31;

    /// ⌥⇧S, as `ShortcutConfig::default_record().matches` sees it.
    fn alt_shift_s(code: u16, cmd: bool, ctrl: bool, alt: bool, shift: bool) -> bool {
        code == u16::from(b'S') && !cmd && !ctrl && alt && shift
    }

    #[test]
    fn evdev_keys_map_to_shortcut_key_codes() {
        assert_eq!(shortcut_key(S, Layout::Us), Some(u16::from(b'S')));
        assert_eq!(shortcut_key(2, Layout::Br), Some(u16::from(b'1')));
        assert_eq!(shortcut_key(57, Layout::Us), Some(0x20));
        assert_eq!(shortcut_key(59, Layout::Us), Some(0xF001));
        assert_eq!(shortcut_key(88, Layout::Us), Some(0xF00C));
        // Esc, punctuation and Ç can't be the shortcut.
        assert_eq!(shortcut_key(KEY_ESC, Layout::Us), None);
        assert_eq!(shortcut_key(51, Layout::Us), None);
        assert_eq!(shortcut_key(39, Layout::Br), None);
    }

    #[test]
    fn the_shortcut_fires_on_its_press_and_the_modifiers_show_the_hold() {
        let mut keys = HotkeyKeys::new(Layout::Us);
        assert_eq!(keys.handle(key(LEFT_ALT, 1), alt_shift_s), KeyAction::None);
        assert_eq!(
            keys.handle(key(LEFT_SHIFT, 1), alt_shift_s),
            KeyAction::None
        );
        assert_eq!(keys.modifier_bits(), MOD_ALT | MOD_SHIFT);
        assert_eq!(keys.handle(key(S, 1), alt_shift_s), KeyAction::Shortcut);
        // Holding S repeats; letting go of S is nothing; a second S steps the switcher.
        assert_eq!(keys.handle(key(S, 2), alt_shift_s), KeyAction::None);
        assert_eq!(keys.handle(key(S, 0), alt_shift_s), KeyAction::None);
        assert_eq!(keys.handle(key(S, 1), alt_shift_s), KeyAction::Shortcut);
        // ⌥ up: the hold is over.
        keys.handle(key(LEFT_ALT, 0), alt_shift_s);
        assert_eq!(keys.modifier_bits(), MOD_SHIFT);
        assert_eq!(keys.handle(key(S, 1), alt_shift_s), KeyAction::None);
    }

    #[test]
    fn esc_without_ctrl_or_super_and_nothing_else_is_reported() {
        let mut keys = HotkeyKeys::new(Layout::Us);
        assert_eq!(keys.handle(key(KEY_ESC, 1), alt_shift_s), KeyAction::Escape);
        assert_eq!(keys.handle(key(KEY_ESC, 0), alt_shift_s), KeyAction::None);
        // Typing is not an action.
        for code in [16, 17, 18, 30, 57, 28] {
            assert_eq!(keys.handle(key(code, 1), alt_shift_s), KeyAction::None);
        }
        keys.handle(key(LEFT_CTRL, 1), alt_shift_s);
        assert_eq!(keys.modifier_bits(), MOD_CTRL);
        assert_eq!(keys.handle(key(KEY_ESC, 1), alt_shift_s), KeyAction::None);
    }

    #[test]
    fn dropped_events_forget_the_modifiers() {
        let mut keys = HotkeyKeys::new(Layout::Us);
        keys.handle(key(LEFT_ALT, 1), alt_shift_s);
        let dropped = RawEvent {
            kind: EV_SYN,
            code: SYN_DROPPED,
            value: 0,
        };
        assert_eq!(keys.handle(dropped, alt_shift_s), KeyAction::None);
        assert_eq!(keys.modifier_bits(), 0);
    }
}
