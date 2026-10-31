use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use da_core::*;

/// A fresh scratch directory per test; removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "da-core-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn open(&self, path: &Path) -> Dataset {
        Dataset::open(
            path,
            &OpenOptions {
                cache_dir: self.0.join("cache"),
                sheet: 0,
            },
        )
        .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn cells(page: &Page) -> Vec<Vec<String>> {
    page.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| c.clone().unwrap_or_else(|| "∅".into()))
                .collect()
        })
        .collect()
}

fn names(frame: &Frame) -> Vec<&str> {
    frame.columns.iter().map(|c| c.name.as_str()).collect()
}

#[test]
fn csv_previews_then_materializes() {
    let s = Scratch::new();
    let path = s.write(
        "people.csv",
        "id,name,score\n1,Ada,9.5\n2,Grace,\n3,Linus,7\n",
    );
    let ds = s.open(&path);
    assert!(!ds.is_loaded());

    let preview = ds.frame().unwrap();
    assert_eq!(preview.rows, None);
    assert_eq!(names(&preview), ["id", "name", "score"]);
    assert_eq!(
        cells(&ds.fetch(&preview, 0, 2).unwrap())[1],
        ["2", "Grace", "∅"]
    );

    let progress = std::sync::Mutex::new(Vec::new());
    let frame = ds
        .materialize(&|p| progress.lock().unwrap().push(p))
        .unwrap();
    assert!(ds.is_loaded());
    assert_eq!(frame.rows, Some(3));
    assert_eq!(*progress.lock().unwrap().last().unwrap(), 1.0);
    let kinds: Vec<_> = frame.columns.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        [ColumnKind::Number, ColumnKind::Text, ColumnKind::Number]
    );
    assert_eq!(
        cells(&ds.fetch(&frame, 2, 10).unwrap()),
        [["3", "Linus", "7"]]
    );
    assert_eq!(ds.notice(), None);
}

#[test]
fn csv_with_late_type_change_falls_back_instead_of_failing() {
    let s = Scratch::new();
    // 30k numeric rows, then text: past the sniffer's default sample.
    let mut csv = String::from("a,b\n");
    for i in 0..30_000 {
        csv.push_str(&format!("{i},x\n"));
    }
    csv.push_str("oops,y\n");
    let path = s.write("late.csv", &csv);
    let ds = s.open(&path);
    let frame = ds.materialize(&|_| {}).unwrap();
    assert_eq!(frame.rows, Some(30_001));
    let last = ds.fetch(&frame, 30_000, 1).unwrap();
    assert_eq!(cells(&last), [["oops", "y"]]);
}

#[test]
fn floats_print_compactly() {
    let s = Scratch::new();
    let path = s.write(
        "f.csv",
        "x\n0.0000581092822151589\n501.00073403878823\n0.1\n1e20\n\n",
    );
    let ds = s.open(&path);
    let frame = ds.materialize(&|_| {}).unwrap();
    assert_eq!(frame.columns[0].sql_type, "DOUBLE");
    assert_eq!(
        cells(&ds.fetch(&frame, 0, 10).unwrap()),
        [
            ["5.81092822152e-05"],
            ["501.000734039"],
            ["0.1"],
            ["1e+20"],
            ["∅"]
        ]
    );
}

#[test]
fn ragged_csv_opens() {
    let s = Scratch::new();
    // Mostly well-formed, with one short row and one long row past the sniff sample.
    let mut csv = String::from("a,b,c\n");
    for i in 0..30_000 {
        csv.push_str(&format!("{i},x,y\n"));
    }
    csv.push_str("short,row\nlong,row,with,extra\n");
    let ds = s.open(&s.write("ragged.csv", &csv));
    let frame = ds.materialize(&|_| {}).unwrap();
    assert_eq!(names(&frame), ["a", "b", "c"]);
    assert_eq!(frame.rows, Some(30_002), "{frame:?}");
    assert_eq!(
        cells(&ds.fetch(&frame, 30_000, 1).unwrap()),
        [["short", "row", "∅"]]
    );
    assert!(ds.notice().is_some());
}

#[test]
fn tsv_and_ndjson() {
    let s = Scratch::new();
    let tsv = s.write("t.tsv", "x\ty\n1\thello, world\n");
    let ds = s.open(&tsv);
    let frame = ds.materialize(&|_| {}).unwrap();
    assert_eq!(
        cells(&ds.fetch(&frame, 0, 5).unwrap()),
        [["1", "hello, world"]]
    );

    let ndjson = s.write("t.jsonl", "{\"a\":1,\"b\":\"x\"}\n{\"a\":2,\"b\":null}\n");
    let ds = s.open(&ndjson);
    let frame = ds.materialize(&|_| {}).unwrap();
    assert_eq!(frame.rows, Some(2));
    assert_eq!(
        cells(&ds.fetch(&frame, 0, 5).unwrap()),
        [["1", "x"], ["2", "∅"]]
    );
}

