//! Offscreen screenshots of the real app, for checking UI work by eye.
//!
//! Each [`Scenario`] opens a file in the app's own `Workspace` view, with its
//! theme, fonts and icons, in a 1180×760 window (or narrower) that Metal renders at 2× and
//! never puts on screen. A [`Scenario::settings`] one opens the Settings window instead. It runs the scenario's steps, drives every
//! background job (open, load, profile, page fetches, stats) to completion,
//! and writes `<name>.png`.
//!
//! Run every scenario, or one by name:
//!
//! ```sh
//! cargo test -p data-analyzer shots -- --ignored --nocapture
//! DA_SHOT=clean-sales-stats cargo test -p data-analyzer shots -- --ignored --nocapture
//! ```
//!
//! PNGs go to `$DA_SHOTS_DIR`, or `target/shots` in the repo. A relative
//! `DA_SHOTS_DIR` is taken from the repo root.
//!
//! To add a scenario, add a line to [`scenarios`]: a name, a file (relative to
//! the repo root), then any of `.light()`, `.width(points)`, `.select(column)`,
//! `.theme(name)`, `.action(A)`, `.wait()` (lets an animation finish) and
//! `.with(|doc, window, cx| ...)` for anything the others don't cover.
//! Settings scenarios take `.settings_with(|view, window, cx| ...)`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::{Theme, ThemeMode, TitleBar};
use gpui_kit::{
    Action, AnyWindowHandle, App, AppContext, Bounds, Context, Entity, HeadlessAppContext,
    Keystroke, Window, WindowBounds, WindowOptions, point, px, size,
};

use da_core::{Aggregate, ChartKind, ChartSpec, Filter, FilterOp, ReadAs};

use crate::document::{AdjustImport, Document, ToggleChart, ToggleStats};
use crate::recent::RecentStore;
use crate::settings::{self, Appearance, SettingsStore};
use crate::settings_window::{Page, SettingsView};
use crate::workspace::Workspace;
use crate::{Windows, document, filter, import};

const MESSY: &str = "crates/core/tests/fixtures/messy";

