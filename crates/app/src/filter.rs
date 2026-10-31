//! The column menu that drops down from a header: sort shortcuts, "show
//! stats", and a filter editor with quick picks for categorical columns.

use da_core::{Column, ColumnKind, Distribution, Filter, FilterOp};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Icon, Sizable, ThemeStyled, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::format::percent;
use crate::theme::kind_icon;

actions!(filter, [ApplyFilter, DismissFilter]);

const CONTEXT: &str = "FilterEditor";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", ApplyFilter, Some(CONTEXT)),
        KeyBinding::new("escape", DismissFilter, Some(CONTEXT)),
    ]);
}

#[derive(Clone, Debug)]
pub enum FilterEvent {
    Apply(Filter),
    Clear(String),
    Sort { column: String, descending: bool },
    ShowStats(usize),
    Dismiss,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Contains,
    NotContains,
    Equals,
    NotEquals,
    GreaterThan,
    LessThan,
    Between,
    IsNull,
    IsNotNull,
}

impl Op {
    fn label(self) -> &'static str {
        match self {
            Op::Contains => "contains",
            Op::NotContains => "doesn't contain",
            Op::Equals => "=",
            Op::NotEquals => "≠",
            Op::GreaterThan => ">",
            Op::LessThan => "<",
            Op::Between => "between",
            Op::IsNull => "is empty",
            Op::IsNotNull => "is not empty",
        }
    }

    fn for_kind(kind: ColumnKind) -> &'static [Op] {
        match kind {
            ColumnKind::Number | ColumnKind::Temporal => &[
                Op::Equals,
                Op::NotEquals,
                Op::GreaterThan,
                Op::LessThan,
                Op::Between,
                Op::IsNull,
                Op::IsNotNull,
            ],
            ColumnKind::Boolean => &[Op::Equals, Op::NotEquals, Op::IsNull, Op::IsNotNull],
            _ => &[
                Op::Contains,
                Op::NotContains,
                Op::Equals,
                Op::NotEquals,
                Op::IsNull,
                Op::IsNotNull,
            ],
        }
    }

    fn needs_value(self) -> bool {
        !matches!(self, Op::IsNull | Op::IsNotNull)
    }

    fn from_filter(op: &FilterOp) -> (Op, String, String) {
        match op {
            FilterOp::Contains(v) => (Op::Contains, v.clone(), String::new()),
            FilterOp::NotContains(v) => (Op::NotContains, v.clone(), String::new()),
            FilterOp::Equals(v) => (Op::Equals, v.clone(), String::new()),
            FilterOp::NotEquals(v) => (Op::NotEquals, v.clone(), String::new()),
            FilterOp::GreaterThan(v) => (Op::GreaterThan, v.clone(), String::new()),
            FilterOp::LessThan(v) => (Op::LessThan, v.clone(), String::new()),
            FilterOp::Between(a, b) => (Op::Between, a.clone(), b.clone()),
            FilterOp::OneOf(values) => (
                Op::Equals,
                values.first().cloned().unwrap_or_default(),
                String::new(),
            ),
            FilterOp::IsNull => (Op::IsNull, String::new(), String::new()),
            FilterOp::IsNotNull => (Op::IsNotNull, String::new(), String::new()),
        }
    }

    /// Values keep their spaces so text filters match exactly; the engine trims
    /// values it casts to numbers or dates.
    fn to_filter(self, a: &str, b: &str) -> Option<FilterOp> {
        Some(match self {
            Op::IsNull => FilterOp::IsNull,
            Op::IsNotNull => FilterOp::IsNotNull,
            _ if a.trim().is_empty() => return None,
            Op::Contains => FilterOp::Contains(a.into()),
            Op::NotContains => FilterOp::NotContains(a.into()),
            Op::Equals => FilterOp::Equals(a.into()),
            Op::NotEquals => FilterOp::NotEquals(a.into()),
            Op::GreaterThan => FilterOp::GreaterThan(a.into()),
            Op::LessThan => FilterOp::LessThan(a.into()),
            Op::Between => {
                if b.trim().is_empty() {
                    return None;
                }
                FilterOp::Between(a.into(), b.into())
            }
        })
    }
}

pub struct FilterEditor {
    focus_handle: FocusHandle,
    column: Option<Column>,
    col_ix: usize,
    op: Op,
    value: Entity<InputState>,
    value2: Entity<InputState>,
    /// Top values of the column (from the header sketch), for one-click filters.
    quick: Vec<(Option<String>, u64)>,
    quick_total: u64,
    has_filter: bool,
    _subscriptions: Vec<Subscription>,
}

