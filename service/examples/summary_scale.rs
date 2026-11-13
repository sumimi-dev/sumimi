//! The whole summary query, at two sizes, for two shapes of the identity.
//!
//! The visitor count is one aggregate over one column; the dashboard's summary
//! is a scan of every column plus two of those aggregates. This measures the
//! whole thing — the query the dashboard actually runs — so the three shapes of
//! the identity can be compared at scale.
//!
//! The engine comparison is in `visitor_matrix`.
//!
//! ```bash
//! cargo run --release --example summary_scale -p sumimi-service
//! ```

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::pool::Db;

const VISITORS: i64 = 120_000;

/// The summary: the three numbers at the top of the dashboard.
const SUMMARY: &str = "SELECT COUNT(*) FILTER (WHERE event_type NOT IN ('event', 'identify')),
                              COUNT(DISTINCT visitor_id),
                              COUNT(DISTINCT visit_id)
                       FROM probe WHERE website_id = 'w1'";

/// The audience breakdown: the visitors per browser.
const BREAKDOWN: &str = "SELECT COALESCE(browser, 'unknown') AS name, COUNT(DISTINCT visitor_id) AS value
                          FROM probe WHERE website_id = 'w1' AND event_type NOT IN ('event', 'identify')
                          GROUP BY name ORDER BY value DESC LIMIT 50";

/// FNV-1a — SQLite has no hash of its own.
fn fnv(text: &str, seed: u64) -> u64 {
    let mut hash = seed;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn register_hash8_bytes(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sumimi_hash8_bytes",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<Vec<u8>> {
            let text = ctx.get::<String>(0)?;
            Ok(fnv(&text, 0xcbf29ce484222325u64).to_le_bytes().to_vec())
        },
    )
}

fn register_hash8(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sumimi_hash8",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<i64> {
            let text = ctx.get::<String>(0)?;
            Ok(fnv(&text, 0xcbf29ce484222325u64) as i64)
        },
    )
}

fn build(engine: &str, rows: i64, shape: &str) -> anyhow::Result<(PathBuf, f64)> {
    let path = PathBuf::from(format!("/tmp/summary-scale-{engine}-{shape}-{rows}.duckdb"));
    let _ = std::fs::remove_file(&path);
    let mut write = 0.0f64;
    if engine == "duckdb" {
        let db = Db::open(&path, "256MB")?;
        db.with_read(|conn| {
            conn.execute_batch(&format!(
                "CREATE TABLE probe (
                     id BIGINT, website_id VARCHAR, visit_id BIGINT, visitor_id {}, user_id VARCHAR,
                     created_at TIMESTAMP, url VARCHAR, url_query VARCHAR, title VARCHAR,
                     referrer VARCHAR, referrer_path VARCHAR, hostname VARCHAR, channel VARCHAR,
                     browser VARCHAR, browser_version VARCHAR, os VARCHAR, os_version VARCHAR,
                     device VARCHAR, screen VARCHAR, language VARCHAR, country VARCHAR,
                     region VARCHAR, city VARCHAR, latitude DOUBLE, longitude DOUBLE,
                     event_type VARCHAR);",
                match shape { "fixed" => "BIGINT", "wide" => "UUID", _ => "BLOB" },
            ))?;
            Ok::<_, anyhow::Error>(())
        })?;
        let started = Instant::now();
        db.with_write(|conn| {
            let sql = traffic(engine, rows, shape);
            if let Err(e) = conn.execute_batch(&sql) {
                eprintln!("SQL: {}", &sql[..sql.len().min(400)]);
                return Err(anyhow::anyhow!(e));
            }
            Ok::<_, anyhow::Error>(())
        })?;
        write = started.elapsed().as_secs_f64() * 1000.0;
        db.with_read(|conn| {
            conn.execute_batch("CHECKPOINT")?;
            Ok::<_, anyhow::Error>(())
        })?;
    } else {
        let conn = rusqlite::Connection::open(&path)?;
        register_hash8(&conn)?;
        register_hash8_bytes(&conn)?;
        conn.execute_batch(&format!(
            "CREATE TABLE probe (
                 id INTEGER, website_id TEXT, visit_id INTEGER, visitor_id {}, user_id TEXT,
                 created_at TIMESTAMP, url TEXT, url_query TEXT, title TEXT,
                 referrer TEXT, referrer_path TEXT, hostname TEXT, channel TEXT,
                 browser TEXT, browser_version TEXT, os TEXT, os_version TEXT,
                 device TEXT, screen TEXT, language TEXT, country TEXT,
                 region TEXT, city TEXT, latitude DOUBLE, longitude DOUBLE,
                 event_type TEXT);",
            match shape { "fixed" => "INTEGER", "wide" => "TEXT", _ => "BLOB" },
        ))?;
        let sql = traffic(engine, rows, shape);
        if let Err(e) = conn.execute_batch(&sql) {
            eprintln!("SQL: {}", &sql[..sql.len().min(600)]);
            return Err(anyhow::anyhow!(e));
        }
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    }
    Ok((path, write))
}

