//! Text encodings of CSV/TSV files. DuckDB and the table finder read UTF-8
//! only, so a file in another encoding is transcoded to a UTF-8 copy first
//! and everything reads the copy. A valid UTF-8 file is read in place, and
//! a UTF-8 file with a few bytes that aren't is mended: those bytes alone
//! are read as Windows-1252.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context as _, Result};

/// The text encoding of a CSV/TSV file. `Auto` reads UTF-16 when the file
/// starts with its byte-order mark or looks like it (a NUL in every other
/// byte), UTF-8 when the start of the file is valid UTF-8 or mostly so, and
/// Windows-1252 otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    #[default]
    Auto,
    Utf8,
    /// Little-endian unless a byte-order mark or the bytes say otherwise.
    Utf16,
    /// Windows Western, a superset of Latin-1 with `€` and curly quotes.
    Windows1252,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic",
            Self::Utf8 => "UTF-8",
            Self::Utf16 => "UTF-16",
            Self::Windows1252 => "Windows-1252",
        }
    }
}

/// Bytes checked for valid UTF-8 at open. Files no larger are checked
/// whole, so every file the table finder scans is.
const PREFIX: usize = 4 << 20;

/// How a file's bytes become the UTF-8 text everything reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decode {
    Utf16 {
        big_endian: bool,
    },
    Windows1252,
    /// UTF-8, with each byte that isn't part of a valid sequence read as
    /// Windows-1252: a UTF-8 file with a stray line from another program.
    MendedUtf8,
}

/// How to read `path` under `choice`: `None` reads it as it is, as UTF-8.
/// For `Auto`, the first [`PREFIX`] bytes decide.
pub(crate) fn detect(path: &Path, choice: Encoding) -> Result<Option<Decode>> {
    if choice == Encoding::Utf8 {
        return Ok(None);
    }
    if choice == Encoding::Windows1252 {
        return Ok(Some(Decode::Windows1252));
    }
    let mut prefix = Vec::with_capacity(PREFIX.min(1 << 16));
    File::open(path)?
        .take(PREFIX as u64)
        .read_to_end(&mut prefix)?;
    let utf16 = |big_endian| Ok(Some(Decode::Utf16 { big_endian }));
    if prefix.starts_with(&[0xFF, 0xFE]) {
        return utf16(false);
    }
    if prefix.starts_with(&[0xFE, 0xFF]) {
        return utf16(true);
    }
    if let Some(big_endian) = utf16_without_bom(&prefix) {
        return utf16(big_endian);
    }
    if choice == Encoding::Utf16 {
        return utf16(false);
    }
    let (sequences, invalid) = utf8_census(&prefix, prefix.len() < PREFIX);
    Ok(match invalid {
        0 => None,
        // Mostly UTF-8: keep it, and mend the bytes that aren't.
        _ if sequences > invalid => Some(Decode::MendedUtf8),
        _ => Some(Decode::Windows1252),
    })
}

/// Counts in `bytes` the characters outside ASCII that are valid UTF-8,
/// and the bytes that aren't part of a valid sequence. A sequence cut off
/// at the end counts as invalid only when `bytes` is the `whole` file.
fn utf8_census(mut bytes: &[u8], whole: bool) -> (u64, u64) {
    let wide = |valid: &[u8]| {
        let text = std::str::from_utf8(valid).unwrap_or_default();
        text.chars().filter(|c| !c.is_ascii()).count() as u64
    };
    let (mut sequences, mut invalid) = (0, 0);
    loop {
        match std::str::from_utf8(bytes) {
            Ok(_) => return (sequences + wide(bytes), invalid),
            Err(err) => {
                let (valid, rest) = bytes.split_at(err.valid_up_to());
                sequences += wide(valid);
                match err.error_len() {
                    Some(n) => {
                        invalid += n as u64;
                        bytes = &rest[n..];
                    }
                    None if whole => return (sequences, invalid + rest.len() as u64),
                    None => return (sequences, invalid),
                }
            }
        }
    }
}

/// Whether `prefix` looks like UTF-16 without a byte-order mark, and if so
/// its byte order: ASCII text in UTF-16 has a NUL in every other byte. Kept
/// narrow so no 8-bit text is taken for it: nine in ten of one half of the
/// first bytes are NUL, and none of the other half.
fn utf16_without_bom(prefix: &[u8]) -> Option<bool> {
    let sample = &prefix[..prefix.len().min(1024) & !1];
    if sample.len() < 4 {
        return None;
    }
    let nuls = |offset: usize| {
        sample
            .iter()
            .skip(offset)
            .step_by(2)
            .filter(|&&b| b == 0)
            .count()
    };
    let half = sample.len() / 2;
    let mostly = |n: usize| n * 10 >= half * 9;
    match (nuls(0), nuls(1)) {
        // Little-endian: the high byte, NUL for ASCII, comes second.
        (0, odd) if mostly(odd) => Some(false),
        (even, 0) if mostly(even) => Some(true),
        _ => None,
    }
}

