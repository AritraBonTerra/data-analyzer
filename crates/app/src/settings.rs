//! App settings: appearance, theme, zoom and custom shortcuts. They are saved
//! as `settings.json` next to the recent-files list, and [`SettingsStore::edit`]
//! saves a change and applies it to every window at once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use crate::theme::{self, THEMES};

actions!(settings, [OpenSettings, ZoomIn, ZoomOut, ResetZoom]);

/// Whether the app follows the system's light or dark appearance or keeps one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    /// The name of a theme in [`THEMES`]. An unknown name shows the first one.
    pub theme: String,
    /// How much bigger than actual size the interface is drawn: one of [`ZOOM_STEPS`].
    pub zoom: f32,
    /// Shortcuts the user changed, by command id (see `keymap::COMMANDS`).
    /// An empty list means the command has no shortcut.
    pub shortcuts: BTreeMap<String, Vec<String>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            theme: THEMES[0].name.into(),
            zoom: 1.0,
            shortcuts: BTreeMap::new(),
        }
    }
}

/// The zoom levels that Zoom In and Zoom Out step through.
pub const ZOOM_STEPS: [f32; 9] = [0.75, 0.85, 1.0, 1.1, 1.25, 1.4, 1.6, 1.8, 2.0];

/// The next zoom level up (`step` 1) or down (`step` -1) from `zoom`,
/// stopping at either end of [`ZOOM_STEPS`].
pub fn step_zoom(zoom: f32, step: i32) -> f32 {
    let at = match ZOOM_STEPS.iter().position(|z| (z - zoom).abs() < 0.01) {
        Some(at) => at as i32 + step,
        // A hand-edited value between steps goes to the step above or below it.
        None => {
            let above = ZOOM_STEPS.iter().filter(|z| **z < zoom).count() as i32;
            if step > 0 { above } else { above - 1 }
        }
    };
    let at = at.clamp(0, ZOOM_STEPS.len() as i32 - 1);
    ZOOM_STEPS[at as usize]
}

