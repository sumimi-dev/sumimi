//! Where does the write time go?
//!
//! The three-engine comparison says DuckDB is **slower than SQLite** on small
//! batches — the way the tracker actually writes. That is worth knowing, but
//! "slower" is not an explanation. This probe takes the write path apart and
//! times each piece:
//!
//! | piece | what it is |
//! | --- | --- |
//! | the id query | **the resume query** — the largest counter in use, asked **once per file** when it opens |
//! | the appender | the appender on `website_event` — starting an append transaction |
//! | the appends | binding 42 values per row |
//! | the flush | committing the append transaction |
//!
//! It also runs two variants, so the cost of the design choices is visible:
//!
//! * **the id counter** — the id is handed out from memory (the counter);
//! * **one appender** — the appender is created once and flushed per batch.
//!
//! **The id query left the write path.** The id is a counter in memory now
//! (the snowflake it replaces was asked for from the table on every batch,
//! which cost 0.24 ms — 18% of a small batch). What is left of it is the
//! **resume query**: the largest counter in use, asked once when the file
//! opens.
//!
//! ```bash
//! cargo run --release --example write_probe -p sumimi-service -- 100000
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use sumimi_service::models::Hit;
use sumimi_service::pool::Db;
use sumimi_service::sqlite::SqliteBackend;
use sumimi_service::storage::Storage;

/// How many rows to write.
fn rows() -> usize {
    std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(100_000)
}

/// How many rows per batch.
fn batch() -> usize {
    std::env::args()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(100)
}

/// The pieces of one write path, in microseconds.
#[derive(Debug, Default, Clone, Copy)]
struct Parts {
    id_query: Duration,
    appender: Duration,
    appends: Duration,
    flush: Duration,
}

impl Parts {
    fn total(&self) -> Duration {
        self.id_query + self.appender + self.appends + self.flush
    }

    fn ms(&self) -> f64 {
        self.total().as_secs_f64() * 1000.0
    }
}

/// The facts live in the site's own file (one file per site), so the id query
/// and the appender both name the alias that file is attached under.
fn site_table(db: &Db) -> anyhow::Result<(String, String)> {
    let alias = db.ensure_site("w1")?;
    Ok((alias.clone(), format!("{alias}.website_event")))
}

/// Write `hits` to DuckDB the way the server does it: one appender per batch,
/// the next id asked for every time.
fn duckdb_current(hits: &[Hit]) -> anyhow::Result<Parts> {
    let path = temp("duckdb-current");
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path, "256MB")?;
    let (alias, table) = site_table(&db)?;

    let mut parts = Parts::default();
    db.with_write(|conn| {
        // The resume query: once per file, not once per batch.
        let started = Instant::now();
        let counter = resume_counter(conn, &table)?;
        parts.id_query += started.elapsed();

        for chunk in hits.chunks(batch()) {
            let started = Instant::now();
            let mut appender = conn.appender_to_catalog_and_db("website_event", &alias, "main")?;
            parts.appender += started.elapsed();

            let started = Instant::now();
            for (index, hit) in chunk.iter().enumerate() {
                append_row(&mut appender, counter + index as u64, hit)?;
            }
            parts.appends += started.elapsed();

            let started = Instant::now();
            appender.flush()?;
            parts.flush += started.elapsed();
        }
        Ok::<_, anyhow::Error>(())
    })?;
    let _ = std::fs::remove_file(&path);
    Ok(parts)
}

