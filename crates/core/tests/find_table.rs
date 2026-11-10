//! The table finder against the messy fixtures: each test is a row of the
//! acceptance table for "find the table inside the file".

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use da_core::*;

/// A cache directory per opened file; removed on drop, with the file itself
/// when a test wrote it.
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
    fn columns(&self) -> Vec<(&str, &str)> {
        self.frame
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.sql_type.as_str()))
            .collect()
    }

    fn names(&self) -> Vec<&str> {
        self.frame.columns.iter().map(|c| c.name.as_str()).collect()
    }

    fn rows(&self) -> u64 {
        self.frame.rows.unwrap()
    }

    fn scalar(&self, sql: &str) -> String {
        let SqlOutcome::Rows(frame) = self.dataset.query(sql).unwrap() else {
            panic!("no rows from {sql}");
        };
        let page = self.dataset.fetch(&frame, 0, 1).unwrap();
        page.rows[0][0].clone().unwrap()
    }

    fn parts(&self) -> Vec<(&str, Option<u64>, Option<&str>)> {
        self.dataset
            .parts()
            .iter()
            .map(|p| (p.label.as_str(), p.rows, p.at.as_deref()))
            .collect()
    }
}

fn messy(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/messy")
        .join(name)
}

/// A fresh temp path for one test file or cache dir.
fn temp_path(suffix: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "da-find-table-{}-{}{suffix}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Opens and fully loads a messy fixture with `options` (its cache dir is filled in).
fn open_with(name: &str, options: OpenOptions) -> Opened {
    open_path(&messy(name), options)
}

/// Writes `text` to a temp CSV file and opens it.
fn open_csv(text: &str) -> Opened {
    let file = temp_path(".csv");
    std::fs::write(&file, text).unwrap();
    let mut opened = open_path(&file, OpenOptions::default());
    opened.written = Some(file);
    opened
}

fn open_path(path: &Path, options: OpenOptions) -> Opened {
    let cache = temp_path("");
    let dataset = Dataset::open(
        path,
        &OpenOptions {
            cache_dir: cache.clone(),
            ..options
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
    open_with(name, OpenOptions::default())
}

fn open_part(name: &str, part: usize) -> Opened {
    open_with(
        name,
        OpenOptions {
            part: Some(part),
            ..Default::default()
        },
    )
}

/// The columns of the report in `01`, `20` and `25`.
const REPORT_COLUMNS: [(&str, &str); 7] = [
    ("Order ID", "BIGINT"),
    ("Order Date", "DATE"),
    ("Region", "VARCHAR"),
    ("Product", "VARCHAR"),
    ("Units", "BIGINT"),
    ("Unit Price", "DOUBLE"),
    ("Revenue", "DOUBLE"),
];

#[test]
fn clean_file_is_unchanged() {
    let o = open("00_clean_sales.csv");
    assert_eq!(o.rows(), 500);
    assert_eq!(
        o.columns(),
        [
            ("order_id", "BIGINT"),
            ("order_date", "DATE"),
            ("region", "VARCHAR"),
            ("product", "VARCHAR"),
            ("units", "BIGINT"),
            ("unit_price", "DOUBLE"),
            ("revenue", "DOUBLE"),
        ]
    );
    assert_eq!(o.dataset.summary().as_deref(), None);
    assert!(o.dataset.notes().is_empty());
    assert_eq!(o.dataset.delimiter(), Some(Delimiter::Comma));
}

#[test]
fn report_preamble_sets_aside_titles_total_and_note() {
    let o = open("01_report_preamble.csv");
    assert_eq!(o.columns(), REPORT_COLUMNS);
    assert_eq!(o.rows(), 40);
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Table found on lines 5 to 45. Set aside 3 title lines, 1 total and 1 note.")
    );
    let notes: Vec<(usize, RowRole)> = o.dataset.notes().iter().map(|n| (n.row, n.role)).collect();
    assert_eq!(
        notes,
        [
            (1, RowRole::Title),
            (2, RowRole::Title),
            (3, RowRole::Title),
            (47, RowRole::Total),
            (48, RowRole::Note),
        ]
    );
    assert_eq!(
        o.dataset.notes()[0].text,
        "Northwind Foods - Weekly Sales Report"
    );
    // The original path is still the file, not the cleaned copy.
    assert_eq!(
        o.dataset.path(),
        messy("01_report_preamble.csv").canonicalize().unwrap()
    );
}

#[test]
fn two_stacked_tables_are_two_parts() {
    let o = open("04_two_tables.csv");
    assert_eq!(
        o.parts(),
        [
            ("Table 1", Some(4), Some("lines 1 to 5")),
            ("Table 2", Some(5), Some("lines 7 to 12")),
        ]
    );
    assert_eq!(o.dataset.part(), 1, "opens the part with the most rows");
    assert_eq!(
        o.names(),
        ["Product", "SKU", "Units Sold", "Returns", "Return Rate"]
    );
    assert_eq!(o.rows(), 5);

    let first = open_part("04_two_tables.csv", 0);
    assert_eq!(
        first.columns(),
        [
            ("Region", "VARCHAR"),
            ("Orders", "BIGINT"),
            ("Revenue", "BIGINT")
        ]
    );
    assert_eq!(first.rows(), 4);
    assert_eq!(
        first.dataset.summary().as_deref(),
        Some("2 tables in the file. Showing lines 1 to 5.")
    );
}

#[test]
fn headerless_file_is_unchanged() {
    let o = open("05_no_header.csv");
    assert_eq!(o.rows(), 40);
    assert_eq!(o.names()[0], "column0", "still DuckDB's own reading");
    assert_eq!(o.dataset.summary().as_deref(), None);
}

#[test]
fn multirow_header_is_joined_with_units() {
    let o = open("06_multirow_header.csv");
    assert_eq!(
        o.columns(),
        [
            ("Store", "VARCHAR"),
            ("Week", "VARCHAR"),
            ("Sales · Units", "BIGINT"),
            ("Sales · Revenue", "BIGINT"),
            ("Inventory · On Hand", "BIGINT"),
            ("Inventory · On Order", "BIGINT"),
        ]
    );
    assert_eq!(o.rows(), 20);
    let units: Vec<Option<&str>> = o.names().iter().map(|n| o.dataset.unit(n)).collect();
    assert_eq!(
        units,
        [
            None,
            None,
            Some("each"),
            Some("USD"),
            Some("each"),
            Some("each")
        ]
    );
}

#[test]
fn wide_months_is_unchanged() {
    let o = open("07_wide_months.csv");
    assert_eq!(o.rows(), 12);
    assert_eq!(o.names().len(), 8);
    assert_eq!(o.dataset.summary().as_deref(), None);
}

#[test]
fn trailing_delimiters_leave_no_phantom_column() {
    let o = open("09_bom_trailing_delims.csv");
    assert_eq!(o.names(), ["id", "name", "amount", "amount_2", "notes"]);
    assert_eq!(o.rows(), 12);
}

#[test]
fn metadata_lines_become_notes() {
    let o = open("11_metadata_then_table.txt");
    assert_eq!(
        o.columns(),
        [
            ("timestamp", "TIMESTAMP"),
            ("temp_c", "DOUBLE"),
            ("humidity", "DOUBLE"),
            ("battery_v", "DOUBLE"),
        ]
    );
    assert_eq!(o.rows(), 60);
    let notes: Vec<&str> = o.dataset.notes().iter().map(|n| n.text.as_str()).collect();
    assert_eq!(
        notes,
        [
            "# Device: ThermoLog TL-200",
            "# Serial: TL2-88213",
            "# Started: 2026-05-02T08:00:00Z",
            "# Interval: 60 s",
        ]
    );
    assert_eq!(o.dataset.delimiter(), Some(Delimiter::Tab));
}

#[test]
fn subtotals_are_left_out_unless_kept() {
    let o = open("12_subtotals.csv");
    assert_eq!(o.rows(), 12);
    assert_eq!(o.scalar("SELECT sum(Revenue) FROM data"), "53504");
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("5 total rows set aside. They are left out of stats and charts.")
    );
    assert_eq!(
        note_roles(&o),
        [
            (5, RowRole::Total, "North Total 1404 11768"),
            (9, RowRole::Total, "South Total 992 14402"),
            (13, RowRole::Total, "East Total 1524 10404"),
            (17, RowRole::Total, "West Total 1765 16930"),
            (18, RowRole::Total, "Grand Total 5685 53504"),
        ]
    );

    let plan = ImportPlan {
        keep_totals: true,
        ..*o.dataset.plan().unwrap()
    };
    let kept = open_with(
        "12_subtotals.csv",
        OpenOptions {
            plan: Some(plan),
            ..Default::default()
        },
    );
    assert_eq!(kept.rows(), 17);
    assert_eq!(kept.dataset.plan(), Some(&plan));
    assert_eq!(
        kept.dataset.summary().as_deref(),
        Some("5 total rows kept in the data.")
    );
    assert!(kept.dataset.notes().is_empty(), "kept totals are data");
}

#[test]
fn preview_marks_totals_as_loading_does() {
    // Labelled like a total, but 4 is not a sum of the rows above: loaded as data.
    let o = open_csv("team,points\nRed,5\nBlue,3\nTotal:,4\n");
    assert_eq!(o.rows(), 3);
    let preview = o.dataset.preview(o.dataset.plan().unwrap()).unwrap();
    assert_eq!(preview.rows[3].role, RowRole::Data);

    // A real subtotal shows as a total, and as data once totals are kept.
    let o = open("12_subtotals.csv");
    let plan = *o.dataset.plan().unwrap();
    let totals = |plan: &ImportPlan| {
        let preview = o.dataset.preview(plan).unwrap();
        preview
            .rows
            .iter()
            .filter(|r| r.role == RowRole::Total)
            .count()
    };
    assert_eq!(totals(&plan), 5);
    let kept = ImportPlan {
        keep_totals: true,
        ..plan
    };
    assert_eq!(totals(&kept), 0);
}

#[test]
fn excel_report_finds_the_table_at_b5() {
    let o = open("20_report_title.xlsx");
    assert_eq!(o.columns(), REPORT_COLUMNS);
    assert_eq!(o.rows(), 40);
    assert_eq!(o.parts(), [("Weekly Sales", Some(40), Some("B5:H45"))]);
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Table found at B5:H45. Set aside 2 title rows, 1 total and 1 note.")
    );
}

