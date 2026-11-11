//! Nested data opened into rows and columns.
//!
//! A JSON file that is one object holding its records ([`single_object`]) is
//! unwrapped first: the largest list of objects under its keys, or a list of
//! names beside a list of rows ([`Records`]), becomes the rows, and the
//! object's other plain fields become file notes ([`notes`]).
//!
//! Then, for JSON and Parquet alike, struct columns open into one column per
//! field, named with dots (`customer.address.city`), through one projection
//! decided from the column types alone ([`Opened`]). Lists stay in one cell;
//! [`crate::Dataset::fetch`] shows how many items each holds.

use std::io::Read;
use std::path::Path;

use anyhow::Result;
use duckdb::Connection;

use crate::dataset::describe;
use crate::import::{Note, RowRole};
use crate::layout::unique_names;
use crate::sql::{quote_ident, quote_str};
use crate::{Column, ColumnKind};

/// Struct levels opened into columns; a struct deeper down stays one cell.
const MAX_DEPTH: usize = 4;
/// Bytes of a JSON file read to tell one object from a run of them.
const PEEK_BYTES: u64 = 1 << 20;

/// A DuckDB type, as far as opening it goes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Type {
    /// Fields in order, with their names as written.
    Struct(Vec<(String, Type)>),
    /// A list or a fixed-size array.
    List(Box<Type>),
    /// Anything else, as DuckDB names it: `BIGINT`, `MAP(VARCHAR, BIGINT)`, `JSON`.
    Other(String),
}

impl Type {
    /// Reads a type as DuckDB describes it, like
    /// `STRUCT("name" VARCHAR, tags VARCHAR[])[]`.
    pub(crate) fn parse(text: &str) -> Self {
        let text = text.trim();
        if let Some((element, _)) = text.strip_suffix(']').and_then(|t| t.rsplit_once('[')) {
            return Self::List(Box::new(Self::parse(element)));
        }
        let inner = text
            .get(..7)
            .filter(|head| head.eq_ignore_ascii_case("STRUCT("))
            .and_then(|_| text[7..].strip_suffix(')'));
        match inner {
            Some(inner) => Self::Struct(split_fields(inner).into_iter().map(field).collect()),
            None => Self::Other(text.to_string()),
        }
    }
}

/// Splits a struct's fields on the commas outside quotes and parentheses.
fn split_fields(text: &str) -> Vec<&str> {
    let (mut depth, mut quoted, mut start) = (0, false, 0);
    let mut fields = Vec::new();
    for (i, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                fields.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    fields.push(&text[start..]);
    fields
}

/// One struct field, `name TYPE` or `"quoted ""name""" TYPE`.
fn field(text: &str) -> (String, Type) {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '"' {
                if chars.peek().is_some_and(|&(_, next)| next == '"') {
                    chars.next();
                } else {
                    return (name, Type::parse(&rest[i + 1..]));
                }
            }
            name.push(c);
        }
        return (name, Type::Other(String::new()));
    }
    let (name, ty) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    (name.to_string(), Type::parse(ty))
}

/// The projection that opens struct columns into dotted columns. Empty when
/// there is no struct to open, and then the relation is used as it is.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Opened {
    select: Option<String>,
}

impl Opened {
    /// Decides from the column types alone. A field whose dotted name is
    /// taken, by a column or an earlier field, gets `_2`, `_3`.
    pub(crate) fn new(columns: &[Column]) -> Self {
        let mut flat: Vec<(String, String, bool)> = Vec::new();
        for column in columns {
            open(
                quote_ident(&column.name),
                column.name.clone(),
                &Type::parse(&column.sql_type),
                0,
                &mut flat,
            );
        }
        if flat.iter().all(|(_, _, opened)| !opened) {
            return Self::default();
        }
        // Names of the columns as they are come first, so a field gives way to them.
        let mut taken: Vec<String> = flat
            .iter()
            .filter(|(_, _, opened)| !opened)
            .map(|(_, name, _)| name.clone())
            .collect();
        let select = flat
            .into_iter()
            .map(|(expr, name, opened)| {
                let name = if opened {
                    let unique = unique_names(taken.iter().cloned().chain([name]))
                        .pop()
                        .unwrap_or_default();
                    taken.push(unique.clone());
                    unique
                } else {
                    name
                };
                format!("{expr} AS {}", quote_ident(&name))
            })
            .collect::<Vec<_>>()
            .join(", ");
        Self {
            select: Some(select),
        }
    }

