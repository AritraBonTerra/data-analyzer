//! The Adjust import panel: the raw file or sheet with the import plan drawn
//! over it, and a rail to change the plan. Clicking a row makes it the
//! header. Nothing reloads until Apply; the document reopens the file then.

use std::sync::Arc;

use da_core::{
    Dataset, Decimal, Delimiter, Encoding, ImportPlan, Preview, PreviewRow, RowRole, Rows,
};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::format::{group_digits, plural};
use crate::theme::{self, STATS_WIDTH, tabular};

actions!(import, [CancelImport]);

const CONTEXT: &str = "ImportPanel";

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", CancelImport, Some(CONTEXT))]);
}

#[derive(Clone, Debug)]
pub enum ImportEvent {
    /// Reopen the part with this plan and decimal mark.
    Apply(ImportPlan, Decimal),
    /// Reopen with a new delimiter or encoding and detect everything again.
    Redetect(Reading),
    /// Reopen as detected: no plan, every choice automatic.
    Reset,
    Cancel,
}

/// The rail's choices besides the plan: how the text is split into fields
/// and decoded (CSV/TSV only), and the decimal mark of its numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    pub delimiter: Delimiter,
    pub encoding: Encoding,
    pub decimal: Decimal,
}

/// The rail's fields as the user left them, checked only when read as a plan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Draft {
    /// `5`, `1 to 3`, or `None`.
    header: String,
    first: String,
    last: String,
    reading: Reading,
    keep_totals: bool,
}

impl Draft {
    fn new(plan: &ImportPlan, reading: Reading) -> Self {
        Self {
            header: match plan.header {
                None => "None".into(),
                Some(h) if h.first == h.last => h.first.to_string(),
                Some(h) => format!("{} to {}", h.first, h.last),
            },
            first: plan.data.first.to_string(),
            last: plan.data.last.to_string(),
            reading,
            keep_totals: plan.keep_totals,
        }
    }

    /// What a click on row `row` of a file of `rows` rows does: one header
    /// row there, data from the next. A click at or past the end of the data
    /// runs the data to the last row, so the plan stays valid.
    fn pick_header(&mut self, row: usize, rows: usize) {
        self.header = row.to_string();
        self.first = (row + 1).to_string();
        if number(&self.last).is_none_or(|last| row >= last) {
            self.last = rows.to_string();
        }
    }

    /// The plan the fields describe, or what is wrong with them, for a file
    /// of `rows` rows called `line`s (CSV) or `row`s (sheets).
    fn plan(&self, rows: usize, line: &str) -> Result<ImportPlan, String> {
        let header = self.header.trim();
        let header = if header.is_empty() || header.eq_ignore_ascii_case("none") {
            None
        } else {
            let (a, b) = header
                .split_once(" to ")
                .or_else(|| header.split_once('-'))
                .unwrap_or((header, header));
            match (number(a), number(b)) {
                (Some(first), Some(last)) if first <= last => Some(Rows { first, last }),
                _ => return Err(format!("Header must be a {line} number, like 5, or None.")),
            }
        };
        let (Some(first), Some(last)) = (number(&self.first), number(&self.last)) else {
            return Err(format!("Data must be two {line} numbers."));
        };
        let highest = header.map_or(last, |h| h.last.max(last));
        if first == 0 || header.is_some_and(|h| h.first == 0) || highest > rows {
            return Err(format!(
                "The file has {line}s 1 to {}.",
                group_digits(rows as u64)
            ));
        }
        if first > last {
            return Err("Data must start before it ends.".into());
        }
        if header.is_some_and(|h| h.last >= first) {
            return Err("The header must come before the data.".into());
        }
        Ok(ImportPlan {
            header,
            data: Rows { first, last },
            keep_totals: self.keep_totals,
        })
    }
}

/// A row number as typed: `45` or `1,024`.
fn number(text: &str) -> Option<usize> {
    text.trim().replace(',', "").parse().ok()
}

