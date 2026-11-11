//! Times the open → preview → load → page → sort → stats path on a large CSV.
//! Generates a synthetic file first if it doesn't exist.
//!
//! cargo run -p da-core --release --example bench -- /tmp/da-bench.csv 30000000

use std::path::PathBuf;
use std::time::Instant;

use da_core::*;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().unwrap_or_else(|| "/tmp/da-bench.csv".into()));
    let rows: u64 = args.next().map_or(30_000_000, |n| n.parse().unwrap());
    let cache_dir = std::env::temp_dir().join("da-bench-cache");

    if !path.exists() {
        let t = Instant::now();
        let conn = duckdb::Connection::open_in_memory()?;
        conn.execute_batch(&format!(
            "COPY (SELECT i AS id, 'name_' || (i % 100000) AS name, random() * 1000 AS amount,
                          DATE '2020-01-01' + CAST(i % 1500 AS INTEGER) AS day,
                          CASE WHEN i % 10 = 0 THEN NULL ELSE md5(CAST(i AS VARCHAR)) END AS hash,
                          i % 7 AS bucket
                   FROM range({rows}) t(i)) TO '{}' (HEADER)",
            path.display()
        ))?;
        println!("generated {} in {:.2?}", path.display(), t.elapsed());
    }
    let size = std::fs::metadata(&path)?.len() as f64 / 1e9;
    println!("file: {size:.2} GB");

    let t = Instant::now();
    let ds = Dataset::open(
        &path,
        &OpenOptions {
            cache_dir,
            sheet: 0,
        },
    )?;
    let preview = ds.frame()?;
    let page = ds.fetch(&preview, 0, 200)?;
    println!(
        "open + first 200 rows: {:.2?} ({} rows)",
        t.elapsed(),
        page.rows.len()
    );

    let t = Instant::now();
    let frame = ds.materialize(&|_| {})?;
    println!("materialize: {:.2?} ({:?} rows)", t.elapsed(), frame.rows);

    let t = Instant::now();
    let deep = ds.fetch(&frame, frame.rows.unwrap() / 2, 200)?;
    println!(
        "page at middle: {:.2?} ({} rows)",
        t.elapsed(),
        deep.rows.len()
    );

    let t = Instant::now();
    let sorted = ds.apply(
        &frame,
        &ViewSpec {
            sort: vec![SortKey {
                column: "amount".into(),
                descending: true,
            }],
            ..Default::default()
        },
    )?;
    println!("sort by amount: {:.2?}", t.elapsed());
    let t = Instant::now();
    ds.fetch(&sorted, sorted.rows.unwrap() / 2, 200)?;
    println!("sorted page at middle: {:.2?}", t.elapsed());

    let t = Instant::now();
    let filtered = ds.apply(
        &frame,
        &ViewSpec {
            search: "name_4242".into(),
            ..Default::default()
        },
    )?;
    println!(
        "search all columns: {:.2?} ({:?} rows)",
        t.elapsed(),
        filtered.rows
    );

    let t = Instant::now();
    ds.summarize(&frame)?;
    println!("summarize: {:.2?}", t.elapsed());

    let t = Instant::now();
    ds.sketches(&frame)?;
    println!("header sketches: {:.2?}", t.elapsed());

    let t = Instant::now();
    ds.distribution(&frame, &frame.columns[2], 40, 10)?;
    println!("exact histogram: {:.2?}", t.elapsed());
    Ok(())
}
