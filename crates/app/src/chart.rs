//! The chart panel: the frame on screen drawn as a bar, line, scatter or
//! histogram above the grid, with its controls in a rail on the right. The
//! engine (`Dataset::chart`) does the grouping; this module picks the spec,
//! draws the result on a canvas and turns hovers and clicks into tooltips and
//! filters.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use da_core::{
    Aggregate, Axis, BarSort, Cancelled, Chart, ChartKind, ChartSpec, Column, Dataset, Filter,
    Frame, MAX_Y, NumberFormat, Scale, Series, ViewSpec, XAxis,
};
use gpui_kit::base::plot::label::truncate_text_to_width;
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme, Icon, Sizable, ThemeStyled, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::format::{group_digits, plural};
use crate::theme::{self, kind_icon, tabular};

/// Width of the controls rail.
const RAIL_WIDTH: f32 = 292.0;
/// Size of tick, category and value labels on the plot.
const LABEL_SIZE: f32 = 11.0;
const LABEL_HEIGHT: f32 = 14.0;
/// Widest a bar is drawn, and the gap between bars of one group.
const BAR_WIDTH: f32 = 24.0;
const BAR_GAP: f32 = 2.0;
/// Most bars that get a value label on top.
const LABELLED_BARS: usize = 12;
/// A dot's coloured disc and the ring around it in the surface colour.
const DOT: f32 = 8.0;
const RING: f32 = 2.0;
/// Most points per series a line marks with dots.
const LINE_DOTS: usize = 40;
/// Smallest plot area, each way, worth drawing in; a smaller one is left blank.
const MIN_PLOT: f32 = 48.0;

#[derive(Clone, Debug)]
pub enum ChartEvent {
    /// A bar was clicked: filter the grid to its category.
    AddFilter(Filter),
    /// Put this query in the SQL drawer, not run.
    ShowSql(String),
    Close,
}

/// What the chart draws from: the frame on screen and how it came about.
#[derive(Clone)]
pub struct Source {
    pub dataset: Arc<Dataset>,
    pub frame: Frame,
    /// The grid's filters and search (its sort doesn't change a chart).
    pub view: ViewSpec,
    /// The SQL the frame is the result of, when it is one.
    pub query: Option<String>,
    /// The unfiltered frame `frame` comes from (the file or the SQL result):
    /// its id, to tell a re-sort from a real change, and its rows.
    pub base: (u64, Option<u64>),
}

/// A chart as drawn: the engine's result and each series' colour slot.
struct Drawn {
    chart: Chart,
    /// Index into the categorical colours, or `None` for the accent
    /// (a single series) or the neutral grey (`Other`).
    slots: Vec<Option<usize>>,
}

/// The mark under the pointer, and where the pointer is (window coordinates).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Hover {
    series: usize,
    point: usize,
    at: Point<Pixels>,
}

/// A mark's hit target, in window coordinates, as the last paint placed it.
/// A line has one per x (`series` 0), covering the whole column.
#[derive(Clone, Copy, Debug)]
struct Hit {
    area: Bounds<Pixels>,
    series: usize,
    point: usize,
}

/// Which colour slot each series name has, kept while the colour field is
/// the same, so a series keeps its colour when a filter removes another.
#[derive(Default)]
struct Colors {
    /// The colour column, or `None` for one series per Y column.
    key: Option<Option<String>>,
    slots: HashMap<String, usize>,
}

impl Colors {
    /// The slot of each of `series` (see [`Drawn::slots`]). A name seen
    /// before under the same `key` keeps its slot; a new one takes the
    /// first slot no series on screen has.
    fn assign(&mut self, key: Option<String>, series: &[Series]) -> Vec<Option<usize>> {
        if self.key.as_ref() != Some(&key) {
            self.key = Some(key);
            self.slots.clear();
        }
        let named: Vec<&str> = series
            .iter()
            .filter(|s| !s.other)
            .map(|s| s.name.as_str())
            .collect();
        if named.len() < 2 {
            return vec![None; series.len()];
        }
        for name in &named {
            if !self.slots.contains_key(*name) {
                let taken: Vec<usize> = named
                    .iter()
                    .filter_map(|n| self.slots.get(*n).copied())
                    .collect();
                let free = (0..theme::SERIES_COLORS)
                    .find(|slot| !taken.contains(slot))
                    .unwrap_or(0);
                self.slots.insert(name.to_string(), free);
            }
        }
        series
            .iter()
            .map(|s| (!s.other).then(|| self.slots[&s.name]))
            .collect()
    }
}

pub struct ChartPanel {
    source: Option<Source>,
    spec: ChartSpec,
    drawn: Option<Rc<Drawn>>,
    /// What `drawn` (or the request in flight) was computed from: the spec,
    /// the base frame and the filters and search, so a re-sort is skipped.
    computed: Option<(ChartSpec, u64, ViewSpec)>,
    /// Why there is nothing to draw: `Pick a number column for Y.`
    error: Option<String>,
    /// Said once after a field was reset because its column went away.
    message: Option<String>,
    loading: bool,
    /// Bumped per request; a chart arriving for an older one is dropped.
    generation: u64,
    colors: Colors,
    hovered: Option<Hover>,
    hits: Rc<RefCell<Vec<Hit>>>,
}

impl ChartPanel {
    pub fn new() -> Self {
        Self {
            source: None,
            spec: ChartSpec::default(),
            drawn: None,
            computed: None,
            error: None,
            message: None,
            loading: false,
            generation: 0,
            colors: Colors::default(),
            hovered: None,
            hits: Rc::default(),
        }
    }

