//! One open file: owns the dataset and the frames derived from it, and
//! coordinates the grid, search, filters, chart and stats panels and SQL drawer. Every
//! dataset call runs on the background executor; results are matched against
//! generation counters so stale work is dropped.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use da_core::{
    Cancelled, Column, ColumnSummary, Conversion, Dataset, Decimal, Delimiter, Distribution,
    Encoding, ExportFormat, FileKind, Filter, FilterOp, Frame, Guessed, ImportPlan, NumberFormat,
    OpenOptions, Part, ReadAs, RowRole, SortKey, SqlOutcome, ValueOptions, ViewSpec, tidy_number,
};
use futures::StreamExt;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::DropdownMenu;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{DataTable, TableEvent, TableSelection, TableState};
use gpui_kit::component::{
    ActiveTheme, FocusableExt, Icon, Sizable, Size, ThemeStyled, TitleBar, WindowExt, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde::Deserialize;

use crate::chart::{self, ChartEvent, ChartPanel};
use crate::filter::{FilterEditor, FilterEvent, MenuFor};
use crate::format::{formatted, group_digits, percent, plural};
use crate::grid::{GridDelegate, GridEvent, PREVIEW_ROWS};
use crate::import::{ImportEvent, ImportPanel, Reading};
use crate::keymap;
use crate::platform;
use crate::sql::{QueryStatus, SqlEvent, SqlPanel};
use crate::stats::{StatsEvent, StatsPanel};
use crate::theme::{self, HEADER_HEIGHT, ROW_HEIGHT, STATS_WIDTH, TOOLBAR_HEIGHT, tabular};

actions!(
    document,
    [
        ToggleStats,
        ToggleChart,
        ToggleSql,
        Find,
        Copy,
        Export,
        ShowFileData,
        ClearFilters,
        CancelLoad,
        CopyCell,
        CopyRow,
        GoToRow,
        AdjustImport,
        /// Escape: closes the "Go to row" field, the file notes or the details.
        Dismiss,
    ]
);

/// Export formats as an action payload (the core enum has no serde derive).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum ExportKind {
    Csv,
    Parquet,
    Json,
}

impl From<ExportKind> for ExportFormat {
    fn from(kind: ExportKind) -> Self {
        match kind {
            ExportKind::Csv => ExportFormat::Csv,
            ExportKind::Parquet => ExportFormat::Parquet,
            ExportKind::Json => ExportFormat::Json,
        }
    }
}

#[derive(Action, Clone, Debug, PartialEq, Eq, Deserialize)]
#[action(namespace = document, no_json)]
pub struct ExportAs(pub ExportKind);

/// The key context of a document window; shortcuts in `keymap` bound here
/// work only while a document has focus.
pub const CONTEXT: &str = "Document";
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(220);
const SQL_HEIGHT: f32 = 210.0;
/// The share of the grid's height the chart panel takes when open.
const CHART_SHARE: f32 = 0.45;

pub fn init(cx: &mut App) {
    cx.bind_keys([
        // The panel, find and export shortcuts can be changed, so they're in `keymap`.
        KeyBinding::new(cmd_or_ctrl!("c"), Copy, Some("DataTable")),
        KeyBinding::new("escape", Dismiss, Some(CONTEXT)),
    ]);
}

#[derive(Clone, Debug)]
enum DocState {
    Opening,
    Failed(String),
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Load {
    /// Nothing to load, or loading finished.
    Done,
    Loading(f32),
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    File,
    Query,
}

/// What's being computed in the background, for the status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Busy {
    Idle,
    Sorting,
    Filtering,
    Searching,
    Querying,
    Exporting,
    Reading,
}

impl Busy {
    fn label(self) -> &'static str {
        match self {
            Busy::Idle => "",
            Busy::Sorting => "Sorting…",
            Busy::Filtering => "Filtering…",
            Busy::Searching => "Searching…",
            Busy::Querying => "Running query…",
            Busy::Exporting => "Exporting…",
            Busy::Reading => "Reading values…",
        }
    }
}

/// How the user chose to read the file. They outlive the dataset, so a
/// reopen can pass them on; each way of reopening keeps what still applies:
/// - Apply in Adjust import sets the plan and decimal mark, keeping the rest.
/// - A new delimiter or encoding finds the tables anew, so the plan and the
///   column types go; the decimal mark stays.
/// - Another tab drops the plan and column types, which belong to the
///   table they were made for; delimiter, encoding and decimal mark stay.
/// - Reset to detected drops everything.
/// - Load anyway, after a cancelled load, keeps everything.
#[derive(Clone, Debug, Default, PartialEq)]
struct Choices {
    /// How the open part is read; `None` reads it as detected.
    plan: Option<ImportPlan>,
    reading: Reading,
    /// Read as choices by column name; columns not listed are automatic.
    types: std::collections::BTreeMap<String, ReadAs>,
}

impl Choices {
    /// For another part of the file.
    fn switch_part(&mut self) {
        self.plan = None;
        self.types.clear();
    }

    /// For a new delimiter or encoding: the tables are found anew.
    fn redetect(&mut self, reading: Reading) {
        self.switch_part();
        self.reading = reading;
    }

    /// Whether the user changed how the file is read, leaving out column
    /// types, which the strip's summary already reflects.
    fn adjusted(&self) -> bool {
        self.plan.is_some() || self.reading != Reading::default()
    }

    fn values(&self) -> ValueOptions {
        ValueOptions {
            decimal: self.reading.decimal,
            types: self.types.clone(),
        }
    }

    /// The options to open `part` with, all but the cache directory.
    fn options(&self, part: Option<usize>) -> OpenOptions {
        OpenOptions {
            part,
            plan: self.plan,
            delimiter: self.reading.delimiter,
            encoding: self.reading.encoding,
            values: self.values(),
            ..Default::default()
        }
    }
}

/// SUMMARIZE + sketches for one frame, shared with the header and stats panel.
struct Profile {
    frame_id: u64,
    summaries: Arc<Vec<ColumnSummary>>,
    sketches: Arc<Vec<Distribution>>,
}

pub struct Document {
    focus_handle: FocusHandle,
    path: PathBuf,
    name: SharedString,
    state: DocState,
    dataset: Option<Arc<Dataset>>,
    /// The whole file.
    base: Option<Frame>,
    /// The last SQL result, when the grid is showing one, and its query.
    query_frame: Option<Frame>,
    query_sql: Option<String>,
    /// The frame on screen: the base, a sorted/filtered copy, or a query result.
    view: Option<Frame>,
    source: Source,
    spec: ViewSpec,
    load: Load,
    notice: Option<String>,
    /// The file's parts (tables and sheets), shown as tabs.
    parts: Vec<Part>,
    /// The open part, or `None` until the first open picks one.
    part: Option<usize>,
    choices: Choices,
    /// Whether the Adjust import panel has taken over the grid's place.
    adjusting: bool,
    /// Where the open File notes popover is anchored, if it is open.
    notes_at: Option<Point<Pixels>>,
    /// Where the open Details popover is anchored, if it is open.
    details_at: Option<Point<Pixels>>,
    busy: Busy,
    apply_generation: u64,
    /// Separate from `apply_generation`, so sorting or filtering while SQL runs
    /// doesn't drop the query's result and leave its panel stuck on "Running".
    query_generation: u64,
    open_generation: u64,
    /// Bumped per request to read values another way.
    values_generation: u64,
    profile: Option<Profile>,
    selected_column: Option<usize>,
    /// Where the open column menu is anchored, if one is open.
    header_menu: Option<Point<Pixels>>,
    /// A column menu to initialise on the next render (it needs the window).
    pending_menu: Option<MenuFor>,
    /// Errors from background work, shown as toasts on the next render.
    pending_errors: Vec<String>,
    show_stats: bool,
    show_chart: bool,
    show_sql: bool,

    table: Entity<TableState<GridDelegate>>,
    search: Entity<InputState>,
    search_task: Task<()>,
    /// Set by [`Document::reset_spec`]; the next render empties the search field.
    clear_search_input: bool,
    /// The "Go to row" field in the status bar.
    goto: Entity<InputState>,
    goto_open: bool,
    filter_editor: Entity<FilterEditor>,
    stats: Entity<StatsPanel>,
    chart: Entity<ChartPanel>,
    sql: Entity<SqlPanel>,
    import: Entity<ImportPanel>,
    _subscriptions: Vec<Subscription>,
}

impl Document {
    /// Redraws the grid's columns when the zoom changes. The table keeps
    /// column widths as drawn, so they're converted back to actual size (with
    /// any resizing by hand) and drawn again at the new zoom.
    fn follow_zoom(cx: &mut Context<Self>) -> Subscription {
        let mut zoom = theme::zoom(cx);
        cx.observe_global::<gpui_kit::component::Theme>(move |this, cx| {
            let before = std::mem::replace(&mut zoom, theme::zoom(cx));
            if before == zoom {
                return;
            }
            this.table.update(cx, |table, cx| {
                let columns = table.delegate().column_count();
                let widths = (1..=columns)
                    .filter_map(|ix| table.col_width(ix).map(|w| f32::from(w) / before))
                    .collect();
                table.delegate_mut().set_widths(widths);
                table.refresh(cx);
            });
        })
    }

    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name: SharedString = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
            .into();
        window.set_window_title(&name);

