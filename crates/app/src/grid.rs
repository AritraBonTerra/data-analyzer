//! The virtualized data grid: a `TableDelegate` that pages rows in from the
//! dataset on demand and draws cells, the row-number gutter and the rich
//! column headers (type, null share, sketch, sort state).

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use da_core::{ColumnKind, ColumnSummary, Dataset, Distribution, Frame, SortKey};
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::format::{self, percent};
use crate::pages::{PAGE_SIZE, PageCache};
use crate::theme::{self, kind_icon, tabular};

/// What the grid tells its document about header interactions.
#[derive(Clone, Debug)]
pub enum GridEvent {
    /// A header was clicked; `additive` is shift-click (add a secondary sort).
    SortClicked { col: usize, additive: bool },
    /// The header menu button was clicked at `position` (window coordinates).
    HeaderMenu { col: usize, position: Point<Pixels> },
}

impl EventEmitter<GridEvent> for TableState<GridDelegate> {}

/// Per-column facts shown in the header once profiling finishes.
#[derive(Clone, Debug)]
pub struct ColumnProfile {
    pub null_percent: f64,
    pub sketch: Option<Distribution>,
}

pub struct GridDelegate {
    dataset: Option<Arc<Dataset>>,
    frame: Option<Frame>,
    /// Rows the grid lets you scroll through: the frame's count, or the
    /// preview size while a file is still loading.
    rows: u64,
    cache: PageCache,
    /// Width per data column; empty until the first page lets us size them.
    widths: Vec<f32>,
    /// Bumped whenever the frame changes; in-flight fetches for an older
    /// epoch are dropped. Frame ids alone aren't unique across datasets.
    epoch: u64,
    /// Whether the widths have been checked against this epoch's data.
    sized: bool,
    /// Shown when the frame has no rows.
    empty_message: SharedString,
    gutter_width: f32,
    sort: Vec<SortKey>,
    filtered: HashSet<String>,
    profiles: Vec<Option<ColumnProfile>>,
    visible: Range<usize>,
}

/// Rows shown for CSV/JSON files before they're fully loaded.
pub const PREVIEW_ROWS: u64 = 1000;

impl GridDelegate {
    pub fn new() -> Self {
        Self {
            dataset: None,
            frame: None,
            rows: 0,
            cache: PageCache::default(),
            widths: Vec::new(),
            epoch: 0,
            sized: false,
            empty_message: "No rows".into(),
            gutter_width: format::gutter_width(1),
            sort: Vec::new(),
            filtered: HashSet::new(),
            profiles: Vec::new(),
            visible: 0..0,
        }
    }