    /// Whether any struct was opened.
    pub(crate) fn is_open(&self) -> bool {
        self.select.is_some()
    }

    /// `from` with the structs opened, as a relation for `FROM`.
    pub(crate) fn wrap(&self, from: &str) -> String {
        match &self.select {
            Some(select) => format!("(SELECT {select} FROM {from})"),
            None => from.to_string(),
        }
    }

    /// One line for the summary, when something was opened.
    pub(crate) fn summary(&self) -> Option<String> {
        self.is_open()
            .then(|| "Nested fields opened into columns.".into())
    }
}

/// Adds `expr` to `flat` as the column `name`, or, for a struct not yet
/// `MAX_DEPTH` deep, each of its fields under `name.field`. The flag says
/// whether the column came from opening a struct.
fn open(
    expr: String,
    name: String,
    ty: &Type,
    depth: usize,
    flat: &mut Vec<(String, String, bool)>,
) {
    match ty {
        Type::Struct(fields) if depth < MAX_DEPTH && !fields.is_empty() => {
            for (field, ty) in fields {
                open(
                    format!("struct_extract({expr}, {})", quote_str(field)),
                    format!("{name}.{field}"),
                    ty,
                    depth + 1,
                    flat,
                );
            }
        }
        _ => flat.push((expr, name, depth > 0)),
    }
}

/// Keys that name a table's columns, and keys that hold its rows. A pair
/// under these keys is taken over any other list of names.
const NAME_KEYS: [&str; 4] = ["columns", "headers", "fields", "cols"];
const ROW_KEYS: [&str; 3] = ["rows", "data", "values"];
/// File notes kept from the object, at most.
const MAX_NOTES: usize = 50;

/// Where the records of a one-object JSON file are.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Records {
    /// A list of objects under these keys, one or two deep.
    List(Vec<String>),
    /// A list of column names and a list of rows, each a list of values,
    /// under these top-level keys. `json` when the values are JSON;
    /// `defined` when the names are objects, each naming a column in its
    /// `name` field.
    Table {
        names: String,
        rows: String,
        json: bool,
        defined: bool,
    },
}

impl Records {
    /// The lists of objects one or two keys down: where records could be.
    pub(crate) fn candidates(columns: &[Column]) -> Vec<Self> {
        reachable(columns)
            .into_iter()
            .filter(|(_, ty)| matches!(ty, Type::List(e) if matches!(**e, Type::Struct(_))))
            .map(|(path, _)| Self::List(path))
            .collect()
    }

    /// The SQL expression for the list of records.
    fn list(&self) -> String {
        match self {
            Self::List(path) => path_expr(path),
            Self::Table { rows, .. } => quote_ident(rows),
        }
    }

    /// The query that turns the object in `from` into one row per record.
    /// `names` are a table's column names, read from the file.
    pub(crate) fn select(&self, from: &str, names: &[String]) -> String {
        let unnest = format!("(SELECT unnest({}) AS _r FROM {from})", self.list());
        match self {
            Self::List(_) => format!("SELECT unnest(_r) FROM {unnest}"),
            Self::Table { json, .. } => {
                let names = unique_names(names.iter().enumerate().map(|(i, name)| {
                    if name.trim().is_empty() {
                        format!("column_{}", i + 1)
                    } else {
                        name.clone()
                    }
                }));
                let columns = names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        let value = format!("_r[{}]", i + 1);
                        let value = if *json {
                            format!("json_extract_string({value}, '$')")
                        } else {
                            value
                        };
                        format!("{value} AS {}", quote_ident(name))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("SELECT {columns} FROM {unnest}")
            }
        }
    }

    /// One line for the summary, like `40 records under data.`
    pub(crate) fn summary(&self, count: u64) -> String {
        match self {
            Self::List(path) => {
                let records = if count == 1 { "record" } else { "records" };
                format!("{count} {records} under {}.", path.join("."))
            }
            Self::Table { names, rows, .. } => {
                let noun = if count == 1 { "row" } else { "rows" };
                format!("{count} {noun} under {rows}, named by {names}.")
            }
        }
    }
}