/// The traffic: the same shape the matrix uses.
fn traffic(engine: &str, rows: i64, shape: &str) -> String {
    let hash = if engine == "duckdb" {
        format!("hash('v' || (i % {VISITORS})) % 9223372036854775807")
    } else {
        format!("sumimi_hash8('v' || (i % {VISITORS}))")
    };
    let visitor = match (engine, shape) {
        ("duckdb", "fixed") => format!("CAST({hash} AS BIGINT)"),
        ("duckdb", "wide") => format!(
            "CAST(substr(sha256('v' || (i % {VISITORS})), 1, 8) || '-0000-0000-0000-000000000000' AS UUID)"
        ),
        ("duckdb", "blob6") => format!(
            "from_hex(substr(sha256('v' || (i % {VISITORS})), 1, 12))"
        ),
        ("duckdb", "blob8") => format!(
            "from_hex(substr(sha256('v' || (i % {VISITORS})), 1, 16))"
        ),
        ("duckdb", "blob9") => format!(
            "from_hex(substr(sha256('v' || (i % {VISITORS})), 1, 18))"
        ),
        ("duckdb", "blob10") => format!(
            "from_hex(substr(sha256('v' || (i % {VISITORS})), 1, 20))"
        ),
        ("duckdb", _) => format!("unhex(lpad(hex({hash}), 16, '0'))"),
        (_, "fixed") => format!("CAST({hash} AS INTEGER)"),
        (_, "wide") => format!(
            "substr(lower(hex(sumimi_hash8_bytes('v' || (i % {VISITORS}))), 1, 8) \
             || '-0000-0000-0000-000000000000')"
        ),
        (_, _) => format!("sumimi_hash8_bytes('v' || (i % {VISITORS}))"),
    };
    // The visit id: the segment's second, with the visitor's low bits.
    let visit = if engine == "duckdb" {
        format!(
            "(((i // {VISITORS}) // 4)::BIGINT << 32) | ({hash} % 4294967296)"
        )
    } else {
        format!(
            "(CAST(((i / {VISITORS}) / 4) AS INTEGER) * 4294967296 + ({hash} % 4294967296))"
        )
    };
    let rows_sql = if engine == "duckdb" {
        format!("FROM generate_series(1, {rows}) t(i)")
    } else {
        format!(
            "FROM (WITH RECURSIVE seq(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM seq WHERE x < {rows}) \
             SELECT x AS i FROM seq) t"
        )
    };
    format!(
        "INSERT INTO probe
         SELECT i AS id, 'w1' AS website_id,
            {visit} AS visit_id,
            {visitor} AS visitor_id,
            CASE WHEN i % 3 = 0 THEN NULL ELSE 'user-' || (i % {VISITORS}) END AS user_id,
            TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND AS created_at,
            '/blog/post-' || (i % 5000) AS url,
            CASE WHEN i % 7 = 0 THEN 'utm_source=twitter&utm_campaign=spring' ELSE '' END AS url_query,
            'A reasonably long article title number ' || (i % 1000) AS title,
            CASE WHEN i % 3 = 0 THEN 'https://www.google.com/search?q=some+term' ELSE '' END AS referrer,
            '' AS referrer_path, 'example.com' AS hostname, 'Search' AS channel,
            CASE (i % {VISITORS}) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                 WHEN 2 THEN 'Firefox' ELSE 'Edge' END AS browser,
            '120' AS browser_version, 'macOS' AS os, '10' AS os_version, 'desktop' AS device,
            '1920x1080' AS screen, 'en-US' AS language, 'DE' AS country, 'Berlin' AS region,
            'Berlin' AS city, 52.52 AS latitude, 13.405 AS longitude, 'pageview' AS event_type
         {rows_sql}"
    )
}

fn measure(engine: &str, path: &PathBuf, rows: i64) -> anyhow::Result<(f64, f64, f64)> {
    let bytes = std::fs::metadata(path)?.len();
    let mut summary = f64::MAX;
    let mut breakdown = f64::MAX;
    for _ in 0..3 {
        if engine == "duckdb" {
            let db = Db::open(path, "256MB")?;
            let started = Instant::now();
            let _: i64 = db.with_read(|conn| {
                let v: i64 = conn.query_row(SUMMARY, [], |r| r.get(0))?;
                Ok::<_, anyhow::Error>(v)
            })?;
            summary = summary.min(started.elapsed().as_secs_f64() * 1000.0);

            let started = Instant::now();
            db.with_read(|conn| {
                let mut stmt = conn.prepare(BREAKDOWN)?;
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
                Ok::<_, anyhow::Error>(())
            })?;
            breakdown = breakdown.min(started.elapsed().as_secs_f64() * 1000.0);
        } else {
            let conn = rusqlite::Connection::open(path)?;
            let started = Instant::now();
            let _: i64 = conn.query_row(SUMMARY, [], |r| r.get(0))?;
            summary = summary.min(started.elapsed().as_secs_f64() * 1000.0);

            let started = Instant::now();
            {
                let mut stmt = conn.prepare(BREAKDOWN)?;
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
            }
            breakdown = breakdown.min(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let _ = rows;
    Ok((bytes as f64 / rows as f64, summary, breakdown))
}

fn main() -> anyhow::Result<()> {
    println!("the whole summary query — the three numbers at the top of the dashboard\n");
    for engine in ["duckdb"] {
        println!("== {engine} ==");
        println!(
            "  {:>10} {:<8} {:>10} {:>12} {:>12} {:>12} {:>10}",
            "rows", "shape", "B/row", "the write", "summary", "browsers", "the file"
        );
        for rows in [2_000_000i64, 8_000_000] {
            for shape in ["fixed", "blob6", "blob8", "blob9", "blob10"] {
                let (path, write) = build(engine, rows, shape)?;
                let (b, summary, breakdown) = measure(engine, &path, rows)?;
                println!(
                    "  {rows:>10} {shape:<8} {:>10.2} {:>11.1}ms {:>10.1}ms {:>11.1}ms {:>10.2} GB",
                    b,
                    write,
                    summary,
                    breakdown,
                    b * rows as f64 / 1_073_741_824.0
                );
                let _ = std::fs::remove_file(&path);
            }
        }
        println!();
    }
    Ok(())
}
