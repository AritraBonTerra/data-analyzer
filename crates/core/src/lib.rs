//! Data engine for Data Analyzer. Opens CSV, TSV, Parquet, JSON and Excel files
//! into DuckDB and serves pages of rows, sorted and filtered frames, column
//! statistics, SQL queries and exports. No UI code lives here.
//!
//! Typical flow, all off the UI thread:
//! 1. [`Dataset::open`] finds the table in the file and exposes it as the SQL
//!    view `data`. A file can hold several tables ([`Dataset::parts`]);
//!    [`OpenOptions`] picks one and can override how it is read.
//! 2. [`Dataset::frame`] + [`Dataset::fetch`] show the first rows right away.
//! 3. [`Dataset::materialize`] loads big CSV/JSON files fully in the background.
//! 4. [`Dataset::apply`], [`Dataset::query`], [`Dataset::summarize`] and
//!    [`Dataset::sketches`] power sorting, SQL and stats.
//!
//! Finding the table: before loading a CSV/TSV file (up to 4 MB, not
//! compressed) or an Excel sheet, the engine looks at it as a grid of cells
//! and finds the blocks shaped like tables, skipping titles, notes and totals
//! rows. What it did is reported back for the UI: [`Dataset::summary`] (one
//! line, or none when the file was a plain table), [`Dataset::notes`] (the
//! lines set aside), [`Dataset::unit`] (units from a `(USD)` header row),
//! [`Dataset::plan`] (the [`ImportPlan`] in effect, to show and edit) and
//! [`Dataset::preview`] (the raw rows with their roles under a plan). To
//! adjust, reopen with `OpenOptions { part, plan, delimiter, encoding, .. }`.
//!
//! Reading values: a CSV/TSV file that isn't UTF-8 is read from a UTF-8 copy
//! ([`Encoding`], [`Dataset::encoding`]). Once a table is loaded, a text
//! column whose every value reads as a number (`$1,234.00`, `12.6%`,
//! `1.424,50`), a date (`Jul 8, 2025`, `24/03/2025`) or yes/no, leaving out
//! placeholders such as `N/A`, is shown converted in `data`; one misfit and
//! it stays text. Converted numbers keep the file's look through
//! [`Column::format`]. [`Dataset::conversions`] says per column what was done,
//! and the summary says it in one line. The user's choices ([`ValueOptions`]:
//! the decimal mark and a [`ReadAs`] per column) go in
//! [`OpenOptions::values`], or apply to an open file with
//! [`Dataset::set_values`]; [`Dataset::fit`] says how many values a type
//! would read before choosing it.
//!
//! Charts: [`ChartSpec::suggest`] picks a starting chart from the columns,
//! the UI edits the spec, and [`Dataset::chart`] draws it from a frame as one
//! grouped query (a sample for scatters), returning a [`Chart`] with series,
//! axes, a title and footer notes. [`Chart::sql`] is its query for Show SQL.

mod chart;
mod clean;
mod dataset;
mod encoding;
mod excel;
mod format;
mod import;
mod layout;
mod profile;
mod sql;

pub use chart::{
    Aggregate, Axis, BarSort, Category, Chart, ChartKind, ChartSpec, DateUnit, MAX_Y, Point, Scale,
    Series, Tick, XAxis,
};
pub use clean::{Conversion, Decimal, Fit, Guessed, NumberFormat, ReadAs, ValueOptions};
pub use dataset::{
    Cancelled, Column, ColumnKind, Dataset, ExportFormat, Frame, OpenOptions, Page, QueryTicket,
    SqlOutcome, clean_cache, remove_own_sessions,
};
pub use encoding::Encoding;
pub use format::FileKind;
pub use import::{Delimiter, ImportPlan, Note, Part, Preview, PreviewRow, RowRole, Rows};
pub use profile::{Bin, ColumnSummary, Distribution, tidy_number};
pub use sql::{Filter, FilterOp, SortKey, ViewSpec};
