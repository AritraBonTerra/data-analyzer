//! Reading values the way people write them, against the messy fixtures
//! and shapes made here: each test is a row of the acceptance table for
//! "read values the way people write them", or one of its adversarial cases.

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
    fn sql_type(&self, column: &str) -> &str {
        &self.column(column).sql_type
    }

    fn column(&self, column: &str) -> &Column {
        self.frame
            .columns
            .iter()
            .find(|c| c.name == column)
            .unwrap_or_else(|| panic!("no column {column}"))
    }

    /// Every cell of `column`, as the grid shows it.
    fn cells(&self, column: &str) -> Vec<Option<String>> {
        cells(&self.dataset, &self.frame, column)
    }

    /// The first value `sql` returns, as text.
    fn scalar(&self, sql: &str) -> Option<String> {
        let SqlOutcome::Rows(frame) = self.dataset.query(sql).unwrap() else {
            panic!("no rows from {sql}");
        };
        self.dataset.fetch(&frame, 0, 1).unwrap().rows[0][0].clone()
    }

    fn notes(&self, column: &str) -> Vec<String> {
        self.conversion(column)
            .unwrap_or_else(|| panic!("{column} was not converted"))
            .notes
    }

    fn conversion(&self, column: &str) -> Option<Conversion> {
        self.dataset
            .conversions()
            .into_iter()
            .find(|c| c.column == column)
    }

    /// Reads values under new options and takes the new frame.
    fn set(&mut self, values: ValueOptions) {
        self.frame = self.dataset.set_values(&values).unwrap();
    }
}

fn cells(dataset: &Dataset, frame: &Frame, column: &str) -> Vec<Option<String>> {
    let index = frame
        .columns
        .iter()
        .position(|c| c.name == column)
        .unwrap_or_else(|| panic!("no column {column}"));
    let rows = frame.rows.unwrap_or(10_000);
    let page = dataset.fetch(frame, 0, rows).unwrap();
    page.rows
        .into_iter()
        .map(|row| row[index].clone())
        .collect()
}

fn some(values: &[&str]) -> Vec<Option<String>> {
    values.iter().map(|v| Some(v.to_string())).collect()
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
        "da-read-values-{}-{}{suffix}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
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
    open_path(&messy(name), OpenOptions::default())
}

/// Writes `bytes` to a temp file with extension `ext` and opens it.
fn open_bytes(bytes: &[u8], ext: &str, options: OpenOptions) -> Opened {
    let file = temp_path(ext);
    std::fs::write(&file, bytes).unwrap();
    let mut opened = open_path(&file, options);
    opened.written = Some(file);
    opened
}

fn open_csv(text: &str) -> Opened {
    open_bytes(text.as_bytes(), ".csv", OpenOptions::default())
}

/// `ValueOptions` with one column's type chosen.
fn typed(column: &str, read_as: ReadAs) -> ValueOptions {
    let mut values = ValueOptions::default();
    values.types.insert(column.into(), read_as);
    values
}

#[test]
fn decimal_commas() {
    let o = open("02_semicolon_european.csv");
    assert_eq!(o.sql_type("Einzelpreis"), "DOUBLE");
    assert_eq!(o.sql_type("Umsatz"), "DOUBLE");
    assert_eq!(o.cells("Einzelpreis")[0].as_deref(), Some("1.75"));
    assert_eq!(o.cells("Umsatz")[..2], some(&["647.5", "1424.5"]));
    assert_eq!(o.notes("Einzelpreis"), ["decimal comma"]);
    assert_eq!(o.notes("Umsatz"), ["decimal comma"]);
    // `24.03.2026` dates: DuckDB read them before; now the cleaner does.
    assert_eq!(o.sql_type("Datum"), "DATE");
    assert_eq!(o.cells("Datum")[0].as_deref(), Some("2026-03-24"));
    assert_eq!(o.notes("Datum"), ["read day first"]);
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Read 2 text columns as numbers and 1 as dates.")
    );
}

#[test]
fn currency_and_percent() {
    let o = open("03_currency_percent.csv");
    for column in ["revenue", "cost", "headcount"] {
        assert_eq!(o.sql_type(column), "BIGINT", "{column}");
    }
    for column in ["margin_pct", "yoy_growth"] {
        assert_eq!(o.sql_type(column), "DOUBLE", "{column}");
    }
    // Shown as the file wrote them.
    assert_eq!(o.cells("revenue")[0].as_deref(), Some("$723,278.00"));
    assert_eq!(o.cells("revenue")[3].as_deref(), Some("$1,126,243.00"));
    assert_eq!(o.cells("cost")[0].as_deref(), Some("$632,085"));
    assert_eq!(o.cells("margin_pct")[0].as_deref(), Some("12.6%"));
    // `(7.6%)` is negative.
    assert_eq!(o.cells("yoy_growth")[6].as_deref(), Some("-7.6%"));
    assert_eq!(o.notes("yoy_growth"), ["(x) read as negative"]);
    // SQL sees plain numbers: fractions for percents.
    assert_eq!(
        o.scalar("SELECT revenue FROM data LIMIT 1").as_deref(),
        Some("723278")
    );
    assert_eq!(
        o.scalar("SELECT yoy_growth FROM data WHERE account = 'ACC-006'")
            .as_deref(),
        Some("-0.076")
    );
    assert_eq!(
        o.scalar("SELECT count(*) - count(headcount) FROM data")
            .as_deref(),
        Some("5")
    );
    assert_eq!(o.notes("headcount"), ["N/A, - and TBD read as empty"]);
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Read 5 text columns as numbers. N/A, - and TBD read as empty.")
    );

    // The format follows the column into sorted frames, sorting by number...
    let sorted = o
        .dataset
        .apply(
            &o.frame,
            &ViewSpec {
                sort: vec![SortKey {
                    column: "revenue".into(),
                    descending: true,
                }],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        cells(&o.dataset, &sorted, "revenue")[0].as_deref(),
        Some("$2,389,026.00")
    );
    // ...but not onto SQL results.
    let SqlOutcome::Rows(result) = o.dataset.query("SELECT revenue FROM data").unwrap() else {
        panic!("no rows");
    };
    assert_eq!(result.columns[0].format, None);
    assert_eq!(
        cells(&o.dataset, &result, "revenue")[0].as_deref(),
        Some("723278")
    );
}

