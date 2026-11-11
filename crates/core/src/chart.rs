//! Charts. The UI edits a small [`ChartSpec`] field by field, and
//! [`Dataset::chart`] turns it into SQL over a frame and returns a [`Chart`]:
//! the series to draw, both axes with their ticks and display formats, a
//! title, and notes for the footer. [`Chart::sql`] gives the query back as
//! readable SQL over `data` for Show SQL. No UI types live here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use duckdb::{Connection, InterruptHandle, Row};

use crate::clean::{NumberFormat, group_thousands};
use crate::dataset::{Cancelled, Column, ColumnKind, Dataset, Frame, is_current};
use crate::profile::{self, tidy_number};
use crate::sql::{Filter, FilterOp, ViewSpec, quote_ident};

/// Most bars drawn; the rest are summed into `Other` or left out.
const TOP_BARS: usize = 20;
/// Most series a colour column splits bars and lines into, before `Other`.
const TOP_SERIES: usize = 6;
/// Most Y columns a bar or line takes, one series each: as many as there
/// are categorical colours.
pub const MAX_Y: usize = 6;
/// Most series a colour column splits a scatter into, before `Other`.
const TOP_SCATTER_SERIES: usize = 3;
/// Most points a scatter draws; more rows are sampled down to this.
const SAMPLE_POINTS: u64 = 5000;
/// About how many points a line has at most: numbers with more distinct
/// values are binned, and dates are never bucketed finer than this allows.
const LINE_POINTS: usize = 100;
/// Fewest points a date line aims for; see [`DateUnit::for_line`].
const LINE_MIN_POINTS: usize = 6;
/// About how many bins a histogram has.
const HISTOGRAM_BINS: f64 = 30.0;
/// Most ticks on a date axis.
const DATE_TICKS: usize = 8;
/// The label of a NULL category or colour.
const EMPTY: &str = "(empty)";
const OTHER: &str = "Other";
const NO_ROWS: &str = "No rows to chart.";
const DAY: f64 = 86_400.0;

/// The four chart forms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChartKind {
    #[default]
    Bar,
    Line,
    Scatter,
    Histogram,
}

impl ChartKind {
    /// In the order of the segmented control.
    pub const ALL: [Self; 4] = [Self::Bar, Self::Line, Self::Scatter, Self::Histogram];

    pub fn label(self) -> &'static str {
        match self {
            Self::Bar => "Bar",
            Self::Line => "Line",
            Self::Scatter => "Scatter",
            Self::Histogram => "Histogram",
        }
    }

    /// Whether `column` can be this chart's X: any column for a bar (its
    /// values become categories), a date or number for a line or
    /// histogram, a number for a scatter.
    pub fn fits_x(self, column: &Column) -> bool {
        match self {
            Self::Bar => column.kind != ColumnKind::Other,
            Self::Line | Self::Histogram => {
                matches!(column.kind, ColumnKind::Number | ColumnKind::Temporal)
            }
            Self::Scatter => column.kind == ColumnKind::Number,
        }
    }

    /// Whether `column` can be a Y: numbers only, for every kind that has one.
    pub fn fits_y(self, column: &Column) -> bool {
        self.uses_y() && column.kind == ColumnKind::Number
    }

    /// Whether `column` can split the chart into coloured series: text or yes/no.
    pub fn fits_color(self, column: &Column) -> bool {
        self.uses_color() && matches!(column.kind, ColumnKind::Text | ColumnKind::Boolean)
    }

    /// Whether the Y field applies. A histogram counts rows of X.
    pub fn uses_y(self) -> bool {
        self != Self::Histogram
    }

    /// Whether Y takes several columns (one series each). A scatter uses the first.
    pub fn several_y(self) -> bool {
        matches!(self, Self::Bar | Self::Line)
    }

    /// Whether the aggregate applies. A scatter plots rows as they are.
    pub fn uses_aggregate(self) -> bool {
        matches!(self, Self::Bar | Self::Line)
    }

    /// Whether the Color field applies. It is ignored while Y has several columns.
    pub fn uses_color(self) -> bool {
        self != Self::Histogram
    }

    /// Whether the Sort field applies.
    pub fn uses_sort(self) -> bool {
        self == Self::Bar
    }
}

/// How bars and lines combine the Y values that fall on one X.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Aggregate {
    #[default]
    Sum,
    Average,
    /// Rows with a Y value (every row when there is no Y).
    Count,
    Min,
    Max,
    Median,
}

impl Aggregate {
    /// In the order of the dropdown.
    pub const ALL: [Self; 6] = [
        Self::Sum,
        Self::Average,
        Self::Count,
        Self::Min,
        Self::Max,
        Self::Median,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Sum => "Sum",
            Self::Average => "Average",
            Self::Count => "Count",
            Self::Min => "Min",
            Self::Max => "Max",
            Self::Median => "Median",
        }
    }

    fn function(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Average => "avg",
            Self::Count => "count",
            Self::Min => "min",
            Self::Max => "max",
            Self::Median => "median",
        }
    }

    /// Whether the result is in Y's unit, so Y's display format carries over.
    fn keeps_unit(self) -> bool {
        self != Self::Count
    }

    /// Whether the values of several groups add up, so the ones cut off can
    /// be shown summed as `Other`, and empty line buckets are zero.
    fn adds_up(self) -> bool {
        matches!(self, Self::Sum | Self::Count)
    }
}

/// The order bars are drawn in. Which bars are drawn is always the top 20 by value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BarSort {
    #[default]
    ValueDescending,
    ValueAscending,
    /// By category: text alphabetically, numbers and dates in order.
    Label,
}

impl BarSort {
    /// In the order of the dropdown.
    pub const ALL: [Self; 3] = [Self::ValueDescending, Self::ValueAscending, Self::Label];

    pub fn label(self) -> &'static str {
        match self {
            Self::ValueDescending => "Value, high to low",
            Self::ValueAscending => "Value, low to high",
            Self::Label => "Label",
        }
    }
}

/// What to chart, edited by the UI field by field. Fields the kind doesn't
/// use (see the `uses_*` methods on [`ChartKind`]) are kept but ignored, so
/// switching kinds back and forth loses nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChartSpec {
    pub kind: ChartKind,
    pub x: Option<String>,
    /// Bars and lines draw one series per column; with none they count rows.
    pub y: Vec<String>,
    pub aggregate: Aggregate,
    pub color: Option<String>,
    pub sort: BarSort,
}

impl ChartSpec {
    /// The chart that suits an X and Y column:
    ///
    /// | X | Y | Chart |
    /// |---|---|---|
    /// | text, yes/no | number | bar, sum of Y |
    /// | text, yes/no | none | bar, row count |
    /// | date | number | line, sum of Y |
    /// | date | none | line, row count |
    /// | number | number | scatter |
    /// | number | none | histogram of X |
    pub fn for_columns(x: &Column, y: Option<&Column>) -> Self {
        let kind = match (x.kind, y.is_some()) {
            (ColumnKind::Temporal, _) => ChartKind::Line,
            (ColumnKind::Number, true) => ChartKind::Scatter,
            (ColumnKind::Number, false) => ChartKind::Histogram,
            _ => ChartKind::Bar,
        };
        Self {
            kind,
            x: Some(x.name.clone()),
            y: y.map(|y| vec![y.name.clone()]).unwrap_or_default(),
            ..Self::default()
        }
    }

    /// The chart the panel opens with. X is the selected column, or else the
    /// first text, then date, then number column. Y is the measure (the
    /// first currency column, else the last number column not named like an
    /// id), except under a number X, which gets a histogram. Then
    /// [`ChartSpec::for_columns`] picks the form.
    pub fn suggest(columns: &[Column], selected: Option<&str>) -> Self {
        let first = |kinds: &[ColumnKind]| columns.iter().find(|c| kinds.contains(&c.kind));
        let x = selected
            .and_then(|name| columns.iter().find(|c| c.name == name))
            .filter(|c| c.kind != ColumnKind::Other)
            .or_else(|| first(&[ColumnKind::Text, ColumnKind::Boolean]))
            .or_else(|| first(&[ColumnKind::Temporal]))
            .or_else(|| first(&[ColumnKind::Number]));
        let Some(x) = x else {
            return Self::default();
        };
        let y = match x.kind {
            ColumnKind::Number => None,
            _ => measure(columns, &x.name),
        };
        Self::for_columns(x, y)
    }

