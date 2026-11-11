use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use duckdb::{Connection, InterruptHandle};

use crate::clean::{
    self, Cleaning, Conversion, Decimal, Fit, Mark, NumberFormat, ReadAs, ValueOptions,
};
use crate::encoding::{self, Decode, Encoding};
use crate::excel;
use crate::format::FileKind;
use crate::import::{self, Delimiter, Import, ImportPlan, Load, Note, Part, Preview, Scan};
use crate::layout;
use crate::sql::{ViewSpec, quote_ident, quote_str};

/// Broad type families, used for alignment, filters and which chart to draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnKind {
    Number,
    Text,
    Temporal,
    Boolean,
    Other,
}

impl ColumnKind {
    pub fn from_sql(sql_type: &str) -> Self {
        let upper = sql_type.to_ascii_uppercase();
        let base = upper.split('(').next().unwrap_or("").trim();
        match base {
            "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "HUGEINT" | "UTINYINT"
            | "USMALLINT" | "UINTEGER" | "UBIGINT" | "UHUGEINT" | "FLOAT" | "DOUBLE" | "REAL"
            | "DECIMAL" => Self::Number,
            "DATE" => Self::Temporal,
            t if t.starts_with("TIMESTAMP") => Self::Temporal,
            "BOOLEAN" => Self::Boolean,
            "VARCHAR" | "UUID" => Self::Text,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    /// DuckDB's type name, e.g. `BIGINT` or `DECIMAL(18,3)`.
    pub sql_type: String,
    pub kind: ColumnKind,
    /// How [`Dataset::fetch`] shows the values: set on a number column read
    /// from text like `$1,234.00` or `12.6%`, so it shows the way the file
    /// wrote it. Kept by sorted and filtered frames, never on SQL results.
    pub format: Option<NumberFormat>,
}

impl Column {
    pub fn new(name: impl Into<String>, sql_type: impl Into<String>) -> Self {
        let sql_type = sql_type.into();
        Self {
            name: name.into(),
            kind: ColumnKind::from_sql(&sql_type),
            sql_type,
            format: None,
        }
    }
}

/// A queryable set of rows: the file itself, a sorted/filtered copy of it, or a
/// SQL result. Cheap to clone; the UI keeps one per grid and pages through it.
/// Sorted, filtered and SQL frames own a scratch table that is dropped when the
/// last clone goes away, so a frame stays readable for as long as you hold it.
#[derive(Clone, Debug)]
pub struct Frame {
    /// Unique within the process, so the UI can drop results for an old frame.
    pub id: u64,
    pub(crate) relation: String,
    pub columns: Arc<[Column]>,
    /// `None` while a CSV/JSON file is still loading and the total is unknown.
    pub rows: Option<u64>,
    _table: Option<Arc<ScratchTable>>,
}

/// Drops its table when the last [`Frame`] using it is dropped.
struct ScratchTable {
    name: String,
    conn: Mutex<Connection>,
}

impl Drop for ScratchTable {
    /// Runs on a short-lived thread: the last frame is often dropped on the UI thread.
    fn drop(&mut self) {
        let sql = format!("DROP TABLE IF EXISTS {}", self.name);
        if let Ok(conn) = self.conn.get_mut().map(|conn| conn.try_clone()) {
            std::thread::spawn(move || {
                if let Ok(conn) = conn {
                    let _ = conn.execute_batch(&sql);
                }
            });
        }
    }
}

impl std::fmt::Debug for ScratchTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// A window of rows rendered as text. `None` is SQL NULL.
#[derive(Clone, Debug)]
pub struct Page {
    pub offset: u64,
    pub rows: Vec<Vec<Option<String>>>,
}

#[derive(Debug)]
pub enum SqlOutcome {
    Rows(Frame),
    /// A statement with no result set ran, e.g. `CREATE TABLE`.
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Tsv,
    Parquet,
    /// Newline-delimited JSON.
    Json,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::Parquet => "parquet",
            Self::Json => "jsonl",
        }
    }
}

/// Orders SQL requests; see [`Dataset::query_ticket`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryTicket(u64);

/// Returned (inside `anyhow::Error`) when work was cancelled or superseded.
/// Check with `err.is::<Cancelled>()` and ignore it.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    /// Where per-file DuckDB scratch databases live, e.g. `~/Library/Caches/<bundle id>`.
    pub cache_dir: PathBuf,
    /// Which part to open, an index into [`Dataset::parts`]. `None` opens the
    /// part with the most data rows.
    pub part: Option<usize>,
    /// How to read the opened part, in place of what was detected. Start from
    /// [`Dataset::plan`] and change what the user changed.
    pub plan: Option<ImportPlan>,
    /// The CSV/TSV delimiter. Ignored for other formats. The parts depend on
    /// it, so a change usually goes with `part: None, plan: None`.
    pub delimiter: Delimiter,
    /// The text encoding of a CSV/TSV file. Ignored for other formats and
    /// for compressed files. Like the delimiter, it changes the parts.
    pub encoding: Encoding,
    /// How values are read: the decimal mark and the type of each column.
    /// Pass [`Dataset::values`] to keep the user's choices on a reopen.
    pub values: ValueOptions,
}

