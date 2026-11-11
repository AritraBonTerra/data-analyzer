//! Plain CSV/TSV files must read exactly as they did before the table finder.
//! Each shape below is opened two ways: through `Dataset::open`, and through
//! DuckDB's `read_csv` on the same file with the fallback levels the app used
//! before (the old path). Both must give the same columns, types and cells,
//! and the import must have nothing to say about the file.
//!
//! The one change on purpose is the cleaner's: a text column whose values
//! are all numbers once placeholders like `N/A` are left out now reads as
//! numbers, and dates and date-times not written year first are read by the
//! cleaner, not DuckDB.
//! Those shapes are listed in [`CLEANED`]; the summary must say what was
//! read, and every other column in them must still read exactly as before.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use da_core::*;

/// Plain shapes people really have: (file name, contents). The name's
/// extension picks CSV or TSV.
const SHAPES: &[(&str, &str)] = &[
    // The last row misses its value.
    ("sparse_last.csv", "name,amount\nAlice,10\nBob,20\nCarol,\n"),
    (
        "sparse_last_two.csv",
        "name,amount\nAlice,10\nBob,20\nCarol,\nDan,\n",
    ),
    (
        "sparse_last_no_newline.csv",
        "name,amount\nAlice,10\nBob,20\nCarol,",
    ),
    ("sparse_last_middle.csv", "a,b,c\nx,1,y\np,,q\n"),
    (
        "sparse_last_wide.csv",
        "id,name,qty,price\n1,ann,2,3.5\n2,bob,4,1.25\n3,,,\n",
    ),
    ("sparse_all_body.csv", "id,note\n1,\n2,\n3,\n"),
    (
        "sparse_first_data_row.csv",
        "name,a,b\nAlice,,\nBob,1,2\nCarol,3,4\n",
    ),
    ("sparse_middle_row.csv", "name,a,b\nx,1,2\ny,,\nz,3,4\n"),
    (
        "sparse_text_last.csv",
        "name,city\nann,Leeds\nbob,Pune\ncy,\n",
    ),
    (
        "sparse_value_only_last.csv",
        "name,amount\nAlice,10\nBob,20\n,30\n",
    ),
    // Ordinary text in a numeric column.
    (
        "na_first.csv",
        "name,amount\nAlice,N/A\nBob,10\nCarol,20\nDan,30\n",
    ),
    (
        "unknown_first.csv",
        "name,amount\nAlice,unknown\nBob,10\nCarol,20\nDan,30\n",
    ),
    (
        "pending_first.csv",
        "name,amount,qty\nAlice,pending,5\nBob,10,6\nCarol,20,7\nDan,30,8\n",
    ),
    (
        "dash_first.csv",
        "name,amount\nAlice,-\nBob,10\nCarol,20\nDan,30\n",
    ),
    (
        "na_middle.csv",
        "name,amount\nAlice,10\nBob,N/A\nCarol,20\nDan,30\n",
    ),
    (
        "first_row_all_text.csv",
        "name,amount,count\nAlice,unknown,missing\nBob,10,1\nCarol,20,2\nDan,30,3\n",
    ),
    (
        "first_two_rows_text.csv",
        "name,amount\nAlice,pending\nBob,unknown\nCarol,20\nDan,30\nEve,40\nFay,50\n",
    ),
    (
        "na_text_column.csv",
        "name,city,n\nN/A,Paris,1\nbob,Rome,2\ncy,Oslo,3\n",
    ),
    // Blank lines.
    (
        "row_after_blank.csv",
        "name,amount\nAlice,10\nBob,20\n\nCarol,30\n",
    ),
    (
        "blank_in_middle.csv",
        "id,name,score\n1,ann,10\n2,bob,20\n\n3,cy,30\n4,di,40\n",
    ),
    (
        "na_row_after_blank.csv",
        "name,amount\nAlice,10\nBob,20\n\nCarol,N/A\n",
    ),
    (
        "dash_row_after_blank.csv",
        "name,amount\nAlice,10\nBob,20\n\nCarol,-\n",
    ),
    (
        "tbd_row_after_blank.csv",
        "name,amount\nAlice,10\nBob,20\n\nCarol,TBD\n",
    ),
    ("trailing_blank.csv", "a,b\n1,2\n3,4\n\n"),
    ("trailing_blanks.csv", "a,b\n1,2\n3,4\n\n\n\n"),
    (
        "text_row_after_blank.csv",
        "first,last\nann,lee\nbob,ray\n\ncy,fox\n",
    ),
    // Headers.
    (
        "title_year.csv",
        "title,year\nAlien,1979\nHeat,1995\nRonin,1998\nDune,2021\n",
    ),
    ("year_first.csv", "year,value\n2019,10\n2020,12\n2021,15\n"),
    (
        "all_text.csv",
        "first,last,city\nann,lee,Leeds\nbob,ray,Pune\ncy,fox,Austin\n",
    ),
    ("no_header_numbers.csv", "1,2,3\n4,5,6\n7,8,9\n"),
    ("no_header_text_first.csv", "North,10\nSouth,20\nEast,30\n"),
    ("header_one_row.csv", "a,b\n1,2\n"),
    ("header_one_text_row.csv", "name,city\nAlice,Paris\n"),
    ("header_only.csv", "id,name\n"),
    ("single_column.csv", "name\nann\nbob\ncy\n"),
    ("single_column_numbers.csv", "n\n1\n2\n3\n"),
    // Quoting, delimiters, line ends.
    (
        "quoted.csv",
        "id,comment,n\n1,\"line one\nline two\",5\n2,\"plain\",6\n3,\"a, b\",7\n",
    ),
    ("quoted_empty.csv", "id,label\n1,\"\"\n2,\"x\"\n3,\"y\"\n"),
    ("semicolon.csv", "k;v\n\"a,b\";1\n\"c,d\";2\n"),
    (
        "semicolon_plain.csv",
        "name;amount;city\nann;10;Leeds\nbob;20;Pune\n",
    ),
    ("pipe.csv", "name|amount\nann|10\nbob|20\n"),
    ("tabs.tsv", "name\tamount\nann\t10\nbob\t20\n"),
    (
        "tabs_sparse_last.tsv",
        "name\tamount\nann\t10\nbob\t20\ncy\t\n",
    ),
    ("crlf.csv", "a,b\r\n1,x\r\n2,y\r\n"),
    (
        "crlf_sparse_last.csv",
        "name,amount\r\nAlice,10\r\nBob,\r\n",
    ),
    // Names and kinds of values.
    (
        "total_product.csv",
        "product,units\nTotal Care Shampoo,5\nSoap,3\nWax,2\n",
    ),
    (
        "total_product_last.csv",
        "product,region,units\nSoap,North,3\nTotal Care Shampoo,South,5\n",
    ),
    (
        "empty_named_column.csv",
        "id,comment,amount\n1,,10\n2,,20\n3,,30\n",
    ),
    (
        "empty_named_last_column.csv",
        "id,amount,comment\n1,10,\n2,20,\n3,30,\n",
    ),
    ("dates.csv", "date,value\n2024-01-01,5\n2024-01-02,6\n"),
    (
        "slash_dates.csv",
        "date,value\n03/04/2025,5\n05/06/2025,6\n",
    ),
    (
        "day_first_dates.csv",
        "date,value\n24/03/2025,5\n05/06/2025,6\n",
    ),
    // Dates and times with the year first: DuckDB's, exactly as before.
    (
        "year_first_slash.csv",
        "date,value\n2025/01/02,5\n2025/03/24,6\n",
    ),
    (
        "iso_t_z.csv",
        "at,v\n2025-01-05T10:00:00Z,1\n2025-01-22T10:00:00.5Z,2\n",
    ),
    (
        "iso_t.csv",
        "at,v\n2025-01-05T10:00:00,1\n2025-01-06T11:30:00,2\n",
    ),
    (
        "iso_offset.csv",
        "at,v\n2025-01-05 10:00:00+02:00,1\n2025-01-06 11:00:00-05:30,2\n",
    ),
    (
        "iso_fraction.csv",
        "at,v\n2025-01-05 10:00:00.123,1\n2025-01-06 11:00:00.5,2\n",
    ),
    (
        "slash_datetimes.csv",
        "at,v\n03/04/2025 10:30:00,1\n05/06/2025 11:00:00,2\n",
    ),
    ("two_digit_years.csv", "d,v\n01/05/25,1\n01/22/25,2\n"),
    ("booleans.csv", "id,active\n1,true\n2,false\n3,true\n"),
    ("decimals.csv", "item,price\na,-1.5\nb,2.25\nc,1e3\n"),
    ("numeric_names.csv", "sku,100,200\na,5,6\nb,7,8\n"),
    (
        "sentences.csv",
        "review,stars\nGreat product. Would buy again.,5\nNot bad,3\nNote: arrived late,\n",
    ),
    (
        "source_column.csv",
        "name,source\nann,Source: web\nbob,Source: shop\n",
    ),
    // More shapes in the same spirit: sparse edges, unnamed headers, padding.
    ("no_header_sparse_first.csv", "Alice,\nBob,10\nCarol,20\n"),
    ("header_unnamed_last.csv", "id,\n1,a\n2,b\n3,c\n"),
    ("header_unnamed_middle.csv", "id,,amount\n1,a,10\n2,b,20\n"),
    ("header_duplicates.csv", "a,a,b\n1,2,3\n4,5,6\n"),
    (
        "quoted_header.csv",
        "\"name\",\"amount\"\n\"ann\",1\n\"bob\",2\n",
    ),
    (
        "sparse_rows_after_blank.csv",
        "name,amount\nAlice,10\nBob,20\n\nCarol,30\nDan,40\n",
    ),
    (
        "sparse_row_after_blank.csv",
        "name,amount,qty\nAlice,10,1\nBob,20,2\n\nCarol,30,\n",
    ),
    ("sparse_and_blank.csv", "name,amount\nAlice,10\n\nBob,\n"),
    (
        "all_na_column.csv",
        "name,amount\nAlice,N/A\nBob,N/A\nCarol,N/A\n",
    ),
    (
        "mostly_na_column.csv",
        "name,amount\nAlice,N/A\nBob,N/A\nCarol,5\nDan,6\n",
    ),
    (
        "null_words.csv",
        "name,amount\nAlice,null\nBob,10\nCarol,20\n",
    ),
    (
        "tbd_first_two_values.csv",
        "name,q1,q2\nAlice,TBD,TBD\nBob,1,2\nCarol,3,4\n",
    ),
    (
        "text_code_first.csv",
        "code,amount\nX1,ten\nX2,20\nX3,30\nX4,40\n",
    ),
    ("trailing_comma_rows.csv", "a,b,\n1,2,\n3,4,\n"),
    ("one_data_row_sparse.csv", "name,amount\nAlice,\n"),
    ("whitespace_last_line.csv", "a,b\n1,2\n3,4\n   \n"),
    ("leading_spaces.csv", "name, amount\nann, 10\nbob, 20\n"),
    ("percent_column.csv", "name,rate\nann,12.5%\nbob,7%\n"),
    ("ids_with_zeros.csv", "id,name\n007,ann\n008,bob\n"),
    (
        "timestamps.csv",
        "at,v\n2024-01-01 10:00:00,1\n2024-01-02 11:30:00,2\n",
    ),
    (
        "key_value.csv",
        "key,value\ncolor,red\nsize,10\nweight,heavy\n",
    ),
    (
        "long_text_last.csv",
        "id,comment\n1,fine\n2,ok\n3,This one is a long sentence about the order.\n",
    ),
    (
        "padded_rows.csv",
        "name,amount,,\nAlice,10,,\nBob,20,,\nCarol,,,\n",
    ),
    (
        "note_padded_last.csv",
        "name,amount\nAlice,10\nBob,20\nNote: draft,\n",
    ),
    // Empty spacer columns: one table, not side-by-side tables or notes.
    ("spacer_third.csv", "a,b,,c\n1,x,,2\n3,y,,4\n"),
    ("spacer_second.csv", "a,,b,c\n1,,x,2\n3,,y,4\n"),
    ("spacer_two.csv", "a,,b,c,,d\n1,,x,2,,5\n3,,y,4,,6\n"),
    ("spacer_all_text.csv", "first,,last\nann,,lee\nbob,,ray\n"),
    (
        "spacer_before_comment.csv",
        "id,name,,comment\n1,ann,,ok\n2,bob,,\n3,cy,,fine\n",
    ),
    (
        "spacer_after_label.csv",
        "Name,,Jan,Feb,,Total\nAnn,,1,2,,3\nBob,,4,5,,9\n",
    ),
    (
        "spacer_between_halves.csv",
        "Region,Product,,Units,Revenue\nNorth,Oat,,10,100.5\nSouth,Tea,,20,200.25\nEast,Oat,,5,50\n",
    ),
    // A header that starts with `#` splits like the rows below it: not a comment.
    ("hash_header.csv", "#id,name,qty\n1,a,2\n2,b,3\n"),
];

