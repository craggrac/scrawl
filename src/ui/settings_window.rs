//! The Linux Settings window. It draws `settings::View` and turns clicks and keys into
//! `settings::Action`s, like the SwiftUI window on macOS does through the helper; the `Panel`
//! decides and writes everything (`crate::settings`). Linux shows General, Recording, Storage,
//! Agents and About: no permissions to grant, no Hex, no video export, and no Paste shortcut.

use super::*;

use gpui::{ClickEvent, ClipboardItem, Modifiers, ModifiersChangedEvent, WeakEntity};

use crate::settings::{Action, AppHost, Effect, Pane, Panel, ShortcutKind, ShortcutView, View};

/// The panes this window offers, in the sidebar's order.
const PANES: [Pane; 5] = [
    Pane::General,
    Pane::Recording,
    Pane::Storage,
    Pane::Agents,
    Pane::About,
];

const SIDEBAR_W: f32 = 190.0;
const ROW_H: f32 = 42.0;
const GROUP_RADIUS: f32 = 12.0;
/// How long a Copy button says Copied.
const COPIED_FOR: Duration = Duration::from_millis(1_500);
const WEBSITE: &str = "https://scrawl.kitlangton.com";
const SOURCE: &str = "https://github.com/kitlangton/scrawl";

fn pane_title(pane: Pane) -> &'static str {
    match pane {
        Pane::General => "General",
        Pane::Recording => "Recording",
        Pane::Storage => "Storage",
        Pane::Permissions => "Permissions",
        Pane::Agents => "Agents",
        Pane::About => "About",
    }
}

/// A glyph and the colour of the sidebar tile (the Mac window's symbol and tint).
fn pane_tile(pane: Pane) -> (&'static str, u32) {
    match pane {
        Pane::General | Pane::Permissions => ("⚙", 0x6f6f6f),
        Pane::Recording => ("●", 0xeb3b30),
        Pane::Storage => ("▤", 0x2973f2),
        Pane::Agents => (">_", 0x383838),
        Pane::About => ("i", 0x6f6f6f),
    }
}

pub struct SettingsWindow {
    panel: Panel,
    /// What the panel shows now; read again after each action and when the window is entered.
    view: View,
    pane: Pane,
    /// Modifiers held now, lit on the recorder's keycaps while it listens.
    mods: Modifiers,
    /// The Copy button says Copied until then.
    copied_until: Option<Instant>,
    telemetry: Arc<SharedCaptureTelemetry>,
    focus: FocusHandle,
}

