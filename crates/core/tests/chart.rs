//! Charts drawn from the messy fixtures and from small files made here:
//! the starting charts of the chart panel's table, wide files, display
//! formats, the top-20 cap, NULL categories, filtered and SQL frames, odd
//! column names, empty frames and a superseded request.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use da_core::*;

/// An opened, fully loaded file. Removes its cache dir (and the file, when
/// a test wrote it) on drop.
struct Opened {
    dataset: Dataset,
    frame: Frame,
    cache: PathBuf,
    written: Option<PathBuf>,
}

impl Drop for Opened {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cache);
        if let Some(file) = &self.written {
            let _ = std::fs::remove_file(file);
        }
    }
}

impl Opened {
    fn column(&self, name: &str) -> &Column {
        self.frame
            .columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    }

    fn chart(&self, spec: &ChartSpec) -> Chart {
        self.dataset.chart(&self.frame, spec).unwrap()
    }

    fn query(&self, sql: &str) -> Frame {
        let SqlOutcome::Rows(frame) = self.dataset.query(sql).unwrap() else {
            panic!("no rows from {sql}");
        };
        frame
    }

    /// The first value `sql` returns, as a number.
    fn number(&self, sql: &str) -> f64 {
        let frame = self.query(&format!("SELECT CAST(({sql}) AS DOUBLE)"));
        let page = self.dataset.fetch(&frame, 0, 1).unwrap();
        page.rows[0][0].as_deref().unwrap().parse().unwrap()
    }
}

fn messy(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/messy")
        .join(name)
}

