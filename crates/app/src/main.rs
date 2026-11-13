//! Data Analyzer: a native viewer for CSV, TSV, Excel, Parquet and JSON, for
//! macOS and Linux.
//!
//! This file wires the app together: actions and shortcuts, the menus (the
//! system menu bar on macOS, one in each window's title bar on Linux),
//! routing of files to windows (CLI args, Open…, Finder or a second launch,
//! drag and drop), the app-wide state (recent files, open windows) and the
//! two command-line flags, `--version` and `--update`.

/// A key binding on the platform's main modifier: `cmd_or_ctrl!("o")` is
/// `"cmd-o"` on macOS and `"ctrl-o"` on Linux.
macro_rules! cmd_or_ctrl {
    ($keys:literal) => {
        if cfg!(target_os = "macos") {
            concat!("cmd-", $keys)
        } else {
            concat!("ctrl-", $keys)
        }
    };
}

mod chart;
mod details;
mod document;
mod filter;
mod format;
mod grid;
mod import;
mod keymap;
mod pages;
mod platform;
mod recent;
mod settings;
mod settings_window;
#[cfg(all(test, target_os = "macos"))]
mod shots;
mod sql;
mod stats;
mod theme;
mod update;
mod welcome;
mod workspace;

use std::path::PathBuf;

use futures::StreamExt;
use futures::channel::mpsc::unbounded;
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::component::{Theme, TitleBar};
use gpui_kit::*;
use serde::Deserialize;

use crate::document::{
    AdjustImport, ClearFilters, Export, ExportAs, ExportKind, Find, ShowFileData, ToggleChart,
    ToggleSql, ToggleStats,
};
use crate::recent::RecentStore;
use crate::settings::{OpenSettings, ResetZoom, ZoomIn, ZoomOut};
use crate::workspace::Workspace;

actions!(
    data_analyzer,
    [
        Open,
        NewWindow,
        CloseWindow,
        Quit,
        About,
        CheckForUpdates,
        MakeDefault,
        ClearRecent,
        Minimize,
        Zoom,
    ]
);

#[derive(Action, Clone, Debug, PartialEq, Eq, Deserialize)]
#[action(namespace = data_analyzer, no_json)]
pub struct OpenRecent(pub PathBuf);

/// Every open window, so a file that's already open can be brought forward.
#[derive(Default)]
struct Windows(Vec<(AnyWindowHandle, Entity<Workspace>)>);

impl Global for Windows {}

