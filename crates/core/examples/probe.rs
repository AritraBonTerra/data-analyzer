//! Prints how da-core reads each file given on the command line, as one JSON
//! object per part (table) per line: the parts found, the summary, the notes,
//! the plan in effect, detected columns with types and units, the notice, the
//! row count and the first rows. Use it to see what the grid would show for an
//! awkward file. `"default": true` marks the part the app opens first.
//!
//! cargo run -p da-core --example probe -- messy.csv report.xlsx

use std::path::Path;

use da_core::*;

/// Rows printed per part.
const ROWS: u64 = 12;

fn main() {
    let cache_dir = std::env::temp_dir().join("da-probe-cache");
    for arg in std::env::args().skip(1) {
        let path = Path::new(&arg);
        let open = |part: Option<usize>| {
            Dataset::open(
                path,
                &OpenOptions {
                    cache_dir: cache_dir.clone(),
                    part,
                    ..Default::default()
                },
            )
        };
        let (parts, default) = match open(None) {
            Ok(dataset) => (dataset.parts().len(), dataset.part()),
            Err(err) => {
                let err = json_str(&format!("{err:#}"));
                println!("{{\"file\": {}, \"error\": {err}}}", json_str(&arg));
                continue;
            }
        };
        for part in 0..parts {
            let body = match open(Some(part)).and_then(|dataset| probe(&dataset)) {
                Ok(body) => body,
                Err(err) => format!("\"error\": {}", json_str(&format!("{err:#}"))),
            };
            println!(
                "{{\"file\": {}, \"part_index\": {part}, \"default\": {}, {body}}}",
                json_str(&arg),
                part == default
            );
        }
    }
}

/// Loads an opened part the way the app does and describes the result.
fn probe(dataset: &Dataset) -> anyhow::Result<String> {
    let frame = dataset.materialize(&|_| {})?;
    let page = dataset.fetch(&frame, 0, ROWS)?;

    let opt = |text: Option<&str>| text.map_or("null".into(), json_str);
    let parts = dataset
        .parts()
        .iter()
        .map(|p| {
            format!(
                "{{\"label\": {}, \"rows\": {}, \"at\": {}}}",
                json_str(&p.label),
                p.rows.map_or("null".into(), |n| n.to_string()),
                opt(p.at.as_deref())
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let notes = dataset
        .notes()
        .iter()
        .map(|n| format!("[{}, \"{:?}\", {}]", n.row, n.role, json_str(&n.text)))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = |r: Rows| format!("[{}, {}]", r.first, r.last);
    let plan = dataset.plan().map_or("null".into(), |p| {
        format!(
            "{{\"header\": {}, \"data\": {}, \"keep_totals\": {}, \"delimiter\": {}}}",
            p.header.map_or("null".into(), rows),
            rows(p.data),
            p.keep_totals,
            opt(dataset.delimiter().map(|d| format!("{d:?}")).as_deref())
        )
    });
    let columns = frame
        .columns
        .iter()
        .map(|c| {
            format!(
                "{{\"name\": {}, \"type\": {}, \"unit\": {}}}",
                json_str(&c.name),
                json_str(&c.sql_type),
                opt(dataset.unit(&c.name))
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
                .map(|cell| opt(cell.as_deref()))
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{cells}]")
        })
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "\"kind\": {}, \"part\": {}, \"parts\": [{parts}], \"summary\": {}, \"notes\": [{notes}], \"plan\": {plan}, \"notice\": {}, \"row_count\": {}, \"columns\": [{columns}], \"rows\": [{rows}]",
        json_str(dataset.kind().label()),
        json_str(&dataset.parts()[dataset.part()].label),
        opt(dataset.summary()),
        opt(dataset.notice().as_deref()),
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