fn temp_path(suffix: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "da-chart-{}-{}{suffix}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

fn open_path(path: &Path) -> Opened {
    let cache = temp_path("");
    let dataset = Dataset::open(
        path,
        &OpenOptions {
            cache_dir: cache.clone(),
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let frame = dataset.materialize(&|_| {}).unwrap();
    Opened {
        dataset,
        frame,
        cache,
        written: None,
    }
}

fn open(name: &str) -> Opened {
    open_path(&messy(name))
}

fn open_csv(text: &str) -> Opened {
    let file = temp_path(".csv");
    std::fs::write(&file, text).unwrap();
    let mut opened = open_path(&file);
    opened.written = Some(file);
    opened
}

fn bar(x: &str, y: &[&str], aggregate: Aggregate) -> ChartSpec {
    ChartSpec {
        kind: ChartKind::Bar,
        x: Some(x.into()),
        y: y.iter().map(|y| y.to_string()).collect(),
        aggregate,
        ..ChartSpec::default()
    }
}

fn categories(chart: &Chart) -> Vec<Category> {
    match &chart.x {
        XAxis::Categories(categories) => categories.clone(),
        XAxis::Values(_) => panic!("not a bar chart"),
    }
}

fn values(series: &Series) -> Vec<f64> {
    series.points.iter().map(|p| p.y.unwrap_or(0.0)).collect()
}

fn total(chart: &Chart) -> f64 {
    chart.series.iter().flat_map(values).sum()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6 * b.abs().max(1.0)
}

/// The five rows of the starting-chart table, on the clean sales file.
#[test]
fn starting_charts_on_clean_sales() {
    let o = open("00_clean_sales.csv");
    let col = |name| o.column(name);

    // Text and number: sum of revenue by product, Granola first.
    let spec = ChartSpec::for_columns(col("product"), Some(col("revenue")));
    assert_eq!(spec, bar("product", &["revenue"], Aggregate::Sum));
    assert_eq!(ChartSpec::suggest(&o.frame.columns, Some("product")), spec);
    let chart = o.chart(&spec);
    assert_eq!(chart.title, "Sum of revenue by product");
    let first = &categories(&chart)[0];
    assert_eq!(first.label, "Granola 500g");
    assert_eq!(
        first.filter,
        Some(Filter {
            column: "product".into(),
            op: FilterOp::Equals("Granola 500g".into()),
        })
    );
    assert_eq!(chart.series.len(), 1);
    assert!(close(chart.series[0].points[0].y.unwrap(), 8317.87));
    assert_eq!(chart.y.label(8317.87), "8,317.87");
    let values = values(&chart.series[0]);
    assert!(values.windows(2).all(|w| w[0] >= w[1]), "{values:?}");
    assert!(close(
        total(&chart),
        o.number("SELECT sum(revenue) FROM data")
    ));
    assert!(chart.notes.is_empty());

    // Date and number: a line of revenue by month, every month present.
    let spec = ChartSpec::for_columns(col("order_date"), Some(col("revenue")));
    assert_eq!(spec.kind, ChartKind::Line);
    let chart = o.chart(&spec);
    assert_eq!(chart.title, "Sum of revenue by month");
    let XAxis::Values(axis) = &chart.x else {
        panic!("a line has a value axis")
    };
    assert_eq!(axis.scale, Scale::Date(DateUnit::Month));
    let labels: Vec<&str> = chart.series[0]
        .points
        .iter()
        .map(|p| p.label.as_str())
        .collect();
    assert_eq!(
        labels,
        [
            "Jan 2026", "Feb 2026", "Mar 2026", "Apr 2026", "May 2026", "Jun 2026"
        ]
    );
    assert!(close(
        total(&chart),
        o.number("SELECT sum(revenue) FROM data")
    ));
    let march = o.number(
        "SELECT sum(revenue) FROM data WHERE order_date >= DATE '2026-03-01' AND order_date < DATE '2026-04-01'",
    );
    assert!(close(chart.series[0].points[2].y.unwrap(), march));

    // Number and number: units against revenue, every row, no sample.
    let spec = ChartSpec::for_columns(col("units"), Some(col("revenue")));
    assert_eq!(spec.kind, ChartKind::Scatter);
    let chart = o.chart(&spec);
    assert_eq!(chart.title, "Units against revenue");
    assert_eq!(chart.series[0].points.len(), 500);
    assert!(chart.notes.is_empty());

    // Number alone: the spread of unit_price, every row in a bin.
    let spec = ChartSpec::for_columns(col("unit_price"), None);
    assert_eq!(spec.kind, ChartKind::Histogram);
    assert_eq!(
        ChartSpec::suggest(&o.frame.columns, Some("unit_price")),
        spec
    );
    let chart = o.chart(&spec);
    assert_eq!(chart.title, "Spread of unit_price");
    assert_eq!(total(&chart), 500.0);
    let bins = chart.series[0].points.len();
    assert!((15..=45).contains(&bins), "{bins} bins");
    let width = chart.bin_width.unwrap();
    let XAxis::Values(axis) = &chart.x else {
        panic!("a histogram has a value axis")
    };
    assert!(close(axis.max - axis.min, width * bins as f64));

    // Text alone: rows per region.
    let spec = ChartSpec::for_columns(col("region"), None);
    let chart = o.chart(&spec);
    assert_eq!(chart.title, "Row count per region");
    assert_eq!(total(&chart), 500.0);
    assert_eq!(chart.y.scale, Scale::Number(None));
}

/// Show SQL runs for every kind of chart, over the file and with a filter.
#[test]
fn show_sql_runs() {
    let o = open("00_clean_sales.csv");
    let view = ViewSpec {
        search: "north".into(),
        sort: vec![SortKey {
            column: "units".into(),
            descending: true,
        }],
        ..ViewSpec::default()
    };
    let frame = o.dataset.apply(&o.frame, &view).unwrap();
    for (x, y) in [
        ("product", Some("revenue")),
        ("order_date", Some("revenue")),
        ("units", Some("revenue")),
        ("unit_price", None),
        ("order_date", None),
    ] {
        let spec = ChartSpec::for_columns(o.column(x), y.map(|y| o.column(y)));
        let spec = if y.is_none() && x == "order_date" {
            spec.with_kind(ChartKind::Histogram, &o.frame.columns)
        } else {
            spec
        };
        let chart = o.dataset.chart(&frame, &spec).unwrap();
        let sql = chart.sql(&view, None);
        assert!(sql.contains("contains(lower("), "{sql}");
        assert!(!sql.contains("ORDER BY \"units\""), "{sql}");
        o.query(&sql);
    }
}

/// Several Y columns give one series per column, with no reshaping.
#[test]
fn wide_months_give_one_series_per_column() {
    let o = open("07_wide_months.csv");
    let months = [
        "Jan 2026", "Feb 2026", "Mar 2026", "Apr 2026", "May 2026", "Jun 2026",
    ];
    let mut spec = bar("Region", &months, Aggregate::Sum);
    // Colour is off while Y has several columns.
    spec.color = Some("Product".into());
    let chart = o.chart(&spec);
    let names: Vec<&str> = chart.series.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, months);
    assert_eq!(chart.title, "Sum of 6 columns by Region");
    let regions = categories(&chart);
    let north = regions.iter().position(|c| c.label == "North").unwrap();
    let want = o.number(r#"SELECT sum("Jan 2026") FROM data WHERE "Region" = 'North'"#);
    assert!(close(chart.series[0].points[north].y.unwrap(), want));
    for series in &chart.series {
        assert_eq!(series.points.len(), regions.len());
    }
}

/// Sum and average keep a currency or percent column's format; count doesn't.
#[test]
fn display_formats_carry_into_the_axis() {
    let o = open("03_currency_percent.csv");
    let chart = o.chart(&bar("region", &["revenue"], Aggregate::Sum));
    assert!(matches!(
        chart.y.scale,
        Scale::Number(Some(NumberFormat::Currency { symbol: '$', .. }))
    ));
    assert!(chart.y.ticks.iter().all(|t| t.label.starts_with('$')));
    assert!(chart.y.label(1234.5).starts_with("$1,234.50"));

    let chart = o.chart(&bar("region", &["margin_pct"], Aggregate::Average));
    assert!(matches!(
        chart.y.scale,
        Scale::Number(Some(NumberFormat::Percent { .. }))
    ));
    assert!(chart.y.ticks.iter().all(|t| t.label.ends_with('%')));

    let chart = o.chart(&bar("region", &["revenue"], Aggregate::Count));
    assert_eq!(chart.y.scale, Scale::Number(None));
    // The measure the panel starts with is the first currency column.
    let spec = ChartSpec::suggest(&o.frame.columns, Some("region"));
    assert_eq!(spec.y, ["revenue"]);
}

/// Thirty categories: the top 20, the rest summed as Other or left out.
#[test]
fn bars_stop_at_twenty() {
    let mut csv = String::from("cat,v\n");
    for i in 1..=30 {
        csv.push_str(&format!("c{i:02},{i}\nc{i:02},{i}\n"));
    }
    let o = open_csv(&csv);
    let chart = o.chart(&bar("cat", &["v"], Aggregate::Sum));
    let cats = categories(&chart);
    assert_eq!(cats.len(), 21);
    assert_eq!(cats[0].label, "c30");
    assert_eq!(cats[20].label, "Other");
    assert_eq!(cats[20].filter, None);
    // c01..c10 are the rest: 2 × (1 + … + 10).
    assert_eq!(chart.series[0].points[20].y, Some(110.0));
    assert_eq!(
        chart.notes,
        ["Top 20 of 30 values. The rest are grouped as Other."]
    );

    let chart = o.chart(&bar("cat", &["v"], Aggregate::Average));
    assert_eq!(categories(&chart).len(), 20);
    assert_eq!(chart.notes, ["Top 20 of 30 values. The rest are left out."]);

    let mut spec = bar("cat", &["v"], Aggregate::Sum);
    spec.sort = BarSort::Label;
    let cats = categories(&o.chart(&spec));
    assert_eq!(cats[0].label, "c11");
    assert_eq!(cats[20].label, "Other");
}

/// A NULL category is its own bar, and clicking it filters for empty.
#[test]
fn null_category_is_a_bar() {
    let o = open_csv("region,v\nEast,1\n,2\n,3\nWest,4\n");
    let chart = o.chart(&bar("region", &["v"], Aggregate::Sum));
    let cats = categories(&chart);
    let empty = cats.iter().position(|c| c.label == "(empty)").unwrap();
    assert_eq!(
        cats[empty].filter.as_ref().map(|f| &f.op),
        Some(&FilterOp::IsNull)
    );
    assert_eq!(chart.series[0].points[empty].y, Some(5.0));
}

/// The chart follows a filtered frame, and Show SQL carries the filter.
#[test]
fn filtered_frame() {
    let o = open("00_clean_sales.csv");
    let view = ViewSpec {
        filters: vec![Filter {
            column: "region".into(),
            op: FilterOp::Equals("East".into()),
        }],
        ..ViewSpec::default()
    };
    let frame = o.dataset.apply(&o.frame, &view).unwrap();
    let spec = bar("product", &["revenue"], Aggregate::Sum);
    let chart = o.dataset.chart(&frame, &spec).unwrap();
    let east = o.number("SELECT sum(revenue) FROM data WHERE region = 'East'");
    assert!(close(total(&chart), east));

    let sql = chart.sql(&view, None);
    assert_eq!(
        sql,
        "SELECT \"product\", sum(\"revenue\") AS \"revenue\"\nFROM data\nWHERE \"region\" = 'East'\nGROUP BY ALL\nORDER BY 2 DESC"
    );
    // The SQL runs and gives the same bars.
    let shown = o.query(&sql);
    assert_eq!(shown.rows, Some(categories(&chart).len() as u64));
}

/// A SQL result charts like the file, and Show SQL wraps the query.
#[test]
fn sql_result_frame() {
    let o = open("00_clean_sales.csv");
    let query = "SELECT region, sum(units) AS total_units FROM data GROUP BY region";
    let frame = o.query(query);
    let spec = ChartSpec::suggest(&frame.columns, None);
    assert_eq!(spec, bar("region", &["total_units"], Aggregate::Sum));
    let chart = o.dataset.chart(&frame, &spec).unwrap();
    assert!(close(
        total(&chart),
        o.number("SELECT sum(units) FROM data")
    ));
    let sql = chart.sql(&ViewSpec::default(), Some(query));
    assert!(
        sql.contains(&format!("FROM (\n{query}\n) AS result")),
        "{sql}"
    );
    o.query(&sql);
}

/// Column names with quotes and spaces are quoted everywhere.
#[test]
fn odd_column_names() {
    let o = open_csv("\"the \"\"big\"\" one\",\"it's value\"\na,1\nb,2\na,3\n");
    let spec = bar("the \"big\" one", &["it's value"], Aggregate::Sum);
    let chart = o.chart(&spec);
    assert_eq!(categories(&chart)[0].label, "a");
    assert_eq!(chart.series[0].points[0].y, Some(4.0));
    let sql = chart.sql(&ViewSpec::default(), None);
    assert!(sql.contains(r#""the ""big"" one""#), "{sql}");
    o.query(&sql);

    let scatter = ChartSpec {
        kind: ChartKind::Scatter,
        x: Some("it's value".into()),
        y: vec!["it's value".into()],
        color: Some("the \"big\" one".into()),
        ..ChartSpec::default()
    };
    let chart = o.chart(&scatter);
    assert_eq!(chart.series.len(), 2);
}

/// No rows, and fields that don't fit, are plain sentences.
#[test]
fn empty_frame_and_bad_fields() {
    let o = open("00_clean_sales.csv");
    let view = ViewSpec {
        filters: vec![Filter {
            column: "region".into(),
            op: FilterOp::Equals("Atlantis".into()),
        }],
        ..ViewSpec::default()
    };
    let empty = o.dataset.apply(&o.frame, &view).unwrap();
    for kind in ChartKind::ALL {
        let spec =
            ChartSpec::suggest(&o.frame.columns, Some("product")).with_kind(kind, &o.frame.columns);
        let err = o.dataset.chart(&empty, &spec).unwrap_err();
        assert_eq!(err.to_string(), "No rows to chart.", "{kind:?}");
    }
    let err = o
        .dataset
        .chart(&o.frame, &bar("product", &["region"], Aggregate::Sum))
        .unwrap_err();
    assert_eq!(err.to_string(), "Pick a number column for Y.");
}

/// Switching kinds keeps what fits and fills in what doesn't.
#[test]
fn switching_kinds() {
    let o = open("00_clean_sales.csv");
    let columns = &o.frame.columns;
    let spec = ChartSpec::suggest(columns, Some("product"));
    let scatter = spec.with_kind(ChartKind::Scatter, columns);
    assert_eq!(scatter.x.as_deref(), Some("units"));
    assert_eq!(scatter.y, ["revenue"]);
    // A line prefers a date, and a bar doesn't keep one.
    let line = spec.with_kind(ChartKind::Line, columns);
    assert_eq!(line.x.as_deref(), Some("order_date"));
    let back = line.with_kind(ChartKind::Bar, columns);
    assert_eq!(back.x.as_deref(), Some("region"));
    assert_eq!(back.y, spec.y);
}

/// Y takes at most six columns, one colour each.
#[test]
fn at_most_six_ys() {
    let o = open_csv("k,a,b,c,d,e,f,g\nx,1,2,3,4,5,6,7\n");
    let six = ["a", "b", "c", "d", "e", "f"];
    assert_eq!(o.chart(&bar("k", &six, Aggregate::Sum)).series.len(), 6);
    let seven = [&six[..], &["g"]].concat();
    let err = o
        .dataset
        .chart(&o.frame, &bar("k", &seven, Aggregate::Sum))
        .unwrap_err();
    assert_eq!(err.to_string(), "Pick at most 6 columns for Y.");
}

/// Two currency columns that differ only in decimals keep the currency.
#[test]
fn currency_ys_keep_the_symbol() {
    let o = open("03_currency_percent.csv");
    let chart = o.chart(&bar("region", &["revenue", "cost"], Aggregate::Sum));
    assert_eq!(
        chart.y.scale,
        Scale::Number(Some(NumberFormat::Currency {
            symbol: '$',
            decimals: 2
        }))
    );
    let chart = o.chart(&bar("region", &["revenue", "margin_pct"], Aggregate::Sum));
    assert_eq!(chart.y.scale, Scale::Number(None));
}

/// Counts tick on whole numbers, even when they are small.
#[test]
fn small_counts_tick_on_whole_numbers() {
    let mut csv = String::from("region\n");
    for (region, n) in [("East", 10), ("West", 8), ("North", 7), ("South", 5)] {
        csv.push_str(&format!("{region}\n").repeat(n));
    }
    let o = open_csv(&csv);
    let chart = o.chart(&bar("region", &[], Aggregate::Sum));
    assert!(
        chart.y.ticks.iter().all(|t| t.value.fract() == 0.0),
        "{:?}",
        chart.y.ticks
    );
}

/// Rows of a Show SQL result, as text.
fn shown(o: &Opened, sql: &str) -> Vec<Vec<Option<String>>> {
    let frame = o.query(sql);
    o.dataset
        .fetch(&frame, 0, frame.rows.unwrap())
        .unwrap()
        .rows
}

/// A histogram's non-empty bins as (start, rows).
type Bins = Vec<(f64, f64)>;

/// A histogram's non-empty bins, from the chart and from Show SQL.
fn histogram_bins(o: &Opened, column: &str) -> (Bins, Bins) {
    let spec = ChartSpec {
        kind: ChartKind::Histogram,
        x: Some(column.into()),
        ..ChartSpec::default()
    };
    let chart = o.chart(&spec);
    let drawn = chart.series[0]
        .points
        .iter()
        .filter(|p| p.y != Some(0.0))
        .map(|p| (p.x, p.y.unwrap()))
        .collect();
    let number = |cell: &Option<String>| cell.as_deref().unwrap().parse::<f64>().unwrap();
    let sql = shown(o, &chart.sql(&ViewSpec::default(), None))
        .iter()
        .map(|row| (number(&row[0]), number(&row[1])))
        .collect();
    (drawn, sql)
}

/// Whole numbers get a bar each, the largest too, and Show SQL agrees.
#[test]
fn histogram_bins_match_show_sql() {
    let o = open("00_clean_sales.csv");
    let (drawn, sql) = histogram_bins(&o, "units");
    let rows = |v: f64| o.number(&format!("SELECT count(*) FROM data WHERE units = {v}"));
    let max = o.number("SELECT max(units) FROM data");
    let last = drawn.last().unwrap();
    assert_eq!(*last, (max, rows(max)));
    assert_eq!(drawn[drawn.len() - 2], (max - 1.0, rows(max - 1.0)));
    assert_eq!(drawn, sql);

    // Decimals, and whole numbers 0 to 10 in quarter bins.
    let (drawn, sql) = histogram_bins(&o, "unit_price");
    assert_eq!(drawn.len(), sql.len());
    assert!(
        drawn
            .iter()
            .zip(&sql)
            .all(|(a, b)| close(a.0, b.0) && a.1 == b.1)
    );
    let tens = open_csv(&format!(
        "v\n{}\n",
        (0..=10)
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    ));
    let (drawn, sql) = histogram_bins(&tens, "v");
    assert_eq!(drawn.len(), 11);
    assert_eq!(drawn, sql);
}

/// With 25 categories sorted low to high, Show SQL gives the chart's bars:
/// the top 20 by value in display order, then Other.
#[test]
fn top_bars_match_show_sql() {
    let csv: String = std::iter::once("cat,v".to_string())
        .chain((1..=25).map(|i| format!("c{i},{i}")))
        .collect::<Vec<_>>()
        .join("\n");
    let o = open_csv(&csv);
    for sort in BarSort::ALL {
        for (aggregate, y) in [
            (Aggregate::Sum, &["v"][..]),
            (Aggregate::Count, &[][..]),
            (Aggregate::Max, &["v"][..]),
        ] {
            let mut spec = bar("cat", y, aggregate);
            spec.sort = sort;
            let chart = o.chart(&spec);
            let drawn: Vec<(String, f64)> = categories(&chart)
                .into_iter()
                .zip(values(&chart.series[0]))
                .map(|(c, v)| (c.label, v))
                .collect();
            let sql: Vec<(String, f64)> = shown(&o, &chart.sql(&ViewSpec::default(), None))
                .into_iter()
                .map(|row| {
                    (
                        row[0].clone().unwrap(),
                        row[1].as_deref().unwrap().parse().unwrap(),
                    )
                })
                .collect();
            assert_eq!(drawn, sql, "{sort:?} {aggregate:?}");
            if sort == BarSort::ValueAscending && aggregate == Aggregate::Sum {
                assert_eq!(drawn[0], ("c6".into(), 6.0));
                assert_eq!(drawn[20], ("Other".into(), 15.0));
            }
        }
    }
}

/// Colour splits bars and lines into series, largest first.
#[test]
fn color_splits_series() {
    let o = open("00_clean_sales.csv");
    let mut spec = bar("product", &["revenue"], Aggregate::Sum);
    spec.color = Some("region".into());
    let chart = o.chart(&spec);
    assert_eq!(chart.title, "Sum of revenue by product and region");
    let regions = o.number("SELECT count(DISTINCT region) FROM data") as usize;
    assert_eq!(chart.series.len(), regions);
    let totals: Vec<f64> = chart
        .series
        .iter()
        .map(|s| values(s).iter().sum())
        .collect();
    assert!(totals.windows(2).all(|w| w[0] >= w[1]), "{totals:?}");
    assert!(close(
        total(&chart),
        o.number("SELECT sum(revenue) FROM data")
    ));

    spec.kind = ChartKind::Line;
    spec.x = Some("order_date".into());
    let chart = o.chart(&spec);
    assert_eq!(chart.series.len(), regions);
    assert!(chart.series.iter().all(|s| s.points.len() == 6));
}

/// Empty date buckets are zero for sums and gaps for averages.
#[test]
fn line_gaps() {
    let o = open_csv("day,v\n2026-01-10,4\n2026-01-20,6\n2026-03-05,1\n");
    let mut spec = ChartSpec {
        kind: ChartKind::Line,
        x: Some("day".into()),
        y: vec!["v".into()],
        ..ChartSpec::default()
    };
    let chart = o.chart(&spec);
    let XAxis::Values(axis) = &chart.x else {
        panic!("a line has a value axis")
    };
    // Mid-January to early March is nine weeks, six of them empty.
    assert_eq!(axis.scale, Scale::Date(DateUnit::Week));
    let zero = chart.series[0].points.iter().filter(|p| p.y == Some(0.0));
    assert_eq!(zero.count(), 6);

    spec.aggregate = Aggregate::Average;
    let chart = o.chart(&spec);
    let gaps = chart.series[0].points.iter().filter(|p| p.y.is_none());
    assert_eq!(gaps.count(), 6);
}

/// A big scatter is a repeatable 5,000-point sample, and says so.
#[test]
fn scatter_samples() {
    let o = open("00_clean_sales.csv");
    let frame = o.query("SELECT range AS a, range % 97 AS b FROM range(20000)");
    let spec = ChartSpec {
        kind: ChartKind::Scatter,
        x: Some("a".into()),
        y: vec!["b".into()],
        ..ChartSpec::default()
    };
    let first = o.dataset.chart(&frame, &spec).unwrap();
    assert_eq!(first.series[0].points.len(), 5000);
    assert_eq!(
        first.notes,
        ["Showing a 5,000-point sample of 20,000 rows."]
    );
    let again = o.dataset.chart(&frame, &spec).unwrap();
    assert_eq!(first.series, again.series);
}

/// A newer chart request cancels an older one still running.
#[test]
fn newer_chart_cancels_older() {
    let o = open("00_clean_sales.csv");
    let frame = o.query("SELECT range AS a, range % 1000 AS g FROM range(4000000)");
    let slow = ChartSpec {
        kind: ChartKind::Bar,
        x: Some("a".into()),
        y: vec!["g".into()],
        aggregate: Aggregate::Median,
        ..ChartSpec::default()
    };
    let quick = bar("g", &[], Aggregate::Count);
    std::thread::scope(|scope| {
        let older = scope.spawn(|| o.dataset.chart(&frame, &slow));
        std::thread::sleep(std::time::Duration::from_millis(100));
        let newer = o.dataset.chart(&frame, &quick);
        assert!(newer.is_ok(), "{:?}", newer.err());
        let err = older.join().unwrap().unwrap_err();
        assert!(err.is::<Cancelled>(), "{err}");
    });
}

/// Times for the four chart kinds on a generated 1M-row CSV. Run with
/// `cargo test --release -p da-core --test chart -- --ignored --nocapture`.
#[test]
#[ignore]
fn performance_on_a_million_rows() {
    let mut csv = String::from("day,region,product,units,price,revenue\n");
    for i in 0..1_000_000u64 {
        let day = 1 + (i * 7919) % 730;
        let units = 1 + i % 37;
        let price = 1.0 + (i % 13) as f64 * 0.75;
        csv.push_str(&format!(
            "{},R{},P{:03},{units},{price},{:.2}\n",
            civil(day),
            i % 7,
            (i * 31) % 143,
            units as f64 * price
        ));
    }
    let start = std::time::Instant::now();
    let o = open_csv(&csv);
    println!("open + load: {:?}", start.elapsed());
    let specs = [
        (
            "bar, sum by text",
            bar("product", &["revenue"], Aggregate::Sum),
        ),
        (
            "line, monthly",
            ChartSpec {
                kind: ChartKind::Line,
                x: Some("day".into()),
                y: vec!["revenue".into()],
                ..ChartSpec::default()
            },
        ),
        (
            "histogram",
            ChartSpec {
                kind: ChartKind::Histogram,
                x: Some("revenue".into()),
                ..ChartSpec::default()
            },
        ),
        (
            "scatter sample",
            ChartSpec {
                kind: ChartKind::Scatter,
                x: Some("units".into()),
                y: vec!["revenue".into()],
                ..ChartSpec::default()
            },
        ),
    ];
    for (name, spec) in specs {
        let chart = o.chart(&spec);
        println!("{name}: {} points", chart.series[0].points.len());
        let start = std::time::Instant::now();
        let runs = 5;
        for _ in 0..runs {
            o.chart(&spec);
        }
        println!("{name}: {:?}", start.elapsed() / runs);
    }
}

/// `2024-01-01` plus `day - 1` days, as `YYYY-MM-DD`.
fn civil(day: u64) -> String {
    let months = [
        31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30,
        31,
    ];
    let mut left = day - 1;
    for (i, len) in months.iter().enumerate() {
        if left < *len {
            return format!("{}-{:02}-{:02}", 2024 + i / 12, i % 12 + 1, left + 1);
        }
        left -= len;
    }
    unreachable!()
}
