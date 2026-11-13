//! The Settings window (⌘, / Ctrl+,): one per app, brought forward if it's
//! already open. Changes apply as they're made; there's no Save button.
//!
//! Two pages: Appearance (light or dark, theme, zoom) and Keyboard Shortcuts,
//! where clicking a command's shortcut records a new one.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, Sizable, ThemeRegistry, TitleBar, h_flex, try_parse_color,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::keymap::{self, COMMANDS, Command};
use crate::settings::{self, Appearance, SettingsStore, ZOOM_STEPS, step_zoom};
use crate::theme::{self, THEMES, ThemeFamily};

/// The open Settings window and its view, if any. The window's root is
/// gpui-kit's `Root`, which wraps the view, so the two are kept apart.
#[derive(Default)]
struct SettingsWindow(Option<(AnyWindowHandle, Entity<SettingsView>)>);

impl Global for SettingsWindow {}

/// Opens the Settings window, or brings it forward if it's open.
pub fn open(cx: &mut App) {
    if let Some((handle, _)) = cx.try_global::<SettingsWindow>().and_then(|w| w.0.clone())
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    let bounds = Bounds::centered(None, size(px(760.), px(560.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(560.), px(400.))),
        titlebar: Some(TitlebarOptions {
            title: Some("Settings".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(12.), px(17.))),
        }),
        window_decorations: Some(WindowDecorations::Client),
        app_id: Some(crate::platform::BUNDLE_ID.into()),
        ..TitleBar::window_options()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| SettingsView::new(window, cx))
    }) {
        Ok((handle, view)) => {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
            cx.set_global(SettingsWindow(Some((handle, view))));
        }
        Err(err) => eprintln!("couldn't open Settings: {err:#}"),
    }
}

/// Forgets the Settings window once it's closed. Called for every closed window.
pub fn closed(id: WindowId, cx: &mut App) {
    if cx
        .try_global::<SettingsWindow>()
        .and_then(|w| w.0.as_ref())
        .is_some_and(|(handle, _)| handle.window_id() == id)
    {
        cx.set_global(SettingsWindow(None));
    }
}

pub fn is_open(cx: &App) -> bool {
    cx.try_global::<SettingsWindow>()
        .is_some_and(|w| w.0.is_some())
}

/// A page of Settings, picked in the sidebar.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Appearance,
    Shortcuts,
}

impl Page {
    const ALL: [Page; 2] = [Page::Appearance, Page::Shortcuts];

    fn name(self) -> &'static str {
        match self {
            Page::Appearance => "Appearance",
            Page::Shortcuts => "Keyboard Shortcuts",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Page::Appearance => "icons/palette.svg",
            Page::Shortcuts => "icons/keyboard.svg",
        }
    }
}

/// The command whose next keystroke becomes its shortcut. While this lives,
/// its subscription takes every keystroke before the keymap sees it.
struct Recording {
    id: &'static str,
    _keystrokes: Subscription,
}

/// A line under a command's name: why a key was refused, or what else changed.
struct Note {
    id: &'static str,
    text: SharedString,
    problem: bool,
}

