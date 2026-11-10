//! The table finder. Looks at a file as a plain grid of cells, finds the
//! blocks shaped like tables and works out each one's header rows, column
//! names, units and totals rows. Everything else is set aside as notes.
//! Pure, no IO: CSV and Excel both turn their file into a grid first.
//!
//! Rows and columns here count from 0.

use std::ops::{Range, RangeInclusive};

use crate::clean::{self, Mark};

/// One cell of the grid. It is empty when its text is blank.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Cell {
    /// The text as written (CSV) or as Excel would show it.
    pub text: String,
    /// A number, date or boolean, or text that parses as a number.
    pub value: bool,
}

impl Cell {
    /// A cell read from text, which is value-like when it reads as a number.
    pub fn from_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let value = number(&text).is_some();
        Self { text, value }
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    fn is_label(&self) -> bool {
        !self.value && !self.is_empty()
    }

    /// An integer from 1900 to 2100: a value, but a label when it heads a
    /// column whose body is not years too.
    fn is_year(&self) -> bool {
        self.value
            && self
                .text
                .trim()
                .parse::<u16>()
                .is_ok_and(|year| (1900..=2100).contains(&year))
    }
}

/// Reads a number as people write it: `1234`, `-1.5`, `$1,234.00`, `12.6%`,
/// `(4.2%)` (negative). A percentage reads as written, so `12.6%` is 12.6.
/// Commas only count as thousands separators in groups of three. The
/// cleaner reads numbers by the same rules ([`clean::read_number`]).
pub(crate) fn number(text: &str) -> Option<f64> {
    clean::read_number(text, Mark::Point).map(|n| n.value)
}

/// What the finder made of a grid.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Layout {
    pub tables: Vec<Table>,
    /// Lines outside every table, in grid order. Captions are kept by their
    /// table instead (see [`Table::caption`]).
    pub notes: Vec<Aside>,
}

/// One line set aside from the tables.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Aside {
    pub row: usize,
    /// The row's filled cells, joined by spaces.
    pub text: String,
    /// A totals line just below a table, separated from it by a blank line.
    pub total: bool,
}

/// A table in the grid and how to read it. Build one with [`Table::new`],
/// which works out everything below `caption` from the rows it is given.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Table {
    pub cols: RangeInclusive<usize>,
    /// Header rows, joined top-down into names. `None` names columns `column_N`.
    pub header: Option<RangeInclusive<usize>>,
    /// The body. Blank rows and header rows inside it are skipped.
    pub data: RangeInclusive<usize>,
    pub keep_totals: bool,
    /// A lone line just above the table. It is set aside like a title, and
    /// names the table when its file or sheet holds several.
    pub caption: Option<Aside>,
    /// The columns to load, in order. Empty unnamed columns are left out.
    pub columns: Vec<TableColumn>,
    /// Body rows that are totals rows.
    pub totals: Vec<usize>,
    /// Rows to load: non-blank body rows, minus totals rows unless they are kept.
    pub body: Vec<usize>,
    /// Whether a header row holds years where the body holds numbers.
    pub year_header: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TableColumn {
    pub col: usize,
    pub name: String,
    /// From a units row such as `(USD)` in the header.
    pub unit: Option<String>,
}

static EMPTY: Cell = Cell {
    text: String::new(),
    value: false,
};

/// The cell at `col` of `row`, or an empty one past the end of a ragged row.
pub(crate) fn cell(row: &[Cell], col: usize) -> &Cell {
    row.get(col).unwrap_or(&EMPTY)
}

fn row_of(grid: &[Vec<Cell>], row: usize) -> &[Cell] {
    grid.get(row).map_or(&[], Vec::as_slice)
}