        let table = cx.new(|cx| {
            TableState::new(GridDelegate::new(), window, cx)
                .cell_selectable(true)
                .row_header(false)
                .col_selectable(false)
                .col_movable(false)
                .sortable(false)
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search all columns"));
        let goto = cx.new(|cx| InputState::new(window, cx).placeholder("Row number"));
        let filter_editor = cx.new(|cx| FilterEditor::new(window, cx));
        let stats = cx.new(|_| StatsPanel::new());
        let chart = cx.new(|_| ChartPanel::new());
        let sql = cx.new(|cx| SqlPanel::new(window, cx));
        let import = cx.new(|cx| ImportPanel::new(window, cx));

        let subscriptions = vec![
            cx.subscribe(&table, Self::on_grid_event),
            cx.subscribe(&table, Self::on_table_event),
            cx.subscribe(&search, Self::on_search_event),
            cx.subscribe(&goto, Self::on_goto_event),
            cx.subscribe_in(&filter_editor, window, Self::on_filter_event),
            cx.subscribe(&stats, Self::on_stats_event),
            cx.subscribe_in(&chart, window, Self::on_chart_event),
            cx.subscribe_in(&sql, window, Self::on_sql_event),
            cx.subscribe_in(&import, window, Self::on_import_event),
            Self::follow_zoom(cx),
        ];

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            path,
            name,
            state: DocState::Opening,
            dataset: None,
            base: None,
            query_frame: None,
            query_sql: None,
            view: None,
            source: Source::File,
            spec: ViewSpec::default(),
            load: Load::Done,
            notice: None,
            parts: Vec::new(),
            part: None,
            choices: Choices::default(),
            adjusting: false,
            notes_at: None,
            details_at: None,
            busy: Busy::Idle,
            apply_generation: 0,
            query_generation: 0,
            open_generation: 0,
            values_generation: 0,
            profile: None,
            selected_column: None,
            header_menu: None,
            pending_menu: None,
            pending_errors: Vec::new(),
            show_stats: false,
            show_chart: false,
            show_sql: false,
            table,
            search,
            search_task: Task::ready(()),
            clear_search_input: false,
            goto,
            goto_open: false,
            filter_editor,
            stats,
            chart,
            sql,
            import,
            _subscriptions: subscriptions,
        };
        this.open_part(None, cx);
        this
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    // ----- opening and loading -------------------------------------------------

    /// Opens (or re-opens, for another part) the file in the background,
    /// with the user's choices. `None` lets the engine pick the part with
    /// the most data.
    pub(crate) fn open_part(&mut self, part: Option<usize>, cx: &mut Context<Self>) {
        if let Some(old) = self.dataset.take() {
            old.cancel_load();
            old.cancel_query();
        }
        self.sql
            .update(cx, |panel, cx| panel.set_status(QueryStatus::Idle, cx));
        self.state = DocState::Opening;
        self.base = None;
        self.view = None;
        self.query_frame = None;
        self.source = Source::File;
        self.reset_spec();
        self.profile = None;
        self.load = Load::Done;
        self.selected_column = None;
        self.part = part;
        self.adjusting = false;
        self.notes_at = None;
        self.details_at = None;
        self.busy = Busy::Idle;
        // Results from the old dataset's filters or queries must not land here.
        self.apply_generation += 1;
        self.query_generation += 1;
        self.open_generation += 1;
        let generation = self.open_generation;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear();
            table.refresh(cx);
        });
        self.stats.update(cx, |stats, cx| stats.clear(cx));
        self.chart.update(cx, |chart, cx| chart.clear(cx));
        self.import.update(cx, |panel, cx| panel.clear(cx));

