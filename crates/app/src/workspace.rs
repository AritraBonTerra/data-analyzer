//! The root view of every window: either the welcome screen or a document.
//! Handles drag-and-drop of files, window-level actions and appearance changes.

use std::path::PathBuf;

use gpui_kit::component::{ActiveTheme, Theme, TitleBar, h_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::document::Document;
use crate::theme::TOOLBAR_HEIGHT;
use crate::welcome::{Welcome, WelcomeEvent};
use crate::{CloseWindow, Minimize, Zoom};

enum Content {
    Welcome(Entity<Welcome>),
    Document(Entity<Document>),
}

pub struct Workspace {
    focus_handle: FocusHandle,
    content: Content,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
            cx.notify();
        });
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            content: Content::Welcome(cx.new(Welcome::new)),
            _subscriptions: vec![appearance],
        };
        match path {
            Some(path) => this.open(path, window, cx),
            None => this.show_welcome(window, cx),
        }
        this
    }

    /// The file this window shows, if any.
    pub fn path(&self, cx: &App) -> Option<PathBuf> {
        match &self.content {
            Content::Document(doc) => Some(doc.read(cx).path().clone()),
            Content::Welcome(_) => None,
        }
    }

    pub fn document(&self) -> Option<&Entity<Document>> {
        match &self.content {
            Content::Document(doc) => Some(doc),
            Content::Welcome(_) => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self.content, Content::Welcome(_))
    }

    fn show_welcome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let welcome = cx.new(Welcome::new);
        self._subscriptions.truncate(1);
        self._subscriptions
            .push(cx.subscribe_in(&welcome, window, Self::on_welcome_event));
        self.content = Content::Welcome(welcome);
        window.set_window_title("Data Analyzer");
        cx.notify();
    }

    /// Replaces whatever this window shows with `path`.
    pub fn open(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let document = cx.new(|cx| Document::new(path, window, cx));
        self._subscriptions.truncate(1);
        let focus = document.focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        self.content = Content::Document(document);
        cx.notify();
    }

    fn on_welcome_event(
        &mut self,
        _: &Entity<Welcome>,
        event: &WelcomeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            WelcomeEvent::OpenPath(path) => {
                let path = path.clone();
                cx.defer(move |cx| crate::open_path(path, cx));
            }
            WelcomeEvent::OpenDialog => window.dispatch_action(Box::new(crate::Open), cx),
            WelcomeEvent::RemoveRecent(path) => {
                let path = path.clone();
                cx.global_mut::<crate::recent::RecentStore>()
                    .edit(|files| files.remove(&path));
                crate::refresh_menus_later(cx);
            }
        }
    }

    fn close_window(&mut self, _: &CloseWindow, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    fn minimize(&mut self, _: &Minimize, window: &mut Window, _: &mut Context<Self>) {
        window.minimize_window();
    }

    fn zoom(&mut self, _: &Zoom, window: &mut Window, _: &mut Context<Self>) {
        window.zoom_window();
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match &self.content {
            Content::Welcome(welcome) => div()
                .size_full()
                .flex()
                .flex_col()
                .child(
                    TitleBar::new().h(px(TOOLBAR_HEIGHT)).child(
                        h_flex()
                            .w_full()
                            .h_full()
                            .justify_center()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().muted_foreground)
                            .child("Data Analyzer"),
                    ),
                )
                .child(div().flex_1().min_h_0().child(welcome.clone()))
                .into_any_element(),
            Content::Document(document) => document.clone().into_any_element(),
        };

        div()
            .id("workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close_window))
            .on_action(cx.listener(Self::minimize))
            .on_action(cx.listener(Self::zoom))
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_drop(cx.listener(|_, paths: &ExternalPaths, _, cx| {
                let paths: Vec<PathBuf> = paths.paths().to_vec();
                cx.defer(move |cx| {
                    for path in paths {
                        crate::open_path(path, cx);
                    }
                });
            }))
            .drag_over::<ExternalPaths>(|style, _, _, cx| {
                style
                    .bg(cx.theme().primary.opacity(0.06))
                    .border_2()
                    .border_color(cx.theme().primary.opacity(0.6))
            })
            .child(content)
            .when(self.is_empty(), |this| this)
    }
}
