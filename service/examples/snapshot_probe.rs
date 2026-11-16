//! What does DuckDB do when there is more than one connection?
//!
//! This is the measurement behind the design in `service/src/pool.rs`: the DuckDB
//! backend uses **one** connection, because a second one is not safe. The three
//! facts it shows:
//!
//! 1. A connection that queried **before** a write never sees that write — not
//!    even after a checkpoint.
//! 2. A connection opened **after** the write sees it — but stops seeing it when
//!    the older connection is closed.
//! 3. Closing that older connection makes the write invisible again — but
//!    nothing is lost: after a restart the rows are there.
//!
//! ```bash
//! cargo run --release --example snapshot_probe -p sumimi-service
//! ```

use std::time::Instant;

use duckdb::Connection;

/// How many rows the event table holds.
fn count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM website_event", [], |row| row.get(0))
        .unwrap()
}

/// Write one row through the appender — the way the writer writes.
fn append(conn: &Connection) {
    let mut appender = conn.appender("website_event").unwrap();
    appender
        .append_row(duckdb::params![1i64, "w1", "/one"])
        .unwrap();
    appender.flush().unwrap();
}

fn main() -> anyhow::Result<()> {
    let path = std::env::temp_dir().join("sumimi-snapshot-probe.duckdb");
    let _ = std::fs::remove_file(&path);

    let writer = Connection::open(&path)?;
    writer.execute_batch(
        "CREATE TABLE website_event (event_id BIGINT, website_id VARCHAR, url_path VARCHAR)",
    )?;

    // A connection that queries before the write.
    let reader = Connection::open(&path)?;
    println!("1. the reader queried before the write: {}", count(&reader));

    append(&writer);
    println!("   the writer sees:                    {}", count(&writer));
    println!("   the reader still sees:               {}", count(&reader));

    let fresh = Connection::open(&path)?;
    println!("2. a connection opened after the write: {}", count(&fresh));

    // A checkpoint — the write-ahead log moves into the file.
    writer.execute_batch("CHECKPOINT")?;
    let after_checkpoint = Connection::open(&path)?;
    println!(
        "3. after a checkpoint:                  {}",
        count(&after_checkpoint)
    );

    // **And now the connection that was opened before the write is closed** —
    // the rows that were written are gone.
    drop(reader);
    let after_close = Connection::open(&path)?;
    println!("   after the old connection is closed:  {}", count(&after_close));
    drop(after_close);
    drop(after_checkpoint);
    drop(fresh);
    drop(writer);
    let reopened = Connection::open(&path)?;
    println!("   after everything is closed:          {}", count(&reopened));

    // How much does a connection cost?
    let rounds = 500;
    let started = Instant::now();
    for _ in 0..rounds {
        let _conn = Connection::open(&path)?;
    }
    println!(
        "\nopening a connection: {:.0} µs (reusing one: {:.0} µs per query)",
        started.elapsed().as_secs_f64() / rounds as f64 * 1e6,
        {
            let conn = Connection::open(&path)?;
            let started = Instant::now();
            for _ in 0..rounds {
                let _ = count(&conn);
            }
            started.elapsed().as_secs_f64() / rounds as f64 * 1e6
        }
    );

    let _ = std::fs::remove_file(&path);
    Ok(())
}
