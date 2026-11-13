//! The Settings window (⌘, / Ctrl+,): one per app, brought forward if it's
//! already open. Changes apply as they're made; there's no Save button.

use gpui_kit::component::{ActiveTheme, TitleBar, h_flex};
use gpui_kit::*;

/// The open Settings window, if any.
#[derive(Default)]
struct SettingsWindow(Option<WindowHandle<SettingsView>>);

impl Global for SettingsWindow {}

/// Opens the Settings window, or brings it forward if it's open.
pub fn open(cx: &mut App) {
    if let Some(handle) = cx.try_global::<SettingsWindow>().and_then(|w| w.0) {
        if handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            return;
        }
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
        Ok((handle, _)) => {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
            cx.set_global(SettingsWindow(Some(
                handle.downcast().expect("a settings window"),
            )));
        }
        Err(err) => eprintln!("couldn't open Settings: {err:#}"),
    }
}

/// Forgets the Settings window once it's closed. Called for every closed window.
pub fn closed(id: WindowId, cx: &mut App) {
    if cx
        .try_global::<SettingsWindow>()
        .and_then(|w| w.0)
        .is_some_and(|handle| handle.window_id() == id)
    {
        cx.set_global(SettingsWindow(None));
    }
}

pub fn is_open(cx: &App) -> bool {
    cx.try_global::<SettingsWindow>()
        .is_some_and(|w| w.0.is_some())
}

/// The window's content.
pub struct SettingsView {
    focus_handle: FocusHandle,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self { focus_handle }
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new()
                    .h(crate::theme::scaled(crate::theme::TOOLBAR_HEIGHT, cx))
                    .children(crate::menu_bar(window, cx))
                    .child(
                        h_flex()
                            .w_full()
                            .h_full()
                            .justify_center()
                            .text_sm()
                            .child("Settings"),
                    ),
            )
    }
}
