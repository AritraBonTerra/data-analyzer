//! The Details popover off the import strip: one row per column whose values
//! are shown differently from how the file was loaded, with what it was read
//! as and why. A date column whose day/month order is a guess, or was chosen
//! by the user, gets one click to read it the other way; a column whose
//! decimal mark is a guess points at the Decimal field in Adjust import.

use std::collections::BTreeMap;

use da_core::{Conversion, Guessed, ReadAs};
use gpui_kit::component::{ActiveTheme, ThemeStyled, h_flex, v_flex};
use gpui_kit::*;

use crate::document::Document;
use crate::theme;

/// The hint for a column whose decimal mark the engine guessed.
const DECIMAL_HINT: &str = "Decimal in Adjust import changes it.";

/// What a column was read as, as Details names it.
fn kind_name(read_as: ReadAs) -> &'static str {
    match read_as {
        ReadAs::Number => "Number",
        ReadAs::MonthFirstDate | ReadAs::DayFirstDate => "Date",
        ReadAs::YesNo => "Yes/No",
        ReadAs::Auto | ReadAs::Text => "Text",
    }
}

/// The other day/month order for a converted date column, with its action's
/// label, when the order is open to question: the engine guessed it, or the
/// user chose it (`chosen`, the column's Read as choice).
fn flip(conversion: &Conversion, chosen: Option<ReadAs>) -> Option<(ReadAs, &'static str)> {
    let open = conversion.guessed == Some(Guessed::DateOrder)
        || matches!(chosen, Some(ReadAs::MonthFirstDate | ReadAs::DayFirstDate));
    match conversion.read_as {
        ReadAs::MonthFirstDate if open => Some((ReadAs::DayFirstDate, "Read day first")),
        ReadAs::DayFirstDate if open => Some((ReadAs::MonthFirstDate, "Read month first")),
        _ => None,
    }
}

/// The popover, anchored at `position`. `types` holds the user's Read as
/// choices by column name.
pub fn render(
    conversions: &[Conversion],
    types: &BTreeMap<String, ReadAs>,
    position: Point<Pixels>,
    cx: &mut Context<Document>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let accent = theme::palette(cx).accent;
    let rows = conversions.iter().map(|conversion| {
        let chosen = types.get(&conversion.column).copied();
        let mut notes = conversion.notes.clone();
        if chosen.is_some() {
            notes.insert(0, "your choice".into());
        }
        if conversion.guessed == Some(Guessed::DecimalMark) {
            notes.push(DECIMAL_HINT.into());
        }
        let action = flip(conversion, chosen).map(|(read_as, label)| {
            let column = conversion.column.clone();
            div()
                .id(SharedString::from(format!("flip-{column}")))
                .flex_shrink_0()
                .text_color(accent)
                .cursor_pointer()
                .hover(|s| s.text_color(cx.theme().foreground))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.read_column_as(column.clone(), read_as, cx)
                }))
        });
        h_flex()
            .gap(px(10.))
            .py(px(4.))
            .items_start()
            .child(
                div()
                    .w(px(120.))
                    .flex_shrink_0()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(conversion.column.clone())),
            )
            .child(
                div()
                    .w(px(52.))
                    .flex_shrink_0()
                    .text_color(muted)
                    .child(kind_name(conversion.read_as)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(muted)
                    .child(SharedString::from(notes.join(" · "))),
            )
            .children(action)
    });
    deferred(
        anchored()
            .position(point(position.x + px(10.), position.y + px(12.)))
            .anchor(Anchor::TopRight)
            .snap_to_window_with_margin(px(8.))
            .child(
                v_flex()
                    .id("details-popover")
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_details(cx)))
                    .w(px(460.))
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .p(px(10.))
                    .popover_style(cx)
                    .text_xs()
                    .text_color(cx.theme().popover_foreground)
                    .child(
                        h_flex()
                            .pb(px(6.))
                            .mb(px(2.))
                            .gap(px(10.))
                            .items_center()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(
                                div()
                                    .flex_1()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Details"),
                            )
                            .child(
                                div()
                                    .text_color(muted)
                                    .child("Columns read differently from the file's text"),
                            ),
                    )
                    .children(rows),
            ),
    )
    .with_priority(2)
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use da_core::{Conversion, Guessed, ReadAs};

    use super::flip;

    fn date(read_as: ReadAs, guessed: Option<Guessed>) -> Conversion {
        Conversion {
            column: "when".into(),
            read_as,
            // Whatever the notes say: the app goes by `guessed`.
            notes: vec!["any wording".into()],
            guessed,
        }
    }

    #[test]
    fn offers_the_other_order_when_it_is_open_to_question() {
        let guessed = date(ReadAs::MonthFirstDate, Some(Guessed::DateOrder));
        assert_eq!(
            flip(&guessed, None),
            Some((ReadAs::DayFirstDate, "Read day first"))
        );
        // After the flip the engine no longer guesses, but the user chose.
        let flipped = date(ReadAs::DayFirstDate, None);
        assert_eq!(
            flip(&flipped, Some(ReadAs::DayFirstDate)),
            Some((ReadAs::MonthFirstDate, "Read month first"))
        );
        // Settled by the values: nothing to offer.
        assert_eq!(flip(&date(ReadAs::DayFirstDate, None), None), None);
        assert_eq!(
            flip(&date(ReadAs::Number, Some(Guessed::DecimalMark)), None),
            None
        );
        assert_eq!(
            flip(&date(ReadAs::Number, None), Some(ReadAs::Number)),
            None
        );
    }
}
