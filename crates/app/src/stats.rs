//! The stats side panel: SUMMARIZE facts for one column and its distribution,
//! drawn as a clickable histogram or a top-values list.

use std::sync::Arc;

use da_core::{
    Bin, Column, ColumnKind, ColumnSummary, Dataset, Distribution, Filter, FilterOp, Frame,
    tidy_number,
};
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::format::{formatted, group_digits, percent, type_label};
use crate::theme::{self, kind_icon, tabular};

/// Histogram bins and top values requested for the panel.
const BINS: usize = 24;
const TOP: usize = 12;

#[derive(Clone, Debug)]
pub enum StatsEvent {
    AddFilter(Filter),
    Close,
}

pub struct StatsPanel {
    column: Option<Column>,
    /// The frame the distribution was computed for.
    frame_id: Option<u64>,
    summary: Option<ColumnSummary>,
    distribution: Option<Distribution>,
    error: Option<String>,
    loading: bool,
    /// Bumped per `show`; a distribution arriving for an older one is dropped.
    generation: u64,
    hovered: Option<usize>,
    scroll: ScrollHandle,
}

impl StatsPanel {
    pub fn new() -> Self {
        Self {
            column: None,
            frame_id: None,
            summary: None,
            distribution: None,
            error: None,
            loading: false,
            generation: 0,
            hovered: None,
            scroll: ScrollHandle::new(),
        }
    }

    pub fn column(&self) -> Option<&Column> {
        self.column.as_ref()
    }

    pub fn frame_id(&self) -> Option<u64> {
        self.frame_id
    }