/// Every value one or two keys down in the object, by its keys and type,
/// in the object's order: a struct, then its fields.
fn reachable(columns: &[Column]) -> Vec<(Vec<String>, Type)> {
    let mut found = Vec::new();
    for column in columns {
        let ty = Type::parse(&column.sql_type);
        let fields = match &ty {
            Type::Struct(fields) => fields.clone(),
            _ => Vec::new(),
        };
        found.push((vec![column.name.clone()], ty));
        found.extend(
            fields
                .into_iter()
                .map(|(field, ty)| (vec![column.name.clone(), field], ty)),
        );
    }
    found
}

/// The SQL expression for the value under `path`.
fn path_expr(path: &[String]) -> String {
    let mut expr = quote_ident(&path[0]);
    for key in &path[1..] {
        expr = format!("struct_extract({expr}, {})", quote_str(key));
    }
    expr
}

/// Whether `ty` is a list of rows, each a list of values, and whether the
/// values are JSON.
fn grid(ty: &Type) -> Option<bool> {
    match ty {
        Type::List(row) => match &**row {
            Type::List(value) => Some(**value == Type::Other("JSON".into())),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `ty` is a list of column names: text, or objects with a text
/// `name` field (then `Some(true)`).
fn name_list(ty: &Type) -> Option<bool> {
    match ty {
        Type::List(name) => match &**name {
            Type::Other(t) if t == "VARCHAR" => Some(false),
            Type::Struct(fields) => fields
                .iter()
                .any(|(f, t)| f == "name" && *t == Type::Other("VARCHAR".into()))
                .then_some(true),
            _ => None,
        },
        _ => None,
    }
}

/// A list of rows as measured: its key, whether it is at the top level,
/// whether its values are JSON, how many rows and how long the longest is.
struct Grid<'a> {
    key: &'a str,
    top: bool,
    json: bool,
    rows: u64,
    width: u64,
}

/// A list of names as measured: its key, whether its names are objects,
/// and how many there are.
struct Names<'a> {
    key: &'a str,
    defined: bool,
    len: u64,
}

/// The one unambiguous table among the top-level lists: the names under
/// [`NAME_KEYS`] beside the rows under [`ROW_KEYS`] when both are there,
/// else the only list of rows beside the only list of names as long as its
/// longest row. Either way the names must be as many as the longest row.
fn table(names: &[Names], grids: &[Grid]) -> Option<(Records, u64)> {
    let grids: Vec<&Grid> = grids.iter().filter(|g| g.top).collect();
    let named: Vec<&Names> = names
        .iter()
        .filter(|n| NAME_KEYS.contains(&n.key))
        .collect();
    let rows: Vec<&&Grid> = grids.iter().filter(|g| ROW_KEYS.contains(&g.key)).collect();
    let (names, grid) = match (&named[..], &rows[..]) {
        ([names], [grid]) => (*names, **grid),
        _ => {
            let [grid] = grids[..] else { return None };
            let mut fit = names.iter().filter(|n| n.len == grid.width);
            match (fit.next(), fit.next()) {
                (Some(names), None) => (names, grid),
                _ => return None,
            }
        }
    };
    (names.len == grid.width && grid.rows > 0).then(|| {
        let table = Records::Table {
            names: names.key.to_string(),
            rows: grid.key.to_string(),
            json: grid.json,
            defined: names.defined,
        };
        (table, grid.rows)
    })
}

/// Finds the records of the object loaded as `relation`, and how many: the
/// largest list of objects, or a table of names and rows, a list of objects
/// winning a tie. `None` when there are none, or when a list of rows that
/// can't be read as a table holds more rows than the records found, as the
/// records are then likely not the data.
pub(crate) fn find_records(conn: &Connection, relation: &str) -> Result<Option<(Records, u64)>> {
    let columns = describe(conn, relation)?;
    let lists = Records::candidates(&columns);
    let reachable = reachable(&columns);
    let grids: Vec<(&[String], bool)> = reachable
        .iter()
        .filter_map(|(path, ty)| grid(ty).map(|json| (path.as_slice(), json)))
        .collect();
    let names: Vec<(&str, bool)> = reachable
        .iter()
        .filter(|(path, _)| path.len() == 1)
        .filter_map(|(path, ty)| name_list(ty).map(|defined| (path[0].as_str(), defined)))
        .collect();
    if lists.is_empty() && grids.is_empty() {
        return Ok(None);
    }
    // One read of the object: each list's length, and each grid's longest row.
    let mut exprs: Vec<String> = lists.iter().map(|l| format!("len({})", l.list())).collect();
    for (path, _) in &grids {
        let expr = path_expr(path);
        exprs.push(format!("len({expr})"));
        exprs.push(format!(
            "list_max(list_transform({expr}, lambda r: len(r)))"
        ));
    }
    exprs.extend(
        names
            .iter()
            .map(|(key, _)| format!("len({})", quote_ident(key))),
    );
    let select = exprs
        .iter()
        .map(|e| format!("CAST(coalesce({e}, 0) AS UBIGINT)"))
        .collect::<Vec<_>>()
        .join(", ");
    let counts: Vec<u64> = conn.query_row(
        &format!("SELECT {select} FROM (SELECT * FROM {relation} LIMIT 1)"),
        [],
        |row| (0..exprs.len()).map(|i| row.get(i)).collect(),
    )?;
    let (list_counts, rest) = counts.split_at(lists.len());
    let (grid_counts, name_counts) = rest.split_at(grids.len() * 2);
    let grids: Vec<Grid> = grids
        .iter()
        .zip(grid_counts.chunks(2))
        .map(|((path, json), counts)| Grid {
            key: path.last().map_or("", String::as_str),
            top: path.len() == 1,
            json: *json,
            rows: counts[0],
            width: counts[1],
        })
        .collect();
    let names: Vec<Names> = names
        .iter()
        .zip(name_counts)
        .map(|(&(key, defined), &len)| Names { key, defined, len })
        .collect();
    let mut best: Option<(Records, u64)> = None;
    for (candidate, count) in lists
        .into_iter()
        .zip(list_counts.iter().copied())
        .chain(table(&names, &grids))
    {
        if best.as_ref().is_none_or(|(_, most)| count > *most) {
            best = Some((candidate, count));
        }
    }
    Ok(best.filter(|(records, count)| {
        matches!(records, Records::Table { .. }) || grids.iter().all(|g| g.rows <= *count)
    }))
}

/// A table's column names, read from the object loaded as `relation`.
pub(crate) fn table_names(
    conn: &Connection,
    relation: &str,
    records: &Records,
) -> Result<Vec<String>> {
    let Records::Table { names, defined, .. } = records else {
        return Ok(Vec::new());
    };
    let mut names = quote_ident(names);
    if *defined {
        names = format!("list_transform({names}, lambda d: struct_extract(d, 'name'))");
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT unnest({names}) FROM (SELECT * FROM {relation} LIMIT 1)"
    ))?;
    let names = stmt
        .query_map([], |row| row.get::<_, Option<String>>(0))?
        .map(|name| name.map(Option::unwrap_or_default))
        .collect::<duckdb::Result<Vec<_>>>()?;
    Ok(names)
}