    /// Whether a chart is still being computed. For the screenshot harness.
    #[cfg(all(test, target_os = "macos"))]
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Shows the panel for `source`. With a selected column, or nothing
    /// picked yet, it starts from the chart that suits the columns.
    pub fn open(&mut self, source: Source, selected: Option<&str>, cx: &mut Context<Self>) {
        if self.spec.x.is_none() || selected.is_some_and(|s| self.spec.x.as_deref() != Some(s)) {
            self.spec = ChartSpec::suggest(&source.frame.columns, selected);
            self.message = None;
        }
        self.show(source, cx);
    }

    /// Follows the frame on screen: recomputes unless only the sort changed.
    /// Fields whose column is gone are reset, and the panel says so. With no
    /// X yet (as when the panel opened while the file was loading), it
    /// starts from the chart that suits the columns.
    pub fn show(&mut self, source: Source, cx: &mut Context<Self>) {
        let columns = &source.frame.columns;
        let has = |name: &String| columns.iter().any(|c| &c.name == name);
        let spec = &mut self.spec;
        if spec.x.is_none() {
            *spec = ChartSpec::suggest(columns, None);
        }
        let mut gone: Vec<String> = Vec::new();
        if let Some(x) = spec.x.clone().filter(|x| !has(x)) {
            gone.push(x);
            *spec = ChartSpec::suggest(columns, None);
        }
        gone.extend(spec.y.iter().filter(|y| !has(y)).cloned());
        spec.y.retain(|y| has(y));
        if let Some(color) = spec.color.take_if(|c| !has(c)) {
            gone.push(color);
        }
        if !gone.is_empty() {
            self.message = Some(format!(
                "{} {} not in this data, so the chart changed.",
                gone.join(", "),
                if gone.len() == 1 { "is" } else { "are" }
            ));
        }
        self.source = Some(source);
        self.refresh(cx);
    }