fn main() {
    // Flags are only recognized on their own; anything else is a file to open.
    match std::env::args().nth(1).as_deref() {
        Some("--version") => {
            println!("{}", update::VERSION);
            return;
        }
        Some("--update") => std::process::exit(update::run_cli()),
        _ => {}
    }

    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    // On Linux the installed app runs once per user, as macOS does it: a
    // second launch hands its files to the first and ends here.
    #[cfg(target_os = "linux")]
    let instance = match update::running_app().map(|_| platform::claim_instance(&args)) {
        Some(platform::Launch::Forwarded) => return,
        Some(platform::Launch::First(listener)) => listener,
        None => None,
    };

    let app = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);

    // Files arriving while the app runs: from Finder and `open` through the
    // app delegate on macOS (sometimes before `run` has started), from later
    // launches on Linux. Queued here and drained inside the app loop; an
    // empty batch is a launch with no files and asks for a window.
    let (files_tx, mut files_rx) = unbounded::<Vec<PathBuf>>();
    let delegate_tx = files_tx.clone();
    #[cfg(target_os = "linux")]
    if let Some(listener) = instance {
        platform::listen_for_launches(listener, move |paths| {
            let _ = files_tx.unbounded_send(paths);
        });
    }
    app.on_open_urls(move |urls| {
        let paths: Vec<PathBuf> = urls
            .iter()
            .filter_map(|u| platform::url_to_path(u))
            .collect();
        if !paths.is_empty() {
            let _ = delegate_tx.unbounded_send(paths);
        }
    });
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            new_window(None, cx);
        }
    });

    app.run(move |cx| {
        gpui_kit::init(cx);
        filter::init(cx);
        import::init(cx);
        document::init(cx);
        // After the other modules: it captures their bindings, adds the
        // changeable shortcuts and applies the theme.
        settings::init(platform::data_dir().join("settings.json"), cx);
        cx.set_app_identity(platform::BUNDLE_ID, "Data Analyzer");
        Theme::sync_scrollbar_appearance(cx);
        cx.set_global(Windows::default());
        cx.set_global(MenuBars::default());
        cx.set_global(RecentStore::load(platform::data_dir().join("recent.json")));
        #[cfg(target_os = "linux")]
        cx.background_spawn(async { platform::refresh_desktop_entry() })
            .detach();
        cx.background_spawn(async { da_core::clean_cache(&platform::cache_dir()) })
            .detach();
        // Open datasets aren't dropped on quit, so delete their scratch files here.
        cx.on_app_quit(|_| {
            da_core::remove_own_sessions(&platform::cache_dir());
            async {}
        })
        .detach();

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &Open, cx| open_dialog(cx));
        cx.on_action(|_: &NewWindow, cx| new_window(None, cx));
        cx.on_action(|action: &OpenRecent, cx| open_path(action.0.clone(), cx));
        cx.on_action(|_: &ClearRecent, cx| {
            cx.global_mut::<RecentStore>().edit(|files| files.clear());
            refresh_menus(cx);
        });
        cx.on_action(|_: &About, cx| {
            if let Some(window) = cx.active_window() {
                let _ = window.update(cx, |_, window, cx| {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::info(
                            format!(
                                "Data Analyzer {} · a fast viewer for CSV, Excel, Parquet and JSON files.",
                                update::VERSION
                            ),
                        ),
                        cx,
                    );
                });
            }
        });
        cx.on_action(|_: &CheckForUpdates, cx| update::check_from_menu(cx));
        cx.on_action(|_: &MakeDefault, cx| {
            let result = platform::make_default_for_data_files();
            if let Some(window) = cx.active_window() {
                let _ = window.update(cx, |_, window, cx| {
                    let note = match result {
                        Ok(done) => {
                            gpui_kit::component::notification::Notification::success(done)
                        }
                        Err(err) => {
                            gpui_kit::component::notification::Notification::info(err.to_string())
                        }
                    };
                    window.push_notification(note, cx);
                });
            }
        });
        // Fallback when no window content handled Close Window (e.g. nothing focused).
        cx.on_action(|_: &CloseWindow, cx| {
            for handle in cx.windows() {
                let _ = handle.update(cx, |_, window, _| {
                    if window.is_window_active() {
                        window.remove_window();
                    }
                });
            }
        });

        cx.on_window_closed(|cx, id| {
            cx.global_mut::<Windows>()
                .0
                .retain(|(handle, _)| handle.window_id() != id);
            cx.global_mut::<MenuBars>().0.remove(&id);
            if cx.try_global::<MenuWindow>().and_then(|w| w.0) == Some(id) {
                cx.set_global(MenuWindow(None));
            }
            settings_window::closed(id, cx);
            // A macOS app stays in the Dock with no windows; on Linux nothing
            // would be left to show it is running.
            if cfg!(not(target_os = "macos"))
                && cx.global::<Windows>().0.is_empty()
                && !settings_window::is_open(cx)
            {
                cx.quit();
            }
        })
        .detach();
        cx.observe_global::<Theme>(refresh_menus).detach();
        rebuild_menus(cx);

        cx.spawn(async move |cx| {
            while let Some(paths) = files_rx.next().await {
                cx.update(|cx| {
                    if paths.is_empty() {
                        new_window(None, cx);
                    }
                    for path in paths {
                        open_path(path, cx);
                    }
                });
            }
        })
        .detach();

        if args.is_empty() {
            new_window(None, cx);
        } else {
            for path in args {
                open_path(path, cx);
            }
        }
        cx.activate(true);
        update::init(cx);
    });
}

use gpui_kit::component::WindowExt;

/// Shows `path` in a window: the one that already has it, an empty welcome
/// window, or a new one. Also records it in Open Recent.
pub fn open_path(path: PathBuf, cx: &mut App) {
    let path = path.canonicalize().unwrap_or(path);
    let windows = cx.global::<Windows>().0.clone();
    for (handle, workspace) in &windows {
        if workspace.read(cx).path(cx).as_ref() == Some(&path) {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
            return;
        }
    }
    cx.global_mut::<RecentStore>()
        .edit(|files| files.push(&path));
    refresh_menus(cx);

    if let Some((handle, workspace)) = windows.iter().find(|(_, w)| w.read(cx).is_empty()) {
        let workspace = workspace.clone();
        let _ = handle.update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.open(path, window, cx));
            window.activate_window();
        });
        return;
    }
    new_window(Some(path), cx);
}