    pub fn frame(&self) -> Option<&Frame> {
        self.frame.as_ref()
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Shows `frame`. `widths` carries over user-resized column widths when the
    /// columns are unchanged; pass an empty vec to size them from the data again.
    pub fn set_frame(&mut self, dataset: Arc<Dataset>, frame: Frame, widths: Vec<f32>) {
        let same_columns = self
            .frame
            .as_ref()
            .is_some_and(|old| old.columns == frame.columns);
        self.rows = frame.rows.unwrap_or(PREVIEW_ROWS);
        self.epoch += 1;
        self.sized = false;
        self.cache.reset(self.epoch);
        self.widths = if same_columns && widths.len() == frame.columns.len() {
            widths
        } else {
            Vec::new()
        };
        if !same_columns {
            self.profiles = vec![None; frame.columns.len()];
        }
        self.gutter_width = format::gutter_width(self.rows);
        self.dataset = Some(dataset);
        self.frame = Some(frame);
    }

    pub fn clear(&mut self) {
        self.dataset = None;
        self.frame = None;
        self.rows = 0;
        self.epoch += 1;
        self.cache.reset(self.epoch);
        self.widths.clear();
        self.profiles.clear();
    }

    pub fn set_empty_message(&mut self, message: impl Into<SharedString>) {
        self.empty_message = message.into();
    }

    pub fn set_sort(&mut self, sort: Vec<SortKey>) {
        self.sort = sort;
    }

    pub fn set_filtered(&mut self, columns: impl IntoIterator<Item = String>) {
        self.filtered = columns.into_iter().collect();
    }

    /// Fills header facts from a summary and sketches computed for the frame.
    /// Also widens columns whose extreme values (min/max) wouldn't fit, so a
    /// deep scroll doesn't meet truncated numbers. Returns whether widths changed.
    pub fn set_profiles(&mut self, summaries: &[ColumnSummary], sketches: &[Distribution]) -> bool {
        let Some(frame) = &self.frame else {
            return false;
        };
        let mut widened = false;
        self.profiles = frame
            .columns
            .iter()
            .enumerate()
            .map(|(ix, column)| {
                let summary = summaries.iter().find(|s| s.name == column.name)?;
                if let Some(width) = self.widths.get_mut(ix) {
                    let extremes = [&summary.min, &summary.max]
                        .into_iter()
                        .flatten()
                        .map(|v| v.as_str());
                    let fit = format::initial_column_width(column, extremes.map(Some));
                    if fit > *width {
                        *width = fit;
                        widened = true;
                    }
                }
                Some(ColumnProfile {
                    null_percent: summary.null_percent,
                    sketch: sketches.get(ix).cloned(),
                })
            })
            .collect();
        widened
    }

    pub fn profile(&self, col: usize) -> Option<&ColumnProfile> {
        self.profiles.get(col).and_then(|p| p.as_ref())
    }

    /// Text of a loaded cell, `Some(None)` for NULL, `None` if not loaded.
    pub fn cell(&self, row: u64, col: usize) -> Option<Option<&str>> {
        self.cache.cell(row, col)
    }

    pub fn row_cells(&self, row: u64) -> Option<&[Option<String>]> {
        self.cache.row(row)
    }

    /// Requests the pages covering `visible` (plus a prefetch) in the background.
    pub fn request(&mut self, visible: Range<usize>, cx: &mut Context<TableState<Self>>) {
        self.visible = visible.clone();
        let (Some(dataset), Some(frame)) = (&self.dataset, &self.frame) else {
            return;
        };
        for page in self.cache.plan(visible, self.rows) {
            let dataset = dataset.clone();
            let frame = frame.clone();
            let epoch = self.epoch;
            cx.spawn(async move |table, cx| {
                let offset = page * PAGE_SIZE;
                let fetched = cx
                    .background_spawn(async move { dataset.fetch(&frame, offset, PAGE_SIZE) })
                    .await;
                let _ = table.update(cx, |table, cx| {
                    let delegate = table.delegate_mut();
                    if delegate.epoch != epoch {
                        return;
                    }
                    match fetched {
                        Ok(fetched) => {
                            let first = !delegate.sized;
                            if first {
                                delegate.size_columns(&fetched);
                                delegate.sized = true;
                            }
                            delegate.cache.insert(epoch, fetched);
                            if first {
                                table.refresh(cx);
                            }
                        }
                        Err(_) => delegate.cache.abandon(epoch, page),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Re-requests the current visible range, after the frame changed.
    pub fn refetch(&mut self, cx: &mut Context<TableState<Self>>) {
        let visible = self.visible.clone();
        self.request(visible, cx);
    }

    /// Sizes columns from the header names and a page of cells. Widths carried
    /// over from a previous frame only grow, so a sort that surfaces longer
    /// values doesn't truncate them, and a user's wider columns stay wide.
    fn size_columns(&mut self, page: &da_core::Page) {
        let Some(frame) = &self.frame else {
            return;
        };
        let fitted: Vec<f32> = frame
            .columns
            .iter()
            .enumerate()
            .map(|(col, column)| {
                let sample = page
                    .rows
                    .iter()
                    .take(200)
                    .filter_map(|row| row.get(col).map(|c| c.as_deref()));
                format::initial_column_width(column, sample)
            })
            .collect();
        if self.widths.len() == fitted.len() {
            for (width, fit) in self.widths.iter_mut().zip(fitted) {
                *width = width.max(fit);
            }
        } else {
            self.widths = fitted;
        }
    }

    fn sort_state(&self, name: &str) -> Option<(usize, bool)> {
        self.sort
            .iter()
            .position(|k| k.column == name)
            .map(|ix| (ix, self.sort[ix].descending))
    }

    fn render_sketch(&self, sketch: &Distribution, cx: &App) -> AnyElement {
        let palette = theme::palette(cx);
        match sketch {
            Distribution::Histogram(bins) => {
                let max = bins.iter().map(|b| b.count).max().unwrap_or(0).max(1) as f32;
                h_flex()
                    .h(px(14.))
                    .w_full()
                    .items_end()
                    .gap(px(1.))
                    .children(bins.iter().map(|bin| {
                        let share = bin.count as f32 / max;
                        let height = if bin.count == 0 {
                            px(1.)
                        } else {
                            px((share * 14.0).max(2.0))
                        };
                        div()
                            .flex_1()
                            .h(height)
                            .rounded(px(1.))
                            .bg(if bin.count == 0 {
                                palette.chart_track
                            } else {
                                palette.chart
                            })
                    }))
                    .into_any_element()
            }
            Distribution::Categories { values, other } => {
                let total: u64 = values.iter().map(|(_, n)| n).sum::<u64>() + other;
                if total == 0 {
                    return div().into_any_element();
                }
                // Lead with the most common real value; nulls have their own readout.
                let top = values
                    .iter()
                    .find(|(value, _)| value.is_some())
                    .or(values.first());
                let label = top.map(|(value, count)| {
                    let name = value.as_deref().unwrap_or("null");
                    let name: String = name.chars().take(18).collect();
                    format!("{name} {}", percent(*count as f64 / total as f64))
                });
                v_flex()
                    .w_full()
                    .gap(px(3.))
                    .child(
                        h_flex()
                            .h(px(5.))
                            .w_full()
                            .rounded_full()
                            .overflow_hidden()
                            .bg(palette.chart_track)
                            .children(values.iter().enumerate().map(|(ix, (_, count))| {
                                let share = *count as f32 / total as f32;
                                div()
                                    .h_full()
                                    .w(relative(share))
                                    .bg(palette.chart.opacity(1.0 - ix as f32 * 0.28))
                            })),
                    )
                    .when_some(label, |this, label| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(label),
                        )
                    })
                    .into_any_element()
            }
            Distribution::Empty => div().into_any_element(),
        }
    }
}

impl TableDelegate for GridDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.frame.as_ref().map_or(0, |f| f.columns.len() + 1)
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows as usize
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        if col_ix == 0 {
            return Column::new("#", "")
                .width(px(self.gutter_width))
                .fixed_left()
                .resizable(false)
                .movable(false)
                .selectable(false)
                .p_0();
        }
        let col = col_ix - 1;
        let Some(column) = self.frame.as_ref().and_then(|f| f.columns.get(col)) else {
            return Column::new("", "");
        };
        let width = self.widths.get(col).copied().unwrap_or(140.0);
        Column::new(column.name.clone(), column.name.clone())
            .width(px(width))
            .max_width(px(2000.))
            .movable(false)
            .map(|c| {
                if column.kind == ColumnKind::Number {
                    c.text_right()
                } else {
                    c
                }
            })
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let palette = theme::palette(cx);
        if col_ix == 0 {
            return div().size_full().bg(palette.gutter).into_any_element();
        }
        let col = col_ix - 1;
        let Some(column) = self.frame.as_ref().and_then(|f| f.columns.get(col)) else {
            return div().into_any_element();
        };
        let name: SharedString = column.name.clone().into();
        let is_number = column.kind == ColumnKind::Number;
        let sort = self.sort_state(&column.name);
        let multi = self.sort.len() > 1;
        let filtered = self.filtered.contains(&column.name);
        let profile = self.profile(col).cloned();
        let muted = cx.theme().muted_foreground;

        let sort_badge = sort.map(|(rank, descending)| {
            h_flex()
                .items_center()
                .gap(px(1.))
                .text_color(palette.accent)
                .child(
                    Icon::empty()
                        .path(if descending {
                            "icons/arrow-down.svg"
                        } else {
                            "icons/arrow-up.svg"
                        })
                        .size(px(12.)),
                )
                .when(multi, |this| {
                    this.child(div().text_size(px(10.)).child((rank + 1).to_string()))
                })
        });

        let menu_button = div()
            .id(("th-menu", col))
            .flex_shrink_0()
            .size(px(18.))
            .rounded(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .text_color(muted)
            .map(|this| {
                if filtered {
                    this.text_color(palette.accent)
                } else {
                    this.opacity(0.0).group_hover("th", |s| s.opacity(1.0))
                }
            })
            .hover(|s| s.bg(cx.theme().secondary_hover).opacity(1.0))
            .on_click(cx.listener(move |_, event: &ClickEvent, _, cx| {
                cx.stop_propagation();
                cx.emit(GridEvent::HeaderMenu {
                    col,
                    position: event.position(),
                });
            }))
            .child(
                Icon::empty()
                    .path(if filtered {
                        "icons/funnel.svg"
                    } else {
                        "icons/chevron-down.svg"
                    })
                    .size(px(12.)),
            );

        let null_text = profile.as_ref().and_then(|p| {
            if p.null_percent <= 0.0 {
                None
            } else {
                Some(format!("{} null", percent(p.null_percent / 100.0)))
            }
        });
        let high_nulls = profile.as_ref().is_some_and(|p| p.null_percent >= 50.0);

        v_flex()
            .id(("th", col))
            .group("th")
            .size_full()
            .py(px(6.))
            .justify_between()
            .cursor_pointer()
            .on_click(cx.listener(move |_, event: &ClickEvent, _, cx| {
                cx.emit(GridEvent::SortClicked {
                    col,
                    additive: event.modifiers().shift,
                });
            }))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(px(5.))
                    .when(is_number, |this| this.flex_row_reverse())
                    .child(
                        Icon::empty()
                            .path(kind_icon(column.kind))
                            .size(px(11.))
                            .text_color(muted.opacity(0.8))
                            .flex_shrink_0(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().foreground)
                            .text_size(px(12.5))
                            .child(name),
                    )
                    .children(sort_badge)
                    .child(menu_button),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_end()
                    .gap(px(6.))
                    .when(is_number, |this| this.flex_row_reverse())
                    .child(div().flex_1().min_w_0().when_some(
                        profile.as_ref().and_then(|p| p.sketch.as_ref()),
                        |this, s| this.child(self.render_sketch(s, cx)),
                    ))
                    .when_some(null_text, |this, text| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(10.))
                                .font_features(tabular())
                                .text_color(if high_nulls { palette.warning } else { muted })
                                .child(text),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let palette = theme::palette(cx);
        if col_ix == 0 {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_end()
                .pr(px(8.))
                .bg(palette.gutter)
                .text_size(px(11.))
                .font_features(tabular())
                .text_color(cx.theme().muted_foreground.opacity(0.8))
                .child(format::group_digits(row_ix as u64 + 1))
                .into_any_element();
        }
        let col = col_ix - 1;
        let is_number = self
            .frame
            .as_ref()
            .and_then(|f| f.columns.get(col))
            .is_some_and(|c| c.kind == ColumnKind::Number);
        match self.cache.cell(row_ix as u64, col) {
            None => {
                // Not loaded yet: a quiet placeholder bar.
                let width = 0.35 + ((row_ix * 7 + col * 13) % 5) as f32 * 0.08;
                h_flex()
                    .size_full()
                    .items_center()
                    .when(is_number, |this| this.justify_end())
                    .child(
                        div()
                            .h(px(8.))
                            .w(relative(width))
                            .rounded(px(3.))
                            .bg(cx.theme().skeleton.opacity(0.6)),
                    )
                    .into_any_element()
            }
            Some(None) => div()
                .text_color(palette.null)
                .text_size(px(11.))
                .child("null")
                .into_any_element(),
            Some(Some(text)) => div()
                .truncate()
                .when(is_number, |this| this.font_features(tabular()))
                .child(SharedString::from(text.to_string()))
                .into_any_element(),
        }
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        if col_ix == 0 {
            return (row_ix + 1).to_string();
        }
        match self.cache.cell(row_ix as u64, col_ix - 1) {
            Some(Some(text)) => text.to_string(),
            _ => String::new(),
        }
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, cx: &App) -> Option<SharedString> {
        Some(self.cell_text(row_ix, col_ix, cx).into())
    }

    fn visible_rows_changed(
        &mut self,
        visible_range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        self.request(visible_range, cx);
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let has_frame = self.frame.is_some();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .text_color(cx.theme().muted_foreground)
            .when(has_frame, |this| {
                this.child(div().text_sm().child(self.empty_message.clone()))
            })
    }
}
