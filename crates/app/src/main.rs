//! Data Analyzer: a native macOS viewer for CSV, TSV, Excel, Parquet and JSON.
//!
//! This file wires the app together: actions and shortcuts, the native menu
//! bar, routing of files to windows (CLI args, ⌘O, Finder, drag and drop) and
//! the app-wide state (recent files, open windows).

mod document;
mod filter;
mod format;
mod grid;
mod pages;
mod platform;
mod recent;
mod sql;
mod stats;
mod theme;
mod welcome;
mod workspace;

use std::path::PathBuf;

use futures::StreamExt;
use futures::channel::mpsc::unbounded;
use gpui_kit::component::{Theme, TitleBar};
use gpui_kit::*;
use serde::Deserialize;

use crate::document::{
    ClearFilters, Export, ExportAs, ExportKind, Find, ShowFileData, ToggleSql, ToggleStats,
};
use crate::recent::RecentStore;
use crate::workspace::Workspace;

actions!(
    data_analyzer,
    [
        Open,
        NewWindow,
        CloseWindow,
        Quit,
        About,
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
    let app = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);

    // Finder and `open` deliver files through the app delegate, sometimes
    // before `run` has started; queue them and drain inside the app loop.
    let (urls_tx, mut urls_rx) = unbounded::<Vec<String>>();
    app.on_open_urls(move |urls| {
        let _ = urls_tx.unbounded_send(urls);
    });
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            new_window(None, cx);
        }
    });

    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    app.run(move |cx| {
        gpui_kit::init(cx);
        filter::init(cx);
        sql::init(cx);
        document::init(cx);
        cx.set_app_identity(platform::BUNDLE_ID, "Data Analyzer");
        Theme::sync_system_appearance(None, cx);
        Theme::sync_scrollbar_appearance(cx);
        cx.set_global(Windows::default());
        cx.set_global(RecentStore::load(
            platform::app_support_dir().join("recent.json"),
        ));
        cx.background_spawn(async { da_core::clean_cache(&platform::cache_dir()) })
            .detach();

        cx.bind_keys([
            KeyBinding::new("cmd-o", Open, None),
            KeyBinding::new("cmd-n", NewWindow, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-m", Minimize, None),
        ]);
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
                            "Data Analyzer 0.1 · a fast viewer for CSV, Excel, Parquet and JSON files.",
                        ),
                        cx,
                    );
                });
            }
        });
        cx.on_action(|_: &MakeDefault, cx| {
            let result = platform::make_default_for_data_files();
            if let Some(window) = cx.active_window() {
                let _ = window.update(cx, |_, window, cx| {
                    let note = match result {
                        Ok(()) => gpui_kit::component::notification::Notification::success(
                            "Requested. macOS will ask you to confirm each file type.",
                        ),
                        Err(err) => {
                            gpui_kit::component::notification::Notification::info(err.to_string())
                        }
                    };
                    window.push_notification(note, cx);
                });
            }
        });
        // Fallback when no window content handled ⌘W (e.g. nothing focused).
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
        })
        .detach();
        cx.observe_global::<Theme>(refresh_menus).detach();
        rebuild_menus(cx);

        cx.spawn(async move |cx| {
            while let Some(urls) = urls_rx.next().await {
                cx.update(|cx| {
                    for url in urls {
                        if let Some(path) = platform::url_to_path(&url) {
                            open_path(path, cx);
                        }
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

fn rebuild_menus(cx: &mut App) {
    let (stats, sql) = cx
        .active_window()
        .and_then(|handle| {
            let windows = &cx.global::<Windows>().0;
            windows
                .iter()
                .find(|(h, _)| h.window_id() == handle.window_id())
                .map(|(_, w)| w.clone())
        })
        .and_then(|workspace| workspace.read(cx).document().cloned())
        .map(|doc| {
            let doc = doc.read(cx);
            (doc.stats_shown(), doc.sql_shown())
        })
        .unwrap_or((false, false));

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

    cx.set_menus(vec![
        Menu::new("Data Analyzer").items([
            MenuItem::action("About Data Analyzer", About),
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
        Menu::new("File").items([
            MenuItem::action("Open…", Open),
            MenuItem::submenu(recent_menu),
            MenuItem::action("New Window", NewWindow),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
            MenuItem::separator(),
            MenuItem::action("Export…", Export),
            MenuItem::submenu(Menu::new("Export As").items([
                MenuItem::action("CSV…", ExportAs(ExportKind::Csv)),
                MenuItem::action("Parquet…", ExportAs(ExportKind::Parquet)),
                MenuItem::action("JSON Lines…", ExportAs(ExportKind::Json)),
            ])),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Copy", document::Copy, OsAction::Copy),
            MenuItem::separator(),
            MenuItem::action("Find", Find),
            MenuItem::action("Clear Filters and Search", ClearFilters),
        ]),
        Menu::new("View").items([
            MenuItem::action("Stats Panel", ToggleStats).checked(stats),
            MenuItem::action("SQL Panel", ToggleSql).checked(sql),
            MenuItem::separator(),
            MenuItem::action("Show File Data", ShowFileData),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
        ]),
    ]);
}