#[test]
fn merged_group_headers_carry_right() {
    let o = open("21_merged_headers.xlsx");
    assert_eq!(
        o.columns(),
        [
            ("Region", "VARCHAR"),
            ("Q1 · Units", "BIGINT"),
            ("Q1 · Revenue", "BIGINT"),
            ("Q2 · Units", "BIGINT"),
            ("Q2 · Revenue", "BIGINT"),
        ]
    );
    assert_eq!(o.rows(), 4);
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Table at A1:E6. 2 header rows joined into one.")
    );
}

#[test]
fn side_by_side_tables_are_parts_named_by_caption() {
    let o = open("22_side_by_side_tables.xlsx");
    assert_eq!(
        o.parts(),
        [
            ("Revenue by region", Some(4), Some("A2:C6")),
            ("Top products", Some(5), Some("F2:H7")),
        ]
    );
    assert_eq!(o.dataset.part(), 1);
    assert_eq!(o.names(), ["Product", "Units", "Return rate"]);
    assert_eq!(o.rows(), 5);
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("2 tables on sheet Dashboard. Showing F2:H7. Set aside 1 title row.")
    );
    assert_eq!(note_roles(&o), [(1, RowRole::Title, "Top products")]);
    let left = open_part("22_side_by_side_tables.xlsx", 0);
    assert_eq!(left.names(), ["Region", "Orders", "Revenue"]);
    assert_eq!(left.rows(), 4);
    assert_eq!(
        note_roles(&left),
        [(1, RowRole::Title, "Revenue by region")]
    );
}