/// Writes `path`, decoded as `decode` says, to `out` as UTF-8. Streams, so
/// a large file never sits in memory. Returns what mending found, for
/// [`Decode::MendedUtf8`].
pub(crate) fn transcode(path: &Path, decode: Decode, out: &Path) -> Result<Mended> {
    let mut input =
        File::open(path).with_context(|| format!("couldn't read {}", path.display()))?;
    let mut output = BufWriter::new(File::create(out)?);
    let mut buf = vec![0u8; 1 << 16];
    let mut text = String::new();
    let mut utf16 = Utf16::default();
    let mut mender = Mender::default();
    if let Decode::Utf16 { big_endian } = decode {
        utf16.big_endian = big_endian;
    }
    loop {
        let n = input.read(&mut buf)?;
        text.clear();
        match decode {
            Decode::Utf16 { .. } => utf16.decode(&buf[..n], n == 0, &mut text),
            Decode::Windows1252 => text.extend(buf[..n].iter().map(|&b| windows_1252(b))),
            Decode::MendedUtf8 => mender.decode(&buf[..n], n == 0, &mut text),
        }
        output.write_all(text.as_bytes())?;
        if n == 0 {
            break;
        }
    }
    output.flush()?;
    Ok(mender.found)
}

/// The encoding a file decoded as `decode` reads as, for the user, and the
/// bytes in it that were read as Windows-1252 though the file is UTF-8.
pub(crate) fn read_as(decode: Decode, found: Mended) -> (Encoding, u64) {
    match decode {
        Decode::Utf16 { .. } => (Encoding::Utf16, 0),
        Decode::Windows1252 => (Encoding::Windows1252, 0),
        // Nothing but ASCII was UTF-8: the same text Windows-1252 reads.
        Decode::MendedUtf8 if !found.utf8 => (Encoding::Windows1252, 0),
        Decode::MendedUtf8 => (Encoding::Utf8, found.bytes),
    }
}

/// What mending a UTF-8 file found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mended {
    /// Whether any character outside ASCII was valid UTF-8. When none was,
    /// the file reads exactly as Windows-1252 would read it.
    pub utf8: bool,
    /// The bytes read as Windows-1252.
    pub bytes: u64,
}

/// A UTF-8 decoder fed in chunks that reads each byte outside a valid
/// sequence as Windows-1252, holding back a sequence a chunk boundary cut.
#[derive(Default)]
struct Mender {
    pending: Vec<u8>,
    found: Mended,
}

impl Mender {
    fn decode(&mut self, chunk: &[u8], last: bool, out: &mut String) {
        let Self { pending, found } = self;
        pending.extend_from_slice(chunk);
        let mut keep = |valid: &[u8], out: &mut String| {
            let valid = std::str::from_utf8(valid).unwrap_or_default();
            // A byte-order mark is UTF-8 too, but says nothing about the text.
            found.utf8 |= !valid.trim_start_matches('\u{feff}').is_ascii();
            out.push_str(valid);
        };
        let mend = |bad: &[u8], out: &mut String| {
            out.extend(bad.iter().map(|&b| windows_1252(b)));
            bad.len() as u64
        };
        let mut rest = &pending[..];
        let mut mended = 0;
        loop {
            match std::str::from_utf8(rest) {
                Ok(_) => {
                    keep(rest, out);
                    rest = &[];
                    break;
                }
                Err(err) => {
                    let (valid, bad) = rest.split_at(err.valid_up_to());
                    keep(valid, out);
                    match err.error_len() {
                        Some(n) => {
                            mended += mend(&bad[..n], out);
                            rest = &bad[n..];
                        }
                        // Cut off at the chunk's end: wait for the rest of it.
                        None if !last => {
                            rest = bad;
                            break;
                        }
                        None => {
                            mended += mend(bad, out);
                            rest = &[];
                            break;
                        }
                    }
                }
            }
        }
        found.bytes += mended;
        let done = pending.len() - rest.len();
        pending.drain(..done);
    }
}