#[test]
fn a_percent_column_in_the_second_table() {
    let o = open_path(
        &messy("04_two_tables.csv"),
        OpenOptions {
            part: Some(1),
            ..Default::default()
        },
    );
    assert_eq!(o.sql_type("Return Rate"), "DOUBLE");
    assert_eq!(o.cells("Return Rate")[0].as_deref(), Some("0.10%"));
    assert_eq!(
        o.scalar(r#"SELECT "Return Rate" FROM data LIMIT 1"#)
            .as_deref(),
        Some("0.001")
    );
}

#[test]
fn windows_1252_opens() {
    let o = open("08_latin1.csv");
    assert_eq!(o.frame.rows, Some(12));
    assert_eq!(o.sql_type("kunde"), "VARCHAR");
    assert_eq!(o.sql_type("stadt"), "VARCHAR");
    assert_eq!(o.sql_type("betrag"), "DOUBLE");
    assert_eq!(o.cells("kunde")[0].as_deref(), Some("Jürgen Müller"));
    assert_eq!(o.cells("stadt")[0].as_deref(), Some("München"));
    assert_eq!(o.dataset.encoding(), Some(Encoding::Windows1252));
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Read as Windows-1252, not UTF-8.")
    );
}

#[test]
fn utf16_with_a_byte_order_mark_opens() {
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(
        "name,city\nZoë,Malmö\nJosé,A Coruña\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    let o = open_bytes(&bytes, ".csv", OpenOptions::default());
    assert_eq!(o.cells("city"), some(&["Malmö", "A Coruña"]));
    assert_eq!(o.dataset.encoding(), Some(Encoding::Utf16));
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Read as UTF-16, not UTF-8.")
    );
}

#[test]
fn bytes_that_are_not_utf8_late_in_a_big_file() {
    // More than the 4 MB checked at open, all ASCII, then one Latin-1 line.
    let mut bytes = b"id,city\n".to_vec();
    for i in 0..400_000 {
        bytes.extend(format!("{i},Leeds\n").as_bytes());
    }
    bytes.extend(b"400000,M\xfcnchen\n");
    let o = open_bytes(&bytes, ".csv", OpenOptions::default());
    assert_eq!(o.frame.rows, Some(400_001));
    assert_eq!(o.dataset.encoding(), Some(Encoding::Windows1252));
    assert_eq!(
        o.scalar("SELECT city FROM data WHERE id = 400000")
            .as_deref(),
        Some("München")
    );
}

#[test]
fn spaces_are_trimmed() {
    let o = open("09_bom_trailing_delims.csv");
    assert_eq!(o.cells("name")[..2], some(&["item 1", "item 2"]));
    assert_eq!(o.notes("name"), ["spaces trimmed"]);
    // Trimming is shown per column, not in the summary.
    assert_eq!(o.dataset.summary(), None);
}

#[test]
fn ids_dates_and_flags() {
    let o = open("10_ids_dates_flags.csv");
    // Leading zeros matter: these stay text.
    assert_eq!(o.sql_type("zip"), "VARCHAR");
    assert_eq!(o.sql_type("phone"), "VARCHAR");
    assert_eq!(o.sql_type("sku"), "VARCHAR");
    assert_eq!(o.cells("zip")[1].as_deref(), Some("00501"));
    assert_eq!(o.cells("phone")[0].as_deref(), Some("07901490718"));
    assert_eq!(o.cells("sku")[0].as_deref(), Some("000248"));

    // Three spellings in one column, month first (01/13/2025 proves it).
    assert_eq!(o.sql_type("signup"), "DATE");
    assert_eq!(
        o.cells("signup")[..3],
        some(&["2025-01-13", "2025-07-08", "2025-02-22"])
    );
    assert_eq!(o.notes("signup"), ["3 date spellings unified"]);

    assert_eq!(o.sql_type("active"), "BOOLEAN");
    assert_eq!(o.cells("active")[..2], some(&["true", "false"]));

    // Day first: 24/03/2025 has a day above 12.
    assert_eq!(o.sql_type("last_seen"), "DATE");
    assert_eq!(o.cells("last_seen")[2].as_deref(), Some("2025-03-24"));
    assert_eq!(o.cells("last_seen")[0].as_deref(), Some("2025-02-12"));
    assert_eq!(o.notes("last_seen"), ["read day first"]);
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Read 2 text columns as dates and 1 as yes/no.")
    );
}

#[test]
fn formats_sheet() {
    let o = open_path(
        &messy("23_year_headers_and_formats.xlsx"),
        OpenOptions {
            part: Some(1),
            ..Default::default()
        },
    );
    assert_eq!(o.sql_type("revenue_text"), "BIGINT");
    assert_eq!(o.cells("revenue_text")[0].as_deref(), Some("358672"));
    assert_eq!(o.sql_type("ratio"), "DOUBLE");
    // `#DIV/0!` on rows 0, 5 and 10.
    assert_eq!(o.cells("ratio")[..2], [None, Some("2.46".into())]);
    assert_eq!(o.notes("ratio"), ["#DIV/0! read as empty"]);
    assert_eq!(o.sql_type("closed_on"), "DATE");
    assert_eq!(o.cells("closed_on")[1].as_deref(), Some("2026-06-17"));
    assert_eq!(o.notes("closed_on"), ["read day first"]);
}