        let path = self.path.clone();
        let options = self.choices.options(part);
        cx.spawn(async move |this, cx| {
            let opened = cx
                .background_spawn(async move {
                    let options = OpenOptions {
                        cache_dir: platform::cache_dir(),
                        ..options
                    };
                    let dataset = Dataset::open(&path, &options)?;
                    let frame = dataset.frame()?;
                    Ok::<_, anyhow::Error>((Arc::new(dataset), frame))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.open_generation != generation {
                    return;
                }
                match opened {
                    Ok((dataset, frame)) => {
                        this.parts = dataset.parts().to_vec();
                        this.part = Some(dataset.part());
                        // File > Adjust Import… follows whether there is a plan.
                        crate::refresh_menus(cx);
                        this.notice = dataset.notice();
                        this.state = DocState::Ready;
                        this.dataset = Some(dataset.clone());
                        this.base = Some(frame.clone());
                        this.show_frame(frame, cx);
                        if dataset.is_loaded() {
                            this.profile_view(cx);
                        } else {
                            this.start_load(cx);
                        }
                    }
                    Err(err) => this.state = DocState::Failed(format!("{err:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Opens another part of the file, as clicking its tab does.
    pub(crate) fn show_part(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.part != Some(ix) {
            self.choices.switch_part();
            self.open_part(Some(ix), cx);
        }
    }

    /// Loads a CSV/JSON file fully, reporting progress to the status bar.
    fn start_load(&mut self, cx: &mut Context<Self>) {
        let Some(dataset) = self.dataset.clone() else {
            return;
        };
        self.load = Load::Loading(0.0);
        let generation = self.open_generation;
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<f32>();

        cx.spawn(async move |this, cx| {
            while let Some(progress) = rx.next().await {
                let _ = this.update(cx, |this, cx| {
                    if this.open_generation == generation && this.load != Load::Done {
                        this.load = Load::Loading(progress);
                        cx.notify();
                    }
                });
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    dataset.materialize(&move |p| {
                        let _ = tx.unbounded_send(p);
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.open_generation != generation {
                    return;
                }
                match loaded {
                    Ok(frame) => {
                        this.load = Load::Done;
                        this.notice = this.dataset.as_ref().and_then(|d| d.notice());
                        this.show_base(frame, cx);
                    }
                    Err(err) if err.is::<Cancelled>() => this.load = Load::Cancelled,
                    Err(err) => {
                        this.load = Load::Cancelled;
                        this.notify_error(format!("Couldn't load the whole file: {err:#}"), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Makes `frame` the whole file and shows it with the sort, filters and
    /// search re-applied. A SQL result on screen stays ("Back to file" shows
    /// the new frame); it just gets the stats it couldn't compute while loading.
    fn show_base(&mut self, frame: Frame, cx: &mut Context<Self>) {
        self.base = Some(frame.clone());
        if self.source == Source::Query {
            self.profile_view(cx);
        } else if self.spec.is_empty() {
            self.show_frame(frame, cx);
            self.profile_view(cx);
        } else {
            self.apply(Busy::Filtering, cx);
        }
    }

    /// Reads `column` as `read_as` from now on (`Auto` forgets the choice),
    /// without reopening the file. A filter on the column goes: its values
    /// were typed for the old type.
    pub(crate) fn read_column_as(
        &mut self,
        column: String,
        read_as: ReadAs,
        cx: &mut Context<Self>,
    ) {
        if read_as == ReadAs::Auto {
            self.choices.types.remove(&column);
        } else {
            self.choices.types.insert(column.clone(), read_as);
        }
        self.spec.filters.retain(|f| f.column != column);
        self.sync_spec_to_grid(cx);
        self.read_values(cx);
    }

    /// Makes the dataset read values as the choices say, in the background,
    /// then shows the new whole-file frame.
    fn read_values(&mut self, cx: &mut Context<Self>) {
        let Some(dataset) = self.dataset.clone() else {
            return;
        };
        let values = self.choices.values();
        // A sort or filter still running derives from the base frame about
        // to be replaced, and `read_column_as` may have just dropped its
        // filter. `show_base` re-applies only when some remain, so without
        // this an older result could land after the new base and put back
        // the old column types. A SQL result's sort doesn't use the base.
        // (Not unit-tested: it needs a window and a worker held mid-flight.)
        if self.source == Source::File {
            self.apply_generation += 1;
        }
        self.values_generation += 1;
        let (generation, open) = (self.values_generation, self.open_generation);
        self.busy = Busy::Reading;
        cx.spawn(async move |this, cx| {
            let (read, now) = cx
                .background_spawn(async move {
                    let read = dataset.set_values(&values);
                    (read, dataset.values())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.open_generation != open {
                    return;
                }
                // Requests can finish out of order, and the last to run is what
                // the dataset reads. If that isn't the latest choice, ask again.
                if now != this.choices.values() {
                    this.read_values(cx);
                    return;
                }
                if this.values_generation != generation {
                    return;
                }
                this.busy = Busy::Idle;
                match read {
                    Ok(frame) => this.show_base(frame, cx),
                    Err(err) => {
                        this.notify_error(
                            format!("Couldn't read the values that way: {err:#}"),
                            cx,
                        );
                        // The filter dropped for it goes from the view too.
                        this.apply(Busy::Filtering, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn close_details(&mut self, cx: &mut Context<Self>) {
        self.details_at = None;
        cx.notify();
    }

    fn cancel_load(&mut self, _: &CancelLoad, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(dataset) = &self.dataset {
            dataset.cancel_load();
        }
        self.load = Load::Cancelled;
        cx.notify();
    }

    /// Switches the grid to `frame`, keeping column widths when the columns match.
    fn show_frame(&mut self, frame: Frame, cx: &mut Context<Self>) {
        let Some(dataset) = self.dataset.clone() else {
            return;
        };
        self.view = Some(frame.clone());
        let sort = self.spec.sort.clone();
        let filtered: Vec<String> = self.spec.filters.iter().map(|f| f.column.clone()).collect();
        let empty_message = if !self.spec.is_empty() {
            "No rows match. Try loosening a filter or clearing the search."
        } else if self.source == Source::Query {
            "The query returned no rows."
        } else if self.parts.len() > 1 {
            "This table is empty."
        } else {
            "This file has no rows."
        };
        let zoom = theme::zoom(cx);
        self.table.update(cx, |table, cx| {
            // The table reports widths as drawn; the delegate keeps them at actual size.
            let widths: Vec<f32> = (0..frame.columns.len())
                .filter_map(|ix| table.col_width(ix + 1).map(|w| f32::from(w) / zoom))
                .collect();
            let delegate = table.delegate_mut();
            delegate.set_frame(dataset, frame, widths);
            delegate.set_sort(sort);
            delegate.set_filtered(filtered);
            delegate.set_empty_message(empty_message);
            table.refresh(cx);
            table.delegate_mut().refetch(cx);
            cx.notify();
        });
        self.sync_header_profile(cx);
        self.refresh_stats(cx);
        self.refresh_chart(cx);
        cx.notify();
    }

    /// Computes the summary and sketches for the view frame in the background.
    fn profile_view(&mut self, cx: &mut Context<Self>) {
        let (Some(dataset), Some(frame)) = (self.dataset.clone(), self.view.clone()) else {
            return;
        };
        if !dataset.is_loaded() {
            return;
        }
        if self
            .profile
            .as_ref()
            .is_some_and(|p| p.frame_id == frame.id)
        {
            return;
        }
        let frame_id = frame.id;
        cx.spawn(async move |this, cx| {
            let (summaries, sketches) = {
                let (d1, f1) = (dataset.clone(), frame.clone());
                let (d2, f2) = (dataset, frame);
                futures::join!(
                    cx.background_spawn(async move { d1.summarize(&f1) }),
                    cx.background_spawn(async move { d2.sketches(&f2) })
                )
            };
            let _ = this.update(cx, |this, cx| {
                if this.view.as_ref().map(|f| f.id) != Some(frame_id) {
                    return;
                }
                match (summaries, sketches) {
                    (Ok(summaries), Ok(sketches)) => {
                        this.profile = Some(Profile {
                            frame_id,
                            summaries: Arc::new(summaries),
                            sketches: Arc::new(sketches),
                        });
                        this.sync_header_profile(cx);
                        this.refresh_stats(cx);
                    }
                    (Err(err), _) | (_, Err(err)) => {
                        eprintln!("profiling failed: {err:#}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Pushes the profile for the current view into the grid headers.
    fn sync_header_profile(&mut self, cx: &mut Context<Self>) {
        let view_id = self.view.as_ref().map(|f| f.id);
        let Some(profile) = self
            .profile
            .as_ref()
            .filter(|p| Some(p.frame_id) == view_id)
        else {
            return;
        };
        let (summaries, sketches) = (profile.summaries.clone(), profile.sketches.clone());
        self.table.update(cx, |table, cx| {
            if table.delegate_mut().set_profiles(&summaries, &sketches) {
                table.refresh(cx);
            }
            cx.notify();
        });
    }

    fn summary_for(&self, column: &Column) -> Option<ColumnSummary> {
        let view_id = self.view.as_ref().map(|f| f.id);
        self.profile
            .as_ref()
            .filter(|p| Some(p.frame_id) == view_id)?
            .summaries
            .iter()
            .find(|s| s.name == column.name)
            .cloned()
    }

    fn sketch_for(&self, col: usize) -> Option<Distribution> {
        let view_id = self.view.as_ref().map(|f| f.id);
        self.profile
            .as_ref()
            .filter(|p| Some(p.frame_id) == view_id)?
            .sketches
            .get(col)
            .cloned()
    }

    // ----- sort, filter, search --------------------------------------------------

    /// Re-derives the view from the current spec and shows it when it lands.
    fn apply(&mut self, busy: Busy, cx: &mut Context<Self>) {
        let Some(dataset) = self.dataset.clone() else {
            return;
        };
        let source = match self.source {
            Source::Query => self.query_frame.clone(),
            Source::File => self.base.clone(),
        };
        let Some(source) = source else {
            return;
        };
        self.apply_generation += 1;
        let generation = self.apply_generation;
        self.sync_spec_to_grid(cx);
        if self.spec.is_empty() {
            self.busy = Busy::Idle;
            self.show_frame(source, cx);
            self.profile_view(cx);
            return;
        }
        self.busy = busy;
        let spec = self.spec.clone();
        cx.spawn(async move |this, cx| {
            let applied = cx
                .background_spawn(async move { dataset.apply(&source, &spec) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.apply_generation != generation {
                    return;
                }
                this.busy = Busy::Idle;
                match applied {
                    Ok(frame) => {
                        this.show_frame(frame, cx);
                        this.profile_view(cx);
                    }
                    // Only an older worker racing ahead can cancel the newest request.
                    Err(err) if err.is::<Cancelled>() => this.apply(busy, cx),
                    Err(err) => this.notify_error(format!("{err:#}"), cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Shows the pending sort and filter state in the headers right away,
    /// before the derived frame lands.
    fn sync_spec_to_grid(&mut self, cx: &mut Context<Self>) {
        let sort = self.spec.sort.clone();
        let filtered: Vec<String> = self.spec.filters.iter().map(|f| f.column.clone()).collect();
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.set_sort(sort);
            delegate.set_filtered(filtered);
            cx.notify();
        });
    }

    /// Scrolls to and selects a 1-based row number.
    fn go_to_row(&mut self, _: &GoToRow, window: &mut Window, cx: &mut Context<Self>) {
        self.goto_open = true;
        self.goto.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.notes_at.take().is_some() || self.details_at.take().is_some() {
            cx.notify();
        }
        if self.goto_open {
            self.goto_open = false;
            self.table.read(cx).focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    fn on_goto_event(&mut self, _: Entity<InputState>, event: &InputEvent, cx: &mut Context<Self>) {
        match event {
            InputEvent::PressEnter { .. } => {
                let text = self.goto.read(cx).value().replace(',', "");
                let rows = self.table.read(cx).delegate().rows();
                let has_columns = self.view.as_ref().is_some_and(|f| !f.columns.is_empty());
                if let (Ok(n), true) = (text.trim().parse::<u64>(), rows > 0) {
                    let row = (n.clamp(1, rows) - 1) as usize;
                    self.table.update(cx, |table, cx| {
                        table.scroll_to_row(row, cx);
                        if has_columns {
                            table.set_selected_cell(row, 1, cx);
                        }
                    });
                }
                self.goto_open = false;
                cx.notify();
            }
            InputEvent::Blur => {
                self.goto_open = false;
                cx.notify();
            }
            _ => {}
        }
    }

    fn toggle_sort(&mut self, col: usize, additive: bool, cx: &mut Context<Self>) {
        let Some(name) = self.column_name(col) else {
            return;
        };
        let existing = self.spec.sort.iter().position(|k| k.column == name);
        match (existing, additive) {
            (Some(ix), _) => {
                if self.spec.sort[ix].descending {
                    self.spec.sort.remove(ix);
                } else {
                    self.spec.sort[ix].descending = true;
                }
            }
            (None, true) => self.spec.sort.push(SortKey {
                column: name,
                descending: false,
            }),
            (None, false) => {
                self.spec.sort = vec![SortKey {
                    column: name,
                    descending: false,
                }]
            }
        }
        self.apply(Busy::Sorting, cx);
    }

    fn set_sort(&mut self, column: String, descending: bool, cx: &mut Context<Self>) {
        self.spec.sort = vec![SortKey { column, descending }];
        self.apply(Busy::Sorting, cx);
    }

    fn remove_sort(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.spec.sort.len() {
            self.spec.sort.remove(ix);
            self.apply(Busy::Sorting, cx);
        }
    }

    /// Adds `filter`, replacing any existing filter on the same column.
    fn add_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.spec.filters.retain(|f| f.column != filter.column);
        self.spec.filters.push(filter);
        self.apply(Busy::Filtering, cx);
    }

    fn remove_filter(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.spec.filters.len() {
            self.spec.filters.remove(ix);
            self.apply(Busy::Filtering, cx);
        }
    }

    /// Drops sort, filters and search, including a search still being typed, so
    /// the toolbar matches the grid. Callers re-show or re-apply the view.
    fn reset_spec(&mut self) {
        self.spec = ViewSpec::default();
        self.search_task = Task::ready(());
        self.clear_search_input = true;
    }

    fn clear_filters(&mut self, _: &ClearFilters, window: &mut Window, cx: &mut Context<Self>) {
        self.spec = ViewSpec::default();
        self.search.update(cx, |s, cx| s.set_value("", window, cx));
        self.apply(Busy::Filtering, cx);
    }

    fn column_name(&self, col: usize) -> Option<String> {
        self.view
            .as_ref()
            .and_then(|f| f.columns.get(col))
            .map(|c| c.name.clone())
    }

    fn on_search_event(
        &mut self,
        _: Entity<InputState>,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let text = self.search.read(cx).value().to_string();
                self.search_task = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SEARCH_DEBOUNCE).await;
                    let _ = this.update(cx, |this, cx| {
                        if this.spec.search != text {
                            this.spec.search = text;
                            this.apply(Busy::Searching, cx);
                        }
                    });
                });
            }
            InputEvent::PressEnter { .. } => {
                let text = self.search.read(cx).value().to_string();
                if self.spec.search != text {
                    self.spec.search = text;
                    self.apply(Busy::Searching, cx);
                }
            }
            _ => {}
        }
    }

    fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }

    // ----- grid events -----------------------------------------------------------

    fn on_grid_event(
        &mut self,
        _: Entity<TableState<GridDelegate>>,
        event: &GridEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            GridEvent::SortClicked { col, additive } => self.toggle_sort(*col, *additive, cx),
            GridEvent::HeaderMenu { col, position } => self.open_header_menu(*col, *position, cx),
        }
    }

    fn on_table_event(
        &mut self,
        _: Entity<TableState<GridDelegate>>,
        event: &TableEvent,
        cx: &mut Context<Self>,
    ) {
        if let TableEvent::SelectCell(_, col) = event
            && *col > 0
        {
            self.select_column(col - 1, cx);
        }
    }

    fn select_column(&mut self, col: usize, cx: &mut Context<Self>) {
        if self.selected_column == Some(col) {
            return;
        }
        self.selected_column = Some(col);
        self.refresh_stats(cx);
        cx.notify();
    }

    fn open_header_menu(&mut self, col: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(column) = self.view.as_ref().and_then(|f| f.columns.get(col).cloned()) else {
            return;
        };
        let existing = self
            .spec
            .filters
            .iter()
            .find(|f| f.column == column.name)
            .cloned();
        let sketch = self.sketch_for(col);
        // Columns of the file itself can be read another way; Parquet
        // columns are typed already.
        let read_as = self
            .dataset
            .clone()
            .filter(|d| self.source == Source::File && d.kind() != FileKind::Parquet)
            .map(|d| {
                let current = self.choices.types.get(&column.name).copied();
                (d, current.unwrap_or_default())
            });
        self.header_menu = Some(position);
        // The editor needs the window to focus its input, so it's set up in render.
        self.pending_menu = Some(MenuFor {
            col_ix: col,
            column,
            filter: existing,
            sketch,
            read_as,
        });
        cx.notify();
    }

    // ----- panel events -----------------------------------------------------------

    fn on_filter_event(
        &mut self,
        _: &Entity<FilterEditor>,
        event: &FilterEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.header_menu = None;
        match event {
            FilterEvent::Apply(filter) => self.add_filter(filter.clone(), cx),
            FilterEvent::Clear(column) => {
                self.spec.filters.retain(|f| &f.column != column);
                self.apply(Busy::Filtering, cx);
            }
            FilterEvent::Sort { column, descending } => {
                self.set_sort(column.clone(), *descending, cx)
            }
            FilterEvent::ShowStats(col) => {
                self.show_stats = true;
                self.selected_column = Some(*col);
                self.refresh_stats(cx);
                crate::refresh_menus(cx);
            }
            FilterEvent::ReadAs { column, read_as } => {
                self.read_column_as(column.clone(), *read_as, cx)
            }
            FilterEvent::Dismiss => {}
        }
        cx.notify();
    }

    fn on_stats_event(
        &mut self,
        _: Entity<StatsPanel>,
        event: &StatsEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            StatsEvent::AddFilter(filter) => self.add_filter(filter.clone(), cx),
            StatsEvent::Close => {
                self.show_stats = false;
                crate::refresh_menus(cx);
                cx.notify();
            }
        }
    }

    /// Points the stats panel at the selected column of the current view.
    fn refresh_stats(&mut self, cx: &mut Context<Self>) {
        if !self.show_stats {
            return;
        }
        let (Some(dataset), Some(frame)) = (self.dataset.clone(), self.view.clone()) else {
            return;
        };
        let Some(column) = self
            .selected_column
            .and_then(|col| frame.columns.get(col).cloned())
        else {
            self.stats.update(cx, |stats, cx| stats.clear(cx));
            return;
        };
        if !dataset.is_loaded() {
            return;
        }
        let summary = self.summary_for(&column);
        self.stats.update(cx, |stats, cx| {
            if stats.column() == Some(&column) && stats.frame_id() == Some(frame.id) {
                stats.set_summary(summary, cx);
            } else {
                stats.show(dataset, frame, column, summary, cx);
            }
        });
    }

    fn toggle_stats(&mut self, _: &ToggleStats, _: &mut Window, cx: &mut Context<Self>) {
        self.show_stats = !self.show_stats;
        if self.show_stats && self.selected_column.is_none() {
            self.selected_column = Some(0);
        }
        self.refresh_stats(cx);
        crate::refresh_menus(cx);
        cx.notify();
    }

    /// What the chart panel draws from: the frame on screen, once the file
    /// is fully loaded.
    fn chart_source(&self) -> Option<chart::Source> {
        let dataset = self.dataset.clone().filter(|d| d.is_loaded())?;
        let base = match self.source {
            Source::Query => self.query_frame.as_ref(),
            Source::File => self.base.as_ref(),
        }?;
        Some(chart::Source {
            dataset,
            frame: self.view.clone()?,
            view: self.spec.clone(),
            query: match self.source {
                Source::Query => self.query_sql.clone(),
                Source::File => None,
            },
            base: (base.id, base.rows),
        })
    }

    /// Points the chart panel at the frame on screen.
    fn refresh_chart(&mut self, cx: &mut Context<Self>) {
        if !self.show_chart {
            return;
        }
        if let Some(source) = self.chart_source() {
            self.chart.update(cx, |chart, cx| chart.show(source, cx));
        }
    }

    fn toggle_chart(&mut self, _: &ToggleChart, _: &mut Window, cx: &mut Context<Self>) {
        self.show_chart = !self.show_chart;
        if self.show_chart
            && let Some(source) = self.chart_source()
        {
            let selected = self.selected_column.and_then(|col| self.column_name(col));
            self.chart
                .update(cx, |chart, cx| chart.open(source, selected.as_deref(), cx));
        }
        crate::refresh_menus(cx);
        cx.notify();
    }

    pub fn chart_shown(&self) -> bool {
        self.show_chart
    }

    fn on_chart_event(
        &mut self,
        _: &Entity<ChartPanel>,
        event: &ChartEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ChartEvent::AddFilter(filter) => self.add_filter(filter.clone(), cx),
            ChartEvent::ShowSql(sql) => {
                self.show_sql = true;
                self.sql.update(cx, |panel, cx| {
                    panel.set_query(sql.clone(), window, cx);
                    panel.focus(window, cx);
                });
                crate::refresh_menus(cx);
            }
            ChartEvent::Close => {
                self.show_chart = false;
                crate::refresh_menus(cx);
            }
        }
        cx.notify();
    }

    fn toggle_sql(&mut self, _: &ToggleSql, window: &mut Window, cx: &mut Context<Self>) {
        self.show_sql = !self.show_sql;
        if self.show_sql {
            self.sql.update(cx, |sql, cx| sql.focus(window, cx));
        }
        crate::refresh_menus(cx);
        cx.notify();
    }

    pub fn stats_shown(&self) -> bool {
        self.show_stats
    }

    pub fn sql_shown(&self) -> bool {
        self.show_sql
    }

    fn on_sql_event(
        &mut self,
        _: &Entity<SqlPanel>,
        event: &SqlEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SqlEvent::Run(sql) => self.run_query(sql.clone(), cx),
            SqlEvent::Cancel => {
                if let Some(dataset) = &self.dataset {
                    dataset.cancel_query();
                }
            }
            SqlEvent::ShowFile => self.show_file_data(&ShowFileData, cx),
        }
    }

    fn run_query(&mut self, sql: String, cx: &mut Context<Self>) {
        let Some(dataset) = self.dataset.clone() else {
            return;
        };
        self.sql
            .update(cx, |panel, cx| panel.set_status(QueryStatus::Running, cx));
        self.busy = Busy::Querying;
        self.query_generation += 1;
        let generation = self.query_generation;
        // Taken now, on the UI thread, so a late-starting older worker can't
        // interrupt this query (see `Dataset::query_with`).
        let ticket = dataset.query_ticket();
        let sql_text = sql.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move { dataset.query_with(ticket, &sql) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.query_generation != generation {
                    return;
                }
                this.busy = Busy::Idle;
                let status = match outcome {
                    Ok(SqlOutcome::Rows(frame)) => {
                        // The result replaces the grid, so pending sorts/filters are stale.
                        this.apply_generation += 1;
                        let rows = frame.rows.unwrap_or(0);
                        this.source = Source::Query;
                        this.reset_spec();
                        this.query_frame = Some(frame.clone());
                        this.query_sql = Some(sql_text);
                        this.selected_column = None;
                        this.show_frame(frame, cx);
                        this.profile_view(cx);
                        QueryStatus::Rows(rows)
                    }
                    Ok(SqlOutcome::Done) => QueryStatus::Done,
                    Err(err) if err.is::<Cancelled>() => QueryStatus::Idle,
                    Err(err) => QueryStatus::Error(format!("{err:#}")),
                };
                this.sql
                    .update(cx, |panel, cx| panel.set_status(status, cx));
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn show_file_data(&mut self, _: &ShowFileData, cx: &mut Context<Self>) {
        if self.source == Source::File {
            return;
        }
        self.source = Source::File;
        self.reset_spec();
        self.selected_column = None;
        self.busy = Busy::Idle;
        self.apply_generation += 1;
        // Going back to the file abandons a query that is still running.
        self.query_generation += 1;
        if let Some(dataset) = &self.dataset {
            dataset.cancel_query();
        }
        if let Some(base) = self.base.clone() {
            self.show_frame(base, cx);
            self.profile_view(cx);
        }
        self.sql
            .update(cx, |panel, cx| panel.set_status(QueryStatus::Idle, cx));
        cx.notify();
    }

    // ----- adjust import -----------------------------------------------------------

    /// Whether File > Adjust Import… applies: the open part has a plan to edit.
    pub fn can_adjust_import(&self) -> bool {
        matches!(self.state, DocState::Ready)
            && self.dataset.as_ref().is_some_and(|d| d.plan().is_some())
    }

    /// Opens the Adjust import panel in place of the grid.
    fn adjust_import(&mut self, _: &AdjustImport, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dataset) = self.dataset.clone().filter(|_| self.can_adjust_import()) else {
            return;
        };
        self.adjusting = true;
        self.notes_at = None;
        self.details_at = None;
        let reading = self.choices.reading;
        self.import
            .update(cx, |panel, cx| panel.open(dataset, reading, window, cx));
        cx.notify();
    }

    fn on_import_event(
        &mut self,
        _: &Entity<ImportPanel>,
        event: &ImportEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ImportEvent::Apply(plan, decimal) => self.apply_plan(*plan, *decimal, cx),
            ImportEvent::Redetect(reading) => {
                self.choices.redetect(*reading);
                self.open_part(None, cx);
            }
            ImportEvent::Reset => self.reset_import(cx),
            ImportEvent::Cancel => {
                self.adjusting = false;
                self.table.read(cx).focus_handle(cx).focus(window, cx);
            }
        }
        cx.notify();
    }

    /// Reads the open part under `plan` and `decimal`, as Apply in Adjust
    /// import does. Nothing is reopened when neither changed.
    fn apply_plan(&mut self, plan: ImportPlan, decimal: Decimal, cx: &mut Context<Self>) {
        let same_plan = self.dataset.as_ref().and_then(|d| d.plan()) == Some(&plan);
        if same_plan && decimal == self.choices.reading.decimal {
            self.adjusting = false;
        } else {
            self.choices.plan = Some(plan);
            self.choices.reading.decimal = decimal;
            self.open_part(self.part, cx);
        }
    }

    /// Show totals and Hide totals in the strip: the open part's plan with
    /// totals rows kept in the data or set aside.
    pub(crate) fn keep_totals(&mut self, keep: bool, cx: &mut Context<Self>) {
        let Some(plan) = self.dataset.as_ref().and_then(|d| d.plan().copied()) else {
            return;
        };
        let decimal = self.choices.reading.decimal;
        self.apply_plan(
            ImportPlan {
                keep_totals: keep,
                ..plan
            },
            decimal,
            cx,
        );
        cx.notify();
    }

    /// Reopens the file as detected, forgetting every choice. Parts found
    /// with a chosen delimiter or encoding may not exist under Auto.
    fn reset_import(&mut self, cx: &mut Context<Self>) {
        let reading = self.choices.reading;
        let part = if reading.delimiter == Delimiter::Auto && reading.encoding == Encoding::Auto {
            self.part
        } else {
            None
        };
        self.choices = Choices::default();
        self.open_part(part, cx);
    }

    // ----- copy and export -------------------------------------------------------

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.table.read(cx).selection();
        self.copy_selection(selection, cx);
    }

    fn copy_cell(&mut self, _: &CopyCell, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((row, col)) = self.table.read(cx).right_clicked_cell() {
            self.copy_selection(TableSelection::Cell(row, col), cx);
        }
    }

    fn copy_row(&mut self, _: &CopyRow, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((row, _)) = self.table.read(cx).right_clicked_cell() {
            self.copy_selection(TableSelection::Row(row), cx);
        }
    }

    fn copy_selection(&mut self, selection: TableSelection, cx: &mut Context<Self>) {
        let table = self.table.read(cx);
        let delegate = table.delegate();
        let text = match selection {
            TableSelection::Cell(row, col) if col > 0 => delegate
                .cell(row as u64, col - 1)
                .map(|cell| cell.unwrap_or("").to_string()),
            TableSelection::Row(row) => delegate.row_cells(row as u64).map(|cells| {
                cells
                    .iter()
                    .map(|c| c.as_deref().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join("\t")
            }),
            TableSelection::Column(col) if col > 0 => self.column_name(col - 1),
            _ => None,
        };
        if let Some(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn export(&mut self, _: &Export, window: &mut Window, cx: &mut Context<Self>) {
        self.export_as(&ExportAs(ExportKind::Csv), window, cx);
    }

    /// Asks for a destination with the native save panel, then writes the
    /// current view there. The format follows the chosen extension.
    fn export_as(&mut self, action: &ExportAs, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(dataset), Some(frame)) = (self.dataset.clone(), self.view.clone()) else {
            return;
        };
        let format: ExportFormat = action.0.into();
        let stem = self
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "export".into());
        let suffix = match self.source {
            Source::Query => "-query",
            Source::File if !self.spec.is_empty() => "-filtered",
            Source::File => "",
        };
        let suggested = format!("{stem}{suffix}.{}", format.extension());
        let directory = self
            .path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested));

        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = receiver.await else {
                return;
            };
            let format = match path.extension().and_then(|e| e.to_str()) {
                Some("parquet" | "pq") => ExportFormat::Parquet,
                Some("json" | "jsonl" | "ndjson") => ExportFormat::Json,
                Some("tsv" | "tab") => ExportFormat::Tsv,
                Some("csv" | "txt") => ExportFormat::Csv,
                _ => format,
            };
            let _ = this.update(cx, |this, cx| {
                this.busy = Busy::Exporting;
                cx.notify();
            });
            let rows = frame.rows.unwrap_or(0);
            let exported = cx
                .background_spawn({
                    let path = path.clone();
                    async move { dataset.export(&frame, &path, format) }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = Busy::Idle;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match exported {
                    Ok(()) => {
                        let reveal = path.clone();
                        window.push_notification(
                            Notification::success(format!(
                                "Exported {} to {name}",
                                plural(rows, "row")
                            ))
                            .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
                            cx,
                        );
                    }
                    Err(err) => window.push_notification(
                        Notification::error(format!("Export failed: {err:#}")),
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn notify_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.pending_errors.push(message);
        cx.notify();
    }

    // ----- rendering -------------------------------------------------------------

    fn render_toolbar(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let kind = da_core::FileKind::detect(&self.path)
            .map(|k| k.label())
            .unwrap_or("");
        let size = std::fs::metadata(&self.path)
            .map(|m| human_bytes(m.len()))
            .unwrap_or_default();
        let focus = self.focus_handle.clone();

        TitleBar::new()
            .h(theme::scaled(TOOLBAR_HEIGHT, cx))
            .children(crate::menu_bar(window, cx))
            .child(
                h_flex()
                    .w_full()
                    .h_full()
                    .pr(px(10.))
                    .pl(px(2.))
                    .gap(px(10.))
                    .items_center()
                    .child(
                        Icon::empty()
                            .path(crate::welcome::file_icon(&self.path))
                            .size(px(16.))
                            .text_color(cx.theme().primary),
                    )
                    .child(
                        h_flex()
                            .items_baseline()
                            .gap(px(8.))
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(self.name.clone()),
                            )
                            .child(div().text_xs().text_color(muted).whitespace_nowrap().child(
                                SharedString::from(if size.is_empty() {
                                    kind.to_string()
                                } else {
                                    format!("{kind} · {size}")
                                }),
                            )),
                    )
                    .child(div().flex_1())
                    .child(
                        div().w(px(240.)).child(
                            Input::new(&self.search).small().cleanable(true).prefix(
                                Icon::empty()
                                    .path("icons/search.svg")
                                    .size(px(13.))
                                    .text_color(muted),
                            ),
                        ),
                    )
                    .child(
                        Button::new("toggle-sql")
                            .ghost()
                            .small()
                            .icon(Icon::empty().path("icons/terminal.svg"))
                            .label("SQL")
                            .tooltip(keymap::tooltip("SQL drawer", "sql", cx))
                            .toggled(self.show_sql)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_sql(&ToggleSql, window, cx)
                            })),
                    )
                    .child(
                        Button::new("toggle-stats")
                            .ghost()
                            .small()
                            .icon(Icon::empty().path("icons/chart-column.svg"))
                            .label("Stats")
                            .tooltip(keymap::tooltip("Column stats", "stats", cx))
                            .toggled(self.show_stats)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_stats(&ToggleStats, window, cx)
                            })),
                    )
                    .child(
                        Button::new("toggle-chart")
                            .ghost()
                            .small()
                            .icon(Icon::empty().path("icons/chart-line.svg"))
                            .label("Chart")
                            .tooltip(keymap::tooltip("Chart", "chart", cx))
                            .toggled(self.show_chart)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_chart(&ToggleChart, window, cx)
                            })),
                    )
                    .child(
                        Button::new("export")
                            .ghost()
                            .small()
                            .icon(Icon::empty().path("icons/download.svg"))
                            .label("Export")
                            .dropdown_caret(true)
                            .dropdown_menu(move |menu, _, _| {
                                menu.action_context(focus.clone())
                                    .menu("CSV…", Box::new(ExportAs(ExportKind::Csv)))
                                    .menu("Parquet…", Box::new(ExportAs(ExportKind::Parquet)))
                                    .menu("JSON Lines…", Box::new(ExportAs(ExportKind::Json)))
                            }),
                    ),
            )
    }

    fn render_notice(&self, cx: &mut Context<Self>) -> Option<Div> {
        let palette = theme::palette(cx);
        let text = self.notice.clone()?;
        Some(
            h_flex()
                .px(px(12.))
                .py(px(5.))
                .gap(px(8.))
                .items_center()
                .bg(palette.warning.opacity(0.1))
                .border_b_1()
                .border_color(palette.warning.opacity(0.25))
                .text_xs()
                .text_color(cx.theme().foreground)
                .child(
                    Icon::empty()
                        .path("icons/info.svg")
                        .size(px(13.))
                        .text_color(palette.warning),
                )
                .child(SharedString::from(text)),
        )
    }

    /// Tabs for the file's parts, when it has more than one: each label with
    /// its row count, scrolling sideways when they don't fit.
    fn render_parts(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if self.parts.len() < 2 {
            return None;
        }
        let palette = theme::palette(cx);
        let muted = cx.theme().muted_foreground;
        Some(
            h_flex()
                .id("parts")
                .px(px(10.))
                .gap(px(2.))
                .flex_shrink_0()
                .items_end()
                .overflow_x_scroll()
                .bg(palette.chrome)
                .border_b_1()
                .border_color(cx.theme().border)
                .children(self.parts.iter().enumerate().map(|(ix, part)| {
                    let selected = self.part == Some(ix);
                    h_flex()
                        .id(("part", ix))
                        .flex_shrink_0()
                        .gap(px(6.))
                        .px(px(10.))
                        .pt(px(7.))
                        .pb(px(6.))
                        .text_xs()
                        .whitespace_nowrap()
                        .cursor_pointer()
                        .border_b_2()
                        .border_color(if selected {
                            palette.accent
                        } else {
                            transparent_black()
                        })
                        .text_color(if selected {
                            cx.theme().foreground
                        } else {
                            muted
                        })
                        .when(selected, |this| this.font_weight(FontWeight::MEDIUM))
                        .hover(|s| s.text_color(cx.theme().foreground))
                        .child(SharedString::from(part.label.clone()))
                        .when_some(part.rows, |this, rows| {
                            this.child(
                                div()
                                    .font_weight(FontWeight::NORMAL)
                                    .font_features(tabular())
                                    .text_color(muted.opacity(0.8))
                                    .child(SharedString::from(group_digits(rows))),
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.show_part(ix, cx);
                        }))
                })),
        )
    }

    /// The quiet line saying what the import did, with its Details, File
    /// notes, Show or Hide totals and Adjust import actions. Absent for a
    /// file read plainly.
    fn render_import_strip(&self, cx: &mut Context<Self>) -> Option<Div> {
        let dataset = self.dataset.as_ref()?;
        let conversions = dataset.conversions();
        let text = if self.adjusting {
            "Adjusting import. The grid updates when you apply.".to_string()
        } else {
            strip_text(
                dataset.summary().as_deref(),
                self.choices.adjusted(),
                &conversions,
            )?
        };
        let palette = theme::palette(cx);
        let link = |id: &'static str, label: &'static str, cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex_shrink_0()
                .text_color(palette.accent)
                .cursor_pointer()
                .hover(|s| s.text_color(cx.theme().foreground))
                .child(label)
        };
        let show_actions = !self.adjusting;
        // Totals set aside can be shown; totals kept by choice can be hidden.
        let totals = if dataset.notes().iter().any(|n| n.role == RowRole::Total) {
            Some(("show-totals", "Show totals", true))
        } else if dataset.plan().is_some_and(|p| p.keep_totals) {
            Some(("hide-totals", "Hide totals", false))
        } else {
            None
        };
        Some(
            h_flex()
                .px(px(12.))
                .py(px(5.))
                .gap(px(8.))
                .flex_shrink_0()
                .items_center()
                .bg(palette.accent.opacity(0.08))
                .border_b_1()
                .border_color(palette.accent.opacity(0.2))
                .text_xs()
                .text_color(cx.theme().foreground)
                .child(
                    Icon::empty()
                        .path("icons/table.svg")
                        .size(px(13.))
                        .text_color(palette.accent)
                        .flex_shrink_0(),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(text)),
                )
                .when(show_actions && !conversions.is_empty(), |this| {
                    this.child(link("details", "Details", cx).on_click(cx.listener(
                        |this, event: &ClickEvent, _, cx| {
                            this.details_at = match this.details_at {
                                Some(_) => None,
                                None => Some(event.position()),
                            };
                            this.notes_at = None;
                            cx.notify();
                        },
                    )))
                })
                .when(show_actions && !dataset.notes().is_empty(), |this| {
                    this.child(link("file-notes", "File notes", cx).on_click(cx.listener(
                        |this, event: &ClickEvent, _, cx| {
                            this.notes_at = match this.notes_at {
                                Some(_) => None,
                                None => Some(event.position()),
                            };
                            this.details_at = None;
                            cx.notify();
                        },
                    )))
                })
                .when_some(
                    totals.filter(|_| show_actions),
                    |this, (id, label, keep)| {
                        this.child(link(id, label, cx).on_click(
                            cx.listener(move |this, _, _, cx| this.keep_totals(keep, cx)),
                        ))
                    },
                )
                .when(show_actions && dataset.plan().is_some(), |this| {
                    this.child(
                        link("adjust-import", "Adjust import", cx).on_click(cx.listener(
                            |this, _, window, cx| this.adjust_import(&AdjustImport, window, cx),
                        )),
                    )
                }),
        )
    }

    /// The File notes popover: each set-aside line with its number and role.
    /// For JSON, the fields beside the records, each `key: value` alone.
    fn render_notes(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let position = self.notes_at?;
        let dataset = self.dataset.as_ref()?;
        let notes = dataset.notes();
        let muted = cx.theme().muted_foreground;
        let json = dataset.kind() == FileKind::Json;
        let heading = if json {
            "Fields set aside from the records"
        } else if dataset.delimiter().is_some() {
            "Lines set aside from the table"
        } else {
            "Rows set aside from the table"
        };
        let copy_text = notes
            .iter()
            .map(|n| {
                if json {
                    n.text.clone()
                } else {
                    format!("{}\t{}", n.row, n.text)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let rows = notes.iter().map(|note| {
            let role = match note.role {
                RowRole::Title => "TITLE",
                RowRole::Total => "TOTAL",
                _ => "NOTE",
            };
            h_flex()
                .gap(px(10.))
                .py(px(4.))
                .items_start()
                .when(!json, |this| {
                    this.child(
                        div()
                            .w(px(28.))
                            .flex_shrink_0()
                            .text_right()
                            .font_features(tabular())
                            .text_color(muted)
                            .child(SharedString::from(note.row.to_string())),
                    )
                    .child(
                        div()
                            .w(px(40.))
                            .flex_shrink_0()
                            .pt(px(1.))
                            .text_size(theme::scaled(10., cx))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(muted.opacity(0.8))
                            .child(role),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(SharedString::from(note.text.clone())),
                )
        });
        Some(
            deferred(
                anchored()
                    .position(point(position.x + px(10.), position.y + px(12.)))
                    .anchor(Anchor::TopRight)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        v_flex()
                            .id("file-notes-popover")
                            .occlude()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.notes_at = None;
                                cx.notify();
                            }))
                            .w(px(440.))
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .p(px(10.))
                            .popover_style(cx)
                            .text_xs()
                            .text_color(cx.theme().popover_foreground)
                            .child(
                                h_flex()
                                    .pb(px(6.))
                                    .mb(px(2.))
                                    .gap(px(10.))
                                    .items_center()
                                    .border_b_1()
                                    .border_color(cx.theme().border)
                                    .child(
                                        div()
                                            .flex_1()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("File notes"),
                                    )
                                    .child(div().text_color(muted).child(heading))
                                    .child(
                                        div()
                                            .id("copy-notes")
                                            .text_color(theme::palette(cx).accent)
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(cx.theme().foreground))
                                            .child("Copy")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    copy_text.clone(),
                                                ));
                                                this.notes_at = None;
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .children(rows),
                    ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// The Details popover, if it is open.
    fn render_details(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let position = self.details_at?;
        let conversions = self.dataset.as_ref()?.conversions();
        Some(crate::details::render(
            &conversions,
            &self.choices.types,
            position,
            cx,
        ))
    }

    /// Removable chips for the active sort keys, filters and query.
    fn render_chips(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.spec.sort.is_empty() && self.spec.filters.is_empty() && self.source == Source::File
        {
            return None;
        }
        let palette = theme::palette(cx);
        let chip = |id: ElementId, icon: &'static str, text: String, cx: &mut Context<Self>| {
            h_flex()
                .id(id)
                .h(px(22.))
                .pl(px(7.))
                .pr(px(4.))
                .gap(px(5.))
                .items_center()
                .rounded(px(6.))
                .bg(palette.accent.opacity(0.1))
                .border_1()
                .border_color(palette.accent.opacity(0.3))
                .text_xs()
                .text_color(cx.theme().foreground)
                .child(
                    Icon::empty()
                        .path(icon)
                        .size(px(11.))
                        .text_color(palette.accent),
                )
                .child(
                    div()
                        .max_w(px(260.))
                        .truncate()
                        .child(SharedString::from(text)),
                )
        };
        let remove = |cx: &mut Context<Self>| {
            div()
                .size(px(16.))
                .rounded(px(4.))
                .flex()
                .items_center()
                .justify_center()
                .text_color(cx.theme().muted_foreground)
                .hover(|s| {
                    s.bg(palette.accent.opacity(0.2))
                        .text_color(cx.theme().foreground)
                })
                .child(Icon::empty().path("icons/x.svg").size(px(10.)))
        };

        let mut chips: Vec<AnyElement> = Vec::new();
        if self.source == Source::Query {
            chips.push(
                chip(
                    "chip-query".into(),
                    "icons/terminal.svg",
                    "SQL result".into(),
                    cx,
                )
                .child(
                    remove(cx).id("remove-query").on_click(
                        cx.listener(|this, _, _, cx| this.show_file_data(&ShowFileData, cx)),
                    ),
                )
                .into_any_element(),
            );
        }
        for (ix, key) in self.spec.sort.iter().enumerate() {
            let text = format!("{} {}", key.column, if key.descending { "↓" } else { "↑" });
            chips.push(
                chip(
                    ("chip-sort", ix).into(),
                    "icons/arrow-up-down.svg",
                    text,
                    cx,
                )
                .child(
                    remove(cx)
                        .id(("remove-sort", ix))
                        .on_click(cx.listener(move |this, _, _, cx| this.remove_sort(ix, cx))),
                )
                .into_any_element(),
            );
        }
        for (ix, filter) in self.spec.filters.iter().enumerate() {
            let format = self
                .view
                .as_ref()
                .and_then(|f| f.columns.iter().find(|c| c.name == filter.column))
                .and_then(|c| c.format);
            let text = format!("{} {}", filter.column, describe_filter(&filter.op, format));
            chips.push(
                chip(("chip-filter", ix).into(), "icons/funnel.svg", text, cx)
                    .child(
                        remove(cx).id(("remove-filter", ix)).on_click(
                            cx.listener(move |this, _, _, cx| this.remove_filter(ix, cx)),
                        ),
                    )
                    .into_any_element(),
            );
        }

        Some(
            h_flex()
                .px(px(10.))
                .py(px(6.))
                .gap(px(6.))
                .flex_wrap()
                .items_center()
                .bg(palette.chrome)
                .border_b_1()
                .border_color(cx.theme().border)
                .children(chips)
                .child(
                    div()
                        .id("clear-all")
                        .ml(px(4.))
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .cursor_pointer()
                        .hover(|s| s.text_color(cx.theme().foreground))
                        .child("Clear all")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.show_file_data(&ShowFileData, cx);
                            this.clear_filters(&ClearFilters, window, cx);
                        })),
                ),
        )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> Div {
        let palette = theme::palette(cx);
        let muted = cx.theme().muted_foreground;
        let columns = self.view.as_ref().map_or(0, |f| f.columns.len() as u64);
        let base_rows = match self.source {
            Source::Query => self.query_frame.as_ref().and_then(|f| f.rows),
            Source::File => self.base.as_ref().and_then(|f| f.rows),
        };
        let view_rows = self.view.as_ref().and_then(|f| f.rows);
        let adjusting = self
            .adjusting
            .then(|| self.import.read(cx).status())
            .flatten();

        let rows_text = match (view_rows, base_rows) {
            _ if matches!(self.state, DocState::Opening) => "Opening…".to_string(),
            _ if adjusting.is_some() => adjusting.unwrap_or_default(),
            (Some(v), Some(b)) if v != b => format!(
                "{} of {} rows · {}",
                group_digits(v),
                group_digits(b),
                plural(columns, "column")
            ),
            (Some(v), _) => format!("{} · {}", plural(v, "row"), plural(columns, "column")),
            (None, _) => format!(
                "Previewing the first {} rows · {}",
                group_digits(PREVIEW_ROWS),
                plural(columns, "column")
            ),
        };

        let load: Option<AnyElement> = match self.load {
            Load::Done => None,
            Load::Loading(progress) => Some(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        div()
                            .w(px(120.))
                            .h(px(4.))
                            .rounded_full()
                            .bg(palette.chart_track)
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(progress.clamp(0.02, 1.0)))
                                    .rounded_full()
                                    .bg(palette.accent),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_features(tabular())
                            .text_color(muted)
                            .w(px(84.))
                            .child(SharedString::from(format!(
                                "Loading {}",
                                percent(progress as f64)
                            ))),
                    )
                    .child(
                        div()
                            .id("cancel-load")
                            .text_xs()
                            .text_color(cx.theme().link)
                            .cursor_pointer()
                            .hover(|s| s.text_color(cx.theme().link_hover))
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel_load(&CancelLoad, window, cx)
                            })),
                    )
                    .into_any_element(),
            ),
            Load::Cancelled => Some(
                h_flex()
                    .gap(px(8.))
                    .text_xs()
                    .text_color(muted)
                    .child("Load cancelled · showing the preview")
                    .child(
                        div()
                            .id("retry-load")
                            .text_color(cx.theme().link)
                            .cursor_pointer()
                            .hover(|s| s.text_color(cx.theme().link_hover))
                            .child("Load anyway")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let part = this.part;
                                this.open_part(part, cx);
                            })),
                    )
                    .into_any_element(),
            ),
        };

        let busy = (self.busy != Busy::Idle).then(|| {
            h_flex()
                .gap(px(6.))
                .items_center()
                .text_xs()
                .text_color(muted)
                .child(Spinner::new().xsmall())
                .child(self.busy.label())
        });

        let selection = match self.table.read(cx).selection() {
            TableSelection::Cell(row, col) if col > 0 => self
                .column_name(col - 1)
                .map(|name| format!("Row {} · {name}", group_digits(row as u64 + 1))),
            TableSelection::Row(row) => Some(format!("Row {}", group_digits(row as u64 + 1))),
            _ => None,
        };

        h_flex()
            .h(px(26.))
            .px(px(12.))
            .gap(px(16.))
            .flex_shrink_0()
            .items_center()
            .bg(palette.chrome)
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .font_features(tabular())
            .text_color(muted)
            .map(|this| {
                if self.goto_open {
                    this.child(
                        h_flex()
                            .gap(px(6.))
                            .items_center()
                            .child("Go to row")
                            .child(div().w(px(120.)).child(Input::new(&self.goto).xsmall())),
                    )
                } else {
                    this.child(
                        div()
                            .id("rows-text")
                            .whitespace_nowrap()
                            .cursor_pointer()
                            .hover(|s| s.text_color(cx.theme().foreground))
                            .child(SharedString::from(rows_text))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.go_to_row(&GoToRow, window, cx)
                            })),
                    )
                }
            })
            .children(load)
            .child(div().flex_1())
            .children(busy)
            .when_some(selection, |this, text| {
                this.child(div().whitespace_nowrap().child(SharedString::from(text)))
            })
    }

    fn render_failed(&self, message: &str, cx: &mut Context<Self>) -> Div {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .px(px(40.))
            .child(
                Icon::empty()
                    .path("icons/file-x.svg")
                    .size(px(36.))
                    .text_color(cx.theme().muted_foreground.opacity(0.6)),
            )
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(format!("Couldn't open {}", self.name))),
            )
            .child(
                div()
                    .max_w(px(520.))
                    .text_sm()
                    .text_center()
                    .text_color(cx.theme().muted_foreground)
                    // DuckDB follows its reason with pages of reader settings.
                    .child(SharedString::from(
                        message.split("\n\n").next().unwrap_or(message).to_string(),
                    )),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    // A choice made in Adjust import may be why it failed.
                    .when(self.choices.adjusted(), |this| {
                        this.child(
                            Button::new("reset-import")
                                .small()
                                .label("Reset Import")
                                .on_click(cx.listener(|this, _, _, cx| this.reset_import(cx))),
                        )
                    })
                    .child(
                        Button::new("open-another")
                            .small()
                            .label("Open Another File…")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(crate::Open), cx);
                            }),
                    ),
            )
    }

    fn render_header_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let position = self.header_menu?;
        if let Some(menu) = self.pending_menu.take() {
            self.filter_editor
                .update(cx, |editor, cx| editor.open(menu, window, cx));
        }
        Some(
            deferred(
                anchored()
                    .position(point(position.x + px(10.), position.y + px(12.)))
                    .anchor(Anchor::TopRight)
                    .snap_to_window_with_margin(px(8.))
                    .child(self.filter_editor.clone()),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }
}

/// Hooks for the screenshot harness (`crate::shots`).
#[cfg(all(test, target_os = "macos"))]
impl Document {
    /// Selects the column called `name`, as clicking one of its cells does.
    pub(crate) fn select_column_named(&mut self, name: &str, cx: &mut Context<Self>) {
        let col = self
            .view
            .as_ref()
            .and_then(|f| f.columns.iter().position(|c| c.name == name))
            .unwrap_or_else(|| panic!("{} has no column named {name:?}", self.name));
        self.select_column(col, cx);
    }

    /// What the document is still waiting for, or `None` once everything it
    /// shows has landed: the file, the full load, the profile, the visible
    /// rows and the stats panel's chart.
    pub(crate) fn pending_work(&self, cx: &App) -> Option<&'static str> {
        match self.state {
            DocState::Opening => return Some("opening the file"),
            DocState::Failed(_) => return None,
            DocState::Ready => {}
        }
        if matches!(self.load, Load::Loading(_)) {
            return Some("loading the whole file");
        }
        if self.busy != Busy::Idle {
            return Some(self.busy.label());
        }
        let loaded = self.dataset.as_ref().is_some_and(|d| d.is_loaded());
        if loaded && self.profile.as_ref().map(|p| p.frame_id) != self.view.as_ref().map(|f| f.id) {
            return Some("profiling the columns");
        }
        if !self.table.read(cx).delegate().visible_rows_loaded() {
            return Some("fetching the visible rows");
        }
        if self.show_stats && self.stats.read(cx).is_loading() {
            return Some("computing the stats panel");
        }
        if self.show_chart && self.chart.read(cx).is_loading() {
            return Some("drawing the chart");
        }
        if self.adjusting && self.import.read(cx).is_loading() {
            return Some("previewing the import");
        }
        if self.header_menu.is_some() && self.filter_editor.read(cx).is_counting() {
            return Some("counting values that fit");
        }
        None
    }

    /// Opens the column menu of the column called `name`, as its header's
    /// menu button does.
    pub(crate) fn open_menu_named(&mut self, name: &str, cx: &mut Context<Self>) {
        let col = self
            .view
            .as_ref()
            .and_then(|f| f.columns.iter().position(|c| c.name == name))
            .unwrap_or_else(|| panic!("{} has no column named {name:?}", self.name));
        self.open_header_menu(
            col,
            point(px(360.), theme::scaled(TOOLBAR_HEIGHT + 64., cx)),
            cx,
        );
    }

    /// The chart panel, for steps that set its fields as a user would.
    pub(crate) fn chart_panel(&self) -> Entity<ChartPanel> {
        self.chart.clone()
    }

    /// Runs `sql` from the SQL drawer, as Run does.
    pub(crate) fn run_sql(&mut self, sql: &str, cx: &mut Context<Self>) {
        self.run_query(sql.to_string(), cx);
    }

    /// Searches the grid, as typing in the search field does.
    pub(crate) fn search_for(&mut self, text: &str, cx: &mut Context<Self>) {
        self.spec.search = text.to_string();
        self.apply(Busy::Searching, cx);
    }

    /// Adds a filter, as a column menu or a click on a chart bar does.
    pub(crate) fn filter_by(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.add_filter(filter, cx);
    }

    /// The column menu, for steps that use it as a user would.
    pub(crate) fn filter_editor(&self) -> Entity<FilterEditor> {
        self.filter_editor.clone()
    }

    /// Opens the Details popover, as clicking "Details" does, anchored
    /// below the strip's right end.
    pub(crate) fn show_details(&mut self, window: &Window, cx: &mut Context<Self>) {
        let right = window.viewport_size().width - px(12.);
        self.details_at = Some(point(right, theme::scaled(TOOLBAR_HEIGHT + 20., cx)));
        cx.notify();
    }

    /// The Adjust import panel, for steps that edit it as a user would.
    pub(crate) fn import_panel(&self) -> Entity<ImportPanel> {
        self.import.clone()
    }

    /// Opens the File notes popover, as clicking "File notes" does, anchored
    /// below the strip's right end.
    pub(crate) fn show_notes(&mut self, window: &Window, cx: &mut Context<Self>) {
        let right = window.viewport_size().width - px(12.);
        self.notes_at = Some(point(right, theme::scaled(TOOLBAR_HEIGHT + 20., cx)));
        cx.notify();
    }
}

/// What the import strip says: the engine's summary, led by "Adjusted
/// import." when the user changed how the file is read, and followed by the
/// columns whose day/month order or decimal mark is a guess. When the
/// summary has nothing to say but columns still show differently (spaces
/// trimmed, say), it says so, so Details can be reached. `None` hides it.
fn strip_text(summary: Option<&str>, adjusted: bool, conversions: &[Conversion]) -> Option<String> {
    let guessed = |what: Guessed| {
        conversions
            .iter()
            .filter(|c| c.guessed == Some(what))
            .map(|c| c.column.as_str())
            .collect::<Vec<_>>()
    };
    let mut sentences: Vec<String> = Vec::new();
    if adjusted {
        sentences.push("Adjusted import.".into());
    }
    match summary {
        Some(summary) => sentences.push(summary.into()),
        None if !conversions.is_empty() => sentences.push(format!(
            "Tidied the values of {}.",
            plural(conversions.len() as u64, "column")
        )),
        None => {}
    }
    for (what, sentence) in [
        (Guessed::DateOrder, "Month or day first in"),
        (Guessed::DecimalMark, "The decimal mark in"),
    ] {
        let columns = guessed(what);
        if !columns.is_empty() {
            sentences.push(format!("{sentence} {} is a guess.", columns.join(", ")));
        }
    }
    (!sentences.is_empty()).then(|| sentences.join(" "))
}

/// Like [`FilterOp::describe`], with numbers shown as their column shows
/// them (`format`), or with long decimals trimmed.
fn describe_filter(op: &FilterOp, format: Option<NumberFormat>) -> String {
    let show = |v: &str| formatted(format, v).unwrap_or_else(|| tidy_number(v));
    match op {
        FilterOp::Equals(v) => format!("= {}", show(v)),
        FilterOp::NotEquals(v) => format!("≠ {}", show(v)),
        FilterOp::GreaterThan(v) => format!("> {}", show(v)),
        FilterOp::LessThan(v) => format!("< {}", show(v)),
        FilterOp::Between(a, b) => format!("{} – {}", show(a), show(b)),
        other => other.describe(),
    }
}

/// `2_400_000_000` → `2.4 GB`.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        if let Some(dataset) = &self.dataset {
            dataset.cancel_load();
            dataset.cancel_query();
        }
    }
}

