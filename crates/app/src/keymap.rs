//! The shortcuts people can change in Settings. [`COMMANDS`] lists each
//! command with its default keys. [`apply`] rebuilds the app's keymap from
//! those defaults and the user's changes.
//!
//! Keys use GPUI's syntax (`"secondary-shift-e"`). `secondary` is ⌘ on macOS
//! and Ctrl on Linux and Windows. Bindings that aren't listed here (Escape,
//! Enter in a filter, text editing keys) are fixed. They are the "base" keymap
//! captured by [`init`].

use std::rc::Rc;

use gpui_kit::component::kbd::Kbd;
use gpui_kit::*;

use crate::document::{ClearFilters, Export, Find, GoToRow, ToggleChart, ToggleSql, ToggleStats};
use crate::settings::{OpenSettings, ResetZoom, SettingsStore, ZoomIn, ZoomOut};
use crate::sql::{CancelQuery, RunQuery};
use crate::{CloseWindow, Minimize, NewWindow, Open, Quit};

/// A command that can have a shortcut.
pub struct Command {
    /// Stable key in `settings.json`. Never rename one.
    pub id: &'static str,
    /// What Settings calls it.
    pub name: &'static str,
    /// The heading it's listed under in Settings.
    pub group: &'static str,
    /// Its shortcuts until the user changes them. The first is the one shown.
    pub defaults: &'static [&'static str],
    /// Where the shortcut works: a key context, or anywhere when `None`.
    context: Option<&'static str>,
    action: fn() -> Box<dyn Action>,
}

/// Every command that can have a shortcut, in the order Settings lists them.
pub const COMMANDS: &[Command] = &[
    Command {
        id: "open",
        name: "Open…",
        group: "File",
        defaults: &["secondary-o"],
        context: None,
        action: || Box::new(Open),
    },
    Command {
        id: "new_window",
        name: "New Window",
        group: "File",
        defaults: &["secondary-n"],
        context: None,
        action: || Box::new(NewWindow),
    },
    Command {
        id: "close_window",
        name: "Close Window",
        group: "File",
        defaults: &["secondary-w"],
        context: None,
        action: || Box::new(CloseWindow),
    },
    Command {
        id: "export",
        name: "Export…",
        group: "File",
        defaults: &["secondary-shift-e"],
        context: Some(crate::document::CONTEXT),
        action: || Box::new(Export),
    },
    Command {
        id: "settings",
        name: "Settings…",
        group: "File",
        defaults: &["secondary-,"],
        context: None,
        action: || Box::new(OpenSettings),
    },
    Command {
        id: "quit",
        name: "Quit",
        group: "File",
        defaults: &["secondary-q"],
        context: None,
        action: || Box::new(Quit),
    },
    Command {
        id: "find",
        name: "Find",
        group: "Edit",
        defaults: &["secondary-f"],
        context: Some(crate::document::CONTEXT),
        action: || Box::new(Find),
    },
    Command {
        id: "clear_filters",
        name: "Clear Filters and Search",
        group: "Edit",
        defaults: &["secondary-shift-k"],
        context: Some(crate::document::CONTEXT),
        action: || Box::new(ClearFilters),
    },
    Command {
        id: "go_to_row",
        name: "Go to Row",
        group: "Edit",
        defaults: &["secondary-g"],
        context: Some(crate::document::CONTEXT),
        action: || Box::new(GoToRow),
    },
    Command {
        id: "stats",
        name: "Stats Panel",
        group: "View",
        defaults: &["secondary-i"],
        context: Some(crate::document::CONTEXT),
        action: || Box::new(ToggleStats),
    },
    Command {
        id: "chart",
        name: "Chart Panel",
        group: "View",
        defaults: &["secondary-shift-c"],
        context: Some(crate::document::CONTEXT),
        action: || Box::new(ToggleChart),
    },
    Command {
        id: "sql",
        name: "SQL Panel",
        group: "View",
        defaults: &["secondary-e"],
        context: Some(crate::document::CONTEXT),
        action: || Box::new(ToggleSql),
    },
    Command {
        id: "zoom_in",
        name: "Zoom In",
        group: "View",
        // `=` is the key with `+` on it; `+` covers keyboards that type it directly.
        defaults: &["secondary-=", "secondary-+"],
        context: None,
        action: || Box::new(ZoomIn),
    },
    Command {
        id: "zoom_out",
        name: "Zoom Out",
        group: "View",
        defaults: &["secondary--"],
        context: None,
        action: || Box::new(ZoomOut),
    },
    Command {
        id: "reset_zoom",
        name: "Actual Size",
        group: "View",
        defaults: &["secondary-0"],
        context: None,
        action: || Box::new(ResetZoom),
    },
    Command {
        id: "run_query",
        name: "Run Query",
        group: "SQL",
        defaults: &["secondary-enter"],
        context: Some(crate::sql::CONTEXT),
        action: || Box::new(RunQuery),
    },
    Command {
        id: "cancel_query",
        name: "Cancel Query",
        group: "SQL",
        defaults: &["secondary-."],
        context: Some(crate::sql::CONTEXT),
        action: || Box::new(CancelQuery),
    },
    Command {
        id: "minimize",
        name: "Minimize",
        group: "Window",
        defaults: &["secondary-m"],
        context: None,
        action: || Box::new(Minimize),
    },
];