    /// This spec switched to `kind`: X is kept if it fits (a bar doesn't
    /// keep a date), else it is the first column that fits and isn't a Y,
    /// preferring a date for a line and text for a bar, and skipping
    /// columns named like an id. A scatter gets the measure as Y if it has
    /// no number Y. Everything else is kept as it was.
    pub fn with_kind(&self, kind: ChartKind, columns: &[Column]) -> Self {
        let find = |name: &str| columns.iter().find(|c| c.name == name);
        let mut spec = Self {
            kind,
            ..self.clone()
        };
        let keep = spec.x.as_deref().and_then(find).is_some_and(|c| {
            kind.fits_x(c) && !(kind == ChartKind::Bar && c.kind == ColumnKind::Temporal)
        });
        if !keep {
            let preferred = |c: &Column| match kind {
                ChartKind::Line => c.kind == ColumnKind::Temporal,
                ChartKind::Bar => matches!(c.kind, ColumnKind::Text | ColumnKind::Boolean),
                _ => true,
            };
            spec.x = columns
                .iter()
                .filter(|c| kind.fits_x(c) && !spec.y.contains(&c.name))
                .min_by_key(|c| (!preferred(c), is_id(&c.name)))
                .map(|c| c.name.clone());
        }
        let x = spec.x.clone().unwrap_or_default();
        let y_fits = spec
            .y
            .first()
            .and_then(|y| find(y))
            .is_some_and(|c| kind.fits_y(c) && c.name != x);
        if kind == ChartKind::Scatter && !y_fits {
            spec.y = measure(columns, &x)
                .map(|c| vec![c.name.clone()])
                .unwrap_or_default();
        }
        spec
    }
}

/// The column a chart most likely measures: the first currency column,
/// else the last number column not named like an id. Never `except`.
fn measure<'a>(columns: &'a [Column], except: &str) -> Option<&'a Column> {
    let numbers = || {
        columns
            .iter()
            .filter(move |c| c.kind == ColumnKind::Number && c.name != except)
    };
    numbers()
        .find(|c| matches!(c.format, Some(NumberFormat::Currency { .. })))
        .or_else(|| numbers().rfind(|c| !is_id(&c.name)))
}

/// Whether a column is named like an id: `id`, `order_id`, `Order ID`.
fn is_id(name: &str) -> bool {
    let name = name.to_lowercase();
    name == "id" || name.ends_with("_id") || name.ends_with(" id")
}

/// What to draw, from [`Dataset::chart`].
#[derive(Clone, Debug)]
pub struct Chart {
    pub kind: ChartKind,
    /// Written from the spec, like `Sum of revenue by product`.
    pub title: String,
    pub x: XAxis,
    /// The value axis, from zero for bars and histograms. Bars, lines and
    /// histograms measure with it; a scatter's Y column is on it.
    pub y: Axis,
    /// In legend order: largest first, `Other` last. Every series of a bar
    /// or line chart has one point per category or bucket, at the same xs.
    pub series: Vec<Series>,
    /// For a histogram, the width of every bin: a point covers
    /// `x .. x + bin_width`. `None` for the other kinds.
    pub bin_width: Option<f64>,
    /// Sentences for the footer, like `Top 20 of 143 values. The rest are
    /// grouped as Other.` Empty when there is nothing to say.
    pub notes: Vec<String>,
    query: Readable,
}

/// What the x axis is.
#[derive(Clone, Debug)]
pub enum XAxis {
    /// Bars: one slot per category, in drawing order. A point's x is its
    /// index here.
    Categories(Vec<Category>),
    /// Lines, scatters and histograms: points sit at their value (epoch
    /// seconds for dates).
    Values(Axis),
}

/// One bar slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Category {
    /// The value as text, `(empty)` for NULL, or `Other`.
    pub label: String,
    /// The filter that clicking this bar adds, like `product = Granola 500g`.
    /// `None` for `Other`, which isn't clickable.
    pub filter: Option<Filter>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    /// The Y column, the colour value (`(empty)` for NULL), `Other`, or
    /// `Rows` when counting.
    pub name: String,
    /// Whether this is the `Other` series, drawn in a neutral colour.
    pub other: bool,
    pub points: Vec<Point>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    /// A category index, a value, epoch seconds, or a bin's start.
    pub x: f64,
    /// `None` is a gap: nothing to draw here.
    pub y: Option<f64>,
    /// What this point stands for on X, ready for a tooltip: the category,
    /// the date bucket (`Mar 2026`), the bin (`5 to 5.5`) or the value.
    pub label: String,
}

/// A continuous axis: its extent, where the ticks go, and how values read.
#[derive(Clone, Debug, PartialEq)]
pub struct Axis {
    pub scale: Scale,
    /// The extent to draw. On value axes these are clean tick values.
    pub min: f64,
    pub max: f64,
    /// At clean values within `min ..= max`, labelled for the axis:
    /// `2,500`, `10K`, `$1.2M`, `20%`, `Mar 2026`.
    pub ticks: Vec<Tick>,
}

/// How an axis's values read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scale {
    /// Plain numbers, or in the column's currency or percent format.
    Number(Option<NumberFormat>),
    /// Epoch seconds, read as dates to this unit (a line's bucket unit).
    Date(DateUnit),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tick {
    pub value: f64,
    pub label: String,
}

impl Axis {
    /// A value in full in the axis's format, for tooltips and bar labels:
    /// `8,317.87`, `$8,317.87`, `12.6%`, `Mar 2026`.
    pub fn label(&self, value: f64) -> String {
        self.scale.label(value)
    }
}

impl Scale {
    /// See [`Axis::label`].
    pub fn label(self, value: f64) -> String {
        match self {
            Self::Number(Some(format)) => format.render(value),
            Self::Number(None) => plain(value),
            Self::Date(unit) => unit.format(value),
        }
    }
}

/// The calendar unit dates are grouped by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateUnit {
    Day,
    /// Starting on Monday.
    Week,
    Month,
    Quarter,
    Year,
}

impl DateUnit {
    const ALL: [Self; 5] = [
        Self::Day,
        Self::Week,
        Self::Month,
        Self::Quarter,
        Self::Year,
    ];

