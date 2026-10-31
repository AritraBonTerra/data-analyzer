//! One open file: owns the dataset and the frames derived from it, and
//! coordinates the grid, search, filters, stats panel and SQL drawer. Every
//! dataset call runs on the background executor; results are matched against
//! generation counters so stale work is dropped.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use da_core::{
    Cancelled, Column, ColumnSummary, Dataset, Distribution, ExportFormat, Filter, FilterOp, Frame,
    OpenOptions, SortKey, SqlOutcome, ViewSpec, tidy_number,
};
use futures::StreamExt;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::DropdownMenu;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{DataTable, TableEvent, TableSelection, TableState};
use gpui_kit::component::{
    ActiveTheme, FocusableExt, Icon, Sizable, Size, TitleBar, WindowExt, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde::Deserialize;

use crate::filter::{FilterEditor, FilterEvent};
use crate::format::{group_digits, percent, plural};
use crate::grid::{GridDelegate, GridEvent, PREVIEW_ROWS};
use crate::platform;
use crate::sql::{QueryStatus, SqlEvent, SqlPanel};
use crate::stats::{StatsEvent, StatsPanel};
use crate::theme::{self, HEADER_HEIGHT, ROW_HEIGHT, STATS_WIDTH, TOOLBAR_HEIGHT, tabular};

actions!(
    document,
    [
        ToggleStats,
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
        CloseGoToRow,
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

const CONTEXT: &str = "Document";
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(220);
const SQL_HEIGHT: f32 = 210.0;

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-i", ToggleStats, Some(CONTEXT)),
        KeyBinding::new("cmd-e", ToggleSql, Some(CONTEXT)),
        KeyBinding::new("cmd-f", Find, Some(CONTEXT)),
        KeyBinding::new("cmd-c", Copy, Some("DataTable")),
        KeyBinding::new("cmd-shift-e", Export, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-k", ClearFilters, Some(CONTEXT)),
        KeyBinding::new("cmd-g", GoToRow, Some(CONTEXT)),
        KeyBinding::new("escape", CloseGoToRow, Some(CONTEXT)),
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
    /// The last SQL result, when the grid is showing one.
    query_frame: Option<Frame>,
    /// The frame on screen: the base, a sorted/filtered copy, or a query result.
    view: Option<Frame>,
    source: Source,
    spec: ViewSpec,
    load: Load,
    notice: Option<String>,
    sheets: Vec<String>,
    sheet: usize,
    busy: Busy,
    apply_generation: u64,
    open_generation: u64,
    profile: Option<Profile>,
    selected_column: Option<usize>,
    /// Where the open column menu is anchored, if one is open.
    header_menu: Option<Point<Pixels>>,
    /// A column menu to initialise on the next render (it needs the window).
    pending_menu: Option<(usize, Column, Option<Filter>, Option<Distribution>)>,
    /// Errors from background work, shown as toasts on the next render.
    pending_errors: Vec<String>,
    show_stats: bool,
    show_sql: bool,

    table: Entity<TableState<GridDelegate>>,
    search: Entity<InputState>,
    search_task: Task<()>,
    /// The "Go to row" field in the status bar.
    goto: Entity<InputState>,
    goto_open: bool,
    filter_editor: Entity<FilterEditor>,
    stats: Entity<StatsPanel>,
    sql: Entity<SqlPanel>,
    _subscriptions: Vec<Subscription>,
}

impl Document {
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
        let sql = cx.new(|cx| SqlPanel::new(window, cx));

        let subscriptions = vec![
            cx.subscribe(&table, Self::on_grid_event),
            cx.subscribe(&table, Self::on_table_event),
            cx.subscribe(&search, Self::on_search_event),
            cx.subscribe(&goto, Self::on_goto_event),
            cx.subscribe_in(&filter_editor, window, Self::on_filter_event),
            cx.subscribe(&stats, Self::on_stats_event),
            cx.subscribe_in(&sql, window, Self::on_sql_event),
        ];

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            path,
            name,
            state: DocState::Opening,
            dataset: None,
            base: None,
            query_frame: None,
            view: None,
            source: Source::File,
            spec: ViewSpec::default(),
            load: Load::Done,
            notice: None,
            sheets: Vec::new(),
            sheet: 0,
            busy: Busy::Idle,
            apply_generation: 0,
            open_generation: 0,
            profile: None,
            selected_column: None,
            header_menu: None,
            pending_menu: None,
            pending_errors: Vec::new(),
            show_stats: false,
            show_sql: false,
            table,
            search,
            search_task: Task::ready(()),
            goto,
            goto_open: false,
            filter_editor,
            stats,
            sql,
            _subscriptions: subscriptions,
        };
        this.open_sheet(0, cx);
        this
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    // ----- opening and loading -------------------------------------------------

    /// Opens (or re-opens, for another sheet) the file in the background.
    fn open_sheet(&mut self, sheet: usize, cx: &mut Context<Self>) {
        if let Some(old) = self.dataset.take() {
            old.cancel_load();
        }
        self.state = DocState::Opening;
        self.base = None;
        self.view = None;
        self.query_frame = None;
        self.source = Source::File;
        self.spec = ViewSpec::default();
        self.profile = None;
        self.load = Load::Done;
        self.selected_column = None;
        self.sheet = sheet;
        self.busy = Busy::Idle;
        // Results from the old dataset's filters or queries must not land here.
        self.apply_generation += 1;
        self.open_generation += 1;
        let generation = self.open_generation;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear();
            table.refresh(cx);
        });
        self.stats.update(cx, |stats, cx| stats.clear(cx));

        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let opened = cx
                .background_spawn(async move {
                    let options = OpenOptions {
                        cache_dir: platform::cache_dir(),
                        sheet,
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
                        this.sheets = dataset.sheets().to_vec();
                        this.sheet = dataset.sheet();
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
                        this.base = Some(frame.clone());
                        // A SQL result on screen stays; "Back to file" shows the loaded file.
                        if this.source == Source::File {
                            if this.spec.is_empty() {
                                this.show_frame(frame, cx);
                                this.profile_view(cx);
                            } else {
                                this.apply(Busy::Filtering, cx);
                            }
                        }
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
        } else if self.sheets.len() > 1 {
            "This sheet is empty."
        } else {
            "This file has no rows."
        };
        self.table.update(cx, |table, cx| {
            let widths: Vec<f32> = (0..frame.columns.len())
                .filter_map(|ix| table.col_width(ix + 1).map(f32::from))
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
                    Err(err) if err.is::<Cancelled>() => {}
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

    fn close_go_to_row(&mut self, _: &CloseGoToRow, window: &mut Window, cx: &mut Context<Self>) {
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
        self.header_menu = Some(position);
        // The editor needs the window to focus its input, so it's set up in render.
        self.pending_menu = Some((col, column, existing, sketch));
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
        self.apply_generation += 1;
        let generation = self.apply_generation;
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move { dataset.query(&sql) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.apply_generation != generation {
                    return;
                }
                this.busy = Busy::Idle;
                let status = match outcome {
                    Ok(SqlOutcome::Rows(frame)) => {
                        let rows = frame.rows.unwrap_or(0);
                        this.source = Source::Query;
                        this.spec = ViewSpec::default();
                        this.query_frame = Some(frame.clone());
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
        self.spec = ViewSpec::default();
        self.selected_column = None;
        self.busy = Busy::Idle;
        self.apply_generation += 1;
        if let Some(base) = self.base.clone() {
            self.show_frame(base, cx);
            self.profile_view(cx);
        }
        self.sql
            .update(cx, |panel, cx| panel.set_status(QueryStatus::Idle, cx));
        cx.notify();
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
                Some("csv" | "tsv" | "txt") => ExportFormat::Csv,
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

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let kind = da_core::FileKind::detect(&self.path)
            .map(|k| k.label())
            .unwrap_or("");
        let size = std::fs::metadata(&self.path)
            .map(|m| human_bytes(m.len()))
            .unwrap_or_default();
        let focus = self.focus_handle.clone();

        TitleBar::new().h(px(TOOLBAR_HEIGHT)).child(
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
                        .tooltip("SQL drawer (⌘E)")
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
                        .tooltip("Column stats (⌘I)")
                        .toggled(self.show_stats)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_stats(&ToggleStats, window, cx)
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

    fn render_sheets(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.sheets.len() < 2 {
            return None;
        }
        let palette = theme::palette(cx);
        Some(
            h_flex()
                .px(px(10.))
                .gap(px(2.))
                .items_end()
                .bg(palette.chrome)
                .border_b_1()
                .border_color(cx.theme().border)
                .children(self.sheets.iter().enumerate().map(|(ix, name)| {
                    let selected = ix == self.sheet;
                    div()
                        .id(("sheet", ix))
                        .px(px(10.))
                        .pt(px(7.))
                        .pb(px(6.))
                        .text_xs()
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
                            cx.theme().muted_foreground
                        })
                        .when(selected, |this| this.font_weight(FontWeight::MEDIUM))
                        .hover(|s| s.text_color(cx.theme().foreground))
                        .child(SharedString::from(name.clone()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.sheet != ix {
                                this.open_sheet(ix, cx);
                            }
                        }))
                })),
        )
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
            let text = format!("{} {}", filter.column, describe_filter(&filter.op));
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

        let rows_text = match (view_rows, base_rows) {
            _ if matches!(self.state, DocState::Opening) => "Opening…".to_string(),
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
                                let sheet = this.sheet;
                                this.open_sheet(sheet, cx);
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
                    .child(SharedString::from(message.to_string())),
            )
            .child(
                Button::new("open-another")
                    .small()
                    .label("Open Another File…")
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(crate::Open), cx);
                    }),
            )
    }

    fn render_header_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let position = self.header_menu?;
        if let Some((col, column, existing, sketch)) = self.pending_menu.take() {
            self.filter_editor.update(cx, |editor, cx| {
                editor.open(col, column, existing.as_ref(), sketch.as_ref(), window, cx)
            });
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

/// Like [`FilterOp::describe`], with long decimals trimmed for the chip.
fn describe_filter(op: &FilterOp) -> String {
    match op {
        FilterOp::Equals(v) => format!("= {}", tidy_number(v)),
        FilterOp::NotEquals(v) => format!("≠ {}", tidy_number(v)),
        FilterOp::GreaterThan(v) => format!("> {}", tidy_number(v)),
        FilterOp::LessThan(v) => format!("< {}", tidy_number(v)),
        FilterOp::Between(a, b) => format!("{} – {}", tidy_number(a), tidy_number(b)),
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
            DocState::Ready => h_flex()
                .flex_1()
                .min_h_0()
                .items_stretch()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .child(
                            div().flex_1().min_h_0().text_size(px(13.)).child(
                                DataTable::new(&self.table)
                                    .focus_ring(false)
                                    .bordered(false)
                                    .with_size(Size::Size(px(ROW_HEIGHT)))
                                    .header_row_height(px(HEADER_HEIGHT)),
                            ),
                        )
                        .when(self.show_sql, |this| {
                            this.child(
                                div()
                                    .h(px(SQL_HEIGHT))
                                    .flex_shrink_0()
                                    .child(self.sql.clone()),
                            )
                        }),
                )
                .when(self.show_stats, |this| {
                    this.child(
                        div()
                            .w(px(STATS_WIDTH))
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
            .on_action(cx.listener(Self::close_go_to_row))
            .on_action(
                cx.listener(|this, action: &ShowFileData, _, cx| this.show_file_data(action, cx)),
            )
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_toolbar(cx))
            .children(self.render_notice(cx))
            .children(self.render_sheets(cx))
            .children(self.render_chips(cx))
            .child(body)
            .child(self.render_status(cx))
            .children(self.render_header_menu(window, cx))
    }
}