/// Shapes in [`SHAPES`] that read differently from the old path on purpose,
/// and why. The test checks they still differ, so this list stays honest.
const DIFFERS: &[(&str, &str)] = &[
    (
        "trailing_comma_rows.csv",
        "a trailing comma on every line makes no empty, unnamed column",
    ),
    (
        "padded_rows.csv",
        "empty, unnamed columns from padding commas are dropped",
    ),
    (
        "sparse_and_blank.csv",
        "a one-cell line after a blank line is a note, not a row",
    ),
];

/// Shapes in [`SHAPES`] where the cleaner reads text columns, with the
/// columns it reads and why. Every other column must read as before.
const CLEANED: &[(&str, &[&str], &str)] = &[
    (
        "na_first.csv",
        &["amount"],
        "N/A is a placeholder among numbers",
    ),
    (
        "dash_first.csv",
        &["amount"],
        "- is a placeholder among numbers",
    ),
    (
        "na_middle.csv",
        &["amount"],
        "N/A is a placeholder among numbers",
    ),
    (
        "na_row_after_blank.csv",
        &["amount"],
        "N/A is a placeholder among numbers",
    ),
    (
        "dash_row_after_blank.csv",
        &["amount"],
        "- is a placeholder among numbers",
    ),
    (
        "tbd_row_after_blank.csv",
        &["amount"],
        "TBD is a placeholder among numbers",
    ),
    (
        "mostly_na_column.csv",
        &["amount"],
        "N/A is a placeholder among numbers",
    ),
    (
        "null_words.csv",
        &["amount"],
        "null is a placeholder among numbers",
    ),
    (
        "tbd_first_two_values.csv",
        &["q1", "q2"],
        "TBD is a placeholder among numbers",
    ),
    ("percent_column.csv", &["rate"], "12.5% is the number 0.125"),
    (
        "slash_dates.csv",
        &["date"],
        "with no day above 12 the order is a guess, month first, not DuckDB's day first",
    ),
    (
        "day_first_dates.csv",
        &["date"],
        "the same dates, now read by the cleaner and reported",
    ),
    (
        "slash_datetimes.csv",
        &["at"],
        "date-times follow the dates' rule: month first, a guess, not DuckDB's day first",
    ),
    (
        "two_digit_years.csv",
        &["d"],
        "the same dates, now read by the cleaner and reported",
    ),
];