impl SettingsWindow {
    fn new(
        telemetry: Arc<SharedCaptureTelemetry>,
        pane: Pane,
        focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = Panel::default();
        let view = panel.view(&AppHost {
            telemetry: &telemetry,
        });
        // The size takes a walk over the captures folder.
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let bytes = cx
                .background_executor()
                .spawn(async {
                    crate::retention::captures_size(&crate::handoff::default_captures_dir())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.panel.set_captures_size(bytes);
                this.refresh(cx);
            });
        })
        .detach();
        // Something may have changed while another window had the keys (the shortcut set in the
        // review window, the autostart entry); and a recorder that lost the keys stops listening.
        cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.refresh(cx);
            } else {
                this.act(Action::CancelShortcut, window, cx);
            }
        })
        .detach();
        Self {
            panel,
            view,
            pane: if PANES.contains(&pane) {
                pane
            } else {
                Pane::General
            },
            mods: Modifiers::default(),
            copied_until: None,
            telemetry,
            focus,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.view = self.panel.view(&AppHost {
            telemetry: &self.telemetry,
        });
        cx.notify();
    }

    /// Does what the panel decides for `action`, then redraws.
    fn act(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        let effect = self.panel.on(
            action,
            &mut AppHost {
                telemetry: &self.telemetry,
            },
        );
        match effect {
            Effect::Close => window.remove_window(),
            Effect::ShowCaptures => {
                let dir = crate::handoff::default_captures_dir();
                let _ = std::fs::create_dir_all(&dir);
                crate::platform::open_in_file_manager(&dir);
            }
            _ => {}
        }
        self.refresh(cx);
    }

    fn recording(&self) -> bool {
        self.view
            .shortcuts
            .iter()
            .any(|shortcut| shortcut.recording)
    }

    fn set_bool(key: &'static str, on: bool) -> Action {
        Action::Set {
            key: key.to_owned(),
            value: on.into(),
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        if let Some(shortcut) = self.view.shortcuts.iter().find(|s| s.recording) {
            let action = if key == "escape" {
                Action::CancelShortcut
            } else {
                Action::Shortcut {
                    kind: shortcut.kind,
                    pressed: crate::settings::Pressed {
                        key: key.to_owned(),
                        cmd: mods.platform,
                        ctrl: mods.control,
                        alt: mods.alt,
                        shift: mods.shift,
                    },
                }
            };
            self.act(action, window, cx);
        } else if key == "escape" || (mods.platform && key.eq_ignore_ascii_case("w")) {
            self.act(Action::Close, window, cx);
        } else {
            return;
        }
        cx.stop_propagation();
    }

    // MARK: Parts

    fn sidebar(&self, cx: &mut Context<Self>) -> Div {
        div()
            .w(px(SIDEBAR_W))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px(px(10.0))
            .bg(rgb(SIDEBAR_BG))
            .border_r_1()
            .border_color(rgb(LINE_COLOR))
            .child(
                div()
                    .h(px(52.0))
                    .flex_none()
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()),
            )
            .children(PANES.into_iter().enumerate().map(|(index, pane)| {
                let selected = pane == self.pane;
                let (glyph, tint) = pane_tile(pane);
                div()
                    .id(("settings-pane", index))
                    .h(px(36.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .rounded(px(9.0))
                    .text_size(px(14.0))
                    .text_color(rgb(TEXT_COLOR))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(rgb(SURFACE_SELECTED)))
                    .when(!selected, |d| d.hover(|s| s.bg(rgb(SURFACE_HOVER))))
                    .child(
                        div()
                            .size(px(24.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .bg(rgb(tint))
                            .text_size(px(12.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(glyph),
                    )
                    .child(pane_title(pane))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        // A pane change ends a recorder that was listening.
                        this.act(Action::CancelShortcut, window, cx);
                        this.pane = pane;
                        cx.notify();
                    }))
            }))
    }

    fn content(&self, cx: &mut Context<Self>) -> Div {
        let body = match self.pane {
            Pane::General => self.general(cx),
            Pane::Recording => self.recording_pane(),
            Pane::Storage => self.storage(cx),
            Pane::Agents => self.agents(cx),
            Pane::About | Pane::Permissions => self.about(cx),
        };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .px(px(20.0))
            .child(
                div()
                    .h(px(52.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .text_size(px(16.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(pane_title(self.pane)),
            )
            .child(body)
    }

    fn general(&self, cx: &mut Context<Self>) -> Div {
        let login = &self.view.login;
        let (on, available) = (login.on, login.available);
        let mut pane = div().flex().flex_col().child(group().child(row(
            "Launch at login",
            switch("launch-at-login", on, available).on_click(cx.listener(
                move |this, _: &ClickEvent, window, cx| {
                    if available {
                        this.act(Self::set_bool("launchAtLogin", !on), window, cx);
                    }
                },
            )),
        )));
        pane = pane.child(caption(
            if available {
                "Starts Scrawl with your session: an entry in ~/.config/autostart that runs scrawl app."
            } else {
                "There's no config folder to put the autostart entry in."
            },
            false,
        ));
        for shortcut in &self.view.shortcuts {
            pane = pane.child(self.recorder(shortcut, cx));
        }
        pane
    }

    /// The keycaps of one shortcut; a click listens for a new one.
    fn recorder(&self, shortcut: &ShortcutView, cx: &mut Context<Self>) -> Div {
        let recording = shortcut.recording;
        let kind: ShortcutKind = shortcut.kind;
        // While listening the caps light as the keys go down.
        let lit = |held: bool, flag: bool| if recording { flag } else { held };
        let mods = self.mods;
        let caps = [
            ("⇧", "shift", lit(shortcut.shift, mods.shift)),
            ("⌃", "ctrl", lit(shortcut.control, mods.control)),
            ("⌥", "alt", lit(shortcut.option, mods.alt)),
            ("⌘", "super", lit(shortcut.command, mods.platform)),
        ];
        let caption_text = if recording {
            "Press the new shortcut. Esc cancels.".to_owned()
        } else if let Some(error) = &shortcut.error {
            error.clone()
        } else {
            "Tap to record, hold to choose what to record. Hyprland's bind for scrawl toggle \
             is separate."
                .to_owned()
        };
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(46.0))
                    .pt(px(18.0))
                    .px(px(4.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(MUTED_COLOR))
                    .child(shortcut.title)
                    .child(
                        div()
                            .id("reset-shortcut")
                            .h(px(20.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .rounded(px(10.0))
                            .bg(rgb(SURFACE_SELECTED))
                            .when(shortcut.is_default || recording, |d| d.invisible())
                            .when(!(shortcut.is_default || recording), |d| {
                                d.cursor_pointer()
                                    .hover(|s| s.text_color(rgb(TEXT_COLOR)))
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.act(Action::ResetShortcut { kind }, window, cx);
                                        },
                                    ))
                            })
                            .child("Reset"),
                    ),
            )
            .child(
                div()
                    .id("record-shortcut")
                    .p(px(7.0))
                    .flex()
                    .gap(px(6.0))
                    .rounded(px(GROUP_RADIUS + 2.0))
                    .bg(rgb(SURFACE_BG))
                    .border_1()
                    .border_color(rgb(if recording { ACCENT_COLOR } else { LINE_COLOR }))
                    .cursor_pointer()
                    .children(
                        caps.into_iter()
                            .map(|(glyph, name, lit)| keycap(glyph, name, lit)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h(px(52.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(9.0))
                            .bg(rgb(CANVAS_BG))
                            .border_1()
                            .border_color(rgb(LINE_COLOR))
                            .text_size(px(if shortcut.key.chars().count() > 2 {
                                14.0
                            } else {
                                20.0
                            }))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(TEXT_COLOR))
                            .child(if recording {
                                "…".to_owned()
                            } else {
                                shortcut.key.clone()
                            }),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        let action = if this.recording() {
                            Action::CancelShortcut
                        } else {
                            Action::RecordShortcut { kind }
                        };
                        this.act(action, window, cx);
                    })),
            )
            .child(caption(
                &caption_text,
                shortcut.error.is_some() && !recording,
            ))
    }

    fn recording_pane(&self) -> Div {
        let parakeet = &self.view.parakeet;
        div()
            .flex()
            .flex_col()
            .child(
                group().child(row(
                    "Transcription",
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(14.0))
                        .child(div().text_color(rgb(TEXT_SOFT)).child("Parakeet"))
                        .child(
                            div()
                                .size(px(8.0))
                                .rounded_full()
                                .bg(rgb(if parakeet.ready {
                                    POSITIVE_COLOR
                                } else {
                                    INK_RED
                                })),
                        ),
                )),
            )
            .child(caption(&parakeet.detail, !parakeet.ready))
    }

    fn storage(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .child(
                group().child(row(
                    "Captures folder",
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .child(
                            div()
                                .text_size(px(14.0))
                                .text_color(rgb(MUTED_COLOR))
                                .child(
                                    self.view
                                        .captures_size
                                        .clone()
                                        .unwrap_or_else(|| "Measuring…".to_owned()),
                                ),
                        )
                        .child(
                            action_button(
                                "show-captures",
                                crate::platform::SHOW_IN_FILE_MANAGER,
                                false,
                            )
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, window, cx| {
                                    this.act(Action::ShowCaptures, window, cx);
                                },
                            )),
                        ),
                )),
            )
            .child(caption(
                "Captures stay until you delete them; Scrawl doesn't clean them up on Linux.",
                false,
            ))
    }

    fn agents(&self, cx: &mut Context<Self>) -> Div {
        let command = self.view.skill_command;
        let copied = self
            .copied_until
            .is_some_and(|until| until > Instant::now());
        div()
            .flex()
            .flex_col()
            .child(heading("Agent skill"))
            .child(
                group().child(
                    div()
                        .h(px(ROW_H))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(12.0))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .font_family("monospace")
                                .child(command),
                        )
                        .child(
                            action_button(
                                "copy-skill",
                                if copied { "Copied" } else { "Copy" },
                                false,
                            )
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        command.to_owned(),
                                    ));
                                    this.copied_until = Some(Instant::now() + COPIED_FOR);
                                    cx.notify();
                                    cx.spawn(async move |this: WeakEntity<Self>, cx| {
                                        cx.background_executor().timer(COPIED_FOR).await;
                                        let _ = this.update(cx, |_, cx| cx.notify());
                                    })
                                    .detach();
                                },
                            )),
                        ),
                ),
            )
            .child(caption(
                "Run it in a terminal. It teaches Claude Code, OpenCode, Codex and other agents \
                 to read your captures and use scrawl.",
                false,
            ))
    }

    fn about(&self, cx: &mut Context<Self>) -> Div {
        let link =
            |id: &'static str, title: &'static str, shown: &'static str, url: &'static str| {
                div()
                    .id(id)
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(SURFACE_HOVER)))
                    .child(row(
                        title,
                        div()
                            .text_size(px(14.0))
                            .text_color(rgb(MUTED_COLOR))
                            .child(format!("{shown} ↗")),
                    ))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.open_url(url)))
            };
        div()
            .flex()
            .flex_col()
            .child(
                group().child(
                    div()
                        .h(px(64.0))
                        .px(px(12.0))
                        .flex()
                        .flex_col()
                        .justify_center()
                        .gap(px(1.0))
                        .child(
                            div()
                                .text_size(px(14.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Scrawl"),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(MUTED_COLOR))
                                .child(format!("Version {}", self.view.version)),
                        ),
                ),
            )
            .child(heading("Links"))
            .child(
                group()
                    .child(link(
                        "link-website",
                        "Website",
                        "scrawl.kitlangton.com",
                        WEBSITE,
                    ))
                    .child(separator())
                    .child(link(
                        "link-source",
                        "Source",
                        "github.com/kitlangton/scrawl",
                        SOURCE,
                    )),
            )
    }
}