impl FilterEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let value = cx.new(|cx| InputState::new(window, cx).placeholder("Value"));
        let value2 = cx.new(|cx| InputState::new(window, cx).placeholder("and"));
        let subscriptions = vec![
            cx.subscribe(&value, Self::on_input),
            cx.subscribe(&value2, Self::on_input),
        ];
        Self {
            focus_handle: cx.focus_handle(),
            column: None,
            col_ix: 0,
            op: Op::Contains,
            value,
            value2,
            quick: Vec::new(),
            quick_total: 0,
            has_filter: false,
            _subscriptions: subscriptions,
        }
    }

    fn on_input(&mut self, _: Entity<InputState>, event: &InputEvent, cx: &mut Context<Self>) {
        if let InputEvent::PressEnter { .. } = event {
            self.apply(&ApplyFilter, cx);
        }
    }

    /// Prepares the editor for `column`, pre-filling an existing filter.
    pub fn open(
        &mut self,
        col_ix: usize,
        column: Column,
        existing: Option<&Filter>,
        sketch: Option<&Distribution>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ops = Op::for_kind(column.kind);
        let (op, a, b) = existing.map(|f| Op::from_filter(&f.op)).unwrap_or((
            ops[0],
            String::new(),
            String::new(),
        ));
        self.op = if ops.contains(&op) { op } else { ops[0] };
        self.has_filter = existing.is_some();
        self.value.update(cx, |s, cx| s.set_value(a, window, cx));
        self.value2.update(cx, |s, cx| s.set_value(b, window, cx));
        (self.quick, self.quick_total) = match sketch {
            Some(Distribution::Categories { values, other }) => {
                let total = values.iter().map(|(_, n)| n).sum::<u64>() + other;
                (values.clone(), total)
            }
            _ => (Vec::new(), 0),
        };
        self.column = Some(column);
        self.col_ix = col_ix;
        let value = self.value.clone();
        window.defer(cx, move |window, cx| {
            value.update(cx, |s, cx| s.focus(window, cx));
        });
        cx.notify();
    }

    fn apply(&mut self, _: &ApplyFilter, cx: &mut Context<Self>) {
        let Some(column) = &self.column else {
            return;
        };
        let a = self.value.read(cx).value();
        let b = self.value2.read(cx).value();
        if let Some(op) = self.op.to_filter(&a, &b) {
            cx.emit(FilterEvent::Apply(Filter {
                column: column.name.clone(),
                op,
            }));
        }
    }

    fn dismiss(&mut self, _: &DismissFilter, cx: &mut Context<Self>) {
        cx.emit(FilterEvent::Dismiss);
    }

    fn row_button(
        &self,
        id: &'static str,
        icon: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        h_flex()
            .id(id)
            .w_full()
            .h(px(26.))
            .px(px(8.))
            .gap(px(8.))
            .rounded(px(5.))
            .items_center()
            .text_sm()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().secondary_hover))
            .child(
                Icon::empty()
                    .path(icon)
                    .size(px(13.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(label)
    }
}

impl EventEmitter<FilterEvent> for FilterEditor {}

impl Focusable for FilterEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FilterEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(column) = self.column.clone() else {
            return div().into_any_element();
        };
        let ops = Op::for_kind(column.kind);
        let current = self.op;
        let col_ix = self.col_ix;
        let name = column.name.clone();
        let muted = cx.theme().muted_foreground;
        let accent = crate::theme::palette(cx).accent;

        let quick_total = self.quick_total.max(1) as f64;
        let quick: Vec<_> = self
            .quick
            .iter()
            .take(3)
            .cloned()
            .map(|(value, count)| {
                let share = percent(count as f64 / quick_total);
                let text: SharedString = match &value {
                    Some(v) if v.is_empty() => "(empty text)".into(),
                    Some(v) => v.chars().take(40).collect::<String>().into(),
                    None => "null".into(),
                };
                let column = name.clone();
                h_flex()
                    .id(SharedString::from(format!("quick-{text}")))
                    .w_full()
                    .h(px(24.))
                    .px(px(8.))
                    .gap(px(6.))
                    .rounded(px(5.))
                    .items_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().secondary_hover))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .when(value.is_none(), |this| this.text_color(muted))
                            .child(text),
                    )
                    .child(div().text_xs().text_color(muted).child(share))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        let op = match &value {
                            Some(v) => FilterOp::Equals(v.clone()),
                            None => FilterOp::IsNull,
                        };
                        cx.emit(FilterEvent::Apply(Filter {
                            column: column.clone(),
                            op,
                        }));
                    }))
            })
            .collect();

        let section_label = |text: &'static str| {
            div()
                .px(px(8.))
                .pt(px(8.))
                .pb(px(4.))
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(muted)
                .child(text)
        };

        v_flex()
            .id("filter-editor")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, a: &ApplyFilter, _, cx| this.apply(a, cx)))
            .on_action(cx.listener(|this, a: &DismissFilter, _, cx| this.dismiss(a, cx)))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(FilterEvent::Dismiss)))
            .occlude()
            .w(px(272.))
            .p(px(6.))
            .popover_style(cx)
            .text_color(cx.theme().popover_foreground)
            .child(
                h_flex()
                    .px(px(8.))
                    .pt(px(4.))
                    .pb(px(6.))
                    .gap(px(6.))
                    .items_center()
                    .child(
                        Icon::empty()
                            .path(kind_icon(column.kind))
                            .size(px(12.))
                            .text_color(muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(crate::format::type_label(&column.sql_type)),
                    ),
            )
            .child(
                self.row_button("sort-asc", "icons/arrow-up.svg", "Sort ascending", cx)
                    .on_click(cx.listener({
                        let name = name.clone();
                        move |_, _, _, cx| {
                            cx.emit(FilterEvent::Sort {
                                column: name.clone(),
                                descending: false,
                            })
                        }
                    })),
            )
            .child(
                self.row_button("sort-desc", "icons/arrow-down.svg", "Sort descending", cx)
                    .on_click(cx.listener({
                        let name = name.clone();
                        move |_, _, _, cx| {
                            cx.emit(FilterEvent::Sort {
                                column: name.clone(),
                                descending: true,
                            })
                        }
                    })),
            )
            .child(
                self.row_button("show-stats", "icons/chart-column.svg", "Show stats", cx)
                    .on_click(
                        cx.listener(move |_, _, _, cx| cx.emit(FilterEvent::ShowStats(col_ix))),
                    ),
            )
            .child(section_label("Filter"))
            .child(
                h_flex()
                    .px(px(8.))
                    .flex_wrap()
                    .gap(px(4.))
                    .children(ops.iter().copied().map(|op| {
                        let selected = op == current;
                        div()
                            .id(op.label())
                            .px(px(7.))
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .text_xs()
                            .cursor_pointer()
                            .border_1()
                            .border_color(if selected { accent } else { cx.theme().border })
                            .when(selected, |this| {
                                this.bg(accent.opacity(0.12)).text_color(accent)
                            })
                            .when(!selected, |this| {
                                this.hover(|s| s.bg(cx.theme().secondary_hover))
                            })
                            .child(op.label())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.op = op;
                                cx.notify();
                            }))
                    })),
            )
            .when(current.needs_value(), |this| {
                this.child(
                    h_flex()
                        .px(px(8.))
                        .pt(px(8.))
                        .gap(px(6.))
                        .child(div().flex_1().child(Input::new(&self.value).small()))
                        .when(current == Op::Between, |this| {
                            this.child(div().flex_1().child(Input::new(&self.value2).small()))
                        }),
                )
            })
            .child(
                h_flex()
                    .px(px(8.))
                    .pt(px(8.))
                    .pb(px(4.))
                    .gap(px(6.))
                    .justify_end()
                    .when(self.has_filter, |this| {
                        let name = name.clone();
                        this.child(
                            div()
                                .id("clear-filter")
                                .px(px(10.))
                                .h(px(24.))
                                .flex()
                                .items_center()
                                .rounded(px(5.))
                                .text_xs()
                                .text_color(muted)
                                .cursor_pointer()
                                .hover(|s| s.bg(cx.theme().secondary_hover))
                                .child("Clear filter")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(FilterEvent::Clear(name.clone()))
                                })),
                        )
                    })
                    .child(
                        div()
                            .id("apply-filter")
                            .px(px(12.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .bg(cx.theme().primary)
                            .text_color(cx.theme().primary_foreground)
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().primary_hover))
                            .child("Apply")
                            .on_click(cx.listener(|this, _, _, cx| this.apply(&ApplyFilter, cx))),
                    ),
            )
            .when(!quick.is_empty(), |this| {
                this.child(section_label("Top values")).children(quick)
            })
            .into_any_element()
    }
}