/// The same, but the next id is kept in memory.
fn duckdb_counter(hits: &[Hit]) -> anyhow::Result<Parts> {
    let path = temp("duckdb-counter");
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path, "256MB")?;
    let (alias, table) = site_table(&db)?;

    let mut parts = Parts::default();
    db.with_write(|conn| {
        // The resume query: the largest counter in use, asked once per file.
        let started = Instant::now();
        let counter = resume_counter(conn, &table)?;
        parts.id_query += started.elapsed();
        for chunk in hits.chunks(batch()) {
            let started = Instant::now();
            let mut appender = conn.appender_to_catalog_and_db("website_event", &alias, "main")?;
            parts.appender += started.elapsed();

            let started = Instant::now();
            for (index, hit) in chunk.iter().enumerate() {
                append_row(&mut appender, counter + index as u64, hit)?;
            }
            parts.appends += started.elapsed();

            let started = Instant::now();
            appender.flush()?;
            parts.flush += started.elapsed();
        }
        Ok::<_, anyhow::Error>(())
    })?;
    let _ = std::fs::remove_file(&path);
    Ok(parts)
}

/// The same, but the appender is created **once** and flushed per batch.
fn duckdb_one_appender(hits: &[Hit]) -> anyhow::Result<Parts> {
    let path = temp("duckdb-one-appender");
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path, "256MB")?;
    let (alias, table) = site_table(&db)?;

    let mut parts = Parts::default();
    db.with_write(|conn| {
        // The resume query: the largest counter in use, asked once per file.
        let started = Instant::now();
        let counter = resume_counter(conn, &table)?;
        parts.id_query += started.elapsed();
        let mut appender = conn.appender_to_catalog_and_db("website_event", &alias, "main")?;
        for chunk in hits.chunks(batch()) {
            let started = Instant::now();
            for (index, hit) in chunk.iter().enumerate() {
                append_row(&mut appender, counter + index as u64, hit)?;
            }
            parts.appends += started.elapsed();

            let started = Instant::now();
            appender.flush()?;
            parts.flush += started.elapsed();
        }
        Ok::<_, anyhow::Error>(())
    })?;
    let _ = std::fs::remove_file(&path);
    Ok(parts)
}

/// The SQLite backend, for the comparison.
fn sqlite(hits: &[Hit]) -> anyhow::Result<(f64, u64)> {
    let path = temp("sqlite");
    let _ = std::fs::remove_file(&path);
    let backend = SqliteBackend::open(&path)?;

    let started = Instant::now();
    for chunk in hits.chunks(batch()) {
        backend.write_batch(chunk)?;
    }
    let seconds = started.elapsed().as_secs_f64();
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&path);
    Ok((seconds, size))
}

/// Append one row, in the column order of the schema.
fn append_row(
    appender: &mut duckdb::Appender<'_>,
    counter: u64,
    hit: &Hit,
) -> anyhow::Result<()> {
    // The row is the server's row: the columns and their order come from the
    // schema, so this cannot drift away from it.
    let kind = hit.kind.as_str();
    let id = sumimi_service::event_id::compose(7, counter);
    let session = sumimi_service::ua::visitor_bytes(&hit.visitor_id);
    let row = sumimi_service::db::event_row(&id, hit, &kind, &session);
    appender.append_row(&row[..])?;
    Ok(())
}

/// The largest counter in use — the query that runs when a file opens.
fn resume_counter(conn: &duckdb::Connection, table: &str) -> anyhow::Result<u64> {
    let low = sumimi_service::event_id::compose(7, 0);
    let high = sumimi_service::event_id::compose(7, sumimi_service::event_id::COUNTER_MAX);
    let id: Option<uuid::Uuid> = conn.query_row(
        &format!("SELECT MAX(event_id) FROM {table} WHERE event_id BETWEEN ?1 AND ?2"),
        [&low, &high],
        |row| row.get(0),
    )?;
    Ok(id.map(|id| sumimi_service::event_id::counter_of(&id)).unwrap_or(0))
}