#[test]
fn year_headers_are_names() {
    let o = open_part("23_year_headers_and_formats.xlsx", 0);
    assert_eq!(o.dataset.parts()[0].label, "By Year");
    assert_eq!(
        o.columns(),
        [
            ("Region", "VARCHAR"),
            ("2022", "BIGINT"),
            ("2023", "BIGINT"),
            ("2024", "BIGINT"),
            ("2025", "BIGINT"),
        ]
    );
    assert_eq!(o.rows(), 4);

    // The Formats sheet is read whole, so the finder has nothing to say;
    // what it says is the cleaner's (see `read_values.rs`).
    let formats = open_part("23_year_headers_and_formats.xlsx", 1);
    assert_eq!(formats.rows(), 12);
    assert_eq!(
        formats.dataset.summary().as_deref(),
        Some("Read 2 text columns as numbers and 1 as dates. #DIV/0! read as empty.")
    );
}

#[test]
fn cover_sheet_workbook_opens_on_the_data() {
    let o = open("24_cover_sheet_first.xlsx");
    let labels: Vec<&str> = o.parts().iter().map(|p| p.0).collect();
    assert_eq!(labels, ["README", "Definitions", "Orders"]);
    assert_eq!(o.dataset.part(), 2);
    assert_eq!(o.rows(), 200);

    // README has no table and reads as it did before the finder.
    let readme = open_part("24_cover_sheet_first.xlsx", 0);
    assert_eq!(
        readme.columns(),
        [("Northwind Foods data extract", "VARCHAR")]
    );
    assert_eq!(readme.rows(), 7);
    assert_eq!(readme.dataset.summary().as_deref(), None);
}