    /// Forgets the data, as when the document reopens; keeps the spec.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.source = None;
        self.drawn = None;
        self.computed = None;
        self.error = None;
        self.loading = false;
        self.hovered = None;
        self.generation += 1;
        cx.notify();
    }

    /// Changes the spec, as a control in the rail does, and redraws.
    pub fn set_spec(&mut self, spec: ChartSpec, cx: &mut Context<Self>) {
        self.spec = spec;
        self.message = None;
        self.refresh(cx);
    }

    /// Computes the chart in the background unless it is already drawn (or
    /// being drawn) for the same spec, base frame, filters and search.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.source.clone() else {
            return;
        };
        let view = ViewSpec {
            sort: Vec::new(),
            ..source.view.clone()
        };
        let key = (self.spec.clone(), source.base.0, view);
        if self.computed.as_ref() == Some(&key) {
            cx.notify();
            return;
        }
        self.computed = Some(key);
        self.loading = true;
        self.generation += 1;
        let generation = self.generation;
        let spec = self.spec.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { source.dataset.chart(&source.frame, &spec) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    // Only an older request racing ahead cancels the newest; ask again.
                    Err(err) if err.is::<Cancelled>() => {
                        this.computed = None;
                        this.refresh(cx);
                        return;
                    }
                    Ok(chart) => {
                        let key = (this.spec.y.len() <= 1).then(|| this.spec.color.clone());
                        let slots = this.colors.assign(key.flatten(), &chart.series);
                        this.drawn = Some(Rc::new(Drawn { chart, slots }));
                        this.error = None;
                    }
                    Err(err) => {
                        this.drawn = None;
                        this.error = Some(format!("{err:#}"));
                    }
                }
                this.loading = false;
                this.hovered = None;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn columns(&self) -> Vec<Column> {
        self.source
            .as_ref()
            .map(|s| s.frame.columns.to_vec())
            .unwrap_or_default()
    }

    fn set_kind(&mut self, kind: ChartKind, cx: &mut Context<Self>) {
        let spec = self.spec.with_kind(kind, &self.columns());
        self.set_spec(spec, cx);
    }

    /// Opens Show SQL: the drawn chart's query over `data` or the SQL result.
    fn show_sql(&mut self, cx: &mut Context<Self>) {
        if let (Some(drawn), Some(source)) = (&self.drawn, &self.source) {
            let sql = drawn.chart.sql(&source.view, source.query.as_deref());
            cx.emit(ChartEvent::ShowSql(sql));
        }
    }

    /// The colour of series `ix` of the drawn chart.
    fn series_color(drawn: &Drawn, ix: usize, cx: &App) -> Hsla {
        match (drawn.chart.series[ix].other, drawn.slots[ix]) {
            (true, _) => theme::other_series(cx),
            (false, Some(slot)) => theme::series_color(slot, cx),
            (false, None) => theme::palette(cx).accent,
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let hit = self
            .hits
            .borrow()
            .iter()
            .rev()
            .find(|h| h.area.contains(&event.position))
            .copied();
        let hover = hit.map(|h| Hover {
            series: h.series,
            point: h.point,
            at: event.position,
        });
        if hover != self.hovered {
            self.hovered = hover;
            cx.notify();
        }
    }

    /// A click on a bar filters the grid to its category (not `Other`).
    fn on_click(&mut self, cx: &mut Context<Self>) {
        let (Some(drawn), Some(hover)) = (&self.drawn, self.hovered) else {
            return;
        };
        let XAxis::Categories(categories) = &drawn.chart.x else {
            return;
        };
        let category = drawn.chart.series[hover.series]
            .points
            .get(hover.point)
            .and_then(|p| categories.get(p.x as usize));
        if let Some(filter) = category.and_then(|c| c.filter.clone()) {
            cx.emit(ChartEvent::AddFilter(filter));
        }
    }

    /// Hovers the first mark, as the pointer would. For the screenshot harness.
    #[cfg(all(test, target_os = "macos"))]
    pub fn hover_first(&mut self, cx: &mut Context<Self>) {
        let hit = self.hits.borrow().first().copied();
        self.hovered = hit.map(|h| Hover {
            series: h.series,
            point: h.point,
            at: h.area.center(),
        });
        cx.notify();
    }

    fn render_plot(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let sentence = |text: String| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .px(px(24.))
                .text_sm()
                .text_center()
                .text_color(muted)
                .child(SharedString::from(text))
                .into_any_element()
        };
        if let Some(error) = &self.error {
            // DuckDB can follow its reason with pages of detail.
            return sentence(error.lines().next().unwrap_or_default().to_string());
        }
        let Some(drawn) = self.drawn.clone() else {
            return sentence(if self.source.is_none() {
                "Loading the file…".into()
            } else {
                String::new()
            });
        };
        let colors: Vec<Hsla> = (0..drawn.chart.series.len())
            .map(|ix| Self::series_color(&drawn, ix, cx))
            .collect();
        let ink = Ink {
            text: cx.theme().foreground,
            muted,
            grid: cx.theme().border,
            surface: cx.theme().background,
            other: theme::other_series(cx),
        };
        let hits = self.hits.clone();
        let hovered = self.hovered;
        div()
            .id("plot")
            .size_full()
            .when(self.loading, |this| this.opacity(0.45))
            .on_mouse_move(cx.listener(|this, event, _, cx| this.on_mouse_move(event, cx)))
            .on_hover(cx.listener(|this, hovering: &bool, _, cx| {
                if !hovering && this.hovered.take().is_some() {
                    cx.notify();
                }
            }))
            .when(drawn.chart.kind == ChartKind::Bar, |this| {
                this.cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.on_click(cx)))
            })
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, cx| {
                        let painted = paint(bounds, &drawn, &colors, ink, hovered, window, cx);
                        *hits.borrow_mut() = painted;
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }

    /// The legend: a swatch and name per series, when there are two or more.
    fn render_legend(&self, cx: &App) -> Option<Div> {
        let drawn = self.drawn.as_ref()?;
        if drawn.chart.series.len() < 2 {
            return None;
        }
        Some(
            h_flex()
                .flex_wrap()
                .gap_x(px(14.))
                .gap_y(px(4.))
                .text_xs()
                .text_color(cx.theme().foreground)
                .children(drawn.chart.series.iter().enumerate().map(|(ix, s)| {
                    h_flex()
                        .gap(px(6.))
                        .items_center()
                        .child(
                            div()
                                .size(px(9.))
                                .rounded(px(2.))
                                .bg(Self::series_color(drawn, ix, cx)),
                        )
                        .child(SharedString::from(s.name.clone()))
                })),
        )
    }

    /// The tooltip for the hovered mark: its label and value in display format.
    fn render_tooltip(&self, cx: &App) -> Option<AnyElement> {
        let hover = self.hovered?;
        let drawn = self.drawn.as_ref()?;
        let chart = &drawn.chart;
        let mark = chart.series.get(hover.series)?.points.get(hover.point)?;
        let muted = cx.theme().muted_foreground;
        let row = |color: Option<Hsla>, name: String, value: String| {
            h_flex()
                .gap(px(6.))
                .items_center()
                .when_some(color, |this, color| {
                    this.child(div().size(px(8.)).rounded(px(2.)).bg(color))
                })
                .child(
                    div()
                        .flex_1()
                        .text_color(muted)
                        .child(SharedString::from(name)),
                )
                .child(
                    div()
                        .pl(px(12.))
                        .font_features(tabular())
                        .child(SharedString::from(value)),
                )
        };
        let several = chart.series.len() > 1;
        let rows: Vec<Div> = match chart.kind {
            // Every series' value at this x.
            ChartKind::Line => chart
                .series
                .iter()
                .enumerate()
                .map(|(ix, s)| {
                    let value = s.points.get(hover.point).and_then(|p| p.y);
                    row(
                        several.then(|| Self::series_color(drawn, ix, cx)),
                        s.name.clone(),
                        value.map_or("no value".into(), |v| chart.y.label(v)),
                    )
                })
                .collect(),
            ChartKind::Histogram => vec![row(
                None,
                "Rows".into(),
                group_digits(mark.y.unwrap_or(0.0) as u64),
            )],
            ChartKind::Bar => {
                let name = if several {
                    chart.series[hover.series].name.clone()
                } else {
                    measure(chart)
                };
                vec![row(
                    several.then(|| Self::series_color(drawn, hover.series, cx)),
                    name,
                    mark.y.map_or("no value".into(), |v| chart.y.label(v)),
                )]
            }
            // Both values, named by their columns.
            ChartKind::Scatter => {
                let name = |column: Option<&String>| column.cloned().unwrap_or_default();
                vec![
                    row(None, name(self.spec.x.as_ref()), mark.label.clone()),
                    row(
                        None,
                        name(self.spec.y.first()),
                        mark.y.map_or("no value".into(), |v| chart.y.label(v)),
                    ),
                ]
            }
        };
        // A scatter's dot has no label of its own beyond its values.
        let heading = match chart.kind {
            ChartKind::Scatter if several => Some(chart.series[hover.series].name.clone()),
            ChartKind::Scatter => None,
            _ => Some(mark.label.clone()),
        };
        Some(
            deferred(
                anchored()
                    .position(point(hover.at.x + px(14.), hover.at.y + px(14.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        v_flex()
                            .min_w(px(140.))
                            .px(px(10.))
                            .py(px(7.))
                            .gap(px(3.))
                            .popover_style(cx)
                            .text_xs()
                            .text_color(cx.theme().popover_foreground)
                            .children(heading.map(|heading| {
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(heading))
                            }))
                            .children(rows),
                    ),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }

    /// The footer: rows charted, whether filters apply, and the engine's notes.
    fn footer(&self) -> String {
        let Some(source) = &self.source else {
            return String::new();
        };
        let rows = source.frame.rows.unwrap_or(0);
        let filtered = !source.view.filters.is_empty() || !source.view.search.is_empty();
        let mut sentences = vec![match (filtered, source.query.is_some()) {
            (true, _) => format!(
                "{} of {}. Follows the current filters and search.",
                group_digits(rows),
                plural(source.base.1.unwrap_or(rows), "row")
            ),
            (false, true) => format!("{} of the SQL result.", plural(rows, "row")),
            (false, false) => format!("{}.", plural(rows, "row")),
        }];
        if let Some(drawn) = &self.drawn {
            if drawn.chart.kind == ChartKind::Scatter
                && let (Some(x), Some(y)) = (&self.spec.x, self.spec.y.first())
            {
                sentences.push(format!("X is {x}, Y is {y}."));
            }
            sentences.extend(drawn.chart.notes.iter().cloned());
        }
        sentences.join(" ")
    }

    fn render_rail(&self, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let columns = self.columns();
        let spec = self.spec.clone();
        let kind = spec.kind;
        let field = |label: &'static str, control: AnyElement| {
            h_flex()
                .w_full()
                .gap(px(8.))
                .items_center()
                .child(
                    div()
                        .w(theme::scaled(48., cx))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(control))
        };
        let icon_of = |name: Option<&String>| {
            name.and_then(|n| columns.iter().find(|c| &c.name == n))
                .map(|c| kind_icon(c.kind))
        };
        // Each picker's menu lists what it offers, checked when chosen.
        let menu = Menu {
            panel: cx.entity().downgrade(),
            spec: spec.clone(),
            columns: columns.clone(),
        };

        let x = picker(
            "chart-x",
            spec.x.clone().unwrap_or_else(|| "Pick a column".into()),
            icon_of(spec.x.as_ref()),
            menu.clone(),
            |m| {
                m.columns(
                    |c| m.spec.kind.fits_x(c),
                    |s, name| s.x.as_deref() == Some(name),
                    |s, name| s.x = Some(name),
                )
                .into_iter()
                .collect()
            },
        );

        let y_shown = match kind {
            ChartKind::Scatter => &spec.y[..spec.y.len().min(1)],
            _ => &spec.y[..],
        };
        let (y_label, y_icon) = match y_shown {
            [] if kind == ChartKind::Scatter => ("Pick a column".to_string(), None),
            [] => ("Count rows".to_string(), None),
            [one] => (one.clone(), icon_of(Some(one))),
            several => (format!("{} columns", several.len()), None),
        };
        let y = picker("chart-y", y_label, y_icon, menu.clone(), |m| {
            let kind = m.spec.kind;
            let several = kind.several_y();
            let count = (kind != ChartKind::Scatter).then(|| {
                m.item("None, count rows".into(), None, m.spec.y.is_empty(), |s| {
                    s.y.clear()
                })
            });
            // Bars and lines take up to `MAX_Y` Ys: a pick adds or removes one.
            let chosen = |s: &ChartSpec, name: &str| s.y.iter().any(|y| y == name);
            let full = several && m.spec.y.len() >= MAX_Y;
            let columns = m
                .columns(
                    |c| kind.fits_y(c),
                    chosen,
                    move |s, name| {
                        if !several {
                            s.y = vec![name];
                        } else if let Some(ix) = s.y.iter().position(|y| *y == name) {
                            s.y.remove(ix);
                        } else if s.y.len() < MAX_Y {
                            s.y.push(name);
                        }
                    },
                )
                .into_iter()
                .zip(m.columns.iter().filter(|c| kind.fits_y(c)))
                .map(|(item, c)| item.disabled(full && !chosen(&m.spec, &c.name)));
            count.into_iter().chain(columns).collect()
        });
        let aggregate = (kind.uses_aggregate() && !spec.y.is_empty()).then(|| {
            picker(
                "chart-aggregate",
                spec.aggregate.label().into(),
                None,
                menu.clone(),
                |m| {
                    Aggregate::ALL
                        .iter()
                        .map(|a| {
                            let a = *a;
                            m.item(a.label().into(), None, m.spec.aggregate == a, move |s| {
                                s.aggregate = a
                            })
                        })
                        .collect()
                },
            )
        });

        let color = picker(
            "chart-color",
            spec.color.clone().unwrap_or_else(|| "None".into()),
            icon_of(spec.color.as_ref()),
            menu.clone(),
            |m| {
                let none = m.item("None".into(), None, m.spec.color.is_none(), |s| {
                    s.color = None
                });
                let columns = m.columns(
                    |c| m.spec.kind.fits_color(c),
                    |s, name| s.color.as_deref() == Some(name),
                    |s, name| s.color = Some(name),
                );
                std::iter::once(none).chain(columns).collect()
            },
        );
        let sort = picker(
            "chart-sort",
            spec.sort.label().into(),
            None,
            menu.clone(),
            |m| {
                BarSort::ALL
                    .iter()
                    .map(|o| {
                        let o = *o;
                        m.item(o.label().into(), None, m.spec.sort == o, move |s| {
                            s.sort = o
                        })
                    })
                    .collect()
            },
        );

        let segments = h_flex()
            .w_full()
            .h(px(26.))
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(6.))
            .bg(cx.theme().secondary)
            .children(ChartKind::ALL.iter().map(|k| {
                let k = *k;
                let selected = k == kind;
                div()
                    .id(k.label())
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .text_xs()
                    .cursor_pointer()
                    .map(|this| {
                        if selected {
                            this.bg(cx.theme().background)
                                .text_color(cx.theme().foreground)
                                .font_weight(FontWeight::MEDIUM)
                                .shadow_xs()
                        } else {
                            this.text_color(muted)
                                .hover(|s| s.text_color(cx.theme().foreground))
                        }
                    })
                    .child(k.label())
                    .on_click(cx.listener(move |this, _, _, cx| this.set_kind(k, cx)))
            }));

        v_flex()
            .w(theme::scaled(RAIL_WIDTH, cx))
            .flex_shrink_0()
            .h_full()
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
                            .child("CHART"),
                    )
                    .child(
                        div()
                            .id("close-chart")
                            .size(px(22.))
                            .rounded(px(5.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(muted)
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().secondary_hover))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ChartEvent::Close)))
                            .child(Icon::empty().path("icons/x.svg").size(px(13.))),
                    ),
            )
            .child(
                v_flex()
                    .id("chart-controls")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(px(12.))
                    .gap(px(8.))
                    .child(segments)
                    .child(field(
                        if kind == ChartKind::Histogram {
                            "Column"
                        } else {
                            "X"
                        },
                        x.into_any_element(),
                    ))
                    .when(kind.uses_y(), |this| {
                        this.child(field(
                            "Y",
                            h_flex()
                                .gap(px(6.))
                                .child(div().flex_1().min_w_0().child(y))
                                .children(aggregate.map(|a| {
                                    div().w(theme::scaled(96., cx)).flex_shrink_0().child(a)
                                }))
                                .into_any_element(),
                        ))
                    })
                    .when(kind.uses_color() && spec.y.len() <= 1, |this| {
                        this.child(field("Color", color.into_any_element()))
                    })
                    .when(kind.uses_sort(), |this| {
                        this.child(field("Sort", sort.into_any_element()))
                    })
                    .when_some(self.message.clone(), |this, message| {
                        this.child(
                            div()
                                .pt(px(2.))
                                .text_xs()
                                .text_color(theme::palette(cx).warning)
                                .child(SharedString::from(message)),
                        )
                    }),
            )
            // Only a drawn chart has a query to show.
            .when(self.drawn.is_some(), |this| {
                this.child(
                    h_flex().p(px(12.)).flex_shrink_0().child(
                        Button::new("chart-sql")
                            .outline()
                            .small()
                            .icon(Icon::empty().path("icons/terminal.svg"))
                            .label("Show SQL")
                            .on_click(cx.listener(|this, _, _, cx| this.show_sql(cx))),
                    ),
                )
            })
    }
}