#[test]
fn forcing_a_type() {
    let mut o = open("10_ids_dates_flags.csv");
    // Every zip fits a number; asking doesn't change anything.
    assert_eq!(
        o.dataset.fit("zip", ReadAs::Number).unwrap(),
        Fit {
            values: 24,
            fit: 24
        }
    );
    assert_eq!(o.sql_type("zip"), "VARCHAR");
    o.set(typed("zip", ReadAs::Number));
    assert_eq!(o.sql_type("zip"), "BIGINT");
    assert_eq!(o.cells("zip")[..2], some(&["10001", "501"]));
    assert_eq!(o.dataset.values(), typed("zip", ReadAs::Number));
    // Back to automatic: text again, as loaded.
    o.set(ValueOptions::default());
    assert_eq!(o.cells("zip")[1].as_deref(), Some("00501"));

    // A column of words: nothing fits, and forcing it leaves it empty.
    let mut o = open("03_currency_percent.csv");
    assert_eq!(
        o.dataset.fit("region", ReadAs::Number).unwrap(),
        Fit { values: 30, fit: 0 }
    );
    o.set(typed("region", ReadAs::Number));
    assert!(o.cells("region").iter().all(Option::is_none));
    assert_eq!(o.notes("region"), ["30 values didn't fit and are empty"]);
    // Forcing text shows a converted column as written.
    o.set(typed("revenue", ReadAs::Text));
    assert_eq!(o.sql_type("revenue"), "VARCHAR");
    assert_eq!(o.cells("revenue")[0].as_deref(), Some("$723,278.00"));
    assert_eq!(o.column("revenue").format, None);
}

#[test]
fn an_ambiguous_date_order_is_a_guess_the_user_can_flip() {
    let mut o = open_csv("when,n\n03/04/2025,1\n05/06/2025,2\nN/A,3\n");
    assert_eq!(
        o.cells("when"),
        [Some("2025-03-04".into()), Some("2025-05-06".into()), None]
    );
    assert_eq!(
        o.notes("when"),
        ["month/day order is a guess", "N/A read as empty"]
    );
    assert_eq!(
        o.conversion("when").unwrap().read_as,
        ReadAs::MonthFirstDate
    );
    o.set(typed("when", ReadAs::DayFirstDate));
    assert_eq!(o.cells("when")[..2], some(&["2025-04-03", "2025-06-05"]));
}

#[test]
fn slash_dates_in_a_plain_file() {
    // DuckDB would guess the order on its own; the cleaner reads month first and says so.
    let mut o = open_csv("when,n\n03/04/2025,1\n05/06/2025,2\n");
    assert_eq!(o.cells("when"), some(&["2025-03-04", "2025-05-06"]));
    assert_eq!(o.notes("when"), ["month/day order is a guess"]);
    o.set(typed("when", ReadAs::DayFirstDate));
    assert_eq!(o.cells("when"), some(&["2025-04-03", "2025-06-05"]));
    // A day above 12 settles it. Beside dates DuckDB would read day first,
    // it reads ISO dates as timestamps, so the cleaner reads them as dates.
    let o = open_csv("when,iso\n24/03/2025,2024-01-05\n05/06/2025,2024-02-06\n");
    assert_eq!(o.cells("when"), some(&["2025-03-24", "2025-06-05"]));
    assert_eq!(o.sql_type("iso"), "DATE");
    assert_eq!(o.cells("iso"), some(&["2024-01-05", "2024-02-06"]));
    assert_eq!(o.notes("iso"), Vec::<String>::new());
    // On their own, ISO dates are DuckDB's as before.
    let o = open_csv("n,iso\n1,2024-01-05\n2,2024-02-06\n");
    assert_eq!(o.sql_type("iso"), "DATE");
    assert!(o.conversion("iso").is_none());
}