#[test]
fn xls_report_sets_aside_the_adjacent_total() {
    let o = open("25_report_title.xls");
    assert_eq!(o.columns(), REPORT_COLUMNS);
    assert_eq!(o.rows(), 40);
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Table found at A4:G45. Set aside 2 title rows and 1 total.")
    );
    assert_eq!(
        note_roles(&o),
        [
            (1, RowRole::Title, "Northwind Foods - Weekly Sales Report"),
            (2, RowRole::Title, "Generated 3 Jul 2026"),
            (45, RowRole::Total, "Total 2388.09"),
        ]
    );
    assert_eq!(
        o.scalar("SELECT round(sum(Revenue), 2) FROM data"),
        "2388.09"
    );
}

/// Files the finder leaves alone: their columns and row counts as before,
/// and nothing to say about the table. The values in `02`, `03` and `10`,
/// and the encoding of `08`, are read by the cleaner: see `read_values.rs`.
#[test]
fn out_of_scope_files_are_unchanged() {
    let cases: [(&str, u64, &[&str]); 7] = [
        (
            "02_semicolon_european.csv",
            40,
            &[
                "Bestellnr",
                "Datum",
                "Region",
                "Produkt",
                "Menge",
                "Einzelpreis",
                "Umsatz",
            ],
        ),
        (
            "03_currency_percent.csv",
            30,
            &[
                "account",
                "region",
                "revenue",
                "cost",
                "margin_pct",
                "yoy_growth",
                "headcount",
            ],
        ),
        (
            "10_ids_dates_flags.csv",
            24,
            &["zip", "phone", "sku", "signup", "active", "last_seen"],
        ),
        (
            "30_api_envelope.json",
            1,
            &["status", "page", "total_pages", "data"],
        ),
        (
            "31_nested.jsonl",
            40,
            &["id", "date", "customer", "items", "total"],
        ),
        ("32_columns_and_rows.json", 1, &["columns", "rows"]),
        (
            "40_nested.parquet",
            200,
            &["id", "customer", "scores", "amount", "seen_at", "attrs"],
        ),
    ];
    for (name, rows, names) in cases {
        let o = open(name);
        assert_eq!(o.rows(), rows, "{name}");
        assert_eq!(o.names(), names, "{name}");
        let summary = o.dataset.summary().unwrap_or_default();
        assert!(
            summary.is_empty() || summary.starts_with("Read "),
            "{name}: {summary}"
        );
    }
}