    /// The lower-case name, for titles: `Sum of revenue by month`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Year => "year",
        }
    }

    /// Epoch seconds as this unit writes a bucket: `Mar 4, 2026` (a day, or
    /// the Monday a week starts), `Mar 2026`, `Q1 2026`, `2026`.
    pub fn format(self, seconds: f64) -> String {
        let (y, m, d) = civil_from_days((seconds / DAY).floor() as i64);
        let month = MONTHS[m as usize - 1];
        match self {
            Self::Day | Self::Week => format!("{month} {d}, {y}"),
            Self::Month => format!("{month} {y}"),
            Self::Quarter => format!("Q{} {y}", (m - 1) / 3 + 1),
            Self::Year => y.to_string(),
        }
    }

    /// The unit a line over `lo ..= hi` (epoch seconds) is bucketed by, and
    /// its buckets' first days. Month when that gives 6 to 100 points; under
    /// 6, week, or day when weeks give under 6 too; over 100, quarter, or
    /// year when quarters give over 100 too.
    fn for_line(lo: f64, hi: f64) -> (Self, Vec<i64>) {
        let fits = |unit: Self| {
            unit.buckets(lo, hi, LINE_POINTS)
                .filter(|b| b.len() >= LINE_MIN_POINTS)
                .map(|b| (unit, b))
        };
        let all = |unit: Self| (unit, unit.buckets(lo, hi, usize::MAX).unwrap_or_default());
        if Self::Month.buckets(lo, hi, LINE_POINTS).is_some() {
            fits(Self::Month)
                .or_else(|| fits(Self::Week))
                .unwrap_or_else(|| all(Self::Day))
        } else {
            Self::Quarter
                .buckets(lo, hi, LINE_POINTS)
                .map(|b| (Self::Quarter, b))
                .unwrap_or_else(|| all(Self::Year))
        }
    }

    /// The first day (days since 1970-01-01) of the bucket `day` is in.
    fn start(self, day: i64) -> i64 {
        let (y, m, _) = civil_from_days(day);
        match self {
            Self::Day => day,
            Self::Week => day - (day + 3).rem_euclid(7),
            Self::Month => days_from_civil(y, m, 1),
            Self::Quarter => days_from_civil(y, (m - 1) / 3 * 3 + 1, 1),
            Self::Year => days_from_civil(y, 1, 1),
        }
    }

    /// The first day of the bucket after the one starting on `start`.
    fn next(self, start: i64) -> i64 {
        let (y, m, _) = civil_from_days(start);
        let months = |n: u32| {
            let m0 = m - 1 + n;
            days_from_civil(y + i64::from(m0 / 12), m0 % 12 + 1, 1)
        };
        match self {
            Self::Day => start + 1,
            Self::Week => start + 7,
            Self::Month => months(1),
            Self::Quarter => months(3),
            Self::Year => days_from_civil(y + 1, 1, 1),
        }
    }

    /// The starts of the buckets from the one holding `lo` to the one
    /// holding `hi` (epoch seconds), or `None` past `limit` buckets.
    fn buckets(self, lo: f64, hi: f64, limit: usize) -> Option<Vec<i64>> {
        let last = (hi / DAY).floor() as i64;
        let mut day = self.start((lo / DAY).floor() as i64);
        let mut out = Vec::new();
        while day <= last {
            if out.len() == limit {
                return None;
            }
            out.push(day);
            day = self.next(day);
        }
        Some(out)
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

impl Chart {
    /// The chart's query as readable SQL for Show SQL: over `data`, or over
    /// `query` when the frame is the result of that SQL, with the filters
    /// and search of `view` (its sort is left out). It shows the grouping
    /// as plain SQL; the top-20 and `Other` trimming is left out.
    pub fn sql(&self, view: &ViewSpec, query: Option<&str>) -> String {
        let filters = ViewSpec {
            sort: Vec::new(),
            ..view.clone()
        }
        .clauses(&self.query.columns);
        let conditions = filters
            .strip_prefix(" WHERE ")
            .map(|c| vec![c.to_string()])
            .unwrap_or_default();
        let from = match query {
            Some(sql) => format!("(\n{}\n) AS result", sql.trim().trim_end_matches(';')),
            None => "data".into(),
        };
        self.query.sql(&from, conditions)
    }
}

impl Dataset {
    /// Computes `spec` over `frame`, the rows the grid shows. Blocks; call
    /// it off the UI thread. A newer call cancels an older one still
    /// running, which then returns [`Cancelled`]. Other errors are plain
    /// sentences to show in the plot area, like `Pick a number column for
    /// Y.` or `No rows to chart.`
    pub fn chart(&self, frame: &Frame, spec: &ChartSpec) -> Result<Chart> {
        let conn = self.connection()?;
        let handle = conn.interrupt_handle();
        if let Some(previous) = self.chart_interrupt.lock().unwrap().replace(handle.clone()) {
            previous.interrupt();
        }
        let run = Run {
            conn,
            handle,
            slot: &self.chart_interrupt,
            frame,
            spec,
        };
        let chart = run.chart();
        run.live()?;
        chart
    }
}

/// One [`Dataset::chart`] call: its connection, and the slot that says
/// whether a newer call has replaced it.
struct Run<'a> {
    conn: Connection,
    handle: Arc<InterruptHandle>,
    slot: &'a Mutex<Option<Arc<InterruptHandle>>>,
    frame: &'a Frame,
    spec: &'a ChartSpec,
}

/// Rows of a grouped query, from [`Run::groups`].
struct Groups {
    rows: Vec<Group>,
    /// Distinct keys and colours before any were cut off.
    keys: u64,
    colors: u64,
}

/// One (key, colour) cell of a grouped query.
struct Group {
    /// The key's rank (bars) or value (lines).
    key: f64,
    /// The key as text; `None` for NULL.
    label: Option<String>,
    /// The colour's rank from 1, where `TOP_SERIES + 1` is `Other`; 1 without colour.
    color: usize,
    color_label: Option<String>,
    /// One per Y column, or one count.
    values: Vec<Option<f64>>,
}

/// What [`Run::groups`] groups by.
struct GroupBy<'a> {
    /// SQL for the key, over the frame's columns.
    key: String,
    /// `WHERE …` over the frame, or empty.
    filter: String,
    /// For bars: keep the top keys by value, and say whether the rest are
    /// kept as one `Other` key. `None` keeps every key, by value.
    top: Option<(usize, bool)>,
    color: Option<&'a Column>,
    ys: &'a [&'a Column],
    aggregate: Aggregate,
}

