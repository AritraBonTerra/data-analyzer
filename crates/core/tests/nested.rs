//! Nested data against the fixtures: JSON records unwrapped from the object
//! holding them, struct columns opened into dotted columns for JSON and
//! Parquet, list cells showing their count, and plain JSON and Parquet files
//! reading exactly as before. Each fixture test is a row of the acceptance
//! table for "totals and nested data".

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use da_core::*;

/// An opened, fully loaded file. Removes its cache dir and any files a test
/// wrote on drop.
struct Opened {
    dataset: Dataset,
    frame: Frame,
    cache: PathBuf,
    written: Vec<PathBuf>,
}

impl Drop for Opened {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cache);
        for file in &self.written {
            let _ = std::fs::remove_file(file);
        }
    }
}

impl Opened {
    /// Column names and DuckDB types, in order.
    fn columns(&self) -> Vec<(String, String)> {
        self.frame
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.sql_type.clone()))
            .collect()
    }

    fn names(&self) -> Vec<String> {
        self.frame.columns.iter().map(|c| c.name.clone()).collect()
    }

    /// Every row of `frame`, as the grid shows it.
    fn rows(&self, frame: &Frame) -> Vec<Vec<Option<String>>> {
        self.dataset.fetch(frame, 0, 10_000).unwrap().rows
    }

    /// Every cell of `column` in `frame`, as the grid shows it.
    fn cells(&self, frame: &Frame, column: &str) -> Vec<Option<String>> {
        let i = frame
            .columns
            .iter()
            .position(|c| c.name == column)
            .unwrap_or_else(|| panic!("no column {column}"));
        self.rows(frame)
            .into_iter()
            .map(|row| row[i].clone())
            .collect()
    }

    fn notes(&self) -> Vec<String> {
        self.dataset
            .notes()
            .iter()
            .map(|n| n.text.clone())
            .collect()
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
        "da-nested-{}-{}{suffix}",
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
    .unwrap_or_else(|e| panic!("open {}: {e:#}", path.display()));
    let frame = dataset.materialize(&|_| {}).unwrap();
    Opened {
        dataset,
        frame,
        cache,
        written: Vec::new(),
    }
}