#[test]
fn a_plan_overrides_detection() {
    // Say the title line is the header and the body is lines 6 to 10.
    let plan = ImportPlan {
        header: Some(Rows { first: 5, last: 5 }),
        data: Rows { first: 6, last: 10 },
        keep_totals: false,
    };
    let o = open_with(
        "01_report_preamble.csv",
        OpenOptions {
            plan: Some(plan),
            ..Default::default()
        },
    );
    assert_eq!(o.rows(), 5);
    assert_eq!(o.dataset.plan(), Some(&plan));
    assert_eq!(o.names()[0], "Order ID");

    // No header at all on the clean file: the first line becomes data.
    let plan = ImportPlan {
        header: None,
        data: Rows {
            first: 1,
            last: 501,
        },
        keep_totals: false,
    };
    let o = open_with(
        "00_clean_sales.csv",
        OpenOptions {
            plan: Some(plan),
            ..Default::default()
        },
    );
    assert_eq!(o.rows(), 501);
    assert_eq!(o.names()[0], "column_1");
    assert_eq!(o.columns()[0].1, "VARCHAR");
}

#[test]
fn a_chosen_delimiter_reruns_detection() {
    let o = open_with(
        "00_clean_sales.csv",
        OpenOptions {
            delimiter: Delimiter::Semicolon,
            ..Default::default()
        },
    );
    assert_eq!(o.dataset.delimiter(), Some(Delimiter::Semicolon));
    assert_eq!(o.names().len(), 1, "no semicolons, so one column");
    assert_eq!(o.rows(), 500);
}

#[test]
fn preview_shows_roles_under_a_plan() {
    let o = open("01_report_preamble.csv");
    let plan = *o.dataset.plan().unwrap();
    let preview = o.dataset.preview(&plan).unwrap();
    assert_eq!((preview.total_rows, preview.data_rows), (48, 40));
    let role = |n: usize| preview.rows.iter().find(|r| r.number == n).unwrap().role;
    assert_eq!(
        (1..=7).map(role).collect::<Vec<_>>(),
        [
            RowRole::Title,
            RowRole::Title,
            RowRole::Title,
            RowRole::Blank,
            RowRole::Header,
            RowRole::Data,
            RowRole::Data,
        ]
    );
    assert_eq!(
        (role(46), role(47), role(48)),
        (RowRole::Blank, RowRole::Total, RowRole::Note)
    );
    let header = &preview.rows[4];
    assert_eq!(
        header.line.as_deref(),
        Some("Order ID,Order Date,Region,Product,Units,Unit Price,Revenue")
    );
    assert_eq!(header.cells.len(), 7);

    // A sheet previews as cells, numbered as the sheet numbers its rows.
    let o = open("20_report_title.xlsx");
    let preview = o.dataset.preview(o.dataset.plan().unwrap()).unwrap();
    let header = preview.rows.iter().find(|r| r.number == 5).unwrap();
    assert_eq!(
        (header.role, header.line.as_deref()),
        (RowRole::Header, None)
    );
    assert_eq!(header.cells[1], "Order ID");

    // Files that aren't scanned have nothing to preview.
    let o = open("40_nested.parquet");
    assert!(o.dataset.plan().is_none());
}

#[test]
fn a_column_of_years_is_data_not_header() {
    let o = open_csv(
        "title,year\nAlien,1979\nHeat,1995\nRonin,1998\nDrive,2011\nTenet,2020\nDune,2021\n",
    );
    assert_eq!(o.columns(), [("title", "VARCHAR"), ("year", "BIGINT")]);
    assert_eq!(o.rows(), 6);
    assert_eq!(o.dataset.summary().as_deref(), None);

    // Years over numbers that are not years are still a header.
    let o = open_csv("date,2020,2021\n2024-01-01,5,6\n2024-01-02,7,8\n");
    assert_eq!(o.names(), ["date", "2020", "2021"]);
    assert_eq!(o.rows(), 2);
}