impl Run<'_> {
    /// Fails with [`Cancelled`] once a newer chart has started.
    fn live(&self) -> Result<()> {
        if is_current(self.slot, &self.handle) {
            Ok(())
        } else {
            Err(Cancelled.into())
        }
    }

    fn rows<T>(&self, sql: &str, f: impl FnMut(&Row) -> duckdb::Result<T>) -> Result<Vec<T>> {
        self.live()?;
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], f)?;
        Ok(rows.collect::<duckdb::Result<_>>()?)
    }

    fn row<T>(&self, sql: &str, f: impl FnOnce(&Row) -> duckdb::Result<T>) -> Result<T> {
        self.live()?;
        Ok(self.conn.query_row(sql, [], f)?)
    }

    fn relation(&self) -> &str {
        &self.frame.relation
    }

    fn column(&self, name: &str) -> Result<&Column> {
        self.frame
            .columns
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| anyhow!("There is no column {name}."))
    }

    /// The X column, checked against the kind.
    fn x(&self) -> Result<&Column> {
        let kind = self.spec.kind;
        let ask = match kind {
            ChartKind::Bar => "Pick a column for X.",
            ChartKind::Line => "Pick a date or number column for X.",
            ChartKind::Scatter => "Pick a number column for X.",
            ChartKind::Histogram => "Pick a number or date column.",
        };
        let x = self.column(self.spec.x.as_deref().ok_or_else(|| anyhow!(ask))?)?;
        if !kind.fits_x(x) {
            bail!(ask);
        }
        Ok(x)
    }

    /// The Y columns, all numbers, at most [`MAX_Y`]. A scatter takes the
    /// first and needs one.
    fn ys(&self) -> Result<Vec<&Column>> {
        let names = match self.spec.kind {
            ChartKind::Scatter => &self.spec.y[..self.spec.y.len().min(1)],
            _ => &self.spec.y[..],
        };
        if names.len() > MAX_Y {
            bail!("Pick at most {MAX_Y} columns for Y.");
        }
        let ys = names
            .iter()
            .map(|name| self.column(name))
            .collect::<Result<Vec<_>>>()?;
        if ys.iter().any(|c| c.kind != ColumnKind::Number)
            || (self.spec.kind == ChartKind::Scatter && ys.is_empty())
        {
            bail!("Pick a number column for Y.");
        }
        Ok(ys)
    }

    /// The colour column, unless Y has several columns.
    fn color(&self, ys: &[&Column]) -> Result<Option<&Column>> {
        match &self.spec.color {
            Some(name) if ys.len() <= 1 => {
                let column = self.column(name)?;
                if !self.spec.kind.fits_color(column) {
                    bail!("Pick a text or yes/no column for Color.");
                }
                Ok(Some(column))
            }
            _ => Ok(None),
        }
    }

    fn chart(&self) -> Result<Chart> {
        match self.spec.kind {
            ChartKind::Bar => self.bar(),
            ChartKind::Line => self.line(),
            ChartKind::Scatter => self.scatter(),
            ChartKind::Histogram => self.histogram(),
        }
    }

    fn bar(&self) -> Result<Chart> {
        let x = self.x()?;
        let ys = self.ys()?;
        let color = self.color(&ys)?;
        let aggregate = if ys.is_empty() {
            Aggregate::Count
        } else {
            self.spec.aggregate
        };
        let groups = self.groups(&GroupBy {
            key: quote_ident(&x.name),
            filter: String::new(),
            top: Some((TOP_BARS, aggregate.adds_up())),
            color,
            ys: &ys,
            aggregate,
        })?;

        // Categories by rank, which is by value, largest first.
        let mut ranks: Vec<(usize, Option<String>)> = Vec::new();
        for g in &groups.rows {
            let rank = g.key as usize;
            if !ranks.iter().any(|(r, _)| *r == rank) {
                ranks.push((rank, g.label.clone()));
            }
        }
        ranks.sort_by_key(|(r, _)| *r);
        let other = if ranks.last().is_some_and(|(r, _)| *r > TOP_BARS) {
            ranks.pop()
        } else {
            None
        };
        match self.spec.sort {
            BarSort::ValueDescending => {}
            BarSort::ValueAscending => ranks.reverse(),
            BarSort::Label => ranks.sort_by(|(_, a), (_, b)| compare_labels(x, a, b)),
        }
        ranks.extend(other);
        let categories: Vec<Category> = ranks
            .iter()
            .map(|(rank, label)| category(x, *rank, label.as_deref()))
            .collect();

        let cells: HashMap<(usize, usize), &Group> = groups
            .rows
            .iter()
            .map(|g| ((g.key as usize, g.color), g))
            .collect();
        let series = series_of(&groups, color.is_some(), &ys, |series, value| {
            ranks
                .iter()
                .zip(&categories)
                .enumerate()
                .map(|(i, ((rank, _), category))| Point {
                    x: i as f64,
                    y: cells.get(&(*rank, series)).and_then(|g| g.values[value]),
                    label: category.label.clone(),
                })
                .collect()
        });

        let mut notes = Vec::new();
        if groups.keys > TOP_BARS as u64 {
            let rest = if aggregate.adds_up() {
                "The rest are grouped as Other."
            } else {
                "The rest are left out."
            };
            notes.push(format!(
                "Top {TOP_BARS} of {} values. {rest}",
                thousands(groups.keys)
            ));
        }
        notes.extend(color_note(color, groups.colors, TOP_SERIES));
        Ok(Chart {
            kind: ChartKind::Bar,
            title: self.title(&ys, color, aggregate, None),
            x: XAxis::Categories(categories),
            y: value_axis(
                &series,
                true,
                aggregate == Aggregate::Count,
                y_format(&ys, aggregate),
            ),
            series,
            bin_width: None,
            notes,
            query: self.readable(
                &ys,
                color,
                aggregate,
                Shape::Bar {
                    trimmed: groups.keys > TOP_BARS as u64,
                },
            ),
        })
    }

    fn line(&self) -> Result<Chart> {
        let x = self.x()?;
        let ys = self.ys()?;
        let color = self.color(&ys)?;
        let aggregate = if ys.is_empty() {
            Aggregate::Count
        } else {
            self.spec.aggregate
        };
        let col = quote_ident(&x.name);
        // Each bucket: its key in the query, where it sits on X, its label.
        let (key, filter, buckets, shape, x_scale) = if x.kind == ColumnKind::Temporal {
            let (lo, hi): (Option<f64>, Option<f64>) = self.row(
                &format!(
                    "SELECT CAST(epoch(min(CAST({col} AS TIMESTAMP))) AS DOUBLE),
                            CAST(epoch(max(CAST({col} AS TIMESTAMP))) AS DOUBLE)
                     FROM {}",
                    self.relation()
                ),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let (Some(lo), Some(hi)) = (lo, hi) else {
                bail!(NO_ROWS);
            };
            let (unit, days) = DateUnit::for_line(lo, hi);
            let buckets: Vec<(f64, f64, String)> = days
                .iter()
                .map(|day| {
                    let at = *day as f64 * DAY;
                    (at, at, unit.format(at))
                })
                .collect();
            (
                format!(
                    "CAST(epoch(date_trunc('{}', CAST({col} AS TIMESTAMP))) AS DOUBLE)",
                    unit.name()
                ),
                format!(" WHERE {col} IS NOT NULL"),
                buckets,
                Shape::LineDates(unit),
                Scale::Date(unit),
            )
        } else {
            let e = format!("CAST({col} AS DOUBLE)");
            let (lo, hi, distinct): (Option<f64>, Option<f64>, i64) = self.row(
                &format!(
                    "SELECT min({e}) FILTER (isfinite({e})), max({e}) FILTER (isfinite({e})),
                            approx_count_distinct({e}) FILTER (isfinite({e}))
                     FROM {}",
                    self.relation()
                ),
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let (Some(lo), Some(hi)) = (lo, hi) else {
                bail!(NO_ROWS);
            };
            let filter = format!(" WHERE isfinite({e})");
            let scale = Scale::Number(x.format);
            if distinct as usize <= LINE_POINTS {
                // Few values: one point each. The buckets are the values found.
                (e, filter, Vec::new(), Shape::LineValues, scale)
            } else {
                // Many values: equal-width bins, each point at its bin's middle.
                let (start, width, bins) = number_bins(lo, hi, LINE_POINTS as f64 / 2.0);
                let buckets = (0..bins)
                    .map(|i| {
                        let from = start + i as f64 * width;
                        let label = format!(
                            "{} to {}",
                            scale.label(clean(from, width)),
                            scale.label(clean(from + width, width))
                        );
                        (i as f64, from + width / 2.0, label)
                    })
                    .collect();
                (
                    format!(
                        "least(floor(({e} - {start}) / {width}), {})",
                        bins as f64 - 1.0
                    ),
                    filter,
                    buckets,
                    Shape::LineBins { start, width, bins },
                    scale,
                )
            }
        };
        let groups = self.groups(&GroupBy {
            key,
            filter,
            top: None,
            color,
            ys: &ys,
            aggregate,
        })?;
        let buckets = if buckets.is_empty() {
            let mut values: Vec<f64> = groups.rows.iter().map(|g| g.key).collect();
            values.sort_by(f64::total_cmp);
            values.dedup();
            values
                .into_iter()
                .map(|v| (v, v, x_scale.label(v)))
                .collect()
        } else {
            buckets
        };

        let cells: HashMap<(u64, usize), &Group> = groups
            .rows
            .iter()
            .map(|g| ((g.key.to_bits(), g.color), g))
            .collect();
        // An empty bucket is a gap, or zero when values add up.
        let empty = aggregate.adds_up().then_some(0.0);
        let series = series_of(&groups, color.is_some(), &ys, |series, value| {
            buckets
                .iter()
                .map(|(key, at, label)| Point {
                    x: *at,
                    y: cells
                        .get(&(key.to_bits(), series))
                        .map_or(empty, |g| g.values[value].or(empty)),
                    label: label.clone(),
                })
                .collect()
        });

        let x_axis = match x_scale {
            Scale::Date(_) => {
                let every = buckets.len().div_ceil(DATE_TICKS).max(1);
                Axis {
                    scale: x_scale,
                    min: buckets.first().map_or(0.0, |b| b.1),
                    max: buckets.last().map_or(0.0, |b| b.1),
                    ticks: buckets
                        .iter()
                        .step_by(every)
                        .map(|(_, at, label)| Tick {
                            value: *at,
                            label: label.clone(),
                        })
                        .collect(),
                }
            }
            Scale::Number(format) => {
                let (lo, hi) = match shape {
                    Shape::LineBins { start, width, .. } => {
                        (start, start + width * buckets.len() as f64)
                    }
                    _ => (buckets[0].1, buckets[buckets.len() - 1].1),
                };
                number_axis(lo, hi, format, false, false)
            }
        };
        let unit = match shape {
            Shape::LineDates(unit) => Some(unit),
            _ => None,
        };
        Ok(Chart {
            kind: ChartKind::Line,
            title: self.title(&ys, color, aggregate, unit),
            x: XAxis::Values(x_axis),
            y: value_axis(
                &series,
                false,
                aggregate == Aggregate::Count,
                y_format(&ys, aggregate),
            ),
            series,
            bin_width: None,
            notes: color_note(color, groups.colors, TOP_SERIES)
                .into_iter()
                .collect(),
            query: self.readable(&ys, color, aggregate, shape),
        })
    }

    fn scatter(&self) -> Result<Chart> {
        let x = self.x()?;
        let ys = self.ys()?;
        let y = ys[0];
        let color = self.color(&ys)?;
        let (xc, yc) = (quote_ident(&x.name), quote_ident(&y.name));
        let condition =
            format!("isfinite(CAST({xc} AS DOUBLE)) AND isfinite(CAST({yc} AS DOUBLE))");
        let rows: i64 = self.row(
            &format!("SELECT count(*) FROM {} WHERE {condition}", self.relation()),
            |r| r.get(0),
        )?;
        if rows == 0 {
            bail!(NO_ROWS);
        }
        let sampled = rows as u64 > SAMPLE_POINTS;
        let cc = color.map(|c| quote_ident(&c.name));
        // The sample is the rows with the smallest hashes: the same rows every time.
        let order = if sampled {
            let hashed = [Some(&xc), Some(&yc), cc.as_ref()]
                .into_iter()
                .flatten()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            format!(" ORDER BY hash({hashed}) LIMIT {SAMPLE_POINTS}")
        } else {
            String::new()
        };
        let points: Vec<(f64, f64, Option<String>)> = self.rows(
            &format!(
                "SELECT CAST({xc} AS DOUBLE), CAST({yc} AS DOUBLE), {}
                 FROM {} WHERE {condition}{order}",
                cc.as_ref()
                    .map_or("CAST(NULL AS VARCHAR)".into(), |c| format!(
                        "CAST({c} AS VARCHAR)"
                    )),
                self.relation()
            ),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;

        let x_axis = number_axis(
            points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min),
            points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max),
            x.format,
            true,
            false,
        );
        let point = |p: &(f64, f64, Option<String>)| Point {
            x: p.0,
            y: Some(p.1),
            label: x_axis.label(p.0),
        };
        // Colours by how many sampled points they have; the rest are Other.
        let mut series = Vec::new();
        let mut colors = 0;
        if color.is_some() {
            let mut counts: Vec<(Option<String>, usize)> = Vec::new();
            for p in &points {
                match counts.iter_mut().find(|(c, _)| *c == p.2) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((p.2.clone(), 1)),
                }
            }
            counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            colors = counts.len() as u64;
            let top: Vec<Option<String>> = counts
                .into_iter()
                .take(TOP_SCATTER_SERIES)
                .map(|(c, _)| c)
                .collect();
            for c in &top {
                series.push(Series {
                    name: c.clone().unwrap_or_else(|| EMPTY.into()),
                    other: false,
                    points: points.iter().filter(|p| p.2 == *c).map(point).collect(),
                });
            }
            let rest: Vec<Point> = points
                .iter()
                .filter(|p| !top.contains(&p.2))
                .map(point)
                .collect();
            if !rest.is_empty() {
                series.push(Series {
                    name: OTHER.into(),
                    other: true,
                    points: rest,
                });
            }
        } else {
            series.push(Series {
                name: y.name.clone(),
                other: false,
                points: points.iter().map(point).collect(),
            });
        }

        let mut notes = Vec::new();
        if sampled {
            notes.push(format!(
                "Showing a {}-point sample of {} rows.",
                thousands(SAMPLE_POINTS),
                thousands(rows as u64)
            ));
        }
        notes.extend(color_note(color, colors, TOP_SCATTER_SERIES));
        Ok(Chart {
            kind: ChartKind::Scatter,
            title: capitalized(format!("{} against {}", x.name, y.name)),
            x: XAxis::Values(x_axis),
            y: value_axis(&series, false, false, y.format),
            series,
            bin_width: None,
            notes,
            query: self.readable(&ys, color, Aggregate::Sum, Shape::Scatter { sampled }),
        })
    }

    fn histogram(&self) -> Result<Chart> {
        let x = self.x()?;
        let e = profile::numeric(x).ok_or_else(|| anyhow!("Pick a number or date column."))?;
        let (lo, hi): (Option<f64>, Option<f64>) = self.row(
            &format!(
                "SELECT min({e}) FILTER (isfinite({e})), max({e}) FILTER (isfinite({e})) FROM {}",
                self.relation()
            ),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let (Some(lo), Some(hi)) = (lo, hi) else {
            bail!(NO_ROWS);
        };
        let temporal = x.kind == ColumnKind::Temporal;
        let (start, width, bins) = if temporal {
            // Whole days, so bins start at midnight.
            let days = ((hi - lo) / DAY / HISTOGRAM_BINS).round().max(1.0);
            let start = (lo / DAY).floor() * DAY;
            let width = days * DAY;
            (start, width, ((hi - start) / width).floor() as usize + 1)
        } else {
            number_bins(lo, hi, HISTOGRAM_BINS)
        };
        let end = start + width * bins as f64;
        self.live()?;
        let counts = profile::histogram(
            &self.conn,
            self.relation(),
            &quote_ident(&x.name),
            &e,
            start,
            width,
            bins,
        )?;

        let x_axis = if temporal {
            Axis {
                scale: Scale::Date(DateUnit::Day),
                min: start,
                max: end,
                ticks: date_ticks(start, end),
            }
        } else {
            number_axis(start, end, x.format, false, false)
        };
        let points = counts
            .iter()
            .enumerate()
            .map(|(i, bin)| {
                let from = clean(start + i as f64 * width, width);
                let label = if temporal && width == DAY {
                    x_axis.label(from)
                } else if temporal {
                    format!(
                        "{} to {}",
                        x_axis.label(from),
                        x_axis.label(from + width - DAY)
                    )
                } else {
                    format!(
                        "{} to {}",
                        x_axis.label(from),
                        x_axis.label(clean(from + width, width))
                    )
                };
                Point {
                    x: from,
                    y: Some(bin.count as f64),
                    label,
                }
            })
            .collect();
        let series = vec![Series {
            name: "Rows".into(),
            other: false,
            points,
        }];
        Ok(Chart {
            kind: ChartKind::Histogram,
            title: capitalized(format!("Spread of {}", x.name)),
            x: XAxis::Values(x_axis),
            y: value_axis(&series, true, true, None),
            series,
            bin_width: Some(width),
            notes: Vec::new(),
            query: self.readable(
                &[],
                None,
                Aggregate::Count,
                Shape::Histogram {
                    start,
                    width,
                    bins,
                    temporal,
                },
            ),
        })
    }

    /// One GROUP BY over the frame: by key, and by colour when there is one,
    /// aggregating each Y (or counting rows). Keys and colours are ranked by
    /// value, largest first; the colours past [`TOP_SERIES`] are aggregated
    /// together as one, and so are the keys past `top` when it keeps them.
    fn groups(&self, by: &GroupBy) -> Result<Groups> {
        let mut src = vec![
            format!("{} AS k", by.key),
            by.color.map_or("NULL AS s".into(), |c| {
                format!("{} AS s", quote_ident(&c.name))
            }),
        ];
        src.extend(
            by.ys
                .iter()
                .enumerate()
                .map(|(i, c)| format!("CAST({} AS DOUBLE) AS v{i}", quote_ident(&c.name))),
        );
        let aggregates: Vec<String> = if by.ys.is_empty() {
            vec!["count(*)".into()]
        } else {
            (0..by.ys.len())
                .map(|i| format!("{}(v{i})", by.aggregate.function()))
                .collect()
        };
        let mut with = vec![format!(
            "src AS (SELECT {} FROM {}{})",
            src.join(", "),
            self.relation(),
            by.filter
        )];
        let mut joins = String::new();
        let mut filter = String::new();
        let (key, keys) = match by.top {
            Some((top, keep_rest)) => {
                let rank = aggregates
                    .iter()
                    .map(|a| format!("coalesce({a}, 0)"))
                    .collect::<Vec<_>>()
                    .join(" + ");
                with.push(format!(
                    "kr AS (SELECT k, row_number() OVER (ORDER BY {rank} DESC, k NULLS LAST) AS r,
                            count(*) OVER () AS n FROM src GROUP BY k)"
                ));
                joins.push_str(" JOIN kr ON src.k IS NOT DISTINCT FROM kr.k");
                if keep_rest {
                    (format!("least(kr.r, {})", top + 1), "max(kr.n)")
                } else {
                    filter = format!(" WHERE kr.r <= {top}");
                    ("kr.r".into(), "max(kr.n)")
                }
            }
            None => ("src.k".into(), "0"),
        };
        let (color, colors) = if by.color.is_some() {
            let rank = if by.ys.is_empty() || by.aggregate == Aggregate::Count {
                "count(*)"
            } else {
                "coalesce(sum(v0), 0)"
            };
            with.push(format!(
                "sr AS (SELECT s, row_number() OVER (ORDER BY {rank} DESC, s NULLS LAST) AS r,
                        count(*) OVER () AS n FROM src GROUP BY s)"
            ));
            joins.push_str(" JOIN sr ON src.s IS NOT DISTINCT FROM sr.s");
            (format!("least(sr.r, {})", TOP_SERIES + 1), "max(sr.n)")
        } else {
            ("1".into(), "1")
        };
        let values = aggregates.len();
        let sql = format!(
            "WITH {}
             SELECT CAST({key} AS DOUBLE) AS kx, CAST({color} AS BIGINT) AS sx,
                    any_value(CAST(src.k AS VARCHAR)), any_value(CAST(src.s AS VARCHAR)),
                    CAST({keys} AS BIGINT), CAST({colors} AS BIGINT),
                    {}
             FROM src{joins}{filter}
             GROUP BY kx, sx",
            with.join(",\n"),
            aggregates
                .iter()
                .map(|a| format!("CAST({a} AS DOUBLE)"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        let mut counts = (0, 0);
        let rows = self.rows(&sql, |r| {
            counts = (r.get::<_, i64>(4)? as u64, r.get::<_, i64>(5)? as u64);
            Ok(Group {
                key: r.get(0)?,
                color: r.get::<_, i64>(1)? as usize,
                label: r.get(2)?,
                color_label: r.get(3)?,
                values: (0..values)
                    .map(|i| r.get(6 + i))
                    .collect::<duckdb::Result<_>>()?,
            })
        })?;
        if rows.is_empty() {
            bail!(NO_ROWS);
        }
        Ok(Groups {
            rows,
            keys: counts.0,
            colors: counts.1,
        })
    }

    /// The title, like `Sum of revenue by product` or `Row count per region`.
    fn title(
        &self,
        ys: &[&Column],
        color: Option<&Column>,
        aggregate: Aggregate,
        unit: Option<DateUnit>,
    ) -> String {
        let x = self.spec.x.clone().unwrap_or_default();
        let what = match ys {
            [] => "Row count".to_string(),
            [y] => format!("{} of {}", aggregate.label(), y.name),
            [a, b] => format!("{} of {} and {}", aggregate.label(), a.name, b.name),
            many => format!("{} of {} columns", aggregate.label(), many.len()),
        };
        let mut by = unit.map_or(x, |u| u.name().to_string());
        if let Some(color) = color {
            by = format!("{by} and {}", color.name);
        }
        let joiner = if ys.is_empty() && self.spec.kind == ChartKind::Bar {
            "per"
        } else {
            "by"
        };
        capitalized(format!("{what} {joiner} {by}"))
    }

    /// What [`Chart::sql`] needs to write the query again later.
    fn readable(
        &self,
        ys: &[&Column],
        color: Option<&Column>,
        aggregate: Aggregate,
        shape: Shape,
    ) -> Readable {
        Readable {
            x: self.spec.x.clone().unwrap_or_default(),
            ys: ys.iter().map(|c| c.name.clone()).collect(),
            color: color.map(|c| c.name.clone()),
            aggregate,
            sort: self.spec.sort,
            shape,
            columns: self.frame.columns.clone(),
        }
    }
}

/// The series of a bar or line chart: one per colour (ranked, `Other`
/// last), else one per Y column, else one counting rows. `points(c, v)`
/// makes a series' points from colour rank `c` and value index `v`.
fn series_of(
    groups: &Groups,
    by_color: bool,
    ys: &[&Column],
    points: impl Fn(usize, usize) -> Vec<Point>,
) -> Vec<Series> {
    if by_color {
        let mut colors: Vec<(usize, Option<String>)> = Vec::new();
        for g in &groups.rows {
            if !colors.iter().any(|(c, _)| *c == g.color) {
                colors.push((g.color, g.color_label.clone()));
            }
        }
        colors.sort_by_key(|(c, _)| *c);
        colors
            .into_iter()
            .map(|(rank, label)| {
                let other = rank > TOP_SERIES;
                Series {
                    name: if other {
                        OTHER.into()
                    } else {
                        label.unwrap_or_else(|| EMPTY.into())
                    },
                    other,
                    points: points(rank, 0),
                }
            })
            .collect()
    } else if ys.is_empty() {
        vec![Series {
            name: "Rows".into(),
            other: false,
            points: points(1, 0),
        }]
    } else {
        ys.iter()
            .enumerate()
            .map(|(i, y)| Series {
                name: y.name.clone(),
                other: false,
                points: points(1, i),
            })
            .collect()
    }
}

/// The bar for the key ranked `rank`, labelled `label` (`None` for NULL).
fn category(x: &Column, rank: usize, label: Option<&str>) -> Category {
    if rank > TOP_BARS {
        return Category {
            label: OTHER.into(),
            filter: None,
        };
    }
    let op = match label {
        Some(value) => FilterOp::Equals(value.into()),
        None => FilterOp::IsNull,
    };
    Category {
        label: label.unwrap_or(EMPTY).into(),
        filter: Some(Filter {
            column: x.name.clone(),
            op,
        }),
    }
}

/// Orders bar labels for [`BarSort::Label`]: numbers by value, the rest as
/// text; NULL last.
fn compare_labels(x: &Column, a: &Option<String>, b: &Option<String>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) if x.kind == ColumnKind::Number => {
            let (fa, fb) = (a.parse::<f64>(), b.parse::<f64>());
            match (fa, fb) {
                (Ok(fa), Ok(fb)) => fa.total_cmp(&fb),
                _ => a.cmp(b),
            }
        }
        (Some(a), Some(b)) => a.to_lowercase().cmp(&b.to_lowercase()),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// The footer note when a colour column has more values than series.
fn color_note(color: Option<&Column>, colors: u64, top: usize) -> Option<String> {
    let color = color?;
    (colors > top as u64).then(|| {
        format!(
            "Color shows the top {top} of {} values of {}. The rest are grouped as Other.",
            thousands(colors),
            color.name
        )
    })
}

/// The display format of aggregated Ys: theirs when the aggregate keeps the
/// unit and every Y is the same kind of format (the same currency, or
/// percent), with the most decimals any of them has.
fn y_format(ys: &[&Column], aggregate: Aggregate) -> Option<NumberFormat> {
    use NumberFormat::{Currency, Percent};
    if !aggregate.keeps_unit() {
        return None;
    }
    ys.iter()
        .map(|c| c.format)
        .reduce(|a, b| match (a?, b?) {
            (
                Currency {
                    symbol,
                    decimals: d,
                },
                Currency {
                    symbol: s,
                    decimals: e,
                },
            ) if symbol == s => Some(Currency {
                symbol,
                decimals: d.max(e),
            }),
            (Percent { decimals: d }, Percent { decimals: e }) => {
                Some(Percent { decimals: d.max(e) })
            }
            _ => None,
        })
        .flatten()
}

/// The value axis over every point of `series`, from zero when `from_zero`,
/// with whole-number ticks when `counts`.
fn value_axis(
    series: &[Series],
    from_zero: bool,
    counts: bool,
    format: Option<NumberFormat>,
) -> Axis {
    let values = series.iter().flat_map(|s| &s.points).filter_map(|p| p.y);
    let (mut lo, mut hi) = values.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
        (lo.min(v), hi.max(v))
    });
    if lo > hi {
        (lo, hi) = (0.0, 0.0);
    }
    if from_zero {
        (lo, hi) = (lo.min(0.0), hi.max(0.0));
    }
    number_axis(lo, hi, format, true, counts)
}

