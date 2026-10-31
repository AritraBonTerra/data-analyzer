//! Data engine for Data Analyzer. Opens CSV, TSV, Parquet, JSON and Excel files
//! into DuckDB and serves pages of rows, sorted and filtered frames, column
//! statistics, SQL queries and exports. No UI code lives here.
//!
//! Typical flow, all off the UI thread:
//! 1. [`Dataset::open`] sniffs the file and exposes it as the SQL view `data`.
//! 2. [`Dataset::frame`] + [`Dataset::fetch`] show the first rows right away.
//! 3. [`Dataset::materialize`] loads big CSV/JSON files fully in the background.
//! 4. [`Dataset::apply`], [`Dataset::query`], [`Dataset::summarize`] and
//!    [`Dataset::sketches`] power sorting, SQL and stats.

mod dataset;
mod excel;
mod format;
mod profile;
mod sql;

pub use dataset::{
    Cancelled, Column, ColumnKind, Dataset, ExportFormat, Frame, OpenOptions, Page, QueryTicket,
    SqlOutcome, clean_cache, remove_own_sessions,
};
pub use excel::sheet_names;
pub use format::FileKind;
pub use profile::{Bin, ColumnSummary, Distribution, tidy_number};
pub use sql::{Filter, FilterOp, SortKey, ViewSpec};