/// Longest cell text returned by [`Dataset::fetch`]; the grid can't show more anyway.
const MAX_CELL_CHARS: usize = 2000;
/// Highest fallback level for reading CSV/JSON; see [`Dataset::reader`].
const MAX_LEVEL: u8 = 3;
/// Rows of a CSV/JSON file not loaded yet that the cleaner looks at. They
/// are kept in the table `_preview` until the file is loaded.
const PREVIEW_ROWS: u64 = 5000;

/// One open file. Every method blocks, so call them off the UI thread. Each call
/// uses its own DuckDB connection, so calls from different threads run in parallel.
///
/// The file is exposed to SQL as a view named `data`. CSV/JSON files start as a
/// view over the file (instant preview), and [`Dataset::materialize`] loads them
/// into a table so paging, sorting and stats are fast.
pub struct Dataset {
    path: PathBuf,
    kind: FileKind,
    /// The CSV/TSV text the finder reads: the file, or a UTF-8 copy of it.
    text: PathBuf,
    /// The CSV DuckDB reads: `text`, or the found table written out cleanly
    /// (see [`Dataset::reader`]). A load that finds bytes that aren't UTF-8
    /// late in a big file switches it to a UTF-8 copy.
    source: Mutex<PathBuf>,
    /// Whether `source` is the found table written out by `scan_csv`.
    rewritten: bool,
    /// The CSV reader's `types` option keeping guessed dates text (see
    /// [`Dataset::keep_guessed_dates_as_text`]), or empty.
    text_dates: String,
    /// The encoding the CSV/TSV text was read in; `None` for other formats.
    encoding: Mutex<Option<Encoding>>,
    /// Bytes of a UTF-8 file that weren't UTF-8, read as Windows-1252.
    mended: AtomicU64,
    /// The encoding asked for at open.
    encoding_choice: Encoding,
    /// The decimal mark asked for at open; see [`Dataset::reader`].
    decimal_choice: Decimal,
    /// The value options in effect and what the cleaner made of them.
    clean: Mutex<(ValueOptions, Cleaning)>,
    /// Held while [`Dataset::clean`] decides, so two decisions never overlap.
    cleaning: Mutex<()>,
    import: Import,
    session_dir: PathBuf,
    root: Mutex<Option<Connection>>,
    loaded: AtomicBool,
    /// Reader fallback level that last worked for CSV/JSON.
    level: Mutex<u8>,
    load_cancelled: AtomicBool,
    load_interrupt: Mutex<Option<Arc<InterruptHandle>>>,
    view_interrupt: Mutex<Option<Arc<InterruptHandle>>>,
    query_interrupt: Mutex<Option<Arc<InterruptHandle>>>,
    /// The running [`Dataset::chart`]; a newer chart interrupts it.
    pub(crate) chart_interrupt: Mutex<Option<Arc<InterruptHandle>>>,
    /// The most recently issued [`QueryTicket`]; older tickets are stale.
    latest_query: AtomicU64,
    notice: Mutex<Option<String>>,
}