/// A number axis over `lo ..= hi` with about five ticks at clean values.
/// `snap` widens the extent out to the nearest ticks (value axes); without
/// it the extent stays `lo ..= hi` and the ticks fall inside. `whole` keeps
/// ticks on whole numbers, for counts.
fn number_axis(lo: f64, hi: f64, format: Option<NumberFormat>, snap: bool, whole: bool) -> Axis {
    let (lo, hi) = if hi > lo {
        (lo, hi)
    } else {
        let pad = if lo == 0.0 { 1.0 } else { lo.abs() / 10.0 };
        (lo - if lo == 0.0 { 0.0 } else { pad }, hi + pad)
    };
    let mut step = nice_step((hi - lo) / 4.0, false);
    if whole && step.fract() != 0.0 {
        // 2.5 becomes 5, anything under 1 becomes 1.
        step = nice_step(step.ceil(), false);
    }
    let (min, max) = if snap {
        (
            clean((lo / step).floor() * step, step),
            clean((hi / step).ceil() * step, step),
        )
    } else {
        (lo, hi)
    };
    let first = (min / step - 1e-9).ceil() as i64;
    let last = (max / step + 1e-9).floor() as i64;
    let big = min.abs().max(max.abs());
    let ticks = (first..=last)
        .map(|i| {
            let value = clean(i as f64 * step, step);
            Tick {
                value,
                label: tick_label(value, step, big, format),
            }
        })
        .collect();
    Axis {
        scale: Scale::Number(format),
        min,
        max,
        ticks,
    }
}

