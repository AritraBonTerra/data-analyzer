//! Deciding what to load from a file: the parts (tables) it holds, the import
//! plan in effect, the one-line summary, the file notes and the raw preview.
//! The finder itself is in [`crate::layout`]; this module turns a CSV file or
//! a workbook's sheets into grids for it, and its findings into what
//! [`crate::Dataset`] reports.

use std::ops::RangeInclusive;
use std::path::Path;

use anyhow::Result;

use crate::layout::{self, Aside, Cell, GridKind, Layout, Table};

/// One table the user can look at: a table on a sheet, a table in a CSV, or a
/// sheet (or file) with no table found, which is read whole as before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    /// The table's caption, else its sheet's name (`Sheet · 2` when the sheet
    /// has several), else `Table 1`, `Table 2` in a CSV.
    pub label: String,
    /// Data rows, without header and totals rows. `None` for files read
    /// without being scanned: Parquet, JSON, and large or compressed CSV.
    pub rows: Option<u64>,
    /// Where it sits, like `B5:H45` or `lines 5 to 45`. `None` when not scanned.
    pub at: Option<String>,
}

/// Rows of a file or sheet, numbered from 1 as the preview numbers them, both
/// ends included. In a CSV a row is a line (a quoted value spanning lines
/// makes one row of several lines).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rows {
    pub first: usize,
    pub last: usize,
}

/// How to read the open part: the engine side of Adjust import. Pass one in
/// [`crate::OpenOptions::plan`] to override what was detected, and read the
/// one in effect from [`crate::Dataset::plan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportPlan {
    /// The header rows, joined top-down into column names. `None` names the
    /// columns `column_1`, `column_2`, ...
    pub header: Option<Rows>,
    /// The body. Blank rows and header rows inside it are skipped.
    pub data: Rows,
    /// Keep totals rows (`Total`, `North Total`, `Grand total`) in the data.
    pub keep_totals: bool,
}

/// The field delimiter of a CSV/TSV file. `Auto` picks the one that splits
/// the most lines into the same number of fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Delimiter {
    #[default]
    Auto,
    Comma,
    Semicolon,
    Tab,
    Pipe,
}

impl Delimiter {
    pub(crate) fn byte(self) -> u8 {
        match self {
            Self::Auto | Self::Comma => b',',
            Self::Semicolon => b';',
            Self::Tab => b'\t',
            Self::Pipe => b'|',
        }
    }
}

/// What a row of the file or sheet is under an import plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowRole {
    /// Set aside, above the table.
    Title,
    Header,
    Data,
    /// A totals row, left out of the data.
    Total,
    /// Set aside, below the table.
    Note,
    Blank,
}

/// A line the import set aside, kept so the user can still read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// Its line (CSV) or row (spreadsheet) number, from 1. For a JSON
    /// file, the field's place in the object holding the records.
    pub row: usize,
    /// [`RowRole::Title`], [`RowRole::Total`] or [`RowRole::Note`]. A JSON
    /// field beside the records is a title.
    pub role: RowRole,
    /// The line's cells, joined by spaces; for a JSON field, `key: value`.
    pub text: String,
}

/// The raw file or sheet, for drawing the import plan over it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// The first 200 rows and the last 50, in order. A jump in
    /// [`PreviewRow::number`] marks the rows left out between them.
    pub rows: Vec<PreviewRow>,
    /// Rows in the whole file or sheet.
    pub total_rows: usize,
    /// Rows the plan loads as data.
    pub data_rows: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewRow {
    /// From 1, as in [`Rows`].
    pub number: usize,
    /// The line as written, for CSV/TSV. `None` for spreadsheets.
    pub line: Option<String>,
    /// The row's cells (for CSV, split by the delimiter in effect).
    pub cells: Vec<String>,
    pub role: RowRole,
}

/// Rows the preview shows from the top and from the bottom.
const PREVIEW_HEAD: usize = 200;
const PREVIEW_TAIL: usize = 50;