/// What a picker's menu needs to build its items when it opens: the panel
/// to update, and the spec and columns as they were when it was drawn.
#[derive(Clone)]
struct Menu {
    panel: WeakEntity<ChartPanel>,
    spec: ChartSpec,
    columns: Vec<Column>,
}

impl Menu {
    /// An item that applies `edit` to the spec when picked.
    fn item(
        &self,
        label: String,
        icon: Option<&'static str>,
        checked: bool,
        edit: impl Fn(&mut ChartSpec) + 'static,
    ) -> PopupMenuItem {
        let (panel, spec) = (self.panel.clone(), self.spec.clone());
        PopupMenuItem::new(label)
            .when_some(icon, |item, icon| item.icon(Icon::empty().path(icon)))
            .checked(checked)
            .on_click(move |_, _, cx| {
                let mut spec = spec.clone();
                edit(&mut spec);
                let _ = panel.update(cx, |panel, cx| panel.set_spec(spec, cx));
            })
    }

    /// An item per column that `fits`, with its type icon, checked when
    /// `chosen`; picking one calls `pick` with its name.
    fn columns(
        &self,
        fits: impl Fn(&Column) -> bool,
        chosen: impl Fn(&ChartSpec, &str) -> bool,
        pick: impl Fn(&mut ChartSpec, String) + Clone + 'static,
    ) -> Vec<PopupMenuItem> {
        let spec = &self.spec;
        self.columns
            .iter()
            .filter(|c| fits(c))
            .map(|c| {
                let name = c.name.clone();
                let chosen = chosen(spec, &name);
                let pick = pick.clone();
                self.item(name.clone(), Some(kind_icon(c.kind)), chosen, move |s| {
                    pick(s, name.clone())
                })
            })
            .collect()
    }
}