    /// Whether a distribution is still being computed. For the screenshot harness.
    #[cfg(all(test, target_os = "macos"))]
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Shows `column` of `frame`, computing its distribution in the background.
    pub fn show(
        &mut self,
        dataset: Arc<Dataset>,
        frame: Frame,
        column: Column,
        summary: Option<ColumnSummary>,
        cx: &mut Context<Self>,
    ) {
        let same = self.column.as_ref() == Some(&column) && self.frame_id == Some(frame.id);
        self.column = Some(column.clone());
        self.frame_id = Some(frame.id);
        self.summary = summary;
        if !same {
            self.distribution = None;
            self.hovered = None;
        }
        self.error = None;
        self.loading = true;
        self.generation += 1;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { dataset.distribution(&frame, &column, BINS, TOP) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(distribution) => this.distribution = Some(distribution),
                    Err(err) => this.error = Some(err.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Updates the summary once profiling finishes without refetching the chart.
    pub fn set_summary(&mut self, summary: Option<ColumnSummary>, cx: &mut Context<Self>) {
        self.summary = summary;
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.column = None;
        self.frame_id = None;
        self.summary = None;
        self.distribution = None;
        self.generation += 1;
        cx.notify();
    }

    fn fact(label: &'static str, value: Option<String>, cx: &App) -> Option<Div> {
        let value = value?;
        Some(
            h_flex()
                .w_full()
                .items_baseline()
                .justify_between()
                .gap(px(12.))
                .py(px(3.))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .text_sm()
                        .font_features(tabular())
                        .truncate()
                        .child(SharedString::from(value)),
                ),
        )
    }

    fn render_summary(&self, cx: &App) -> Div {
        let Some(summary) = &self.summary else {
            return div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Computing summary…");
        };
        let column = self.column.as_ref();
        let numeric = column.is_some_and(|c| c.kind == ColumnKind::Number);
        let format = column.and_then(|c| c.format);
        // Numbers show the way the column does; text extremes as-is:
        // "001.00" is a value, not a number.
        let show = |value: &Option<String>| {
            value
                .as_deref()
                .map(|v| formatted(format, v).unwrap_or_else(|| tidy_if(numeric, v)))
        };
        let nulls = summary.null_percent / 100.0;
        let null_count = summary.nulls;
        let number = |label, value: &Option<String>| {
            if numeric {
                Self::fact(label, show(value), cx)
            } else {
                None
            }
        };
        let facts = [
            Self::fact("Rows", Some(group_digits(summary.count)), cx),
            Self::fact(
                "Nulls",
                Some(if null_count == 0 {
                    "none".into()
                } else {
                    format!("{} · {}", group_digits(null_count), percent(nulls))
                }),
                cx,
            ),
            Self::fact(
                "Distinct",
                Some(format!("≈ {}", group_digits(summary.distinct))),
                cx,
            ),
            Self::fact("Min", show(&summary.min), cx),
            Self::fact("Max", show(&summary.max), cx),
            number("Mean", &summary.mean),
            number("Std dev", &summary.std),
            number("25th pct", &summary.q25),
            number("Median", &summary.median),
            number("75th pct", &summary.q75),
        ];
        v_flex().w_full().children(facts.into_iter().flatten())
    }

    fn render_histogram(&self, bins: &[Bin], cx: &mut Context<Self>) -> Div {
        let palette = theme::palette(cx);
        let column = self.column.clone();
        let format = column.as_ref().and_then(|c| c.format);
        let edge = |value: &str| formatted(format, value).unwrap_or_else(|| tidy_number(value));
        let max = bins.iter().map(|b| b.count).max().unwrap_or(0).max(1) as f32;
        let total: u64 = bins.iter().map(|b| b.count).sum();
        let hovered = self.hovered;
        let readout = match hovered.and_then(|ix| bins.get(ix)) {
            Some(bin) if bin.count > 0 => format!(
                "{} – {}  ·  {} rows ({})",
                bin.first.as_deref().map(edge).unwrap_or_default(),
                bin.last.as_deref().map(edge).unwrap_or_default(),
                group_digits(bin.count),
                percent(bin.count as f64 / total.max(1) as f64)
            ),
            Some(_) => "no rows".into(),
            None => "Hover a bar for its range. Click to filter.".into(),
        };
        let low = bins
            .iter()
            .find_map(|b| b.first.as_deref().map(edge))
            .unwrap_or_default();
        let high = bins
            .iter()
            .rev()
            .find_map(|b| b.last.as_deref().map(edge))
            .unwrap_or_default();

        v_flex()
            .w_full()
            .gap(px(6.))
            .child(
                h_flex()
                    .id("histogram")
                    .w_full()
                    .h(px(120.))
                    .items_end()
                    .gap(px(2.))
                    .on_hover(cx.listener(|this, hovering: &bool, _, cx| {
                        if !hovering {
                            this.hovered = None;
                            cx.notify();
                        }
                    }))
                    .children(bins.iter().enumerate().map(|(ix, bin)| {
                        let share = bin.count as f32 / max;
                        let height = if bin.count == 0 {
                            px(1.)
                        } else {
                            px((share * 120.0).max(2.0))
                        };
                        let is_hovered = hovered == Some(ix);
                        let column = column.clone();
                        let range = (bin.first.clone(), bin.last.clone());
                        div()
                            .id(("bin", ix))
                            .flex_1()
                            .h_full()
                            .flex()
                            .items_end()
                            .cursor_pointer()
                            .on_hover(cx.listener(move |this, hovering: &bool, _, cx| {
                                if *hovering {
                                    this.hovered = Some(ix);
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                if let (Some(column), (Some(a), Some(b))) = (&column, &range) {
                                    cx.emit(StatsEvent::AddFilter(Filter {
                                        column: column.name.clone(),
                                        op: FilterOp::Between(a.clone(), b.clone()),
                                    }));
                                }
                            }))
                            .child(div().w_full().h(height).rounded_t(px(2.)).bg(
                                if bin.count == 0 {
                                    palette.chart_track
                                } else if is_hovered {
                                    palette.chart_strong
                                } else {
                                    palette.chart
                                },
                            ))
                    })),
            )
            .child(div().w_full().h(px(1.)).bg(cx.theme().border))
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .text_xs()
                    .font_features(tabular())
                    .gap(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(div().truncate().child(SharedString::from(low)))
                    .child(div().truncate().child(SharedString::from(high))),
            )
            .child(
                div()
                    .text_xs()
                    .font_features(tabular())
                    .text_color(cx.theme().muted_foreground)
                    .min_h(px(16.))
                    .child(SharedString::from(readout)),
            )
    }

    fn render_categories(
        &self,
        values: &[(Option<String>, u64)],
        other: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        let palette = theme::palette(cx);
        let column = self.column.clone();
        let format = column.as_ref().and_then(|c| c.format);
        let total = values.iter().map(|(_, n)| n).sum::<u64>() + other;
        let max = values.iter().map(|(_, n)| *n).max().unwrap_or(0).max(1) as f32;
        let muted = cx.theme().muted_foreground;

        let row =
            |ix: usize,
             label: SharedString,
             count: u64,
             is_null: bool,
             filter: Option<FilterOp>| {
                let share = count as f32 / max;
                let column = column.clone();
                let clickable = filter.is_some();
                v_flex()
                    .id(("cat", ix))
                    .w_full()
                    .gap(px(3.))
                    .py(px(4.))
                    .px(px(6.))
                    .mx(px(-6.))
                    .rounded(px(5.))
                    .when(clickable, |this| {
                        this.cursor_pointer()
                            .hover(|s| s.bg(cx.theme().secondary_hover))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                if let (Some(column), Some(op)) = (&column, &filter) {
                                    cx.emit(StatsEvent::AddFilter(Filter {
                                        column: column.name.clone(),
                                        op: op.clone(),
                                    }));
                                }
                            }))
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .when(is_null, |this| this.text_color(palette.null))
                                    .child(label),
                            )
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .text_xs()
                                    .font_features(tabular())
                                    .text_color(muted)
                                    .child(SharedString::from(group_digits(count)))
                                    .child(div().w(px(42.)).text_right().child(
                                        SharedString::from(percent(
                                            count as f64 / total.max(1) as f64,
                                        )),
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .h(px(5.))
                            .rounded_full()
                            .bg(palette.chart_track)
                            .child(div().h_full().w(relative(share)).rounded_full().bg(
                                if clickable {
                                    palette.chart
                                } else {
                                    palette.chart_track
                                },
                            )),
                    )
            };

        let mut rows: Vec<Stateful<Div>> = values
            .iter()
            .enumerate()
            .map(|(ix, (value, count))| match value {
                Some(v) if v.is_empty() => row(
                    ix,
                    "(empty text)".into(),
                    *count,
                    true,
                    Some(FilterOp::Equals(String::new())),
                ),
                Some(v) => row(
                    ix,
                    formatted(format, v).unwrap_or_else(|| v.clone()).into(),
                    *count,
                    false,
                    Some(FilterOp::Equals(v.clone())),
                ),
                None => row(ix, "null".into(), *count, true, Some(FilterOp::IsNull)),
            })
            .collect();
        if other > 0 {
            rows.push(row(values.len(), "Other values".into(), other, true, None));
        }
        v_flex().w_full().children(rows)
    }

    fn render_distribution(&self, cx: &mut Context<Self>) -> Div {
        if let Some(error) = &self.error {
            return div()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(SharedString::from(error.clone()));
        }
        match &self.distribution {
            None => div()
                .h(px(120.))
                .w_full()
                .rounded(px(6.))
                .bg(cx.theme().skeleton.opacity(0.5)),
            Some(Distribution::Histogram(bins)) => self.render_histogram(bins, cx),
            Some(Distribution::Categories { values, other }) => {
                self.render_categories(values, *other, cx)
            }
            Some(Distribution::Empty) => div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("No values."),
        }
    }
}

impl EventEmitter<StatsEvent> for StatsPanel {}

impl Render for StatsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let section = |text: &'static str| {
            div()
                .pt(px(14.))
                .pb(px(6.))
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(muted)
                .child(text)
        };
        let close = div()
            .id("close-stats")
            .size(px(22.))
            .rounded(px(5.))
            .flex()
            .items_center()
            .justify_center()
            .text_color(muted)
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().secondary_hover))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(StatsEvent::Close)))
            .child(Icon::empty().path("icons/x.svg").size(px(13.)));

        let body = match self.column.clone() {
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .px(px(20.))
                .text_center()
                .child(
                    Icon::empty()
                        .path("icons/chart-column.svg")
                        .size(px(24.))
                        .text_color(muted.opacity(0.6)),
                )
                .child(div().text_sm().text_color(muted).child("Select a column"))
                .child(
                    div()
                        .text_xs()
                        .text_color(muted.opacity(0.8))
                        .child("Click any cell, or choose “Show stats” from a column menu."),
                )
                .into_any_element(),
            Some(column) => {
                let is_categorical =
                    matches!(self.distribution, Some(Distribution::Categories { .. }));
                v_flex()
                    .id("stats-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px(px(16.))
                    .pb(px(16.))
                    .child(
                        h_flex()
                            .pt(px(12.))
                            .gap(px(7.))
                            .items_center()
                            .child(
                                Icon::empty()
                                    .path(kind_icon(column.kind))
                                    .size(px(13.))
                                    .text_color(muted),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(column.name.clone())),
                            ),
                    )
                    .child(
                        div()
                            .pl(px(20.))
                            .text_xs()
                            .text_color(muted)
                            .child(SharedString::from(type_label(&column.sql_type))),
                    )
                    .child(section("Summary"))
                    .child(self.render_summary(cx))
                    .child(section(if is_categorical {
                        "Top values"
                    } else {
                        "Distribution"
                    }))
                    .child(self.render_distribution(cx))
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .h(px(34.))
                    .px(px(12.))
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(muted)
                            .child("STATS"),
                    )
                    .child(close),
            )
            .child(body)
    }
}

fn tidy_if(numeric: bool, value: &str) -> String {
    if numeric {
        tidy_number(value)
    } else {
        value.to_string()
    }
}