#[test]
fn the_decimal_mark_for_the_file() {
    // `1,5` is a decimal comma on its own.
    let o = open_csv("x;y\n1,5;a\n2,25;b\n");
    assert_eq!(o.cells("x"), some(&["1.5", "2.25"]));
    // `1,500` could be either: in a file split on semicolons, a decimal
    // comma, as a guess the user can change; a point read as said.
    let text = "x;y\n1,500;a\n2,250;b\n";
    let o = open_csv(text);
    assert_eq!(o.cells("x"), some(&["1.5", "2.25"]));
    assert_eq!(o.notes("x"), ["decimal comma, a guess"]);
    let o = open_bytes(
        text.as_bytes(),
        ".csv",
        OpenOptions {
            values: ValueOptions {
                decimal: Decimal::Point,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    assert_eq!(o.cells("x"), some(&["1500", "2250"]));
    assert_eq!(o.notes("x"), Vec::<String>::new());
    // Another column with a decimal comma settles it for the file.
    let o = open_csv("x;y;z;w\n1,500;2,75;a;b\n2,250;3,5;c;d\n");
    assert_eq!(o.cells("x"), some(&["1.5", "2.25"]));
    // In a file split on commas, a quoted `"1,5"` may be two values: text,
    // unless the user says the file writes decimal commas.
    let text = b"x,y\n\"1,5\",a\n\"2,5\",b\n";
    let o = open_bytes(text, ".csv", OpenOptions::default());
    assert_eq!(o.cells("x"), some(&["1,5", "2,5"]));
    let o = open_bytes(
        text,
        ".csv",
        OpenOptions {
            values: ValueOptions {
                decimal: Decimal::Comma,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    assert_eq!(o.cells("x"), some(&["1.5", "2.5"]));
}

#[test]
fn an_encoding_override_reads_utf8_as_windows_1252() {
    let o = open_bytes(
        "name,city\nJürgen,München\n".as_bytes(),
        ".csv",
        OpenOptions {
            encoding: Encoding::Windows1252,
            ..Default::default()
        },
    );
    assert_eq!(o.cells("city"), some(&["MÃ¼nchen"]));
}

#[test]
fn choices_survive_the_load_and_apply_to_unscanned_files() {
    // Over 4 MB, so the finder doesn't scan it and there is no plan.
    let mut text = String::from("zip,rate\n");
    for i in 0..400_000 {
        text.push_str(&format!("{:05},{}.5%\n", i % 99_999, i % 90));
    }
    let file = temp_path(".csv");
    std::fs::write(&file, &text).unwrap();
    let cache = temp_path("");
    let dataset = Dataset::open(
        &file,
        &OpenOptions {
            cache_dir: cache.clone(),
            values: typed("zip", ReadAs::Number),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(dataset.plan().is_none());
    // The preview already reads them so.
    let preview = dataset.frame().unwrap();
    let types = |frame: &Frame| -> Vec<String> {
        frame.columns.iter().map(|c| c.sql_type.clone()).collect()
    };
    assert_eq!(types(&preview), ["BIGINT", "DOUBLE"]);
    let loaded = dataset.materialize(&|_| {}).unwrap();
    assert_eq!(types(&loaded), ["BIGINT", "DOUBLE"]);
    assert_eq!(
        dataset.fetch(&loaded, 1, 1).unwrap().rows[0],
        [Some("1".into()), Some("1.5%".into())]
    );
    drop(dataset);
    let _ = std::fs::remove_dir_all(cache);
    let _ = std::fs::remove_file(file);
}

#[test]
fn json_values_are_cleaned_too() {
    let o = open_bytes(
        br#"[{"name": "a", "price": "$1,200.00"}, {"name": "b", "price": "N/A"}, {"name": "c", "price": "$3.50"}]"#,
        ".json",
        OpenOptions::default(),
    );
    assert_eq!(o.sql_type("price"), "DOUBLE");
    assert_eq!(
        o.cells("price"),
        [Some("$1,200.00".into()), None, Some("$3.50".into())]
    );
}

// --- Shapes that must not convert, or not wrongly ------------------------

#[test]
fn one_late_misfit_keeps_the_column_text_and_intact() {
    // 9,000 numbers, more than the sample, then a word.
    let mut text = String::from("id,amount\n");
    for i in 0..9_000 {
        text.push_str(&format!("{i},${i}.00\n"));
    }
    text.push_str("9000,call me\n");
    let o = open_csv(&text);
    assert_eq!(o.sql_type("amount"), "VARCHAR");
    assert!(o.conversion("amount").is_none());
    assert_eq!(o.cells("amount")[8_999..], some(&["$8999.00", "call me"]));

    // The same with the misfit inside the sample: 999 numbers and a word.
    let mut text = String::from("amount\n");
    for i in 0..999 {
        text.push_str(&format!("${i}.50\n"));
    }
    text.push_str("pending\n");
    let o = open_csv(&text);
    assert_eq!(o.sql_type("amount"), "VARCHAR");
    assert_eq!(o.cells("amount")[999].as_deref(), Some("pending"));
}

#[test]
fn ids_that_only_look_numeric_at_first_stay_text() {
    // A leading zero, or too many digits, past the sample. The `N/A` on
    // top makes DuckDB read the column as text, so the cleaner decides.
    for late in ["00042", "1234567890123456"] {
        let mut text = String::from("code\nN/A\n");
        for i in 0..6_000 {
            text.push_str(&format!("{}\n", 10_000 + i));
        }
        text.push_str(&format!("{late}\n"));
        let o = open_csv(&text);
        assert_eq!(o.sql_type("code"), "VARCHAR", "{late}");
        assert!(o.conversion("code").is_none(), "{late}");
        assert_eq!(o.cells("code")[6_001].as_deref(), Some(late));
    }
}

#[test]
fn a_late_day_above_12_reads_the_column_day_first() {
    // The sample can't tell the order; the whole column can.
    let mut text = String::from("day\nN/A\n");
    for _ in 0..6_000 {
        text.push_str("01/02/2025\n");
    }
    text.push_str("24/03/2025\n");
    let o = open_csv(&text);
    assert_eq!(o.sql_type("day"), "DATE");
    assert_eq!(o.cells("day")[1].as_deref(), Some("2025-02-01"));
    assert_eq!(o.cells("day")[6_001].as_deref(), Some("2025-03-24"));
    assert_eq!(o.notes("day"), ["read day first", "N/A read as empty"]);
}

#[test]
fn codes_phone_numbers_and_versions_stay_text() {
    let o = open_csv(
        "phone,code,frac,version,money,blank\n\
         555-0100,1-2,3/4,1.2.3,$5,\n\
         555-0199,3-4,1/2,2.0.1,€6,\n\
         555-0142,10-12,5/8,10.4.0,$7,\n",
    );
    for column in ["phone", "code", "frac", "version", "money", "blank"] {
        assert_eq!(o.sql_type(column), "VARCHAR", "{column}");
        assert!(o.conversion(column).is_none(), "{column}");
    }
    assert_eq!(o.cells("code")[0].as_deref(), Some("1-2"));
    assert_eq!(o.cells("money")[1].as_deref(), Some("€6"));
    assert!(o.cells("blank").iter().all(Option::is_none));
    assert_eq!(o.dataset.summary(), None);
}

#[test]
fn a_dash_in_a_text_column_stays_a_dash() {
    let o = open_csv("fruit,n\napple,1\n-,2\npear,3\n");
    assert_eq!(o.cells("fruit"), some(&["apple", "-", "pear"]));
}

#[test]
fn scientific_notation_reads_as_a_number() {
    let o = open_csv("x\n1.5e3\nN/A\n2E-2\n");
    assert_eq!(o.sql_type("x"), "DOUBLE");
    assert_eq!(o.column("x").format, None);
    assert_eq!(
        o.cells("x"),
        [Some("1500".into()), None, Some("0.02".into())]
    );
}

#[test]
fn an_exponent_with_a_currency_or_percent_sign_stays_text() {
    // Shown as `$0` and `0%`, a filter typed from the grid would find zero.
    let o = open_csv("id,cash,pct\n1,$1e-3,1e-3%\n2,$2e-3,2e-3%\n");
    for column in ["cash", "pct"] {
        assert_eq!(o.sql_type(column), "VARCHAR", "{column}");
        assert!(o.conversion(column).is_none(), "{column}");
    }
    assert_eq!(o.cells("cash"), some(&["$1e-3", "$2e-3"]));
    assert_eq!(o.cells("pct"), some(&["1e-3%", "2e-3%"]));
}

#[test]
fn amounts_too_precise_for_a_double_stay_text() {
    // 17 significant digits: the cents would be lost.
    let o = open_csv("id,cash\n1,$999999999999999.99\n2,$999999999999998.99\n");
    assert_eq!(o.sql_type("cash"), "VARCHAR");
    assert_eq!(
        o.cells("cash"),
        some(&["$999999999999999.99", "$999999999999998.99"])
    );
    // 15 digits as shown converts, trailing zeros and all.
    let o = open_csv("id,cash\n1,\"$1,234,567,890,123.00\"\n2,$0.50\n");
    assert_eq!(o.sql_type("cash"), "DOUBLE");
    assert_eq!(o.cells("cash"), some(&["$1,234,567,890,123.00", "$0.50"]));
    // Each value fits, but shown at the column's 2 decimals one has 16
    // digits, which a double can't show exactly.
    let o = open_csv("id,cash\n1,$99999999999999.9\n2,$0.01\n");
    assert_eq!(o.sql_type("cash"), "VARCHAR");
    assert_eq!(o.cells("cash"), some(&["$99999999999999.9", "$0.01"]));
    // Past the sample, where only the whole-column check sees it.
    let mut text = String::from("id,cash\n");
    for i in 0..6_000 {
        text.push_str(&format!("{i},$1.50\n"));
    }
    text.push_str("6000,$999999999999999.99\n");
    let o = open_csv(&text);
    assert_eq!(o.sql_type("cash"), "VARCHAR");
    assert_eq!(
        o.cells("cash")[6_000].as_deref(),
        Some("$999999999999999.99")
    );
}

#[test]
fn a_number_too_big_for_a_double_stays_text() {
    // Past the sample, where only the whole-column check sees it. The
    // `N/A` makes DuckDB read the column as text.
    let mut text = String::from("x\nN/A\n");
    for i in 0..6_000 {
        text.push_str(&format!("{i}\n"));
    }
    text.push_str("1e309\n");
    let o = open_csv(&text);
    assert_eq!(o.sql_type("x"), "VARCHAR");
    assert_eq!(o.cells("x")[6_001].as_deref(), Some("1e309"));
}

#[test]
fn forcing_a_number_leaves_out_what_a_double_cant_hold() {
    let mut o = open_csv("x\nN/A\n1.5\n1234567890123456\n1e309\n-1e309\n");
    assert_eq!(o.sql_type("x"), "VARCHAR");
    assert_eq!(
        o.dataset.fit("x", ReadAs::Number).unwrap(),
        Fit { values: 4, fit: 1 }
    );
    o.set(typed("x", ReadAs::Number));
    assert_eq!(o.cells("x"), [None, Some("1.5".into()), None, None, None]);
    assert_eq!(
        o.notes("x"),
        ["3 values didn't fit and are empty", "N/A read as empty"]
    );
    // A column the file already typed keeps every value.
    let mut o = open_csv("x\n0.30000000000000004\n2\n");
    assert_eq!(o.sql_type("x"), "DOUBLE");
    assert_eq!(
        o.dataset.fit("x", ReadAs::Number).unwrap(),
        Fit { values: 2, fit: 2 }
    );
    o.set(typed("x", ReadAs::Number));
    assert_eq!(
        o.scalar("SELECT x FROM data LIMIT 1").as_deref(),
        Some("0.3")
    );
    assert_eq!(o.dataset.conversions()[0].notes, Vec::<String>::new());
}

#[test]
fn awkward_column_names() {
    let o = open_csv(
        "\"select\",\"a \"\"quoted\"\" name\",\"x,y\"\n\"$1.50\",N/A,\"1,2\"\n\"$2.00\",7,\"3,4\"\n",
    );
    assert_eq!(o.cells("select"), some(&["$1.50", "$2.00"]));
    assert_eq!(o.sql_type("a \"quoted\" name"), "BIGINT");
    assert_eq!(o.cells("a \"quoted\" name"), [None, Some("7".into())]);
    // A column named with a comma is a line the reader may not have split,
    // so `1,2` there is not read as a decimal comma.
    assert_eq!(o.cells("x,y"), some(&["1,2", "3,4"]));
    let mut o = o;
    o.set(typed("select", ReadAs::Text));
    assert_eq!(o.cells("select")[0].as_deref(), Some("$1.50"));
}

#[test]
fn a_late_accounting_negative_still_converts() {
    // The sample sees no parentheses; the whole column does.
    let mut text = String::from("amount\n");
    for _ in 0..6_000 {
        text.push_str("\"$1,000.00\"\n");
    }
    text.push_str("($3.50)\n");
    let o = open_csv(&text);
    assert_eq!(o.sql_type("amount"), "DOUBLE");
    assert_eq!(o.cells("amount")[6_000].as_deref(), Some("-$3.50"));
    assert_eq!(o.notes("amount"), ["(x) read as negative"]);
}

#[test]
fn shapes_that_only_look_like_values_stay_text() {
    // Each column holds three values of one shape; the `N/A` row makes
    // DuckDB read them all as text, so the cleaner decides every one.
    let columns: &[(&str, [&str; 3])] = &[
        ("quarter", ["Q1 2025", "Q2 2025", "Q3 2025"]),
        ("time", ["10:30", "11:45", "08:00"]),
        (
            "iso_ts",
            ["2025-01-02T10:00", "2025-01-03T11:00", "2025-01-04T12:00"],
        ),
        ("list", ["1,2,3", "4,5,6", "7,8,9"]),
        ("date_like_code", ["12-34-5678", "03-11-7782", "05-12-9999"]),
        ("hex", ["0x1F", "0x20", "0xFF"]),
        ("bad_groups", ["$1,23", "$4,56", "$7,89"]),
        (
            "phone_intl",
            ["+44 20 7946 0958", "+1 555 0100", "+49 30 1234"],
        ),
        (
            "phone_us",
            ["(555) 123-4567", "(555) 987-6543", "(555) 000-1111"],
        ),
        ("units", ["100 kg", "20 kg", "3 kg"]),
        ("ordinal", ["1st", "2nd", "3rd"]),
        ("space_thousands", ["12 345", "1 000", "999 999"]),
        ("version", ["1.2.3", "10.0.1", "2.4.6"]),
        ("zip", ["10001", "02134", "94107"]),
        (
            "long_id",
            ["1234567890123456", "2234567890123456", "3234567890123456"],
        ),
        ("two_currencies", ["$5", "€6", "$7"]),
        ("some_percent", ["12%", "0.5", "7%"]),
        ("yes_no_maybe", ["Yes", "No", "Maybe"]),
        (
            "unknown_month",
            ["Sept 8, 2025", "Oct 1, 2025", "Nov 2, 2025"],
        ),
    ];
    let quote = |v: &str| format!("\"{}\"", v.replace('"', "\"\""));
    let row = |cells: Vec<String>| cells.join(",") + "\n";
    let mut text = row(columns.iter().map(|(name, _)| name.to_string()).collect());
    text += &row(columns.iter().map(|_| "N/A".to_string()).collect());
    for i in 0..3 {
        text += &row(columns.iter().map(|(_, values)| quote(values[i])).collect());
    }
    let o = open_csv(&text);
    assert_eq!(o.dataset.conversions(), []);
    for (name, values) in columns {
        assert_eq!(o.sql_type(name), "VARCHAR", "{name}");
        assert_eq!(o.cells(name)[1..], some(values), "{name}");
    }
}

#[test]
fn one_late_unpadded_value_keeps_the_spaces() {
    // 5,000 padded values, the whole sample, then one that isn't.
    let mut text = String::from("id,name\n");
    for i in 0..5_000 {
        text.push_str(&format!("{i}, item \n"));
    }
    text.push_str("5000,plain\n");
    let o = open_csv(&text);
    assert!(o.conversion("name").is_none());
    assert_eq!(o.cells("name")[4_999..], some(&[" item ", "plain"]));
}

#[test]
fn a_late_value_with_more_decimals_shows_them() {
    // Past the sample, a value written with more decimals than the rest.
    for (first, late, stored) in [("$1", "$1.23", 1.23), ("12%", "12.55%", 0.1255)] {
        let mut text = String::from("id,x\n");
        for i in 0..5_000 {
            text.push_str(&format!("{i},{first}\n"));
        }
        text.push_str(&format!("5000,{late}\n"));
        let o = open_csv(&text);
        let format = o.column("x").format.expect("a display format");
        let shown = o.cells("x");
        assert_eq!(shown[5_000].as_deref(), Some(late), "{late}");
        // What the grid shows reads back as what SQL sees.
        let sql: f64 = o
            .scalar("SELECT x FROM data WHERE id = 5000")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(sql, stored);
        assert_eq!(format.parse(late), Some(sql), "{late}");
        assert_eq!(format.parse(&format.render(sql)), Some(sql), "{late}");
        // The others show at the column's precision.
        let first_shown = format.render(format.parse(first).unwrap());
        assert_eq!(shown[0].as_deref(), Some(first_shown.as_str()));
        assert_ne!(first_shown, first);
    }
}

#[test]
fn a_decimal_comma_reads_columns_duckdb_would_type() {
    let with = |decimal| OpenOptions {
        values: ValueOptions {
            decimal,
            ..Default::default()
        },
        ..Default::default()
    };
    let tsv = b"item\tamount\na\t1.234\nb\t2.345\n";
    let o = open_bytes(tsv, ".tsv", with(Decimal::Comma));
    assert_eq!(o.sql_type("amount"), "BIGINT");
    assert_eq!(o.cells("amount"), some(&["1234", "2345"]));
    // A point, said or not, reads them as DuckDB does.
    for decimal in [Decimal::Point, Decimal::Auto] {
        let o = open_bytes(tsv, ".tsv", with(decimal));
        assert_eq!(o.sql_type("amount"), "DOUBLE");
        assert_eq!(o.cells("amount"), some(&["1.234", "2.345"]));
    }

    let semi = b"item;amount;small\na;1.234,50;7,5\nb;2.000,00;8,25\n";
    let o = open_bytes(semi, ".csv", with(Decimal::Comma));
    assert_eq!(o.cells("amount"), some(&["1234.5", "2000"]));
    assert_eq!(o.cells("small"), some(&["7.5", "8.25"]));

    // Timestamps, dates and booleans keep the types DuckDB gives them.
    let mixed = b"item;when;amount;flag;day\n\
        a;2025-01-02 10:00:00;1.234;true;2025-01-02\n\
        b;2025-03-04 11:30:00;2.345;false;2025-02-03\n";
    let o = open_bytes(mixed, ".csv", with(Decimal::Comma));
    assert_eq!(o.sql_type("when"), "TIMESTAMP");
    assert_eq!(o.sql_type("flag"), "BOOLEAN");
    assert_eq!(o.sql_type("day"), "DATE");
    assert_eq!(o.cells("amount"), some(&["1234", "2345"]));
}

// --- Dates with times, two-digit years, encodings, digits and guesses ----

#[test]
fn date_times_follow_the_same_order_rule_as_dates() {
    // DuckDB would read `when` day first and `d` month first, silently.
    let text = "id,when,d\n1,03/04/2025 10:30:00,03/04/2025\n2,05/06/2025 11:00:00,05/06/2025\n";
    let mut o = open_csv(text);
    assert_eq!(o.sql_type("when"), "TIMESTAMP");
    assert_eq!(
        o.cells("when"),
        some(&["2025-03-04 10:30:00", "2025-05-06 11:00:00"])
    );
    assert_eq!(o.cells("d"), some(&["2025-03-04", "2025-05-06"]));
    for column in ["when", "d"] {
        let conversion = o.conversion(column).unwrap();
        assert_eq!(conversion.read_as, ReadAs::MonthFirstDate, "{column}");
        assert_eq!(conversion.guessed, Some(Guessed::DateOrder), "{column}");
        assert_eq!(conversion.notes, ["month/day order is a guess"], "{column}");
    }
    // Either order fits, and the user can flip it.
    for read_as in [ReadAs::MonthFirstDate, ReadAs::DayFirstDate] {
        assert_eq!(
            o.dataset.fit("when", read_as).unwrap(),
            Fit { values: 2, fit: 2 }
        );
    }
    o.set(typed("when", ReadAs::DayFirstDate));
    assert_eq!(o.sql_type("when"), "TIMESTAMP");
    assert_eq!(
        o.cells("when"),
        some(&["2025-04-03 10:30:00", "2025-06-05 11:00:00"])
    );

    // A day above 12 proves the order; minutes, fractions and AM/PM read too.
    let o = open_csv(
        "a,b,c\n\
         24/03/2025 10:30,3/4/2025 9:15:00 AM,03.04.2025 10:30:00.250\n\
         05/06/2025 23:05,5/6/2025 11:00:00 PM,05.06.2025 11:00:00.5\n",
    );
    assert_eq!(
        o.cells("a"),
        some(&["2025-03-24 10:30:00", "2025-06-05 23:05:00"])
    );
    assert_eq!(o.notes("a"), ["read day first"]);
    assert_eq!(o.conversion("a").unwrap().guessed, None);
    assert_eq!(
        o.cells("b"),
        some(&["2025-03-04 09:15:00", "2025-05-06 23:00:00"])
    );
    // Dotted dates are a day-first habit, still a guess.
    assert_eq!(
        o.cells("c"),
        some(&["2025-04-03 10:30:00.25", "2025-06-05 11:00:00.5"])
    );
    assert_eq!(o.conversion("c").unwrap().guessed, Some(Guessed::DateOrder));
}

#[test]
fn year_first_dates_are_duckdbs_as_before() {
    let o = open_csv(
        "id,slash,iso_z,offset,plain\n\
         1,2025/01/02,2025-01-05T10:00:00Z,2025-01-05 10:00:00+02:00,2025-01-05 10:00:00\n\
         2,2025/03/24,2025-01-22T10:00:00.5Z,2025-01-06 11:00:00-05:30,2025-01-06 11:30:00\n",
    );
    assert_eq!(o.sql_type("slash"), "DATE");
    assert_eq!(o.cells("slash"), some(&["2025-01-02", "2025-03-24"]));
    assert_eq!(o.sql_type("iso_z"), "TIMESTAMP WITH TIME ZONE");
    assert_eq!(o.sql_type("offset"), "TIMESTAMP WITH TIME ZONE");
    assert_eq!(o.sql_type("plain"), "TIMESTAMP");
    assert_eq!(o.dataset.conversions(), []);
    assert_eq!(o.dataset.summary(), None);
}

#[test]
fn two_digit_years_are_dates() {
    // `01/22/25` proves month first; DuckDB's `%y` pivot reads 69 as 1969.
    let o = open_csv("d,n\n01/05/25,1\n01/22/25,2\n12/31/69,3\n");
    assert_eq!(o.sql_type("d"), "DATE");
    assert_eq!(
        o.cells("d"),
        some(&["2025-01-05", "2025-01-22", "1969-12-31"])
    );
    assert_eq!(o.conversion("d").unwrap().guessed, None);
    // No day above 12: a guess, flippable; with times, timestamps.
    let mut o = open_csv("d,t\n03/04/25,03-04-25 10:30\n05/06/25,05-06-25 11:00\n");
    assert_eq!(o.cells("d"), some(&["2025-03-04", "2025-05-06"]));
    assert_eq!(o.conversion("d").unwrap().guessed, Some(Guessed::DateOrder));
    assert_eq!(o.sql_type("t"), "TIMESTAMP");
    assert_eq!(
        o.cells("t"),
        some(&["2025-03-04 10:30:00", "2025-05-06 11:00:00"])
    );
    assert_eq!(
        o.dataset.fit("d", ReadAs::DayFirstDate).unwrap(),
        Fit { values: 2, fit: 2 }
    );
    o.set(typed("d", ReadAs::DayFirstDate));
    assert_eq!(o.cells("d"), some(&["2025-04-03", "2025-06-05"]));
    // Beside four-digit years in one column, each reads as written.
    let o = open_csv("d\nN/A\n24.03.25\n25.03.2025\n");
    assert_eq!(
        o.cells("d"),
        [None, Some("2025-03-24".into()), Some("2025-03-25".into())]
    );
}

#[test]
fn a_stray_line_in_another_encoding_keeps_the_rest_utf8() {
    // More than the 4 MB checked at open, then a Latin-1 line, then UTF-8 again.
    let mut bytes = b"id,name,city\n".to_vec();
    let mut id = 0;
    while bytes.len() < 4_300_000 {
        bytes.extend(format!("{id},Jürgen Müller,München\n").as_bytes());
        id += 1;
    }
    let stray = id;
    bytes.extend(format!("{stray},").as_bytes());
    bytes.extend(b"Jos\xe9 Pe\xf1a,A Coru\xf1a\n");
    for i in 1..=1_000 {
        bytes.extend(format!("{},Zoë,Malmö\n", stray + i).as_bytes());
    }
    let o = open_bytes(&bytes, ".csv", OpenOptions::default());
    assert_eq!(o.frame.rows, Some(stray + 1_001));
    assert_eq!(o.dataset.encoding(), Some(Encoding::Utf8));
    let name = |id: u64| {
        o.scalar(&format!(
            "SELECT name || ' ' || city FROM data WHERE id = {id}"
        ))
    };
    assert_eq!(name(0).as_deref(), Some("Jürgen Müller München"));
    assert_eq!(name(stray).as_deref(), Some("José Peña A Coruña"));
    assert_eq!(name(stray + 1_000).as_deref(), Some("Zoë Malmö"));
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Read 3 bytes that aren't UTF-8 as Windows-1252.")
    );

    // Mostly UTF-8 within the bytes checked at open: mended there too.
    let mut small = "name,city\nZoë,Malmö\nJürgen,München\n".as_bytes().to_vec();
    small.extend(b"Jos\xe9,Le\xf3n\n");
    let o = open_bytes(&small, ".csv", OpenOptions::default());
    assert_eq!(o.cells("city"), some(&["Malmö", "München", "León"]));
    assert_eq!(o.dataset.encoding(), Some(Encoding::Utf8));
    assert_eq!(
        o.dataset.summary().as_deref(),
        Some("Read 2 bytes that aren't UTF-8 as Windows-1252.")
    );
    // Windows-1252 when chosen reads every byte that way.
    let o = open_bytes(
        &small,
        ".csv",
        OpenOptions {
            encoding: Encoding::Windows1252,
            ..Default::default()
        },
    );
    assert_eq!(o.cells("city")[0].as_deref(), Some("MalmÃ¶"));
    assert_eq!(o.cells("city")[2].as_deref(), Some("León"));
}

#[test]
fn utf16_without_a_byte_order_mark_opens() {
    let text = "name,city\nZoë,Malmö\nJosé,A Coruña\n";
    for big_endian in [false, true] {
        let bytes: Vec<u8> = text
            .encode_utf16()
            .flat_map(|u| {
                if big_endian {
                    u.to_be_bytes()
                } else {
                    u.to_le_bytes()
                }
            })
            .collect();
        let o = open_bytes(&bytes, ".csv", OpenOptions::default());
        assert_eq!(
            o.cells("city"),
            some(&["Malmö", "A Coruña"]),
            "{big_endian}"
        );
        assert_eq!(o.dataset.encoding(), Some(Encoding::Utf16));
    }
}

#[test]
fn plain_numbers_convert_only_when_shown_exactly() {
    // 13 significant digits would show as 12: these stay as written.
    let o = open_csv(
        "acct,balance,small,whole\nA,\"12,345,678,901.55\",\"1,234.5\",\"1,234,567,890,123\"\nB,\"98,765,432,109.87\",\"2,000.25\",\"2,000\"\n",
    );
    assert_eq!(o.sql_type("balance"), "VARCHAR");
    assert_eq!(
        o.cells("balance"),
        some(&["12,345,678,901.55", "98,765,432,109.87"])
    );
    assert_eq!(o.sql_type("small"), "DOUBLE");
    assert_eq!(o.cells("small"), some(&["1234.5", "2000.25"]));
    // Whole numbers are integers, shown in full.
    assert_eq!(o.sql_type("whole"), "BIGINT");
    assert_eq!(o.cells("whole"), some(&["1234567890123", "2000"]));
    // With a currency sign, the format shows every digit.
    let o = open_csv("cur\n\"$12,345,678,901.55\"\n$1.00\n");
    assert_eq!(o.cells("cur"), some(&["$12,345,678,901.55", "$1.00"]));
}

#[test]
fn a_forced_number_keeps_the_look_of_the_values_that_fit() {
    let mut text = String::from("id,amount\n");
    for i in 0..29 {
        text.push_str(&format!("{i},${i}.50\n"));
    }
    text.push_str("29,see note\n");
    let mut o = open_csv(&text);
    assert_eq!(o.sql_type("amount"), "VARCHAR");
    o.set(typed("amount", ReadAs::Number));
    assert_eq!(
        o.column("amount").format,
        Some(NumberFormat::Currency {
            symbol: '$',
            decimals: 2
        })
    );
    assert_eq!(o.cells("amount")[10].as_deref(), Some("$10.50"));
    assert_eq!(o.cells("amount")[29], None);
    assert_eq!(
        o.scalar("SELECT amount FROM data WHERE id = 10").as_deref(),
        Some("10.5")
    );

    let mut o = open("03_currency_percent.csv");
    o.set(ValueOptions {
        types: [
            ("revenue".to_string(), ReadAs::Number),
            ("margin_pct".to_string(), ReadAs::Number),
        ]
        .into(),
        ..Default::default()
    });
    assert_eq!(o.cells("revenue")[0].as_deref(), Some("$723,278.00"));
    assert_eq!(o.cells("margin_pct")[0].as_deref(), Some("12.6%"));
    // Values written more than one way: no look to keep.
    let mut o = open_csv("x\n$5\n€6\n7\n");
    o.set(typed("x", ReadAs::Number));
    assert_eq!(o.column("x").format, None);
    assert_eq!(o.cells("x"), some(&["5", "6", "7"]));
}

#[test]
fn a_comma_before_three_digits_is_a_guess() {
    // Split on semicolons, where a comma is the usual decimal mark.
    let o = open_csv("a;b\n1,125;x\n2,250;y\n3,375;z\n");
    assert_eq!(o.cells("a"), some(&["1.125", "2.25", "3.375"]));
    let conversion = o.conversion("a").unwrap();
    assert_eq!(conversion.notes, ["decimal comma, a guess"]);
    assert_eq!(conversion.guessed, Some(Guessed::DecimalMark));
    assert_eq!(o.dataset.decimal(), Decimal::Comma);
    // Elsewhere, thousands.
    let o = open_csv("a,b\n\"1,125\",x\n\"2,250\",y\n");
    assert_eq!(o.cells("a"), some(&["1125", "2250"]));
    assert_eq!(o.notes("a"), ["comma read as thousands, a guess"]);
    assert_eq!(
        o.conversion("a").unwrap().guessed,
        Some(Guessed::DecimalMark)
    );
    // A column that settles the mark settles it for the others.
    let o = open_csv("a,b\n\"1,125\",\"1,500.25\"\n\"2,250\",\"2,000.50\"\n");
    assert_eq!(o.cells("a"), some(&["1125", "2250"]));
    assert_eq!(o.notes("a"), Vec::<String>::new());
    assert_eq!(o.conversion("a").unwrap().guessed, None);
    assert_eq!(o.conversion("b").unwrap().guessed, None);
    // A mark chosen by the user is no guess.
    let o = open_bytes(
        b"a;b\n1,125;x\n2,250;y\n",
        ".csv",
        OpenOptions {
            values: ValueOptions {
                decimal: Decimal::Comma,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    assert_eq!(o.notes("a"), ["decimal comma"]);
    assert_eq!(o.conversion("a").unwrap().guessed, None);
}
