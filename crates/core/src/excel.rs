//! Reads spreadsheet sheets with calamine, turns them into grids for the table
//! finder, and loads a found table (or a whole sheet) into a DuckDB table.
//!
//! We avoid DuckDB's Excel extension: it is downloaded at runtime, can't read
//! `.xls`, and infers types from the first data row only.

use std::path::Path;

use anyhow::{Context as _, Result};
use calamine::{Data, Range, Reader as _, open_workbook_auto};
use duckdb::types::{TimeUnit, Value};
use duckdb::{Connection, appender_params_from_iter};

use crate::layout::{Cell, Table, unique_names};
use crate::sql::quote_ident;

/// Every sheet of the workbook, in order, with its name. A sheet that can't be
/// read comes back empty, so one bad sheet doesn't stop the others opening.
pub(crate) fn read_sheets(path: &Path) -> Result<Vec<(String, Range<Data>)>> {
    let mut workbook = open_workbook_auto(path).context("couldn't open the workbook")?;
    let names = workbook.sheet_names();
    Ok(names
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let range = workbook
                .worksheet_range_at(index)
                .and_then(Result::ok)
                .unwrap_or_else(Range::empty);
            (name, range)
        })
        .collect())
}

/// The sheet at `index`, read again (for a preview after opening).
pub(crate) fn read_sheet(path: &Path, index: usize) -> Result<Range<Data>> {
    let mut workbook = open_workbook_auto(path).context("couldn't open the workbook")?;
    workbook
        .worksheet_range_at(index)
        .context("the sheet is gone")?
        .context("couldn't read the sheet")
}

/// The sheet as a grid for the table finder. Grid positions are sheet
/// positions: row 0 is row 1 and column 0 is column A.
pub(crate) fn grid(range: &Range<Data>) -> Vec<Vec<Cell>> {
    let Some((top, left)) = range.start() else {
        return Vec::new();
    };
    let mut grid = vec![Vec::new(); top as usize];
    for row in range.rows() {
        let padding = std::iter::repeat_with(Cell::default).take(left as usize);
        grid.push(padding.chain(row.iter().map(grid_cell)).collect());
    }
    grid
}

fn grid_cell(cell: &Data) -> Cell {
    match cell {
        Data::Empty => Cell::default(),
        Data::String(s) => Cell::from_text(s.as_str()),
        Data::DateTime(dt) if dt.is_datetime() && dt.as_f64().fract() == 0.0 => {
            let (y, mo, d, ..) = dt.to_ymd_hms_milli();
            Cell {
                text: format!("{y:04}-{mo:02}-{d:02}"),
                value: true,
            }
        }
        other => Cell {
            text: text(other),
            value: true,
        },
    }
}

/// How the app read a sheet before the table finder, kept for sheets where it
/// finds no table: skip leading blank rows, then the first row is the header
/// if every cell in it is text. Returns the header and the body rows.
fn whole_sheet(range: &Range<Data>) -> (Option<&[Data]>, Vec<&[Data]>) {
    let rows: Vec<&[Data]> = range.rows().collect();
    let first = rows
        .iter()
        .position(|row| row.iter().any(|cell| !matches!(cell, Data::Empty)))
        .unwrap_or(rows.len());
    let rows = &rows[first..];
    let has_header = rows.first().is_some_and(|row| {
        row.iter()
            .all(|cell| matches!(cell, Data::String(_) | Data::Empty))
    });
    match rows.split_first() {
        Some((header, body)) if has_header => (Some(*header), body.to_vec()),
        _ => (None, rows.to_vec()),
    }
}

/// Data rows in a sheet read whole.
pub(crate) fn whole_sheet_rows(range: &Range<Data>) -> usize {
    whole_sheet(range).1.len()
}

/// Loads a sheet read whole into a new table called `table`.
pub(crate) fn load_sheet(conn: &Connection, range: &Range<Data>, table: &str) -> Result<()> {
    let (header, body) = whole_sheet(range);
    // DuckDB tables need a column, so an empty sheet gets one with no rows.
    let width = range.width().max(1);
    let names = unique_names(
        (0..width).map(|col| match header.and_then(|row| row.get(col)) {
            Some(Data::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
            _ => format!("column_{}", col + 1),
        }),
    );
    let body: Vec<Vec<&Data>> = body
        .iter()
        .map(|row| {
            (0..width)
                .map(|col| row.get(col).unwrap_or(&Data::Empty))
                .collect()
        })
        .collect();
    create(conn, table, &names, &body)
}

/// Loads a table the finder found (or the user adjusted) into a new table called `name`.
pub(crate) fn load_table(
    conn: &Connection,
    range: &Range<Data>,
    table: &Table,
    name: &str,
) -> Result<()> {
    let names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let body: Vec<Vec<&Data>> = table
        .body
        .iter()
        .map(|&row| {
            table
                .columns
                .iter()
                .map(|c| {
                    range
                        .get_value((row as u32, c.col as u32))
                        .unwrap_or(&Data::Empty)
                })
                .collect()
        })
        .collect();
    create(conn, name, &names, &body)
}

/// Creates `table` with one column per name, typed from its cells, and fills it.
fn create(conn: &Connection, table: &str, names: &[String], body: &[Vec<&Data>]) -> Result<()> {
    let types: Vec<ColType> = (0..names.len())
        .map(|col| infer(body.iter().map(|row| row[col])))
        .collect();
    let columns = names
        .iter()
        .zip(&types)
        .map(|(name, ty)| format!("{} {}", quote_ident(name), ty.sql()))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute_batch(&format!("CREATE TABLE {} ({columns})", quote_ident(table)))?;

    let mut appender = conn.appender(table)?;
    for row in body {
        let values = types.iter().zip(row).map(|(ty, cell)| ty.value(cell));
        appender.append_row(appender_params_from_iter(values))?;
    }
    appender.flush()?;
    Ok(())
}

/// The DuckDB type chosen for a spreadsheet column. Mixed columns fall back to text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColType {
    BigInt,
    Double,
    Boolean,
    Date,
    Timestamp,
    Varchar,
}

