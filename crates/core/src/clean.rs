//! Reading values the way people write them. Once the table is loaded, each
//! text column is checked: when every value in it, leaving out placeholders
//! such as `N/A`, reads as a number (`$1,234.00`, `12.6%`, `1.424,50`), a
//! date (`Jul 8, 2025`, `24/03/2025`) or yes/no, the `data` view converts it.
//! The rule is all or nothing per column: one misfit and the column stays
//! text, untouched. The user can force a type instead ([`ReadAs`]), which
//! converts what fits and leaves the rest empty.
//!
//! Deciding is cheap: one read of a few thousand rows, classified here in
//! Rust, and only for the columns that sample fits, one SQL pass that checks
//! the whole column with the very expression that will convert it. A column
//! of ordinary text costs the sample read and nothing more.
//!
//! Nothing is copied: `data` is a view with one expression per converted
//! column over the loaded table, so changing a column's type replaces the view.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use duckdb::Connection;
use duckdb::types::Value;

use crate::dataset::{Column, describe};
use crate::sql::{quote_ident, quote_str};

/// Which character marks decimals in the file's numbers; the other one
/// groups thousands. `Auto` reads it from the values: a column of `1,75` or
/// `1.424,50` says comma, and columns that could be either follow the file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Decimal {
    #[default]
    Auto,
    Point,
    Comma,
}

/// How to read one column. `Auto` converts a text column only when every
/// value fits one type; the others force that type, converting what fits
/// and leaving the rest empty. `Text` shows the column as loaded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReadAs {
    #[default]
    Auto,
    Text,
    Number,
    /// Dates, reading `03/04/2025` as March 4.
    MonthFirstDate,
    /// Dates, reading `03/04/2025` as April 3.
    DayFirstDate,
    /// `Yes`, `No`, `Y`, `N`, `true`, `false`, in any case.
    YesNo,
}

/// How values are read: the file's decimal mark and the type chosen for
/// columns, by name. Columns not listed are `Auto`. Pass it in
/// [`crate::OpenOptions::values`], or change it on an open dataset with
/// [`crate::Dataset::set_values`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValueOptions {
    pub decimal: Decimal,
    pub types: BTreeMap<String, ReadAs>,
}

/// A column whose values `data` shows differently from the loaded table:
/// converted to another type, or trimmed of spaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversion {
    pub column: String,
    /// What it became. A trimmed text column is [`ReadAs::Text`].
    pub read_as: ReadAs,
    /// Short notes for the user, like `decimal comma`, `read day first` or
    /// `N/A and TBD read as empty`.
    pub notes: Vec<String>,
    /// What the values left open and the cleaner chose, if anything. The
    /// notes say it in words; this is for pointing at the way to change it.
    pub guessed: Option<Guessed>,
}

/// A choice the values of a column left open, made by the cleaner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guessed {
    /// Whether `03/04/2025` is March 4 or April 3: no day above 12 settles
    /// it. Change it by reading the column as the other [`ReadAs`] date.
    DateOrder,
    /// Whether the comma in `1,125` groups thousands or marks decimals.
    /// Change it with [`ValueOptions::decimal`].
    DecimalMark,
}

/// How many of a column's values a type reads. Empty cells and
/// placeholders such as `N/A` are not values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fit {
    pub values: u64,
    pub fit: u64,
}

/// How a converted number column shows its values, as the file wrote them.
/// Sorting, filters, stats and SQL see the plain number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberFormat {
    /// `$1,234.00`, and `-$1,234.00` below zero.
    Currency { symbol: char, decimals: u8 },
    /// A fraction shown as a percentage: 0.126 is `12.6%`.
    Percent { decimals: u8 },
}

impl NumberFormat {
    /// The value as this format writes it.
    pub fn render(self, value: f64) -> String {
        let (body, prefix, suffix) = match self {
            Self::Currency { symbol, decimals } => (
                group_thousands(&format!("{:.*}", usize::from(decimals), value.abs())),
                symbol.to_string(),
                "",
            ),
            Self::Percent { decimals } => (
                format!("{:.*}", usize::from(decimals), value.abs() * 100.0),
                String::new(),
                "%",
            ),
        };
        // No sign on a value that rounds to zero.
        let sign = if value < 0.0 && body.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
            "-"
        } else {
            ""
        };
        format!("{sign}{prefix}{body}{suffix}")
    }

    /// Reads text typed in this format back to the number, with or without
    /// its marks: in a percent column `10` and `10%` are both 0.1, and in a
    /// currency column `$1,000`, `1,000` and `1000` are all 1000. `None`
    /// when it isn't a number, or is written in another currency.
    pub fn parse(self, text: &str) -> Option<f64> {
        let written = read_number(text, Mark::Point)?;
        match self {
            Self::Currency { symbol, .. } => (!written.percent
                && written.symbols <= 1
                && written.symbol.is_none_or(|s| s == symbol))
            .then_some(written.value),
            Self::Percent { .. } => written.symbol.is_none().then(|| written.value / 100.0),
        }
    }
}

/// `1234567.50` as `1,234,567.50`.
fn group_thousands(plain: &str) -> String {
    let (whole, fraction) = plain.split_at(plain.find('.').unwrap_or(plain.len()));
    let mut out = String::new();
    for (i, digit) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out + fraction
}

// --- Reading one value ----------------------------------------------------

/// Spaces trimmed from both ends of a value, here and in SQL alike.
const SPACES: &[char] = &[' ', '\t', '\r', '\n', '\u{a0}'];
/// Currency symbols read in front of a number.
const SYMBOLS: [char; 4] = ['$', '€', '£', '¥'];
/// Placeholder words for a missing value, lowercase, in the order notes list them.
const PLACEHOLDERS: [&str; 8] = ["n/a", "na", "-", "--", "tbd", "null", "none", "#n/a"];
const YES: [&str; 3] = ["yes", "y", "true"];
const NO: [&str; 3] = ["no", "n", "false"];
/// The most significant digits a number may be written with to convert,
/// counting from the first non-zero digit, trailing zeros included. A double
/// holds any 15 exactly; more is an id, or an amount whose last digits it
/// would lose. A column shown with a format pads every value to the same
/// decimals, so there the limit holds for each value as shown.
const MAX_DIGITS: usize = 15;
/// The significant digits [`crate::Dataset::fetch`] shows a double with when
/// the column has no [`NumberFormat`]. A column read from text with more
/// would show a value other than the one stored, unless it is whole and
/// stored as an integer.
const SHOWN_DIGITS: usize = 12;

/// A value that stands for a missing one: blank, `N/A`, `-`, `TBD`, `null`,
/// or a spreadsheet error like `#DIV/0!`. Neither a label nor a value.
pub(crate) fn is_placeholder(text: &str) -> bool {
    let text = text.trim().to_lowercase();
    text.is_empty()
        || (text.starts_with('#') && text.ends_with(['!', '?']))
        || PLACEHOLDERS.contains(&text.as_str())
}

/// Which character marks decimals; the other one groups thousands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mark {
    #[default]
    Point,
    Comma,
}

impl Mark {
    fn chars(self) -> (char, char) {
        match self {
            Self::Point => ('.', ','),
            Self::Comma => (',', '.'),
        }
    }
}

/// A number as written, taken apart.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Written {
    /// The value as written: `12.6%` is 12.6, `(4.2%)` is -4.2.
    pub value: f64,
    /// The currency symbol in front, and how many there were (`$$5` has 2).
    pub symbol: Option<char>,
    pub symbols: usize,
    pub percent: bool,
    /// Written with an exponent: `1.5e3`.
    pub exponent: bool,
    /// Significant digits as written, before any exponent: `0.0120` has 3.
    pub digits: usize,
    /// Digits after the decimal mark, before any exponent: `0.0120` has 4.
    pub decimals: usize,
    /// More than one digit before the decimal mark, the first a zero: `007`.
    pub leading_zero: bool,
}

/// Reads a number as people write it: `1234`, `-1.5`, `$1,234.00`,
/// `12.6%`, `(4.2%)`, `1.5e3`, and with [`Mark::Comma`], `1.424,50`.
/// Separators group thousands only in groups of three.
pub(crate) fn read_number(text: &str, mark: Mark) -> Option<Written> {
    let mut text = text.trim();
    let parens = text.starts_with('(') && text.ends_with(')');
    if parens {
        text = text[1..text.len() - 1].trim();
    }
    let (text, percent) = match text.strip_suffix('%') {
        Some(rest) => (rest.trim_end(), true),
        None => (text, false),
    };
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let unsigned = text.trim_start_matches(SYMBOLS);
    let symbols = &text[..text.len() - unsigned.len()];
    let text = unsigned.trim_start();
    let (decimal, group) = mark.chars();
    if parens && negative || !text.starts_with(|c: char| c.is_ascii_digit() || c == decimal) {
        return None;
    }
    let whole = text.split(decimal).next().unwrap_or(text);
    let fraction = &text[whole.len()..];
    if fraction.contains(group) {
        return None;
    }
    if whole.contains(group) {
        let mut groups = whole.split(group);
        let first = groups.next().unwrap_or("");
        if !(1..=3).contains(&first.len()) || groups.any(|g| g.len() != 3) {
            return None;
        }
    }
    let digits = whole.replace(group, "");
    let plain = format!("{digits}{}", fraction.replacen(decimal, ".", 1));
    let exponent = plain.contains(['e', 'E']);
    if (exponent && mark == Mark::Comma) || !plain.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: f64 = plain.parse().ok()?;
    let mantissa = plain.split(['e', 'E']).next().unwrap_or("");
    Some(Written {
        value: if parens || negative { -n } else { n },
        symbol: symbols.chars().next(),
        symbols: symbols.chars().count(),
        percent,
        exponent,
        digits: mantissa.replace('.', "").trim_start_matches('0').len(),
        decimals: mantissa.split('.').nth(1).map_or(0, str::len),
        leading_zero: digits.len() > 1 && digits.starts_with('0'),
    })
}