/// The scenarios, in the order they run. Add yours here.
fn scenarios() -> Vec<Scenario> {
    let sales = format!("{MESSY}/00_clean_sales.csv");
    let report = format!("{MESSY}/01_report_preamble.csv");
    let report_xlsx = format!("{MESSY}/20_report_title.xlsx");
    let multirow = format!("{MESSY}/06_multirow_header.csv");
    let subtotals = format!("{MESSY}/12_subtotals.csv");
    let currency = format!("{MESSY}/03_currency_percent.csv");
    let ids = format!("{MESSY}/10_ids_dates_flags.csv");
    let latin1 = format!("{MESSY}/08_latin1.csv");
    let guessed = "crates/core/tests/fixtures/ambiguous_dates.csv";
    let guessed_times = "crates/core/tests/fixtures/ambiguous_datetimes.csv";
    let wide = format!("{MESSY}/07_wide_months.csv");
    let envelope = format!("{MESSY}/30_api_envelope.json");
    let months = [
        "Jan 2026", "Feb 2026", "Mar 2026", "Apr 2026", "May 2026", "Jun 2026",
    ];
    // Thirty categories, so the bars stop at twenty and the rest are Other.
    let many = generated(
        "thirty-categories.csv",
        std::iter::once("team,points".to_string())
            .chain((1..=30).map(|i| format!("Team {i:02},{}", 100 + i * 37 % 300)))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let words = generated(
        "text-only.csv",
        "name,city\nAda,London\nGrace,New York\nEdsger,Austin\nBarbara,Boston".into(),
    );
    vec![
        Scenario::new("clean-sales", &sales),
        Scenario::new("clean-sales-stats", &sales)
            .select("revenue")
            .action(ToggleStats),
        Scenario::new("clean-sales-light", &sales).light(),
        // Zoom In / Zoom Out: text and the grid's rows grow together.
        Scenario::new("zoom-85", &sales).with(zoom(0.85)),
        Scenario::new("zoom-150-stats", &sales)
            .with(zoom(1.5))
            .select("revenue")
            .action(ToggleStats),
        Scenario::new("zoom-150-chart-light", &sales)
            .light()
            .with(zoom(1.5))
            .action(ToggleChart),
        // Each theme, light and dark, with a sketch row, stats and a chart
        // to judge its accent by.
        themed("theme-paper-light", "Paper", &sales).light(),
        themed("theme-paper-dark", "Paper", &sales),
        themed("theme-slate-light", "Slate", &sales).light(),
        themed("theme-slate-dark", "Slate", &sales),
        themed("theme-contrast-light", "High Contrast", &sales).light(),
        themed("theme-contrast-dark", "High Contrast", &sales),
        // The Settings window.
        Scenario::settings("settings-appearance").light(),
        Scenario::settings("settings-appearance-paper-dark")
            .theme("Paper")
            .settings_with(|_, _, cx| SettingsStore::edit(cx, |s| s.zoom = 1.25)),
        Scenario::settings("settings-shortcuts").settings_with(shortcuts_page),
        // Recording Find's shortcut, after a key that can't be one.
        Scenario::settings("settings-shortcuts-recording")
            .light()
            .theme("Slate")
            .settings_with(shortcuts_page)
            .settings_with(|view, window, cx| view.start_recording("find", window, cx))
            .settings_with(press("k")),
        // Find takes Export's shortcut.
        Scenario::settings("settings-shortcuts-conflict")
            .settings_with(shortcuts_page)
            .settings_with(|view, window, cx| view.start_recording("find", window, cx))
            .settings_with(press("cmd-shift-e")),
        Scenario::new("report-preamble", &report),
        Scenario::new("report-preamble-light", &report).light(),
        Scenario::new("report-notes", &report).with(|doc, window, cx| doc.show_notes(window, cx)),
        Scenario::new("report-notes-light", &report)
            .light()
            .with(|doc, window, cx| doc.show_notes(window, cx)),
        Scenario::new("report-xlsx", &report_xlsx),
        Scenario::new(
            "side-by-side-parts",
            format!("{MESSY}/22_side_by_side_tables.xlsx"),
        ),
        Scenario::new(
            "cover-sheet-first",
            format!("{MESSY}/24_cover_sheet_first.xlsx"),
        ),
        Scenario::new("merged-headers", format!("{MESSY}/21_merged_headers.xlsx")),
        Scenario::new("multirow-header", &multirow),
        Scenario::new("multirow-header-light", &multirow).light(),
        Scenario::new("subtotals", &subtotals),
        // File notes list every totals row the strip counts.
        Scenario::new("subtotals-notes", &subtotals)
            .with(|doc, window, cx| doc.show_notes(window, cx)),
        Scenario::new("adjust-import-csv", &report).action(AdjustImport),
        Scenario::new("adjust-import-light", &report)
            .light()
            .action(AdjustImport),
        // Header on line 2, as a click on it does: line 1 turns title, 3 to 5 data.
        Scenario::new("adjust-import-edited", &report)
            .action(AdjustImport)
            .with(|doc, window, cx| {
                doc.import_panel()
                    .update(cx, |panel, cx| panel.pick_header(2, window, cx))
            }),
        // Header on the last line: the data would end before it starts.
        Scenario::new("adjust-import-invalid", &report)
            .action(AdjustImport)
            .with(|doc, window, cx| {
                doc.import_panel()
                    .update(cx, |panel, cx| panel.pick_header(48, window, cx))
            }),
        // From the first table's tab, a click on the second table's header
        // (line 7, below the data): the data runs to the end of the file.
        Scenario::new(
            "adjust-import-below-data",
            format!("{MESSY}/04_two_tables.csv"),
        )
        .with(|doc, _, cx| doc.open_part(Some(0), cx))
        .action(AdjustImport)
        .with(|doc, window, cx| {
            doc.import_panel()
                .update(cx, |panel, cx| panel.pick_header(7, window, cx))
        }),
        Scenario::new("adjust-import-xlsx", &report_xlsx).action(AdjustImport),
        // Reading values the way people write them.
        Scenario::new("currency", &currency),
        Scenario::new("currency-details", &currency).with(details),
        Scenario::new("currency-details-light", &currency)
            .light()
            .with(details),
        Scenario::new("dates-flags", &ids),
        Scenario::new("dates-details", &ids).with(details),
        // `03/04/2025` and no day above 12: month first is a guess, and one
        // click reads the column day first.
        Scenario::new("dates-guess", guessed).with(details),
        Scenario::new("dates-guess-flipped", guessed)
            .with(details)
            .with(|doc, _, cx| doc.read_column_as("placed".into(), ReadAs::DayFirstDate, cx)),
        // Slash date-times read month first as a guess, like dates, and a
        // `1,125` column in a semicolon file read as a decimal comma guess.
        Scenario::new("datetime-guess", guessed_times).with(details),
        Scenario::new("latin1", &latin1),
        Scenario::new("european", format!("{MESSY}/02_semicolon_european.csv")),
        Scenario::new("read-as-menu", &ids)
            .with(|doc, _, cx| doc.open_menu_named("zip", cx))
            .with(|doc, _, cx| read_as(doc, ReadAs::Number, cx)),
        Scenario::new("read-as-applied", &ids)
            .with(|doc, _, cx| doc.open_menu_named("zip", cx))
            .with(|doc, _, cx| read_as(doc, ReadAs::Number, cx))
            .with(|doc, _, cx| {
                doc.filter_editor()
                    .update(cx, |menu, cx| menu.apply_read_as(cx))
            }),
        Scenario::new("read-as-misfit", &currency)
            .with(|doc, _, cx| doc.open_menu_named("region", cx))
            .with(|doc, _, cx| read_as(doc, ReadAs::Number, cx)),
        // Retyping a filtered column drops its filter; the sort stays.
        Scenario::new("read-as-filtered", &currency)
            .with(|doc, _, cx| doc.open_menu_named("revenue", cx))
            .with(|doc, window, cx| {
                doc.filter_editor().update(cx, |menu, cx| {
                    menu.type_filter(">", "$1,000,000", window, cx)
                })
            })
            .with(|doc, _, cx| doc.read_column_as("revenue".into(), ReadAs::Text, cx)),
        // A type chosen on one table is forgotten after a visit to another
        // tab: Region is text again.
        Scenario::new("read-as-tab-switch", format!("{MESSY}/04_two_tables.csv"))
            .with(|doc, _, cx| doc.show_part(0, cx))
            .with(|doc, _, cx| doc.read_column_as("Region".into(), ReadAs::Number, cx))
            .with(|doc, _, cx| doc.show_part(1, cx))
            .with(|doc, _, cx| doc.show_part(0, cx)),
        Scenario::new("adjust-import-encoding", &latin1).action(AdjustImport),
        // Forcing UTF-8 on a Windows-1252 file: the screen it gets.
        Scenario::new("encoding-utf8-forced", &latin1)
            .action(AdjustImport)
            .with(|doc, _, cx| {
                doc.import_panel().update(cx, |panel, cx| {
                    panel.set_encoding(da_core::Encoding::Utf8, cx);
                    panel.apply(cx);
                })
            }),
        Scenario::new("stats-percent", &currency)
            .select("margin_pct")
            .action(ToggleStats),
        Scenario::new("filter-chip-currency", &currency)
            .with(|doc, _, cx| doc.open_menu_named("revenue", cx))
            .with(|doc, window, cx| {
                doc.filter_editor().update(cx, |menu, cx| {
                    menu.type_filter(">", "$1,000,000", window, cx)
                })
            }),
        // The chart panel.
        Scenario::new("chart-bar", &sales)
            .select("product")
            .action(ToggleChart),
        Scenario::new("chart-bar-light", &sales)
            .light()
            .select("product")
            .action(ToggleChart),
        Scenario::new("chart-bar-count", &sales)
            .action(ToggleChart)
            .with(chart(ChartKind::Bar, "region", &[], None)),
        Scenario::new("chart-bar-colour", &sales)
            .action(ToggleChart)
            .with(chart(
                ChartKind::Bar,
                "product",
                &["revenue"],
                Some("region"),
            )),
        Scenario::new("chart-bar-colour-light", &sales)
            .light()
            .action(ToggleChart)
            .with(chart(
                ChartKind::Bar,
                "product",
                &["revenue"],
                Some("region"),
            )),
        Scenario::new("chart-line", &sales)
            .action(ToggleChart)
            .with(chart(
                ChartKind::Line,
                "order_date",
                &["revenue"],
                Some("region"),
            )),
        Scenario::new("chart-line-light", &sales)
            .light()
            .action(ToggleChart)
            .with(chart(
                ChartKind::Line,
                "order_date",
                &["revenue"],
                Some("region"),
            )),
        Scenario::new("chart-scatter", &sales)
            .action(ToggleChart)
            .with(chart(ChartKind::Scatter, "units", &["revenue"], None)),
        Scenario::new("chart-histogram", &sales)
            .select("unit_price")
            .action(ToggleChart),
        Scenario::new("chart-wide", &wide)
            .action(ToggleChart)
            .with(move |doc, _, cx| set_chart(doc, ChartKind::Bar, "Region", &months, None, cx)),
        Scenario::new("chart-currency", &currency)
            .action(ToggleChart)
            .with(chart(ChartKind::Bar, "region", &["revenue"], None)),
        Scenario::new("chart-filtered", &sales)
            .select("product")
            .action(ToggleChart)
            .with(|doc, _, cx| {
                doc.filter_by(
                    Filter {
                        column: "region".into(),
                        op: FilterOp::Equals("North".into()),
                    },
                    cx,
                )
            }),
        Scenario::new("chart-sql", &sales)
            .with(|doc, _, cx| {
                doc.run_sql(
                    "SELECT region, sum(revenue) AS revenue FROM data GROUP BY region",
                    cx,
                )
            })
            .action(ToggleChart),
        Scenario::new("chart-top20", &many).action(ToggleChart),
        Scenario::new("chart-with-stats", &sales)
            .select("product")
            .action(ToggleStats)
            .action(ToggleChart),
        Scenario::new("chart-empty", &words)
            .action(ToggleChart)
            .with(|doc, _, cx| {
                doc.chart_panel().update(cx, |panel, cx| {
                    panel.set_spec(
                        ChartSpec {
                            kind: ChartKind::Scatter,
                            ..ChartSpec::default()
                        },
                        cx,
                    )
                })
            }),
        Scenario::new("chart-no-rows", &sales)
            .select("product")
            .action(ToggleChart)
            .with(|doc, _, cx| doc.search_for("no such thing", cx)),
        Scenario::new("chart-tooltip", &sales)
            .select("product")
            .action(ToggleChart)
            .with(hover),
        // The narrowest window: a plot too small to draw in, which must not panic.
        Scenario::new("chart-narrow", &sales)
            .width(640.)
            .action(ToggleStats)
            .action(ToggleChart)
            .with(chart(
                ChartKind::Line,
                "order_date",
                &["revenue"],
                Some("region"),
            )),
        Scenario::new("chart-histogram-units", &sales)
            .select("units")
            .action(ToggleChart)
            .with(chart(ChartKind::Histogram, "units", &[], None)),
        Scenario::new("chart-scatter-colour", &sales)
            .action(ToggleChart)
            .with(chart(
                ChartKind::Scatter,
                "units",
                &["revenue"],
                Some("product"),
            )),
        Scenario::new("chart-two-series", &wide)
            .action(ToggleChart)
            .with(move |doc, _, cx| {
                doc.chart_panel().update(cx, |panel, cx| {
                    panel.set_spec(
                        ChartSpec {
                            kind: ChartKind::Bar,
                            x: Some("Region".into()),
                            y: months[..2].iter().map(|m| m.to_string()).collect(),
                            aggregate: Aggregate::Average,
                            ..ChartSpec::default()
                        },
                        cx,
                    )
                })
            }),
        Scenario::new("chart-line-tooltip", &sales)
            .action(ToggleChart)
            .with(chart(
                ChartKind::Line,
                "order_date",
                &["revenue"],
                Some("region"),
            ))
            .with(hover),
        Scenario::new("adjusted-applied", &subtotals)
            .action(AdjustImport)
            .with(|doc, _, cx| {
                doc.import_panel().update(cx, |panel, cx| {
                    panel.set_keep_totals(true, cx);
                    panel.apply(cx);
                })
            }),
        // Totals and nested data.
        Scenario::new("subtotals-show-totals", &subtotals),
        // As a click on Show totals does: the 17 rows, with Hide totals.
        Scenario::new("subtotals-shown", &subtotals).with(|doc, _, cx| doc.keep_totals(true, cx)),
        Scenario::new("json-envelope", &envelope),
        Scenario::new("json-notes", &envelope).with(|doc, window, cx| doc.show_notes(window, cx)),
        Scenario::new("parquet-nested", format!("{MESSY}/40_nested.parquet")),
        Scenario::new("json-chart", &envelope)
            .action(ToggleChart)
            .with(chart(ChartKind::Bar, "customer.tier", &["total"], None)),
        // The updater's prompt card in each state, and the menu's answer.
        Scenario::new("update-available", &sales)
            .with(update_card("available"))
            .wait(),
        Scenario::new("update-available-light", &sales)
            .light()
            .with(update_card("available"))
            .wait(),
        Scenario::new("update-downloading", &sales)
            .with(update_card("downloading"))
            .wait(),
        Scenario::new("update-ready", &sales)
            .with(update_card("ready"))
            .wait(),
        Scenario::new("update-cannot", &sales)
            .with(update_card("cannot"))
            .wait(),
        Scenario::new("update-uptodate", &sales)
            .with(update_card("uptodate"))
            .wait(),
    ]
}

/// A document in theme `theme`, with the stats and chart panels open.
fn themed(name: &'static str, theme: &'static str, file: &str) -> Scenario {
    Scenario::new(name, file)
        .theme(theme)
        .select("revenue")
        .action(ToggleStats)
        .action(ToggleChart)
        .with(chart(ChartKind::Bar, "region", &["revenue"], None))
}

/// Shows Settings' Keyboard Shortcuts page.
fn shortcuts_page(view: &mut SettingsView, _: &mut Window, cx: &mut Context<SettingsView>) {
    view.show_page(Page::Shortcuts, cx);
}

/// A step that presses `keys` while Settings records a shortcut.
fn press(
    keys: &'static str,
) -> impl FnOnce(&mut SettingsView, &mut Window, &mut Context<SettingsView>) {
    move |view, _, cx| view.record(&Keystroke::parse(keys).unwrap(), cx)
}

/// Writes `text` to a scratch file for a scenario and returns its path.
fn generated(name: &str, text: String) -> PathBuf {
    let path = std::env::temp_dir().join(format!("da-shots-{name}"));
    std::fs::write(&path, text).unwrap();
    path
}

/// Sets the chart panel's fields, as picking them in its rail does.
fn set_chart(
    doc: &mut Document,
    kind: ChartKind,
    x: &str,
    y: &[&str],
    color: Option<&str>,
    cx: &mut Context<Document>,
) {
    let spec = ChartSpec {
        kind,
        x: Some(x.into()),
        y: y.iter().map(|y| y.to_string()).collect(),
        color: color.map(Into::into),
        ..ChartSpec::default()
    };
    doc.chart_panel()
        .update(cx, |panel, cx| panel.set_spec(spec, cx));
}

/// A step that sets the chart panel's fields.
fn chart(
    kind: ChartKind,
    x: &'static str,
    y: &'static [&'static str],
    color: Option<&'static str>,
) -> impl FnOnce(&mut Document, &mut Window, &mut Context<Document>) {
    move |doc, _, cx| set_chart(doc, kind, x, y, color, cx)
}