/// A field's dropdown: the current choice (with its column's type icon),
/// and a menu that `items` builds from `menu` each time it opens.
fn picker(
    id: &'static str,
    label: String,
    icon: Option<&'static str>,
    menu: Menu,
    items: impl Fn(&Menu) -> Vec<PopupMenuItem> + 'static,
) -> impl IntoElement {
    Button::new(id)
        .outline()
        .small()
        .w_full()
        .dropdown_caret(true)
        .child(
            h_flex()
                .min_w_0()
                .gap(px(6.))
                .items_center()
                .when_some(icon, |this, icon| {
                    this.child(Icon::empty().path(icon).size(px(12.)).flex_shrink_0())
                })
                .child(div().min_w_0().truncate().child(SharedString::from(label))),
        )
        .dropdown_menu(move |popup: PopupMenu, _, _| {
            items(&menu)
                .into_iter()
                .fold(popup.max_h(px(320.)).scrollable(true), |popup, item| {
                    popup.item(item)
                })
        })
}

/// What a one-series bar chart measures, for its tooltip: the title up to
/// ` by ` (`Sum of revenue`), or `Rows` when counting.
fn measure(chart: &Chart) -> String {
    match chart.series.first() {
        Some(s) if s.name == "Rows" => "Rows".into(),
        _ => chart
            .title
            .split(" by ")
            .next()
            .unwrap_or_default()
            .to_string(),
    }
}

/// A bar's value for the label on its top: millions short (`$12.2M`),
/// whole units from 100 up (`8,318`, `$8,318`), else as the axis writes it.
fn bar_label(axis: &Axis, value: f64) -> String {
    let symbol = match axis.scale {
        Scale::Number(Some(NumberFormat::Currency { symbol, .. })) => Some(symbol),
        _ => None,
    };
    let plain_or_currency = matches!(axis.scale, Scale::Number(None)) || symbol.is_some();
    if plain_or_currency && value.abs() >= 1e6 {
        let sign = if value < 0.0 { "-" } else { "" };
        let symbol = symbol.map(String::from).unwrap_or_default();
        return format!("{sign}{symbol}{:.1}M", value.abs() / 1e6);
    }
    match axis.scale {
        Scale::Number(Some(NumberFormat::Currency { symbol, .. })) if value.abs() >= 100.0 => {
            NumberFormat::Currency {
                symbol,
                decimals: 0,
            }
            .render(value)
        }
        Scale::Number(None) if value.abs() >= 100.0 => axis.label(value.round()),
        _ => axis.label(value),
    }
}

