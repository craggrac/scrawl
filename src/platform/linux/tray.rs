//! The tray icon (StatusNotifierItem, through `ksni`), the Linux status menu: Record or Stop,
//! Open Latest Capture, Quit. It needs a tray host on the bar (waybar's `tray`, Noctalia's);
//! without one Scrawl logs that once and runs without it.

use std::sync::Arc;

use ksni::blocking::TrayMethods;

use crate::capture::scrawl_log;

/// A menu item that was clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayItem {
    /// Record, or stop the recording.
    Record,
    OpenLatest,
    Quit,
}

/// The menu's rows for the state: (label, item).
pub fn rows(recording: bool, shortcut: &str) -> Vec<(String, TrayItem)> {
    let record = if recording {
        "Stop Recording"
    } else {
        "Record Screen"
    };
    vec![
        (format!("{record}  {shortcut}"), TrayItem::Record),
        ("Open Latest Capture".to_owned(), TrayItem::OpenLatest),
        ("Quit Scrawl".to_owned(), TrayItem::Quit),
    ]
}

/// Side of the square icon, in pixels.
const ICON: i32 = 22;

/// The icon as ARGB32, big-endian (`ksni::Icon`): a ring while idle, a red dot recording.
pub fn icon_pixels(recording: bool, size: i32) -> Vec<u8> {
    const RING: [u8; 4] = [0xFF, 0xEE, 0xEE, 0xEE];
    const DOT: [u8; 4] = [0xFF, 0xFF, 0x3B, 0x30];
    const CLEAR: [u8; 4] = [0; 4];
    let centre = f64::from(size) / 2.0;
    let outer = centre - 1.0;
    let inner = outer - 2.5;
    let dot = inner - 2.0;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let d = (f64::from(x) + 0.5 - centre).hypot(f64::from(y) + 0.5 - centre);
            let colour = if d <= outer && d >= inner {
                RING
            } else if recording && d <= dot {
                DOT
            } else {
                CLEAR
            };
            data.extend_from_slice(&colour);
        }
    }
    data
}

struct ScrawlTray {
    recording: bool,
    shortcut: String,
    on_item: Arc<dyn Fn(TrayItem) + Send + Sync>,
}

impl ksni::Tray for ScrawlTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "scrawl".into()
    }

    fn title(&self) -> String {
        "Scrawl".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![ksni::Icon {
            width: ICON,
            height: ICON,
            data: icon_pixels(self.recording, ICON),
        }]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: if self.recording {
                "Scrawl: recording".into()
            } else {
                "Scrawl".into()
            },
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let mut menu = Vec::new();
        for (label, item) in rows(self.recording, &self.shortcut) {
            if item == TrayItem::Quit {
                menu.push(ksni::MenuItem::Separator);
            }
            menu.push(
                ksni::menu::StandardItem {
                    label,
                    activate: Box::new(move |tray: &mut Self| (tray.on_item)(item)),
                    ..Default::default()
                }
                .into(),
            );
        }
        menu
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        scrawl_log(format!("Tray: the tray host went away ({reason:?})"));
        true
    }
}

/// A change for the tray's worker.
enum Update {
    Recording(bool),
    Shortcut(String),
}

/// The tray, run by a worker thread: ksni's blocking calls wait on the session bus, and a
/// wedged bus must not freeze the UI. `set_recording` and `set_shortcut` only queue changes.
pub struct Tray {
    updates: flume::Sender<Update>,
    recording: bool,
    shortcut: String,
}

impl Tray {
    /// Starts the worker, which shows the tray or logs why it can't (no tray host on the bus).
    pub fn start(shortcut: String, on_item: impl Fn(TrayItem) + Send + Sync + 'static) -> Self {
        let (updates, queue) = flume::unbounded();
        let tray = ScrawlTray {
            recording: false,
            shortcut: shortcut.clone(),
            on_item: Arc::new(on_item),
        };
        let spawned = std::thread::Builder::new()
            .name("scrawl-tray".into())
            .spawn(move || run(tray, &queue));
        if let Err(error) = spawned {
            scrawl_log(format!("Tray: could not start its thread: {error}"));
        }
        Self {
            updates,
            recording: false,
            shortcut,
        }
    }

    pub fn set_recording(&mut self, recording: bool) {
        if recording != self.recording {
            self.recording = recording;
            let _ = self.updates.send(Update::Recording(recording));
        }
    }

    /// The record shortcut changed (in the review window).
    pub fn set_shortcut(&mut self, shortcut: &str) {
        if shortcut != self.shortcut {
            self.shortcut = shortcut.to_owned();
            let _ = self.updates.send(Update::Shortcut(shortcut.to_owned()));
        }
    }
}

/// The worker: shows the tray, then applies changes until the app goes away.
fn run(tray: ScrawlTray, queue: &flume::Receiver<Update>) {
    let handle = match tray.spawn() {
        Ok(handle) => handle,
        Err(error) => {
            scrawl_log(format!(
                "Tray: none ({error}); is a StatusNotifierItem host (waybar's tray) running?"
            ));
            return;
        }
    };
    while let Ok(update) = queue.recv() {
        handle.update(|tray| match update {
            Update::Recording(recording) => tray.recording = recording,
            Update::Shortcut(shortcut) => tray.shortcut = shortcut,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_follows_the_recording() {
        let idle = rows(false, "⌥⇧S");
        assert_eq!(idle[0], ("Record Screen  ⌥⇧S".to_owned(), TrayItem::Record));
        assert_eq!(idle[1].1, TrayItem::OpenLatest);
        assert_eq!(
            idle.last().unwrap(),
            &("Quit Scrawl".to_owned(), TrayItem::Quit)
        );
        let recording = rows(true, "⌥⇧S");
        assert_eq!(
            recording[0],
            ("Stop Recording  ⌥⇧S".to_owned(), TrayItem::Record)
        );
        assert_eq!(recording.len(), idle.len());
    }

    fn pixel(data: &[u8], size: i32, x: i32, y: i32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [data[i], data[i + 1], data[i + 2], data[i + 3]]
    }

    #[test]
    fn the_icon_is_a_ring_idle_and_a_red_dot_recording() {
        let idle = icon_pixels(false, ICON);
        assert_eq!(idle.len(), (ICON * ICON * 4) as usize);
        // Corners are clear; the centre of the ring is clear; the ring itself is opaque.
        assert_eq!(pixel(&idle, ICON, 0, 0)[0], 0);
        assert_eq!(pixel(&idle, ICON, ICON / 2, ICON / 2)[0], 0);
        assert_eq!(pixel(&idle, ICON, ICON / 2, 2)[0], 255);
        let recording = icon_pixels(true, ICON);
        let [a, r, g, b] = pixel(&recording, ICON, ICON / 2, ICON / 2);
        assert_eq!(a, 255);
        assert!(r > 200 && g < 100 && b < 100);
        assert_eq!(pixel(&recording, ICON, 0, 0)[0], 0);
    }
}
