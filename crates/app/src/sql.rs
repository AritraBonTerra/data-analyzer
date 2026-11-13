//! The SQL drawer: a small highlighted editor, Run/Cancel, and the error from
//! the last query. Results show in the document's grid.

use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::theme::{self, tabular};

actions!(sql, [RunQuery, CancelQuery]);

/// The SQL editor's key context. Run and Cancel are bound in `keymap`.
pub const CONTEXT: &str = "SqlPanel";

#[derive(Clone, Debug)]
pub enum SqlEvent {
    Run(String),
    Cancel,
    /// Leave the query result and show the file's rows again.
    ShowFile,
}

/// What the panel knows about the last run, for the status line.
#[derive(Clone, Debug, Default)]
pub enum QueryStatus {
    #[default]
    Idle,
    Running,
    Rows(u64),
    Done,
    Error(String),
}

pub struct SqlPanel {
    focus_handle: FocusHandle,
    editor: Entity<EditorState>,
    status: QueryStatus,
    _subscriptions: Vec<Subscription>,
}

impl SqlPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(false)
                .placeholder("SELECT * FROM data LIMIT 100")
        });
        let subscriptions = vec![cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter {
                secondary: true, ..
            } = event
            {
                this.run(&RunQuery, cx);
            }
        })];
        Self {
            focus_handle: cx.focus_handle(),
            editor,
            status: QueryStatus::Idle,
            _subscriptions: subscriptions,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| e.focus(window, cx));
    }

    /// Puts `sql` in the editor without running it, as Show SQL does.
    pub fn set_query(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| e.set_value(sql, window, cx));
    }

    pub fn set_status(&mut self, status: QueryStatus, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    pub fn is_running(&self) -> bool {
        matches!(self.status, QueryStatus::Running)
    }

    fn run(&mut self, _: &RunQuery, cx: &mut Context<Self>) {
        let sql = self.editor.read(cx).value().to_string();
        cx.emit(SqlEvent::Run(sql));
    }

    fn cancel(&mut self, _: &CancelQuery, cx: &mut Context<Self>) {
        cx.emit(SqlEvent::Cancel);
    }

    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        primary: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        h_flex()
            .id(id)
            .h(px(24.))
            .px(px(10.))
            .gap(px(5.))
            .items_center()
            .rounded(px(5.))
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .map(|this| {
                if primary {
                    this.bg(cx.theme().primary)
                        .text_color(cx.theme().primary_foreground)
                        .hover(|s| s.bg(cx.theme().primary_hover))
                } else {
                    this.border_1()
                        .border_color(cx.theme().border)
                        .text_color(cx.theme().foreground)
                        .hover(|s| s.bg(cx.theme().secondary_hover))
                }
            })
            .child(label)
    }
}

/// Drops DuckDB's trailing "Error code" line, which says nothing to a user.
fn tidy_error(message: &str) -> String {
    message
        .lines()
        .filter(|line| !line.trim_start().starts_with("Error code"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

impl EventEmitter<SqlEvent> for SqlPanel {}

impl Focusable for SqlPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SqlPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = theme::palette(cx);
        let muted = cx.theme().muted_foreground;
        let running = self.is_running();
        let status: Option<AnyElement> = match &self.status {
            QueryStatus::Idle => None,
            QueryStatus::Running => Some(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Running…")
                    .into_any_element(),
            ),
            QueryStatus::Rows(n) => Some(
                h_flex()
                    .gap(px(8.))
                    .text_xs()
                    .font_features(tabular())
                    .text_color(muted)
                    .child(SharedString::from(format!(
                        "{} returned",
                        crate::format::plural(*n, "row")
                    )))
                    .child(
                        div()
                            .id("show-file")
                            .text_color(cx.theme().link)
                            .cursor_pointer()
                            .hover(|s| s.text_color(cx.theme().link_hover))
                            .child("Back to file")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SqlEvent::ShowFile))),
                    )
                    .into_any_element(),
            ),
            QueryStatus::Done => Some(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Statement ran. It returned no rows.")
                    .into_any_element(),
            ),
            QueryStatus::Error(_) => None,
        };
        let error = match &self.status {
            QueryStatus::Error(message) => Some(tidy_error(message)),
            _ => None,
        };

        v_flex()
            .id("sql-panel")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, a: &RunQuery, _, cx| this.run(a, cx)))
            .on_action(cx.listener(|this, a: &CancelQuery, _, cx| this.cancel(a, cx)))
            .size_full()
            .bg(cx.theme().background)
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .h(px(34.))
                    .px(px(12.))
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .bg(palette.chrome)
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap(px(10.))
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(muted)
                                    .child("SQL"),
                            )
                            .child(
                                h_flex()
                                    .gap(px(4.))
                                    .text_xs()
                                    .text_color(muted)
                                    .child("The file is the table")
                                    .child(
                                        div()
                                            .px(px(5.))
                                            .rounded(px(4.))
                                            .bg(cx.theme().secondary)
                                            .font_family(cx.theme().mono_font_family.clone())
                                            .text_color(cx.theme().foreground)
                                            .child("data"),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap(px(10.))
                            .items_center()
                            .children(status)
                            .when(running, |this| {
                                this.child(
                                    self.button("cancel-query", "Cancel", false, cx).on_click(
                                        cx.listener(|this, _, _, cx| this.cancel(&CancelQuery, cx)),
                                    ),
                                )
                            })
                            .child(
                                self.button("run-query", "Run", true, cx)
                                    .children(theme::shortcut_hint("run_query", cx).map(|hint| {
                                        hint.text_color(cx.theme().primary_foreground.opacity(0.7))
                                    }))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.run(&RunQuery, cx)),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .text_size(theme::scaled(13., cx))
                    .child(Editor::new(&self.editor).appearance(false).h_full()),
            )
            .when_some(error, |this, message| {
                this.child(
                    h_flex()
                        .id("sql-error")
                        .flex_shrink_0()
                        .max_h(px(110.))
                        .overflow_y_scroll()
                        .px(px(12.))
                        .py(px(8.))
                        .gap(px(8.))
                        .items_start()
                        .border_t_1()
                        .border_color(cx.theme().danger.opacity(0.3))
                        .bg(cx.theme().danger.opacity(0.06))
                        .child(
                            Icon::empty()
                                .path("icons/circle-alert.svg")
                                .size(px(14.))
                                .text_color(cx.theme().danger)
                                .mt(px(1.)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_color(cx.theme().danger)
                                .child(SharedString::from(message)),
                        ),
                )
            })
    }
}