/// How expensive is the resume query on a table of `rows` rows?
///
/// The schema comment says the id is handed out by `MAX(event_id) + 1`, "answered from
/// the column statistics in constant time". This checks that claim.
fn id_query_cost(rows: usize) -> anyhow::Result<(f64, f64)> {
    let path = temp(&format!("id-cost-{rows}"));
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path, "256MB")?;
    let (alias, table) = site_table(&db)?;

    let hits = generate(rows as i64);
    db.with_write(|conn| {
        let mut appender = conn.appender_to_catalog_and_db("website_event", &alias, "main")?;
        for (index, hit) in hits.iter().enumerate() {
            append_row(&mut appender, 1 + index as u64, hit)?;
        }
        appender.flush()?;
        Ok::<_, anyhow::Error>(())
    })?;

    let (plain, coalesced) = db.with_read(|conn| {
        // The plain aggregate — the whole column.
        let started = Instant::now();
        let _: Option<uuid::Uuid> =
            conn.query_row(&format!("SELECT MAX(event_id) FROM {table}"), [], |row| row.get(0))?;
        let plain = started.elapsed().as_secs_f64() * 1000.0;

        // The one the server actually runs: the range that covers this
        // instance's own ids.
        let started = Instant::now();
        let _ = resume_counter(conn, &table)?;
        let coalesced = started.elapsed().as_secs_f64() * 1000.0;
        Ok::<_, anyhow::Error>((plain, coalesced))
    })?;

    let _ = std::fs::remove_file(&path);
    Ok((plain, coalesced))
}

fn main() -> anyhow::Result<()> {
    let total = rows();
    let per_batch = batch();
    let hits = generate(total as i64);

    println!(
        "{total} rows, {per_batch} per batch, the production schema ({} columns)\n",
        sumimi_service::db::event_columns().len()
    );

    let current = duckdb_current(&hits)?;
    let counter = duckdb_counter(&hits)?;
    let one = duckdb_one_appender(&hits)?;
    let (sqlite_seconds, sqlite_size) = sqlite(&hits)?;

    println!(
        "{:<24} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "path", "total", "id query", "appender", "appends", "flush"
    );
    println!("{}", "-".repeat(78));
    for (name, parts) in [
        ("the server, today", &current),
        ("the id counter", &counter),
        ("one appender", &one),
    ] {
        println!(
            "{:<24} {:>8.0} ms {:>8.0} ms {:>8.0} ms {:>8.0} ms {:>8.0} ms",
            name,
            parts.ms(),
            parts.id_query.as_secs_f64() * 1000.0,
            parts.appender.as_secs_f64() * 1000.0,
            parts.appends.as_secs_f64() * 1000.0,
            parts.flush.as_secs_f64() * 1000.0,
        );
    }
    println!(
        "{:<24} {:>8.0} ms {:>30}",
        "SQLite",
        sqlite_seconds * 1000.0,
        format!("{} B/row", sqlite_size as f64 / total as f64)
    );

    println!();
    println!("the id query, on a table of N rows");
    println!("{}", "-".repeat(78));
    println!(
        "{:<24} {:>14} {:>20}",
        "rows", "MAX(event_id)", "the resume query"
    );
    for rows in [100usize, 10_000, 100_000, 1_000_000] {
        let (plain, coalesced) = id_query_cost(rows)?;
        println!("{:<24} {:>11.3} ms {:>17.3} ms", rows, plain, coalesced);
    }

    println!();
    println!("per row, in microseconds");
    println!("{}", "-".repeat(78));
    for (name, parts) in [
        ("the server, today", &current),
        ("the id counter", &counter),
        ("one appender", &one),
    ] {
        println!(
            "{:<24} {:>8.2} us {:>8.2} us {:>8.2} us {:>8.2} us",
            name,
            parts.id_query.as_secs_f64() * 1e6 / total as f64,
            parts.appender.as_secs_f64() * 1e6 / total as f64,
            parts.appends.as_secs_f64() * 1e6 / total as f64,
            parts.flush.as_secs_f64() * 1e6 / total as f64,
        );
    }
    println!(
        "{:<24} {:>8.2} us",
        "SQLite",
        sqlite_seconds * 1e6 / total as f64
    );

    Ok(())
}

/// A temporary path.
fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-write-probe-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("sumimi.data")
}

/// Traffic that looks like a site's.
fn generate(views: i64) -> Vec<Hit> {
    sumimi_service::testing::traffic(views)
}