pub struct ImportPanel {
    focus_handle: FocusHandle,
    dataset: Option<Arc<Dataset>>,
    /// The choices the file was opened with.
    opened: Reading,
    draft: Draft,
    header: Entity<InputState>,
    first: Entity<InputState>,
    last: Entity<InputState>,
    preview: Option<Preview>,
    /// The plan `preview` was drawn for, or is being drawn for.
    previewed: Option<ImportPlan>,
    loading: bool,
    /// Bumped per preview request; an older result arriving late is dropped.
    generation: u64,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl ImportPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let header = cx.new(|cx| InputState::new(window, cx).placeholder("None"));
        let first = cx.new(|cx| InputState::new(window, cx).placeholder("First"));
        let last = cx.new(|cx| InputState::new(window, cx).placeholder("Last"));
        let subscriptions = [&header, &first, &last]
            .into_iter()
            .map(|input| cx.subscribe(input, Self::on_input))
            .collect();
        Self {
            focus_handle: cx.focus_handle(),
            dataset: None,
            opened: Reading::default(),
            draft: Draft::default(),
            header,
            first,
            last,
            preview: None,
            previewed: None,
            loading: false,
            generation: 0,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Shows `dataset`'s open part under the plan in effect. `opened` is
    /// the choices it was opened with (`Auto` unless the user picked one).
    /// Does nothing for a file with no plan.
    pub fn open(
        &mut self,
        dataset: Arc<Dataset>,
        opened: Reading,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(plan) = dataset.plan().copied() else {
            return;
        };
        self.dataset = Some(dataset);
        self.opened = opened;
        self.draft = Draft::new(&plan, opened);
        self.preview = None;
        self.previewed = None;
        self.scroll = ScrollHandle::new();
        self.sync_inputs(window, cx);
        self.focus_handle.focus(window, cx);
        self.refresh(cx);
    }

    /// Forgets the dataset and its preview, so the panel never keeps a
    /// replaced dataset (and its files) alive. Call when the document reopens.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.dataset = None;
        self.preview = None;
        self.previewed = None;
        self.loading = false;
        self.generation += 1;
        cx.notify();
    }

    /// The status bar's text while the panel is open, like
    /// `48 lines in file · 40 data rows selected`.
    pub fn status(&self) -> Option<String> {
        let preview = self.preview.as_ref()?;
        Some(format!(
            "{} in file · {} selected",
            plural(preview.total_rows as u64, self.line_word()),
            plural(preview.data_rows as u64, "data row")
        ))
    }

    /// Whether a preview is still being computed. For the screenshot harness.
    #[cfg(test)]
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    fn is_csv(&self) -> bool {
        self.dataset
            .as_ref()
            .is_some_and(|d| d.delimiter().is_some())
    }