/// Writes `text` to a file named with `suffix` and opens it.
fn open_text(suffix: &str, text: &str) -> Opened {
    let path = temp_path(suffix);
    std::fs::write(&path, text).unwrap();
    let mut opened = open_path(&path);
    opened.written.push(path);
    opened
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// The eight columns of the records in `30` and `31`.
const RECORD_COLUMNS: [&str; 8] = [
    "id",
    "date",
    "customer.name",
    "customer.tier",
    "customer.address.city",
    "customer.address.zip",
    "items",
    "total",
];

/// What `30` and `31` share: 40 records, dotted columns, typed values and
/// list cells that lead with their count.
fn assert_records(opened: &Opened) {
    assert_eq!(opened.frame.rows, Some(40));
    assert_eq!(opened.names(), strings(&RECORD_COLUMNS));
    let types = opened.columns();
    assert_eq!(types[1].1, "DATE");
    assert_eq!(types[5].1, "VARCHAR");
    assert_eq!(types[7].1, "DOUBLE");
    let zips = opened.cells(&opened.frame, "customer.address.zip");
    assert_eq!(zips[0].as_deref(), Some("02134"));
    let items = opened.cells(&opened.frame, "items");
    assert_eq!(
        items[0].as_deref(),
        Some("2 items · [{'sku': SKU-3, 'qty': 3}, {'sku': SKU-1, 'qty': 2}]")
    );
}

#[test]
fn api_envelope_reads_its_records() {
    let opened = open_path(&messy("30_api_envelope.json"));
    assert_records(&opened);
    assert_eq!(opened.notes(), ["status: ok", "page: 1", "total_pages: 4"]);
    assert_eq!(
        opened.dataset.summary().as_deref(),
        Some("40 records under data. Nested fields opened into columns.")
    );
}

#[test]
fn nested_jsonl_opens_its_fields() {
    let opened = open_path(&messy("31_nested.jsonl"));
    assert_records(&opened);
    assert!(opened.notes().is_empty());
    assert_eq!(
        opened.dataset.summary().as_deref(),
        Some("Nested fields opened into columns.")
    );
}

#[test]
fn columns_and_rows_make_a_table() {
    let opened = open_path(&messy("32_columns_and_rows.json"));
    assert_eq!(opened.frame.rows, Some(40));
    assert_eq!(
        opened.columns(),
        [
            ("order_id".to_string(), "BIGINT".to_string()),
            ("region".into(), "VARCHAR".into()),
            ("units".into(), "BIGINT".into()),
            ("revenue".into(), "DOUBLE".into()),
        ]
    );
    assert_eq!(
        opened.rows(&opened.frame)[0],
        [
            Some("1001".to_string()),
            Some("North".into()),
            Some("10".into()),
            Some("17.5".into())
        ]
    );
    assert!(
        opened
            .dataset
            .summary()
            .is_some_and(|s| s.starts_with("40 rows under rows, named by columns."))
    );
}

#[test]
fn nested_parquet_opens_structs_and_counts_lists() {
    let opened = open_path(&messy("40_nested.parquet"));
    assert_eq!(opened.frame.rows, Some(200));
    assert_eq!(
        opened.columns(),
        [
            ("id".to_string(), "BIGINT".to_string()),
            ("customer.name".into(), "VARCHAR".into()),
            ("customer.tier".into(), "VARCHAR".into()),
            ("scores".into(), "BIGINT[]".into()),
            ("amount".into(), "DECIMAL(18,3)".into()),
            ("seen_at".into(), "TIMESTAMP WITH TIME ZONE".into()),
            ("attrs".into(), "MAP(VARCHAR, BIGINT)".into()),
        ]
    );
    assert_eq!(
        opened.rows(&opened.frame)[1],
        [
            Some("1".to_string()),
            Some("Customer 1".into()),
            Some("silver".into()),
            Some("3 items · [1, 1, 1]".into()),
            Some("12.345".into()),
            Some("2026-01-01 01:00:00+00".into()),
            Some("{k=1}".into()),
        ]
    );
    assert_eq!(
        opened.dataset.summary().as_deref(),
        Some("Nested fields opened into columns.")
    );
}

/// Plain records with no nesting and no envelope: the columns, types and
/// cells as before this feature, no summary and no notes.
fn assert_plain(opened: &Opened) {
    assert_eq!(
        opened.columns(),
        [
            ("id".to_string(), "BIGINT".to_string()),
            ("name".into(), "VARCHAR".into()),
            ("amount".into(), "DOUBLE".into()),
        ]
    );
    assert_eq!(
        opened.rows(&opened.frame),
        [
            [
                Some("1".to_string()),
                Some("Ada".into()),
                Some("2.5".into())
            ],
            [Some("2".to_string()), Some("Grace".into()), None],
        ]
    );
    assert_eq!(opened.dataset.summary(), None);
    assert!(opened.notes().is_empty());
}

#[test]
fn flat_json_reads_as_before() {
    let array = open_text(
        ".json",
        r#"[{"id": 1, "name": "Ada", "amount": 2.5}, {"id": 2, "name": "Grace", "amount": null}]"#,
    );
    assert_plain(&array);
    let lines = open_text(
        ".jsonl",
        "{\"id\": 1, \"name\": \"Ada\", \"amount\": 2.5}\n{\"id\": 2, \"name\": \"Grace\", \"amount\": null}\n",
    );
    assert_plain(&lines);
    // One object with no list of records stays one row, as before.
    let object = open_text(".json", r#"{"id": 1, "name": "Ada", "amount": 2.5}"#);
    assert_eq!(object.frame.rows, Some(1));
    assert_eq!(object.names(), ["id", "name", "amount"]);
    assert_eq!(object.dataset.summary(), None);
}

#[test]
fn flat_parquet_reads_as_before() {
    let mut source = open_path(&messy("00_clean_sales.csv"));
    let path = temp_path(".parquet");
    let copy = format!(
        "COPY (SELECT * FROM (VALUES (1, 'Ada', 2.5), (2, 'Grace', NULL)) t(id, name, amount)) \
         TO '{}' (FORMAT parquet)",
        path.display()
    );
    assert!(matches!(
        source.dataset.query(&copy).unwrap(),
        SqlOutcome::Done
    ));
    source.written.push(path.clone());
    let parquet = open_path(&path);
    assert_eq!(
        parquet.columns(),
        [
            ("id".to_string(), "INTEGER".to_string()),
            ("name".into(), "VARCHAR".into()),
            ("amount".into(), "DECIMAL(2,1)".into()),
        ]
    );
    assert_eq!(
        parquet.rows(&parquet.frame),
        [
            [
                Some("1".to_string()),
                Some("Ada".into()),
                Some("2.5".into())
            ],
            [Some("2".to_string()), Some("Grace".into()), None],
        ]
    );
    assert_eq!(parquet.dataset.summary(), None);
}

/// Sort, a filter, the stats summary, a chart and CSV export on a dotted column.
#[test]
fn a_dotted_column_works_everywhere() {
    let mut opened = open_path(&messy("30_api_envelope.json"));
    let city = "customer.address.city";
    let cities = opened.cells(&opened.frame, city);

    let sorted = opened
        .dataset
        .apply(
            &opened.frame,
            &ViewSpec {
                sort: vec![SortKey {
                    column: city.into(),
                    descending: true,
                }],
                ..ViewSpec::default()
            },
        )
        .unwrap();
    let mut expected = cities.clone();
    expected.sort_by(|a, b| b.cmp(a));
    assert_eq!(opened.cells(&sorted, city), expected);

    let filtered = opened
        .dataset
        .apply(
            &opened.frame,
            &ViewSpec {
                filters: vec![Filter {
                    column: city.into(),
                    op: FilterOp::Equals("Pune".into()),
                }],
                ..ViewSpec::default()
            },
        )
        .unwrap();
    let pune = cities
        .iter()
        .filter(|c| c.as_deref() == Some("Pune"))
        .count();
    assert!(pune > 0);
    assert_eq!(filtered.rows, Some(pune as u64));

    let summaries = opened.dataset.summarize(&opened.frame).unwrap();
    let summary = summaries.iter().find(|s| s.name == city).unwrap();
    assert_eq!(summary.count, 40);
    assert_eq!(summary.nulls, 0);
    let sketches = opened.dataset.sketches(&opened.frame).unwrap();
    assert_eq!(sketches.len(), opened.frame.columns.len());

    let chart = opened
        .dataset
        .chart(
            &opened.frame,
            &ChartSpec {
                kind: ChartKind::Bar,
                x: Some("customer.tier".into()),
                y: vec!["total".into()],
                aggregate: Aggregate::Sum,
                color: Some(city.into()),
                sort: BarSort::default(),
            },
        )
        .unwrap();
    let drawn: f64 = chart
        .series
        .iter()
        .flat_map(|s| &s.points)
        .filter_map(|p| p.y)
        .sum();
    let SqlOutcome::Rows(total) = opened.dataset.query("SELECT sum(total) FROM data").unwrap()
    else {
        panic!("no rows");
    };
    let total: f64 = opened.rows(&total)[0][0]
        .as_deref()
        .unwrap()
        .parse()
        .unwrap();
    assert!((drawn - total).abs() < 1e-6, "{drawn} vs {total}");

    let csv = temp_path(".csv");
    opened
        .dataset
        .export(&filtered, &csv, ExportFormat::Csv)
        .unwrap();
    opened.written.push(csv.clone());
    let text = std::fs::read_to_string(&csv).unwrap();
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some(
            "id,date,customer.name,customer.tier,customer.address.city,customer.address.zip,items,total"
        )
    );
    assert_eq!(lines.count(), pune);
}

/// A one-object file whose records were not unwrapped: one row and no
/// notes. Its structs may still open into columns.
fn assert_one_row(opened: &Opened) {
    assert_eq!(opened.frame.rows, Some(1));
    assert!(
        opened
            .dataset
            .summary()
            .is_none_or(|s| s == "Nested fields opened into columns.")
    );
    assert!(opened.notes().is_empty());
}

#[test]
fn names_and_rows_are_paired_only_when_unambiguous() {
    let first_two = |opened: &Opened| opened.rows(&opened.frame)[..2].to_vec();
    let ada_grace = [
        [Some("1".to_string()), Some("Ada".into())],
        [Some("2".to_string()), Some("Grace".into())],
    ];
    // Keys named for it win over an earlier list of text.
    let named = open_text(
        ".json",
        r#"{"warnings":["partial results"],"columns":["id","name"],"rows":[[1,"Ada"],[2,"Grace"]]}"#,
    );
    assert_eq!(named.names(), ["id", "name"]);
    assert_eq!(first_two(&named), ada_grace);
    assert_eq!(named.notes(), Vec::<String>::new());
    // Other keys: the one list of text as long as the rows.
    let fitting = open_text(
        ".json",
        r#"{"warnings":["partial results"],"names":["id","name"],"body":[[1,"Ada"],[2,"Grace"]]}"#,
    );
    assert_eq!(fitting.names(), ["id", "name"]);
    assert_eq!(first_two(&fitting), ada_grace);
    // Two lists of text that fit: no guess.
    assert_one_row(&open_text(
        ".json",
        r#"{"a":["id","name"],"b":["x","y"],"body":[[1,"Ada"],[2,"Grace"]]}"#,
    ));
}

#[test]
fn names_must_be_as_many_as_the_longest_row() {
    assert_one_row(&open_text(
        ".json",
        r#"{"columns":["id","name"],"rows":[[1,"a",3.5],[2,"b",4.5]]}"#,
    ));
    assert_one_row(&open_text(
        ".json",
        r#"{"columns":["id","name","x"],"rows":[[1,"a"],[2,"b"]]}"#,
    ));
}

#[test]
fn column_definitions_name_the_rows() {
    let rows: Vec<String> = (0..50).map(|i| format!(r#"[{i},"Pune"]"#)).collect();
    let rows = rows.join(",");
    let columns = r#"[{"name":"id","type":"int"},{"name":"city","type":"text"}]"#;
    let top = open_text(
        ".json",
        &format!(r#"{{"columns":{columns},"rows":[{rows}]}}"#),
    );
    assert_eq!(top.frame.rows, Some(50));
    assert_eq!(top.names(), ["id", "city"]);
    assert_eq!(
        top.rows(&top.frame)[1],
        [Some("1".to_string()), Some("Pune".into())]
    );
    assert!(
        top.dataset
            .summary()
            .is_some_and(|s| s.starts_with("50 rows under rows, named by columns."))
    );
    // One key down there is no table, and the definitions aren't the data.
    assert_one_row(&open_text(
        ".json",
        &format!(r#"{{"result":{{"columns":{columns},"rows":[{rows}]}}}}"#),
    ));
}

#[test]
fn notes_hold_fields_of_sibling_objects_and_nulls() {
    let records: Vec<String> = (0..25).map(|i| format!(r#"{{"id":{i}}}"#)).collect();
    let opened = open_text(
        ".json",
        &format!(
            r#"{{"data":[{}],"meta":{{"page":1,"per_page":25,"total":250,"links":{{"next":"x"}}}},"previous":null}}"#,
            records.join(",")
        ),
    );
    assert_eq!(opened.frame.rows, Some(25));
    assert_eq!(
        opened.notes(),
        [
            "meta.page: 1",
            "meta.per_page: 25",
            "meta.total: 250",
            "previous: null"
        ]
    );
    // Many fields: the first 50 are kept.
    let fields: Vec<String> = (0..60).map(|i| format!(r#""f{i}":{i}"#)).collect();
    let many = open_text(
        ".json",
        &format!(r#"{{{},"data":[{{"id":1}}]}}"#, fields.join(",")),
    );
    let notes = many.notes();
    assert_eq!(notes.len(), 50);
    assert_eq!(notes[49], "f49: 49");
}

#[test]
fn one_object_over_16_mb_opens() {
    let note = "x".repeat(80);
    let records: Vec<String> = (0..170_000)
        .map(|i| format!(r#"{{"id":{i},"note":"{note}"}}"#))
        .collect();
    let text = format!(r#"{{"status":"ok","data":[{}]}}"#, records.join(","));
    assert!(text.len() > 16 << 20, "{} bytes", text.len());
    let opened = open_text(".json", &text);
    assert_eq!(opened.frame.rows, Some(170_000));
    assert_eq!(opened.notes(), ["status: ok"]);
}