/// The bindings that aren't in [`COMMANDS`], kept so [`apply`] can rebuild
/// the keymap from scratch.
struct BaseKeymap(Vec<KeyBinding>);

impl Global for BaseKeymap {}

/// Captures the bindings every module registered, then binds the commands.
/// Call once, after every other `init` and before any window opens.
pub fn init(cx: &mut App) {
    let base = cx.key_bindings().borrow().bindings().cloned().collect();
    cx.set_global(BaseKeymap(base));
    apply(cx);
}

pub fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.id == id)
}

/// The shortcuts `command` has now: the user's, or its defaults.
pub fn shortcuts(command: &Command, cx: &App) -> Vec<String> {
    let custom = cx
        .try_global::<SettingsStore>()
        .and_then(|store| store.settings.shortcuts.get(command.id));
    match custom {
        Some(keys) => keys.clone(),
        None => command
            .defaults
            .iter()
            .map(|keys| keys.to_string())
            .collect(),
    }
}

/// Whether `command` still has its default shortcuts.
pub fn is_default(command: &Command, cx: &App) -> bool {
    cx.try_global::<SettingsStore>()
        .is_none_or(|store| !store.settings.shortcuts.contains_key(command.id))
}

/// Parses a shortcut written in settings or a default, resolving `secondary`.
pub fn parse(keys: &str, cx: &App) -> Option<Keystroke> {
    Keystroke::parse_with_os(keys, cx.platform_info().os).ok()
}

/// A shortcut as the platform writes it: `⇧⌘E` on macOS, `Ctrl+Shift+E` on Linux.
pub fn display(keys: &str, cx: &App) -> String {
    parse(keys, cx)
        .map(|keystroke| Kbd::format(&keystroke))
        .unwrap_or_else(|| keys.to_string())
}

/// The shortcut shown for command `id` in hints and tooltips, if it has one.
pub fn label(id: &str, cx: &App) -> Option<String> {
    let command = command(id)?;
    let keys = shortcuts(command, cx).into_iter().next()?;
    Some(display(&keys, cx))
}

/// A tooltip naming command `id`'s shortcut: `"Chart (⇧⌘C)"`, or just
/// `"Chart"` when it has none.
pub fn tooltip(text: &str, id: &str, cx: &App) -> String {
    match label(id, cx) {
        Some(keys) => format!("{text} ({keys})"),
        None => text.to_string(),
    }
}

/// Whether a recorded keystroke can be a shortcut: a key other than a
/// modifier, held with ⌘/Ctrl or ⌥/Alt (so it can't steal typing), or a
/// function key on its own.
pub fn can_bind(keystroke: &Keystroke) -> bool {
    let key = keystroke.key.as_str();
    let modifier_only = matches!(
        key,
        "shift" | "control" | "alt" | "platform" | "function" | "capslock" | ""
    );
    let function_key = key.len() > 1 && key.starts_with('f') && key[1..].parse::<u8>().is_ok();
    let m = &keystroke.modifiers;
    !modifier_only && (m.platform || m.control || m.alt || function_key)
}

/// Whether two keystrokes press the same keys. A recorded keystroke also
/// carries the character it typed (`key_char`), which a parsed one doesn't.
fn same_keys(a: &Keystroke, b: &Keystroke) -> bool {
    a.modifiers == b.modifiers && a.key == b.key
}

impl Command {
    /// A new instance of the command's action, to dispatch.
    pub fn action(&self) -> Box<dyn Action> {
        (self.action)()
    }
}