/// Ticks at the starts of days, weeks, months, quarters or years, whichever
/// is the smallest unit giving at most [`DATE_TICKS`] in `lo ..= hi`.
fn date_ticks(lo: f64, hi: f64) -> Vec<Tick> {
    let inside = |unit: DateUnit, limit: usize| {
        let starts = unit.buckets(lo, hi, limit)?;
        Some(
            starts
                .into_iter()
                .map(|day| day as f64 * DAY)
                .filter(|at| *at >= lo)
                .collect::<Vec<_>>(),
        )
    };
    let (unit, starts) = DateUnit::ALL
        .iter()
        .find_map(|unit| Some((*unit, inside(*unit, DATE_TICKS + 1)?)))
        .unwrap_or_else(|| {
            let years = inside(DateUnit::Year, usize::MAX).unwrap_or_default();
            (DateUnit::Year, years)
        });
    let every = starts.len().div_ceil(DATE_TICKS).max(1);
    starts
        .into_iter()
        .step_by(every)
        .map(|at| Tick {
            value: at,
            label: unit.format(at),
        })
        .collect()
}

/// Equal-width bins of a clean width over `lo ..= hi`, about `target` of
/// them: `(start, width, count)`. Whole-number bins always give the largest
/// value a bin of its own, so each whole value of width 1 bins is one bar;
/// fractional bins count a largest value on the last edge in the last bin.
fn number_bins(lo: f64, hi: f64, target: f64) -> (f64, f64, usize) {
    if hi <= lo {
        return (lo.floor(), 1.0, 1);
    }
    let width = nice_step((hi - lo) / target, true);
    let start = clean((lo / width).floor() * width, width);
    let spans = (hi - start) / width;
    let bins = if width.fract() == 0.0 {
        (spans + 1e-6).floor() + 1.0
    } else if (spans - spans.round()).abs() < 1e-6 {
        spans.round()
    } else {
        spans.ceil()
    };
    let bins = bins.max(1.0) as usize;
    (start, width, bins)
}