    fn line_word(&self) -> &'static str {
        if self.is_csv() { "line" } else { "row" }
    }

    /// The draft as a plan, or why it isn't one. `None` until the first
    /// preview says how many rows the file has.
    fn checked(&self) -> Option<Result<ImportPlan, String>> {
        let rows = self.preview.as_ref()?.total_rows;
        Some(self.draft.plan(rows, self.line_word()))
    }

    fn sync_inputs(&self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.draft.clone();
        for (input, value) in [
            (&self.header, draft.header),
            (&self.first, draft.first),
            (&self.last, draft.last),
        ] {
            input.update(cx, |s, cx| s.set_value(value, window, cx));
        }
    }

    fn on_input(&mut self, _: Entity<InputState>, event: &InputEvent, cx: &mut Context<Self>) {
        match event {
            InputEvent::Change => {
                self.draft.header = self.header.read(cx).value().to_string();
                self.draft.first = self.first.read(cx).value().to_string();
                self.draft.last = self.last.read(cx).value().to_string();
                self.refresh(cx);
            }
            InputEvent::PressEnter { .. } => self.apply(cx),
            _ => {}
        }
    }

    /// Makes `row` the header and starts the data on the next row, as a
    /// click on it in the raw view does.
    pub fn pick_header(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.preview.as_ref().map_or(0, |p| p.total_rows);
        self.draft.pick_header(row, rows);
        self.sync_inputs(window, cx);
        self.refresh(cx);
    }

    pub fn set_keep_totals(&mut self, keep: bool, cx: &mut Context<Self>) {
        self.draft.keep_totals = keep;
        self.refresh(cx);
    }

    fn set_delimiter(&mut self, delimiter: Delimiter, cx: &mut Context<Self>) {
        self.draft.reading.delimiter = delimiter;
        cx.notify();
    }

    pub fn set_encoding(&mut self, encoding: Encoding, cx: &mut Context<Self>) {
        self.draft.reading.encoding = encoding;
        cx.notify();
    }

    fn set_decimal(&mut self, decimal: Decimal, cx: &mut Context<Self>) {
        self.draft.reading.decimal = decimal;
        cx.notify();
    }

    /// What changed that makes Apply find the table anew, if anything:
    /// `delimiter`, `encoding`, or both.
    fn redetects(&self) -> Option<&'static str> {
        let (draft, opened) = (self.draft.reading, self.opened);
        match (
            draft.delimiter != opened.delimiter,
            draft.encoding != opened.encoding,
        ) {
            (true, true) => Some("delimiter and encoding"),
            (true, false) => Some("delimiter"),
            (false, true) => Some("encoding"),
            (false, false) => None,
        }
    }

    /// Redraws the roles for the draft in the background, if it is a valid
    /// plan the preview doesn't show yet. The first call previews the plan in effect.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(dataset) = self.dataset.clone() else {
            return;
        };
        let plan = match self.checked() {
            Some(Ok(plan)) => plan,
            Some(Err(_)) => {
                cx.notify();
                return;
            }
            None => match dataset.plan() {
                Some(plan) => *plan,
                None => return,
            },
        };
        if self.previewed == Some(plan) {
            cx.notify();
            return;
        }
        self.previewed = Some(plan);
        self.loading = true;
        self.generation += 1;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let preview = cx
                .background_spawn(async move { dataset.preview(&plan) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                this.preview = preview;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Emits what Apply does: re-detect with a changed delimiter or
    /// encoding (the plan's fields then don't matter), or apply the draft
    /// if it is a valid plan.
    pub fn apply(&mut self, cx: &mut Context<Self>) {
        if self.redetects().is_some() {
            cx.emit(ImportEvent::Redetect(self.draft.reading));
        } else if let Some(Ok(plan)) = self.checked() {
            cx.emit(ImportEvent::Apply(plan, self.draft.reading.decimal));
        }
    }

    fn cancel(&mut self, _: &CancelImport, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ImportEvent::Cancel);
    }

    /// One raw row: number, role tag, then the line (CSV) or its cells.
    fn render_row(
        &self,
        row: &PreviewRow,
        widths: &[f32],
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let palette = theme::palette(cx);
        let muted = cx.theme().muted_foreground;
        let set_aside = matches!(row.role, RowRole::Title | RowRole::Total | RowRole::Note);
        let header = row.role == RowRole::Header;
        let tag = match row.role {
            RowRole::Title => "TITLE",
            RowRole::Header => "HEADER",
            RowRole::Total => "TOTAL",
            RowRole::Note => "NOTE",
            RowRole::Data | RowRole::Blank => "",
        };
        let ink = if set_aside {
            muted.opacity(0.75)
        } else {
            cx.theme().foreground
        };
        let mut filled = row
            .cells
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.trim().is_empty());
        let lone = match (filled.next(), filled.next()) {
            (Some(cell), None) => Some(cell),
            _ => None,
        };
        let content = match (&row.line, lone) {
            (Some(line), _) => div()
                .pr(px(16.))
                .whitespace_nowrap()
                .child(SharedString::from(line.clone())),
            // A lone cell, like a title, runs on over the empty cells to its right.
            (None, Some((col, text))) => h_flex()
                .pl(px(widths.iter().take(col).sum::<f32>()))
                .pr(px(16.))
                .whitespace_nowrap()
                .child(SharedString::from(text.clone())),
            (None, None) => h_flex().children(widths.iter().enumerate().map(|(col, width)| {
                div()
                    .w(px(*width))
                    .flex_shrink_0()
                    .pr(px(12.))
                    .truncate()
                    .child(SharedString::from(
                        row.cells.get(col).cloned().unwrap_or_default(),
                    ))
            })),
        };
        let number = row.number;
        h_flex()
            .id(("raw-row", number))
            .w_full()
            .h(px(24.))
            .flex_shrink_0()
            .items_center()
            .cursor_pointer()
            .text_color(ink)
            .map(|this| match row.role {
                RowRole::Header => this
                    .bg(palette.accent.opacity(0.14))
                    .border_b_1()
                    .border_color(palette.accent.opacity(0.7))
                    .font_weight(FontWeight::SEMIBOLD),
                RowRole::Data => this.bg(palette.accent.opacity(0.05)),
                _ => this,
            })
            .hover(|s| s.bg(cx.theme().secondary_hover))
            .on_click(cx.listener(move |this, _, window, cx| this.pick_header(number, window, cx)))
            .child(
                div()
                    .w(px(52.))
                    .h_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_end()
                    .pr(px(10.))
                    .bg(palette.gutter)
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .font_features(tabular())
                    .text_color(if header { palette.accent } else { muted })
                    .child(SharedString::from(number.to_string())),
            )
            .child(
                div()
                    .w(px(64.))
                    .pl(px(10.))
                    .flex_shrink_0()
                    .text_size(px(10.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if header {
                        palette.accent
                    } else {
                        muted.opacity(0.8)
                    })
                    .child(tag),
            )
            .child(content)
    }

    /// The raw file, with a "N more rows" line where the preview skips rows.
    fn render_raw(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(preview) = &self.preview else {
            return div().flex_1().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        // Sheet cells get a column each, as wide as their longest text
        // (capped). Rows with one cell, like titles, overflow instead.
        let columns = preview
            .rows
            .iter()
            .map(|r| r.cells.len())
            .max()
            .unwrap_or(0);
        let gridded: Vec<&PreviewRow> = preview
            .rows
            .iter()
            .filter(|r| r.cells.iter().filter(|c| !c.trim().is_empty()).count() > 1)
            .collect();
        let widths: Vec<f32> = if self.is_csv() {
            Vec::new()
        } else {
            (0..columns)
                .map(|col| {
                    let chars = gridded
                        .iter()
                        .filter_map(|r| r.cells.get(col))
                        .map(|c| c.chars().count())
                        .max()
                        .unwrap_or(0);
                    chars.clamp(2, 28) as f32 * 7.4 + 12.0
                })
                .collect()
        };
        let mut children: Vec<AnyElement> = Vec::new();
        let mut next = 1;
        for row in &preview.rows {
            if row.number > next {
                let skipped = (row.number - next) as u64;
                children.push(
                    div()
                        .h(px(24.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .pl(px(116.))
                        .border_y_1()
                        .border_color(cx.theme().border)
                        .italic()
                        .text_color(muted)
                        .child(SharedString::from(format!(
                            "{} more {}s",
                            group_digits(skipped),
                            self.line_word()
                        )))
                        .into_any_element(),
                );
            }
            next = row.number + 1;
            children.push(self.render_row(row, &widths, cx).into_any_element());
        }
        div()
            .id("raw")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_scroll()
            .track_scroll(&self.scroll)
            .child(
                v_flex()
                    .min_w(relative(1.))
                    .pb(px(12.))
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(px(12.))
                    .children(children),
            )
            .into_any_element()
    }

    /// A row of choices, one selected, styled like the filter menu's operators.
    fn choices<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: &[(T, SharedString)],
        current: T,
        pick: fn(&mut Self, T, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> Div {
        let accent = theme::palette(cx).accent;
        h_flex()
            .flex_wrap()
            .gap(px(4.))
            .children(options.iter().enumerate().map(|(ix, (value, label))| {
                let value = *value;
                let selected = value == current;
                div()
                    .id((id, ix))
                    .px(px(7.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .text_xs()
                    .cursor_pointer()
                    .border_1()
                    .border_color(if selected { accent } else { cx.theme().border })
                    .when(selected, |this| {
                        this.bg(accent.opacity(0.12)).text_color(accent)
                    })
                    .when(!selected, |this| {
                        this.hover(|s| s.bg(cx.theme().secondary_hover))
                    })
                    .child(label.clone())
                    .on_click(cx.listener(move |this, _, _, cx| pick(this, value, cx)))
            }))
    }

    fn render_rail(&self, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let field = |label: &'static str, input: AnyElement| {
            h_flex()
                .w_full()
                .gap(px(10.))
                .items_center()
                .child(
                    div()
                        .w(px(64.))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(input))
        };

        let checked = self.checked();
        let redetects = self.redetects();
        let (hint, error) = match (&checked, redetects) {
            (Some(Err(message)), _) => (message.clone(), true),
            (_, Some(what)) => (
                format!(
                    "The {what} changed, so Apply reads the file again and finds the table anew."
                ),
                false,
            ),
            _ => (
                format!(
                    "Click a {} to make it the header. Data starts on the next one.",
                    self.line_word()
                ),
                false,
            ),
        };
        // Re-detecting finds everything anew, so the plan's fields needn't be valid.
        let can_apply = redetects.is_some() || matches!(checked, Some(Ok(_)));

        let dataset = self.dataset.as_ref();
        let delimiters = labelled(
            [
                Delimiter::Comma,
                Delimiter::Semicolon,
                Delimiter::Tab,
                Delimiter::Pipe,
            ],
            Delimiter::Auto,
            dataset.and_then(|d| d.delimiter()),
            delimiter_name,
        );
        let encoding = dataset.and_then(|d| d.encoding());
        let encodings = labelled(
            [Encoding::Utf8, Encoding::Utf16, Encoding::Windows1252],
            Encoding::Auto,
            encoding,
            Encoding::label,
        );
        let decimals = labelled(
            [Decimal::Point, Decimal::Comma],
            Decimal::Auto,
            dataset.map(|d| d.decimal()),
            decimal_name,
        );

        v_flex()
            .w(px(STATS_WIDTH))
            .flex_shrink_0()
            .h_full()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .h(px(34.))
                    .px(px(12.))
                    .flex_shrink_0()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(muted)
                            .child("IMPORT"),
                    ),
            )
            .child(
                v_flex()
                    .p(px(12.))
                    .gap(px(10.))
                    .child(field(
                        "Header",
                        Input::new(&self.header).small().into_any_element(),
                    ))
                    .child(field(
                        "Data",
                        h_flex()
                            .gap(px(6.))
                            .items_center()
                            .child(div().flex_1().child(Input::new(&self.first).small()))
                            .child(div().text_xs().text_color(muted).child("to"))
                            .child(div().flex_1().child(Input::new(&self.last).small()))
                            .into_any_element(),
                    ))
                    .when(self.is_csv(), |this| {
                        this.child(field(
                            "Delimiter",
                            self.choices(
                                "delimiter",
                                &delimiters,
                                self.draft.reading.delimiter,
                                Self::set_delimiter,
                                cx,
                            )
                            .into_any_element(),
                        ))
                    })
                    .when(encoding.is_some(), |this| {
                        this.child(field(
                            "Encoding",
                            self.choices(
                                "encoding",
                                &encodings,
                                self.draft.reading.encoding,
                                Self::set_encoding,
                                cx,
                            )
                            .into_any_element(),
                        ))
                    })
                    .child(field(
                        "Decimal",
                        self.choices(
                            "decimal",
                            &decimals,
                            self.draft.reading.decimal,
                            Self::set_decimal,
                            cx,
                        )
                        .into_any_element(),
                    ))
                    .child(field(
                        "Totals",
                        self.choices(
                            "totals",
                            &[(false, "Set aside".into()), (true, "Keep".into())],
                            self.draft.keep_totals,
                            Self::set_keep_totals,
                            cx,
                        )
                        .into_any_element(),
                    ))
                    .child(
                        div()
                            .pt(px(2.))
                            .text_xs()
                            .text_color(if error { cx.theme().danger } else { muted })
                            .child(SharedString::from(hint)),
                    )
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .child(
                                Button::new("import-cancel")
                                    .ghost()
                                    .small()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel(&CancelImport, window, cx)
                                    })),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("import-reset")
                                    .outline()
                                    .small()
                                    .label("Reset to detected")
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(ImportEvent::Reset)),
                                    ),
                            )
                            .child(
                                Button::new("import-apply")
                                    .primary()
                                    .small()
                                    .label("Apply")
                                    .disabled(!can_apply)
                                    .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                            ),
                    ),
            )
    }
}