/// The filled cells of `row` within `cols`, with their column.
fn filled<'a>(
    row: &'a [Cell],
    cols: &RangeInclusive<usize>,
) -> impl Iterator<Item = (usize, &'a Cell)> + 'a {
    cols.clone()
        .map(move |col| (col, cell(row, col)))
        .filter(|(_, cell)| !cell.is_empty())
}

fn is_blank(row: &[Cell], cols: &RangeInclusive<usize>) -> bool {
    filled(row, cols).next().is_none()
}

/// The filled cells of `row` within `cols`, joined by spaces.
pub(crate) fn joined(row: &[Cell], cols: &RangeInclusive<usize>) -> String {
    filled(row, cols)
        .map(|(_, cell)| cell.text.trim())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A totals label: `Total`, `Totals`, `Subtotal`, `Sub total`, `Sub-total`,
/// `Grand total`, or a label ending in one of them (`North Total`), in any
/// case and with an optional trailing colon. `Total Care Shampoo` is not one.
fn is_total_label(text: &str) -> bool {
    let text = text.trim().trim_end_matches(':').to_lowercase();
    text.split_whitespace()
        .next_back()
        .is_some_and(|last| matches!(last, "total" | "totals" | "subtotal" | "sub-total"))
}

/// Text that announces a note rather than a value: it starts with a footnote
/// mark (`*`, `†`) or with `Note:`, `Notes:`, `Source:` or `Sources:`.
/// Kept narrow on purpose: a one-cell line at the foot of a sheet's table is
/// data unless it says it is a note.
fn is_note_text(text: &str) -> bool {
    let text = text.trim();
    text.starts_with(['*', '†', '‡'])
        || text.split_once(':').is_some_and(|(head, _)| {
            matches!(
                head.trim().to_lowercase().as_str(),
                "note" | "notes" | "source" | "sources"
            )
        })
}

/// Whether a row reads as a totals row: its first filled cell is a totals
/// label and every other filled cell is a value. [`is_total`] also checks
/// its sums.
pub(crate) fn is_total_row(row: &[Cell], cols: &RangeInclusive<usize>) -> bool {
    let mut cells = filled(row, cols);
    cells
        .next()
        .is_some_and(|(_, first)| is_total_label(&first.text))
        && cells.all(|(_, cell)| cell.value)
}

/// Whether `row` is a totals row over the data rows above it: it reads as one,
/// and in at least one numeric column its value is the sum of the rows since
/// the last totals row (`since`) or of all of them (`all`). A row with no
/// numeric column to check passes on its label alone.
fn is_total(
    grid: &[Vec<Cell>],
    row: usize,
    cols: &RangeInclusive<usize>,
    since: &[usize],
    all: &[usize],
) -> bool {
    if !is_total_row(row_of(grid, row), cols) {
        return false;
    }
    // Each column's value in the totals row and its sums, where every data cell reads as a number.
    let checked: Vec<(f64, f64, f64)> = cols
        .clone()
        .filter_map(|col| {
            let total = number(&cell(row_of(grid, row), col).text)?;
            let sum = |rows: &[usize]| -> Option<f64> {
                rows.iter().try_fold(0.0, |sum, &r| {
                    let cell = cell(row_of(grid, r), col);
                    if cell.is_empty() {
                        Some(sum)
                    } else {
                        number(&cell.text).map(|n| sum + n)
                    }
                })
            };
            Some((total, sum(since)?, sum(all)?))
        })
        .collect();
    let close = |a: f64, b: f64| (a - b).abs() <= 0.01_f64.max(b.abs() * 1e-3);
    all.is_empty()
        || checked.is_empty()
        || checked
            .iter()
            .any(|&(total, since, all)| close(total, since) || close(total, all))
}

/// A header row like `(each), (USD)`: every filled cell is in parentheses.
fn is_units_row(row: &[Cell], cols: &RangeInclusive<usize>) -> bool {
    let mut cells = filled(row, cols).peekable();
    cells.peek().is_some()
        && cells.all(|(_, cell)| {
            let text = cell.text.trim();
            text.starts_with('(') && text.ends_with(')')
        })
}

/// Where a grid comes from, which says what the length of a row means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GridKind {
    /// Each row is a CSV record, as long as its number of fields.
    Csv,
    /// Each row is a spreadsheet row, as long as the sheet is wide.
    Sheet,
}

/// Finds the tables in a grid and sets everything else aside.
pub(crate) fn find(grid: &[Vec<Cell>], kind: GridKind) -> Layout {
    let mut layout = Layout::default();
    // Whether the last thing found was a table: a lone totals line or a
    // continuation right after it belongs to it.
    let mut after_table = false;
    for band in bands(grid) {
        for segment in peel_footnotes(grid, band, kind) {
            after_table = layout.add_segment(grid, segment, after_table);
        }
    }
    layout.notes.sort_by_key(|note| note.row);
    layout
}

/// The whole grid as one table, for a file or sheet with no table found:
/// every column, from the first filled row to the last.
pub(crate) fn whole(grid: &[Vec<Cell>]) -> Option<Table> {
    let width = grid.iter().map(Vec::len).max()?;
    let all = 0..=width.checked_sub(1)?;
    let first = grid.iter().position(|row| !is_blank(row, &all))?;
    let last = grid.iter().rposition(|row| !is_blank(row, &all))?;
    let header = detect_header(grid, first..last + 1, &all);
    let data = header.as_ref().map_or(first, |h| h.end() + 1)..=last;
    Some(Table::new(grid, all, header, data, false, None))
}

/// Runs of non-empty rows. Fully empty rows separate them.
fn bands(grid: &[Vec<Cell>]) -> Vec<Range<usize>> {
    let mut bands = Vec::new();
    let mut start = None;
    for (index, row) in grid.iter().enumerate() {
        let empty = row.iter().all(Cell::is_empty);
        match (start, empty) {
            (None, false) => start = Some(index),
            (Some(first), true) => {
                bands.push(first..index);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(first) = start {
        bands.push(first..grid.len());
    }
    bands
}

/// Splits trailing one-cell lines (footnotes) off a band whose other rows are
/// wider. Each becomes a one-row segment of its own, after the band. A
/// one-cell line is only a footnote on evidence that it is not a row of the
/// table: in a CSV, fewer fields than the body's rows (`Carol,` has as many
/// as `Bob,20`, so it is data); in a sheet, text that reads as a note
/// ([`is_note_text`]). A line filled like a row of the body, such as `bob,`
/// in a table with an often empty second column, is data either way.
fn peel_footnotes(grid: &[Vec<Cell>], band: Range<usize>, kind: GridKind) -> Vec<Range<usize>> {
    let filled_cols = |row: usize| -> Vec<usize> {
        let row = row_of(grid, row);
        (0..row.len()).filter(|&col| !row[col].is_empty()).collect()
    };
    let one_cell = |row: usize| filled_cols(row).len() == 1;
    let body_start = (band.start..band.end)
        .find(|&row| !one_cell(row))
        .unwrap_or(band.end);
    let body_end = (body_start..band.end)
        .rfind(|&row| !one_cell(row))
        .map_or(body_start, |row| row + 1);
    let in_body = |row: usize| (body_start..body_end).any(|r| filled_cols(r) == filled_cols(row));
    let fields = (body_start..body_end)
        .map(|row| row_of(grid, row).len())
        .max()
        .unwrap_or(0);
    let not_a_row = |row: usize| match kind {
        GridKind::Csv => row_of(grid, row).len() < fields,
        GridKind::Sheet => row_of(grid, row).iter().any(|c| is_note_text(&c.text)),
    };
    let mut end = band.end;
    while end > body_end && end - band.start > 1 && !in_body(end - 1) && not_a_row(end - 1) {
        end -= 1;
    }
    std::iter::once(band.start..end)
        .chain((end..band.end).map(|row| row..row + 1))
        .collect()
}

impl Layout {
    /// Classifies one segment of a band, splitting it into side-by-side blocks
    /// first when they are separate tables. Returns whether it held a table.
    fn add_segment(&mut self, grid: &[Vec<Cell>], rows: Range<usize>, after_table: bool) -> bool {
        let blocks = if rows.len() >= 2 {
            column_blocks(grid, &rows)
        } else {
            filled_span(row_of(grid, rows.start)).into_iter().collect()
        };
        // A block's rows can be empty at its edges, where a neighbour block is longer.
        let mut blocks: Vec<(Range<usize>, RangeInclusive<usize>)> = blocks
            .into_iter()
            .filter_map(|cols| Some((filled_rows(grid, &rows, &cols)?, cols)))
            .collect();
        if blocks.len() > 1 && !separate_tables(grid, &blocks) {
            // One table with empty spacer columns: keep it whole.
            let cols = *blocks[0].1.start()..=*blocks[blocks.len() - 1].1.end();
            blocks = filled_rows(grid, &rows, &cols)
                .map(|rows| (rows, cols))
                .into_iter()
                .collect();
        }
        let mut found = false;
        for (rows, cols) in blocks {
            found |= self.add_block(grid, rows, cols, after_table);
        }
        found
    }

    /// Adds one block as a table, a continuation of the table above, a totals
    /// line or notes. Returns whether it was a table.
    fn add_block(
        &mut self,
        grid: &[Vec<Cell>],
        mut rows: Range<usize>,
        cols: RangeInclusive<usize>,
        after_table: bool,
    ) -> bool {
        // Only the line right above the table names it; earlier ones are notes.
        let lines = lines_above(grid, &rows, &cols);
        let caption = (lines > 0).then(|| rows.start + lines - 1);
        for row in rows.start..caption.unwrap_or(rows.start) {
            self.note(grid, row, &cols, false);
        }
        rows.start += lines;

        // Whether the rows carry on the table above is decided before any
        // minimum size, so a single row after a blank line can stay data.
        if after_table && caption.is_none() && self.continues(grid, &rows, &cols) {
            let last = self.tables.pop().expect("continues checked it");
            let data = *last.data.start()..=rows.end - 1;
            self.tables.push(Table::new(
                grid,
                last.cols,
                last.header,
                data,
                false,
                last.caption,
            ));
            return true;
        }
        if is_table(grid, &rows, &cols) {
            let header = detect_header(grid, rows.clone(), &cols);
            let caption = caption.map(|row| Aside {
                row,
                text: joined(row_of(grid, row), &cols),
                total: false,
            });
            let data = header.as_ref().map_or(rows.start, |h| h.end() + 1)..=rows.end - 1;
            self.tables
                .push(Table::new(grid, cols, header, data, false, caption));
            return true;
        }

        let total = after_table && rows.len() == 1 && self.totals_line(grid, rows.start);
        for row in caption.into_iter().chain(rows) {
            self.note(grid, row, &cols, total);
        }
        false
    }

    fn note(&mut self, grid: &[Vec<Cell>], row: usize, cols: &RangeInclusive<usize>, total: bool) {
        let text = joined(row_of(grid, row), cols);
        if !text.is_empty() {
            self.notes.push(Aside { row, text, total });
        }
    }

    /// Whether `row` is the last table's totals line, checked against that
    /// table's data rows (its body: the finder's tables never keep totals rows).
    fn totals_line(&self, grid: &[Vec<Cell>], row: usize) -> bool {
        self.tables.last().is_some_and(|table| {
            let data = &table.body;
            let since = table.totals.last().map_or(0, |&last| {
                data.iter()
                    .position(|&row| row > last)
                    .unwrap_or(data.len())
            });
            is_total(grid, row, &table.cols, &data[since..], data)
        })
    }

    /// Whether `rows` carry on the last table after a blank line: the first
    /// row reads as data, not as a header of its own. Several rows need the
    /// table's columns. A single row needs the table's shape: two or more
    /// cells within its columns, values where the table has values, and not
    /// the table's totals line. Placeholders such as `N/A` count as neither
    /// label nor value, as in a data row (`Carol,N/A` carries on).
    fn continues(
        &self,
        grid: &[Vec<Cell>],
        rows: &Range<usize>,
        cols: &RangeInclusive<usize>,
    ) -> bool {
        let Some(last) = self.tables.last() else {
            return false;
        };
        let first = row_of(grid, rows.start);
        // The first row with its placeholders blanked, for the label and value checks.
        let plain: Vec<Cell> = first
            .iter()
            .map(|c| {
                if is_placeholder(c) {
                    EMPTY.clone()
                } else {
                    c.clone()
                }
            })
            .collect();
        let body = BodyColumns::new(grid, &last.body, &last.cols);
        if holds_labels(&plain, &body) {
            return false;
        }
        if rows.len() > 1 {
            return last.cols == *cols;
        }
        last.cols.contains(cols.start())
            && last.cols.contains(cols.end())
            && filled(first, cols).count() >= 2
            && body.values.iter().all(|&col| {
                let cell = cell(&plain, col);
                cell.is_empty() || cell.value
            })
            && !self.totals_line(grid, rows.start)
    }
}

/// Splits rows into side-by-side blocks on columns that are empty in every row.
fn column_blocks(grid: &[Vec<Cell>], rows: &Range<usize>) -> Vec<RangeInclusive<usize>> {
    let width = rows
        .clone()
        .map(|row| row_of(grid, row).len())
        .max()
        .unwrap_or(0);
    let used: Vec<bool> = (0..width)
        .map(|col| {
            rows.clone()
                .any(|row| !cell(row_of(grid, row), col).is_empty())
        })
        .collect();
    let mut blocks = Vec::new();
    let mut start = None;
    for (col, &on) in used.iter().chain([&false]).enumerate() {
        match (start, on) {
            (None, true) => start = Some(col),
            (Some(first), false) => {
                blocks.push(first..=col - 1);
                start = None;
            }
            _ => {}
        }
    }
    blocks
}

/// First to last row of `rows` with a filled cell in `cols`.
fn filled_rows(
    grid: &[Vec<Cell>],
    rows: &Range<usize>,
    cols: &RangeInclusive<usize>,
) -> Option<Range<usize>> {
    let mut filled = rows
        .clone()
        .filter(|&row| !is_blank(row_of(grid, row), cols));
    let first = filled.next()?;
    Some(first..filled.next_back().unwrap_or(first) + 1)
}

/// Whether side-by-side blocks (each with its rows) are separate tables
/// rather than one table with empty spacer columns: each is a table on its
/// own, and each has its own caption or they end on different rows. Tops are
/// not compared: a title over the first block, or a group label over a later
/// one, makes them differ in one table too.
fn separate_tables(grid: &[Vec<Cell>], blocks: &[(Range<usize>, RangeInclusive<usize>)]) -> bool {
    // Each block's caption lines and its rows below them.
    let tables: Vec<(usize, Range<usize>)> = blocks
        .iter()
        .map(|(rows, cols)| {
            let lines = lines_above(grid, rows, cols);
            (lines, rows.start + lines..rows.end)
        })
        .collect();
    let all_tables = tables
        .iter()
        .zip(blocks)
        .all(|((_, rows), (_, cols))| is_table(grid, rows, cols));
    let captioned = tables.iter().all(|(lines, _)| *lines > 0);
    let different_ends = blocks.iter().any(|(rows, _)| rows.end != blocks[0].0.end);
    all_tables && (captioned || different_ends)
}

/// Whether `rows` at `cols` make a table: two or more rows, and a row with
/// two or more filled cells.
fn is_table(grid: &[Vec<Cell>], rows: &Range<usize>, cols: &RangeInclusive<usize>) -> bool {
    rows.len() >= 2
        && rows
            .clone()
            .any(|row| filled(row_of(grid, row), cols).count() >= 2)
}

/// How many leading rows of a block are lines above its table rather than
/// part of it; the last of them is the caption. A one-cell row is such a line
/// over a wider table with a header row, unless a body row is filled the
/// same way (`ann,` over `cy,` in a file with no header): then it is data.
/// A one-cell row that would head a header of several rows is always such a
/// line: a title, never a group label.
fn lines_above(grid: &[Vec<Cell>], rows: &Range<usize>, cols: &RangeInclusive<usize>) -> usize {
    if cols.clone().count() < 2 {
        return 0;
    }
    let pattern = |row: usize| -> Vec<usize> {
        filled(row_of(grid, row), cols)
            .map(|(col, _)| col)
            .collect()
    };
    let end = rows.end;
    let body_start = rows
        .clone()
        .find(|&row| pattern(row).len() != 1)
        .unwrap_or(end);
    let in_body = |row: usize| (body_start..end).any(|r| pattern(r) == pattern(row));
    let headed = || body_start < end && detect_header(grid, body_start..end, cols).is_some();
    let heads_header =
        |row: usize| detect_header(grid, row..end, cols).is_some_and(|h| h.count() >= 2);
    let mut start = rows.start;
    while end - start > 1
        && pattern(start).len() == 1
        && ((!in_body(start) && headed()) || heads_header(start))
    {
        start += 1;
    }
    start - rows.start
}

/// First to last filled column of a row.
fn filled_span(row: &[Cell]) -> Option<RangeInclusive<usize>> {
    let first = row.iter().position(|c| !c.is_empty())?;
    let last = row.iter().rposition(|c| !c.is_empty())?;
    Some(first..=last)
}

/// Columns whose filled cells in `rows` are mostly `kind`.
fn columns_of(
    grid: &[Vec<Cell>],
    rows: &[usize],
    cols: &RangeInclusive<usize>,
    kind: fn(&Cell) -> bool,
) -> Vec<usize> {
    cols.clone()
        .filter(|&col| {
            let cells = rows.iter().map(|&row| cell(row_of(grid, row), col));
            let (filled, matching) = cells
                .filter(|cell| !cell.is_empty())
                .fold((0, 0), |(f, m), cell| (f + 1, m + usize::from(kind(cell))));
            filled > 0 && matching * 2 > filled
        })
        .collect()
}

/// The columns of `rows` that hold values, and those of them that hold years.
struct BodyColumns {
    values: Vec<usize>,
    years: Vec<usize>,
}

impl BodyColumns {
    fn new(grid: &[Vec<Cell>], rows: &[usize], cols: &RangeInclusive<usize>) -> Self {
        Self {
            values: columns_of(grid, rows, cols, |cell| cell.value),
            years: columns_of(grid, rows, cols, Cell::is_year),
        }
    }

    /// Whether `cell` in `col` is a label there: text, or a year heading a
    /// column whose body is not itself years.
    fn is_label(&self, cell: &Cell, col: usize) -> bool {
        cell.is_label() || (cell.is_year() && !self.years.contains(&col))
    }
}

/// A row that holds labels (or years) in the value columns and no other values there.
fn holds_labels(row: &[Cell], body: &BodyColumns) -> bool {
    let mut label = false;
    for &col in &body.values {
        let cell = cell(row, col);
        if body.is_label(cell, col) {
            label = true;
        } else if cell.value {
            return false;
        }
    }
    label
}

/// Most header rows a table can have.
const MAX_HEADER_ROWS: usize = 4;

/// A cell that stands for a missing value: `N/A`, `-`, `TBD`, `null`, an
/// Excel error like `#DIV/0!`. Neither a label nor a value.
fn is_placeholder(cell: &Cell) -> bool {
    clean::is_placeholder(&cell.text)
}

/// Whether `row` is one more header row under the header rows `above`: a
/// units row like `(USD)`, or a row with structural evidence of being a
/// header. It must leave alone every label column (one whose body is not
/// values) that a row above already names, and hold real labels in the value
/// columns, no values, and labels in at least two of them when there are two
/// or more. Placeholders such as `N/A` count for nothing. So `Alice,N/A`
/// under `name,amount` is data.
fn is_more_header(
    grid: &[Vec<Cell>],
    above: Range<usize>,
    row: &[Cell],
    cols: &RangeInclusive<usize>,
    body: &BodyColumns,
) -> bool {
    if !holds_labels(row, body) {
        return false;
    }
    if is_units_row(row, cols) {
        return true;
    }
    let named = |col: usize| {
        above.clone().any(|r| {
            let row = row_of(grid, r);
            !is_units_row(row, cols) && !is_placeholder(cell(row, col))
        })
    };
    let renames = cols
        .clone()
        .filter(|col| !body.values.contains(col))
        .any(|col| !is_placeholder(cell(row, col)) && named(col));
    let labels = body
        .values
        .iter()
        .filter(|&&col| {
            let cell = cell(row, col);
            !is_placeholder(cell) && body.is_label(cell, col)
        })
        .count();
    !renames && labels >= body.values.len().min(2)
}

/// Which leading rows of a block are its header. The block's later rows say
/// which columns hold values; a first row with labels there is the header,
/// and rows below it join it only as [`is_more_header`] allows. With no
/// value columns, the first row is the header when it is all text, leaving
/// aside spacer columns that are empty in every row.
fn detect_header(
    grid: &[Vec<Cell>],
    rows: Range<usize>,
    cols: &RangeInclusive<usize>,
) -> Option<RangeInclusive<usize>> {
    let tail: Vec<usize> = (rows.start + rows.len() / 3..rows.end).collect();
    let body = BodyColumns::new(grid, &tail, cols);
    let count = if body.values.is_empty() {
        let first = row_of(grid, rows.start);
        let spacer = |col: usize| {
            rows.clone()
                .all(|row| cell(row_of(grid, row), col).is_empty())
        };
        usize::from(
            cols.clone()
                .all(|col| cell(first, col).is_label() || spacer(col)),
        )
    } else {
        rows.clone()
            .take(MAX_HEADER_ROWS.min(rows.len() - 1))
            .take_while(|&row| {
                let cells = row_of(grid, row);
                if row == rows.start {
                    holds_labels(cells, &body)
                } else {
                    is_more_header(grid, rows.start..row, cells, cols, &body)
                }
            })
            .count()
    };
    (count > 0).then(|| rows.start..=rows.start + count - 1)
}

impl Table {
    /// Reads a table at `cols` with the given header and body rows: names its
    /// columns, finds its units and totals rows and picks the rows to load.
    pub fn new(
        grid: &[Vec<Cell>],
        cols: RangeInclusive<usize>,
        header: Option<RangeInclusive<usize>>,
        data: RangeInclusive<usize>,
        keep_totals: bool,
        caption: Option<Aside>,
    ) -> Self {
        let header_rows: Vec<&[Cell]> = header
            .clone()
            .into_iter()
            .flatten()
            .map(|row| row_of(grid, row))
            .collect();
        let rows: Vec<usize> = data
            .clone()
            .filter(|row| !header.as_ref().is_some_and(|h| h.contains(row)))
            .filter(|&row| !is_blank(row_of(grid, row), &cols))
            .collect();
        // Walk the body in order, so each totals row is checked against the data above it.
        let mut totals = Vec::new();
        let mut data_rows = Vec::new();
        let mut since = 0;
        for &row in &rows {
            if is_total(grid, row, &cols, &data_rows[since..], &data_rows) {
                totals.push(row);
                since = data_rows.len();
            } else {
                data_rows.push(row);
            }
        }
        let body = if keep_totals { rows.clone() } else { data_rows };

        let (units, labels): (Vec<&[Cell]>, Vec<&[Cell]>) =
            header_rows.iter().partition(|row| is_units_row(row, &cols));
        let body_cols = BodyColumns::new(grid, &body, &cols);
        let year_header = labels.iter().any(|row| {
            body_cols.values.iter().any(|&col| {
                let cell = cell(row, col);
                cell.is_year() && body_cols.is_label(cell, col)
            })
        });

        let mut columns = Vec::new();
        for (index, col) in cols.clone().enumerate() {
            let name = column_name(&labels, &cols, col);
            // Empty in every body and header row: a spacer or a trailing
            // delimiter. A group label carried over it does not name it.
            let empty = rows
                .iter()
                .map(|&row| row_of(grid, row))
                .chain(header_rows.iter().copied())
                .all(|row| cell(row, col).is_empty());
            if empty && !rows.is_empty() {
                continue;
            }
            let unit = units.last().and_then(|row| {
                let text = cell(row, col).text.trim();
                let text = text.trim_start_matches('(').trim_end_matches(')').trim();
                (!text.is_empty()).then(|| text.to_string())
            });
            columns.push(TableColumn {
                col,
                name: if name.is_empty() {
                    format!("column_{}", index + 1)
                } else {
                    name
                },
                unit,
            });
        }
        let names = unique_names(columns.iter().map(|c| c.name.clone()));
        for (column, name) in columns.iter_mut().zip(names) {
            column.name = name;
        }

        Self {
            cols,
            header,
            data,
            keep_totals,
            caption,
            columns,
            totals,
            body,
            year_header,
        }
    }

    /// The first row: the first header row, or the first body row.
    pub fn top(&self) -> usize {
        self.header
            .as_ref()
            .map_or(*self.data.start(), |h| *h.start().min(self.data.start()))
    }

    /// The last row: the end of the body, or of the header when the body is empty.
    pub fn bottom(&self) -> usize {
        let header_end = self.header.as_ref().map_or(0, |h| *h.end());
        (*self.data.end()).max(header_end)
    }

    pub fn header_rows(&self) -> usize {
        self.header.as_ref().map_or(0, |h| h.clone().count())
    }
}

/// One column's name: its header labels top-down, joined with ` · `. A group
/// label (a merged cell) fills only its first column, so in rows above the
/// last, an empty cell takes the nearest label to its left.
fn column_name(labels: &[&[Cell]], cols: &RangeInclusive<usize>, col: usize) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for (depth, row) in labels.iter().enumerate() {
        let mut text = cell(row, col).text.trim();
        if text.is_empty() && depth + 1 < labels.len() {
            text = (*cols.start()..col)
                .rev()
                .map(|left| cell(row, left).text.trim())
                .find(|t| !t.is_empty())
                .unwrap_or("");
        }
        if !text.is_empty() && parts.last() != Some(&text) {
            parts.push(text);
        }
    }
    parts.join(" · ")
}

/// Makes names unique, ignoring case: a repeat of `x` becomes `x_2`, `x_3`.
pub(crate) fn unique_names(raw: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for raw in raw {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A grid from CSV-ish lines: cells split on commas, numbers are values.
    fn grid(lines: &[&str]) -> Vec<Vec<Cell>> {
        lines
            .iter()
            .map(|line| {
                if line.is_empty() {
                    Vec::new()
                } else {
                    line.split(',').map(Cell::from_text).collect()
                }
            })
            .collect()
    }

    /// The finder over a grid of CSV records, which is what [`grid`] makes.
    fn find(grid: &[Vec<Cell>]) -> Layout {
        super::find(grid, GridKind::Csv)
    }

    fn names(table: &Table) -> Vec<&str> {
        table.columns.iter().map(|c| c.name.as_str()).collect()
    }

    fn caption(table: &Table) -> Option<&str> {
        table.caption.as_ref().map(|c| c.text.as_str())
    }

    #[test]
    fn sets_aside_titles_totals_and_notes() {
        let g = grid(&[
            "Weekly report",
            "Generated today",
            "",
            "id,region,units",
            "1,North,10",
            "2,South,20",
            "",
            "Total,,30",
            "Note: excludes returns",
        ]);
        let layout = find(&g);
        assert_eq!(layout.tables.len(), 1);
        let table = &layout.tables[0];
        assert_eq!(table.header, Some(3..=3));
        assert_eq!(table.data, 4..=5);
        assert_eq!(names(table), ["id", "region", "units"]);
        let notes: Vec<(usize, &str, bool)> = layout
            .notes
            .iter()
            .map(|n| (n.row, n.text.as_str(), n.total))
            .collect();
        assert_eq!(
            notes,
            [
                (0, "Weekly report", false),
                (1, "Generated today", false),
                (7, "Total 30", true),
                (8, "Note: excludes returns", false),
            ]
        );
    }

    #[test]
    fn blank_lines_inside_a_table_do_not_split_it() {
        let g = grid(&["id,units", "1,10", "2,20", "", "3,30", "4,40"]);
        let layout = find(&g);
        assert_eq!(layout.tables.len(), 1);
        assert_eq!(layout.tables[0].data, 1..=5);
        assert_eq!(layout.tables[0].body, [1, 2, 4, 5]);
    }

    #[test]
    fn a_new_header_after_a_blank_line_starts_a_new_table() {
        let g = grid(&["id,units", "1,10", "2,20", "", "id,units", "3,30", "4,40"]);
        assert_eq!(find(&g).tables.len(), 2);
    }

    #[test]
    fn stacked_tables_of_different_widths_are_separate() {
        let g = grid(&["a,b", "x,1", "y,2", "", "c,d,e", "p,1,2", "q,3,4"]);
        let layout = find(&g);
        assert_eq!(layout.tables.len(), 2);
        assert_eq!(names(&layout.tables[1]), ["c", "d", "e"]);
    }

    #[test]
    fn a_one_column_file_has_no_table() {
        let g = grid(&["name", "apple", "pear", "plum"]);
        let layout = find(&g);
        assert!(layout.tables.is_empty());
        let whole = whole(&g).unwrap();
        assert_eq!(whole.header, Some(0..=0));
        assert_eq!(whole.body, [1, 2, 3]);
    }

    #[test]
    fn side_by_side_tables_with_captions() {
        let g = grid(&[
            "Left,,,,Right,",
            "k,v,,,name,n",
            "a,1,,,x,5",
            "b,2,,,y,6",
            ",,,,z,7",
        ]);
        let layout = find(&g);
        assert_eq!(layout.tables.len(), 2);
        let (left, right) = (&layout.tables[0], &layout.tables[1]);
        assert_eq!(caption(left), Some("Left"));
        assert_eq!(
            (left.cols.clone(), left.top(), left.bottom()),
            (0..=1, 1, 3)
        );
        assert_eq!(caption(right), Some("Right"));
        assert_eq!((right.cols.clone(), right.body.len()), (4..=5, 3));
        assert!(layout.notes.is_empty());
    }

    #[test]
    fn a_spacer_column_does_not_split_a_table() {
        for (lines, expected) in [
            (
                &["id,name,,comment", "1,ann,,ok", "2,bob,,", "3,cy,,fine"][..],
                &["id", "name", "comment"][..],
            ),
            (
                &["Name,,Jan,Feb,,Total", "Ann,,1,2,,3", "Bob,,4,5,,9"],
                &["Name", "Jan", "Feb", "Total"],
            ),
            // Two blocks that are tables, but neither captioned nor of different rows.
            (
                &[
                    "Region,Product,,Units,Revenue",
                    "North,Oat,,10,1.5",
                    "South,Tea,,20,2.5",
                ],
                &["Region", "Product", "Units", "Revenue"],
            ),
            // A title over one half does not make the halves different.
            (
                &[
                    "Sales,,,,",
                    "Region,Product,,Units,Revenue",
                    "North,Oat,,10,1.5",
                    "South,Tea,,20,2.5",
                ],
                &["Region", "Product", "Units", "Revenue"],
            ),
            // A group label carried over the spacer does not name it.
            (
                &[
                    "Region,Q1,,,Q2,",
                    ",Units,Revenue,,Units,Revenue",
                    "North,1,2,,3,4",
                    "South,5,6,,7,8",
                ],
                &[
                    "Region",
                    "Q1 · Units",
                    "Q1 · Revenue",
                    "Q2 · Units",
                    "Q2 · Revenue",
                ],
            ),
        ] {
            let layout = find(&grid(lines));
            assert_eq!(layout.tables.len(), 1, "{lines:?}");
            assert_eq!(names(&layout.tables[0]), expected, "{lines:?}");
            assert!(layout.notes.is_empty(), "{lines:?}");
        }
        // Blocks covering different rows are separate tables.
        let g = grid(&["a,b,,c,d", "1,2,,x,5", "3,4,,y,6", ",,,z,7"]);
        let layout = find(&g);
        assert_eq!(layout.tables.len(), 2);
        assert_eq!(layout.tables[1].body, [1, 2, 3]);
    }

    #[test]
    fn a_one_cell_row_over_a_header_is_a_caption_not_a_group_label() {
        // The footer is filled like the title, which once made the title a header row.
        let g = grid(&[
            "Customer follow-ups",
            "id,customer,amount",
            "1,Ada,120.5",
            "2,Bob,80",
            "3,Cy,45",
            "Prepared by support team",
        ]);
        let table = &super::find(&g, GridKind::Sheet).tables[0];
        assert_eq!(caption(table), Some("Customer follow-ups"));
        assert_eq!(table.header, Some(1..=1));
        assert_eq!(names(table), ["id", "customer", "amount"]);
    }

    #[test]
    fn joins_header_rows_and_reads_units() {
        let g = grid(&[
            ",,Sales,,Stock",
            "Store,Week,Units,Revenue,On Hand",
            ",,(each),(USD),(each)",
            "S1,W1,1,2,3",
            "S2,W1,4,5,6",
            "S3,W1,7,8,9",
        ]);
        let table = &find(&g).tables[0];
        assert_eq!(table.header, Some(0..=2));
        assert_eq!(
            names(table),
            [
                "Store",
                "Week",
                "Sales · Units",
                "Sales · Revenue",
                "Stock · On Hand"
            ]
        );
        let units: Vec<Option<&str>> = table.columns.iter().map(|c| c.unit.as_deref()).collect();
        assert_eq!(units, [None, None, Some("each"), Some("USD"), Some("each")]);
    }

    #[test]
    fn text_in_a_value_column_does_not_make_a_second_header_row() {
        for first in [
            "Alice,N/A,5",
            "Alice,unknown,5",
            "Alice,pending,5",
            "Alice,-,-",
        ] {
            let g = grid(&[
                "name,amount,qty",
                first,
                "Bob,10,6",
                "Carol,20,7",
                "Dan,30,8",
            ]);
            let table = &find(&g).tables[0];
            assert_eq!(table.header, Some(0..=0), "{first}");
            assert_eq!(names(table), ["name", "amount", "qty"], "{first}");
        }
        // A group row over a row of names is still a two-row header.
        let g = grid(&[
            "Region,Q1,,Q2,",
            ",Units,Revenue,Units,Revenue",
            "North,1,2,3,4",
            "South,5,6,7,8",
        ]);
        let table = &find(&g).tables[0];
        assert_eq!(table.header, Some(0..=1));
        assert_eq!(names(table)[..2], ["Region", "Q1 · Units"]);
    }

    #[test]
    fn years_head_columns() {
        let g = grid(&["Region,2022,2023", "North,100,200", "South,300,400"]);
        let table = &find(&g).tables[0];
        assert_eq!(table.header, Some(0..=0));
        assert_eq!(names(table), ["Region", "2022", "2023"]);
        assert!(table.year_header);
    }

    #[test]
    fn a_column_of_years_is_data() {
        let g = grid(&[
            "title,year",
            "Alien,1979",
            "Heat,1995",
            "Ronin,1998",
            "Dune,2021",
        ]);
        let table = &find(&g).tables[0];
        assert_eq!(table.header, Some(0..=0));
        assert_eq!(table.body, [1, 2, 3, 4]);
        assert!(!table.year_header);
    }

    #[test]
    fn reads_numbers_as_people_write_them() {
        let read = |text| number(text);
        assert_eq!(read("1234"), Some(1234.0));
        assert_eq!(read(" $1,234.00 "), Some(1234.0));
        assert_eq!(read("12.6%"), Some(12.6));
        assert_eq!(read("(4.2%)"), Some(-4.2));
        assert_eq!(read("-£5"), Some(-5.0));
        for text in [
            "",
            "-",
            "abc",
            "1,5",
            "12,34,567",
            "1.234,5",
            "(-3)",
            "inf",
            "--5",
        ] {
            assert_eq!(read(text), None, "{text:?}");
        }
    }

    #[test]
    fn no_header_when_the_first_row_is_data() {
        let g = grid(&["1,North,10", "2,South,20", "3,East,30"]);
        let table = &find(&g).tables[0];
        assert_eq!(table.header, None);
        assert_eq!(names(table), ["column_1", "column_2", "column_3"]);
        assert_eq!(table.body, [0, 1, 2]);
    }

    #[test]
    fn totals_rows_are_left_out_unless_kept() {
        let g = grid(&[
            "Region,Product,Units",
            "North,Oat,1",
            "North,Tea,2",
            "North Total,,3",
            "Grand Total,,3",
        ]);
        let table = &find(&g).tables[0];
        assert_eq!(table.totals, [3, 4]);
        assert_eq!(table.body, [1, 2]);
        let kept = Table::new(
            &g,
            table.cols.clone(),
            table.header.clone(),
            table.data.clone(),
            true,
            None,
        );
        assert_eq!(kept.body, [1, 2, 3, 4]);
        // A product whose name starts with Total is data: `Total Gym` is not a totals label.
        assert!(!is_total_row(&grid(&["Total Gym,Gear,5"])[0], &(0..=2)));
    }

    #[test]
    fn a_totals_row_needs_a_totals_label_and_matching_sums() {
        let g = grid(&[
            "product,units,revenue",
            "Total Care Shampoo,5,50",
            "Soap,3,9",
            "Total,8,59",
        ]);
        let table = &find(&g).tables[0];
        assert_eq!(
            (table.totals.as_slice(), table.body.as_slice()),
            (&[3][..], &[1, 2][..])
        );

        // Labelled Total, but 4 is not a sum of the rows above: data.
        let g = grid(&["team,points", "Red,5", "Blue,3", "Total:,4"]);
        assert_eq!(find(&g).tables[0].body, [1, 2, 3]);

        // Sums are checked on formatted numbers too.
        let mut g = grid(&["item,cost", "a,x", "b,x", "Sub-total,x"]);
        for (row, text) in [(1, "$1,200.50"), (2, "$34.00"), (3, "$1,234.50")] {
            g[row][1] = Cell::from_text(text);
        }
        assert_eq!(find(&g).tables[0].totals, [3]);

        for label in [
            "Total",
            "TOTALS",
            "subtotal:",
            "Sub Total",
            "North Grand Total",
        ] {
            assert!(is_total_label(label), "{label}");
        }
        for label in ["Total Care Shampoo", "Subtotal Wax", "Totally"] {
            assert!(!is_total_label(label), "{label}");
        }
    }

    #[test]
    fn a_detached_line_is_a_total_only_when_it_sums() {
        let g = grid(&["id,units", "a,10", "b,20", "", "Total,30"]);
        assert!(find(&g).notes[0].total);
        // Not a sum, so it is a row like any other after a blank line.
        let g = grid(&["id,units", "a,10", "b,20", "", "Total,7"]);
        let layout = find(&g);
        assert_eq!(layout.tables[0].body, [1, 2, 4]);
        assert!(layout.notes.is_empty());
    }

    #[test]
    fn one_row_after_a_blank_line_continues_the_table_when_shaped_like_it() {
        let g = grid(&["name,amount", "Alice,10", "Bob,20", "", "Carol,30"]);
        let layout = find(&g);
        assert_eq!(layout.tables[0].body, [1, 2, 4]);
        assert!(layout.notes.is_empty());
        // A placeholder where the table has values is still a row.
        for line in ["Carol,N/A", "Carol,-", "Carol,TBD"] {
            let g = grid(&["name,amount", "Alice,10", "Bob,20", "", line]);
            let layout = find(&g);
            assert_eq!(layout.tables[0].body, [1, 2, 4], "{line}");
            assert!(layout.notes.is_empty(), "{line}");
        }
        // One cell, or text where the table has values: a note.
        for line in ["Source: finance", "Carol,thirty"] {
            let g = grid(&["name,amount", "Alice,10", "Bob,20", "", line]);
            let layout = find(&g);
            assert_eq!(layout.tables[0].body, [1, 2], "{line}");
            assert_eq!(layout.notes.len(), 1, "{line}");
        }
    }

    #[test]
    fn one_cell_rows_shaped_like_the_body_stay() {
        // `zed,` is filled like `bob,`: data, not a footnote.
        let g = grid(&["name,note", "ann,ok", "bob,", "cy,fine", "zed,"]);
        let layout = find(&g);
        assert_eq!(layout.tables[0].body, [1, 2, 3, 4]);
        assert!(layout.notes.is_empty());

        // A record with as many fields as the table is a row, however sparse.
        let g = grid(&["name,amount", "Alice,10", "Bob,20", "Carol,"]);
        assert_eq!(find(&g).tables[0].body, [1, 2, 3]);
        let g = grid(&["name,note", "ann,ok", "cy,fine", ",Source: HR"]);
        assert_eq!(find(&g).tables[0].body, [1, 2, 3]);

        // A record with fewer fields is a note.
        let g = grid(&["name,amount", "Alice,10", "Bob,20", "Source: HR"]);
        let layout = find(&g);
        assert_eq!((layout.tables[0].body.len(), layout.notes.len()), (2, 1));

        // In a sheet, a one-cell row is a note only when it says so.
        let g = grid(&["name,note", "ann,ok", "cy,fine", ",Source: HR"]);
        assert_eq!(super::find(&g, GridKind::Sheet).notes.len(), 1);
        let g = grid(&["name,amount", "Alice,10", "Bob,20", "Carol,"]);
        assert_eq!(super::find(&g, GridKind::Sheet).tables[0].body, [1, 2, 3]);

        // A caption needs a header below it: over headerless data, `Alice,` is a row.
        let g = grid(&["Alice,", "Bob,10", "Carol,20"]);
        let table = &find(&g).tables[0];
        assert_eq!((caption(table), table.body.len()), (None, 3));
        let g = grid(&["Sales report,,", "region,q1,q2", "North,1,2", "South,3,4"]);
        let table = &find(&g).tables[0];
        assert_eq!(caption(table), Some("Sales report"));

        // With no header, `ann,` is filled like `cy,`: data, not a caption.
        let g = grid(&["ann,", "bob,ok", "cy,", "di,fine"]);
        let table = &find(&g).tables[0];
        assert_eq!((caption(table), table.body.len()), (None, 4));
    }

    #[test]
    fn drops_empty_unnamed_columns_and_dedupes_names() {
        let g = grid(&["id, name ,amount,amount,", "1,a,2,3,", "2,b,4,5,"]);
        let table = Table::new(&g, 0..=4, Some(0..=0), 1..=2, false, None);
        assert_eq!(names(&table), ["id", "name", "amount", "amount_2"]);
    }

    #[test]
    fn a_plan_override_wins_over_detection() {
        let g = grid(&["Title", "", "a,b", "1,2", "3,4", "5,6"]);
        let detected = &find(&g).tables[0];
        assert_eq!(
            (detected.header.clone(), detected.body.len()),
            (Some(2..=2), 3)
        );
        // Say there is no header and the body is the last two rows.
        let table = Table::new(&g, detected.cols.clone(), None, 4..=5, false, None);
        assert_eq!(names(&table), ["column_1", "column_2"]);
        assert_eq!(table.body, [4, 5]);
    }
}
