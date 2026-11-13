//! The column menu that drops down from a header: sort shortcuts, "show
//! stats", how to read the column's values, and a filter editor with quick
//! picks for categorical columns.

use std::sync::Arc;

use da_core::{Column, ColumnKind, Dataset, Distribution, Filter, FilterOp, Fit, ReadAs};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Icon, Sizable, ThemeStyled, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::format::{formatted, group_digits, percent, plural};
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
    Sort {
        column: String,
        descending: bool,
    },
    ShowStats(usize),
    /// Read the column's values as this type from now on.
    ReadAs {
        column: String,
        read_as: ReadAs,
    },
    Dismiss,
}

/// What the column menu opens on.
pub struct MenuFor {
    pub col_ix: usize,
    pub column: Column,
    /// The filter on the column, to edit.
    pub filter: Option<Filter>,
    /// The column's header sketch, for quick picks.
    pub sketch: Option<Distribution>,
    /// The open file and how the column is read now, when the menu may
    /// change that. `None` for SQL results and Parquet files.
    pub read_as: Option<(Arc<Dataset>, ReadAs)>,
}

/// The Read as choices, in menu order.
const READ_AS: [(ReadAs, &str); 6] = [
    (ReadAs::Auto, "Automatic"),
    (ReadAs::Text, "Text"),
    (ReadAs::Number, "Number"),
    (ReadAs::MonthFirstDate, "Date m/d"),
    (ReadAs::DayFirstDate, "Date d/m"),
    (ReadAs::YesNo, "Yes/No"),
];

/// A Read as choice picked but not applied, and what counting its fit says.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Counted {
    Counting,
    Done(Fit),
    Failed,
}

