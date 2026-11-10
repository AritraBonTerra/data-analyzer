//! Offscreen screenshots of the real app, for checking UI work by eye.
//!
//! Each [`Scenario`] opens a file in the app's own `Workspace` view, with its
//! theme, fonts and icons, in a 1180×760 window that Metal renders at 2× and
//! never puts on screen. It runs the scenario's steps, drives every
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
//! the repo root), then any of `.light()`, `.select(column)`, `.action(A)` and
//! `.with(|doc, window, cx| ...)` for anything the others don't cover.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::{Theme, ThemeMode, TitleBar};
use gpui_kit::{
    Action, AnyWindowHandle, App, AppContext, Bounds, Context, Entity, HeadlessAppContext, Window,
    WindowBounds, WindowOptions, point, px, size,
};

use crate::document::{AdjustImport, Document, ToggleStats};
use crate::recent::RecentStore;
use crate::workspace::Workspace;
use crate::{Windows, document, filter, import, sql};

const MESSY: &str = "crates/core/tests/fixtures/messy";

/// The scenarios, in the order they run. Add yours here.
fn scenarios() -> Vec<Scenario> {
    let sales = format!("{MESSY}/00_clean_sales.csv");
    let report = format!("{MESSY}/01_report_preamble.csv");
    let report_xlsx = format!("{MESSY}/20_report_title.xlsx");
    let multirow = format!("{MESSY}/06_multirow_header.csv");
    let subtotals = format!("{MESSY}/12_subtotals.csv");
    vec![
        Scenario::new("clean-sales", &sales),
        Scenario::new("clean-sales-stats", &sales)
            .select("revenue")
            .action(ToggleStats),
        Scenario::new("clean-sales-light", &sales).light(),
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
        Scenario::new("adjusted-applied", &subtotals)
            .action(AdjustImport)
            .with(|doc, _, cx| {
                doc.import_panel().update(cx, |panel, cx| {
                    panel.set_keep_totals(true, cx);
                    panel.apply(cx);
                })
            }),
    ]
}

/// The window size, in logical points; the image is twice this.
const WINDOW: (f32, f32) = (1180., 760.);
/// Upper bound on draw-and-run rounds while waiting for work to land.
const MAX_ROUNDS: usize = 200;

/// One screenshot: a file, a theme and the steps to run once it has loaded.
struct Scenario {
    name: &'static str,
    file: PathBuf,
    mode: ThemeMode,
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
}

/// Code a [`Step::With`] runs against the document.
type DocFn = dyn FnOnce(&mut Document, &mut Window, &mut Context<Document>);

impl Scenario {
    /// A dark-theme shot of `file` (relative to the repo root) with no steps.
    fn new(name: &'static str, file: impl AsRef<Path>) -> Self {
        Self {
            name,
            file: repo_root().join(file),
            mode: ThemeMode::Dark,
            steps: Vec::new(),
        }
    }

    fn light(mut self) -> Self {
        self.mode = ThemeMode::Light;
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

    fn with(
        mut self,
        f: impl FnOnce(&mut Document, &mut Window, &mut Context<Document>) + 'static,
    ) -> Self {
        self.steps.push(Step::With(Box::new(f)));
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
        let (window, workspace) = cx.update(|cx| {
            gpui_kit::init(cx);
            filter::init(cx);
            sql::init(cx);
            import::init(cx);
            document::init(cx);
            cx.set_global(Windows::default());
            // Never written: nothing in a scenario edits the recent list.
            cx.set_global(RecentStore::load(out_dir.join("recent.json")));

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(WINDOW.0), px(WINDOW.1)),
                })),
                focus: true,
                show: false,
                ..TitleBar::window_options()
            };
            let path = self.file.clone();
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(Some(path), window, cx))
            })
            .unwrap_or_else(|err| panic!("{name}: couldn't open a window: {err:#}"));
            cx.global_mut::<Windows>().0.push(opened.clone());
            // Opening a window syncs the theme to its appearance, so pick ours after.
            Theme::change(self.mode, None, cx);
            opened
        });
        let doc = cx
            .update(|cx| workspace.read(cx).document().cloned())
            .unwrap_or_else(|| panic!("{name}: the window has no document"));

        settle(&mut cx, window, &doc, name, "opening");
        for step in self.steps {
            let what = match &step {
                Step::Action(action) => action.name(),
                Step::SelectColumn(_) => "select column",
                Step::With(_) => "custom step",
            };
            match step {
                Step::Action(action) => cx
                    .update_window(window, |_, window, cx| window.dispatch_action(action, cx))
                    .unwrap(),
                Step::SelectColumn(column) => cx.update(|cx| {
                    doc.update(cx, |doc, cx| doc.select_column_named(column, cx));
                }),
                Step::With(f) => cx
                    .update_window(window, |_, window, cx| {
                        doc.update(cx, |doc, cx| f(doc, window, cx))
                    })
                    .unwrap(),
            }
            settle(&mut cx, window, &doc, name, what);
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

/// Draws and runs tasks until the document reports nothing pending, then
/// draws once more so the frame shows the final state. Panics on a timeout.
fn settle(
    cx: &mut HeadlessAppContext,
    window: AnyWindowHandle,
    doc: &Entity<Document>,
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
        pending = cx.update(|cx| doc.read(cx).pending_work(cx));
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
