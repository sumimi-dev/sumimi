//! Which compression does DuckDB actually pick for each column?
//!
//! The storage question behind the session id: a column whose values repeat
//! (one value per session, say) — does the repetition get compressed, and how
//! does the width of the value show up in the bill?
//!
//! ```bash
//! cargo run --release --example compression -p sumimi-service -- ~/.cache/sumimi-scratch/big/data.duckdb
//! ```

use duckdb::Connection;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_default();
    let config = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;
    let conn = Connection::open_with_flags(&path, config)?;

    let mut stmt = conn.prepare(
        "SELECT column_name, segment_type, compression, count(*) AS segments
         FROM pragma_storage_info('event')
         GROUP BY column_name, segment_type, compression
         ORDER BY column_name, segments DESC",
    )?;
    let mut rows = stmt.query([])?;
    println!("{:<18} {:<16} {:<16} {:>8}", "column", "segment", "compression", "segments");
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        let segment: String = row.get(1)?;
        let compression: String = row.get(2)?;
        let segments: i64 = row.get(3)?;
        println!("{name:<18} {segment:<16} {compression:<16} {segments:>8}");
    }
    Ok(())
}