/// Opens a document window, or the welcome window when `path` is `None`.
fn new_window(path: Option<PathBuf>, cx: &mut App) {
    let mut size = size(px(1180.), px(760.));
    if let Some(display) = cx.primary_display() {
        let bounds = display.bounds().size;
        size.width = size.width.min(bounds.width * 0.9);
        size.height = size.height.min(bounds.height * 0.9);
    }
    // Cascade new windows a little so they don't stack exactly.
    let count = cx.global::<Windows>().0.len() as f32;
    let mut bounds = Bounds::centered(None, size, cx);
    bounds.origin.x += px(24. * count);
    bounds.origin.y += px(24. * count);

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(gpui_kit::Size {
            width: px(640.),
            height: px(400.),
        }),
        titlebar: Some(TitlebarOptions {
            title: Some("Data Analyzer".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(12.), px(17.))),
        }),
        // Linux only: the app draws its own title bar, and the desktop matches
        // windows to the installed `.desktop` file (and its icon) by app id.
        window_decorations: Some(WindowDecorations::Client),
        app_id: Some(platform::BUNDLE_ID.into()),
        ..TitleBar::window_options()
    };
    let opened = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| Workspace::new(path, window, cx))
    });
    match opened {
        Ok((handle, workspace)) => {
            cx.global_mut::<Windows>().0.push((handle, workspace));
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        Err(err) => eprintln!("couldn't open a window: {err:#}"),
    }
}

/// The native open panel; every chosen file gets its own window.
fn open_dialog(cx: &mut App) {
    let receiver = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: true,
        prompt: Some("Open".into()),
    });
    cx.spawn(async move |cx| {
        if let Ok(Ok(Some(paths))) = receiver.await {
            cx.update(|cx| {
                for path in paths {
                    open_path(path, cx);
                }
            });
        }
    })
    .detach();
}

/// Rebuilds the menu bar once the current update finishes; call after
/// anything it reflects changes (recent files, panel visibility).
///
/// Always deferred: rebuilding reads the active window's document, and
/// reading an entity that is mid-update panics, which aborts the app when
/// it happens inside a key-event handler.
pub fn refresh_menus(cx: &mut App) {
    cx.defer(rebuild_menus);
}

/// The window that last became active. The menus describe it: Linux can
/// report no active window while focus moves from one window to another.
#[derive(Default)]
struct MenuWindow(Option<WindowId>);

impl Global for MenuWindow {}

/// Points the menus at `window`, which just became active.
pub fn window_activated(window: WindowId, cx: &mut App) {
    cx.set_global(MenuWindow(Some(window)));
    refresh_menus(cx);
}

