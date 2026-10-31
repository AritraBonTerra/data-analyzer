//! SQL text helpers and the filter/sort spec that the grid turns into a query.

use crate::{Column, ColumnKind};

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub fn quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// How the grid wants a frame filtered and sorted. An empty spec means "as loaded".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewSpec {
    pub sort: Vec<SortKey>,
    pub filters: Vec<Filter>,
    /// Case-insensitive substring match against every column.
    pub search: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SortKey {
    pub column: String,
    pub descending: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
}

/// Values arrive as text from the UI and are cast to the column's type in SQL,
/// so `> 10` compares numerically on a number column and by date on a date column.
#[derive(Clone, Debug, PartialEq)]
pub enum FilterOp {
    Contains(String),
    NotContains(String),
    Equals(String),
    NotEquals(String),
    GreaterThan(String),
    LessThan(String),
    Between(String, String),
    OneOf(Vec<String>),
    IsNull,
    IsNotNull,
}

impl FilterOp {
    /// Short label for filter chips, e.g. `> 10` or `contains "abc"`.
    pub fn describe(&self) -> String {
        match self {
            Self::Contains(v) => format!("contains “{v}”"),
            Self::NotContains(v) => format!("doesn't contain “{v}”"),
            Self::Equals(v) => format!("= {v}"),
            Self::NotEquals(v) => format!("≠ {v}"),
            Self::GreaterThan(v) => format!("> {v}"),
            Self::LessThan(v) => format!("< {v}"),
            Self::Between(a, b) => format!("between {a} and {b}"),
            Self::OneOf(values) if values.len() == 1 => format!("= {}", values[0]),
            Self::OneOf(values) => format!("is one of {} values", values.len()),
            Self::IsNull => "is empty".into(),
            Self::IsNotNull => "is not empty".into(),
        }
    }
}

impl ViewSpec {
    pub fn is_empty(&self) -> bool {
        self.sort.is_empty() && self.filters.is_empty() && self.search.trim().is_empty()
    }

    /// `WHERE … ORDER BY …` for this spec, or an empty string.
    pub fn clauses(&self, columns: &[Column]) -> String {
        let mut conditions: Vec<String> = self
            .filters
            .iter()
            .filter_map(|f| {
                let column = columns.iter().find(|c| c.name == f.column)?;
                Some(condition(column, &f.op))
            })
            .collect();

        let search = self.search.trim();
        if !search.is_empty() && !columns.is_empty() {
            let haystack = columns
                .iter()
                .map(|c| format!("coalesce(CAST({} AS VARCHAR), '')", quote_ident(&c.name)))
                .collect::<Vec<_>>()
                .join(" || chr(31) || ");
            conditions.push(format!(
                "contains(lower({haystack}), lower({}))",
                quote_str(search)
            ));
        }

        let mut sql = String::new();
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        let order: Vec<String> = self
            .sort
            .iter()
            .filter(|k| columns.iter().any(|c| c.name == k.column))
            .map(|k| {
                let dir = if k.descending { "DESC" } else { "ASC" };
                format!("{} {dir} NULLS LAST", quote_ident(&k.column))
            })
            .collect();
        if !order.is_empty() {
            sql.push_str(" ORDER BY ");
            sql.push_str(&order.join(", "));
        }
        sql
    }
}

fn condition(column: &Column, op: &FilterOp) -> String {
    let col = quote_ident(&column.name);
    let text = format!("lower(CAST({col} AS VARCHAR))");
    // Text columns compare as text; others cast the typed-in value to the column type.
    let typed = |v: &str| match column.kind {
        ColumnKind::Text => quote_str(v),
        _ => format!("TRY_CAST({} AS {})", quote_str(v.trim()), column.sql_type),
    };
    match op {
        FilterOp::Contains(v) => format!("contains({text}, lower({}))", quote_str(v)),
        FilterOp::NotContains(v) => {
            format!(
                "({col} IS NULL OR NOT contains({text}, lower({})))",
                quote_str(v)
            )
        }
        FilterOp::Equals(v) => format!("{col} = {}", typed(v)),
        FilterOp::NotEquals(v) => format!("{col} IS DISTINCT FROM {}", typed(v)),
        FilterOp::GreaterThan(v) => format!("{col} > {}", typed(v)),
        FilterOp::LessThan(v) => format!("{col} < {}", typed(v)),
        FilterOp::Between(a, b) => format!("{col} BETWEEN {} AND {}", typed(a), typed(b)),
        FilterOp::OneOf(values) if values.is_empty() => "false".into(),
        FilterOp::OneOf(values) => format!(
            "CAST({col} AS VARCHAR) IN ({})",
            values
                .iter()
                .map(|v| quote_str(v))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        FilterOp::IsNull => format!("{col} IS NULL"),
        FilterOp::IsNotNull => format!("{col} IS NOT NULL"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes() {
        assert_eq!(quote_ident(r#"a"b"#), r#""a""b""#);
        assert_eq!(quote_str("it's"), "'it''s'");
    }

    #[test]
    fn builds_clauses() {
        let columns = vec![Column::new("n", "BIGINT"), Column::new("s", "VARCHAR")];
        let spec = ViewSpec {
            sort: vec![SortKey {
                column: "n".into(),
                descending: true,
            }],
            filters: vec![Filter {
                column: "n".into(),
                op: FilterOp::GreaterThan("10".into()),
            }],
            search: String::new(),
        };
        assert_eq!(
            spec.clauses(&columns),
            r#" WHERE "n" > TRY_CAST('10' AS BIGINT) ORDER BY "n" DESC NULLS LAST"#
        );
        assert_eq!(ViewSpec::default().clauses(&columns), "");
    }
}