/// A field's choices for the rail: `auto` first, labelled with what was
/// detected, like `Auto · Comma` (the name's first word, leaving out an
/// example like `1,234.56`), then each of `choices` by `name`.
fn labelled<T: Copy, const N: usize>(
    choices: [T; N],
    auto: T,
    detected: Option<T>,
    name: fn(T) -> &'static str,
) -> Vec<(T, SharedString)> {
    let auto_label = match detected {
        Some(detected) => format!("Auto · {}", name(detected).split(' ').next().unwrap_or("")),
        None => "Auto".into(),
    };
    std::iter::once((auto, auto_label.into()))
        .chain(choices.map(|c| (c, name(c).into())))
        .collect()
}

/// The decimal mark's name, with an example, as the rail lists it.
fn decimal_name(decimal: Decimal) -> &'static str {
    match decimal {
        Decimal::Auto => "Auto",
        Decimal::Point => "Point 1,234.56",
        Decimal::Comma => "Comma 1.234,56",
    }
}

/// The delimiter's name, as the rail lists it.
fn delimiter_name(delimiter: Delimiter) -> &'static str {
    match delimiter {
        Delimiter::Auto => "Auto",
        Delimiter::Comma => "Comma",
        Delimiter::Semicolon => "Semicolon",
        Delimiter::Tab => "Tab",
        Delimiter::Pipe => "Pipe",
    }
}