/// A clean step (1, 2, 2.5 or 5 times a power of ten) at least `raw`, or
/// the nearest one when `nearest`.
fn nice_step(raw: f64, nearest: bool) -> f64 {
    if raw.is_nan() || raw <= 0.0 || raw.is_infinite() {
        return 1.0;
    }
    let magnitude = 10f64.powf(raw.log10().floor());
    let candidates = [1.0, 2.0, 2.5, 5.0, 10.0].map(|c| c * magnitude);
    if nearest {
        candidates
            .into_iter()
            .min_by(|a, b| (a / raw).ln().abs().total_cmp(&(b / raw).ln().abs()))
            .unwrap_or(magnitude)
    } else {
        candidates
            .into_iter()
            .find(|c| *c >= raw * (1.0 - 1e-9))
            .unwrap_or(10.0 * magnitude)
    }
}

/// `value` rounded to the decimals `step` has, so `0.1 * 3` is `0.3`.
fn clean(value: f64, step: f64) -> f64 {
    let scale = 10f64.powi(decimals(step) as i32);
    (value * scale).round() / scale
}

/// How many decimals `step` needs, up to 10.
fn decimals(step: f64) -> usize {
    (0..10)
        .find(|d| {
            let scaled = step.abs() * 10f64.powi(*d as i32);
            (scaled - scaled.round()).abs() < 1e-6 * scaled.max(1.0)
        })
        .unwrap_or(10)
}

/// A tick label: in full when the axis goes up to 10,000 at most (`2,500`,
/// `10,000`), compact past it (`15K`, `1.2M`), in the axis's format
/// (`$15K`, `20%`). `big` is the largest magnitude on the axis.
fn tick_label(value: f64, step: f64, big: f64, format: Option<NumberFormat>) -> String {
    let (times, prefix, suffix) = match format {
        Some(NumberFormat::Currency { symbol, .. }) => (1.0, symbol.to_string(), ""),
        Some(NumberFormat::Percent { .. }) => (100.0, String::new(), "%"),
        None => (1.0, String::new(), ""),
    };
    let (value, step, big) = (value * times, step * times, big * times);
    let (divisor, unit) = [(1e12, "T"), (1e9, "B"), (1e6, "M"), (1e3, "K")]
        .into_iter()
        .find(|(d, _)| big > 10_000.0 && value.abs() >= *d)
        .unwrap_or((1.0, ""));
    let shown = format!(
        "{:.*}",
        decimals(step / divisor).min(6),
        value.abs() / divisor
    );
    let shown = if shown.contains('.') {
        shown
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    } else {
        shown
    };
    let sign = if value < 0.0 && shown.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        "-"
    } else {
        ""
    };
    format!("{sign}{prefix}{}{unit}{suffix}", group_thousands(&shown))
}

/// A plain number in full for reading: grouped, whole numbers as they
/// are, others to two decimals, small ones to about five digits.
fn plain(value: f64) -> String {
    if !value.is_finite() || value.abs() >= 1e15 {
        return value.to_string();
    }
    let magnitude = value.abs();
    let text = if magnitude.fract() == 0.0 {
        format!("{magnitude:.0}")
    } else if magnitude >= 1.0 {
        let fixed = format!("{magnitude:.2}");
        fixed
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    } else {
        tidy_number(&magnitude.to_string())
    };
    let sign = if value < 0.0 && text.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        "-"
    } else {
        ""
    };
    format!("{sign}{}", group_thousands(&text))
}

/// A count with thousands separators: `30,000,000`.
fn thousands(n: u64) -> String {
    group_thousands(&n.to_string())
}

/// `text` with its first letter in upper case.
fn capitalized(text: String) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

/// Days since 1970-01-01 for a calendar date (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The calendar date `(year, month, day)` of a day since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// How a chart's rows were grouped, for writing its query again.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// `trimmed` when there were more categories than bars.
    Bar {
        trimmed: bool,
    },
    LineDates(DateUnit),
    LineValues,
    LineBins {
        start: f64,
        width: f64,
        bins: usize,
    },
    Scatter {
        sampled: bool,
    },
    Histogram {
        start: f64,
        width: f64,
        bins: usize,
        temporal: bool,
    },
}

/// A chart's query in a form that can be written over any source.
#[derive(Clone, Debug)]
struct Readable {
    x: String,
    ys: Vec<String>,
    color: Option<String>,
    aggregate: Aggregate,
    sort: BarSort,
    shape: Shape,
    /// The frame's columns, for writing the grid's filters.
    columns: Arc<[Column]>,
}