/// The object's plain fields beside the records, and those one level inside
/// its other objects, as file notes like `status: ok`, `meta.page: 1` and
/// `previous: null`; [`MAX_NOTES`] at most. A note's `row` is the place in
/// the object of its top-level field, from 1.
pub(crate) fn notes(conn: &Connection, relation: &str) -> Result<Vec<Note>> {
    let plain = |ty: &Type| match ty {
        Type::Other(t) => {
            t.trim_matches('"') == "NULL" || ColumnKind::from_sql(t) != ColumnKind::Other
        }
        _ => false,
    };
    let columns = describe(conn, relation)?;
    let fields: Vec<(usize, Vec<String>)> = reachable(&columns)
        .into_iter()
        .filter(|(_, ty)| plain(ty))
        .map(|(path, _)| {
            let place = columns.iter().position(|c| c.name == path[0]).unwrap_or(0);
            (place, path)
        })
        .take(MAX_NOTES)
        .collect();
    if fields.is_empty() {
        return Ok(Vec::new());
    }
    let select = fields
        .iter()
        .map(|(_, path)| format!("coalesce(CAST({} AS VARCHAR), 'null')", path_expr(path)))
        .collect::<Vec<_>>()
        .join(", ");
    let values: Vec<String> = conn.query_row(
        &format!("SELECT {select} FROM {relation} LIMIT 1"),
        [],
        |row| (0..fields.len()).map(|i| row.get(i)).collect(),
    )?;
    Ok(fields
        .into_iter()
        .zip(values)
        .map(|((place, path), value)| Note {
            row: place + 1,
            role: RowRole::Title,
            text: format!("{}: {value}", path.join(".")),
        })
        .collect())
}

