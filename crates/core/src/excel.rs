//! Reads a spreadsheet sheet with calamine and loads it into a DuckDB table.
//!
//! We avoid DuckDB's Excel extension: it is downloaded at runtime, can't read
//! `.xls`, and infers types from the first data row only.

use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use calamine::{Data, Reader as _, open_workbook_auto};
use duckdb::types::{TimeUnit, Value};
use duckdb::{Connection, appender_params_from_iter};

use crate::sql::quote_ident;

/// Sheet names in workbook order.
pub fn sheet_names(path: &Path) -> Result<Vec<String>> {
    let workbook = open_workbook_auto(path).context("couldn't open the workbook")?;
    Ok(workbook.sheet_names())
}

/// Loads sheet `index` into a new table called `table`.
pub(crate) fn load_sheet(conn: &Connection, path: &Path, index: usize, table: &str) -> Result<()> {
    let mut workbook = open_workbook_auto(path).context("couldn't open the workbook")?;
    let range = workbook
        .worksheet_range_at(index)
        .ok_or_else(|| anyhow!("the workbook has no sheet {}", index + 1))?
        .context("couldn't read the sheet")?;

    let rows: Vec<&[Data]> = range.rows().collect();
    // DuckDB tables need a column, so an empty sheet gets one with no rows.
    let width = range.width().max(1);
    // Skip leading blank rows so a title gap above the table doesn't become the header.
    let first = rows
        .iter()
        .position(|row| row.iter().any(|cell| !matches!(cell, Data::Empty)))
        .unwrap_or(rows.len());
    let rows = &rows[first..];

    let has_header = rows.first().is_some_and(|row| {
        row.iter()
            .all(|cell| matches!(cell, Data::String(_) | Data::Empty))
    });
    let (header, body) = match rows.split_first() {
        Some((header, body)) if has_header => (Some(*header), body),
        _ => (None, rows),
    };

    let names = column_names(header, width);
    let types: Vec<ColType> = (0..width)
        .map(|col| infer(body.iter().map(|row| row.get(col).unwrap_or(&Data::Empty))))
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
        let values = types
            .iter()
            .enumerate()
            .map(|(col, ty)| ty.value(row.get(col).unwrap_or(&Data::Empty)));
        appender.append_row(appender_params_from_iter(values))?;
    }
    appender.flush()?;
    Ok(())
}

/// Header cells become names; blanks get `column_N`, and duplicates get a suffix.
fn column_names(header: Option<&[Data]>, width: usize) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(width);
    for col in 0..width {
        let raw = match header.and_then(|row| row.get(col)) {
            Some(Data::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
            _ => format!("column_{}", col + 1),
        };
        let mut name = raw.clone();
        let mut n = 2;
        while names
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&name))
        {
            name = format!("{raw}_{n}");
            n += 1;
        }
        names.push(name);
    }
    names
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

    #[test]
    fn names_blank_and_duplicate_headers() {
        let header = [
            Data::String("id".into()),
            Data::Empty,
            Data::String("ID".into()),
        ];
        assert_eq!(column_names(Some(&header), 3), ["id", "column_2", "ID_2"]);
    }
}
