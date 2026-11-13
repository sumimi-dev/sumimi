//! The visitor identity: a hash, or the raw thing it is made of?
//!
//! The hash has one job — a fixed length. The question is what it costs:
//! a hash is random-looking, so the column compresses badly; the raw pair
//! (the address prefix, the user agent) repeats, so the column compresses
//! well. Which wins?
//!
//! The same rows, written four ways, in both engines:
//!
//! | variant | what the visitor identity is |
//! | --- | --- |
//! | `hash` | 16 bytes, random-looking (the HMAC) |
//! | `pair` | the address prefix + the user agent, two columns |
//! | `raw` | the address + the user agent, two columns |
//! | `one string` | the prefix and the agent glued into one column |
//!
//! For each: the file size, and the time the "how many visitors" query takes.

use std::path::PathBuf;
use std::time::Instant;

/// How many rows.
const ROWS: i64 = 2_000_000;

/// The columns every variant shares.
const COMMON: &str = "browser VARCHAR, browser_version VARCHAR, os VARCHAR, os_version VARCHAR, \
     device VARCHAR, screen VARCHAR, language VARCHAR, country VARCHAR, region VARCHAR, \
     city VARCHAR, latitude DOUBLE, longitude DOUBLE, url VARCHAR, url_query VARCHAR, title VARCHAR";

/// The identity columns of each variant.
fn identity(kind: &str) -> &'static str {
    match kind {
        "hash" => "visitor_id UUID",
        "pair" => "ip_prefix VARCHAR, user_agent VARCHAR",
        "raw" => "ip VARCHAR, user_agent VARCHAR",
        _ => "visitor_id VARCHAR",
    }
}

/// The values of the identity columns of each variant.
///
/// SQLite has no `sha256`, so the hash there is sixteen random bytes — the
/// shape is the same (random-looking, 16 bytes), which is what the column's
/// cost depends on.
fn values(kind: &str, engine: &str) -> String {
    // The address: one per visitor (the benchmark's 40 000), the prefix: the
    // same count here, the agent: a handful.
    let ip = "'203.0.113.' || CAST(i % 40000 AS TEXT)";
    let prefix = "'203.0.113.' || CAST(i % 40000 AS TEXT)";
    let agent = "CASE i % 4 WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36' \
         WHEN 1 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36' \
         WHEN 2 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15' \
         ELSE 'Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0' END";
    match kind {
        // A hash: random-looking, 16 bytes, no way to tell two visitors apart
        // without the secret.
        "hash" if engine == "duckdb" => "CAST(substr(sha256(CAST(i AS VARCHAR)), 1, 8) || '-' || \
             substr(sha256(CAST(i AS VARCHAR)), 9, 4) || '-' || \
             substr(sha256(CAST(i AS VARCHAR)), 13, 4) || '-' || \
             substr(sha256(CAST(i AS VARCHAR)), 17, 4) || '-' || \
             substr(sha256(CAST(i AS VARCHAR)), 21, 12) AS UUID)"
            .to_string(),
        "hash" => "randomblob(16)".to_string(),
        "pair" => format!("{prefix}, {agent}"),
        "raw" => format!("{ip}, {agent}"),
        // One column, the pair glued together.
        _ => format!("{prefix} || ' ' || {agent}"),
    }
}

/// The other columns of every variant.
fn common_values() -> String {
    "'Chrome', '120', 'macOS', '10', 'desktop', '1920x1080', 'en-US', 'DE', 'Berlin', 'Berlin', \
     52.52, 13.405, '/blog/post', 'utm_source=twitter', 'A reasonably long article title'"
        .to_string()
}

/// The "how many visitors" query of each variant.
///
/// The two engines spell the pair differently: DuckDB takes a row, SQLite has
/// no row values — it concatenates.
fn distinct(kind: &str, engine: &str) -> String {
    match kind {
        "hash" => "SELECT count(DISTINCT visitor_id) FROM event".to_string(),
        "pair" | "raw" => {
            let address = if kind == "pair" { "ip_prefix" } else { "ip" };
            if engine == "duckdb" {
                format!("SELECT count(DISTINCT ({address}, user_agent)) FROM event")
            } else {
                format!("SELECT count(DISTINCT {address} || char(31) || user_agent) FROM event")
            }
        }
        _ => "SELECT count(DISTINCT visitor_id) FROM event".to_string(),
    }
}

fn main() -> anyhow::Result<()> {
    let kinds = ["hash", "pair", "raw", "one string"];

    for (engine, name) in [("duckdb", "DuckDB"), ("sqlite", "SQLite")] {
        println!("{name} — {ROWS} rows");
        println!(
            "  {:<12} {:>14} {:>10} {:>14}",
            "the identity", "file", "B/row", "the count (ms)"
        );
        for kind in kinds {
            let path = PathBuf::from(format!(
                "/tmp/visitor-cost-{engine}-{}.tmp",
                kind.replace(' ', "-")
            ));
            let _ = std::fs::remove_file(&path);
            let size = if engine == "duckdb" {
                let conn = duckdb::Connection::open(&path)?;
                conn.execute_batch(&format!(
                    "CREATE TABLE event (id BIGINT, website_id VARCHAR, visit_id BIGINT, {}, {COMMON});",
                    identity(kind)
                ))?;
                let sql = format!(
                    "INSERT INTO event SELECT i, 'w1', i, {}, {} \
                     FROM generate_series(1, {ROWS}) t(i);",
                    values(kind, engine),
                    common_values()
                );
                conn.execute_batch(&sql)?;
                conn.execute_batch("CHECKPOINT")?;
                drop(conn);
                std::fs::metadata(&path)?.len()
            } else {
                let conn = rusqlite::Connection::open(&path)?;
                conn.execute_batch(&format!(
                    "CREATE TABLE event (id BIGINT, website_id VARCHAR, visit_id BIGINT, {}, {COMMON});",
                    identity(kind).replace("BIGINT", "INTEGER").replace("UUID", "BLOB")
                ))?;
                let sql = format!(
                    "INSERT INTO event SELECT i, 'w1', i, {}, {} \
                     FROM (WITH RECURSIVE seq(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM seq WHERE x < {ROWS}) \
                     SELECT x AS i FROM seq) t;",
                    values(kind, engine),
                    common_values()
                );
                conn.execute_batch(&sql)?;
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
                drop(conn);
                std::fs::metadata(&path)?.len()
            };

            // The time the report's visitor count takes.
            let mut best = f64::MAX;
            for _ in 0..3 {
                let started = Instant::now();
                if engine == "duckdb" {
                    let conn = duckdb::Connection::open(&path)?;
                    let _: i64 = conn.query_row(&distinct(kind, engine), [], |r| r.get(0))?;
                    drop(conn);
                } else {
                    let conn = rusqlite::Connection::open(&path)?;
                    let _: i64 = conn.query_row(&distinct(kind, engine), [], |r| r.get(0))?;
                    drop(conn);
                }
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                if ms < best {
                    best = ms;
                }
            }

            println!(
                "  {:<12} {:>12} MB {:>10.1} {:>14.1}",
                kind,
                size as f64 / 1_048_576.0,
                size as f64 / ROWS as f64,
                best
            );
            let _ = std::fs::remove_file(&path);
        }
        println!();
    }
    Ok(())
}
