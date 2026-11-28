//! The rows a server wrote, as ClickHouse's JSON.
//!
//! The traffic that reaches a server comes from outside — a browser, a
//! tracker, a load generator — so what the server writes is what the *real*
//! collect path does, not what a fixture in this repository says. This example
//! opens the database a server wrote and writes its rows out in the shape
//! ClickHouse reads (`JSONEachRow`, one object a line).
//!
//! **The columns come from the schema, and the values are read by name** —
//! the same list the table was made from, in the same order, so the export
//! cannot drift from the table.
//!
//! ```bash
//! cargo run --release --example ch_rows -p sumimi-service -- ./sumimi.duckdb rows.jsonl
//! ```

use std::io::Write;
use std::path::PathBuf;

use sumimi_service::pool::Db;
use sumimi_service::row::Row;
use sumimi_service::schema::{self, Kind};

fn main() -> anyhow::Result<()> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "./sumimi.duckdb".to_string())
        .into();
    let out: PathBuf = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "rows.jsonl".to_string())
        .into();
    let db = Db::open(&path, "256MB")?;

    // The tables live in a file per website, attached under the site's name.
    let schemas: Vec<String> = db.with_read(|conn| {
        let mut rows = conn.prepare(
            "SELECT database_name FROM duckdb_databases() \
             WHERE starts_with(database_name, 's_') ORDER BY database_name",
        )?;
        let mut out = Vec::new();
        let mut result = rows.query([])?;
        while let Some(row) = result.next()? {
            out.push(row.get::<usize, String>(0)?);
        }
        Ok::<_, anyhow::Error>(out)
    })?;

    let mut written = 0usize;
    let file = std::fs::File::create(&out)?;
    let mut writer = std::io::BufWriter::new(file);

    for schema in &schemas {
        let events = format!("\"{schema}\".website_event");
        // **The arrival order.** The event id is the counter the server hands
        // out as the rows come in, so ordering by it is the order the traffic
        // happened in.
        let rows: Vec<String> = db.with_read(|conn| {
            let mut statement = conn.prepare(&format!("SELECT * FROM {events} ORDER BY event_id"))?;
            let mut result = statement.query([])?;
            let mut out = Vec::new();
            while let Some(row) = result.next()? {
                out.push(json_row(&sumimi_service::reports::DuckRow(&row))?);
            }
            Ok::<_, anyhow::Error>(out)
        })?;
        for row in &rows {
            writeln!(writer, "{row}")?;
            written += 1;
        }
    }
    writer.flush()?;

    println!("{} rows → {}", written, out.display());
    Ok(())
}

/// One row, as ClickHouse reads it.
///
/// **The conventions are the ones ClickHouse's columns expect**: a text
/// column is not nullable, so a missing value is the empty string; a UUID has
/// no empty string, so it is the all-zero one; a moment is the number of
/// seconds since the epoch, with the fraction.
fn json_row(row: &dyn Row) -> anyhow::Result<String> {
    let mut fields: Vec<(&str, serde_json::Value)> = Vec::new();
    for column in schema::EVENT {
        let value = match column.kind {
            Kind::Text => serde_json::Value::String(row.text(column.name)?.unwrap_or_default()),
            Kind::Uuid | Kind::EventId => serde_json::Value::String(
                row.uuid(column.name)?
                    .unwrap_or_else(uuid::Uuid::nil)
                    .to_string(),
            ),
            Kind::Blob => {
                // **The visitor's id is eight bytes** ⇒ a UInt64 holds them
                // exactly. The bytes are little-endian, so the number is read
                // the same way.
                let bytes = row.blob(column.name)?.unwrap_or_default();
                let mut raw = [0u8; 8];
                let take = bytes.len().min(8);
                raw[..take].copy_from_slice(&bytes[..take]);
                serde_json::Value::from(u64::from_le_bytes(raw))
            }
            Kind::Int | Kind::Small => {
                serde_json::Value::from(row.int(column.name)?.unwrap_or_default())
            }
            Kind::Real => serde_json::Value::from(row.float(column.name)?.unwrap_or_default()),
            Kind::Time => {
                let moment = row.time(column.name)?;
                let seconds = match moment {
                    Some(moment) => moment.and_utc().timestamp_micros() as f64 / 1_000_000.0,
                    None => 0.0,
                };
                serde_json::Value::from(seconds)
            }
            Kind::RowId => anyhow::bail!("the event table has no row id of its own"),
        };
        fields.push((column.name, value));
    }
    let object: serde_json::Map<String, serde_json::Value> = fields
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    Ok(serde_json::Value::Object(object).to_string())
}