#[test]
fn only_real_totals_rows_are_set_aside() {
    let o = open_csv("product,units,revenue\nTotal Care Shampoo,5,50\nSoap,3,9\nTotal,8,59\n");
    assert_eq!(o.rows(), 2);
    assert_eq!(o.scalar("SELECT sum(revenue) FROM data"), "59");
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("1 total row set aside. It is left out of stats and charts.")
    );

    let o = open_csv(
        "product,region,units\nTotal Care Shampoo,North,5\nSoap,North,3\nSubtotal Wax,South,2\n",
    );
    assert_eq!(o.rows(), 3);

    // A row labelled Total whose numbers are not a sum of the rows above is data.
    let o = open_csv("team,points\nRed,5\nBlue,3\nTotal,4\n");
    assert_eq!(o.rows(), 3);
}

#[test]
fn a_last_row_shaped_like_the_body_is_data() {
    let o = open_csv("name,note\nann,ok\nbob,\ncy,fine\nzed,\n");
    assert_eq!(o.rows(), 4);
    assert!(o.dataset.notes().is_empty());
}

/// Opens a CSV written from `text` with `plan`.
fn open_csv_with(text: &str, plan: ImportPlan) -> Opened {
    let file = temp_path(".csv");
    std::fs::write(&file, text).unwrap();
    let mut opened = open_path(
        &file,
        OpenOptions {
            plan: Some(plan),
            ..Default::default()
        },
    );
    opened.written = Some(file);
    opened
}

fn note_roles(o: &Opened) -> Vec<(usize, RowRole, &str)> {
    o.dataset
        .notes()
        .iter()
        .map(|n| (n.row, n.role, n.text.as_str()))
        .collect()
}

#[test]
fn a_plan_reports_the_rows_it_leaves_out() {
    // Only Bob is data: Alice and Carol are left out and must be said so.
    let plan = ImportPlan {
        header: Some(Rows { first: 1, last: 1 }),
        data: Rows { first: 3, last: 3 },
        keep_totals: false,
    };
    let o = open_csv_with("name,amount\nAlice,10\nBob,20\nCarol,\n", plan);
    assert_eq!(o.rows(), 1);
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Table found on lines 1 to 3. Set aside 1 title line and 1 note.")
    );
    assert_eq!(
        note_roles(&o),
        [(2, RowRole::Title, "Alice 10"), (4, RowRole::Note, "Carol")]
    );
    // The notes agree with the preview's roles.
    let preview = o.dataset.preview(&plan).unwrap();
    for note in o.dataset.notes() {
        let row = preview.rows.iter().find(|r| r.number == note.row).unwrap();
        assert_eq!(row.role, note.role);
    }
}

#[test]
fn the_detected_plan_passed_back_reads_the_same() {
    // The sheet's titles sit in column A, left of the table at B5:H45.
    for file in [
        "01_report_preamble.csv",
        "12_subtotals.csv",
        "20_report_title.xlsx",
        "25_report_title.xls",
    ] {
        let detected = open(file);
        let plan = *detected.dataset.plan().unwrap();
        let o = open_with(
            file,
            OpenOptions {
                plan: Some(plan),
                ..Default::default()
            },
        );
        assert_eq!(
            o.dataset.summary().as_deref(),
            detected.dataset.summary().as_deref(),
            "{file}"
        );
        assert_eq!(o.dataset.notes(), detected.dataset.notes(), "{file}");
    }
}

#[test]
fn a_header_moved_down_turns_the_old_header_into_a_note() {
    let detected = open("01_report_preamble.csv");
    let plan = *detected.dataset.plan().unwrap();
    let moved = ImportPlan {
        header: Some(Rows { first: 6, last: 6 }),
        data: Rows {
            first: 7,
            last: plan.data.last,
        },
        ..plan
    };
    let o = open_with(
        "01_report_preamble.csv",
        OpenOptions {
            plan: Some(moved),
            ..Default::default()
        },
    );
    let rows: Vec<(usize, RowRole)> = o.dataset.notes().iter().map(|n| (n.row, n.role)).collect();
    assert_eq!(
        rows,
        [
            (1, RowRole::Title),
            (2, RowRole::Title),
            (3, RowRole::Title),
            (5, RowRole::Title),
            (47, RowRole::Total),
            (48, RowRole::Note),
        ]
    );
    assert!(o.dataset.notes()[3].text.starts_with("Order ID Order Date"));
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Table found on lines 6 to 45. Set aside 4 title lines, 1 total and 1 note.")
    );
}