/// The window's content.
pub struct SettingsView {
    focus_handle: FocusHandle,
    page: Page,
    recording: Option<Recording>,
    note: Option<Note>,
    _subscriptions: [Subscription; 2],
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            settings::apply_theme(Some(window), cx);
            cx.notify();
        });
        // The menus describe the active window, and this one has no document.
        // Leaving the window ends a recording, so no other window loses keys.
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                crate::window_activated(window.window_handle().window_id(), cx);
            } else {
                this.stop_recording(cx);
            }
        });
        Self {
            focus_handle,
            page: Page::Appearance,
            recording: None,
            note: None,
            _subscriptions: [appearance, activation],
        }
    }

    /// Shows `page`, as a click in the sidebar does.
    pub fn show_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        self.note = None;
        self.stop_recording(cx);
    }

    /// Starts recording a shortcut for command `id`: the next keystroke in
    /// this window goes to [`Self::record`] and nowhere else, so ⌘Q doesn't quit.
    pub fn start_recording(&mut self, id: &'static str, window: &Window, cx: &mut Context<Self>) {
        let view = cx.weak_entity();
        let this_window = window.window_handle().window_id();
        let keystrokes = cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != this_window {
                return;
            }
            cx.stop_propagation();
            let keystroke = event.keystroke.clone();
            view.update(cx, |view, cx| view.record(&keystroke, cx)).ok();
        });
        self.recording = Some(Recording {
            id,
            _keystrokes: keystrokes,
        });
        self.note = None;
        cx.notify();
    }

    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        if self.recording.take().is_some() {
            self.note = None;
        }
        cx.notify();
    }

    /// Takes a keystroke pressed while recording: Escape cancels, a key that
    /// can't be a shortcut gets a hint, and anything else becomes the shortcut.
    pub fn record(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) {
        let Some(id) = self.recording.as_ref().map(|recording| recording.id) else {
            return;
        };
        if keystroke.key == "escape" && !keystroke.modifiers.modified() {
            return self.stop_recording(cx);
        }
        if !keymap::can_bind(keystroke) {
            let text = if cfg!(target_os = "macos") {
                "Hold ⌘, ⌃ or ⌥ with a key, or press a function key."
            } else {
                "Hold Ctrl or Alt with a key, or press a function key."
            };
            self.note = Some(Note {
                id,
                text: text.into(),
                problem: true,
            });
            return cx.notify();
        }
        self.recording = None;
        self.note = keymap::assign(id, Some(keystroke), cx).map(|other| Note {
            id,
            // Menu names end in an ellipsis ("Export…"), which a sentence drops.
            text: format!("Also removed from {}.", other.trim_end_matches('…')).into(),
            problem: false,
        });
        cx.notify();
    }

    /// Runs `change` on the keymap for one command, ending any recording.
    fn change_shortcut(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut App)) {
        self.recording = None;
        self.note = None;
        change(cx);
        cx.notify();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(theme::scaled(190., cx))
            .flex_none()
            .h_full()
            .p(px(8.))
            .gap(px(2.))
            .bg(theme::palette(cx).chrome)
            .border_r_1()
            .border_color(cx.theme().border)
            .children(Page::ALL.into_iter().enumerate().map(|(ix, page)| {
                let selected = page == self.page;
                h_flex()
                    .id(("page", ix))
                    .w_full()
                    .px(px(8.))
                    .py(px(6.))
                    .gap(px(8.))
                    .items_center()
                    .rounded(px(6.))
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |this| {
                        this.bg(cx.theme().secondary)
                            .font_weight(FontWeight::MEDIUM)
                    })
                    .when(!selected, |this| {
                        this.text_color(cx.theme().muted_foreground)
                            .hover(|s| s.bg(cx.theme().secondary_hover.opacity(0.6)))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.show_page(page, cx)))
                    .child(
                        Icon::empty()
                            .path(page.icon())
                            .size(theme::scaled(14., cx))
                            .flex_shrink_0(),
                    )
                    .child(div().min_w_0().truncate().child(page.name()))
            }))
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = settings::current(cx);
        let modes = [
            (Appearance::System, "System", "icons/monitor.svg"),
            (Appearance::Light, "Light", "icons/sun.svg"),
            (Appearance::Dark, "Dark", "icons/moon.svg"),
        ];
        let mode = h_flex()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(8.))
            .bg(cx.theme().secondary)
            .children(
                modes
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (mode, name, icon))| {
                        let selected = settings.appearance == mode;
                        h_flex()
                            .id(("mode", ix))
                            .px(px(10.))
                            .py(px(3.))
                            .gap(px(6.))
                            .items_center()
                            .rounded(px(6.))
                            .text_sm()
                            .cursor_pointer()
                            .when(selected, |this| {
                                this.bg(cx.theme().background)
                                    .shadow_sm()
                                    .font_weight(FontWeight::MEDIUM)
                            })
                            .when(!selected, |this| {
                                this.text_color(cx.theme().muted_foreground)
                                    .hover(|s| s.text_color(cx.theme().foreground))
                            })
                            .on_click(cx.listener(move |_, _, _, cx| {
                                SettingsStore::edit(cx, |s| s.appearance = mode)
                            }))
                            .child(Icon::empty().path(icon).size(theme::scaled(13., cx)))
                            .child(name)
                    }),
            );

        let chosen = theme::family(&settings.theme).name;
        let themes = h_flex()
            .w_full()
            .gap(px(12.))
            .children(THEMES.iter().enumerate().map(|(ix, family)| {
                theme_card(family, family.name == chosen, cx)
                    .id(("theme", ix))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        SettingsStore::edit(cx, |s| s.theme = family.name.into())
                    }))
            }));

        let zoom = settings.zoom;
        let step = |id: &'static str, icon: &'static str, by: i32, cx: &mut Context<Self>| {
            Button::new(id)
                .outline()
                .small()
                .icon(Icon::empty().path(icon))
                .disabled(step_zoom(zoom, by) == zoom)
                .on_click(cx.listener(move |_, _, _, cx| {
                    SettingsStore::edit(cx, |s| s.zoom = step_zoom(s.zoom, by))
                }))
        };
        let zoom_control = h_flex()
            .gap(px(6.))
            .items_center()
            .when(zoom != 1.0, |this| {
                this.child(
                    Button::new("zoom-reset")
                        .ghost()
                        .small()
                        .label("Reset")
                        .on_click(
                            cx.listener(|_, _, _, cx| SettingsStore::edit(cx, |s| s.zoom = 1.0)),
                        ),
                )
            })
            .child(step("zoom-out", "icons/minus.svg", -1, cx))
            .child(
                div()
                    .w(theme::scaled(46., cx))
                    .text_center()
                    .text_sm()
                    .font_features(theme::tabular())
                    .child(format!("{:.0}%", zoom * 100.)),
            )
            .child(step("zoom-in", "icons/plus.svg", 1, cx));
        // "⌘= in · ⌘- out · ⌘0 actual size", leaving out commands with no shortcut.
        let zoom_keys = [
            ("zoom_in", "in"),
            ("zoom_out", "out"),
            ("reset_zoom", "actual size"),
        ]
        .into_iter()
        .filter_map(|(id, does)| Some(format!("{} {does}", keymap::label(id, cx)?)))
        .collect::<Vec<_>>()
        .join("  ·  ");
        let range = format!(
            "{:.0}% to {:.0}%, in every window.",
            ZOOM_STEPS[0] * 100.,
            ZOOM_STEPS[ZOOM_STEPS.len() - 1] * 100.
        );

        v_flex()
            .child(page_title("Appearance", cx))
            .child(setting(
                "Mode",
                "Follow the system, or stay light or dark.",
                mode,
                cx,
            ))
            .child(
                v_flex()
                    .py(px(14.))
                    .gap(px(10.))
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(setting_label(
                        "Theme",
                        "Each theme has a light and a dark side.",
                        cx,
                    ))
                    .child(themes),
            )
            .child(setting(
                "Zoom",
                if zoom_keys.is_empty() {
                    range
                } else {
                    zoom_keys
                },
                zoom_control,
                cx,
            ))
    }

    fn render_shortcuts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let changed = !settings::current(cx).shortcuts.is_empty();
        let mut groups: Vec<(&'static str, Vec<(usize, &'static Command)>)> = Vec::new();
        for (ix, command) in COMMANDS.iter().enumerate() {
            match groups.last_mut() {
                Some((group, commands)) if *group == command.group => commands.push((ix, command)),
                _ => groups.push((command.group, vec![(ix, command)])),
            }
        }
        v_flex()
            .child(
                h_flex()
                    .items_start()
                    .gap(px(12.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(page_title("Keyboard Shortcuts", cx))
                            .child(
                                div()
                                    .mt(px(-8.))
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Click a shortcut, then press the keys you want."),
                            ),
                    )
                    .child(
                        Button::new("reset-all")
                            .outline()
                            .small()
                            .label("Reset All")
                            .disabled(!changed)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.change_shortcut(cx, keymap::reset_all)
                            })),
                    ),
            )
            .children(groups.into_iter().map(|(group, commands)| {
                v_flex()
                    .pt(px(18.))
                    .child(
                        div()
                            .pb(px(6.))
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().muted_foreground)
                            .child(group.to_uppercase()),
                    )
                    .child(
                        v_flex()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(cx.theme().border)
                            .overflow_hidden()
                            .children(commands.into_iter().enumerate().map(
                                |(row, (ix, command))| {
                                    self.render_command(ix, command, cx).when(row > 0, |this| {
                                        this.border_t_1().border_color(cx.theme().border)
                                    })
                                },
                            )),
                    )
            }))
    }

    /// One command: its name, its shortcuts as key caps (click to record),
    /// and buttons to remove the shortcut or put the default back.
    fn render_command(&self, ix: usize, command: &'static Command, cx: &mut Context<Self>) -> Div {
        let id = command.id;
        let muted = cx.theme().muted_foreground;
        let accent = theme::palette(cx).accent;
        let recording = self.recording.as_ref().is_some_and(|r| r.id == id);
        let keys = keymap::shortcuts(command, cx);
        let note = self.note.as_ref().filter(|note| note.id == id);
        let line = match note {
            Some(note) => Some((note.text.clone(), note.problem)),
            None if recording => Some(("Press the new shortcut. Esc cancels.".into(), false)),
            None => None,
        };

        let caps = h_flex()
            .id(("keys", ix))
            // Lets tests find and click a command's shortcut.
            .debug_selector(move || format!("shortcut-{id}"))
            .gap(px(4.))
            .items_center()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.recording.as_ref().is_some_and(|r| r.id == id) {
                    this.stop_recording(cx);
                } else {
                    this.start_recording(id, window, cx);
                }
            }));
        let caps = if recording {
            caps.child(
                key_cap("Recording…", cx)
                    .border_color(accent)
                    .bg(accent.opacity(0.12))
                    .text_color(cx.theme().foreground),
            )
        } else if keys.is_empty() {
            caps.child(
                key_cap("Add", cx)
                    .bg(transparent_black())
                    .border_dashed()
                    .text_color(muted.opacity(0.8)),
            )
        } else {
            caps.children(
                keys.iter()
                    .map(|keys| key_cap(keymap::display(keys, cx), cx)),
            )
        };

        h_flex()
            .group("command")
            .w_full()
            .min_h(theme::scaled(34., cx))
            .px(px(12.))
            .py(px(5.))
            .gap(px(6.))
            .items_center()
            .when(recording, |this| this.bg(cx.theme().secondary.opacity(0.5)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(command.name))
                    .when_some(line, |this, (text, problem)| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(if problem {
                                    theme::palette(cx).warning
                                } else {
                                    muted
                                })
                                .child(text),
                        )
                    }),
            )
            .when(!keys.is_empty() && !recording, |this| {
                this.child(
                    div()
                        .opacity(0.)
                        .group_hover("command", |s| s.opacity(1.))
                        .child(
                            Button::new(("remove", ix))
                                .ghost()
                                .xsmall()
                                .icon(Icon::empty().path("icons/x.svg"))
                                .tooltip("Remove shortcut")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.change_shortcut(cx, |cx| {
                                        keymap::assign(id, None, cx);
                                    })
                                })),
                        ),
                )
            })
            .when(!keymap::is_default(command, cx) && !recording, |this| {
                let default = command
                    .defaults
                    .first()
                    .map(|keys| keymap::display(keys, cx));
                this.child(
                    Button::new(("reset", ix))
                        .ghost()
                        .xsmall()
                        .icon(Icon::empty().path("icons/undo-2.svg"))
                        .tooltip(match default {
                            Some(keys) => format!("Reset to {keys}"),
                            None => "Reset to no shortcut".into(),
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.change_shortcut(cx, |cx| keymap::reset(id, cx))
                        })),
                )
            })
            .child(caps)
    }
}