#[test]
fn json_array() {
    let s = Scratch::new();
    let path = s.write("a.json", r#"[{"k":"a","v":1.5},{"k":"b","v":2}]"#);
    let ds = s.open(&path);
    let frame = ds.materialize(&|_| {}).unwrap();
    assert_eq!(frame.rows, Some(2));
    assert_eq!(names(&frame), ["k", "v"]);
}

#[test]
fn parquet_is_ready_immediately() {
    let s = Scratch::new();
    let csv = s.write("src.csv", "n,s\n1,a\n2,b\n3,c\n");
    let pq = s.0.join("out.parquet");
    let ds = s.open(&csv);
    let frame = ds.materialize(&|_| {}).unwrap();
    ds.export(&frame, &pq, ExportFormat::Parquet).unwrap();

    let ds = s.open(&pq);
    assert!(ds.is_loaded());
    let frame = ds.frame().unwrap();
    assert_eq!(frame.rows, Some(3));
    assert_eq!(cells(&ds.fetch(&frame, 1, 1).unwrap()), [["2", "b"]]);
}

#[test]
fn xlsx_and_xls_load_with_types() {
    for name in ["fruit.xlsx", "fruit.xls"] {
        let s = Scratch::new();
        let ds = s.open(&fixture(name));
        assert!(ds.is_loaded(), "{name}");
        assert_eq!(ds.sheets()[0], "Fruit", "{name}");
        let frame = ds.frame().unwrap();
        assert_eq!(frame.rows, Some(3), "{name}");
        assert_eq!(
            names(&frame),
            ["id", "name", "price", "when", "ok", "mixed"],
            "{name}"
        );
        let types: Vec<&str> = frame.columns.iter().map(|c| c.sql_type.as_str()).collect();
        assert_eq!(
            types,
            [
                "BIGINT",
                "VARCHAR",
                "DOUBLE",
                "TIMESTAMP",
                "BOOLEAN",
                "VARCHAR"
            ],
            "{name}"
        );
        assert_eq!(
            cells(&ds.fetch(&frame, 0, 1).unwrap())[0],
            ["1", "apple", "1.5", "2024-01-02 03:04:05", "true", "10"],
            "{name}"
        );
        assert_eq!(cells(&ds.fetch(&frame, 1, 1).unwrap())[0][2], "∅", "{name}");
    }
}

#[test]
fn empty_sheet_opens() {
    let s = Scratch::new();
    let ds = Dataset::open(
        &fixture("fruit.xlsx"),
        &OpenOptions {
            cache_dir: s.0.join("cache"),
            sheet: 1,
        },
    )
    .unwrap();
    assert_eq!(ds.sheet(), 1);
    assert_eq!(ds.frame().unwrap().rows, Some(0));
}

fn numbers(s: &Scratch) -> (Dataset, Frame) {
    let mut csv = String::from("n,label,day\n");
    for i in 0..100 {
        csv.push_str(&format!(
            "{i},{},2024-01-{:02}\n",
            if i % 3 == 0 { "fizz" } else { "other" },
            i % 28 + 1
        ));
    }
    csv.push_str(",,\n");
    let ds = s.open(&s.write("n.csv", &csv));
    let frame = ds.materialize(&|_| {}).unwrap();
    (ds, frame)
}

#[test]
fn sort_filter_and_search() {
    let s = Scratch::new();
    let (ds, base) = numbers(&s);

    let spec = ViewSpec {
        sort: vec![SortKey {
            column: "n".into(),
            descending: true,
        }],
        filters: vec![Filter {
            column: "n".into(),
            op: FilterOp::GreaterThan("90".into()),
        }],
        search: String::new(),
    };
    let view = ds.apply(&base, &spec).unwrap();
    assert_eq!(view.rows, Some(9));
    assert_eq!(cells(&ds.fetch(&view, 0, 1).unwrap())[0][0], "99");

    let spec = ViewSpec {
        search: "FIZZ".into(),
        ..Default::default()
    };
    assert_eq!(ds.apply(&base, &spec).unwrap().rows, Some(34));

    let spec = ViewSpec {
        filters: vec![Filter {
            column: "label".into(),
            op: FilterOp::IsNull,
        }],
        ..Default::default()
    };
    assert_eq!(ds.apply(&base, &spec).unwrap().rows, Some(1));

    let spec = ViewSpec {
        filters: vec![Filter {
            column: "day".into(),
            op: FilterOp::Between("2024-01-01".into(), "2024-01-02".into()),
        }],
        ..Default::default()
    };
    assert_eq!(ds.apply(&base, &spec).unwrap().rows, Some(8));

    // An empty spec is the base frame itself.
    assert_eq!(ds.apply(&base, &ViewSpec::default()).unwrap().id, base.id);
}

#[test]
fn summaries_and_distributions() {
    let s = Scratch::new();
    let (ds, frame) = numbers(&s);

    let summary = ds.summarize(&frame).unwrap();
    assert_eq!(summary.len(), 3);
    assert_eq!(summary[0].name, "n");
    assert_eq!(summary[0].min.as_deref(), Some("0"));
    assert_eq!(summary[0].max.as_deref(), Some("99"));
    assert!(
        (summary[0].null_percent - 0.99).abs() < 0.01,
        "{:?}",
        summary[0]
    );

    match ds.distribution(&frame, &frame.columns[0], 10, 5).unwrap() {
        Distribution::Histogram(bins) => {
            assert_eq!(bins.len(), 10);
            assert_eq!(bins.iter().map(|b| b.count).sum::<u64>(), 100);
            assert_eq!(bins[0].first.as_deref(), Some("0"));
        }
        other => panic!("{other:?}"),
    }
    match ds.distribution(&frame, &frame.columns[1], 10, 5).unwrap() {
        Distribution::Categories { values, other } => {
            assert_eq!(values[0], (Some("other".into()), 66));
            assert_eq!(values[1], (Some("fizz".into()), 34));
            assert_eq!(other, 0);
        }
        other => panic!("{other:?}"),
    }
    let sketches = ds.sketches(&frame).unwrap();
    assert_eq!(sketches.len(), 3);
    assert!(
        matches!(sketches[2], Distribution::Histogram(_)),
        "{:?}",
        sketches[2]
    );
}

#[test]
fn sql_queries() {
    let s = Scratch::new();
    let (ds, _) = numbers(&s);

    let SqlOutcome::Rows(frame) = ds
        .query("SELECT label, count(*) AS c FROM data GROUP BY label ORDER BY c DESC;")
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert_eq!(names(&frame), ["label", "c"]);
    assert_eq!(frame.rows, Some(3));
    assert_eq!(cells(&ds.fetch(&frame, 0, 1).unwrap()), [["other", "66"]]);

    assert!(matches!(
        ds.query("SUMMARIZE data").unwrap(),
        SqlOutcome::Rows(_)
    ));
    assert!(matches!(
        ds.query("CREATE TABLE t AS SELECT 1 AS x").unwrap(),
        SqlOutcome::Done
    ));
    let err = ds.query("SELEC nope").unwrap_err().to_string();
    assert!(err.to_lowercase().contains("syntax"), "{err}");
}

#[test]
fn exports_filtered_view() {
    let s = Scratch::new();
    let (ds, base) = numbers(&s);
    let spec = ViewSpec {
        filters: vec![Filter {
            column: "n".into(),
            op: FilterOp::LessThan("3".into()),
        }],
        ..Default::default()
    };
    let view = ds.apply(&base, &spec).unwrap();
    let out = s.0.join("out.csv");
    ds.export(&view, &out, ExportFormat::Csv).unwrap();
    let text = std::fs::read_to_string(out).unwrap();
    assert_eq!(text.lines().count(), 4, "{text}");
    assert!(text.starts_with("n,label,day"));
}

#[test]
fn cancel_load() {
    let s = Scratch::new();
    let mut csv = String::from("a,b\n");
    for i in 0..2_000_000 {
        csv.push_str(&format!("{i},{}\n", i * 7));
    }
    let ds = std::sync::Arc::new(s.open(&s.write("big.csv", &csv)));
    let worker = {
        let ds = ds.clone();
        std::thread::spawn(move || ds.materialize(&|_| {}))
    };
    ds.cancel_load();
    let result = worker.join().unwrap();
    assert!(result.unwrap_err().is::<Cancelled>());
}

#[test]
fn session_files_are_removed_on_drop() {
    let s = Scratch::new();
    let path = s.write("x.csv", "a\n1\n");
    let ds = s.open(&path);
    let cache = s.0.join("cache");
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
    drop(ds);
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
}

#[test]
fn removes_own_sessions_on_quit() {
    let s = Scratch::new();
    let cache = s.0.join("cache");
    let ds = s.open(&s.write("x.csv", "a\n1\n"));
    std::fs::create_dir_all(cache.join("session-1-0")).unwrap(); // pid 1 (launchd) is alive
    remove_own_sessions(&cache);
    let left: Vec<_> = std::fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(left, ["session-1-0"]);
    drop(ds);
}

#[test]
fn rejects_unknown_files() {
    let s = Scratch::new();
    let path = s.write("x.png", "nope");
    let err = Dataset::open(
        &path,
        &OpenOptions {
            cache_dir: s.0.clone(),
            sheet: 0,
        },
    );
    assert!(err.is_err());
}