/// Hovers the chart's first mark, as the pointer would.
fn hover(doc: &mut Document, _: &mut Window, cx: &mut Context<Document>) {
    doc.chart_panel()
        .update(cx, |panel, cx| panel.hover_first(cx));
}

/// A step that shows the updater's card in one of its states.
fn update_card(
    case: &'static str,
) -> impl FnOnce(&mut Document, &mut Window, &mut Context<Document>) {
    move |_, _, cx| crate::update::show_for_shots(case, cx)
}

/// A step that draws the app at `zoom`, as Zoom In and Zoom Out do.
fn zoom(zoom: f32) -> impl FnOnce(&mut Document, &mut Window, &mut Context<Document>) {
    move |_, _, cx| {
        Theme::update(cx, |theme| {
            theme.font_size = px(crate::theme::BASE_FONT_SIZE * zoom);
            theme.mono_font_size = px(crate::theme::BASE_MONO_FONT_SIZE * zoom);
        })
    }
}

/// Opens the Details popover, as clicking "Details" in the strip does.
fn details(doc: &mut Document, window: &mut Window, cx: &mut Context<Document>) {
    doc.show_details(window, cx);
}

/// Picks a Read as choice in the open column menu.
fn read_as(doc: &mut Document, choice: ReadAs, cx: &mut Context<Document>) {
    doc.filter_editor()
        .update(cx, |menu, cx| menu.pick_read_as(choice, cx));
}