/// The old path's levels: plain first, then tolerant
/// of ragged rows, then all text (see `Dataset::reader`).
const OLD_LEVELS: [&str; 3] = [
    "",
    ", sample_size = -1, strict_mode = false, null_padding = true",
    ", strict_mode = false, null_padding = true, all_varchar = true",
];

type Columns = Vec<(String, String)>;
type Cells = Vec<Vec<Option<String>>>;

/// A fresh temp path for one file or cache dir.
fn temp_path(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "da-plain-csv-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A frame's columns and every cell, as the app shows them.
fn read(dataset: &Dataset, frame: &Frame) -> (Columns, Cells) {
    let columns = frame
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.sql_type.clone()))
        .collect();
    let rows = frame.rows.unwrap();
    (columns, dataset.fetch(frame, 0, rows).unwrap().rows)
}

/// The old path: DuckDB's `read_csv` on the file in a fresh connection,
/// with cells rendered as [`Dataset::fetch`] renders them.
fn old_read(path: &Path) -> (Columns, Cells) {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let delim = if path.extension().is_some_and(|e| e == "tsv") {
        ", delim = '\\t'"
    } else {
        ""
    };
    let path = path.to_string_lossy().replace('\'', "''");
    let reader = OLD_LEVELS
        .iter()
        .map(|level| format!("read_csv('{path}'{delim}{level})"))
        .find(|reader| {
            conn.execute_batch(&format!(
                "CREATE OR REPLACE TABLE old AS SELECT * FROM {reader}"
            ))
            .is_ok()
        });
    assert!(reader.is_some(), "old reader failed on {path}");
    let mut stmt = conn.prepare("DESCRIBE SELECT * FROM old").unwrap();
    let columns: Columns = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let select = columns
        .iter()
        .map(|(name, sql_type)| {
            let col = format!("\"{}\"", name.replace('"', "\"\""));
            if matches!(sql_type.as_str(), "FLOAT" | "DOUBLE") {
                format!("printf('%.12g', {col})")
            } else {
                format!("CAST({col} AS VARCHAR)")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let width = columns.len();
    let cells = if width == 0 {
        Vec::new()
    } else {
        let mut stmt = conn.prepare(&format!("SELECT {select} FROM old")).unwrap();
        stmt.query_map([], |r| (0..width).map(|i| r.get(i)).collect())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    (columns, cells)
}

/// What is wrong with a cleaned shape's reading `new`, compared with the
/// old one: the same columns and rows, and every column but the `converted`
/// ones exactly as before. `None` when nothing is.
fn cleaned_problem(
    new: &(Columns, Cells),
    old: &(Columns, Cells),
    converted: &[&str],
) -> Option<String> {
    let names = |columns: &Columns| columns.iter().map(|c| c.0.clone()).collect::<Vec<_>>();
    if names(&new.0) != names(&old.0) || new.1.len() != old.1.len() {
        return Some("columns or rows differ".into());
    }
    for (i, (column, old_column)) in new.0.iter().zip(&old.0).enumerate() {
        let cells = |cells: &Cells| cells.iter().map(|row| row[i].clone()).collect::<Vec<_>>();
        let same = column == old_column && cells(&new.1) == cells(&old.1);
        if !same && !converted.contains(&column.0.as_str()) {
            return Some(format!("column {} changed", column.0));
        }
    }
    None
}

#[test]
fn plain_files_read_as_before() {
    let mut failures = Vec::new();
    for &(name, text) in SHAPES {
        let file = temp_path(name);
        let cache = temp_path("cache");
        std::fs::write(&file, text).unwrap();
        let dataset = Dataset::open(
            &file,
            &OpenOptions {
                cache_dir: cache.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        let frame = dataset.materialize(&|_| {}).unwrap();
        let new = read(&dataset, &frame);
        let old = old_read(&file);
        if let Some(&(_, converted, _)) = CLEANED.iter().find(|c| c.0 == name) {
            if let Some(problem) = cleaned_problem(&new, &old, converted) {
                failures.push(format!("{name}: {problem}\n  new: {new:?}\n  old: {old:?}"));
            }
            if !dataset.summary().is_some_and(|s| s.starts_with("Read ")) {
                failures.push(format!("{name}: no summary of the conversion"));
            }
        } else if DIFFERS.iter().any(|&(differs, _)| differs == name) {
            if new == old {
                failures.push(format!("{name} now reads as before: take it off DIFFERS"));
            }
        } else if new != old || dataset.summary().is_some() {
            failures.push(format!(
                "{name}\n  summary: {:?}\n  new: {new:?}\n  old: {old:?}",
                dataset.summary()
            ));
        }
        drop(dataset);
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(&cache);
    }
    assert!(
        failures.is_empty(),
        "{} of {} plain files read differently:\n{}",
        failures.len(),
        SHAPES.len(),
        failures.join("\n")
    );
}