/// The character a Windows-1252 byte stands for. Bytes 0x80 to 0x9F hold
/// `€`, curly quotes and dashes; the five it leaves unused map to the
/// control characters of the same number, as browsers do.
fn windows_1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    match byte {
        0x80..=0x9F => HIGH[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
}

/// A UTF-16 decoder fed in chunks, holding back a byte or a lone high
/// surrogate that a chunk boundary cut off. Little-endian unless set or
/// a byte-order mark says otherwise.
#[derive(Default)]
struct Utf16 {
    started: bool,
    big_endian: bool,
    pending: Vec<u8>,
}

impl Utf16 {
    fn decode(&mut self, chunk: &[u8], last: bool, out: &mut String) {
        self.pending.extend_from_slice(chunk);
        let mut bytes = &self.pending[..];
        // A byte-order mark says the order, and is dropped; without one,
        // `big_endian` is what the caller set.
        if !self.started && bytes.len() >= 2 {
            self.started = true;
            if let Some(big_endian) = match bytes[..2] {
                [0xFF, 0xFE] => Some(false),
                [0xFE, 0xFF] => Some(true),
                _ => None,
            } {
                self.big_endian = big_endian;
                bytes = &bytes[2..];
            }
        }
        let mut units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| {
                if self.big_endian {
                    u16::from_be_bytes(pair)
                } else {
                    u16::from_le_bytes(pair)
                }
            })
            .collect();
        let mut keep = bytes.len() % 2;
        if !last && units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
            units.pop();
            keep += 2;
        }
        out.extend(char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
        if last && keep % 2 == 1 {
            out.push(char::REPLACEMENT_CHARACTER);
        }
        let rest = bytes.len() - keep;
        let start = self.pending.len() - bytes.len() + rest;
        self.pending.drain(..start);
        if last {
            self.pending.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_windows_1252() {
        let text: String = b"J\xfcrgen \x80 \x93q\x94"
            .iter()
            .map(|&b| windows_1252(b))
            .collect();
        assert_eq!(text, "Jürgen € “q”");
    }

    #[test]
    fn mends_bytes_that_are_not_utf8_across_chunks() {
        let mut bytes = "Zoë,€ ".as_bytes().to_vec();
        bytes.extend(b"Jos\xe9 \x93q\x94 ");
        bytes.extend("Malmö".as_bytes());
        bytes.push(0xC3); // cut off at the very end
        let mut mender = Mender::default();
        let mut out = String::new();
        // One byte at a time, so every sequence is cut.
        for byte in &bytes {
            mender.decode(std::slice::from_ref(byte), false, &mut out);
        }
        mender.decode(&[], true, &mut out);
        assert_eq!(out, "Zoë,€ José “q” MalmöÃ");
        assert_eq!(
            mender.found,
            Mended {
                utf8: true,
                bytes: 4
            }
        );
        // Nothing but ASCII as UTF-8: Windows-1252 reads it the same.
        let mut mender = Mender::default();
        mender.decode(b"\xef\xbb\xbfa,M\xfcnchen", true, &mut out);
        assert!(!mender.found.utf8);
    }

    #[test]
    fn tells_mostly_utf8_from_windows_1252() {
        let mut mixed = "Zoë,Malmö,".as_bytes().to_vec();
        mixed.push(0xe9);
        assert_eq!(utf8_census(&mixed, true), (2, 1));
        assert_eq!(utf8_census(b"M\xfcnchen,K\xf6ln", true), (0, 2));
        // A sequence cut off at the end of a prefix is not counted.
        assert_eq!(utf8_census(&"ë".as_bytes()[..1], false), (0, 0));
        assert_eq!(utf8_census(&"ë".as_bytes()[..1], true), (0, 1));
    }

    #[test]
    fn finds_utf16_without_a_byte_order_mark() {
        let text = "id,name\n1,Zoë\n";
        let le: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let be: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(utf16_without_bom(&le), Some(false));
        assert_eq!(utf16_without_bom(&be), Some(true));
        // 8-bit text, even with a stray NUL, is not UTF-16.
        assert_eq!(utf16_without_bom(b"id,name\n1,Zo\xeb\n"), None);
        assert_eq!(utf16_without_bom(b"a\0b,c\nd,e\n"), None);
        assert_eq!(utf16_without_bom(b"a\0"), None);
    }

    #[test]
    fn decodes_utf16_across_chunks() {
        let source = "a,b\nü,😀\n";
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(source.encode_utf16().flat_map(u16::to_le_bytes));
        // Feed one byte at a time, so every boundary is cut.
        let mut decoder = Utf16::default();
        let mut out = String::new();
        for byte in &bytes {
            decoder.decode(std::slice::from_ref(byte), false, &mut out);
        }
        decoder.decode(&[], true, &mut out);
        assert_eq!(out, source);
    }
}