impl Dataset {
    /// Opens a file far enough to show its schema and first rows. For CSV/JSON
    /// this only sniffs the file; call [`Dataset::materialize`] next.
    pub fn open(path: &Path, options: &OpenOptions) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("couldn't find {}", path.display()))?;
        let kind = FileKind::detect(&path)
            .ok_or_else(|| anyhow!("Data Analyzer can't open this kind of file"))?;

        let session_dir = options.cache_dir.join(format!(
            "session-{}-{}",
            std::process::id(),
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&session_dir)
            .with_context(|| format!("couldn't create {}", session_dir.display()))?;
        let conn = Connection::open(session_dir.join("data.duckdb"))?;
        conn.execute_batch(&format!(
            "SET temp_directory = {};
             SET preserve_insertion_order = true;
             SET autoinstall_known_extensions = false;
             SET autoload_known_extensions = false;",
            quote_str(&session_dir.join("spill").to_string_lossy())
        ))?;

        // Text in another encoding, or mended, is read from a UTF-8 copy.
        let text_file = matches!(kind, FileKind::Csv | FileKind::Tsv) && !compressed(&path);
        let decode = if text_file {
            encoding::detect(&path, options.encoding)?
        } else {
            None
        };
        let (text, encoding, mended) = match decode {
            Some(decode) => {
                let copy = session_dir.join("text.csv");
                let found = encoding::transcode(&path, decode, &copy)?;
                let (encoding, mended) = encoding::read_as(decode, found);
                (copy, Some(encoding), mended)
            }
            None => (path.clone(), text_file.then_some(Encoding::Utf8), 0),
        };

        let mut dataset = Self {
            source: Mutex::new(text.clone()),
            text,
            path,
            kind,
            rewritten: false,
            text_dates: String::new(),
            encoding: Mutex::new(encoding),
            mended: AtomicU64::new(mended),
            encoding_choice: options.encoding,
            decimal_choice: options.values.decimal,
            clean: Mutex::new((options.values.clone(), Cleaning::default())),
            cleaning: Mutex::new(()),
            import: Import::unscanned(),
            session_dir,
            root: Mutex::new(Some(conn)),
            loaded: AtomicBool::new(!kind.needs_materialize()),
            level: Mutex::new(0),
            load_cancelled: AtomicBool::new(false),
            load_interrupt: Mutex::new(None),
            view_interrupt: Mutex::new(None),
            query_interrupt: Mutex::new(None),
            chart_interrupt: Mutex::new(None),
            latest_query: AtomicU64::new(0),
            notice: Mutex::new(None),
        };
        dataset.attach(options)?;
        Ok(dataset)
    }

    /// Finds the part to open and creates the `data` view for it.
    fn attach(&mut self, options: &OpenOptions) -> Result<()> {
        let conn = self.connection()?;
        let plan = options.plan.as_ref();
        match self.kind {
            FileKind::Excel => {
                let sheets = excel::read_sheets(&self.path)?;
                if sheets.is_empty() {
                    bail!("the workbook has no sheets");
                }
                let scans: Vec<Scan> = sheets
                    .iter()
                    .map(|(name, range)| {
                        let whole_rows = excel::whole_sheet_rows(range);
                        Scan::new(Some(name.clone()), excel::grid(range), whole_rows)
                    })
                    .collect();
                let (import, load) = Import::new(&scans, options.part, plan, true);
                // The grids are only for finding tables; loading reads the sheet itself.
                drop(scans);
                self.import = import;
                match load {
                    Load::Whole { scan } => excel::load_sheet(&conn, &sheets[scan].1, "_data")?,
                    Load::Table { scan, table } => {
                        excel::load_table(&conn, &sheets[scan].1, &table, "_data")?;
                    }
                }
                self.clean_quietly(&conn);
            }
            FileKind::Parquet => self.clean_quietly(&conn),
            _ => {
                if matches!(self.kind, FileKind::Csv | FileKind::Tsv) {
                    self.scan_csv(options)?;
                    self.keep_guessed_dates_as_text(&conn);
                }
                // Try the strict reader first; relax it only if the first rows
                // fail. They are kept in `_preview` for the cleaner until loaded.
                let mut last_err = None;
                let mut levels = [0, 2, MAX_LEVEL].into_iter().peekable();
                while let Some(&level) = levels.peek() {
                    let attempt = conn.execute_batch(&format!(
                        "CREATE OR REPLACE TABLE _preview AS SELECT * FROM {} LIMIT {PREVIEW_ROWS}",
                        self.reader(level)
                    ));
                    match attempt {
                        Ok(()) => {
                            *self.level.lock().unwrap() = level;
                            self.note_level(level);
                            self.clean_quietly(&conn);
                            return Ok(());
                        }
                        Err(err) if self.recode_after(&err) => {}
                        Err(err) => {
                            last_err = Some(err);
                            levels.next();
                        }
                    }
                }
                return Err(last_err.map_or_else(|| anyhow!("couldn't read the file"), Into::into));
            }
        }
        Ok(())
    }

    /// Runs the table finder over a small plain CSV/TSV file. When the table
    /// isn't simply the whole file, writes it out cleanly for DuckDB to read
    /// instead. Large or compressed files are left exactly as before.
    fn scan_csv(&mut self, options: &OpenOptions) -> Result<()> {
        let tsv = self.kind == FileKind::Tsv;
        let Some((scan, delimiter)) = import::scan_csv(&self.text, tsv, options.delimiter)? else {
            return Ok(());
        };
        let auto = options.delimiter == Delimiter::Auto;
        let (mut import, load) = Import::new(
            std::slice::from_ref(&scan),
            options.part,
            options.plan.as_ref(),
            auto,
        );
        import.delimiter = Some(delimiter);
        self.import = import;
        let table = match load {
            Load::Table { table, .. } => Some(table),
            // No table found, but the user chose a delimiter: split on it anyway.
            Load::Whole { .. } if !auto => layout::whole(&scan.grid),
            Load::Whole { .. } => None,
        };
        if let Some(table) = table {
            let clean = self.session_dir.join("table.csv");
            import::write_csv(&scan, &table, &clean)?;
            *self.source.get_mut().unwrap() = clean;
            self.rewritten = true;
        }
        Ok(())
    }

    /// Keeps text the CSV columns DuckDB would type as dates or timestamps
    /// from a spelling whose day/month order it guessed: any but a
    /// four-digit year first (`2025-01-02`, `2025/01/02`,
    /// `2025-01-05T10:00:00Z`), which DuckDB reads as it always has. The
    /// cleaner reads the others over the whole column and says when the
    /// order is a guess. Looks at the types DuckDB sniffs and the text of
    /// the first rows of those columns, never the whole file. Typing at the
    /// fallback levels, which sniff every row, is not looked at again.
    fn keep_guessed_dates_as_text(&mut self, conn: &Connection) {
        self.text_dates = String::new();
        let Ok(columns) = describe(conn, &self.reader(0)) else {
            return;
        };
        let dates: Vec<&Column> = columns
            .iter()
            .filter(|c| c.kind == ColumnKind::Temporal)
            .collect();
        let as_text = |columns: &[&Column]| {
            let types: Vec<String> = columns
                .iter()
                .map(|c| format!("{}: 'VARCHAR'", quote_str(&c.name)))
                .collect();
            format!("types = {{{}}}", types.join(", "))
        };
        if dates.is_empty() {
            return;
        }
        // Read as text, those columns' first values say how they are written.
        // A timestamp column with no time of day in sight is dates DuckDB
        // couldn't type as such; the cleaner reads it as dates.
        self.text_dates = as_text(&dates);
        let year_first: Vec<String> = dates
            .iter()
            .map(|c| {
                let col = quote_ident(&c.name);
                let check = format!(r"bool_and(regexp_matches({col}, '^\s*\d{{4}}[-/]'))");
                if c.sql_type == "TIMESTAMP" {
                    format!("{check} AND bool_or(contains({col}, ':'))")
                } else {
                    check
                }
            })
            .collect();
        let sql = format!(
            "SELECT {} FROM (SELECT * FROM {} LIMIT {PREVIEW_ROWS})",
            year_first.join(", "),
            self.reader(0)
        );
        let read = conn.query_row(&sql, [], |row| {
            (0..dates.len())
                .map(|i| row.get::<_, Option<bool>>(i))
                .collect::<duckdb::Result<Vec<_>>>()
        });
        self.text_dates = match read {
            Ok(year_first) => {
                let guessed: Vec<&Column> = dates
                    .iter()
                    .zip(year_first)
                    .filter(|(_, year_first)| *year_first == Some(false))
                    .map(|(column, _)| *column)
                    .collect();
                if guessed.is_empty() {
                    String::new()
                } else {
                    as_text(&guessed)
                }
            }
            Err(_) => String::new(),
        };
    }

    /// The table function that reads the file. Higher levels trade type accuracy
    /// for tolerance: 1 sniffs the whole file, 2 also accepts ragged rows, and 3
    /// reads every column as text.
    fn reader(&self, level: u8) -> String {
        let path = quote_str(&self.source.lock().unwrap().to_string_lossy());
        let mut options: Vec<&str> = Vec::new();
        match self.kind {
            FileKind::Json | FileKind::NdJson => {
                if self.kind == FileKind::NdJson {
                    options.push("format = 'newline_delimited'");
                }
                if level >= MAX_LEVEL {
                    let options = options.iter().map(|o| format!(", {o}")).collect::<String>();
                    return format!("(SELECT json FROM read_json_objects({path}{options}))");
                }
                if level >= 1 {
                    options.push("sample_size = -1");
                }
                format_reader("read_json", &path, &options)
            }
            _ => {
                if self.rewritten {
                    // Written by `scan_csv`: always commas, always a header line.
                    options.extend(["delim = ','", "header = true"]);
                } else if self.kind == FileKind::Tsv {
                    options.push("delim = '\\t'");
                }
                // Dates DuckDB would read with a guessed day/month order, such
                // as `03/04/2025`, stay text for the cleaner, which settles the
                // order over the whole column instead of guessing from a sample.
                if !self.text_dates.is_empty() {
                    options.push(&self.text_dates);
                }
                // DuckDB reads `1.234` as 1.234 whatever the file's mark. With
                // a decimal comma chosen, numbers stay text for the cleaner,
                // which reads them that way; other types are typed as ever.
                // (DuckDB's own `decimal_separator` and `thousands` don't check
                // groups of three, so they would read `1.5` as 15.)
                if self.decimal_choice == Decimal::Comma {
                    options.push(
                        "auto_type_candidates = ['BOOLEAN', 'DATE', 'TIME', 'TIMESTAMP', 'TIMESTAMPTZ', 'VARCHAR']",
                    );
                }
                if (1..MAX_LEVEL).contains(&level) {
                    options.push("sample_size = -1");
                }
                if level >= 2 {
                    options.extend(["strict_mode = false", "null_padding = true"]);
                }
                if level >= MAX_LEVEL {
                    options.push("all_varchar = true");
                }
                format_reader("read_csv", &path, &options)
            }
        }
    }

    /// After a read failed on bytes that aren't UTF-8 further into a file
    /// whose start was, switches to a mended copy of it, with those bytes
    /// read as Windows-1252, so the read can be retried. Returns whether it did.
    fn recode_after(&self, err: &duckdb::Error) -> bool {
        let mut encoding = self.encoding.lock().unwrap();
        if self.encoding_choice != Encoding::Auto
            || *encoding != Some(Encoding::Utf8)
            || self.rewritten
            || !err.to_string().contains("Invalid unicode")
        {
            return false;
        }
        let copy = self.session_dir.join("text.csv");
        match encoding::transcode(&self.text, Decode::MendedUtf8, &copy) {
            Ok(found) => {
                let (read_as, mended) = encoding::read_as(Decode::MendedUtf8, found);
                *self.source.lock().unwrap() = copy;
                *encoding = Some(read_as);
                self.mended.store(mended, Ordering::Release);
                true
            }
            Err(_) => false,
        }
    }

    /// Decides how to read the values of the table `data` shows (see
    /// [`crate::clean`]) under the value options in effect, and makes `data`
    /// show them that way. Before a CSV/JSON file is loaded, it looks at the
    /// first rows only; loading decides again over every row.
    fn clean(&self, conn: &Connection) -> Result<()> {
        // One decision at a time. `self.clean` itself is only held to copy the
        // options out and to store the result, because the UI reads it while
        // drawing and the whole-column check below can take seconds.
        let _deciding = self.cleaning.lock().unwrap();
        let values = self.clean.lock().unwrap().0.clone();
        let from = self.base();
        let auto = self.kind != FileKind::Parquet;
        let check = self.check(&from);
        let cleaning = clean::clean(conn, check, &values, auto, &|| self.delimiter_char(conn))?;
        conn.execute_batch(&format!(
            "CREATE OR REPLACE VIEW data AS {}",
            cleaning.view(&from)
        ))?;
        self.clean.lock().unwrap().1 = cleaning;
        Ok(())
    }

    /// The character that separates the file's fields: as the finder split
    /// it, or for a CSV file it didn't scan, as DuckDB's sniffer reads it (a
    /// comma when it can't). `None` for a file that isn't delimited text.
    fn delimiter_char(&self, conn: &Connection) -> Option<char> {
        match self.kind {
            FileKind::Tsv => return Some('\t'),
            FileKind::Csv => {}
            _ => return None,
        }
        if let Some(delimiter) = self.import.delimiter {
            return Some(char::from(delimiter.byte()));
        }
        let source = quote_str(&self.source.lock().unwrap().to_string_lossy());
        let sniffed = conn.query_row(
            &format!("SELECT Delimiter FROM sniff_csv({source})"),
            [],
            |row| row.get::<_, String>(0),
        );
        Some(sniffed.ok().and_then(|d| d.chars().next()).unwrap_or(','))
    }

    /// [`Dataset::clean`] for opening and loading, where a failure must not
    /// stop the file opening: `data` then shows the table as loaded.
    fn clean_quietly(&self, conn: &Connection) {
        if self.clean(conn).is_err() {
            let from = self.base();
            let _ = conn.execute_batch(&format!(
                "CREATE OR REPLACE VIEW data AS SELECT * FROM {from}"
            ));
            self.clean.lock().unwrap().1 = Cleaning::default();
        }
    }

    /// The rows the cleaner decides on: the first rows in `_preview` before
    /// a CSV/JSON file is loaded, else `from`, the relation `data` is made from.
    fn check<'a>(&self, from: &'a str) -> &'a str {
        if self.kind.needs_materialize() && !self.is_loaded() {
            "_preview"
        } else {
            from
        }
    }

    /// The relation `data` is made from: the loaded table, the Parquet
    /// file, or the reader of a CSV/JSON file not loaded yet.
    fn base(&self) -> String {
        match self.kind {
            FileKind::Parquet => {
                format!("read_parquet({})", quote_str(&self.path.to_string_lossy()))
            }
            kind if kind.needs_materialize() && !self.is_loaded() => {
                self.reader(*self.level.lock().unwrap())
            }
            _ => "_data".into(),
        }
    }

    fn note_level(&self, level: u8) {
        let notice = match level {
            2 => Some(
                "Some rows have a different number of fields; missing values are shown as empty.",
            ),
            MAX_LEVEL => {
                Some("Some values didn't match a single type, so every column was loaded as text.")
            }
            _ => None,
        };
        *self.notice.lock().unwrap() = notice.map(String::from);
    }

    pub(crate) fn connection(&self) -> Result<Connection> {
        let root = self.root.lock().unwrap();
        let root = root
            .as_ref()
            .ok_or_else(|| anyhow!("the dataset is closed"))?;
        Ok(root.try_clone()?)
    }

    /// Ids are unique across all datasets in the process, so a frame from a
    /// previous sheet or file can never be mistaken for a current one.
    pub(crate) fn next_id(&self) -> u64 {
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn kind(&self) -> FileKind {
        self.kind
    }

    /// The tables in the file: one per table found on each sheet or in the
    /// CSV, plus each sheet with no table, read whole. Always at least one.
    pub fn parts(&self) -> &[Part] {
        &self.import.parts
    }

    /// The open part, an index into [`Dataset::parts`].
    pub fn part(&self) -> usize {
        self.import.part
    }

    /// How the open part was read: detected, or the plan it was opened with.
    /// `None` for files that aren't scanned (Parquet, JSON, large or
    /// compressed CSV), where there's nothing to adjust.
    pub fn plan(&self) -> Option<&ImportPlan> {
        self.import.plan.as_ref()
    }

    /// The delimiter in effect for a scanned CSV/TSV file.
    pub fn delimiter(&self) -> Option<Delimiter> {
        self.import.delimiter
    }

    /// One line saying what the import did, like `Table found on lines 5 to
    /// 45. Set aside 3 title lines, 1 total and 1 note. Read 2 text columns
    /// as numbers.` It covers the encoding, the table found and the values
    /// converted; per-column detail is in [`Dataset::conversions`]. `None`
    /// when the file was read plainly.
    pub fn summary(&self) -> Option<String> {
        let encoding = match (self.encoding(), self.mended.load(Ordering::Acquire)) {
            (Some(e @ (Encoding::Utf16 | Encoding::Windows1252)), _) => {
                Some(format!("Read as {}, not UTF-8.", e.label()))
            }
            (_, 0) => None,
            (_, 1) => Some("Read 1 byte that isn't UTF-8 as Windows-1252.".into()),
            (_, n) => Some(format!("Read {n} bytes that aren't UTF-8 as Windows-1252.")),
        };
        let cleaning = self.clean.lock().unwrap().1.summary();
        let sentences: Vec<String> = [encoding, self.import.summary.clone(), cleaning]
            .into_iter()
            .flatten()
            .collect();
        (!sentences.is_empty()).then(|| sentences.join(" "))
    }

    /// The encoding a CSV/TSV file was read in: UTF-8, UTF-16 or
    /// Windows-1252, never `Auto`. `None` for other formats.
    pub fn encoding(&self) -> Option<Encoding> {
        *self.encoding.lock().unwrap()
    }

    /// The value options in effect: what was passed at open, or last set.
    pub fn values(&self) -> ValueOptions {
        self.clean.lock().unwrap().0.clone()
    }

    /// The decimal mark numbers were read with: the one chosen, or what
    /// the file's numbers say. `Point` or `Comma`, never `Auto`.
    pub fn decimal(&self) -> Decimal {
        match self.clean.lock().unwrap().1.mark {
            Mark::Point => Decimal::Point,
            Mark::Comma => Decimal::Comma,
        }
    }

    /// The columns `data` shows differently from how the file was loaded,
    /// in column order: converted to numbers, dates or yes/no (on their own
    /// or by the user's choice), or trimmed of spaces. Before a CSV/JSON
    /// file is loaded, this is what its first rows say.
    pub fn conversions(&self) -> Vec<Conversion> {
        self.clean.lock().unwrap().1.conversions.clone()
    }

    /// Reads values under new options, such as a column's type chosen in
    /// the column menu, without reopening the file. Returns the new
    /// full-file frame. A file still loading keeps the options when done.
    /// A decimal comma reaches columns DuckDB typed as numbers only when
    /// passed at open, in [`OpenOptions::values`].
    pub fn set_values(&self, values: &ValueOptions) -> Result<Frame> {
        let conn = self.connection()?;
        self.clean.lock().unwrap().0 = values.clone();
        self.clean(&conn)?;
        self.frame()
    }

    /// How many values of `column` would read as `read_as`, to show before
    /// the user commits to it. Counts every row once the file is loaded,
    /// the first rows before.
    pub fn fit(&self, column: &str, read_as: ReadAs) -> Result<Fit> {
        let conn = self.connection()?;
        let from = self.base();
        let check = self.check(&from);
        let column = describe(&conn, check)?
            .into_iter()
            .find(|c| c.name == column)
            .ok_or_else(|| anyhow!("there is no column {column}"))?;
        let mark = self.clean.lock().unwrap().1.mark;
        clean::fit(&conn, check, &column, read_as, mark)
    }

    /// Every line the open part leaves out, in file order: titles and the
    /// caption above it, notes below it, and totals rows wherever they sat.
    /// Totals rows kept in the data by the plan are not listed.
    pub fn notes(&self) -> &[Note] {
        &self.import.notes
    }

    /// The unit a header gave `column`, like `USD` from a `(USD)` units row.
    pub fn unit(&self, column: &str) -> Option<&str> {
        self.import
            .units
            .iter()
            .find(|(name, _)| name == column)
            .map(|(_, unit)| unit.as_str())
    }

    /// The raw rows of the open part's file or sheet, each with its role
    /// under `plan`, for drawing the plan over the file. `None` for files
    /// that aren't scanned, or that can no longer be read. Reads the file or
    /// sheet again on each call rather than keeping a copy of every cell
    /// while it is open: it is only called while the user adjusts the import.
    pub fn preview(&self, plan: &ImportPlan) -> Option<Preview> {
        self.import.plan?;
        let (grid, lines) = match self.kind {
            FileKind::Excel => {
                let range = excel::read_sheet(&self.path, self.import.scan).ok()?;
                (excel::grid(&range), Vec::new())
            }
            _ => {
                let tsv = self.kind == FileKind::Tsv;
                let csv = import::csv_grid(&self.text, tsv, self.import.delimiter?).ok()??;
                (csv.grid, csv.lines)
            }
        };
        self.import.preview(&grid, &lines, plan)
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded.load(Ordering::Acquire)
    }

    /// A message worth showing the user about how the file was read, if any.
    pub fn notice(&self) -> Option<String> {
        self.notice.lock().unwrap().clone()
    }

    /// The whole file as a frame. Row count is `None` until it's loaded.
    pub fn frame(&self) -> Result<Frame> {
        let conn = self.connection()?;
        let rows = if self.is_loaded() {
            Some(count(&conn, "data")?)
        } else {
            None
        };
        let mut columns = describe(&conn, "data")?;
        {
            let clean = self.clean.lock().unwrap();
            for column in &mut columns {
                column.format = clean.1.format(&column.name);
            }
        }
        Ok(Frame {
            id: self.next_id(),
            relation: "data".into(),
            columns: columns.into(),
            rows,
            _table: None,
        })
    }

    /// Loads a CSV/JSON file into a table, calling `progress` with 0.0–1.0 as it
    /// goes. Returns the new full-file frame. A no-op for Parquet and Excel.
    pub fn materialize(&self, progress: &(dyn Fn(f32) + Sync)) -> Result<Frame> {
        if self.is_loaded() {
            return self.frame();
        }
        let conn = self.connection()?;
        conn.set_query_progress_tracking(true)?;
        *self.load_interrupt.lock().unwrap() = Some(conn.interrupt_handle());
        if self.load_cancelled.load(Ordering::Acquire) {
            return Err(Cancelled.into());
        }

        let start = *self.level.lock().unwrap();
        let mut level = start;
        loop {
            let sql = format!(
                "CREATE OR REPLACE TABLE _data AS SELECT * FROM {}",
                self.reader(level)
            );
            match run_with_progress(&conn, &sql, progress) {
                Ok(()) => break,
                Err(_) if self.load_cancelled.load(Ordering::Acquire) => {
                    return Err(Cancelled.into());
                }
                Err(err) if err.downcast_ref().is_some_and(|e| self.recode_after(e)) => {}
                Err(err) if level >= MAX_LEVEL => return Err(err),
                Err(_) => level += 1,
            }
        }
        *self.level.lock().unwrap() = level;
        self.note_level(level);
        self.loaded.store(true, Ordering::Release);
        self.clean_quietly(&conn);
        conn.execute_batch("DROP TABLE IF EXISTS _preview")?;
        *self.load_interrupt.lock().unwrap() = None;
        progress(1.0);
        self.frame()
    }

    /// Stops a running [`Dataset::materialize`]; it returns [`Cancelled`].
    pub fn cancel_load(&self) {
        self.load_cancelled.store(true, Ordering::Release);
        if let Some(handle) = self.load_interrupt.lock().unwrap().as_ref() {
            handle.interrupt();
        }
    }

    /// Reads `limit` rows starting at `offset`, as text.
    pub fn fetch(&self, frame: &Frame, offset: u64, limit: u64) -> Result<Page> {
        let mut page = Page {
            offset,
            rows: Vec::new(),
        };
        if frame.columns.is_empty() {
            return Ok(page);
        }
        let conn = self.connection()?;
        let select = frame
            .columns
            .iter()
            .map(|c| {
                let col = quote_ident(&c.name);
                // Floats print with 12 significant digits: full round-trip text is
                // long, and a truncated `5.8109…e-05` hides its exponent.
                if c.format.is_some() {
                    format!("CAST({col} AS DOUBLE)")
                } else if is_float(&c.sql_type) {
                    format!("printf('%.12g', {col})")
                } else {
                    format!("left(CAST({col} AS VARCHAR), {MAX_CELL_CHARS})")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT {select} FROM {} LIMIT {limit} OFFSET {offset}",
            frame.relation
        ))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            page.rows.push(
                frame
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| match c.format {
                        Some(format) => Ok(row.get::<_, Option<f64>>(i)?.map(|v| format.render(v))),
                        None => row.get::<_, Option<String>>(i),
                    })
                    .collect::<duckdb::Result<_>>()?,
            );
        }
        Ok(page)
    }

    /// Sorts and filters `base` into a new frame. A newer call cancels an older
    /// one still running, which then returns [`Cancelled`].
    pub fn apply(&self, base: &Frame, spec: &ViewSpec) -> Result<Frame> {
        if spec.is_empty() {
            return Ok(base.clone());
        }
        let conn = self.connection()?;
        let handle = conn.interrupt_handle();
        if let Some(previous) = self.view_interrupt.lock().unwrap().replace(handle.clone()) {
            previous.interrupt();
        }
        let id = self.next_id();
        let table = format!("_view_{id}");
        let created = conn.execute_batch(&format!(
            "CREATE TABLE {table} AS SELECT * FROM {}{}",
            base.relation,
            spec.clauses(&base.columns)
        ));
        if let Err(err) = created {
            return Err(if is_current(&self.view_interrupt, &handle) {
                err.into()
            } else {
                Cancelled.into()
            });
        }
        let owner = scratch_table(&conn, &table)?;
        let rows = count(&conn, &table)?;
        Ok(Frame {
            id,
            relation: table,
            columns: base.columns.clone(),
            rows: Some(rows),
            _table: Some(owner),
        })
    }

    /// Runs SQL where the file is the view `data`. Queries that return rows
    /// become a frame you can page, sort and profile like the file itself.
    pub fn query(&self, sql: &str) -> Result<SqlOutcome> {
        self.query_with(self.query_ticket(), sql)
    }

    /// Claims the next query slot. Take it when the user presses Run (on the UI
    /// thread), then pass it to [`Dataset::query_with`] on a worker, so request
    /// order is fixed before any worker runs.
    pub fn query_ticket(&self) -> QueryTicket {
        QueryTicket(self.latest_query.fetch_add(1, Ordering::AcqRel) + 1)
    }

    /// Like [`Dataset::query`], but returns [`Cancelled`] without running if a
    /// newer ticket was issued or the query was cancelled first. A stale request
    /// never interrupts a newer one, so nothing needs to be retried.
    pub fn query_with(&self, ticket: QueryTicket, sql: &str) -> Result<SqlOutcome> {
        let sql = sql.trim().trim_end_matches(';').trim();
        if sql.is_empty() {
            bail!("type a query first");
        }
        let conn = self.connection()?;
        let handle = conn.interrupt_handle();
        {
            let mut slot = self.query_interrupt.lock().unwrap();
            if self.latest_query.load(Ordering::Acquire) != ticket.0 {
                return Err(Cancelled.into());
            }
            if let Some(previous) = slot.replace(handle.clone()) {
                previous.interrupt();
            }
        }
        let table = format!("_query_{}", self.next_id());
        let as_table =
            // The newline keeps a trailing `-- comment` from swallowing the `)`.
            conn.execute_batch(&format!("CREATE TABLE {table} AS SELECT * FROM ({sql}\n)"));
        let cancelled = || !is_current(&self.query_interrupt, &handle);
        match as_table {
            Ok(()) => {
                let owner = scratch_table(&conn, &table)?;
                Ok(SqlOutcome::Rows(Frame {
                    id: self.next_id(),
                    columns: describe(&conn, &table)?.into(),
                    rows: Some(count(&conn, &table)?),
                    relation: table,
                    _table: Some(owner),
                }))
            }
            Err(_) if cancelled() => Err(Cancelled.into()),
            // Not a SELECT (or a syntax error): run it as written so the user
            // sees DuckDB's own error for what they typed.
            Err(_) => match conn.execute_batch(sql) {
                Ok(()) => Ok(SqlOutcome::Done),
                Err(_) if cancelled() => Err(Cancelled.into()),
                Err(err) => Err(err.into()),
            },
        }
    }

    /// Stops a running [`Dataset::query`]; it returns [`Cancelled`].
    pub fn cancel_query(&self) {
        let mut slot = self.query_interrupt.lock().unwrap();
        // Also void tickets whose workers haven't started yet.
        self.latest_query.fetch_add(1, Ordering::AcqRel);
        if let Some(handle) = slot.take() {
            handle.interrupt();
        }
    }

    /// Writes every row of `frame` to `path`.
    pub fn export(&self, frame: &Frame, path: &Path, format: ExportFormat) -> Result<()> {
        let options = match format {
            ExportFormat::Csv => "FORMAT csv, HEADER true",
            ExportFormat::Tsv => "FORMAT csv, HEADER true, DELIMITER '\t'",
            ExportFormat::Parquet => "FORMAT parquet, COMPRESSION zstd",
            ExportFormat::Json => "FORMAT json",
        };
        self.connection()?.execute_batch(&format!(
            "COPY (SELECT * FROM {}) TO {} ({options})",
            frame.relation,
            quote_str(&path.to_string_lossy())
        ))?;
        Ok(())
    }
}