impl Focusable for Document {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Document {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Errors raised from background work are shown as toasts here, where
        // the window is available.
        if std::mem::take(&mut self.clear_search_input) {
            self.search.update(cx, |s, cx| s.set_value("", window, cx));
        }
        for message in std::mem::take(&mut self.pending_errors) {
            window.push_notification(Notification::error(message), cx);
        }

        let body: AnyElement = match self.state.clone() {
            DocState::Failed(message) => self.render_failed(&message, cx).into_any_element(),
            DocState::Opening => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(Spinner::new().small())
                .child("Opening…")
                .into_any_element(),
            DocState::Ready if self.adjusting => self.import.clone().into_any_element(),
            DocState::Ready => h_flex()
                .flex_1()
                .min_h_0()
                .items_stretch()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .when(self.show_chart, |this| {
                            this.child(
                                div()
                                    .h(relative(CHART_SHARE))
                                    .flex_shrink_0()
                                    .child(self.chart.clone()),
                            )
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .text_size(theme::scaled(13., cx))
                                .child(
                                    DataTable::new(&self.table)
                                        .focus_ring(false)
                                        .bordered(false)
                                        .with_size(Size::Size(theme::scaled(ROW_HEIGHT, cx)))
                                        .header_row_height(theme::scaled(HEADER_HEIGHT, cx)),
                                ),
                        )
                        .when(self.show_sql, |this| {
                            this.child(
                                div()
                                    .h(theme::scaled(SQL_HEIGHT, cx))
                                    .flex_shrink_0()
                                    .child(self.sql.clone()),
                            )
                        }),
                )
                .when(self.show_stats, |this| {
                    this.child(
                        div()
                            .w(theme::scaled(STATS_WIDTH, cx))
                            .flex_shrink_0()
                            .child(self.stats.clone()),
                    )
                })
                .into_any_element(),
        };