/// The command whose shortcuts include `keys` (e.g. `"secondary-enter"`), if any.
pub fn owner(keys: &str, cx: &App) -> Option<&'static Command> {
    let wanted = parse(keys, cx)?;
    COMMANDS.iter().find(|command| {
        shortcuts(command, cx)
            .iter()
            .any(|keys| parse(keys, cx).is_some_and(|keys| same_keys(&keys, &wanted)))
    })
}

/// The other commands that have any of `keystrokes`, each with the
/// shortcuts it keeps once those are taken away.
fn take_from_others(
    id: &str,
    keystrokes: &[Keystroke],
    cx: &App,
) -> Vec<(&'static Command, Vec<String>)> {
    let mut changes = Vec::new();
    for other in COMMANDS.iter().filter(|other| other.id != id) {
        let current = shortcuts(other, cx);
        let kept: Vec<String> = current
            .iter()
            .filter(|keys| {
                !parse(keys, cx).is_some_and(|keys| keystrokes.iter().any(|k| same_keys(&keys, k)))
            })
            .cloned()
            .collect();
        if kept.len() != current.len() {
            changes.push((other, kept));
        }
    }
    changes
}

/// Gives command `id` the single shortcut `keystroke`, or none when `None`.
/// Another command that had the same shortcut loses it; its name is
/// returned so the caller can say so.
pub fn assign(id: &str, keystroke: Option<&Keystroke>, cx: &mut App) -> Option<&'static str> {
    let keys = keystroke.map(|keystroke| keystroke.unparse(cx.platform_info().os));
    let changes = take_from_others(
        id,
        keystroke.map(std::slice::from_ref).unwrap_or_default(),
        cx,
    );
    let taken_from = changes.last().map(|(other, _)| other.name);
    SettingsStore::edit(cx, |settings| {
        for (other, kept) in changes {
            settings.shortcuts.insert(other.id.into(), kept);
        }
        settings
            .shortcuts
            .insert(id.into(), keys.into_iter().collect());
    });
    taken_from
}

/// Puts command `id` back to its default shortcuts, taking them from any
/// other command that was given one since.
pub fn reset(id: &str, cx: &mut App) {
    let defaults: Vec<Keystroke> = command(id)
        .map(|command| {
            command
                .defaults
                .iter()
                .filter_map(|keys| parse(keys, cx))
                .collect()
        })
        .unwrap_or_default();
    let changes = take_from_others(id, &defaults, cx);
    SettingsStore::edit(cx, |settings| {
        for (other, kept) in changes {
            settings.shortcuts.insert(other.id.into(), kept);
        }
        settings.shortcuts.remove(id);
    });
}

/// Puts every command back to its default shortcuts.
pub fn reset_all(cx: &mut App) {
    SettingsStore::edit(cx, |settings| settings.shortcuts.clear());
}

/// Rebuilds the app's keymap: the base bindings, then every command's current
/// shortcuts. Called by [`init`] and whenever the shortcuts change.
pub fn apply(cx: &mut App) {
    let mut bindings = cx
        .try_global::<BaseKeymap>()
        .map(|base| base.0.clone())
        .unwrap_or_default();
    for command in COMMANDS {
        let context = command
            .context
            .and_then(|context| KeyBindingContextPredicate::parse(context).ok())
            .map(Rc::new);
        for keys in shortcuts(command, cx) {
            match KeyBinding::load(&keys, (command.action)(), context.clone(), false, None, cx) {
                Ok(binding) => bindings.push(binding),
                Err(err) => eprintln!("ignoring shortcut {keys:?} for {}: {err}", command.id),
            }
        }
    }
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's own `test` macro would shadow `#[test]`.
    use super::{COMMANDS, can_bind};
    use gpui_kit::{Keystroke, OperatingSystem};

    #[test]
    fn command_ids_are_unique() {
        for (i, command) in COMMANDS.iter().enumerate() {
            assert!(
                COMMANDS[i + 1..].iter().all(|other| other.id != command.id),
                "{} is listed twice",
                command.id
            );
        }
    }

    #[test]
    fn only_modified_or_function_keys_can_be_shortcuts() {
        let can =
            |keys: &str| can_bind(&Keystroke::parse_with_os(keys, OperatingSystem::Mac).unwrap());
        assert!(can("cmd-k"));
        assert!(can("alt-shift-x"));
        assert!(can("f5"));
        assert!(!can("k"));
        assert!(!can("shift-k"));
        assert!(!can("enter"));
    }
}