impl Drop for SettingsWindow {
    /// A recorder that was listening hands the shortcut back.
    fn drop(&mut self) {
        self.panel.on(
            Action::Close,
            &mut AppHost {
                telemetry: &self.telemetry,
            },
        );
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("scrawl-settings-root")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                this.mods = event.modifiers;
                cx.notify();
            }))
            .size_full()
            .flex()
            .bg(rgb(CANVAS_BG))
            .text_color(rgb(TEXT_COLOR))
            .child(self.sidebar(cx))
            .child(self.content(cx))
    }
}

// MARK: Controls

fn group() -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(GROUP_RADIUS))
        .bg(rgb(SURFACE_BG))
        .border_1()
        .border_color(rgb(LINE_COLOR))
        .overflow_hidden()
}

fn separator() -> Div {
    div().h(px(1.0)).mx(px(12.0)).bg(rgb(LINE_COLOR))
}

fn row(title: &'static str, control: impl IntoElement) -> Div {
    div()
        .h(px(ROW_H))
        .px(px(12.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .text_size(px(14.0))
        .child(title)
        .child(control)
}

fn heading(label: &'static str) -> Div {
    div()
        .h(px(46.0))
        .pt(px(18.0))
        .px(px(4.0))
        .flex()
        .items_center()
        .text_size(px(12.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED_COLOR))
        .child(label)
}

fn caption(text: &str, error: bool) -> Div {
    div()
        .px(px(4.0))
        .pt(px(6.0))
        .text_size(px(12.0))
        .line_height(relative(1.35))
        .text_color(rgb(if error { INK_RED } else { MUTED_COLOR }))
        .child(SharedString::from(text.to_owned()))
}

fn switch(id: &'static str, on: bool, enabled: bool) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .w(px(38.0))
        .h(px(22.0))
        .flex_none()
        .px(px(2.0))
        .flex()
        .items_center()
        .rounded_full()
        .bg(rgb(if on { ACCENT_COLOR } else { SURFACE_SELECTED }))
        .when(on, |d| d.justify_end())
        .when(enabled, |d| d.cursor_pointer())
        .when(!enabled, |d| d.opacity(0.4))
        .child(div().size(px(18.0)).rounded_full().bg(rgb(0xffffff)))
}

/// A modifier's cap: its symbol and name, bright while it is part of the shortcut.
fn keycap(glyph: &'static str, name: &'static str, lit: bool) -> Div {
    div()
        .flex_1()
        .h(px(52.0))
        .px(px(10.0))
        .py(px(8.0))
        .flex()
        .flex_col()
        .justify_between()
        .rounded(px(9.0))
        .bg(rgb(CANVAS_BG))
        .border_1()
        .border_color(rgb(LINE_COLOR))
        .text_color(rgb(if lit { TEXT_COLOR } else { 0x5a5a5a }))
        .child(div().text_size(px(15.0)).child(glyph))
        .child(div().text_size(px(12.0)).child(name))
}

/// Opens the Settings window, or brings the open one forward.
pub(super) fn open_settings_window(
    coord_entity: &Entity<ScrawlCoordinator>,
    pane: Option<Pane>,
    cx: &mut App,
) {
    for handle in cx.windows() {
        if let Some(settings) = handle.downcast::<SettingsWindow>() {
            cx.activate(true);
            let _ = settings.update(cx, |this, window, cx| {
                if let Some(pane) = pane.filter(|pane| PANES.contains(pane)) {
                    this.pane = pane;
                }
                window.activate_window();
                this.refresh(cx);
            });
            return;
        }
    }
    let telemetry = Arc::clone(&coord_entity.read(cx).telemetry);
    let bounds = Bounds::centered(None, size(px(SETTINGS_WINDOW_W), px(SETTINGS_WINDOW_H)), cx);
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Scrawl Settings".into()),
                appears_transparent: true,
                traffic_light_position: None,
            }),
            focus: true,
            show: true,
            kind: WindowKind::Normal,
            is_resizable: false,
            window_background: WindowBackgroundAppearance::Opaque,
            app_id: Some(SETTINGS_APP_ID.into()),
            ..Default::default()
        },
        move |window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus);
            cx.new(|cx| {
                SettingsWindow::new(telemetry, pane.unwrap_or(Pane::General), focus, window, cx)
            })
        },
    );
    cx.activate(true);
}
