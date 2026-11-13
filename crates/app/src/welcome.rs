//! The empty-window state: a drop zone, an Open button, recent files and the
//! formats we understand.

use std::path::PathBuf;

use da_core::FileKind;
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::recent::RecentStore;
use crate::theme;

#[derive(Clone, Debug)]
pub enum WelcomeEvent {
    OpenPath(PathBuf),
    OpenDialog,
    RemoveRecent(PathBuf),
}

pub struct Welcome {
    _observe_recent: Subscription,
}

impl Welcome {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            _observe_recent: cx.observe_global::<RecentStore>(|_, cx| cx.notify()),
        }
    }

    fn render_recent(&self, cx: &mut Context<Self>) -> Option<Div> {
        let muted = cx.theme().muted_foreground;
        let recent: Vec<PathBuf> = cx
            .global::<RecentStore>()
            .files
            .existing()
            .take(8)
            .map(|f| f.path.clone())
            .collect();
        if recent.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .w_full()
                .gap(px(2.))
                .child(
                    div()
                        .pb(px(6.))
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(muted)
                        .child("RECENT"),
                )
                .children(recent.into_iter().enumerate().map(|(ix, path)| {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let dir = path.parent().map(shorten_home).unwrap_or_default();
                    let kind = FileKind::detect(&path).map(|k| k.label()).unwrap_or("");
                    let open_path = path.clone();
                    let remove_path = path.clone();
                    h_flex()
                        .id(("recent", ix))
                        .group("recent")
                        .w_full()
                        .h(px(40.))
                        .px(px(10.))
                        .gap(px(10.))
                        .items_center()
                        .rounded(px(7.))
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().secondary_hover))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(WelcomeEvent::OpenPath(open_path.clone()))
                        }))
                        .child(
                            Icon::empty()
                                .path(file_icon(&path))
                                .size(px(18.))
                                .text_color(cx.theme().primary.opacity(0.9))
                                .flex_shrink_0(),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .truncate()
                                        .child(SharedString::from(name)),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .truncate()
                                        .child(SharedString::from(dir)),
                                ),
                        )
                        .child(div().text_xs().text_color(muted.opacity(0.8)).child(kind))
                        .child(
                            div()
                                .id("remove")
                                .size(px(20.))
                                .rounded(px(4.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(muted)
                                .opacity(0.0)
                                .group_hover("recent", |s| s.opacity(1.0))
                                .hover(|s| s.bg(cx.theme().secondary_active))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.stop_propagation();
                                    cx.emit(WelcomeEvent::RemoveRecent(remove_path.clone()))
                                }))
                                .child(Icon::empty().path("icons/x.svg").size(px(11.))),
                        )
                })),
        )
    }
}

/// `/Users/me/Documents` → `~/Documents`.
fn shorten_home(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    match std::env::var("HOME") {
        Ok(home) if text.starts_with(&home) => format!("~{}", &text[home.len()..]),
        _ => text.into_owned(),
    }
}

/// Icon for a file kind in lists.
pub fn file_icon(path: &std::path::Path) -> &'static str {
    match FileKind::detect(path) {
        Some(FileKind::Excel) => "icons/file-spreadsheet.svg",
        Some(FileKind::Json | FileKind::NdJson) => "icons/file-braces.svg",
        Some(FileKind::Parquet) => "icons/file-box.svg",
        _ => "icons/file-text.svg",
    }
}

impl EventEmitter<WelcomeEvent> for Welcome {}

impl Render for Welcome {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let recent = self.render_recent(cx);
        let formats = ["CSV", "TSV", "Excel", "Parquet", "JSON", "NDJSON"];

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(cx.theme().background)
            .child(
                v_flex()
                    .w(px(440.))
                    .max_h_full()
                    .items_center()
                    .gap(px(24.))
                    .p(px(24.))
                    .child(
                        v_flex()
                            .items_center()
                            .gap(px(14.))
                            .child(
                                div()
                                    .size(px(64.))
                                    .rounded(px(16.))
                                    .bg(theme::palette(cx).accent)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .shadow_md()
                                    .child(
                                        Icon::empty()
                                            .path("icons/table.svg")
                                            .size(px(32.))
                                            .text_color(white()),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Data Analyzer"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(muted)
                                    .text_center()
                                    .child("Open a data file to explore it: fast table, column stats, filters and SQL."),
                            ),
                    )
                    .child(
                        v_flex()
                            .w_full()
                            .items_center()
                            .gap(px(10.))
                            .py(px(26.))
                            .rounded(px(12.))
                            .border_2()
                            .border_dashed()
                            .border_color(cx.theme().border)
                            .child(
                                h_flex()
                                    .id("open")
                                    .h(px(30.))
                                    .px(px(14.))
                                    .gap(px(8.))
                                    .items_center()
                                    .rounded(px(7.))
                                    .bg(cx.theme().primary)
                                    .text_color(cx.theme().primary_foreground)
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(cx.theme().primary_hover))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(WelcomeEvent::OpenDialog)))
                                    .child("Open File…")
                                    .children(theme::shortcut_hint("open", cx).map(|hint| {
                                        hint.text_color(cx.theme().primary_foreground.opacity(0.7))
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("or drop a file anywhere in this window"),
                            ),
                    )
                    .when_some(recent, |this, recent| this.child(recent))
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .flex_wrap()
                            .justify_center()
                            .children(formats.into_iter().map(|f| {
                                div()
                                    .px(px(7.))
                                    .py(px(2.))
                                    .rounded(px(4.))
                                    .bg(cx.theme().secondary)
                                    .text_xs()
                                    .text_color(muted)
                                    .child(f)
                            })),
                    ),
            )
    }
}