        v_flex()
            .id("document")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_stats))
            .on_action(cx.listener(Self::toggle_chart))
            .on_action(cx.listener(Self::toggle_sql))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::copy_cell))
            .on_action(cx.listener(Self::copy_row))
            .on_action(cx.listener(Self::export))
            .on_action(cx.listener(Self::export_as))
            .on_action(cx.listener(Self::clear_filters))
            .on_action(cx.listener(Self::cancel_load))
            .on_action(cx.listener(Self::go_to_row))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::adjust_import))
            .on_action(
                cx.listener(|this, action: &ShowFileData, _, cx| this.show_file_data(action, cx)),
            )
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_toolbar(window, cx))
            .children(self.render_notice(cx))
            .children(self.render_parts(cx))
            .children(self.render_import_strip(cx))
            .children(self.render_chips(cx))
            .child(body)
            .child(self.render_status(cx))
            .children(self.render_header_menu(window, cx))
            .children(self.render_notes(cx))
            .children(self.render_details(cx))
    }
}

#[cfg(test)]
mod tests {
    use da_core::{
        Conversion, Decimal, Delimiter, Encoding, FilterOp, Guessed, ImportPlan, NumberFormat,
        ReadAs, Rows,
    };

    use super::{Choices, Reading, describe_filter, strip_text};

