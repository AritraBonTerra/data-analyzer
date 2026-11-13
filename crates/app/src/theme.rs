//! Visual tokens layered on top of gpui-component's theme: the few colours and
//! text styles that are specific to a data grid, and small styling helpers so
//! every module spells them the same way.

use std::sync::Arc;

use da_core::ColumnKind;
use gpui_kit::component::{ActiveTheme, kbd::Kbd};
use gpui_kit::*;

/// Row height in the grid. Tight enough to show plenty of rows, roomy enough to read.
pub const ROW_HEIGHT: f32 = 26.0;
/// Header height: a name row plus a sketch row.
pub const HEADER_HEIGHT: f32 = 54.0;
/// Height of the window's unified toolbar.
pub const TOOLBAR_HEIGHT: f32 = 46.0;
/// Width of the stats side panel.
pub const STATS_WIDTH: f32 = 312.0;

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
    let accent = if dark {
        hsla(214. / 360., 0.86, 0.68, 1.0)
    } else {
        hsla(214. / 360., 0.78, 0.48, 1.0)
    };
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

/// A key binding as the platform writes it: `"cmd-shift-c"` is `⇧⌘C` on
/// macOS, `"ctrl-shift-c"` is `Ctrl+Shift+C` on Linux.
pub fn keys(binding: &'static str) -> String {
    Keystroke::parse(binding)
        .map(|keystroke| Kbd::format(&keystroke))
        .unwrap_or_else(|_| binding.into())
}

/// A keyboard-shortcut style hint for a binding, e.g. `⌘F`.
pub fn shortcut_hint(binding: &'static str, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground.opacity(0.7))
        .child(keys(binding))
}