impl EventEmitter<ImportEvent> for ImportPanel {}

impl Focusable for ImportPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ImportPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("import-panel")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(self.render_raw(cx))
            .child(self.render_rail(cx))
    }
}

#[cfg(test)]
mod tests {
    use da_core::{ImportPlan, Rows};

    use super::{Draft, Reading};

    fn plan(header: Option<(usize, usize)>, data: (usize, usize)) -> ImportPlan {
        ImportPlan {
            header: header.map(|(first, last)| Rows { first, last }),
            data: Rows {
                first: data.0,
                last: data.1,
            },
            keep_totals: false,
        }
    }

    #[test]
    fn shows_the_plan_and_reads_it_back() {
        for p in [
            plan(Some((5, 5)), (6, 45)),
            plan(Some((1, 3)), (4, 23)),
            plan(None, (1, 501)),
        ] {
            let draft = Draft::new(&p, Reading::default());
            assert_eq!(draft.plan(501, "line"), Ok(p));
        }
        assert_eq!(
            Draft::new(&plan(Some((1, 3)), (4, 23)), Reading::default()).header,
            "1 to 3"
        );
    }

    #[test]
    fn clicking_a_row_makes_it_the_header() {
        let mut draft = Draft::new(&plan(Some((5, 5)), (6, 45)), Reading::default());
        draft.pick_header(2, 48);
        assert_eq!(draft.plan(48, "line"), Ok(plan(Some((2, 2)), (3, 45))));
        // At or below the end of the data, the data runs to the last line.
        for (row, from) in [(45, 46), (47, 48)] {
            let mut draft = Draft::new(&plan(Some((5, 5)), (6, 45)), Reading::default());
            draft.pick_header(row, 48);
            assert_eq!(
                draft.plan(48, "line"),
                Ok(plan(Some((row, row)), (from, 48)))
            );
        }
        // The second table's header, clicked from the first (lines 1 to 5 of 12).
        let mut draft = Draft::new(&plan(Some((1, 1)), (2, 5)), Reading::default());
        draft.pick_header(7, 12);
        assert_eq!(draft.plan(12, "line"), Ok(plan(Some((7, 7)), (8, 12))));
        // The last line leaves no room for data.
        draft.pick_header(48, 48);
        assert_eq!(
            draft.plan(48, "line"),
            Err("Data must start before it ends.".into())
        );
    }

    #[test]
    fn explains_what_is_wrong() {
        let draft = |header: &str, first: &str, last: &str| Draft {
            header: header.into(),
            first: first.into(),
            last: last.into(),
            reading: Reading::default(),
            keep_totals: false,
        };
        let check = |d: Draft| d.plan(48, "line").unwrap_err();
        assert_eq!(
            check(draft("five", "6", "45")),
            "Header must be a line number, like 5, or None."
        );
        assert_eq!(check(draft("5", "6", "")), "Data must be two line numbers.");
        assert_eq!(check(draft("5", "6", "60")), "The file has lines 1 to 48.");
        assert_eq!(check(draft("0", "6", "45")), "The file has lines 1 to 48.");
        assert_eq!(
            check(draft("10", "6", "45")),
            "The header must come before the data."
        );
        assert_eq!(
            check(draft("3 to 1", "6", "45")),
            "Header must be a line number, like 5, or None."
        );
        assert_eq!(
            draft("", "1", "48").plan(48, "line"),
            Ok(plan(None, (1, 48)))
        );
        assert_eq!(
            draft("1-2", "3", "48").plan(48, "row"),
            Ok(plan(Some((1, 2)), (3, 48)))
        );
    }
}
