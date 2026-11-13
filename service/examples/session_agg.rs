//! What does `MIN(created_at) GROUP BY visit_id` cost?
//!
//! The session id is random per session now, so nothing carries the session's
//! start. The report can work it out — `MIN(created_at)` per session — or the
//! row can carry it in its own column. This measures the first against the
//! second, and against doing neither.
//!
//! ```bash
//! cargo run --release --example session_agg -p sumimi-service -- 10000000
//! ```

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::pool::Db;

const VISITORS: i64 = 1_000_000;
const EVENTS_PER_VISITOR: i64 = 30;

/// The queries: the grouping alone, then with the aggregates.
const QUERIES: [(&str, &str); 6] = [
    (
        "the grouping alone",
        "SELECT visit_id, COUNT(*) FROM probe GROUP BY 1",
    ),
    (
        "+ MIN(created_at)",
        "SELECT visit_id, COUNT(*), MIN(created_at) FROM probe GROUP BY 1",
    ),
    (
        "+ MIN + MAX",
        "SELECT visit_id, COUNT(*), MIN(created_at), MAX(created_at) FROM probe GROUP BY 1",
    ),
    (
        "+ MIN, the report's shape (top 50)",
        "SELECT visit_id, COUNT(*), MIN(created_at), MAX(created_at) FROM probe \
         GROUP BY 1 ORDER BY 2 DESC LIMIT 50",
    ),
    (
        "+ MIN, over one day",
        "SELECT visit_id, COUNT(*), MIN(created_at), MAX(created_at) FROM probe \
         WHERE created_at >= TIMESTAMP '2026-01-05' GROUP BY 1",
    ),
    (
        "+ MIN, over one day, top 50",
        "SELECT visit_id, COUNT(*), MIN(created_at), MAX(created_at) FROM probe \
         WHERE created_at >= TIMESTAMP '2026-01-05' GROUP BY 1 ORDER BY 2 DESC LIMIT 50",
    ),
];

fn time(conn: &duckdb::Connection, sql: &str) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        let mut stmt = match conn.prepare(sql) {
            Ok(stmt) => stmt,
            Err(_) => return f64::NAN,
        };
        let mut rows = match stmt.query([]) {
            Ok(rows) => rows,
            Err(_) => return f64::NAN,
        };
        loop {
            match rows.next() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => return f64::NAN,
            }
        }
        best = best.min(started.elapsed().as_secs_f64() * 1000.0);
    }
    best
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000_000);
    println!("{rows} rows, {VISITORS} visitors, {EVENTS_PER_VISITOR} events each\n");

    for (label, ddl, value) in [
        (
            "the session id is 8 bytes",
            "visit_id BIGINT",
            format!("(i / {EVENTS_PER_VISITOR})"),
        ),
        (
            "the session id is 16 bytes",
            "visit_id UUID",
            format!(
                "CAST(substr(sha256('s' || (i / {EVENTS_PER_VISITOR})), 1, 8) || '-' || \
                 substr(sha256('s' || (i / {EVENTS_PER_VISITOR})), 9, 4) || '-' || \
                 substr(sha256('s' || (i / {EVENTS_PER_VISITOR})), 13, 4) || '-' || \
                 substr(sha256('s' || (i / {EVENTS_PER_VISITOR})), 17, 4) || '-' || \
                 substr(sha256('s' || (i / {EVENTS_PER_VISITOR})), 21, 12) AS UUID)"
            ),
        ),
    ] {
        let path = PathBuf::from("/tmp/session-agg.duckdb");
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path, "512MB")?;
        db.with_read(|conn| {
            conn.execute_batch(&format!(
                "CREATE TABLE probe (id BIGINT, website_id VARCHAR, {ddl}, visitor_id BIGINT, \
                 created_at TIMESTAMP, url VARCHAR, browser VARCHAR);"
            ))?;
            Ok::<_, anyhow::Error>(())
        })?;
        db.with_write(|conn| {
            conn.execute_batch(&format!(
                "INSERT INTO probe
                 SELECT i AS id, 'w1', {value},
                    CAST(hash('v' || (i / {EVENTS_PER_VISITOR})) % 9223372036854775807 AS BIGINT),
                    TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND,
                    '/blog/post-' || (i % 5000),
                    CASE (i % 4) WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                         WHEN 2 THEN 'Firefox' ELSE 'Edge' END
                 FROM generate_series(1, {rows}) t(i)"
            ))?;
            Ok::<_, anyhow::Error>(())
        })?;
        db.with_read(|conn| {
            conn.execute_batch("CHECKPOINT")?;
            Ok::<_, anyhow::Error>(())
        })?;
        drop(db);
        let bytes = std::fs::metadata(&path)?.len() as f64 / rows as f64;

        let conn = duckdb::Connection::open(&path)?;
        println!("  === {label} ===   {bytes:.2} B/row");
        for (name, query) in QUERIES {
            let ms = time(&conn, query);
            println!("      {name:<40} {ms:>9.1}ms");
        }
        let _ = std::fs::remove_file(&path);
        println!();
    }
    Ok(())
}