impl Drop for Dataset {
    fn drop(&mut self) {
        self.cancel_load();
        // Close our connection before deleting its files. Connections cloned by
        // calls still running elsewhere keep the database open until they finish.
        if let Ok(root) = self.root.get_mut() {
            root.take();
        }
        let _ = std::fs::remove_dir_all(&self.session_dir);
    }
}

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Deletes scratch databases left by earlier runs that exited without cleaning
/// up. Only removes `session-<pid>-*` folders whose process is gone.
pub fn clean_cache(cache_dir: &Path) {
    remove_sessions(cache_dir, |pid| {
        pid as u32 != std::process::id() && !process_alive(pid)
    });
}

/// Deletes this process's scratch databases. Call on quit, when open datasets
/// won't get dropped; the next launch's [`clean_cache`] is the fallback.
pub fn remove_own_sessions(cache_dir: &Path) {
    remove_sessions(cache_dir, |pid| pid as u32 == std::process::id());
}

fn remove_sessions(cache_dir: &Path, should_remove: impl Fn(i32) -> bool) {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("session-"))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<i32>().ok())
        else {
            continue;
        };
        if should_remove(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn process_alive(pid: i32) -> bool {
    // Signal 0 checks existence without sending anything. EPERM means it exists.
    unsafe { libc::kill(pid, 0) == 0 || *libc::__error() == libc::EPERM }
}

/// Whether `handle` still belongs to the latest call. Newer calls replace it,
/// and cancelling removes it.
pub(crate) fn is_current(
    slot: &Mutex<Option<Arc<InterruptHandle>>>,
    handle: &Arc<InterruptHandle>,
) -> bool {
    slot.lock()
        .unwrap()
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, handle))
}

