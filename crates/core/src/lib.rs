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
//! adjust, reopen with `OpenOptions { part, plan, delimiter, .. }`.

mod dataset;
mod excel;
mod format;
mod import;
mod layout;
mod profile;
mod sql;

pub use dataset::{
    Cancelled, Column, ColumnKind, Dataset, ExportFormat, Frame, OpenOptions, Page, QueryTicket,
    SqlOutcome, clean_cache, remove_own_sessions,
};
pub use format::FileKind;
pub use import::{Delimiter, ImportPlan, Note, Part, Preview, PreviewRow, RowRole, Rows};
pub use profile::{Bin, ColumnSummary, Distribution, tidy_number};
pub use sql::{Filter, FilterOp, SortKey, ViewSpec};