/// How a date is spelled, which says the `strptime` format that reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Spelling {
    /// Year first, `2025-07-08` or `2025/7/8`, by separator.
    Iso(char),
    /// Day and month as numbers, `07/08/2025` or `8.7.2025`, by separator.
    /// Which comes first is the column's order.
    Numbers(char),
    /// Like `Numbers` with a two-digit year, `07/08/25`, read as DuckDB's
    /// `%y` reads it: 00 to 68 are 2000 to 2068, 69 to 99 are 1969 to 1999.
    ShortYear(char),
    /// With a month name, as its format: `%b %d, %Y` for `Jul 8, 2025`.
    Named(&'static str),
}

/// Every spelling [`read_date`] reads, for a type forced without a sample.
const SPELLINGS: [Spelling; 16] = [
    Spelling::Iso('-'),
    Spelling::Iso('/'),
    Spelling::Numbers('/'),
    Spelling::Numbers('-'),
    Spelling::Numbers('.'),
    Spelling::ShortYear('/'),
    Spelling::ShortYear('-'),
    Spelling::ShortYear('.'),
    Spelling::Named("%b %d, %Y"),
    Spelling::Named("%B %d, %Y"),
    Spelling::Named("%b %d %Y"),
    Spelling::Named("%B %d %Y"),
    Spelling::Named("%d %b %Y"),
    Spelling::Named("%d %B %Y"),
    Spelling::Named("%d-%b-%Y"),
    Spelling::Named("%d-%B-%Y"),
];

impl Spelling {
    /// The `strptime` format, reading numbered days and months in the given order.
    fn format(self, day_first: bool) -> String {
        match self {
            Self::Iso(sep) => format!("%Y{sep}%m{sep}%d"),
            Self::Numbers(sep) if day_first => format!("%d{sep}%m{sep}%Y"),
            Self::Numbers(sep) => format!("%m{sep}%d{sep}%Y"),
            Self::ShortYear(sep) if day_first => format!("%d{sep}%m{sep}%y"),
            Self::ShortYear(sep) => format!("%m{sep}%d{sep}%y"),
            Self::Named(format) => format.into(),
        }
    }

    /// Written with numbers only, so it may take a time of day after it.
    fn numbered(self) -> bool {
        !matches!(self, Self::Named(_))
    }
}

/// A time of day written after a date, by its `strptime` format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Clock {
    /// `10:30`
    Minutes,
    /// `10:30:00`
    Seconds,
    /// `10:30:00.250`, up to microseconds.
    Fraction,
    /// `9:15 AM`
    Minutes12,
    /// `9:15:00 PM`
    Seconds12,
}

/// Every [`Clock`], for a type forced without a sample.
const CLOCKS: [Clock; 5] = [
    Clock::Minutes,
    Clock::Seconds,
    Clock::Fraction,
    Clock::Minutes12,
    Clock::Seconds12,
];

impl Clock {
    fn format(self) -> &'static str {
        match self {
            Self::Minutes => "%H:%M",
            Self::Seconds => "%H:%M:%S",
            Self::Fraction => "%H:%M:%S.%f",
            Self::Minutes12 => "%I:%M %p",
            Self::Seconds12 => "%I:%M:%S %p",
        }
    }
}

/// How a whole value is spelled: a date, and the time of day after it if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Form {
    spelling: Spelling,
    clock: Option<Clock>,
}

impl Form {
    fn format(self, day_first: bool) -> String {
        let date = self.spelling.format(day_first);
        match self.clock {
            Some(clock) => format!("{date} {}", clock.format()),
            None => date,
        }
    }
}

/// Every form [`read_date`] reads, dates alone first, for a type forced
/// without a sample.
fn every_form() -> Vec<Form> {
    let dates = SPELLINGS.map(|spelling| Form {
        spelling,
        clock: None,
    });
    let timed = SPELLINGS
        .into_iter()
        .filter(|s| s.numbered())
        .flat_map(|spelling| {
            CLOCKS.map(|clock| Form {
                spelling,
                clock: Some(clock),
            })
        });
    dates.into_iter().chain(timed).collect()
}

/// A date as written, and which day/month orders it is valid in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WrittenDate {
    form: Form,
    month_first: bool,
    day_first: bool,
}

/// Reads a date in one of the [`SPELLINGS`], with a time of day after it
/// when written with numbers (`03/04/2025 10:30`). Years have two or four
/// digits, so `1/2`, `3-4` and `1.2.3` are not dates.
fn read_date(text: &str) -> Option<WrittenDate> {
    if let Some((date, time)) = text.split_once(' ')
        && time.contains(':')
    {
        let clock = read_clock(time)?;
        let date = read_day(date).filter(|d| d.form.spelling.numbered())?;
        return Some(WrittenDate {
            form: Form {
                clock: Some(clock),
                ..date.form
            },
            ..date
        });
    }
    read_day(text)
}

/// Reads a run of 1 to `len` ASCII digits as a number.
fn digits(part: &str, len: std::ops::RangeInclusive<usize>) -> Option<u32> {
    (len.contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit()))
        .then(|| part.parse().ok())
        .flatten()
}

/// Reads a time of day in one of the [`CLOCKS`].
fn read_clock(text: &str) -> Option<Clock> {
    let (time, twelve) = match text.split_once(' ') {
        Some((time, half)) if ["am", "pm"].contains(&half.to_lowercase().as_str()) => (time, true),
        Some(_) => return None,
        None => (text, false),
    };
    let (time, fraction) = match time.split_once('.') {
        Some((time, fraction)) => (time, Some(digits(fraction, 1..=6)?)),
        None => (time, None),
    };
    let parts: Vec<&str> = time.split(':').collect();
    let hour = digits(parts[0], 1..=2)?;
    let hour_ok = if twelve {
        (1..=12).contains(&hour)
    } else {
        hour <= 23
    };
    let sixty = |part: &str| digits(part, 2..=2).is_some_and(|n| n <= 59);
    if !hour_ok || !parts[1..].iter().all(|p| sixty(p)) {
        return None;
    }
    match (parts.len(), fraction.is_some(), twelve) {
        (2, false, false) => Some(Clock::Minutes),
        (3, false, false) => Some(Clock::Seconds),
        (3, true, false) => Some(Clock::Fraction),
        (2, false, true) => Some(Clock::Minutes12),
        (3, false, true) => Some(Clock::Seconds12),
        _ => None,
    }
}

/// Reads a date alone, in one of the [`SPELLINGS`].
fn read_day(text: &str) -> Option<WrittenDate> {
    let number = digits;
    let fixed = |spelling, valid: bool| {
        valid.then_some(WrittenDate {
            form: Form {
                spelling,
                clock: None,
            },
            month_first: true,
            day_first: true,
        })
    };
    // With numbers: `2025-07-08`, `2025/7/8`, `07/08/2025`, `8.7.2025`,
    // `8-7-2025`, `07/08/25`.
    if let Some(sep) = text.chars().find(|c| !c.is_ascii_digit()) {
        let parts: Vec<&str> = text.split(sep).collect();
        if let [a, b, c] = parts[..] {
            if matches!(sep, '-' | '/')
                && let (Some(y), Some(m), Some(d)) =
                    (number(a, 4..=4), number(b, 1..=2), number(c, 1..=2))
            {
                return fixed(Spelling::Iso(sep), valid_date(y, m, d));
            }
            if matches!(sep, '-' | '/' | '.')
                && let (Some(a), Some(b)) = (number(a, 1..=2), number(b, 1..=2))
            {
                let (spelling, y) = match c.len() {
                    4 => (Spelling::Numbers(sep), number(c, 4..=4)?),
                    // DuckDB's `%y` pivot.
                    2 => match number(c, 2..=2)? {
                        y @ 0..=68 => (Spelling::ShortYear(sep), 2000 + y),
                        y => (Spelling::ShortYear(sep), 1900 + y),
                    },
                    _ => return None,
                };
                let date = WrittenDate {
                    form: Form {
                        spelling,
                        clock: None,
                    },
                    month_first: valid_date(y, a, b),
                    day_first: valid_date(y, b, a),
                };
                return (date.month_first || date.day_first).then_some(date);
            }
        }
    }
    // With a month name: `Jul 8, 2025`, `July 8 2025`, `8 Jul 2025`, `8-Jul-2025`.
    // `%b` reads a three-letter name, `%B` a full one.
    let named = |m: &str, d: &str, y: &str, formats: [&'static str; 2]| {
        let format = formats[usize::from(m.len() != 3)];
        let (m, d, y) = (month(m)?, number(d, 1..=2)?, number(y, 4..=4)?);
        fixed(Spelling::Named(format), valid_date(y, m, d))
    };
    match text.split(' ').collect::<Vec<_>>()[..] {
        [m, d, y] if !m.starts_with(|c: char| c.is_ascii_digit()) => match d.strip_suffix(',') {
            Some(d) => named(m, d, y, ["%b %d, %Y", "%B %d, %Y"]),
            None => named(m, d, y, ["%b %d %Y", "%B %d %Y"]),
        },
        [d, m, y] => named(m, d, y, ["%d %b %Y", "%d %B %Y"]),
        _ => match text.split('-').collect::<Vec<_>>()[..] {
            [d, m, y] => named(m, d, y, ["%d-%b-%Y", "%d-%B-%Y"]),
            _ => None,
        },
    }
}