/// The window size, in logical points; the image is twice this.
const WINDOW: (f32, f32) = (1180., 760.);
/// Upper bound on draw-and-run rounds while waiting for work to land.
const MAX_ROUNDS: usize = 200;

/// The Settings window's size, as the app opens it.
const SETTINGS_WINDOW: (f32, f32) = (760., 560.);

/// One screenshot: a file (or Settings), a theme and the steps to run once
/// it has loaded.
struct Scenario {
    name: &'static str,
    /// The file to open, or `None` for the Settings window.
    file: Option<PathBuf>,
    mode: ThemeMode,
    /// A theme's name in `theme::THEMES`.
    theme: &'static str,
    /// The window's size in points.
    size: (f32, f32),
    steps: Vec<Step>,
}

/// Something to do to the document before the capture. Each step is followed
/// by a wait for whatever background work it started.
enum Step {
    /// Dispatches an action to the focused document, as a shortcut or menu does.
    Action(Box<dyn Action>),
    /// Selects a column by name, as clicking one of its cells does.
    SelectColumn(&'static str),
    /// Runs arbitrary code against the document.
    With(Box<DocFn>),
    /// Runs arbitrary code against the Settings window's view.
    Settings(Box<SettingsFn>),
    /// Lets half a second pass, on the app's clock and the real one, so
    /// animations (a notification sliding in) finish.
    Wait,
}

/// Code a [`Step::With`] runs against the document.
type DocFn = dyn FnOnce(&mut Document, &mut Window, &mut Context<Document>);
/// Code a [`Step::Settings`] runs against the Settings view.
type SettingsFn = dyn FnOnce(&mut SettingsView, &mut Window, &mut Context<SettingsView>);

impl Scenario {
    /// A dark-theme shot of `file` (relative to the repo root) with no steps.
    fn new(name: &'static str, file: impl AsRef<Path>) -> Self {
        Self {
            name,
            file: Some(repo_root().join(file)),
            mode: ThemeMode::Dark,
            theme: "Default",
            size: WINDOW,
            steps: Vec::new(),
        }
    }