impl Settings {
    /// Reads settings from `file`, or the defaults if it's missing or unreadable.
    pub fn load(file: &Path) -> Self {
        let mut settings: Self = std::fs::read(file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        settings.zoom = settings.zoom.clamp(ZOOM_STEPS[0], ZOOM_STEPS[8]);
        settings
    }

    pub fn save(&self, file: &Path) -> anyhow::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(file, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

/// The app's settings: loaded once at startup, saved and applied after every change.
pub struct SettingsStore {
    pub settings: Settings,
    file: PathBuf,
}

impl Global for SettingsStore {}

impl SettingsStore {
    /// Changes the settings, saves them and applies them to every window.
    pub fn edit(cx: &mut App, edit: impl FnOnce(&mut Settings)) {
        let store = cx.global_mut::<Self>();
        let before = store.settings.clone();
        edit(&mut store.settings);
        if store.settings == before {
            return;
        }
        if let Err(err) = store.settings.save(&store.file) {
            eprintln!("couldn't save settings: {err:#}");
        }
        if store.settings.shortcuts != before.shortcuts {
            crate::keymap::apply(cx);
        }
        cx.refresh_windows();
        apply_theme(None, cx);
        crate::refresh_menus(cx);
    }
}

/// The current settings, or the defaults where the store isn't set up (the
/// screenshot harness).
pub fn current(cx: &App) -> Settings {
    cx.try_global::<SettingsStore>()
        .map(|store| store.settings.clone())
        .unwrap_or_default()
}

/// Installs the store with the settings in `file` and registers the bundled
/// themes. [`init`] calls it; the screenshot harness does too, with a scratch file.
pub fn load(file: PathBuf, cx: &mut App) {
    cx.set_global(SettingsStore {
        settings: Settings::load(&file),
        file,
    });
    for family in THEMES {
        if let Some(json) = family.json
            && let Err(err) = ThemeRegistry::global_mut(cx).load_themes_from_str(json)
        {
            eprintln!("couldn't load the {} theme: {err:#}", family.name);
        }
    }
}

/// Loads the settings in `file` and the bundled themes, applies both and
/// registers the actions. Call after every module's `init`, so the keymap sees
/// their bindings.
pub fn init(file: PathBuf, cx: &mut App) {
    load(file, cx);
    crate::keymap::init(cx);
    apply_theme(None, cx);

    cx.on_action(|_: &OpenSettings, cx| crate::settings_window::open(cx));
    cx.on_action(|_: &ZoomIn, cx| SettingsStore::edit(cx, |s| s.zoom = step_zoom(s.zoom, 1)));
    cx.on_action(|_: &ZoomOut, cx| SettingsStore::edit(cx, |s| s.zoom = step_zoom(s.zoom, -1)));
    cx.on_action(|_: &ResetZoom, cx| SettingsStore::edit(cx, |s| s.zoom = 1.0));
}

/// Applies the chosen theme, appearance and zoom. `window` is the window whose
/// appearance changed, if one did: on Linux it knows the system appearance
/// better than the app does.
pub fn apply_theme(window: Option<&mut Window>, cx: &mut App) {
    let settings = current(cx);
    let family = theme::family(&settings.theme);
    let registry = ThemeRegistry::global(cx);
    let light = registry
        .themes()
        .get(family.light)
        .unwrap_or(registry.default_light_theme())
        .clone();
    let dark = registry
        .themes()
        .get(family.dark)
        .unwrap_or(registry.default_dark_theme())
        .clone();
    let mode = match settings.appearance {
        Appearance::Light => ThemeMode::Light,
        Appearance::Dark => ThemeMode::Dark,
        Appearance::System => window
            .as_ref()
            .map(|window| window.appearance())
            .unwrap_or_else(|| cx.window_appearance())
            .into(),
    };
    let theme = Theme::global_mut(cx);
    theme.light_theme = light;
    theme.dark_theme = dark;
    // Loads the mode's theme file, which leaves the font sizes alone.
    Theme::change(mode, window, cx);
    Theme::update(cx, |theme| {
        theme.font_size = px(theme::BASE_FONT_SIZE * settings.zoom);
        theme.mono_font_size = px(theme::BASE_MONO_FONT_SIZE * settings.zoom);
    });
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's own `test` macro would shadow `#[test]`.
    use super::{Appearance, Settings, step_zoom};

    #[test]
    fn zoom_steps_and_stops_at_the_ends() {
        assert_eq!(step_zoom(1.0, 1), 1.1);
        assert_eq!(step_zoom(1.0, -1), 0.85);
        assert_eq!(step_zoom(2.0, 1), 2.0);
        assert_eq!(step_zoom(0.75, -1), 0.75);
        // Between steps: up goes to the step above, down to the one below.
        assert_eq!(step_zoom(1.05, 1), 1.1);
        assert_eq!(step_zoom(1.05, -1), 1.0);
    }

    #[test]
    fn round_trips_and_fills_in_missing_fields() {
        let dir = std::env::temp_dir().join(format!("da-settings-{}", std::process::id()));
        let file = dir.join("settings.json");
        let mut settings = Settings {
            appearance: Appearance::Dark,
            zoom: 1.25,
            ..Settings::default()
        };
        settings
            .shortcuts
            .insert("find".into(), vec!["cmd-shift-f".into()]);
        settings.save(&file).unwrap();
        assert_eq!(Settings::load(&file), settings);

        std::fs::write(&file, r#"{"zoom": 9}"#).unwrap();
        let loaded = Settings::load(&file);
        assert_eq!(loaded.zoom, 2.0);
        assert_eq!(loaded.appearance, Appearance::System);
        assert_eq!(
            Settings::load(&dir.join("missing.json")),
            Settings::default()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