/// CSV/TSV files larger than this skip the finder and open as before, so big
/// files keep their fast first rows. Hand-made messy files are far smaller;
/// at this size the scan adds about 110 ms to the first rows (release build,
/// a plain 4 MB file: 160 ms without it, 270 ms with it).
const SCAN_LIMIT: u64 = 4 << 20;

/// One CSV file or spreadsheet sheet as a grid, and what the finder made of it.
pub(crate) struct Scan {
    /// The sheet's name; `None` for a CSV.
    pub sheet: Option<String>,
    pub grid: Vec<Vec<Cell>>,
    pub layout: Layout,
    /// Data rows when the scan is read whole (no table found), as that
    /// reading counts them. For a sheet, always: see [`plain`].
    pub whole_rows: usize,
}

impl Scan {
    /// Runs the finder over a grid.
    pub fn new(sheet: Option<String>, grid: Vec<Vec<Cell>>, whole_rows: usize) -> Self {
        let kind = if sheet.is_some() {
            GridKind::Sheet
        } else {
            GridKind::Csv
        };
        let layout = layout::find(&grid, kind);
        Self {
            sheet,
            grid,
            layout,
            whole_rows,
        }
    }

    /// Where `table` sits: `lines 5 to 45` in a CSV, `B5:H45` in a sheet.
    fn at(&self, table: &Table) -> String {
        if self.sheet.is_none() {
            format!("lines {} to {}", table.top() + 1, table.bottom() + 1)
        } else {
            format!(
                "{}:{}",
                cell_name(table.top(), *table.cols.start()),
                cell_name(table.bottom(), *table.cols.end())
            )
        }
    }

    /// `at` as it reads after "Table": `on lines 5 to 45` or `at B5:H45`.
    fn place(&self, table: &Table) -> String {
        let at = self.at(table);
        if self.sheet.is_none() {
            format!("on {at}")
        } else {
            format!("at {at}")
        }
    }
}

/// A spreadsheet cell's name, like `B5`, from 0-based row and column.
fn cell_name(row: usize, col: usize) -> String {
    let mut letters = Vec::new();
    let mut n = col + 1;
    while n > 0 {
        letters.push(b'A' + ((n - 1) % 26) as u8);
        n = (n - 1) / 26;
    }
    letters.reverse();
    format!("{}{}", String::from_utf8_lossy(&letters), row + 1)
}

/// Reads a plain CSV/TSV file into a grid for the finder, splitting it on
/// `delimiter` (or the best one, for `Auto`). Returns the scan and the
/// delimiter used, or `None` for files that keep today's reading untouched:
/// compressed, over [`SCAN_LIMIT`], or not UTF-8.
pub(crate) fn scan_csv(
    path: &Path,
    tsv: bool,
    delimiter: Delimiter,
) -> Result<Option<(Scan, Delimiter)>> {
    let Some(CsvGrid {
        grid, delimiter, ..
    }) = csv_grid(path, tsv, delimiter)?
    else {
        return Ok(None);
    };
    let mut scan = Scan::new(None, grid, 0);
    // Only a scan with no table is read whole, so only then count its rows that way.
    if scan.layout.tables.is_empty() {
        scan.whole_rows = layout::whole(&scan.grid).map_or(0, |t| t.body.len());
    }
    Ok(Some((scan, delimiter)))
}

/// A CSV/TSV file split into cells: each record's cells, its text as
/// written, and the delimiter it was split on.
pub(crate) struct CsvGrid {
    pub grid: Vec<Vec<Cell>>,
    pub lines: Vec<String>,
    pub delimiter: Delimiter,
}