/// What reading a column as a picked type would do, like `27 of 30 values
/// fit. The other 3 become empty.`
fn fit_line(fit: Fit) -> String {
    let Fit { values, fit } = fit;
    let misfits = values - fit;
    match (values, fit) {
        (0, _) => "The column has no values to read.".into(),
        (_, 0) => format!(
            "None of the {} fit. The column would be empty.",
            plural(values, "value")
        ),
        _ if misfits == 0 => format!("{} of {} fit.", group_digits(fit), plural(values, "value")),
        _ if misfits == 1 => format!(
            "{} of {} fit. The other one becomes empty.",
            group_digits(fit),
            plural(values, "value")
        ),
        _ => format!(
            "{} of {} fit. The other {} become empty.",
            group_digits(fit),
            plural(values, "value"),
            group_digits(misfits)
        ),
    }
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

    /// The operator and its two values, unformatted.
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
    /// The existing filter's values as the fields first show them, and the
    /// values they stand for: a number shown as `12.6%` stays exactly what
    /// it was unless the user edits it.
    shown: [(String, String); 2],
    /// The open file and the column's Read as choice, when it can change.
    read_as: Option<(Arc<Dataset>, ReadAs)>,
    /// A Read as choice picked and not yet applied.
    picked: Option<(ReadAs, Counted)>,
    /// Bumped per pick; a count arriving for an older pick is dropped.
    generation: u64,
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
            shown: Default::default(),
            read_as: None,
            picked: None,
            generation: 0,
            _subscriptions: subscriptions,
        }
    }

    fn on_input(&mut self, _: Entity<InputState>, event: &InputEvent, cx: &mut Context<Self>) {
        if let InputEvent::PressEnter { .. } = event {
            self.apply(&ApplyFilter, cx);
        }
    }

    /// Prepares the menu for a column, pre-filling an existing filter in the
    /// column's display form.
    pub fn open(&mut self, menu: MenuFor, window: &mut Window, cx: &mut Context<Self>) {
        let MenuFor {
            col_ix,
            column,
            filter,
            sketch,
            read_as,
        } = menu;
        let ops = Op::for_kind(column.kind);
        let (op, a, b) = filter.as_ref().map(|f| Op::from_filter(&f.op)).unwrap_or((
            ops[0],
            String::new(),
            String::new(),
        ));
        self.op = if ops.contains(&op) { op } else { ops[0] };
        self.has_filter = filter.is_some();
        self.shown = [a, b].map(|v| (formatted(column.format, &v).unwrap_or(v.clone()), v));
        for (input, (text, _)) in [&self.value, &self.value2].into_iter().zip(&self.shown) {
            let text = text.clone();
            input.update(cx, |s, cx| s.set_value(text, window, cx));
        }
        (self.quick, self.quick_total) = match sketch {
            Some(Distribution::Categories { values, other }) => {
                let total = values.iter().map(|(_, n)| n).sum::<u64>() + other;
                (values, total)
            }
            _ => (Vec::new(), 0),
        };
        self.column = Some(column);
        self.col_ix = col_ix;
        self.read_as = read_as;
        self.picked = None;
        self.generation += 1;
        let value = self.value.clone();
        window.defer(cx, move |window, cx| {
            value.update(cx, |s, cx| s.focus(window, cx));
        });
        cx.notify();
    }

    /// The filter value a field's text stands for: the existing value while
    /// the text is as first shown, else the text read in the column's
    /// display form (`10%` is 0.1 in a percent column), else the text itself.
    fn value_of(&self, field: usize, text: &str) -> String {
        let (shown, value) = &self.shown[field];
        if text == shown {
            return value.clone();
        }
        self.column
            .as_ref()
            .and_then(|c| c.format?.parse(text))
            .map_or_else(|| text.to_string(), |n| n.to_string())
    }

    fn apply(&mut self, _: &ApplyFilter, cx: &mut Context<Self>) {
        let Some(column) = &self.column else {
            return;
        };
        let a = self.value_of(0, &self.value.read(cx).value());
        let b = self.value_of(1, &self.value2.read(cx).value());
        if let Some(op) = self.op.to_filter(&a, &b) {
            cx.emit(FilterEvent::Apply(Filter {
                column: column.name.clone(),
                op,
            }));
        }
    }

    /// Picks how to read the column and counts in the background how many
    /// of its values that reads. Picking the current choice, or picking
    /// again after a failed count, starts over.
    pub fn pick_read_as(&mut self, read_as: ReadAs, cx: &mut Context<Self>) {
        let (Some((dataset, current)), Some(column)) = (&self.read_as, &self.column) else {
            return;
        };
        self.generation += 1;
        if read_as == *current {
            self.picked = None;
            cx.notify();
            return;
        }
        self.picked = Some((read_as, Counted::Counting));
        let generation = self.generation;
        let (dataset, name) = (dataset.clone(), column.name.clone());
        cx.spawn(async move |this, cx| {
            let fit = cx
                .background_spawn(async move { dataset.fit(&name, read_as) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                // A count can fail if it races the end of a load; picking again retries.
                let counted = fit.map_or(Counted::Failed, Counted::Done);
                this.picked = Some((read_as, counted));
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Applies the picked Read as choice, once its fit is known.
    pub fn apply_read_as(&mut self, cx: &mut Context<Self>) {
        if let (Some(column), Some((read_as, Counted::Done(_)))) = (&self.column, self.picked) {
            cx.emit(FilterEvent::ReadAs {
                column: column.name.clone(),
                read_as,
            });
        }
    }

    /// Types `text` as the filter's value with the operator labelled `op`
    /// and applies it, as a user would. For the screenshot harness.
    #[cfg(all(test, target_os = "macos"))]
    pub fn type_filter(
        &mut self,
        op: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.op = *Op::for_kind(self.column.as_ref().map_or(ColumnKind::Text, |c| c.kind))
            .iter()
            .find(|o| o.label() == op)
            .unwrap_or_else(|| panic!("no operator {op:?}"));
        self.value
            .update(cx, |s, cx| s.set_value(text.to_string(), window, cx));
        self.apply(&ApplyFilter, cx);
    }

    /// Whether a fit is still being counted. For the screenshot harness.
    #[cfg(all(test, target_os = "macos"))]
    pub fn is_counting(&self) -> bool {
        matches!(self.picked, Some((_, Counted::Counting)))
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

    /// A small choice button, filled when `selected`, outlined in the
    /// accent when only `marked`.
    fn chip(
        id: impl Into<ElementId>,
        label: &'static str,
        selected: bool,
        marked: bool,
        cx: &App,
    ) -> Stateful<Div> {
        let accent = crate::theme::palette(cx).accent;
        div()
            .id(id)
            .px(px(7.))
            .h(px(22.))
            .flex()
            .items_center()
            .rounded(px(5.))
            .text_xs()
            .cursor_pointer()
            .border_1()
            .border_color(if selected || marked {
                accent
            } else {
                cx.theme().border
            })
            .when(selected, |this| {
                this.bg(accent.opacity(0.12)).text_color(accent)
            })
            .when(!selected, |this| {
                this.hover(|s| s.bg(cx.theme().secondary_hover))
            })
            .child(label)
    }

    /// The Read as choices, and once one is picked, what it would do and
    /// its Apply button. The choice in effect is outlined.
    fn render_read_as(&self, current: ReadAs, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let shown = self.picked.map_or(current, |(read_as, _)| read_as);
        let line = self.picked.map(|(_, counted)| match counted {
            Counted::Counting => ("Counting values…".to_string(), false),
            Counted::Done(fit) => (fit_line(fit), true),
            Counted::Failed => (
                "Couldn't count the values. Pick it again to retry.".into(),
                false,
            ),
        });
        v_flex()
            .child(
                h_flex()
                    .px(px(8.))
                    .flex_wrap()
                    .gap(px(4.))
                    .children(READ_AS.iter().map(|&(read_as, label)| {
                        Self::chip(
                            SharedString::from(format!("read-as-{label}")),
                            label,
                            read_as == shown,
                            read_as == current,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.pick_read_as(read_as, cx)))
                    })),
            )
            .when_some(line, |this, (line, ready)| {
                this.child(
                    h_flex()
                        .px(px(8.))
                        .pt(px(8.))
                        .gap(px(8.))
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(muted)
                                .child(SharedString::from(line)),
                        )
                        .when(ready, |this| {
                            this.child(
                                Self::action_button("apply-read-as", "Apply", cx)
                                    .on_click(cx.listener(|this, _, _, cx| this.apply_read_as(cx))),
                            )
                        }),
                )
            })
    }

    /// The menu's primary button.
    fn action_button(id: &'static str, label: &'static str, cx: &App) -> Stateful<Div> {
        div()
            .id(id)
            .flex_shrink_0()
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
                    Some(v) => formatted(column.format, v)
                        .unwrap_or_else(|| v.chars().take(40).collect())
                        .into(),
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
            .when_some(self.read_as.as_ref().map(|(_, r)| *r), |this, current| {
                this.child(section_label("Read as"))
                    .child(self.render_read_as(current, cx))
            })
            .child(section_label("Filter"))
            .child(
                h_flex()
                    .px(px(8.))
                    .flex_wrap()
                    .gap(px(4.))
                    .children(ops.iter().copied().map(|op| {
                        Self::chip(op.label(), op.label(), op == current, false, cx).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.op = op;
                                cx.notify();
                            }),
                        )
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
                        Self::action_button("apply-filter", "Apply", cx)
                            .on_click(cx.listener(|this, _, _, cx| this.apply(&ApplyFilter, cx))),
                    ),
            )
            .when(!quick.is_empty(), |this| {
                this.child(section_label("Top values")).children(quick)
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use da_core::Fit;

    use super::fit_line;

    #[test]
    fn says_how_many_values_fit() {
        let line = |values, fit| fit_line(Fit { values, fit });
        assert_eq!(line(30, 30), "30 of 30 values fit.");
        assert_eq!(
            line(30, 27),
            "27 of 30 values fit. The other 3 become empty."
        );
        assert_eq!(
            line(30, 29),
            "29 of 30 values fit. The other one becomes empty."
        );
        assert_eq!(line(1, 1), "1 of 1 value fit.");
        assert_eq!(
            line(2400, 0),
            "None of the 2,400 values fit. The column would be empty."
        );
        assert_eq!(line(0, 0), "The column has no values to read.");
    }
}