impl ColType {
    fn sql(self) -> &'static str {
        match self {
            Self::BigInt => "BIGINT",
            Self::Double => "DOUBLE",
            Self::Boolean => "BOOLEAN",
            Self::Date => "DATE",
            Self::Timestamp => "TIMESTAMP",
            Self::Varchar => "VARCHAR",
        }
    }

    fn value(self, cell: &Data) -> Value {
        match (self, cell) {
            (_, Data::Empty) => Value::Null,
            (Self::BigInt, Data::Int(i)) => Value::BigInt(*i),
            (Self::BigInt, Data::Float(f)) => Value::BigInt(*f as i64),
            (Self::Double, Data::Int(i)) => Value::Double(*i as f64),
            (Self::Double, Data::Float(f)) => Value::Double(*f),
            (Self::Boolean, Data::Bool(b)) => Value::Boolean(*b),
            (Self::Date, Data::DateTime(dt)) => {
                Value::Date32(unix_micros(dt).div_euclid(86_400_000_000) as i32)
            }
            (Self::Timestamp, Data::DateTime(dt)) => {
                Value::Timestamp(TimeUnit::Microsecond, unix_micros(dt))
            }
            (_, cell) => Value::Text(text(cell)),
        }
    }
}

/// Unix microseconds for an Excel date. calamine resolves the 1900/1904 epoch.
fn unix_micros(dt: &calamine::ExcelDateTime) -> i64 {
    let (y, mo, d, h, mi, s, ms) = dt.to_ymd_hms_milli();
    let days = days_from_civil(y.into(), mo.into(), d.into());
    let ms_of_day =
        ((i64::from(h) * 60 + i64::from(mi)) * 60 + i64::from(s)) * 1000 + i64::from(ms);
    (days * 86_400_000 + ms_of_day) * 1000
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn text(cell: &Data) -> String {
    match cell {
        Data::DateTime(dt) if dt.is_datetime() => {
            let (y, mo, d, h, mi, s, ms) = dt.to_ymd_hms_milli();
            let fraction = if ms > 0 {
                format!(".{ms:03}")
            } else {
                String::new()
            };
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}{fraction}")
        }
        // Durations are stored as fractional days; show elapsed h:mm:ss.
        Data::DateTime(dt) => {
            let secs = (dt.as_f64() * 86_400.0).round() as i64;
            format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
        }
        other => other.to_string(),
    }
}

/// Picks the narrowest type that fits every non-empty cell.
fn infer<'a>(cells: impl Iterator<Item = &'a Data>) -> ColType {
    let (mut ints, mut floats, mut bools, mut dates, mut datetimes, mut other) =
        (false, false, false, false, false, false);
    for cell in cells {
        match cell {
            Data::Empty => {}
            Data::Int(_) => ints = true,
            Data::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => ints = true,
            Data::Float(_) => floats = true,
            Data::Bool(_) => bools = true,
            Data::DateTime(dt) if dt.is_datetime() => {
                if dt.as_f64().fract() == 0.0 {
                    dates = true;
                } else {
                    datetimes = true;
                }
            }
            _ => other = true,
        }
    }
    let numeric = ints || floats;
    let temporal = dates || datetimes;
    match (numeric, bools, temporal, other) {
        (true, false, false, false) if floats => ColType::Double,
        (true, false, false, false) => ColType::BigInt,
        (false, true, false, false) => ColType::Boolean,
        (false, false, true, false) if datetimes => ColType::Timestamp,
        (false, false, true, false) => ColType::Date,
        _ => ColType::Varchar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_column_types() {
        let s = |v: &str| Data::String(v.into());
        assert_eq!(infer([Data::Int(1), Data::Empty].iter()), ColType::BigInt);
        assert_eq!(
            infer([Data::Float(2.0), Data::Int(1)].iter()),
            ColType::BigInt
        );
        assert_eq!(
            infer([Data::Float(1.5), Data::Int(1)].iter()),
            ColType::Double
        );
        assert_eq!(infer([Data::Bool(true)].iter()), ColType::Boolean);
        assert_eq!(infer([Data::Int(1), s("x")].iter()), ColType::Varchar);
        assert_eq!(infer([Data::Empty].iter()), ColType::Varchar);
    }

    #[test]
    fn mixed_column_timestamps_keep_milliseconds() {
        use calamine::{ExcelDateTime, ExcelDateTimeType};
        // 2025-10-13 12:59:02.400 and midnight on the same day.
        let precise = ExcelDateTime::new(45943.541, ExcelDateTimeType::DateTime, false);
        let midnight = ExcelDateTime::new(45943.0, ExcelDateTimeType::DateTime, false);
        assert_eq!(text(&Data::DateTime(precise)), "2025-10-13 12:59:02.400");
        assert_eq!(text(&Data::DateTime(midnight)), "2025-10-13 00:00:00");
    }

    #[test]
    fn converts_civil_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }
}