impl Readable {
    /// The query over `from`, keeping rows that meet every one of `conditions`.
    fn sql(&self, from: &str, mut conditions: Vec<String>) -> String {
        let x = quote_ident(&self.x);
        let values: Vec<String> = if self.ys.is_empty() {
            vec!["count(*) AS \"rows\"".into()]
        } else {
            self.ys
                .iter()
                .map(|y| {
                    let y = quote_ident(y);
                    format!("{}({y}) AS {y}", self.aggregate.function())
                })
                .collect()
        };
        let color: Vec<String> = self.color.iter().map(|c| quote_ident(c)).collect();
        let grouped = |key: String| [vec![key], color.clone(), values.clone()].concat();
        // Bins as the chart makes them: X as a finite double, past the last bin's end in it.
        let numeric = self
            .columns
            .iter()
            .find(|c| c.name == self.x)
            .and_then(profile::numeric)
            .unwrap_or_else(|| format!("CAST({x} AS DOUBLE)"));
        let finite = format!("isfinite({numeric})");
        let bin = |start: f64, width: f64, bins: usize| {
            format!(
                "{start} + {width} * least(floor(({numeric} - {start}) / {width}), {})",
                bins - 1
            )
        };
        let top_bars = matches!(self.shape, Shape::Bar { trimmed: true }) && color.is_empty();
        let (select, group, order, limit) = match self.shape {
            // Cut to the top bars and ordered by `top_bars` below.
            Shape::Bar { .. } if top_bars => (grouped(x.clone()), true, String::new(), None),
            Shape::Bar { .. } => {
                let first_value = color.len() + 2;
                let order = match self.sort {
                    BarSort::ValueDescending => format!("{first_value} DESC"),
                    BarSort::ValueAscending => format!("{first_value}"),
                    BarSort::Label => "1".into(),
                };
                (grouped(x.clone()), true, order, None)
            }
            Shape::LineDates(unit) => {
                conditions.push(format!("{x} IS NOT NULL"));
                let key = format!(
                    "date_trunc('{}', {x}) AS {}",
                    unit.name(),
                    quote_ident(unit.name())
                );
                (grouped(key), true, "1".into(), None)
            }
            Shape::LineValues => (grouped(x.clone()), true, "1".into(), None),
            Shape::LineBins { start, width, bins } => {
                conditions.push(finite);
                let key = format!("{} AS bin_start", bin(start, width, bins));
                (grouped(key), true, "1".into(), None)
            }
            Shape::Scatter { sampled } => {
                let y = quote_ident(&self.ys[0]);
                conditions.push(format!("{x} IS NOT NULL AND {y} IS NOT NULL"));
                let columns = [vec![x.clone(), y], color.clone()].concat();
                let order = if sampled {
                    format!("hash({})", columns.join(", "))
                } else {
                    String::new()
                };
                (columns, false, order, sampled.then_some(SAMPLE_POINTS))
            }
            Shape::Histogram {
                start,
                width,
                bins,
                temporal,
            } => {
                conditions.push(finite);
                let key = bin(start, width, bins);
                let key = if temporal {
                    format!("to_timestamp({key})")
                } else {
                    key
                };
                (
                    vec![format!("{key} AS bin_start"), "count(*) AS \"rows\"".into()],
                    true,
                    "1".into(),
                    None,
                )
            }
        };
        let mut sql = format!("SELECT {}\nFROM {from}", select.join(", "));
        if !conditions.is_empty() {
            sql.push_str(&format!("\nWHERE {}", conditions.join("\n  AND ")));
        }
        if group {
            sql.push_str("\nGROUP BY ALL");
        }
        if !order.is_empty() {
            sql.push_str(&format!("\nORDER BY {order}"));
        }
        if let Some(limit) = limit {
            sql.push_str(&format!("\nLIMIT {limit}"));
        }
        if top_bars {
            return self.top_bars(&sql);
        }
        sql
    }

    /// `grouped`, one row per category, cut to the bars the chart drew: the
    /// top [`TOP_BARS`] ranked by value as the chart ranks them, in the
    /// chart's order. When values add up, the rest are summed into an
    /// `Other` row, last.
    fn top_bars(&self, grouped: &str) -> String {
        let x = quote_ident(&self.x);
        let values: Vec<String> = if self.ys.is_empty() {
            vec!["\"rows\"".into()]
        } else {
            self.ys.iter().map(|y| quote_ident(y)).collect()
        };
        let rank = values
            .iter()
            .map(|v| format!("coalesce({v}, 0)"))
            .collect::<Vec<_>>()
            .join(" + ");
        let ranked = format!(
            "WITH grouped AS (\n{grouped}\n), ranked AS (\n\
             SELECT *, row_number() OVER (ORDER BY {rank} DESC, {x} NULLS LAST) AS bar_rank\n\
             FROM grouped\n)\n"
        );
        if !self.aggregate.adds_up() {
            let order = match self.sort {
                BarSort::ValueDescending => "bar_rank".to_string(),
                BarSort::ValueAscending => "bar_rank DESC".into(),
                BarSort::Label => x,
            };
            return format!(
                "{ranked}SELECT * EXCLUDE (bar_rank)\nFROM ranked\nWHERE bar_rank <= {TOP_BARS}\nORDER BY {order}"
            );
        }
        let sums = values
            .iter()
            .map(|v| format!("sum({v}) AS {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let order = match self.sort {
            BarSort::ValueDescending => "min(bar_rank)".to_string(),
            BarSort::ValueAscending => format!("min(bar_rank) > {TOP_BARS}, min(bar_rank) DESC"),
            BarSort::Label => format!("min(bar_rank) > {TOP_BARS}, min(ranked.{x})"),
        };
        format!(
            "{ranked}SELECT CASE WHEN bar_rank <= {TOP_BARS} THEN CAST({x} AS VARCHAR) ELSE '{OTHER}' END AS {x}, {sums}\n\
             FROM ranked\nGROUP BY 1\nORDER BY {order}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_round_trips() {
        for (y, m, d) in [(1970, 1, 1), (2026, 2, 28), (2024, 2, 29), (1969, 12, 31)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
        // 2026-03-04 is a Wednesday; its week starts Monday the 2nd.
        let day = days_from_civil(2026, 3, 4);
        assert_eq!(DateUnit::Week.start(day), days_from_civil(2026, 3, 2));
        assert_eq!(
            DateUnit::Quarter.next(days_from_civil(2025, 10, 1)),
            days_from_civil(2026, 1, 1)
        );
        assert_eq!(DateUnit::Quarter.format(day as f64 * DAY), "Q1 2026");
        assert_eq!(DateUnit::Day.format(day as f64 * DAY), "Mar 4, 2026");
    }

    #[test]
    fn ticks_are_clean() {
        let labels = |axis: Axis| axis.ticks.into_iter().map(|t| t.label).collect::<Vec<_>>();
        assert_eq!(
            labels(number_axis(0.0, 7000.0, None, true, false)),
            ["0", "2,000", "4,000", "6,000", "8,000"]
        );
        assert_eq!(
            labels(number_axis(0.0, 8317.87, None, true, false)),
            ["0", "2,500", "5,000", "7,500", "10,000"]
        );
        assert_eq!(
            labels(number_axis(0.0, 12_000.0, None, true, false)),
            ["0", "5K", "10K", "15K"]
        );
        // Counts tick on whole numbers.
        assert_eq!(
            labels(number_axis(0.0, 10.0, None, true, false)),
            ["0", "2.5", "5", "7.5", "10"]
        );
        assert_eq!(
            labels(number_axis(0.0, 10.0, None, true, true)),
            ["0", "5", "10"]
        );
        assert_eq!(
            labels(number_axis(0.0, 2.0, None, true, true)),
            ["0", "1", "2"]
        );
        let dollars = Some(NumberFormat::Currency {
            symbol: '$',
            decimals: 2,
        });
        assert_eq!(
            labels(number_axis(0.0, 1_234_567.0, dollars, true, false)),
            ["$0", "$500K", "$1M", "$1.5M"]
        );
        let percent = Some(NumberFormat::Percent { decimals: 1 });
        assert_eq!(
            labels(number_axis(0.0, 0.31, percent, true, false)),
            ["0%", "10%", "20%", "30%", "40%"]
        );
        assert_eq!(plain(8317.870000001), "8,317.87");
        assert_eq!(plain(-1500.0), "-1,500");
    }

    #[test]
    fn bins_are_clean() {
        assert_eq!(number_bins(1.75, 5.5, 30.0), (1.7, 0.1, 38));
        assert_eq!(number_bins(3.0, 3.0, 30.0), (3.0, 1.0, 1));
        // Whole numbers: 1 to 40 is forty bars, 40 on its own.
        assert_eq!(number_bins(1.0, 40.0, 30.0), (1.0, 1.0, 40));
        assert_eq!(number_bins(0.0, 150.0, 30.0), (0.0, 5.0, 31));
    }
}