/// Theme colours the plot paints with.
#[derive(Clone, Copy)]
struct Ink {
    text: Hsla,
    muted: Hsla,
    grid: Hsla,
    surface: Hsla,
    /// The neutral grey of `Other`.
    other: Hsla,
}

/// A line of label text, shaped once to measure and paint.
fn shape(text: &str, color: Hsla, window: &mut Window) -> ShapedLine {
    let mut font = window.text_style().font();
    font.features = tabular();
    let text = SharedString::from(text.to_string());
    let run = TextRun {
        len: text.len(),
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(text, px(LABEL_SIZE), &[run], None)
}

/// Paints `line` with its centre (`align` 0.5), left (0) or right (1) edge
/// at `x`, and its middle at `y`.
fn put(line: &ShapedLine, x: f32, y: f32, align: f32, window: &mut Window, cx: &mut App) {
    let width = f32::from(line.width);
    let origin = point(px(x - width * align), px(y - LABEL_HEIGHT / 2.0));
    let _ = line.paint(origin, px(LABEL_HEIGHT), TextAlign::Left, None, window, cx);
}

/// Draws the chart into `bounds`: gridlines and ticks, then the marks.
/// Returns each mark's hit target for hovering and clicking.
fn paint(
    bounds: Bounds<Pixels>,
    drawn: &Drawn,
    colors: &[Hsla],
    ink: Ink,
    hovered: Option<Hover>,
    window: &mut Window,
    cx: &mut App,
) -> Vec<Hit> {
    let chart = &drawn.chart;
    let (left, top) = (f32::from(bounds.left()), f32::from(bounds.top()));
    let (right, bottom) = (f32::from(bounds.right()), f32::from(bounds.bottom()));

    // Y ticks set the left gutter; X labels take a line at the bottom.
    let y_ticks: Vec<(f64, ShapedLine)> = chart
        .y
        .ticks
        .iter()
        .map(|t| (t.value, shape(&t.label, ink.muted, window)))
        .collect();
    let gutter = y_ticks
        .iter()
        .map(|(_, l)| f32::from(l.width))
        .fold(0.0, f32::max)
        + 10.0;
    let x0 = left + gutter;
    let x1 = right - 12.0;
    let y0 = top + LABEL_HEIGHT;
    let y1 = bottom - LABEL_HEIGHT - 8.0;
    if x1 - x0 < MIN_PLOT || y1 - y0 < MIN_PLOT {
        return Vec::new();
    }
    let (w, h) = (x1 - x0, y1 - y0);

    let span = |axis: &Axis| (axis.min, (axis.max - axis.min).max(f64::EPSILON));
    let (y_min, y_span) = span(&chart.y);
    let fy = |v: f64| y1 - ((v - y_min) / y_span) as f32 * h;
    let line = |a: Point<Pixels>, b: Point<Pixels>, color: Hsla, window: &mut Window| {
        window.paint_quad(fill(Bounds::from_corners(a, b), color));
    };

    for (value, label) in &y_ticks {
        let y = fy(*value).round();
        line(
            point(px(x0), px(y)),
            point(px(x1), px(y + 1.0)),
            ink.grid,
            window,
        );
        put(label, x0 - 8.0, y, 1.0, window, cx);
    }

    let mut hits = Vec::new();
    let full = |a: f32, b: f32| Bounds::from_corners(point(px(a), px(y0)), point(px(b), px(y1)));
    // Zero, or the axis end nearest it (`max`/`min`, which don't panic as `clamp` can).
    let baseline = fy(0.0f64.max(chart.y.min).min(chart.y.max));

    match &chart.x {
        XAxis::Categories(categories) => {
            let n = categories.len().max(1);
            let k = chart.series.len().max(1);
            let slot = w / n as f32;
            let bar = ((slot * 0.7 - BAR_GAP * (k - 1) as f32) / k as f32).clamp(1.0, BAR_WIDTH);
            let group = bar * k as f32 + BAR_GAP * (k - 1) as f32;
            let hovering = hovered.is_some();

            // Category labels, every `step`th when slots are narrow.
            let step = (56.0 / slot).ceil().max(1.0) as usize;
            for (ix, category) in categories.iter().enumerate().step_by(step) {
                let room = slot * step as f32 - 8.0;
                let text = truncate_text_to_width(
                    &SharedString::from(category.label.clone()),
                    px(LABEL_SIZE),
                    room,
                    window,
                );
                let label = shape(&text, ink.muted, window);
                put(
                    &label,
                    x0 + slot * (ix as f32 + 0.5),
                    y1 + 14.0,
                    0.5,
                    window,
                    cx,
                );
            }

            // Value labels when there are few bars and every one fits.
            let labels: Vec<Vec<Option<ShapedLine>>> = if n * k <= LABELLED_BARS {
                chart
                    .series
                    .iter()
                    .map(|s| {
                        s.points
                            .iter()
                            .map(|p| {
                                p.y.map(|v| shape(&bar_label(&chart.y, v), ink.text, window))
                            })
                            .collect()
                    })
                    .collect()
            } else {
                Vec::new()
            };
            // One series: every label fits its slot, or none is drawn. Several:
            // each label that fits its own bar and gap, so none runs over a neighbour.
            let room = slot - 4.0;
            let fits = labels
                .iter()
                .flatten()
                .flatten()
                .all(|l| f32::from(l.width) <= room.max(bar));
            let fits = |label: &ShapedLine| {
                if k == 1 {
                    fits
                } else {
                    f32::from(label.width) <= bar + BAR_GAP
                }
            };

            for (si, series) in chart.series.iter().enumerate() {
                for (pi, p) in series.points.iter().enumerate() {
                    let Some(value) = p.y else { continue };
                    let left =
                        x0 + slot * p.x as f32 + (slot - group) / 2.0 + si as f32 * (bar + BAR_GAP);
                    let end = fy(value);
                    let (a, b) = (
                        end.min(baseline),
                        end.max(baseline).max(end.min(baseline) + 1.0),
                    );
                    let radius = px(4.0f32.min(bar / 2.0).min(b - a));
                    let corners = if value >= 0.0 {
                        Corners {
                            top_left: radius,
                            top_right: radius,
                            ..Default::default()
                        }
                    } else {
                        Corners {
                            bottom_left: radius,
                            bottom_right: radius,
                            ..Default::default()
                        }
                    };
                    let is_hovered = hovered.is_some_and(|h| h.series == si && h.point == pi);
                    // The `Other` bar, which can't be clicked, is grey.
                    let other = categories
                        .get(p.x as usize)
                        .is_some_and(|c| c.filter.is_none());
                    let color = if other { ink.other } else { colors[si] };
                    let color = if hovering && !is_hovered {
                        color.opacity(0.6)
                    } else {
                        color
                    };
                    window.paint_quad(
                        fill(
                            Bounds::from_corners(
                                point(px(left), px(a)),
                                point(px(left + bar), px(b)),
                            ),
                            color,
                        )
                        .corner_radii(corners),
                    );
                    if let Some(Some(label)) = labels.get(si).and_then(|l| l.get(pi))
                        && fits(label)
                    {
                        let y = if value >= 0.0 { a - 9.0 } else { b + 9.0 };
                        put(label, left + bar / 2.0, y, 0.5, window, cx);
                    }
                    hits.push(Hit {
                        area: full(left - BAR_GAP / 2.0, left + bar + BAR_GAP / 2.0),
                        series: si,
                        point: pi,
                    });
                }
            }
        }
        XAxis::Values(axis) => {
            let (x_min, x_span) = span(axis);
            // Dots keep clear of the plot's edges; histogram bins run to them.
            let inset = if chart.kind == ChartKind::Histogram {
                0.0
            } else {
                DOT
            };
            // A single value (one bucket) sits in the middle.
            let fx = |v: f64| {
                if axis.max > axis.min {
                    x0 + inset + ((v - x_min) / x_span) as f32 * (w - 2.0 * inset)
                } else {
                    x0 + w / 2.0
                }
            };
            let ticks: Vec<(f32, ShapedLine)> = axis
                .ticks
                .iter()
                .map(|t| (fx(t.value), shape(&t.label, ink.muted, window)))
                .collect();
            // Skip ticks whose label would run into the previous one.
            let mut last_end = f32::NEG_INFINITY;
            for (x, label) in &ticks {
                let width = f32::from(label.width);
                // Inside the plot's bounds, or at its left edge when wider.
                let start = (x - width / 2.0).min(right - width).max(left);
                if start < last_end + 12.0 {
                    continue;
                }
                put(label, start, y1 + 14.0, 0.0, window, cx);
                last_end = start + width;
            }

            match chart.kind {
                ChartKind::Histogram => {
                    let width = chart.bin_width.unwrap_or(0.0);
                    let color = colors.first().copied().unwrap_or(ink.text);
                    for (pi, p) in chart
                        .series
                        .first()
                        .into_iter()
                        .flat_map(|s| s.points.iter())
                        .enumerate()
                    {
                        let (a, b) = (fx(p.x) + 0.5, fx(p.x + width) - 0.5);
                        let value = p.y.unwrap_or(0.0);
                        if value > 0.0 {
                            let end = fy(value).min(baseline - 1.0);
                            let radius = px(4.0f32.min((b - a) / 2.0).min(baseline - end));
                            let is_hovered = hovered.is_some_and(|h| h.point == pi);
                            window.paint_quad(
                                fill(
                                    Bounds::from_corners(
                                        point(px(a), px(end)),
                                        point(px(b), px(baseline)),
                                    ),
                                    if hovered.is_some() && !is_hovered {
                                        color.opacity(0.6)
                                    } else {
                                        color
                                    },
                                )
                                .corner_radii(Corners {
                                    top_left: radius,
                                    top_right: radius,
                                    ..Default::default()
                                }),
                            );
                        }
                        hits.push(Hit {
                            area: full(a - 0.5, b + 0.5),
                            series: 0,
                            point: pi,
                        });
                    }
                }
                ChartKind::Line => {
                    let points = chart.series.first().map_or(0, |s| s.points.len());
                    // Each x owns the column halfway to its neighbours.
                    let xs: Vec<f32> = chart
                        .series
                        .first()
                        .map(|s| s.points.iter().map(|p| fx(p.x)).collect())
                        .unwrap_or_default();
                    if let Some(hover) = hovered
                        && let Some(x) = xs.get(hover.point)
                    {
                        line(
                            point(px(*x), px(y0)),
                            point(px(x + 1.0), px(y1)),
                            ink.muted.opacity(0.5),
                            window,
                        );
                    }
                    for (si, series) in chart.series.iter().enumerate() {
                        let color = colors[si];
                        let mut runs: Vec<Vec<Point<Pixels>>> = vec![Vec::new()];
                        for p in &series.points {
                            match p.y {
                                Some(v) => {
                                    runs.last_mut().unwrap().push(point(px(fx(p.x)), px(fy(v))))
                                }
                                None => runs.push(Vec::new()),
                            }
                        }
                        for run in runs.iter().filter(|r| r.len() > 1) {
                            let mut path = PathBuilder::stroke(px(2.0));
                            path.move_to(run[0]);
                            for at in &run[1..] {
                                path.line_to(*at);
                            }
                            if let Ok(path) = path.build() {
                                window.paint_path(path, color);
                            }
                            // Round joins.
                            for at in run {
                                dot(*at, 2.0, 0.0, color, ink.surface, window);
                            }
                        }
                        for run in &runs {
                            // Dots when there are few points, and always for a lone one.
                            if points <= LINE_DOTS || run.len() == 1 {
                                for at in run {
                                    dot(*at, DOT, RING, color, ink.surface, window);
                                }
                            }
                        }
                    }
                    for (pi, x) in xs.iter().enumerate() {
                        let before = if pi == 0 { x0 } else { (xs[pi - 1] + x) / 2.0 };
                        let after = xs.get(pi + 1).map_or(x1, |next| (x + next) / 2.0);
                        hits.push(Hit {
                            area: full(before.min(x - 6.0), after.max(x + 6.0)),
                            series: 0,
                            point: pi,
                        });
                    }
                }
                ChartKind::Scatter | ChartKind::Bar => {
                    let reach = DOT / 2.0 + RING;
                    // `Other` first, under the named series.
                    let mut order: Vec<usize> = (0..chart.series.len()).collect();
                    order.sort_by_key(|si| !chart.series[*si].other);
                    for (si, series) in order.into_iter().map(|si| (si, &chart.series[si])) {
                        for (pi, p) in series.points.iter().enumerate() {
                            let Some(v) = p.y else { continue };
                            let at = point(px(fx(p.x)), px(fy(v)));
                            dot(at, DOT, RING, colors[si], ink.surface, window);
                            hits.push(Hit {
                                area: Bounds::from_corners(
                                    point(at.x - px(reach), at.y - px(reach)),
                                    point(at.x + px(reach), at.y + px(reach)),
                                ),
                                series: si,
                                point: pi,
                            });
                        }
                    }
                    if let Some(hover) = hovered
                        && let Some(p) = chart.series[hover.series].points.get(hover.point)
                        && let Some(v) = p.y
                    {
                        let at = point(px(fx(p.x)), px(fy(v)));
                        dot(
                            at,
                            DOT + 4.0,
                            RING,
                            colors[hover.series],
                            ink.surface,
                            window,
                        );
                    }
                }
            }
        }
    }
    hits
}

/// A disc `size` across centred on `at`, in a `ring` of the surface colour.
fn dot(at: Point<Pixels>, size: f32, ring: f32, color: Hsla, surface: Hsla, window: &mut Window) {
    let r = size / 2.0 + ring;
    window.paint_quad(
        fill(
            Bounds::from_corners(
                point(at.x - px(r), at.y - px(r)),
                point(at.x + px(r), at.y + px(r)),
            ),
            color,
        )
        .corner_radii(px(r))
        .border_widths(px(ring))
        .border_color(surface),
    );
}

impl EventEmitter<ChartEvent> for ChartPanel {}

impl Render for ChartPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let title = self
            .drawn
            .as_ref()
            .filter(|_| self.error.is_none())
            .map(|d| d.chart.title.clone())
            .unwrap_or_default();
        h_flex()
            .size_full()
            .items_stretch()
            .bg(cx.theme().background)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .px(px(14.))
                    .pt(px(12.))
                    .pb(px(10.))
                    .gap(px(8.))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .min_h(px(18.))
                            .child(SharedString::from(title)),
                    )
                    .children(self.render_legend(cx))
                    .child(div().flex_1().min_h_0().child(self.render_plot(cx)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .truncate()
                            .child(SharedString::from(self.footer())),
                    ),
            )
            .child(self.render_rail(cx))
            .children(self.render_tooltip(cx))
    }
}