    /// A dark-theme shot of the Settings window, on its Appearance page.
    fn settings(name: &'static str) -> Self {
        Self {
            name,
            file: None,
            mode: ThemeMode::Dark,
            theme: "Default",
            size: SETTINGS_WINDOW,
            steps: Vec::new(),
        }
    }

    fn theme(mut self, theme: &'static str) -> Self {
        self.theme = theme;
        self
    }

    fn light(mut self) -> Self {
        self.mode = ThemeMode::Light;
        self
    }

    fn width(mut self, width: f32) -> Self {
        self.size.0 = width;
        self
    }

    fn action(mut self, action: impl Action) -> Self {
        self.steps.push(Step::Action(Box::new(action)));
        self
    }

    fn select(mut self, column: &'static str) -> Self {
        self.steps.push(Step::SelectColumn(column));
        self
    }

    fn wait(mut self) -> Self {
        self.steps.push(Step::Wait);
        self
    }

    fn with(
        mut self,
        f: impl FnOnce(&mut Document, &mut Window, &mut Context<Document>) + 'static,
    ) -> Self {
        self.steps.push(Step::With(Box::new(f)));
        self
    }

    fn settings_with(
        mut self,
        f: impl FnOnce(&mut SettingsView, &mut Window, &mut Context<SettingsView>) + 'static,
    ) -> Self {
        self.steps.push(Step::Settings(Box::new(f)));
        self
    }