#[test]
fn a_spacer_column_keeps_a_plain_sheet_as_before() {
    // Before the finder: five columns, the empty one named as the sheet reader names it.
    let o = open("26_spacer_column.xlsx");
    assert_eq!(
        o.columns(),
        [
            ("Region", "VARCHAR"),
            ("column_2", "VARCHAR"),
            ("Units", "BIGINT"),
            ("Revenue", "DOUBLE"),
            ("Margin", "DOUBLE"),
        ]
    );
    assert_eq!(o.rows(), 3);
    assert_eq!(o.parts(), [("Report", Some(3), Some("A1:E4"))]);
    assert_eq!(o.dataset.summary().as_deref(), None);
}

#[test]
fn a_title_right_above_the_header_is_a_title_not_the_label() {
    // One table per sheet: each tab keeps its sheet's name.
    let o = open("27_monthly_sheets.xlsx");
    let labels: Vec<&str> = o.parts().iter().map(|p| p.0).collect();
    assert_eq!(labels, ["Jan", "Feb", "Mar"]);
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Table found at A2:C5. Set aside 1 title row.")
    );
    assert_eq!(
        note_roles(&o),
        [(1, RowRole::Title, "Monthly Sales Report")]
    );
    assert_eq!(o.names(), ["Region", "Units", "Revenue"]);

    let o = open_csv("Weekly report\nid,units\n1,2\n3,4\n5,6\n");
    assert_eq!(o.parts(), [("Table 1", Some(3), Some("lines 2 to 5"))]);
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Table found on lines 2 to 5. Set aside 1 title line.")
    );
    assert_eq!(note_roles(&o), [(1, RowRole::Title, "Weekly report")]);

    let o = open_csv("Weekly report\nGenerated today\nid,units\n1,2\n3,4\n5,6\n");
    assert_eq!(o.parts()[0].0, "Table 1");
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Table found on lines 3 to 6. Set aside 2 title lines.")
    );
    assert_eq!(
        note_roles(&o),
        [
            (1, RowRole::Title, "Weekly report"),
            (2, RowRole::Title, "Generated today"),
        ]
    );
}

#[test]
fn a_one_cell_title_is_never_a_group_label() {
    // The footer is filled like the title, but the title still is one.
    let o = open("28_title_and_footer.xlsx");
    assert_eq!(o.names(), ["id", "customer", "amount", "Notes"]);
    assert_eq!(
        o.dataset.plan().unwrap().header,
        Some(Rows { first: 2, last: 2 })
    );
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Table found at A2:D7. Set aside 1 title row.")
    );
    assert_eq!(note_roles(&o), [(1, RowRole::Title, "Customer follow-ups")]);
    // A one-cell row in a sheet is data unless it says it is a note.
    assert_eq!(o.rows(), 5);
}

#[test]
fn a_header_starting_with_a_hash_is_not_a_comment() {
    let o = open_csv("#id,name,qty\n1,a,2\n2,b,3\nTotal,,5\n");
    assert_eq!(
        o.columns(),
        [("#id", "BIGINT"), ("name", "VARCHAR"), ("qty", "BIGINT")]
    );
    assert_eq!(o.rows(), 2);
    assert_eq!(note_roles(&o), [(4, RowRole::Total, "Total 5")]);
}

#[test]
fn file_notes_list_every_total_the_summary_counts() {
    for file in [
        "01_report_preamble.csv",
        "12_subtotals.csv",
        "20_report_title.xlsx",
        "25_report_title.xls",
    ] {
        let o = open(file);
        let totals = o
            .dataset
            .notes()
            .iter()
            .filter(|n| n.role == RowRole::Total)
            .count();
        let summary = o.dataset.summary().unwrap();
        let said = if totals == 1 {
            "1 total"
        } else {
            "5 total rows"
        };
        assert!(
            summary.contains(said),
            "{file}: {summary} with {totals} listed"
        );
    }
}