#[cfg(test)]
mod tests {
    use da_core::{Axis, NumberFormat, Point, Scale, Series};

    use super::{Colors, bar_label};

    fn series(names: &[&str]) -> Vec<Series> {
        names
            .iter()
            .map(|name| Series {
                name: name.to_string(),
                other: *name == "Other",
                points: vec![Point {
                    x: 0.0,
                    y: Some(1.0),
                    label: String::new(),
                }],
            })
            .collect()
    }

    #[test]
    fn a_series_keeps_its_colour_when_another_goes() {
        let mut colors = Colors::default();
        let key = Some("region".to_string());
        let all = colors.assign(key.clone(), &series(&["East", "North", "South", "Other"]));
        assert_eq!(all, [Some(0), Some(1), Some(2), None]);
        // A filter removes North; the engine's order shifts, the colours don't.
        let fewer = colors.assign(key.clone(), &series(&["South", "East"]));
        assert_eq!(fewer, [Some(2), Some(0)]);
        // A newcomer takes a free slot.
        let back = colors.assign(key, &series(&["West", "South", "East"]));
        assert_eq!(back, [Some(1), Some(2), Some(0)]);
        // Another colour column starts over; one series takes the accent.
        assert_eq!(
            colors.assign(Some("product".into()), &series(&["Bar", "Oat"])),
            [Some(0), Some(1)]
        );
        assert_eq!(colors.assign(None, &series(&["revenue"])), [None]);
    }

    #[test]
    fn bar_labels_round_big_values() {
        let axis = |scale| Axis {
            scale,
            min: 0.0,
            max: 10_000.0,
            ticks: Vec::new(),
        };
        let plain = axis(Scale::Number(None));
        assert_eq!(bar_label(&plain, 8317.87), "8,318");
        assert_eq!(bar_label(&plain, 12.5), "12.5");
        let dollars = axis(Scale::Number(Some(NumberFormat::Currency {
            symbol: '$',
            decimals: 2,
        })));
        assert_eq!(bar_label(&dollars, 8317.87), "$8,318");
        assert_eq!(bar_label(&dollars, 12.5), "$12.50");
        assert_eq!(bar_label(&dollars, 12_191_185.0), "$12.2M");
    }
}