    /// Opens the file in a fresh offscreen app, runs the steps and returns the PNG's path.
    fn render(self, out_dir: &Path) -> PathBuf {
        let name = self.name;
        let mut cx = HeadlessAppContext::with_platform(
            Arc::new(gpui_apple::AppleTextSystem::new()),
            Arc::new(gpui_kit::assets::AllAssets),
            gpui_kit::platform::current_headless_renderer,
        );
        // Settings start from the defaults each run; a scenario that changes
        // them writes only to this scratch file.
        let settings_file = std::env::temp_dir().join(format!("da-shots-settings-{name}.json"));
        let _ = std::fs::remove_file(&settings_file);
        let (window, doc, settings_view) = cx.update(|cx| {
            gpui_kit::init(cx);
            filter::init(cx);
            import::init(cx);
            document::init(cx);
            settings::load(settings_file, cx);
            crate::keymap::init(cx);
            cx.set_global(Windows::default());
            // Never written: nothing in a scenario edits the recent list.
            cx.set_global(RecentStore::load(out_dir.join("recent.json")));
            let store = cx.global_mut::<SettingsStore>();
            store.settings.theme = self.theme.into();
            store.settings.appearance = match self.mode {
                ThemeMode::Light => Appearance::Light,
                ThemeMode::Dark => Appearance::Dark,
            };

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(self.size.0), px(self.size.1)),
                })),
                focus: true,
                show: false,
                ..TitleBar::window_options()
            };
            let opened = match self.file.clone() {
                Some(path) => {
                    let (window, workspace) = gpui_kit::open_window(options, cx, |window, cx| {
                        cx.new(|cx| Workspace::new(Some(path), window, cx))
                    })
                    .unwrap_or_else(|err| panic!("{name}: couldn't open a window: {err:#}"));
                    cx.global_mut::<Windows>()
                        .0
                        .push((window, workspace.clone()));
                    let doc = workspace.read(cx).document().cloned();
                    (window, doc, None)
                }
                None => {
                    let (window, view) = gpui_kit::open_window(options, cx, |window, cx| {
                        cx.new(|cx| SettingsView::new(window, cx))
                    })
                    .unwrap_or_else(|err| panic!("{name}: couldn't open a window: {err:#}"));
                    (window, None, Some(view))
                }
            };
            // Opening a window syncs the theme to its appearance, so pick ours after.
            settings::apply_theme(None, cx);
            opened
        });
        let doc = doc.as_ref();
        let document = || doc.unwrap_or_else(|| panic!("{name}: the window has no document"));

        settle(&mut cx, window, doc, name, "opening");
        for step in self.steps {
            let what = match &step {
                Step::Action(action) => action.name(),
                Step::SelectColumn(_) => "select column",
                Step::With(_) | Step::Settings(_) => "custom step",
                Step::Wait => "wait",
            };
            match step {
                Step::Action(action) => cx
                    .update_window(window, |_, window, cx| window.dispatch_action(action, cx))
                    .unwrap(),
                Step::SelectColumn(column) => cx.update(|cx| {
                    document().update(cx, |doc, cx| doc.select_column_named(column, cx));
                }),
                Step::With(f) => cx
                    .update_window(window, |_, window, cx| {
                        document().update(cx, |doc, cx| f(doc, window, cx))
                    })
                    .unwrap(),
                Step::Settings(f) => cx
                    .update_window(window, |_, window, cx| {
                        let view = settings_view.as_ref().expect("a Settings scenario");
                        view.update(cx, |view, cx| f(view, window, cx))
                    })
                    .unwrap(),
                Step::Wait => {
                    for _ in 0..5 {
                        std::thread::sleep(Duration::from_millis(100));
                        cx.advance_clock(Duration::from_millis(100));
                        draw(&mut cx, window);
                        cx.run_until_parked();
                    }
                }
            }
            settle(&mut cx, window, doc, name, what);
        }

        let image = cx
            .capture_screenshot(window)
            .unwrap_or_else(|err| panic!("{name}: capture failed: {err:#}"));
        let out = out_dir.join(format!("{name}.png"));
        image
            .save(&out)
            .unwrap_or_else(|err| panic!("{name}: couldn't write {}: {err}", out.display()));
        out
    }
}

