//! Column statistics: DuckDB's SUMMARIZE plus histograms and top values.

use anyhow::Result;
use duckdb::Connection;

use crate::dataset::{Column, ColumnKind, Dataset, Frame};
use crate::sql::quote_ident;

/// One row of DuckDB's `SUMMARIZE`. Values are text so dates and numbers share a shape.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSummary {
    pub name: String,
    pub count: u64,
    pub null_percent: f64,
    /// Approximate (HyperLogLog), which is what makes this fast on big files.
    pub distinct: u64,
    pub min: Option<String>,
    pub max: Option<String>,
    pub mean: Option<String>,
    pub std: Option<String>,
    pub q25: Option<String>,
    pub median: Option<String>,
    pub q75: Option<String>,
}

/// The shape of a column's values: bins for continuous data, counts for the rest.
#[derive(Clone, Debug, PartialEq)]
pub enum Distribution {
    Histogram(Vec<Bin>),
    /// Most common values first. `None` is NULL. `other` counts everything not listed.
    Categories {
        values: Vec<(Option<String>, u64)>,
        other: u64,
    },
    /// No non-null values.
    Empty,
}

/// An equal-width bin. `first`/`last` are the smallest and largest values that
/// fell in it, formatted for the column's type, or `None` when the bin is empty.
#[derive(Clone, Debug, PartialEq)]
pub struct Bin {
    pub count: u64,
    pub first: Option<String>,
    pub last: Option<String>,
}

/// A numeric column with this many distinct values or fewer reads better as categories.
const FEW_DISTINCT: u64 = 12;
/// Rows sampled for header sketches; enough for a faithful shape, fast on any size.
const SKETCH_SAMPLE_ROWS: u64 = 200_000;

impl Dataset {
    /// SUMMARIZE for every column of `frame`.
    pub fn summarize(&self, frame: &Frame) -> Result<Vec<ColumnSummary>> {
        let conn = self.connection()?;
        summarize(&conn, &format!("SELECT * FROM {}", frame.relation))
    }

    /// Exact distribution for one column, for the stats panel.
    pub fn distribution(
        &self,
        frame: &Frame,
        column: &Column,
        bins: usize,
        top: usize,
    ) -> Result<Distribution> {
        distribution(&self.connection()?, &frame.relation, column, bins, top)
    }

    /// Small distributions for every column, computed on a sample, for column headers.
    pub fn sketches(&self, frame: &Frame) -> Result<Vec<Distribution>> {
        let conn = self.connection()?;
        let sample = format!("_sample_{}", self.next_id());
        conn.execute_batch(&format!(
            "CREATE TEMP TABLE {sample} AS SELECT * FROM {} USING SAMPLE reservoir({SKETCH_SAMPLE_ROWS} ROWS) REPEATABLE (7)",
            frame.relation
        ))?;
        // One column that can't be profiled shouldn't hide every other sketch.
        let sketches = frame
            .columns
            .iter()
            .map(|column| {
                distribution(&conn, &sample, column, 24, 3).unwrap_or(Distribution::Empty)
            })
            .collect();
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {sample}"))?;
        Ok(sketches)
    }
}

fn summarize(conn: &Connection, select: &str) -> Result<Vec<ColumnSummary>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT column_name, CAST(\"count\" AS BIGINT), CAST(null_percentage AS DOUBLE),
                CAST(approx_unique AS BIGINT), CAST(min AS VARCHAR), CAST(max AS VARCHAR),
                CAST(avg AS VARCHAR), CAST(std AS VARCHAR), CAST(q25 AS VARCHAR),
                CAST(q50 AS VARCHAR), CAST(q75 AS VARCHAR)
         FROM (SUMMARIZE {select})"
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok(ColumnSummary {
            name: r.get(0)?,
            count: r.get::<_, i64>(1)? as u64,
            null_percent: r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
            distinct: r.get::<_, Option<i64>>(3)?.unwrap_or(0) as u64,
            min: r.get(4)?,
            max: r.get(5)?,
            mean: r.get::<_, Option<String>>(6)?.map(|v| tidy_number(&v)),
            std: r.get::<_, Option<String>>(7)?.map(|v| tidy_number(&v)),
            q25: r.get::<_, Option<String>>(8)?.map(|v| tidy_number(&v)),
            median: r.get::<_, Option<String>>(9)?.map(|v| tidy_number(&v)),
            q75: r.get::<_, Option<String>>(10)?.map(|v| tidy_number(&v)),
        })
    })?;
    Ok(rows.collect::<duckdb::Result<_>>()?)
}