    #[test]
    fn the_strip_says_when_the_import_was_adjusted() {
        assert_eq!(strip_text(None, false, &[]), None);
        assert_eq!(
            strip_text(Some("Table found."), false, &[]).as_deref(),
            Some("Table found.")
        );
        assert_eq!(
            strip_text(Some("5 total rows kept in the data."), true, &[]).as_deref(),
            Some("Adjusted import. 5 total rows kept in the data.")
        );
        assert_eq!(
            strip_text(None, true, &[]).as_deref(),
            Some("Adjusted import.")
        );
    }

    #[test]
    fn the_strip_points_at_details() {
        let conversion = |column: &str, read_as, guessed| Conversion {
            column: column.into(),
            read_as,
            notes: vec!["any wording".into()],
            guessed,
        };
        // Only spaces trimmed: the summary is quiet, but Details has a row.
        let trimmed = [conversion("name", ReadAs::Text, None)];
        assert_eq!(
            strip_text(None, false, &trimmed).as_deref(),
            Some("Tidied the values of 1 column.")
        );
        // What was guessed, whatever the notes say.
        let guessed = [
            conversion("placed", ReadAs::MonthFirstDate, Some(Guessed::DateOrder)),
            conversion("weight", ReadAs::Number, Some(Guessed::DecimalMark)),
        ];
        assert_eq!(
            strip_text(Some("Read 1 text column as dates."), false, &guessed).as_deref(),
            Some(
                "Read 1 text column as dates. Month or day first in placed is a guess. \
                 The decimal mark in weight is a guess."
            )
        );
    }

