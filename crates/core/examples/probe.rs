//! Prints how da-core reads each file given on the command line, as one JSON
//! object per line: detected columns and types, the notice, the row count and
//! the first rows. Use it to see what the grid would show for an awkward file.
//!
//! cargo run -p da-core --example probe -- messy.csv report.xlsx

use std::path::Path;

use da_core::*;

/// Rows printed per file or sheet.
const ROWS: u64 = 12;

fn main() {
    let cache_dir = std::env::temp_dir().join("da-probe-cache");
    for arg in std::env::args().skip(1) {
        let path = Path::new(&arg);
        let sheets = match FileKind::detect(path) {
            Some(FileKind::Excel) => sheet_names(path).map_or(1, |names| names.len().max(1)),
            _ => 1,
        };
        for sheet in 0..sheets {
            let body = match probe(path, &cache_dir, sheet) {
                Ok(body) => body,
                Err(err) => format!("\"error\": {}", json_str(&format!("{err:#}"))),
            };
            println!(
                "{{\"file\": {}, \"sheet_index\": {sheet}, {body}}}",
                json_str(&arg)
            );
        }
    }
}

/// Opens one file (or one sheet of it) the way the app does and describes the result.
fn probe(path: &Path, cache_dir: &Path, sheet: usize) -> anyhow::Result<String> {
    let dataset = Dataset::open(
        path,
        &OpenOptions {
            cache_dir: cache_dir.to_path_buf(),
            sheet,
        },
    )?;
    let frame = dataset.materialize(&|_| {})?;
    let page = dataset.fetch(&frame, 0, ROWS)?;

    let columns = frame
        .columns
        .iter()
        .map(|c| {
            format!(
                "{{\"name\": {}, \"type\": {}}}",
                json_str(&c.name),
                json_str(&c.sql_type)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let rows = page
        .rows
        .iter()
        .map(|row| {
            let cells = row
                .iter()
                .map(|cell| cell.as_deref().map_or("null".into(), json_str))
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{cells}]")
        })
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "\"kind\": {}, \"sheet\": {}, \"notice\": {}, \"row_count\": {}, \"columns\": [{columns}], \"rows\": [{rows}]",
        json_str(dataset.kind().label()),
        dataset
            .sheets()
            .get(dataset.sheet())
            .map_or("null".into(), |name| json_str(name)),
        dataset.notice().as_deref().map_or("null".into(), json_str),
        frame.rows.unwrap_or(0),
    ))
}

fn json_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
