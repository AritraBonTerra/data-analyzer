//! Text formatting helpers for counts, percentages and column widths.

/// `1234567` → `1,234,567`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `0.0423` → `4.2%`; tiny shares read as `<0.1%` rather than `0.0%`.
pub fn percent(fraction: f64) -> String {
    let pct = fraction * 100.0;
    if pct == 0.0 {
        "0%".into()
    } else if pct < 0.1 {
        "<0.1%".into()
    } else if (99.95..100.0).contains(&pct) {
        ">99.9%".into()
    } else if pct >= 10.0 {
        format!("{pct:.0}%")
    } else {
        format!("{pct:.1}%")
    }
}

/// "1 row" / "2 rows".
pub fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{} {word}s", group_digits(n))
    }
}

/// A short, readable label for a DuckDB type: `DECIMAL(18,3)` → `decimal`,
/// `TIMESTAMP WITH TIME ZONE` → `timestamptz`.
pub fn type_label(sql_type: &str) -> String {
    let upper = sql_type.to_ascii_uppercase();
    let base = upper.split('(').next().unwrap_or("").trim();
    match base {
        "BIGINT" | "INTEGER" | "SMALLINT" | "TINYINT" | "HUGEINT" | "UBIGINT" | "UINTEGER"
        | "USMALLINT" | "UTINYINT" | "UHUGEINT" => "integer".into(),
        "DOUBLE" | "FLOAT" | "REAL" => "float".into(),
        "VARCHAR" => "text".into(),
        "BOOLEAN" => "boolean".into(),
        "TIMESTAMP WITH TIME ZONE" => "timestamptz".into(),
        other => other.to_ascii_lowercase(),
    }
}

/// Pixels per character for the grid's 13px UI font; a touch generous so
/// digits with tabular figures fit.
const CHAR_WIDTH: f32 = 7.6;
const MIN_COLUMN_WIDTH: f32 = 64.0;
const MAX_COLUMN_WIDTH: f32 = 420.0;

/// Picks a starting width for a column from its header text and a sample of
/// its cells: wide enough for typical values, capped so one long text column
/// doesn't take over the window.
pub fn initial_column_width<'a>(
    header: &str,
    sample: impl Iterator<Item = Option<&'a str>>,
) -> f32 {
    let mut longest_cell = 4;
    for cell in sample {
        let len = cell.map_or(4, |c| c.chars().count());
        longest_cell = longest_cell.max(len);
    }
    let cells = longest_cell.min(56) as f32 * CHAR_WIDTH + 24.0;
    // Headers carry a type glyph, a menu button and room for a sort arrow.
    let header = header.chars().count().min(40) as f32 * 7.6 + 64.0;
    cells.max(header).clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH)
}

/// Width for the row-number gutter given the largest row number it shows.
pub fn gutter_width(rows: u64) -> f32 {
    let digits = group_digits(rows.max(1)).len() as f32;
    (digits * CHAR_WIDTH + 24.0).max(44.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_digits() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(30_000_000), "30,000,000");
    }

    #[test]
    fn percents() {
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(0.0004), "<0.1%");
        assert_eq!(percent(0.0423), "4.2%");
        assert_eq!(percent(0.5), "50%");
        assert_eq!(percent(0.9996), ">99.9%");
        assert_eq!(percent(1.0), "100%");
    }

    #[test]
    fn type_labels() {
        assert_eq!(type_label("BIGINT"), "integer");
        assert_eq!(type_label("DECIMAL(18,3)"), "decimal");
        assert_eq!(type_label("VARCHAR"), "text");
        assert_eq!(type_label("TIMESTAMP"), "timestamp");
    }

    #[test]
    fn column_widths() {
        let w = initial_column_width("id", [Some("1"), Some("22")].into_iter());
        assert_eq!(w, 2.0 * 7.6 + 64.0);

        let long = "x".repeat(500);
        let w = initial_column_width("description", [Some(long.as_str()), None].into_iter());
        assert_eq!(w, MAX_COLUMN_WIDTH);

        let w = initial_column_width("name", [Some("Ada Lovelace")].into_iter());
        assert!(w > MIN_COLUMN_WIDTH && w < 200.0, "{w}");

        assert!(gutter_width(30_000_000) > gutter_width(100));
    }
}