/// Draws and runs tasks until the document (if the window has one) reports
/// nothing pending, then draws once more so the frame shows the final state.
/// Panics on a timeout.
fn settle(
    cx: &mut HeadlessAppContext,
    window: AnyWindowHandle,
    doc: Option<&Entity<Document>>,
    name: &str,
    after: &str,
) {
    let mut pending = None;
    for _ in 0..MAX_ROUNDS {
        // Drawing lays out the grid, which requests the visible pages.
        draw(cx, window);
        cx.run_until_parked();
        // Let debounces and other short timers fire.
        cx.advance_clock(Duration::from_millis(100));
        pending = cx.update(|cx| doc.and_then(|doc| doc.read(cx).pending_work(cx)));
        if pending.is_none() {
            draw(cx, window);
            return;
        }
    }
    panic!(
        "{name}: still {} after {after} ({MAX_ROUNDS} rounds)",
        pending.unwrap_or_default()
    );
}

/// Lays out and paints a fresh frame, which is what the capture reads.
fn draw(cx: &mut HeadlessAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx: &mut App| {
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .unwrap();
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
#[ignore = "renders PNGs; run with --ignored"]
fn shots() {
    let out_dir = match std::env::var_os("DA_SHOTS_DIR") {
        Some(dir) => repo_root().join(dir),
        None => repo_root().join("target/shots"),
    };
    std::fs::create_dir_all(&out_dir).unwrap();
    let only = std::env::var("DA_SHOT").ok();
    let mut ran = 0;
    for scenario in scenarios() {
        if only.as_deref().is_some_and(|only| only != scenario.name) {
            continue;
        }
        let started = std::time::Instant::now();
        let out = scenario.render(&out_dir);
        println!(
            "{} ({:.1}s)",
            out.display(),
            started.elapsed().as_secs_f32()
        );
        ran += 1;
    }
    assert!(ran > 0, "no scenario is named {only:?}");
}