fn distribution(
    conn: &Connection,
    relation: &str,
    column: &Column,
    bins: usize,
    top: usize,
) -> Result<Distribution> {
    let col = quote_ident(&column.name);
    // Histograms bin a numeric projection of the value: itself, or epoch seconds for dates.
    let numeric = match column.kind {
        ColumnKind::Number => Some(format!("CAST({col} AS DOUBLE)")),
        ColumnKind::Temporal => Some(format!("CAST(epoch({col}) AS DOUBLE)")),
        _ => None,
    };
    if let Some(x) = numeric {
        let (lo, hi, distinct): (Option<f64>, Option<f64>, i64) = conn.query_row(
            // Bins span finite values only; ±inf and NaN would make the bin width non-finite.
            &format!(
                "SELECT min({x}) FILTER (isfinite({x})), max({x}) FILTER (isfinite({x})),
                        approx_count_distinct({col}) FROM {relation}"
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let (Some(lo), Some(hi)) = (lo, hi) else {
            return Ok(Distribution::Empty);
        };
        if distinct as u64 > FEW_DISTINCT && hi > lo {
            return histogram(conn, relation, &col, &x, lo, hi, bins);
        }
    }
    categories(conn, relation, &col, top)
}

fn histogram(
    conn: &Connection,
    relation: &str,
    col: &str,
    x: &str,
    lo: f64,
    hi: f64,
    bins: usize,
) -> Result<Distribution> {
    let width = (hi - lo) / bins as f64;
    let mut stmt = conn.prepare(&format!(
        "SELECT least(CAST(floor(({x} - {lo}) / {width}) AS BIGINT), {last}) AS b,
                count(*), CAST(min({col}) AS VARCHAR), CAST(max({col}) AS VARCHAR)
         FROM {relation} WHERE isfinite({x}) GROUP BY 1 ORDER BY 1",
        last = bins - 1,
    ))?;
    let mut out: Vec<Bin> = (0..bins)
        .map(|_| Bin {
            count: 0,
            first: None,
            last: None,
        })
        .collect();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get(2)?,
            r.get(3)?,
        ))
    })?;
    for row in rows {
        let (b, count, first, last) = row?;
        if let Some(bin) = out.get_mut(b.max(0) as usize) {
            *bin = Bin {
                count: count as u64,
                first,
                last,
            };
        }
    }
    Ok(Distribution::Histogram(out))
}

fn categories(conn: &Connection, relation: &str, col: &str, top: usize) -> Result<Distribution> {
    let total: i64 = conn.query_row(&format!("SELECT count(*) FROM {relation}"), [], |r| {
        r.get(0)
    })?;
    if total == 0 {
        return Ok(Distribution::Empty);
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT CAST({col} AS VARCHAR) AS v, count(*) AS n FROM {relation}
         GROUP BY 1 ORDER BY 2 DESC, 1 NULLS LAST LIMIT {top}"
    ))?;
    let values = stmt
        .query_map([], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)? as u64))
        })?
        .collect::<duckdb::Result<Vec<_>>>()?;
    let listed: u64 = values.iter().map(|(_, n)| n).sum();
    Ok(Distribution::Categories {
        values,
        other: total as u64 - listed,
    })
}

/// Trims long decimals from SUMMARIZE's text (e.g. `12.345678901234` → `12.3457`),
/// keeping about five significant digits for values below 1. Leaves dates,
/// other text, and very large or tiny numbers alone.
pub fn tidy_number(value: &str) -> String {
    match value.parse::<f64>() {
        Ok(n) if value.contains('.') && n.abs() < 1e15 && (n == 0.0 || n.abs() >= 1e-8) => {
            let decimals = if n == 0.0 || n.abs() >= 1.0 {
                4
            } else {
                (4 - n.abs().log10().floor() as i32) as usize
            };
            let rounded = format!("{n:.decimals$}");
            rounded
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string()
        }
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidies_numbers() {
        assert_eq!(tidy_number("12.345678901"), "12.3457");
        assert_eq!(tidy_number("-0.5000001"), "-0.5");
        assert_eq!(tidy_number("3.0"), "3");
        assert_eq!(tidy_number("42"), "42");
        assert_eq!(tidy_number("0.000123456789"), "0.00012346");
        assert_eq!(tidy_number("1.234e-08"), "0.00000001234");
        assert_eq!(tidy_number("1.5e-12"), "1.5e-12");
        assert_eq!(
            tidy_number("2024-01-02 03:04:05.5"),
            "2024-01-02 03:04:05.5"
        );
    }
}
