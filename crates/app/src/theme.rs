//! Visual tokens layered on top of gpui-component's theme: the few colours and
//! text styles that are specific to a data grid, and small styling helpers so
//! every module spells them the same way.

use std::sync::Arc;

use da_core::ColumnKind;
use gpui_kit::component::ActiveTheme;
use gpui_kit::*;

/// Row height in the grid. Tight enough to show plenty of rows, roomy enough to read.
pub const ROW_HEIGHT: f32 = 26.0;
/// Header height: a name row plus a sketch row.
pub const HEADER_HEIGHT: f32 = 54.0;
/// Height of the window's unified toolbar.
pub const TOOLBAR_HEIGHT: f32 = 46.0;
/// Width of the stats side panel.
pub const STATS_WIDTH: f32 = 312.0;

/// Text size at actual size (zoom 1). Zoom scales the theme's font size, which
/// sets the window's rem, so everything sized in rems follows.
pub const BASE_FONT_SIZE: f32 = 16.0;
/// Monospace text size at actual size.
pub const BASE_MONO_FONT_SIZE: f32 = 13.0;

/// The current zoom: 1 at actual size, 1.25 when everything is a quarter bigger.
pub fn zoom(cx: &App) -> f32 {
    cx.theme().font_size.as_f32() / BASE_FONT_SIZE
}

/// `points` at the current zoom. For the layout constants above, which are in
/// pixels and so wouldn't follow the rem size on their own.
pub fn scaled(points: f32, cx: &App) -> Pixels {
    px((points * zoom(cx)).round())
}

/// A theme people can pick in Settings: a light and a dark variant registered
/// with gpui-component, and the accent for data marks in each.
pub struct ThemeFamily {
    pub name: &'static str,
    /// Names of its variants in the theme registry.
    pub light: &'static str,
    pub dark: &'static str,
    /// The theme file declaring both variants; `None` for the built-in default.
    pub json: Option<&'static str>,
    /// Accent for data marks (see [`Palette::accent`]) in light and dark.
    pub accent: (u32, u32),
}

/// Every theme, in the order Settings lists them. The first is the default.
/// The others are files in `crates/app/themes`: the default theme with its
/// greys tinted, and an accent picked to read well on charts in both variants.
pub const THEMES: &[ThemeFamily] = &[
    ThemeFamily {
        name: "Default",
        light: "Default Light",
        dark: "Default Dark",
        json: None,
        accent: (0x1b6eda, 0x67a4f4),
    },
    // Warm paper and ink; a warm charcoal when dark.
    ThemeFamily {
        name: "Paper",
        light: "Paper Light",
        dark: "Paper Dark",
        json: Some(include_str!("../themes/paper.json")),
        accent: (0x1a7f76, 0x5cc4b5),
    },
    // Cool blue-greys.
    ThemeFamily {
        name: "Slate",
        light: "Slate Light",
        dark: "Slate Dark",
        json: Some(include_str!("../themes/slate.json")),
        accent: (0x4c5fd5, 0x8c9cf7),
    },
    // Pure black and white, with stronger borders and secondary text.
    ThemeFamily {
        name: "High Contrast",
        light: "High Contrast Light",
        dark: "High Contrast Dark",
        json: Some(include_str!("../themes/high-contrast.json")),
        accent: (0x0047c2, 0x79b8ff),
    },
];

/// The theme named `name`, or the default when there's none by that name.
pub fn family(name: &str) -> &'static ThemeFamily {
    THEMES
        .iter()
        .find(|family| family.name == name)
        .unwrap_or(&THEMES[0])
}

/// Enables tabular figures so digits line up in columns and counters don't jitter.
pub fn tabular() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

/// Colours that depend on the active appearance but aren't in the base theme.
pub struct Palette {
    /// The one colour for data marks: charts, sort arrows, filter chips.
    pub accent: Hsla,
    /// Dim ink for NULL markers and placeholders.
    pub null: Hsla,
    /// Fill for sketch bars and charts.
    pub chart: Hsla,
    /// Chart fill when the bar is hovered or selected.
    pub chart_strong: Hsla,
    /// Light chart track behind bars.
    pub chart_track: Hsla,
    /// Row-number gutter background.
    pub gutter: Hsla,
    /// Background of the header and toolbars.
    pub chrome: Hsla,
    /// Warm ink for notices.
    pub warning: Hsla,
}

pub fn palette(cx: &App) -> Palette {
    let theme = cx.theme();
    let dark = theme.is_dark();
    let (light_accent, dark_accent) = family(&crate::settings::current(cx).theme).accent;
    let accent: Hsla = rgb(if dark { dark_accent } else { light_accent }).into();
    Palette {
        accent,
        null: theme.muted_foreground.opacity(0.55),
        chart: accent.opacity(if dark { 0.6 } else { 0.5 }),
        chart_strong: accent,
        chart_track: theme.foreground.opacity(if dark { 0.08 } else { 0.05 }),
        gutter: theme
            .background
            .blend(theme.foreground.opacity(if dark { 0.04 } else { 0.025 })),
        chrome: theme
            .background
            .blend(theme.foreground.opacity(if dark { 0.03 } else { 0.02 })),
        warning: if dark {
            hsla(38. / 360., 0.9, 0.62, 1.0)
        } else {
            hsla(32. / 360., 0.85, 0.38, 1.0)
        },
    }
}

/// How many categorical colours charts have for their series.
pub const SERIES_COLORS: usize = 6;

/// The categorical colour in `slot` (below [`SERIES_COLORS`]), in a fixed
/// order tuned for each appearance. Never cycled or generated.
pub fn series_color(slot: usize, cx: &App) -> Hsla {
    const DARK: [u32; SERIES_COLORS] = [0x3987e5, 0xd95926, 0x199e70, 0xc98500, 0xd55181, 0x9085e9];
    const LIGHT: [u32; SERIES_COLORS] =
        [0x2a78d6, 0xeb6834, 0x1baf7a, 0xeda100, 0xe87ba4, 0x6250d6];
    let colors = if cx.theme().is_dark() { DARK } else { LIGHT };
    rgb(colors[slot % SERIES_COLORS]).into()
}

/// The neutral grey of a chart's `Other` series: opaque, so overlapping
/// dots never mix into colours the legend doesn't show.
pub fn other_series(cx: &App) -> Hsla {
    let theme = cx.theme();
    theme.background.blend(theme.muted_foreground.opacity(0.55))
}

/// The glyph that marks a column's type in headers and the stats panel.
pub fn kind_icon(kind: ColumnKind) -> &'static str {
    match kind {
        ColumnKind::Number => "icons/hash.svg",
        ColumnKind::Text => "icons/type.svg",
        ColumnKind::Temporal => "icons/calendar.svg",
        ColumnKind::Boolean => "icons/toggle-left.svg",
        ColumnKind::Other => "icons/braces.svg",
    }
}

/// A keyboard-shortcut style hint for command `id` (see `keymap::COMMANDS`),
/// e.g. `⌘F`, or `None` when the command has no shortcut.
pub fn shortcut_hint(id: &str, cx: &App) -> Option<Div> {
    let keys = crate::keymap::label(id, cx)?;
    Some(
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground.opacity(0.7))
            .child(keys),
    )
}