fn scratch_table(conn: &Connection, name: &str) -> Result<Arc<ScratchTable>> {
    Ok(Arc::new(ScratchTable {
        name: name.to_string(),
        conn: Mutex::new(conn.try_clone()?),
    }))
}

fn count(conn: &Connection, relation: &str) -> Result<u64> {
    let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {relation}"), [], |r| {
        r.get(0)
    })?;
    Ok(n as u64)
}

/// Whether DuckDB reads `path` decompressing it, going by its name.
pub(crate) fn compressed(path: &Path) -> bool {
    let name = path.to_string_lossy().to_ascii_lowercase();
    name.ends_with(".gz") || name.ends_with(".zst")
}

fn is_float(sql_type: &str) -> bool {
    matches!(sql_type, "FLOAT" | "DOUBLE" | "REAL" | "FLOAT4" | "FLOAT8")
}

fn format_reader(function: &str, path: &str, options: &[&str]) -> String {
    let options: String = options.iter().map(|o| format!(", {o}")).collect();
    format!("{function}({path}{options})")
}

pub(crate) fn describe(conn: &Connection, relation: &str) -> Result<Vec<Column>> {
    let mut stmt = conn.prepare(&format!("DESCRIBE SELECT * FROM {relation}"))?;
    let columns = stmt
        .query_map([], |r| {
            Ok(Column::new(r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<duckdb::Result<Vec<_>>>()?;
    Ok(columns)
}

/// Runs `sql`, polling DuckDB's progress estimate every 100 ms.
fn run_with_progress(conn: &Connection, sql: &str, progress: &(dyn Fn(f32) + Sync)) -> Result<()> {
    let handle = conn.progress_handle();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(100));
                if let Some(pct) = handle.query_progress().percentage() {
                    progress((pct / 100.0).clamp(0.0, 0.99) as f32);
                }
            }
        });
        let result = conn.execute_batch(sql);
        done.store(true, Ordering::Release);
        result
    })?;
    Ok(())
}