fn rebuild_menus(cx: &mut App) {
    let active = (cx.try_global::<MenuWindow>().and_then(|w| w.0))
        .or_else(|| cx.active_window().map(|handle| handle.window_id()));
    let (stats, chart, sql, adjust) = active
        .and_then(|id| {
            let windows = &cx.global::<Windows>().0;
            windows
                .iter()
                .find(|(h, _)| h.window_id() == id)
                .map(|(_, w)| w.clone())
        })
        .and_then(|workspace| workspace.read(cx).document().cloned())
        .map(|doc| {
            let doc = doc.read(cx);
            (
                doc.stats_shown(),
                doc.chart_shown(),
                doc.sql_shown(),
                doc.can_adjust_import(),
            )
        })
        .unwrap_or((false, false, false, false));

    let recent: Vec<MenuItem> = cx
        .global::<RecentStore>()
        .files
        .existing()
        .map(|file| {
            let name = file
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.path.display().to_string());
            MenuItem::action(name, OpenRecent(file.path.clone()))
        })
        .collect();
    let mut recent_menu = Menu::new("Open Recent");
    if recent.is_empty() {
        recent_menu
            .items
            .push(MenuItem::action("No Recent Files", ClearRecent).disabled(true));
    } else {
        recent_menu.items = recent;
        recent_menu.items.push(MenuItem::separator());
        recent_menu
            .items
            .push(MenuItem::action("Clear Menu", ClearRecent));
    }

    let mut file = Menu::new("File").items([
        MenuItem::action("Open…", Open),
        MenuItem::submenu(recent_menu),
        MenuItem::action("New Window", NewWindow),
        MenuItem::separator(),
        MenuItem::action("Close Window", CloseWindow),
        MenuItem::separator(),
        MenuItem::action("Adjust Import…", AdjustImport).disabled(!adjust),
        MenuItem::separator(),
        MenuItem::action("Export…", Export),
        MenuItem::submenu(Menu::new("Export As").items([
            MenuItem::action("CSV…", ExportAs(ExportKind::Csv)),
            MenuItem::action("Parquet…", ExportAs(ExportKind::Parquet)),
            MenuItem::action("JSON Lines…", ExportAs(ExportKind::Json)),
        ])),
    ]);
    let edit = Menu::new("Edit").items([
        MenuItem::os_action("Copy", document::Copy, OsAction::Copy),
        MenuItem::separator(),
        MenuItem::action("Find", Find),
        MenuItem::action("Clear Filters and Search", ClearFilters),
    ]);
    let view = Menu::new("View").items([
        MenuItem::action("Stats Panel", ToggleStats).checked(stats),
        MenuItem::action("Chart Panel", ToggleChart).checked(chart),
        MenuItem::action("SQL Panel", ToggleSql).checked(sql),
        MenuItem::separator(),
        MenuItem::action("Show File Data", ShowFileData),
        MenuItem::separator(),
        MenuItem::action("Zoom In", ZoomIn),
        MenuItem::action("Zoom Out", ZoomOut),
        MenuItem::action("Actual Size", ResetZoom),
    ]);

    // macOS puts the app's own items in a menu named after it; Linux apps
    // keep Quit in File and the rest in Help.
    let menus = if cfg!(target_os = "macos") {
        vec![
            Menu::new("Data Analyzer").items([
                MenuItem::action("About Data Analyzer", About),
                MenuItem::action("Check for Updates…", CheckForUpdates),
                MenuItem::separator(),
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::action("Make Default for Data Files…", MakeDefault),
                MenuItem::separator(),
                MenuItem::SystemMenu(OsMenu {
                    name: "Services".into(),
                    menu_type: SystemMenuType::Services,
                }),
                MenuItem::separator(),
                MenuItem::action("Quit Data Analyzer", Quit),
            ]),
            file,
            edit,
            view,
            Menu::new("Window").items([
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
            ]),
        ]
    } else {
        file.items.extend([
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Quit", Quit),
        ]);
        vec![
            file,
            edit,
            view,
            Menu::new("Help").items([
                MenuItem::action("Check for Updates…", CheckForUpdates),
                MenuItem::action("Make Default for Data Files…", MakeDefault),
                MenuItem::separator(),
                MenuItem::action("About Data Analyzer", About),
            ]),
        ]
    };
    cx.set_menus(menus);
    reload_menu_bars(cx);
}

/// Each window's menu bar, for platforms without a system one (Linux). The
/// title bars ask [`menu_bar`] for theirs; [`rebuild_menus`] reloads them all.
#[derive(Default)]
struct MenuBars(std::collections::HashMap<WindowId, Entity<AppMenuBar>>);

impl Global for MenuBars {}

/// The menu bar for `window`'s title bar, or `None` on macOS, where the
/// menus are in the system menu bar.
pub fn menu_bar(window: &Window, cx: &mut App) -> Option<impl IntoElement + use<>> {
    if cfg!(target_os = "macos") {
        return None;
    }
    let id = window.window_handle().window_id();
    let bar = match cx.global::<MenuBars>().0.get(&id) {
        Some(bar) => bar.clone(),
        None => {
            let bar = AppMenuBar::new(cx);
            cx.global_mut::<MenuBars>().0.insert(id, bar.clone());
            bar
        }
    };
    Some(div().flex_none().h_full().py(px(6.)).child(bar))
}

/// Hands the menus just set to the window menu bars (Linux).
fn reload_menu_bars(cx: &mut App) {
    if cfg!(target_os = "macos") {
        return;
    }
    let menus = cx.get_menus().unwrap_or_default();
    gpui_kit::base::GlobalState::global_mut(cx).set_app_menus(menus);
    let bars: Vec<_> = cx.global::<MenuBars>().0.values().cloned().collect();
    for bar in bars {
        bar.update(cx, |bar, cx| bar.reload(cx));
    }
}