    #[test]
    fn reopening_keeps_the_choices_that_still_apply() {
        let plan = ImportPlan {
            header: Some(Rows { first: 2, last: 2 }),
            data: Rows { first: 3, last: 9 },
            keep_totals: false,
        };
        let chosen = Choices {
            plan: Some(plan),
            reading: Reading {
                delimiter: Delimiter::Semicolon,
                encoding: Encoding::Windows1252,
                decimal: Decimal::Comma,
            },
            types: [("zip".to_string(), ReadAs::Number)].into(),
        };
        let options = chosen.options(Some(1));
        assert_eq!(options.part, Some(1));
        assert_eq!(options.plan, Some(plan));
        assert_eq!(options.encoding, Encoding::Windows1252);
        assert_eq!(options.values.decimal, Decimal::Comma);
        assert_eq!(options.values.types.get("zip"), Some(&ReadAs::Number));

        // Another tab: the table's own choices go, the file's stay.
        let mut tab = chosen.clone();
        tab.switch_part();
        assert_eq!(tab.plan, None);
        assert!(tab.types.is_empty());
        assert_eq!(tab.reading, chosen.reading);
        assert!(tab.adjusted());

        // A new encoding finds the tables anew.
        let mut redetected = chosen.clone();
        let reading = Reading {
            encoding: Encoding::Utf8,
            ..chosen.reading
        };
        redetected.redetect(reading);
        assert_eq!(redetected.plan, None);
        assert!(redetected.types.is_empty());
        assert_eq!(redetected.reading.decimal, Decimal::Comma);

        // Reset to detected is the default: nothing chosen, nothing adjusted.
        assert!(!Choices::default().adjusted());
        assert_eq!(Choices::default().options(None).values, Default::default());
    }

    #[test]
    fn chips_show_numbers_as_their_column_does() {
        let percent = Some(NumberFormat::Percent { decimals: 1 });
        let between = FilterOp::Between("0.1".into(), "0.25".into());
        assert_eq!(describe_filter(&between, percent), "10.0% – 25.0%");
        assert_eq!(describe_filter(&between, None), "0.1 – 0.25");
        let dollars = Some(NumberFormat::Currency {
            symbol: '$',
            decimals: 0,
        });
        assert_eq!(
            describe_filter(&FilterOp::GreaterThan("1000".into()), dollars),
            "> $1,000"
        );
        assert_eq!(
            describe_filter(&FilterOp::Contains("abc".into()), dollars),
            "contains “abc”"
        );
    }
}