/// A month's number from its English name or three-letter abbreviation.
fn month(name: &str) -> Option<u32> {
    const NAMES: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let name = name.to_lowercase();
    NAMES
        .iter()
        .position(|full| *full == name || (name.len() == 3 && full.starts_with(&name)))
        .map(|i| i as u32 + 1)
}

/// A real calendar date, in a year from 1000 to 2999: codes like
/// `03-11-7782` have four digits at the end too.
fn valid_date(year: u32, month: u32, day: u32) -> bool {
    if !(1000..=2999).contains(&year) {
        return false;
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

fn yes_no(text: &str) -> Option<bool> {
    let text = text.to_lowercase();
    if YES.contains(&text.as_str()) {
        Some(true)
    } else if NO.contains(&text.as_str()) {
        Some(false)
    } else {
        None
    }
}

// --- Guessing a column's type from a sample -------------------------------

/// What a column's sampled values say about how it is written in one
/// decimal convention, when every one of them reads as a number there.
#[derive(Clone, Copy, Debug, PartialEq)]
struct NumberStyle {
    symbol: Option<char>,
    percent: bool,
}

/// What a column's sample looks like.
#[derive(Clone, Debug, PartialEq)]
enum Guess {
    /// Numbers, read with a decimal point and with a decimal comma: `None`
    /// where some value does not read. `same` when both read every value
    /// alike. `ambiguous` when both read every value but some differently,
    /// with a comma and no point in sight: `1,125` is 1125 or 1.125.
    Number {
        point: Option<NumberStyle>,
        comma: Option<NumberStyle>,
        same: bool,
        ambiguous: bool,
    },
    /// Dates. `month_first` and `day_first` say which orders every value
    /// is valid in; `numbers` whether any is written with numbers only.
    Date {
        month_first: bool,
        day_first: bool,
        numbers: bool,
        dotted: bool,
        /// The forms seen, the most common first.
        forms: Vec<Form>,
    },
    YesNo,
    /// Text with spaces around every value.
    Padded,
}

/// Reads a column's sampled values (non-null, as loaded). `None` when it
/// is plain text, or holds nothing but placeholders.
fn guess(sample: &[String]) -> Option<Guess> {
    let values: Vec<&str> = sample
        .iter()
        .map(|v| v.trim_matches(SPACES))
        .filter(|v| !is_placeholder(v))
        .collect();
    if values.is_empty() {
        return None;
    }
    if values.iter().all(|v| yes_no(v).is_some()) {
        return Some(Guess::YesNo);
    }
    let (point, comma) = (style(&values, Mark::Point), style(&values, Mark::Comma));
    if point.is_some() || comma.is_some() {
        let same = values.iter().all(|v| {
            read_number(v, Mark::Point).map(|w| w.value)
                == read_number(v, Mark::Comma).map(|w| w.value)
        });
        let ambiguous =
            point.is_some() && comma.is_some() && !same && !values.iter().any(|v| v.contains('.'));
        return Some(Guess::Number {
            point,
            comma,
            same,
            ambiguous,
        });
    }
    let dates: Option<Vec<WrittenDate>> = values.iter().map(|v| read_date(v)).collect();
    if let Some(dates) = dates {
        let month_first = dates.iter().all(|d| d.month_first);
        let day_first = dates.iter().all(|d| d.day_first);
        let numbered = || {
            dates.iter().filter_map(|d| match d.form.spelling {
                Spelling::Numbers(sep) | Spelling::ShortYear(sep) => Some(sep),
                _ => None,
            })
        };
        if month_first || day_first {
            let mut counts: HashMap<Form, usize> = HashMap::new();
            for date in &dates {
                *counts.entry(date.form).or_default() += 1;
            }
            let mut forms: Vec<(Form, usize)> = counts.into_iter().collect();
            forms.sort_by_key(|&(form, n)| {
                (
                    std::cmp::Reverse(n),
                    SPELLINGS.iter().position(|s| *s == form.spelling),
                    form.clock.and_then(|c| CLOCKS.iter().position(|k| *k == c)),
                )
            });
            return Some(Guess::Date {
                month_first,
                day_first,
                numbers: numbered().next().is_some(),
                dotted: numbered().all(|sep| sep == '.'),
                forms: forms.into_iter().map(|(form, _)| form).collect(),
            });
        }
    }
    // Padding on every value is how the file was laid out (`a, item 1 ,`);
    // on a few it is meant, like an indented line.
    let mut filled = sample.iter().filter(|v| !v.trim_matches(SPACES).is_empty());
    let padded = |v: &&String| v.trim_matches(SPACES).len() != v.len();
    (filled.clone().next().is_some() && filled.all(|v| padded(&v))).then_some(Guess::Padded)
}

/// How `values` are written as numbers with decimal `mark`, or `None` when
/// one of them is not a number to convert there: not a number, an id with a
/// leading zero or too many digits, too big for a double, another currency,
/// or a percent where the others are not. A column with a currency or
/// percent sign also takes no exponents (`$1e-3`), and no value that,
/// padded to the column's decimals, has too many digits to show exactly.
/// A column with neither shows its values as plain doubles do, so it takes
/// more than [`SHOWN_DIGITS`] only when every value is whole (an integer
/// column shows every digit). [`clean`] checks the whole column by the same
/// rules in SQL.
fn style(values: &[&str], mark: Mark) -> Option<NumberStyle> {
    let mut style: Option<NumberStyle> = None;
    let (mut exponent, mut decimals, mut widest) = (false, 0, isize::MIN);
    let (mut most_digits, mut whole) = (0, true);
    for value in values {
        let w = read_number(value, mark)?;
        if w.symbols > 1 || w.leading_zero || w.digits > MAX_DIGITS || !w.value.is_finite() {
            return None;
        }
        most_digits = most_digits.max(w.digits);
        whole &= w.value.fract() == 0.0;
        exponent |= w.exponent;
        decimals = decimals.max(w.decimals);
        // Digits shown before the decimals, or less than zero for leading zeros after it.
        widest = widest.max(w.digits as isize - w.decimals as isize);
        let s = style.get_or_insert(NumberStyle {
            symbol: w.symbol,
            percent: w.percent,
        });
        if s.percent != w.percent
            || (w.symbol.is_some() && s.symbol.is_some_and(|c| Some(c) != w.symbol))
        {
            return None;
        }
        s.symbol = s.symbol.or(w.symbol);
    }
    let formatted = style.is_some_and(|s| s.symbol.is_some() || s.percent);
    if formatted && (exponent || !shows_exactly(widest, decimals)) {
        return None;
    }
    if !formatted && most_digits > SHOWN_DIGITS && !whole {
        return None;
    }
    style
}

/// Whether a formatted column shows every value exactly: `widest` is the
/// most of any value's significant digits less its decimals, and every
/// value is shown with `decimals`.
fn shows_exactly(widest: isize, decimals: usize) -> bool {
    widest.saturating_add(decimals as isize) <= MAX_DIGITS as isize
}

// --- SQL --------------------------------------------------------------------

/// The `try_strptime` formats for `forms`: one, or a list tried in order.
/// Each format a value doesn't match costs, so the column's most common
/// form goes first, except that two-digit years go before four: `%Y`
/// would read `01/05/25` as the year 25.
fn date_formats(forms: &[Form], day_first: bool) -> String {
    let mut forms = forms.to_vec();
    forms.sort_by_key(|f| !matches!(f.spelling, Spelling::ShortYear(_)));
    let formats: Vec<String> = forms
        .iter()
        .map(|f| quote_str(&f.format(day_first)))
        .collect();
    match &formats[..] {
        [one] => one.clone(),
        many => format!("[{}]", many.join(", ")),
    }
}

/// The shape of a date in one of the [`SPELLINGS`], with a time of day
/// after one written with numbers. `try_strptime` alone would also read
/// `3/4/2025` with `%y` as the year 20.
const DATE_SHAPE: &str = r"(\d{4}[-/]\d{1,2}[-/]\d{1,2}|\d{1,2}[-/.]\d{1,2}[-/.](\d{2}|\d{4}))( \d{1,2}:\d{2}(:\d{2}(\.\d{1,6})?)?( [AaPp][Mm])?)?|[A-Za-z]{3,9} \d{1,2},? \d{4}|\d{1,2}[ -][A-Za-z]{3,9}[ -]\d{4}";

/// Whether trimmed text `t` has a time of day in it, for a date column
/// forced without a sample: then it becomes a timestamp.
fn timed_sql(t: &str) -> String {
    format!(r"regexp_matches({t}, '\d:\d\d')")
}

/// The shape of a number `read_number` reads with `mark`. With a `strict`
/// style it is the shape of a number to convert in a column written that
/// way: that currency or none, a percent sign exactly when the style has
/// one, no leading zero, an exponent only when it has neither, and plain
/// spaces only (the fast conversion drops them). The digit limit is
/// [`significant_sql`]'s to check.
fn number_shape(mark: Mark, strict: Option<NumberStyle>) -> String {
    let whole = match (mark, strict.is_some()) {
        (Mark::Point, false) => r"\d{1,3}(,\d{3})+|\d+",
        (Mark::Comma, false) => r"\d{1,3}(\.\d{3})+|\d+",
        (Mark::Point, true) => r"0|[1-9]\d*|[1-9]\d{0,2}(,\d{3})+",
        (Mark::Comma, true) => r"0|[1-9]\d*|[1-9]\d{0,2}(\.\d{3})+",
    };
    let exponent = match strict {
        Some(s) if s.symbol.is_some() || s.percent => "",
        _ => r"([eE][-+]?\d+)?",
    };
    let number = match mark {
        Mark::Point => format!(r"({whole})(\.\d*)?{exponent}|\.\d+{exponent}"),
        Mark::Comma => format!(r"({whole})(,\d*)?|,\d+"),
    };
    let Some(style) = strict else {
        let body = format!(r"[$€£¥]*\s*({number})\s*%?");
        return format!(r"\(\s*{body}\s*\)|[-+]?\s*{body}");
    };
    let symbol = style
        .symbol
        .map_or(String::new(), |c| format!("([{c}] *)?"));
    let percent = if style.percent { " *%" } else { "" };
    let body = format!(r"{symbol}({number}){percent}");
    format!(r"\( *{body} *\)|[-+]? *{body}")
}

/// The significant digits of number text `t`, as [`Written::digits`]
/// counts them: every digit before any exponent, less leading zeros.
fn significant_sql(t: &str) -> String {
    format!(
        "length(ltrim(regexp_replace(regexp_replace({t}, '[eE].*', ''), '[^0-9]', '', 'g'), '0'))"
    )
}

/// The digits after decimal `mark` in number text `t`, as [`Written::decimals`].
fn decimals_sql(t: &str, mark: Mark) -> String {
    let (decimal, _) = mark.chars();
    let pattern = quote_str(&format!(r"\{decimal}(\d*)"));
    format!("length(regexp_extract({t}, {pattern}, 1))")
}

/// A column's value as trimmed text, whatever its type.
fn text(column: &Column) -> String {
    let col = quote_ident(&column.name);
    let value = if column.sql_type == "VARCHAR" {
        col
    } else {
        format!("CAST({col} AS VARCHAR)")
    };
    format!(
        "trim({value}, {})",
        quote_str(&SPACES.iter().collect::<String>())
    )
}

/// Whether trimmed text `t` is blank or a placeholder, as [`is_placeholder`].
fn placeholder_sql(t: &str) -> String {
    let words = PLACEHOLDERS.map(quote_str).join(", ");
    format!(
        "({t} = '' OR lower({t}) IN ({words}) OR (starts_with({t}, '#') AND (ends_with({t}, '!') OR ends_with({t}, '?'))))"
    )
}

/// What a column becomes. A number conversion with a `style`, or a date
/// conversion with `forms`, is for a column whose every value has been
/// checked to fit: it skips the per-value shape check a forced type needs.
#[derive(Clone, Debug, PartialEq)]
enum Convert {
    Number {
        mark: Mark,
        integral: bool,
        style: Option<NumberStyle>,
        /// For a forced type: whether a value with more than [`MAX_DIGITS`]
        /// doesn't fit. Not for a column the file already typed, whose text
        /// is a number's own exact form.
        limit: bool,
    },
    Date {
        day_first: bool,
        forms: Option<Vec<Form>>,
        /// A timestamp, for a column with a time of day; else a date.
        timed: bool,
    },
    YesNo,
}

impl Convert {
    /// The conversion of trimmed text `t`: NULL where it does not fit.
    fn sql(&self, t: &str) -> String {
        match *self {
            Self::Number {
                mark,
                integral,
                style: Some(style),
                ..
            } => {
                // Plain replaces: a placeholder left over casts to NULL.
                let (_, group) = mark.chars();
                let mut digits = t.to_string();
                let strip = [Some(group), Some(' '), Some('('), Some(')'), style.symbol]
                    .into_iter()
                    .chain([style.percent.then_some('%')])
                    .flatten();
                for c in strip {
                    digits = format!("replace({digits}, {}, '')", quote_str(&c.to_string()));
                }
                if mark == Mark::Comma {
                    digits = format!("replace({digits}, ',', '.')");
                }
                let mut number = format!(
                    "(CASE WHEN starts_with({t}, '(') THEN -1 ELSE 1 END) * TRY_CAST({digits} AS DOUBLE)"
                );
                if style.percent {
                    number = format!("{number} / 100");
                }
                if integral {
                    format!("CAST({number} AS BIGINT)")
                } else {
                    number
                }
            }
            Self::Number {
                mark,
                integral,
                style: None,
                limit,
            } => {
                let digits = match mark {
                    Mark::Point => format!("regexp_replace({t}, '[^0-9.eE+-]', '', 'g')"),
                    Mark::Comma => {
                        format!("replace(regexp_replace({t}, '[^0-9,+-]', '', 'g'), ',', '.')")
                    }
                };
                let value = format!(
                    "(CASE WHEN starts_with({t}, '(') THEN -1 ELSE 1 END) * TRY_CAST({digits} AS DOUBLE) \
                     / (CASE WHEN contains({t}, '%') THEN 100 ELSE 1 END)"
                );
                let limit = if limit {
                    format!(" AND {} <= {MAX_DIGITS}", significant_sql(t))
                } else {
                    String::new()
                };
                // Too many digits, or too big for a double, doesn't fit.
                let number = format!(
                    "CASE WHEN regexp_full_match({t}, {shape}){limit} AND isfinite({value}) THEN {value} END",
                    shape = quote_str(&number_shape(mark, None))
                );
                if integral {
                    format!("CAST({number} AS BIGINT)")
                } else {
                    number
                }
            }
            Self::Date {
                day_first,
                ref forms,
                timed,
            } => {
                let formats = date_formats(forms.as_deref().unwrap_or(&every_form()), day_first);
                let mut date = format!("try_strptime({t}, {formats})");
                if !timed {
                    date = format!("CAST({date} AS DATE)");
                }
                match forms {
                    Some(_) => date,
                    None => format!(
                        "CASE WHEN regexp_full_match({t}, {}) THEN {date} END",
                        quote_str(DATE_SHAPE)
                    ),
                }
            }
            Self::YesNo => format!(
                "CASE WHEN lower({t}) IN ({}) THEN true WHEN lower({t}) IN ({}) THEN false END",
                YES.map(quote_str).join(", "),
                NO.map(quote_str).join(", ")
            ),
        }
    }

    fn read_as(&self) -> ReadAs {
        match self {
            Self::Number { .. } => ReadAs::Number,
            Self::Date {
                day_first: true, ..
            } => ReadAs::DayFirstDate,
            Self::Date {
                day_first: false, ..
            } => ReadAs::MonthFirstDate,
            Self::YesNo => ReadAs::YesNo,
        }
    }

    /// The conversion a forced type asks for on `column`; `None` for
    /// `Auto` and `Text`.
    fn forced(read_as: ReadAs, mark: Mark, column: &Column) -> Option<Self> {
        match read_as {
            ReadAs::Auto | ReadAs::Text => None,
            ReadAs::Number => Some(Self::Number {
                mark,
                integral: false,
                style: None,
                limit: column.sql_type == "VARCHAR",
            }),
            ReadAs::MonthFirstDate | ReadAs::DayFirstDate => Some(Self::Date {
                day_first: read_as == ReadAs::DayFirstDate,
                forms: None,
                timed: false,
            }),
            ReadAs::YesNo => Some(Self::YesNo),
        }
    }
}

/// Aggregates gathered into the one checking query, read back by index.
#[derive(Default)]
struct Checks(Vec<String>);

impl Checks {
    fn add(&mut self, aggregate: String) -> usize {
        self.0.push(aggregate);
        self.0.len() - 1
    }

    /// Rows whose trimmed text `t` is a value (not blank, not a
    /// placeholder) and meets `condition`.
    fn values_where(&mut self, t: &str, condition: &str) -> usize {
        self.add(format!(
            "count(*) FILTER (WHERE NOT {} AND ({condition}))",
            placeholder_sql(t)
        ))
    }

    /// Rows where number conversion `number` gives a fraction or a number
    /// too big to be exact as an integer.
    fn fractional(&mut self, number: &str) -> usize {
        self.add(format!(
            "count(*) FILTER (WHERE NOT abs({number}) < 1e15 OR {number} % 1 <> 0)"
        ))
    }

    /// The most digits after decimal `mark` in any value of trimmed text
    /// `t`, so the display shows every value at the precision it was
    /// written with.
    fn decimals(&mut self, t: &str, mark: Mark) -> usize {
        self.add(format!(
            "max({}) FILTER (WHERE NOT {})",
            decimals_sql(t, mark),
            placeholder_sql(t)
        ))
    }

    /// The most significant digits less decimals of any value of trimmed
    /// text `t`, which with the column's decimals says whether a format
    /// shows every value exactly (see [`shows_exactly`]).
    fn widest(&mut self, t: &str, mark: Mark) -> usize {
        self.add(format!(
            "max({} - {}) FILTER (WHERE NOT {})",
            significant_sql(t),
            decimals_sql(t, mark),
            placeholder_sql(t)
        ))
    }

    /// The placeholders in `t`, as written, separated by a unit separator.
    fn placeholders(&mut self, t: &str) -> usize {
        self.add(format!(
            "string_agg(DISTINCT {t}, chr(31)) FILTER (WHERE {t} <> '' AND {})",
            placeholder_sql(t)
        ))
    }

    fn run(&self, conn: &Connection, relation: &str) -> Result<Vec<Value>> {
        if self.0.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!("SELECT {} FROM {relation}", self.0.join(", "));
        Ok(conn.query_row(&sql, [], |row| {
            (0..self.0.len()).map(|i| row.get(i)).collect()
        })?)
    }
}

/// An integer aggregate's result; 0 for NULL.
fn int(results: &[Value], index: usize) -> i64 {
    match results[index] {
        Value::BigInt(n) => n,
        Value::HugeInt(n) => n as i64,
        Value::UBigInt(n) => n as i64,
        _ => 0,
    }
}

fn count(results: &[Value], index: usize) -> u64 {
    int(results, index) as u64
}

/// The placeholders an aggregate found, in the order notes list them.
fn placeholders(results: &[Value], index: usize) -> Vec<String> {
    let Value::Text(text) = &results[index] else {
        return Vec::new();
    };
    let mut words: Vec<String> = text.split('\u{1f}').map(String::from).collect();
    words.sort_by_key(|w| {
        let lower = w.to_lowercase();
        let rank = PLACEHOLDERS
            .iter()
            .position(|p| *p == lower)
            .unwrap_or(PLACEHOLDERS.len());
        (rank, w.clone())
    });
    words
}

// --- Cleaning a table ------------------------------------------------------

/// What the cleaner did to a table: the expressions that make `data` from
/// it, and what to tell the user.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Cleaning {
    /// Column name and the expression that replaces it in `data`.
    exprs: Vec<(String, String)>,
    formats: Vec<(String, NumberFormat)>,
    pub conversions: Vec<Conversion>,
    /// Placeholders read as empty in converted columns, for the summary.
    placeholders: Vec<String>,
    /// The file's decimal mark, as decided, for numbers forced later.
    pub mark: Mark,
}

impl Cleaning {
    /// The `SELECT` that makes `data` from `from`.
    pub fn view(&self, from: &str) -> String {
        if self.exprs.is_empty() {
            return format!("SELECT * FROM {from}");
        }
        let replace = self
            .exprs
            .iter()
            .map(|(name, expr)| format!("{expr} AS {}", quote_ident(name)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("SELECT * REPLACE ({replace}) FROM {from}")
    }

    /// How `column` shows its values, if it was converted from formatted numbers.
    pub fn format(&self, column: &str) -> Option<NumberFormat> {
        self.formats
            .iter()
            .find(|(name, _)| name == column)
            .map(|(_, format)| *format)
    }

    /// One line saying what was converted, like `Read 5 text columns as
    /// numbers. N/A, - and TBD read as empty.` `None` when nothing was.
    pub fn summary(&self) -> Option<String> {
        let count = |read_as: &[ReadAs]| {
            self.conversions
                .iter()
                .filter(|c| read_as.contains(&c.read_as))
                .count()
        };
        let kinds: Vec<(usize, &str)> = [
            (count(&[ReadAs::Number]), "numbers"),
            (
                count(&[ReadAs::MonthFirstDate, ReadAs::DayFirstDate]),
                "dates",
            ),
            (count(&[ReadAs::YesNo]), "yes/no"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .collect();
        let mut sentences = Vec::new();
        if let Some(((first, kind), rest)) = kinds.split_first() {
            let mut items = vec![format!(
                "Read {first} text column{} as {kind}",
                if *first == 1 { "" } else { "s" }
            )];
            items.extend(rest.iter().map(|(n, kind)| format!("{n} as {kind}")));
            sentences.push(format!("{}.", join_and(&items)));
        }
        if !self.placeholders.is_empty() {
            sentences.push(format!("{} read as empty.", join_and(&self.placeholders)));
        }
        (!sentences.is_empty()).then(|| sentences.join(" "))
    }
}

/// `a`, `a and b`, `a, b and c`.
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Rows sampled to guess each text column's type.
const SAMPLE_ROWS: usize = 5000;

/// Decides how to read every column of the table `check` under `options`:
/// guesses from its first rows, then checks every row in one query. `auto`
/// says whether text columns are converted without being asked (not for
/// Parquet). `delimiter` says what separates the file's fields, `None` for
/// a file that isn't delimited text; it is asked only when a column could
/// hold decimal commas.
pub(crate) fn clean(
    conn: &Connection,
    check: &str,
    options: &ValueOptions,
    auto: bool,
    delimiter: &dyn Fn() -> Option<char>,
) -> Result<Cleaning> {
    if !auto && options.types.values().all(|t| *t == ReadAs::Auto) {
        return Ok(Cleaning::default());
    }
    let columns = describe(conn, check)?;
    let chosen = |c: &Column| options.types.get(&c.name).copied().unwrap_or_default();
    let candidates: Vec<&Column> = columns
        .iter()
        .filter(|c| auto && chosen(c) == ReadAs::Auto && c.sql_type == "VARCHAR")
        .collect();
    let mut guesses: Vec<Option<Guess>> = sample(conn, check, &candidates)?
        .iter()
        .map(|values| guess(values))
        .collect();
    // A decimal comma is read without being asked only in a file not split
    // on commas: in one that is, a quoted `"1,2"` is as likely two values.
    // And a column named `a,b` is a line the reader didn't split at all.
    let decimal_commas = guesses.iter().any(|g| {
        matches!(
            g,
            Some(Guess::Number {
                comma: Some(_),
                same: false,
                ..
            })
        )
    });
    let delimiter = {
        let asked = std::cell::OnceCell::new();
        move || *asked.get_or_init(delimiter)
    };
    let commas_allowed = match options.decimal {
        Decimal::Auto => decimal_commas && delimiter() != Some(','),
        Decimal::Point => false,
        Decimal::Comma => true,
    };
    for (guess, column) in guesses.iter_mut().zip(&candidates) {
        if let Some(Guess::Number {
            comma, point, same, ..
        }) = guess
            && !*same
            && (!commas_allowed || column.name.contains(','))
        {
            *comma = None;
            if point.is_none() {
                *guess = None;
            }
        }
    }

    // The file's decimal mark: a comma when some column reads only with one,
    // and none reads only with a point. When no column settles it but some
    // could be either (`1,125`), it is a guess: a comma in a file split on
    // semicolons, where that is the custom, else a point.
    let only = |mark: Mark| {
        guesses.iter().flatten().any(|g| match g {
            Guess::Number {
                point,
                comma,
                ambiguous: false,
                ..
            } => match mark {
                Mark::Point => point.is_some() && comma.is_none(),
                Mark::Comma => comma.is_some() && point.is_none(),
            },
            _ => false,
        })
    };
    let settled = only(Mark::Point) || only(Mark::Comma);
    let ambiguous = guesses.iter().any(|g| {
        matches!(
            g,
            Some(Guess::Number {
                ambiguous: true,
                ..
            })
        )
    });
    let mark = match options.decimal {
        Decimal::Point => Mark::Point,
        Decimal::Comma => Mark::Comma,
        Decimal::Auto if !commas_allowed => Mark::Point,
        Decimal::Auto if only(Mark::Comma) && !only(Mark::Point) => Mark::Comma,
        Decimal::Auto if !settled && ambiguous && delimiter() == Some(';') => Mark::Comma,
        Decimal::Auto => Mark::Point,
    };
    let mark_guessed = options.decimal == Decimal::Auto && !settled;

    let mut checks = Checks::default();
    let mut plans = Vec::new();
    for (column, guess) in candidates.iter().zip(guesses) {
        let Some(guess) = guess else { continue };
        let t = text(column);
        let plan = match guess {
            Guess::Number {
                point,
                comma,
                same,
                ambiguous,
            } => {
                // The file's mark, or the other one if only that reads the column.
                // An explicit decimal mark reads every column its way.
                let (mark, style) = match (mark, point, comma) {
                    (Mark::Point, Some(style), _) => (Mark::Point, style),
                    (Mark::Comma, _, Some(style)) => (Mark::Comma, style),
                    (_, Some(style), _) if options.decimal == Decimal::Auto => (Mark::Point, style),
                    (_, _, Some(style)) if options.decimal == Decimal::Auto => (Mark::Comma, style),
                    _ => continue,
                };
                let shape = quote_str(&number_shape(mark, Some(style)));
                let number = Convert::Number {
                    mark,
                    integral: false,
                    style: Some(style),
                    limit: true,
                }
                .sql(&t);
                // The same rules as `style`, over every row.
                let misfit = format!(
                    "NOT regexp_full_match({t}, {shape}) OR {} > {MAX_DIGITS} OR NOT isfinite({number})",
                    significant_sql(&t)
                );
                let guessed = ambiguous && mark_guessed;
                let mark_note = match (mark, guessed) {
                    (Mark::Comma, true) => Some("decimal comma, a guess"),
                    (Mark::Point, true) => Some("comma read as thousands, a guess"),
                    (Mark::Comma, false) if !same => Some("decimal comma"),
                    _ => None,
                };
                Plan::Number {
                    mark,
                    style,
                    mark_note,
                    guessed,
                    misfits: checks.values_where(&t, &misfit),
                    fractional: checks.fractional(&number),
                    parens: checks.values_where(&t, &format!("starts_with({t}, '(')")),
                    decimals: checks.decimals(&t, mark),
                    widest: checks.widest(&t, mark),
                    digits: checks.add(format!(
                        "max({}) FILTER (WHERE NOT {})",
                        significant_sql(&t),
                        placeholder_sql(&t)
                    )),
                }
            }
            Guess::Date {
                month_first,
                day_first,
                numbers,
                dotted,
                forms,
            } => {
                let shape = quote_str(DATE_SHAPE);
                let mut misfits = |day_first: bool| {
                    let date = Convert::Date {
                        day_first,
                        forms: Some(forms.clone()),
                        timed: false,
                    }
                    .sql(&t);
                    checks.values_where(
                        &t,
                        &format!("NOT regexp_full_match({t}, {shape}) OR {date} IS NULL"),
                    )
                };
                Plan::Date {
                    month_first: month_first.then(|| misfits(false)),
                    day_first: day_first.then(|| misfits(true)),
                    numbers,
                    dotted,
                    forms,
                }
            }
            Guess::YesNo => Plan::YesNo {
                misfits: checks.values_where(&t, &format!("{} IS NULL", Convert::YesNo.sql(&t))),
            },
            // Trimmed only when no value in the whole column is unpadded.
            Guess::Padded => Plan::Trim {
                unpadded: checks.add(format!(
                    "count(*) FILTER (WHERE {t} <> '' AND {t} = {})",
                    quote_ident(&column.name)
                )),
            },
        };
        let placeholders = (!matches!(plan, Plan::Trim { .. })).then(|| checks.placeholders(&t));
        plans.push((*column, plan, placeholders));
    }
    for column in &columns {
        let read_as = chosen(column);
        if read_as == ReadAs::Auto {
            continue;
        }
        let t = text(column);
        let plan = match Convert::forced(read_as, mark, column) {
            None => {
                plans.push((column, Plan::Text, None));
                continue;
            }
            Some(convert) => {
                let sql = convert.sql(&t);
                let number = matches!(convert, Convert::Number { .. });
                let date = matches!(convert, Convert::Date { .. });
                Plan::Forced {
                    values: checks.values_where(&t, "true"),
                    misfits: checks.values_where(&t, &format!("{sql} IS NULL")),
                    fractional: number.then(|| checks.fractional(&sql)),
                    looks: number.then(|| Looks::check(&mut checks, &t, &sql, mark)),
                    timed: date.then(|| {
                        let timed = format!("{sql} IS NOT NULL AND {}", timed_sql(&t));
                        checks.values_where(&t, &timed)
                    }),
                    convert,
                }
            }
        };
        let placeholders = Some(checks.placeholders(&t));
        plans.push((column, plan, placeholders));
    }

    let results = checks.run(conn, check)?;
    let mut cleaning = Cleaning {
        mark,
        ..Cleaning::default()
    };
    // Keep the table's column order.
    plans.sort_by_key(|(column, ..)| columns.iter().position(|c| c.name == column.name));
    for (column, plan, placeholders_at) in plans {
        let found = placeholders_at.map_or_else(Vec::new, |at| placeholders(&results, at));
        if let Some((expr, format, read_as, mut notes, guessed)) = plan.decide(column, &results) {
            let converted = read_as != ReadAs::Text;
            if converted && !found.is_empty() {
                notes.push(format!("{} read as empty", join_and(&found)));
                for word in found {
                    if !cleaning.placeholders.contains(&word) {
                        cleaning.placeholders.push(word);
                    }
                }
            }
            cleaning.exprs.push((column.name.clone(), expr));
            if let Some(format) = format {
                cleaning.formats.push((column.name.clone(), format));
            }
            cleaning.conversions.push(Conversion {
                column: column.name.clone(),
                read_as,
                notes,
                guessed,
            });
        }
    }
    cleaning.placeholders.sort_by_key(|w| {
        let lower = w.to_lowercase();
        PLACEHOLDERS
            .iter()
            .position(|p| *p == lower)
            .unwrap_or(PLACEHOLDERS.len())
    });
    Ok(cleaning)
}

/// How many of `column`'s values in `check` `read_as` would read, with
/// decimal `mark`. Every value fits `Auto` and `Text`.
pub(crate) fn fit(
    conn: &Connection,
    check: &str,
    column: &Column,
    read_as: ReadAs,
    mark: Mark,
) -> Result<Fit> {
    let t = text(column);
    let mut checks = Checks::default();
    let values = checks.values_where(&t, "true");
    let misfits = Convert::forced(read_as, mark, column)
        .map(|convert| checks.values_where(&t, &format!("{} IS NULL", convert.sql(&t))));
    let results = checks.run(conn, check)?;
    let values = count(&results, values);
    Ok(Fit {
        values,
        fit: values - misfits.map_or(0, |i| count(&results, i)),
    })
}

/// The first non-null values of each column, as text.
fn sample(conn: &Connection, check: &str, columns: &[&Column]) -> Result<Vec<Vec<String>>> {
    let mut values = vec![Vec::new(); columns.len()];
    if columns.is_empty() {
        return Ok(values);
    }
    let select = columns
        .iter()
        .map(|c| quote_ident(&c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = conn.prepare(&format!("SELECT {select} FROM {check} LIMIT {SAMPLE_ROWS}"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        for (i, column) in values.iter_mut().enumerate() {
            if let Some(value) = row.get::<_, Option<String>>(i)? {
                column.push(value);
            }
        }
    }
    Ok(values)
}

/// What to do with one column, with the indexes of its checks' results.
enum Plan {
    Number {
        mark: Mark,
        style: NumberStyle,
        /// What to say about the decimal mark, if anything.
        mark_note: Option<&'static str>,
        /// Whether the mark is a guess (see [`Guessed::DecimalMark`]).
        guessed: bool,
        misfits: usize,
        fractional: usize,
        /// Values in parentheses.
        parens: usize,
        /// The most digits after the decimal mark.
        decimals: usize,
        /// The most significant digits less decimals.
        widest: usize,
        /// The most significant digits.
        digits: usize,
    },
    Date {
        /// The misfits check for each order the sample allows.
        month_first: Option<usize>,
        day_first: Option<usize>,
        numbers: bool,
        dotted: bool,
        forms: Vec<Form>,
    },
    YesNo {
        misfits: usize,
    },
    /// Spaces trimmed; `unpadded` counts the values without any.
    Trim {
        unpadded: usize,
    },
    /// Forced to text.
    Text,
    Forced {
        convert: Convert,
        values: usize,
        misfits: usize,
        /// For numbers only.
        fractional: Option<usize>,
        looks: Option<Looks>,
        /// For dates only: the values that fit with a time of day.
        timed: Option<usize>,
    },
}

/// The checks that say how a forced number column should look: the
/// format its values that fit agree on, as the automatic path finds it.
struct Looks {
    fits: usize,
    /// Distinct currency symbols, counting none as one.
    symbols: usize,
    symbol: usize,
    percents: usize,
    exponents: usize,
    decimals: usize,
    widest: usize,
}

impl Looks {
    /// Adds the checks over the values of trimmed text `t` that number
    /// conversion `number` reads, with decimal `mark`.
    fn check(checks: &mut Checks, t: &str, number: &str, mark: Mark) -> Self {
        let mut over = |aggregate: String, condition: &str| {
            checks.add(format!(
                "{aggregate} FILTER (WHERE {number} IS NOT NULL {condition})"
            ))
        };
        let symbol = format!("regexp_extract({t}, '[$€£¥]')");
        let significant = significant_sql(t);
        let decimals = decimals_sql(t, mark);
        Self {
            fits: over("count(*)".into(), ""),
            symbols: over(format!("count(DISTINCT {symbol})"), ""),
            symbol: over(format!("min({symbol})"), ""),
            percents: over("count(*)".into(), &format!("AND contains({t}, '%')")),
            exponents: over(
                "count(*)".into(),
                &format!("AND regexp_matches({t}, '[eE]')"),
            ),
            decimals: over(format!("max({decimals})"), ""),
            widest: over(format!("max({significant} - {decimals})"), ""),
        }
    }

    /// The format the values that fit agree on: one currency symbol on
    /// every one, or a percent sign on every one, and shown exactly.
    fn format(&self, results: &[Value]) -> Option<NumberFormat> {
        let fits = count(results, self.fits);
        let decimals = count(results, self.decimals) as usize;
        if fits == 0
            || count(results, self.exponents) > 0
            || !shows_exactly(int(results, self.widest) as isize, decimals)
        {
            return None;
        }
        let decimals = u8::try_from(decimals).unwrap_or(u8::MAX);
        let symbol = match &results[self.symbol] {
            Value::Text(text) if count(results, self.symbols) == 1 => text.chars().next(),
            _ => None,
        };
        match (symbol, count(results, self.percents)) {
            (Some(symbol), 0) => Some(NumberFormat::Currency { symbol, decimals }),
            (None, percents) if percents == fits => Some(NumberFormat::Percent { decimals }),
            _ => None,
        }
    }
}

/// What a plan decided for a column: its expression, its format, what it
/// became, the notes and what was guessed.
type Decision = (
    String,
    Option<NumberFormat>,
    ReadAs,
    Vec<String>,
    Option<Guessed>,
);

impl Plan {
    /// Reads the checks' `results` and decides; `None` leaves the column as loaded.
    fn decide(self, column: &Column, results: &[Value]) -> Option<Decision> {
        let t = text(column);
        let clean = |n: usize| count(results, n) == 0;
        match self {
            Plan::Number {
                mark,
                style,
                mark_note,
                guessed,
                misfits,
                fractional,
                parens,
                decimals,
                widest,
                digits,
            } => {
                let decimals = count(results, decimals) as usize;
                let formatted = style.symbol.is_some() || style.percent;
                let integral = !style.percent && clean(fractional);
                // The same rules as `style`, over every row.
                if !clean(misfits)
                    || formatted && !shows_exactly(int(results, widest) as isize, decimals)
                    || !formatted && !integral && count(results, digits) as usize > SHOWN_DIGITS
                {
                    return None;
                }
                let decimals = u8::try_from(decimals).unwrap_or(u8::MAX);
                let format = match style {
                    NumberStyle { percent: true, .. } => Some(NumberFormat::Percent { decimals }),
                    NumberStyle {
                        symbol: Some(symbol),
                        ..
                    } => Some(NumberFormat::Currency { symbol, decimals }),
                    _ => None,
                };
                let mut notes: Vec<String> = mark_note.into_iter().map(String::from).collect();
                if !clean(parens) {
                    notes.push("(x) read as negative".into());
                }
                let convert = Convert::Number {
                    mark,
                    integral,
                    style: Some(style),
                    limit: true,
                };
                let guessed = guessed.then_some(Guessed::DecimalMark);
                Some((convert.sql(&t), format, ReadAs::Number, notes, guessed))
            }
            Plan::Date {
                month_first,
                day_first,
                numbers,
                dotted,
                forms,
            } => {
                let month_first = month_first.is_some_and(clean);
                let day_first = day_first.is_some_and(clean);
                let mut notes = Vec::new();
                let guessed = (month_first && day_first && numbers).then_some(Guessed::DateOrder);
                let chosen = match (month_first, day_first) {
                    (false, false) => return None,
                    (true, true) => {
                        if numbers {
                            notes.push("month/day order is a guess".into());
                        }
                        // Dotted dates (`03.04.2025`) are a day-first habit.
                        numbers && dotted
                    }
                    (false, true) => {
                        notes.push("read day first".into());
                        true
                    }
                    (true, false) => false,
                };
                let mut spellings: Vec<Spelling> = Vec::new();
                for form in &forms {
                    if !spellings.contains(&form.spelling) {
                        spellings.push(form.spelling);
                    }
                }
                if spellings.len() > 1 {
                    notes.push(format!("{} date spellings unified", spellings.len()));
                }
                let convert = Convert::Date {
                    day_first: chosen,
                    timed: forms.iter().any(|f| f.clock.is_some()),
                    forms: Some(forms),
                };
                Some((convert.sql(&t), None, convert.read_as(), notes, guessed))
            }
            Plan::YesNo { misfits } => clean(misfits).then(|| {
                (
                    Convert::YesNo.sql(&t),
                    None,
                    ReadAs::YesNo,
                    Vec::new(),
                    None,
                )
            }),
            Plan::Trim { unpadded } => clean(unpadded).then(|| {
                let notes = vec!["spaces trimmed".into()];
                (t, None, ReadAs::Text, notes, None)
            }),
            Plan::Text => (column.sql_type != "VARCHAR").then(|| {
                (
                    format!("CAST({} AS VARCHAR)", quote_ident(&column.name)),
                    None,
                    ReadAs::Text,
                    Vec::new(),
                    None,
                )
            }),
            Plan::Forced {
                convert,
                values,
                misfits,
                fractional,
                looks,
                timed,
            } => {
                // Shown as the values that fit are written, like `$10.50`.
                let format = looks.and_then(|looks| looks.format(results));
                let percent = matches!(format, Some(NumberFormat::Percent { .. }));
                let convert = match convert {
                    Convert::Number { mark, limit, .. } => Convert::Number {
                        mark,
                        limit,
                        integral: !percent
                            && fractional.is_some_and(clean)
                            && count(results, values) > 0,
                        style: None,
                    },
                    Convert::Date { day_first, .. } => Convert::Date {
                        day_first,
                        forms: None,
                        timed: timed.is_some_and(|n| !clean(n)),
                    },
                    other => other,
                };
                let mut notes = Vec::new();
                match count(results, misfits) {
                    0 => {}
                    1 => notes.push("1 value didn't fit and is empty".into()),
                    n => notes.push(format!("{n} values didn't fit and are empty")),
                }
                Some((convert.sql(&t), format, convert.read_as(), notes, None))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guess_of(values: &[&str]) -> Option<Guess> {
        guess(&values.iter().map(|v| v.to_string()).collect::<Vec<_>>())
    }

    /// The decimal marks a column reads with: (point, comma).
    fn marks(values: &[&str]) -> (bool, bool) {
        match guess_of(values) {
            Some(Guess::Number { point, comma, .. }) => (point.is_some(), comma.is_some()),
            _ => (false, false),
        }
    }

    #[test]
    fn tells_thousands_from_a_decimal_comma() {
        // Thousands with a point: `358,672` reads as a decimal comma too, so it is ambiguous.
        assert_eq!(marks(&["358,672", "1,200"]), (true, true));
        assert_eq!(
            read_number("358,672", Mark::Point).unwrap().value,
            358_672.0
        );
        // A comma followed by two digits is a decimal comma.
        assert_eq!(marks(&["1,75", "647,50"]), (false, true));
        assert_eq!(read_number("1.424,50", Mark::Comma).unwrap().value, 1424.5);
        // Neither: groups of two after a group of three.
        assert_eq!(marks(&["1,234,56"]), (false, false));
        assert_eq!(read_number("1,234,56", Mark::Point), None);
        assert_eq!(read_number("1,234,56", Mark::Comma), None);
        // Both separators settle it.
        assert_eq!(marks(&["$1,234.00"]), (true, false));
        assert_eq!(marks(&["1.234,00"]), (false, true));
    }

    #[test]
    fn reads_formatted_numbers() {
        let w = read_number("(4.2%)", Mark::Point).unwrap();
        assert_eq!((w.value, w.percent), (-4.2, true));
        let w = read_number("-$1,200.00", Mark::Point).unwrap();
        assert_eq!((w.value, w.symbol), (-1200.0, Some('$')));
        assert_eq!(read_number("1.5e3", Mark::Point).unwrap().value, 1500.0);
        assert_eq!(read_number("1.5e3", Mark::Comma), None);
        for text in [
            "1.2.3", "1-2", "3/4", "555-0100", "12a", "--5", "(-3)", "", "%",
        ] {
            assert_eq!(read_number(text, Mark::Point), None, "{text}");
        }
    }

    #[test]
    fn ids_and_codes_stay_text() {
        for values in [
            &["02134", "10001"][..],         // a leading zero matters
            &["07901490718", "07685155615"], // phone numbers
            &["1234567890123456", "1"],      // too long to be exact
            &["555-0100", "555-0199"],       // phone-like
            &["1-2", "3-4"],                 // codes
            &["3/4", "1/2"],                 // fractions, not dates
            &["1.2.3", "2.0.1"],             // versions
            &["$5", "€6"],                   // two currencies
            &["12%", "0.5"],                 // percent and not
            &["10", "ten"],                  // one misfit
            &["N/A", "-", "TBD"],            // nothing but placeholders
        ] {
            assert_eq!(guess_of(values), None, "{values:?}");
        }
        // `-` alone in a text column is text, and stays as written.
        assert_eq!(guess_of(&["apple", "-", "pear"]), None);
    }

    #[test]
    fn placeholders_do_not_stop_a_conversion() {
        assert!(matches!(
            guess_of(&["151", "N/A", "-", "TBD", "#DIV/0!", "46"]),
            Some(Guess::Number { .. })
        ));
        assert_eq!(
            guess_of(&["Yes", "N", "y", "No", "n/a"]),
            Some(Guess::YesNo)
        );
    }

    #[test]
    fn finds_the_day_month_order() {
        let order = |values: &[&str]| match guess_of(values) {
            Some(Guess::Date {
                month_first,
                day_first,
                forms,
                ..
            }) => Some((month_first, day_first, forms.len())),
            _ => None,
        };
        // No day above 12: either order, so a guess.
        assert_eq!(order(&["1/2/2025", "03/04/2025"]), Some((true, true, 1)));
        // A day above 12 proves the order.
        assert_eq!(order(&["12/02/2025", "24/03/2025"]), Some((false, true, 1)));
        assert_eq!(
            order(&["01/13/2025", "Jul 8, 2025", "2025-02-22"]),
            Some((true, false, 3))
        );
        // Both orders broken: not dates.
        assert_eq!(order(&["13/01/2025", "01/13/2025"]), None);
        assert_eq!(order(&["2025-02-30"]), None);
        assert_eq!(
            order(&["8 July 2025", "8-Jul-2025", "July 8 2025"]),
            Some((true, true, 3))
        );
    }

    #[test]
    fn reads_times_and_two_digit_years() {
        let form = |text: &str| read_date(text).map(|d| (d.form, d.month_first, d.day_first));
        let numbers = |sep, clock| Form {
            spelling: Spelling::Numbers(sep),
            clock,
        };
        assert_eq!(
            form("03/04/2025 10:30"),
            Some((numbers('/', Some(Clock::Minutes)), true, true))
        );
        assert_eq!(
            form("24/03/2025 23:59:59.123456"),
            Some((numbers('/', Some(Clock::Fraction)), false, true))
        );
        assert_eq!(
            form("3/4/2025 9:15 pm"),
            Some((numbers('/', Some(Clock::Minutes12)), true, true))
        );
        assert_eq!(
            form("2025-01-02 10:30:00").map(|f| f.0.spelling),
            Some(Spelling::Iso('-'))
        );
        assert_eq!(
            form("01/22/25"),
            Some((
                Form {
                    spelling: Spelling::ShortYear('/'),
                    clock: None
                },
                true,
                false
            ))
        );
        // 29 February 2068 is a date; in 1969, by the `%y` pivot, it isn't.
        assert!(read_date("29.02.68").is_some());
        assert!(read_date("29.02.69").is_none());
        for text in [
            "03/04/2025 24:00",
            "03/04/2025 10:60",
            "03/04/2025 13:00 PM",
            "03/04/2025 10:30:00.1234567",
            "03/04/2025 10",
            "Jul 8, 2025 10:30",
            "1/2/3",
            "1.2.345",
            "10:30",
        ] {
            assert_eq!(read_date(text), None, "{text}");
        }
    }

    #[test]
    fn renders_formats() {
        let dollars = NumberFormat::Currency {
            symbol: '$',
            decimals: 2,
        };
        assert_eq!(dollars.render(723_278.0), "$723,278.00");
        assert_eq!(dollars.render(-1200.0), "-$1,200.00");
        assert_eq!(
            NumberFormat::Currency {
                symbol: '$',
                decimals: 0
            }
            .render(632_085.0),
            "$632,085"
        );
        assert_eq!(NumberFormat::Percent { decimals: 1 }.render(0.126), "12.6%");
        assert_eq!(
            NumberFormat::Percent { decimals: 1 }.render(-0.042),
            "-4.2%"
        );
        assert_eq!(NumberFormat::Percent { decimals: 2 }.render(0.001), "0.10%");
    }

    #[test]
    fn parses_what_it_renders() {
        let dollars = NumberFormat::Currency {
            symbol: '$',
            decimals: 2,
        };
        for text in ["$1,000", "1,000", "1000", " $1,000.00 "] {
            assert_eq!(dollars.parse(text), Some(1000.0), "{text}");
        }
        assert_eq!(dollars.parse("-$1,200.00"), Some(-1200.0));
        assert_eq!(dollars.parse("($5)"), Some(-5.0));
        for text in ["€5", "5%", "$$5", "abc", "", "1,00"] {
            assert_eq!(dollars.parse(text), None, "{text}");
        }

        let pct = NumberFormat::Percent { decimals: 1 };
        for text in ["10", "10%", "10.0 %"] {
            assert_eq!(pct.parse(text), Some(0.1), "{text}");
        }
        assert_eq!(pct.parse("(4.2%)"), Some(-0.042));
        assert_eq!(pct.parse("$10"), None);
        // What it renders reads back as the same number.
        for value in [0.126, -0.042, 0.0, 1.5] {
            assert_eq!(pct.parse(&pct.render(value)), Some(value));
        }
        assert_eq!(dollars.parse(&dollars.render(-723_278.5)), Some(-723_278.5));
    }

    /// Whether a column of `values` converts, decided twice: by the Rust
    /// classifier on all of them, and by the SQL check on a table whose
    /// first `SAMPLE_ROWS` rows are `values[0]`, so the rest are seen by SQL
    /// alone. Keeps the two implementations of each rule from drifting.
    fn converts(values: &[&str]) -> (bool, bool) {
        let rust = matches!(guess_of(values), Some(Guess::Number { .. }));
        let conn = Connection::open_in_memory().unwrap();
        let rows = values
            .iter()
            .map(|v| format!("({})", quote_str(v)))
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute_batch(&format!(
            "CREATE TABLE t AS SELECT {} AS x FROM range({SAMPLE_ROWS}); INSERT INTO t VALUES {rows};",
            quote_str(values[0])
        ))
        .unwrap();
        let cleaning = clean(&conn, "t", &ValueOptions::default(), true, &|| None).unwrap();
        (rust, !cleaning.conversions.is_empty())
    }

    #[test]
    fn rust_and_sql_agree_on_what_converts() {
        for (values, expected) in [
            // An exponent only in a column with no currency or percent sign.
            (&["1.5e3", "2E-2"][..], true),
            (&["$1", "$1e-3"], false),
            (&["1%", "1e-3%"], false),
            (&["$5", "1e3"], false),
            // At most 15 significant digits, trailing zeros included.
            (&["1", "999999999999999"], true),
            (&["1", "1234567890123456"], false),
            (&["0.5", "0.1234567890123456"], false),
            (&["0.5", "0.000000000000001234"], true),
            (&["$1.00", "$999999999999999.99"], false),
            (&["$1.00", "$1,234,567,890,123.00"], true),
            (&["$1", "$123,456,789,012,345"], true),
            (&["$1", "$123,456,789,012,345.00"], false),
            (&["1,5", "1.234.567.890,12"], true),
            (&["1,5", "1.234.567.890.123,456"], false),
            // With no format, a double shows 12 digits: more only in a
            // column of whole numbers, which is stored as integers.
            (&["0.5", "123456789.012"], true),
            (&["0.5", "1234567890.123"], false),
            (&["12,345,678,901.55", "1.5"], false),
            (&["1", "1234567890123"], true),
            (&["1.5", "1234567890123"], false),
            (&["1,5", "1.234.567.890.123,4"], false),
            (&["12.5%", "1234567890123.45%"], true),
            (&["12.5%", "1234567890123.456%"], false),
            // In a formatted column, every value as shown: here at 2 decimals.
            (&["$0.01", "$9,999,999,999,999.9"], true),
            (&["$0.01", "$99,999,999,999,999.9"], false),
            (&["$0.0000000000000001", "$0.0000000000000002"], true),
            // Too big for a double.
            (&["1", "1e308"], true),
            (&["1", "1e309"], false),
            (&["1", "-1e309"], false),
        ] {
            assert_eq!(converts(values), (expected, expected), "{values:?}");
        }
    }

    #[test]
    fn a_formatted_column_shows_and_reads_back_every_value_exactly() {
        // Values with up to 15 digits as shown, in a column shown with
        // `decimals`, written with `written` of them: what the grid shows is
        // what the file said, padded, and reads back as the stored number.
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut digit = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (b'0' + (seed % 10) as u8) as char
        };
        for decimals in 0..=6_usize {
            for written in 0..=decimals {
                for whole_digits in 0..=MAX_DIGITS - decimals {
                    for negative in [false, true] {
                        let mut whole: String = (0..whole_digits).map(|_| digit()).collect();
                        if whole_digits > 0 {
                            whole.replace_range(..1, "7");
                        } else {
                            whole = "0".into();
                        }
                        let fraction: String = (0..written).map(|_| digit()).collect();
                        let padded = format!("{fraction:0<decimals$}");
                        let point = |f: &str| {
                            if f.is_empty() {
                                String::new()
                            } else {
                                format!(".{f}")
                            }
                        };
                        let zero = whole == "0" && !fraction.contains(|c| c != '0');
                        let sign = if negative && !zero { "-" } else { "" };
                        let plain: f64 = format!("{whole}{}", point(&fraction)).parse().unwrap();
                        let plain = if negative { -plain } else { plain };

                        let dollars = NumberFormat::Currency {
                            symbol: '$',
                            decimals: decimals as u8,
                        };
                        let text =
                            format!("{sign}${}{}", group_thousands(&whole), point(&fraction));
                        let shown = format!("{sign}${}{}", group_thousands(&whole), point(&padded));
                        assert!(style(&[&text, &shown], Mark::Point).is_some(), "{text}");
                        assert_eq!(dollars.render(plain), shown, "{text}");
                        assert_eq!(dollars.parse(&shown), Some(plain), "{text}");

                        let pct = NumberFormat::Percent {
                            decimals: decimals as u8,
                        };
                        let text = format!("{sign}{whole}{}%", point(&fraction));
                        let shown = format!("{sign}{whole}{}%", point(&padded));
                        // Stored as SQL stores it: the number written, over 100.
                        let stored = plain / 100.0;
                        assert!(style(&[&text, &shown], Mark::Point).is_some(), "{text}");
                        assert_eq!(pct.render(stored), shown, "{text}");
                        assert_eq!(pct.parse(&shown), Some(stored), "{text}");
                    }
                }
            }
        }
    }
}