/// Whether the JSON file at `path` is one object rather than a list or a
/// run of objects: it starts with `{`, and only space follows where that
/// object ends. Reads the first megabyte at most; an object still open
/// there counts as one.
pub(crate) fn single_object(path: &Path) -> bool {
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|f| f.take(PEEK_BYTES).read_to_end(&mut bytes));
    if read.is_err() {
        return false;
    }
    let text = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let start = text.iter().position(|b| !b.is_ascii_whitespace());
    if start.is_none_or(|i| text[i] != b'{') {
        return false;
    }
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for (i, &b) in text.iter().enumerate().skip(start.unwrap_or(0)) {
        if quoted {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => quoted = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => quoted = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return text[i + 1..].iter().all(u8::is_ascii_whitespace);
                }
            }
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(types: &[(&str, &str)]) -> Vec<Column> {
        types.iter().map(|(n, t)| Column::new(*n, *t)).collect()
    }

    /// The columns `opened` makes of `relation`, as DuckDB names them.
    fn names(conn: &Connection, opened: &Opened, relation: &str) -> Vec<String> {
        describe(conn, &opened.wrap(relation))
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect()
    }

    #[test]
    fn parses_types() {
        assert_eq!(Type::parse("BIGINT"), Type::Other("BIGINT".into()));
        assert_eq!(
            Type::parse("DECIMAL(18,3)[]"),
            Type::List(Box::new(Type::Other("DECIMAL(18,3)".into())))
        );
        assert_eq!(
            Type::parse(
                r#"STRUCT("a.b" VARCHAR, "say ""hi""" MAP(VARCHAR, BIGINT), c STRUCT(d INTEGER[3]))"#
            ),
            Type::Struct(vec![
                ("a.b".into(), Type::Other("VARCHAR".into())),
                (
                    "say \"hi\"".into(),
                    Type::Other("MAP(VARCHAR, BIGINT)".into())
                ),
                (
                    "c".into(),
                    Type::Struct(vec![(
                        "d".into(),
                        Type::List(Box::new(Type::Other("INTEGER".into())))
                    )])
                ),
            ])
        );
    }

    #[test]
    fn flat_columns_stay_as_they_are() {
        let opened = Opened::new(&columns(&[
            ("id", "BIGINT"),
            ("tags", "VARCHAR[]"),
            ("attrs", "MAP(VARCHAR, BIGINT)"),
        ]));
        assert!(!opened.is_open());
        assert_eq!(opened.wrap("t"), "t");
        assert_eq!(opened.summary(), None);
    }

    #[test]
    fn opens_structs_in_structs_with_odd_names() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"CREATE TABLE t AS SELECT 1 AS id,
                {'name': 'Ada', 'address': {'city': 'Pune', 'zip': '02134'}} AS customer,
                {'a.b': 1, 'say "hi"': 2, 'x y': 3} AS odd,
                [{'sku': 'A'}] AS items"#,
        )
        .unwrap();
        let opened = Opened::new(&describe(&conn, "t").unwrap());
        assert!(opened.is_open());
        assert_eq!(
            names(&conn, &opened, "t"),
            [
                "id",
                "customer.name",
                "customer.address.city",
                "customer.address.zip",
                "odd.a.b",
                "odd.say \"hi\"",
                "odd.x y",
                "items"
            ]
        );
        let zip: String = conn
            .query_row(
                &format!(r#"SELECT "customer.address.zip" FROM {}"#, opened.wrap("t")),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(zip, "02134");
    }

    #[test]
    fn stops_four_levels_down() {
        let opened = Opened::new(&columns(&[(
            "a",
            "STRUCT(b STRUCT(c STRUCT(d STRUCT(e STRUCT(f BIGINT)))))",
        )]));
        let select = opened.select.unwrap();
        assert!(select.ends_with(r#"AS "a.b.c.d.e""#), "{select}");
    }

    #[test]
    fn a_field_gives_way_to_a_column_of_its_name() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"CREATE TABLE t AS SELECT {'name': 'Ada', 'tier': 'gold'} AS customer,
                'kept' AS "customer.name", 'also' AS "Customer.Tier_2", 'x' AS "customer.tier""#,
        )
        .unwrap();
        let opened = Opened::new(&describe(&conn, "t").unwrap());
        assert_eq!(
            names(&conn, &opened, "t"),
            [
                "customer.name_2",
                "customer.tier_3",
                "customer.name",
                "Customer.Tier_2",
                "customer.tier"
            ]
        );
    }

    #[test]
    fn finds_the_largest_list_of_records() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE env AS SELECT 'ok' AS status, 2 AS page,
                [{'id': 1}] AS small,
                {'count': 3, 'items': [{'id': 1, 'n': 'a'}, {'id': 2, 'n': 'b'}, {'id': 3, 'n': 'c'}]} AS result",
        )
        .unwrap();
        let (records, count) = find_records(&conn, "env").unwrap().unwrap();
        assert_eq!(
            records,
            Records::List(vec!["result".into(), "items".into()])
        );
        assert_eq!(count, 3);
        assert_eq!(records.summary(count), "3 records under result.items.");
        let rows = format!("({})", records.select("env", &[]));
        let columns: Vec<String> = describe(&conn, &rows)
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(columns, ["id", "n"]);
        let notes: Vec<String> = notes(&conn, "env")
            .unwrap()
            .into_iter()
            .map(|n| n.text)
            .collect();
        assert_eq!(notes, ["status: ok", "page: 2", "result.count: 3"]);
    }

    #[test]
    fn a_list_of_records_at_the_top() {
        let found = Records::candidates(&columns(&[
            ("data", "STRUCT(id BIGINT)[]"),
            ("tags", "VARCHAR[]"),
        ]));
        assert_eq!(found, [Records::List(vec!["data".into()])]);
    }

    #[test]
    fn reads_columns_and_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"CREATE TABLE env AS SELECT ['id', 'region', ''] AS columns,
                [['1', '"North"', '2.5'], ['2', '"West"', 'null']]::JSON[][] AS rows"#,
        )
        .unwrap();
        let (records, count) = find_records(&conn, "env").unwrap().unwrap();
        assert_eq!(
            records,
            Records::Table {
                names: "columns".into(),
                rows: "rows".into(),
                json: true,
                defined: false
            }
        );
        assert_eq!(count, 2);
        let names = table_names(&conn, "env", &records).unwrap();
        let sql = format!(
            "SELECT * FROM ({}) ORDER BY id",
            records.select("env", &names)
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        let rows: Vec<(String, String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<duckdb::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                ("1".into(), "North".into(), Some("2.5".into())),
                ("2".into(), "West".into(), None)
            ]
        );
        let columns: Vec<String> = describe(&conn, &format!("({sql})"))
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(columns, ["id", "region", "column_3"]);
    }

    #[test]
    fn an_object_with_no_list_has_no_records() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE env AS SELECT 'ok' AS status, [1, 2] AS ids")
            .unwrap();
        assert_eq!(find_records(&conn, "env").unwrap(), None);
    }

    #[test]
    fn tells_one_object_from_others() {
        let dir = std::env::temp_dir().join(format!("da-nested-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let check = |text: &str| {
            let path = dir.join("f.json");
            std::fs::write(&path, text).unwrap();
            single_object(&path)
        };
        assert!(check("\u{feff} {\"a\": [{\"b\": \"}\"}]}\n"));
        assert!(check("{\"a\": 1, \"s\": \"\\\"{\"}"));
        assert!(!check("[{\"a\": 1}]"));
        assert!(!check("{\"a\": 1}\n{\"a\": 2}\n"));
        assert!(!check(""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