/// Splits a plain CSV/TSV file into a grid, as [`scan_csv`] does before
/// running the finder. `None` for the files it leaves alone.
pub(crate) fn csv_grid(path: &Path, tsv: bool, delimiter: Delimiter) -> Result<Option<CsvGrid>> {
    if crate::dataset::compressed(path) || std::fs::metadata(path)?.len() > SCAN_LIMIT {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let delimiter = match delimiter {
        Delimiter::Auto => sniff(text, tsv),
        chosen => chosen,
    };

    let records = split_records(text, delimiter.byte());
    // `#` lines before the table are comments, each one note and never split,
    // when they split into a different number of fields than the first line
    // after them. Otherwise, as in `#id,name,qty`, the line is the header.
    let fields = records
        .iter()
        .find(|(line, cells)| !line.starts_with('#') && cells.iter().any(|c| !c.trim().is_empty()))
        .map(|(_, cells)| cells.len());
    let mut grid = Vec::with_capacity(records.len());
    let mut lines = Vec::with_capacity(records.len());
    let mut preamble = true;
    for (line, cells) in records {
        if preamble && line.starts_with('#') && Some(cells.len()) != fields {
            grid.push(vec![Cell::from_text(line)]);
        } else {
            preamble &= cells.iter().all(|c| c.trim().is_empty());
            grid.push(cells.into_iter().map(Cell::from_text).collect());
        }
        lines.push(line.to_string());
    }
    Ok(Some(CsvGrid {
        grid,
        lines,
        delimiter,
    }))
}

/// Lines the delimiter sniffer looks at.
const SNIFF_LINES: usize = 2000;

/// Picks the delimiter that gives the most consistent multi-field split:
/// the most lines with the same field count, weighted by that count.
fn sniff(text: &str, tsv: bool) -> Delimiter {
    let end = text
        .match_indices('\n')
        .nth(SNIFF_LINES)
        .map_or(text.len(), |(i, _)| i);
    let sample = &text[..end];
    let score = |delimiter: Delimiter| {
        let mut counts = std::collections::HashMap::new();
        for (line, cells) in split_records(sample, delimiter.byte()) {
            if cells.len() > 1 && !line.starts_with('#') {
                *counts.entry(cells.len()).or_insert(0) += 1;
            }
        }
        counts
            .into_iter()
            .map(|(width, lines)| width * lines)
            .max()
            .unwrap_or(0)
    };
    let mut best = (
        if tsv {
            Delimiter::Tab
        } else {
            Delimiter::Comma
        },
        0,
    );
    for delimiter in [
        Delimiter::Comma,
        Delimiter::Semicolon,
        Delimiter::Tab,
        Delimiter::Pipe,
    ] {
        let points = score(delimiter);
        if points > best.1 {
            best = (delimiter, points);
        }
    }
    best.0
}

/// Splits CSV text into records: each one's text as written and its fields.
/// Handles quoted fields with delimiters, doubled quotes and line breaks
/// inside. A blank line is a record with one empty field.
fn split_records(text: &str, delimiter: u8) -> Vec<(&str, Vec<String>)> {
    let bytes = text.as_bytes();
    let mut records = Vec::new();
    let mut fields = Vec::new();
    // A plain field is sliced from `field_start`; one that opens with a quote
    // is built in `field` instead.
    let mut field_start = 0;
    let mut field = Vec::new();
    let mut has_quotes = false;
    let mut quoted = false;
    let mut start = 0;
    let mut i = 0;
    // Ends the field at byte `end`. Every split byte is ASCII, so slices stay on char boundaries.
    let take = |field: &mut Vec<u8>, has_quotes: bool, field_start: usize, end: usize| {
        if has_quotes {
            String::from_utf8_lossy(&std::mem::take(field)).into_owned()
        } else {
            text[field_start..end].to_string()
        }
    };
    while i < bytes.len() {
        let b = bytes[i];
        if quoted {
            if b == b'"' && bytes.get(i + 1) == Some(&b'"') {
                field.push(b'"');
                i += 1;
            } else if b == b'"' {
                quoted = false;
            } else {
                field.push(b);
            }
        } else if b == b'"' && i == field_start && !has_quotes {
            quoted = true;
            has_quotes = true;
        } else if b == delimiter || b == b'\n' || b == b'\r' {
            fields.push(take(&mut field, has_quotes, field_start, i));
            has_quotes = false;
            if b != delimiter {
                records.push((&text[start..i], std::mem::take(&mut fields)));
                if b == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            field_start = i + 1;
        } else if has_quotes {
            field.push(b);
        }
        i += 1;
    }
    if start < bytes.len() {
        fields.push(take(&mut field, has_quotes, field_start, bytes.len()));
        records.push((&text[start..], fields));
    }
    records
}

/// Writes `table` as a plain CSV: one header line with its names, then its
/// body rows. DuckDB reads this in place of the original file.
pub(crate) fn write_csv(scan: &Scan, table: &Table, path: &Path) -> Result<()> {
    let mut out = String::new();
    push_csv_row(&mut out, table.columns.iter().map(|c| c.name.as_str()));
    for &row in &table.body {
        let cells = scan.grid.get(row).map_or(&[][..], Vec::as_slice);
        push_csv_row(
            &mut out,
            table
                .columns
                .iter()
                .map(|c| layout::cell(cells, c.col).text.as_str()),
        );
    }
    std::fs::write(path, out)?;
    Ok(())
}

fn push_csv_row<'a>(out: &mut String, fields: impl Iterator<Item = &'a str>) {
    for (i, field) in fields.enumerate() {
        if i > 0 {
            out.push(',');
        }
        if field.contains([',', '"', '\n', '\r']) {
            out.push('"');
            out.push_str(&field.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(field);
        }
    }
    out.push('\n');
}

/// Where a part comes from: a scan, and one of its tables or the scan read whole.
#[derive(Clone, Copy)]
struct PartSource {
    scan: usize,
    table: Option<usize>,
}

/// What to load for the open part.
pub(crate) enum Load {
    /// Read the file or sheet as the app did before the finder.
    Whole { scan: usize },
    /// Read this table of scan `scan`.
    Table { scan: usize, table: Table },
}

/// What the import found and chose, as [`crate::Dataset`] reports it.
pub(crate) struct Import {
    pub parts: Vec<Part>,
    pub part: usize,
    pub plan: Option<ImportPlan>,
    pub delimiter: Option<Delimiter>,
    pub summary: Option<String>,
    pub notes: Vec<Note>,
    /// Column name and unit, for columns that have one.
    pub units: Vec<(String, String)>,
    /// The open part's scan: its sheet's index in the workbook, or 0 for a CSV.
    pub scan: usize,
    /// The columns the open part spans, so [`Import::preview`] can read the
    /// table under another plan as the loader would.
    cols: RangeInclusive<usize>,
}

impl Import {
    /// For a file read without scanning: one part, nothing else to report.
    pub fn unscanned() -> Self {
        Self {
            parts: vec![Part {
                label: "Table 1".into(),
                rows: None,
                at: None,
            }],
            part: 0,
            plan: None,
            delimiter: None,
            summary: None,
            notes: Vec::new(),
            units: Vec::new(),
            scan: 0,
            cols: 0..=0,
        }
    }

    /// Lists the parts of the scans, opens `part` (or the one with the most
    /// data rows) and applies `plan` to it. `as_before` says a plain table
    /// keeps today's reading of the file or sheet: false for a CSV split on
    /// a delimiter the user chose.
    pub fn new(
        scans: &[Scan],
        part: Option<usize>,
        plan: Option<&ImportPlan>,
        as_before: bool,
    ) -> (Self, Load) {
        let (parts, sources) = list_parts(scans);
        let index = part
            .unwrap_or_else(|| {
                // The most data rows; the first wins ties.
                let mut best = 0;
                for (i, part) in parts.iter().enumerate() {
                    if part.rows > parts[best].rows {
                        best = i;
                    }
                }
                best
            })
            .min(parts.len().saturating_sub(1));
        let source = sources[index];
        let scan = &scans[source.scan];
        let detected = match source.table {
            Some(t) => Some(scan.layout.tables[t].clone()),
            None => layout::whole(&scan.grid),
        };
        let table = match (plan, detected) {
            (Some(plan), Some(detected)) => Some(apply(
                &scan.grid,
                detected.cols.clone(),
                detected.caption.clone(),
                plan,
            )),
            (_, detected) => detected,
        };

        let mut import = Self {
            parts,
            part: index,
            plan: table.as_ref().map(|t| ImportPlan {
                header: t.header.as_ref().map(rows),
                data: rows(&t.data),
                keep_totals: t.keep_totals,
            }),
            delimiter: None,
            summary: None,
            notes: Vec::new(),
            units: Vec::new(),
            scan: source.scan,
            cols: table.as_ref().map_or(0..=0, |t| t.cols.clone()),
        };
        let in_table = plan.is_some() || source.table.is_some();
        if let (Some(table), true) = (&table, in_table) {
            let tables = scan.layout.tables.len().max(1);
            import.notes = match plan {
                Some(_) => left_out(&scan.grid, table),
                None => notes(scan, table),
            };
            import.summary = summary(scan, table, tables, &import.notes);
            import.units = table
                .columns
                .iter()
                .filter_map(|c| Some((c.name.clone(), c.unit.clone()?)))
                .collect();
        }
        let load = match table {
            Some(table)
                if in_table && !(as_before && plan.is_none() && plain(scan, &table, &import)) =>
            {
                Load::Table {
                    scan: source.scan,
                    table,
                }
            }
            _ => Load::Whole { scan: source.scan },
        };
        (import, load)
    }

    /// The first rows and last rows of the open part's file or sheet, read
    /// again as `grid` (and, for a CSV, each row's text as written in
    /// `lines`), each with its role under `plan`. `None` for an empty
    /// grid. Reads the table under `plan` as loading would, so a
    /// row shows as a total exactly when loading would leave it out.
    pub fn preview(
        &self,
        grid: &[Vec<Cell>],
        lines: &[String],
        plan: &ImportPlan,
    ) -> Option<Preview> {
        if grid.is_empty() {
            return None;
        }
        let table = apply(grid, self.cols.clone(), None, plan);
        let len = grid.len();
        let tail = len.saturating_sub(PREVIEW_TAIL).max(PREVIEW_HEAD.min(len));
        let rows = (0..PREVIEW_HEAD.min(len))
            .chain(tail..len)
            .map(|row| {
                let cells = &grid[row];
                PreviewRow {
                    number: row + 1,
                    line: lines.get(row).cloned(),
                    cells: cells.iter().map(|c| c.text.clone()).collect(),
                    role: role(row, cells, &table),
                }
            })
            .collect();
        Some(Preview {
            rows,
            total_rows: len,
            data_rows: table.body.len(),
        })
    }
}

/// Every part of every scan, with where each comes from.
fn list_parts(scans: &[Scan]) -> (Vec<Part>, Vec<PartSource>) {
    let mut parts = Vec::new();
    let mut sources = Vec::new();
    for (index, scan) in scans.iter().enumerate() {
        let tables = &scan.layout.tables;
        if tables.is_empty() {
            let whole = layout::whole(&scan.grid);
            parts.push(Part {
                label: scan.sheet.clone().unwrap_or_else(|| "Table 1".into()),
                rows: Some(scan.whole_rows as u64),
                at: whole.as_ref().map(|t| scan.at(t)),
            });
            sources.push(PartSource {
                scan: index,
                table: None,
            });
        }
        for (t, table) in tables.iter().enumerate() {
            // A caption tells several tables apart; a lone table keeps its sheet's name.
            let label = match (&table.caption, &scan.sheet) {
                (Some(caption), _) if tables.len() > 1 => caption.text.clone(),
                (_, Some(sheet)) if tables.len() > 1 => format!("{sheet} · {}", t + 1),
                (_, Some(sheet)) => sheet.clone(),
                (_, None) => format!("Table {}", t + 1),
            };
            parts.push(Part {
                label,
                rows: Some(table.body.len() as u64),
                at: Some(scan.at(table)),
            });
            sources.push(PartSource {
                scan: index,
                table: Some(t),
            });
        }
    }
    (parts, sources)
}

/// The table at `cols` read with the plan's rows in place of the finder's.
fn apply(
    grid: &[Vec<Cell>],
    cols: RangeInclusive<usize>,
    caption: Option<Aside>,
    plan: &ImportPlan,
) -> Table {
    let last = grid.len().saturating_sub(1);
    let range =
        |rows: Rows| rows.first.saturating_sub(1).min(last)..=rows.last.saturating_sub(1).min(last);
    Table::new(
        grid,
        cols,
        plan.header.map(range),
        range(plan.data),
        plan.keep_totals,
        caption,
    )
}

/// 0-based rows as 1-based [`Rows`].
fn rows(range: &RangeInclusive<usize>) -> Rows {
    Rows {
        first: range.start() + 1,
        last: range.end() + 1,
    }
}

/// Whether a table is the whole file or sheet read plainly, so it can be
/// read as before: nothing to say about it, and it spans every column. An
/// empty spacer column inside it may be left out of the table; reading as
/// before keeps it, as `column2`. A sheet must also have the rows its
/// reading before gives, so a blank row inside it is not counted differently.
fn plain(scan: &Scan, table: &Table, import: &Import) -> bool {
    let width = scan.grid.iter().map(Vec::len).max().unwrap_or(0);
    import.summary.is_none()
        && *table.cols.start() == 0
        && table.cols.end() + 1 == width
        && (scan.sheet.is_none() || table.body.len() == scan.whole_rows)
}

/// The one-line summary of what the import did, or `None` when it found a
/// plain table at the top left with nothing set aside. `set_aside` is the
/// file notes: every row the table leaves out.
fn summary(scan: &Scan, table: &Table, tables: usize, set_aside: &[Note]) -> Option<String> {
    let top = table.top();
    let count = |role: RowRole| set_aside.iter().filter(|n| n.role == role).count();
    let titles = count(RowRole::Title);
    let notes = count(RowRole::Note);
    let totals = count(RowRole::Total);
    // Notes are numbered from 1, the table's rows from 0.
    let outside = set_aside
        .iter()
        .any(|n| n.row <= top || n.row > table.bottom() + 1);

    let mut sentences = Vec::new();
    if tables > 1 {
        let place = match &scan.sheet {
            Some(sheet) => format!("on sheet {sheet}"),
            None => "in the file".into(),
        };
        sentences.push(format!(
            "{tables} tables {place}. Showing {}.",
            scan.at(table)
        ));
    } else if top > 0 || *table.cols.start() > 0 || outside {
        sentences.push(format!("Table found {}.", scan.place(table)));
    }
    let header_rows = table.header_rows();
    if header_rows > 1 || table.year_header {
        if sentences.is_empty() {
            sentences.push(format!("Table {}.", scan.place(table)));
        }
        sentences.push(if header_rows > 1 {
            format!("{header_rows} header rows joined into one.")
        } else {
            "Header row holds years.".into()
        });
    }
    if titles + notes == 0 && totals > 0 {
        sentences.push(if totals == 1 {
            "1 total row set aside. It is left out of stats and charts.".into()
        } else {
            format!("{totals} total rows set aside. They are left out of stats and charts.")
        });
    } else if titles + notes > 0 {
        let line = if scan.sheet.is_some() { "row" } else { "line" };
        let items: Vec<String> = [
            (titles, format!("title {line}")),
            (totals, "total".into()),
            (notes, "note".into()),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}{}", if n == 1 { "" } else { "s" }))
        .collect();
        sentences.push(format!("Set aside {}.", join_and(&items)));
    }
    // Only a plan keeps totals, so say what it changed.
    match table.totals.len() {
        0 => {}
        _ if !table.keep_totals => {}
        1 => sentences.push("1 total row kept in the data.".into()),
        n => sentences.push(format!("{n} total rows kept in the data.")),
    }
    (!sentences.is_empty()).then(|| sentences.join(" "))
}

/// `a`, `a and b`, `a, b and c`.
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Every row `table` leaves out, in file order: the scan's set-aside lines
/// outside it, its caption, and its totals rows unless they are kept. Roles
/// are relative to the table.
fn notes(scan: &Scan, table: &Table) -> Vec<Note> {
    let outside = scan
        .layout
        .notes
        .iter()
        .filter(|n| n.row < table.top() || n.row > table.bottom())
        .map(|n| Note {
            row: n.row + 1,
            role: if n.total {
                RowRole::Total
            } else if n.row < table.top() {
                RowRole::Title
            } else {
                RowRole::Note
            },
            text: n.text.clone(),
        });
    let caption = table.caption.iter().map(|c| Note {
        row: c.row + 1,
        role: RowRole::Title,
        text: c.text.clone(),
    });
    let totals = table
        .totals
        .iter()
        .filter(|_| !table.keep_totals)
        .map(|&row| Note {
            row: row + 1,
            role: RowRole::Total,
            text: layout::joined(scan.grid.get(row).map_or(&[], Vec::as_slice), &table.cols),
        });
    let mut notes: Vec<Note> = outside.chain(caption).chain(totals).collect();
    notes.sort_by_key(|n| n.row);
    notes
}

/// Under a plan: every row the plan's `table` leaves out, with its role as
/// [`Import::preview`] gives it and its whole text (a sheet's title can sit
/// left of the table's columns).
fn left_out(grid: &[Vec<Cell>], table: &Table) -> Vec<Note> {
    grid.iter()
        .enumerate()
        .filter_map(|(row, cells)| {
            let role = role(row, cells, table);
            let out = match role {
                RowRole::Title | RowRole::Note | RowRole::Total => true,
                RowRole::Header | RowRole::Data | RowRole::Blank => false,
            };
            out.then(|| Note {
                row: row + 1,
                role,
                text: layout::joined(cells, &(0..=cells.len().saturating_sub(1))),
            })
        })
        .collect()
}

/// The role of 0-based `row` (holding `cells`) when `table` is loaded. In the
/// body, a total is a row the table set aside; below it, any row that reads
/// as a totals row.
fn role(row: usize, cells: &[Cell], table: &Table) -> RowRole {
    if cells.iter().all(Cell::is_empty) {
        return RowRole::Blank;
    }
    if table.header.as_ref().is_some_and(|h| h.contains(&row)) {
        return RowRole::Header;
    }
    if row < *table.data.start() {
        RowRole::Title
    } else if row <= *table.data.end() {
        if !table.keep_totals && table.totals.contains(&row) {
            RowRole::Total
        } else {
            RowRole::Data
        }
    } else if layout::is_total_row(cells, &table.cols) {
        RowRole::Total
    } else {
        RowRole::Note
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_fields_and_keeps_blank_lines() {
        let records = split_records("a,\"b,c\"\r\n\n\"x\"\"y\",\"two\nlines\"\nlast", b',');
        let fields: Vec<Vec<String>> = records.iter().map(|(_, f)| f.clone()).collect();
        assert_eq!(
            fields,
            [
                vec!["a", "b,c"],
                vec![""],
                vec!["x\"y", "two\nlines"],
                vec!["last"],
            ]
        );
        assert_eq!(records[0].0, "a,\"b,c\"");
    }

    #[test]
    fn sniffs_the_delimiter() {
        assert_eq!(
            sniff("a;b;c\n1;2,5;3\n4;5;6\n", false),
            Delimiter::Semicolon
        );
        assert_eq!(
            sniff("Title, with comma\na\tb\n1\t2\n3\t4\n", false),
            Delimiter::Tab
        );
        assert_eq!(sniff("one\ncolumn\n", true), Delimiter::Tab);
    }

    #[test]
    fn names_cells() {
        assert_eq!(cell_name(4, 1), "B5");
        assert_eq!(cell_name(0, 26), "AA1");
    }
}