/// A page's heading.
fn page_title(title: &'static str, cx: &App) -> Div {
    div()
        .pb(px(10.))
        .text_base()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(cx.theme().foreground)
        .child(title)
}

/// A setting's name with a line saying what it does.
fn setting_label(name: &'static str, about: impl Into<SharedString>, cx: &App) -> Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap(px(1.))
        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(name))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(about.into()),
        )
}

/// A row with a setting's label on the left and its control on the right.
fn setting(
    name: &'static str,
    about: impl Into<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> Div {
    h_flex()
        .py(px(14.))
        .gap(px(16.))
        .items_center()
        .border_t_1()
        .border_color(cx.theme().border)
        .child(setting_label(name, about, cx))
        .child(control)
}

/// A key cap showing one shortcut, like `⇧⌘E`.
fn key_cap(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .px(px(7.))
        .py(px(1.))
        .min_w(theme::scaled(24., cx))
        .rounded(px(5.))
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().secondary.opacity(0.6))
        .text_xs()
        .text_center()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().foreground.opacity(0.85))
        .child(text.into())
}

/// A theme's card: a small picture of a table and a chart in its colours for
/// the current appearance, with its name below.
fn theme_card(family: &'static ThemeFamily, selected: bool, cx: &App) -> Div {
    let dark = cx.theme().is_dark();
    let config = ThemeRegistry::global(cx)
        .themes()
        .get(if dark { family.dark } else { family.light })
        .cloned();
    let color = |value: Option<&SharedString>, fallback: Hsla| {
        value
            .and_then(|value| try_parse_color(value).ok())
            .unwrap_or(fallback)
    };
    let colors = config.as_ref().map(|config| &config.colors);
    let background = color(
        colors.and_then(|c| c.background.as_ref()),
        cx.theme().background,
    );
    let ink = color(
        colors.and_then(|c| c.foreground.as_ref()),
        cx.theme().foreground,
    );
    let border = color(colors.and_then(|c| c.border.as_ref()), cx.theme().border);
    let accent: Hsla = rgb(if dark {
        family.accent.1
    } else {
        family.accent.0
    })
    .into();
    let ring = theme::palette(cx).accent;

    let line = |width: f32, opacity: f32| {
        div()
            .w(relative(width))
            .h(px(4.))
            .rounded(px(2.))
            .bg(ink.opacity(opacity))
    };
    let picture = h_flex()
        .w_full()
        .h(theme::scaled(62., cx))
        .p(px(9.))
        .gap(px(10.))
        .rounded(px(6.))
        .border_1()
        .border_color(border)
        .bg(background)
        .child(
            v_flex()
                .flex_1()
                .h_full()
                .justify_between()
                .child(line(0.55, 0.75))
                .child(div().w_full().h(px(1.)).bg(border))
                .child(line(0.9, 0.3))
                .child(line(0.7, 0.3))
                .child(line(0.8, 0.3)),
        )
        .child(h_flex().h_full().gap(px(3.)).items_end().children(
            [0.35, 0.7, 1.0, 0.55, 0.25].into_iter().map(|share| {
                div()
                    .w(px(6.))
                    .h(relative(share))
                    .rounded_t(px(1.5))
                    .bg(accent.opacity(if dark { 0.75 } else { 0.65 }))
            }),
        ));

    v_flex()
        .flex_1()
        .min_w_0()
        .gap(px(6.))
        .cursor_pointer()
        .child(
            // The ring is always there, so picking a theme moves nothing.
            div()
                .p(px(2.))
                .rounded(px(9.))
                .border_2()
                .border_color(if selected { ring } else { transparent_black() })
                .child(picture),
        )
        .child(
            div()
                .px(px(4.))
                .text_xs()
                .truncate()
                .when(selected, |this| this.font_weight(FontWeight::MEDIUM))
                .when(!selected, |this| {
                    this.text_color(cx.theme().muted_foreground)
                })
                .child(family.name),
        )
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = match self.page {
            Page::Appearance => self.render_appearance(cx).into_any_element(),
            Page::Shortcuts => self.render_shortcuts(cx).into_any_element(),
        };
        div()
            .track_focus(&self.focus_handle)
            // Minimize is bound app-wide but handled per window; Close Window
            // falls back to the app's handler.
            .on_action(|_: &crate::Minimize, window, _| window.minimize_window())
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new()
                    .h(theme::scaled(theme::TOOLBAR_HEIGHT, cx))
                    .children(crate::menu_bar(window, cx))
                    .child(
                        h_flex()
                            .w_full()
                            .h_full()
                            .justify_center()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child("Settings"),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_sidebar(cx))
                    .child(
                        // An id per page, so each keeps its own scroll position.
                        div()
                            .id(match self.page {
                                Page::Appearance => "appearance",
                                Page::Shortcuts => "shortcuts",
                            })
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .child(
                                div()
                                    .max_w(theme::scaled(640., cx))
                                    .mx_auto()
                                    .px(px(24.))
                                    .pt(px(18.))
                                    .pb(px(24.))
                                    .child(page),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    /// Real keystrokes and clicks through the window's event dispatch: the
    /// zoom and Settings shortcuts, then recording a new shortcut, where the
    /// keys pressed must not run the command they're bound to (⌘Q here).
    /// Debug builds only, as GPUI records the click selectors only there.
    #[cfg(all(target_os = "macos", debug_assertions))]
    #[test]
    fn shortcuts_from_the_keyboard() {
        use std::cell::Cell;
        use std::rc::Rc;
        use std::sync::Arc;

        use gpui_kit::component::ActiveTheme;
        use gpui_kit::{
            AppContext, Modifiers, TestAppContext, TestDispatcher, VisualTestContext, px,
        };

        use super::{Page, SettingsWindow};
        use crate::keymap::{self, command};
        use crate::settings::{self, SettingsStore};

        let dir = std::env::temp_dir().join(format!("da-settings-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            Arc::new(gpui_apple::AppleTextSystem::new()),
        );
        let app_cx = cx.clone();
        let quits = Rc::new(Cell::new(0));
        let window = app_cx.update(|cx| {
            gpui_kit::init(cx);
            crate::filter::init(cx);
            crate::import::init(cx);
            crate::document::init(cx);
            settings::init(dir.join("settings.json"), cx);
            let counter = quits.clone();
            cx.on_action(move |_: &crate::Quit, _| counter.set(counter.get() + 1));
            cx.set_global(crate::Windows::default());
            cx.set_global(crate::recent::RecentStore::load(dir.join("recent.json")));
            let opened = gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| crate::workspace::Workspace::new(None, window, cx))
            })
            .unwrap();
            cx.global_mut::<crate::Windows>().0.push(opened.clone());
            opened.0
        });
        let mut cx = VisualTestContext::from_window(window, &cx);
        let zoom = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| (settings::current(cx).zoom, cx.theme().font_size))
        };

        cx.simulate_keystrokes("cmd-=");
        assert_eq!(zoom(&mut cx), (1.1, px(16. * 1.1)));
        cx.simulate_keystrokes("cmd-= cmd-0");
        assert_eq!(zoom(&mut cx), (1.0, px(16.)));
        // The saved file has the last zoom.
        let saved = settings::Settings::load(&dir.join("settings.json"));
        assert_eq!(saved.zoom, 1.0);
        cx.simulate_keystrokes("cmd-,");
        let (settings_window, view) = app_cx
            .update(|cx| cx.global::<SettingsWindow>().0.clone())
            .expect("⌘, opened Settings");
        let mut cx = VisualTestContext::from_window(settings_window, &app_cx);
        cx.update(|_, cx| view.update(cx, |view, cx| view.show_page(Page::Shortcuts, cx)));
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds("shortcut-find")
            .expect("Find's shortcut is on screen");
        cx.simulate_click(bounds.center(), Modifiers::none());
        // ⌘Q while recording becomes Find's shortcut instead of quitting,
        // and Quit, which had it, is left without one.
        cx.simulate_keystrokes("cmd-q");
        assert_eq!(quits.get(), 0, "⌘Q quit while recording");
        let (find, quit) = cx.update(|_, cx| {
            (
                keymap::shortcuts(command("find").unwrap(), cx),
                keymap::shortcuts(command("quit").unwrap(), cx),
            )
        });
        assert_eq!(find, ["cmd-q"]);
        assert!(quit.is_empty());

        // Recording is over: ⌘Q now goes to the keymap, where Quit no longer has it.
        cx.simulate_keystrokes("cmd-q");
        assert_eq!(quits.get(), 0);
        // Putting Quit back takes ⌘Q from Find rather than sharing it.
        cx.update(|_, cx| keymap::reset("quit", cx));
        let find = cx.update(|_, cx| keymap::shortcuts(command("find").unwrap(), cx));
        assert!(find.is_empty(), "Find kept ⌘Q: {find:?}");
        cx.simulate_keystrokes("cmd-q");
        assert_eq!(quits.get(), 1, "⌘Q didn't quit after resetting Quit");

        cx.update(|_, cx| keymap::reset_all(cx));
        assert!(app_cx.update(|cx| cx.global::<SettingsStore>().settings.shortcuts.is_empty()));
        let _ = std::fs::remove_dir_all(dir);
        // Dropping the app here deadlocks in gpui-base's text-layout cleanup
        // (a lock its own drop waits on), so the test leaks it instead.
        std::mem::forget(cx);
        std::mem::forget(app_cx);
    }
}
