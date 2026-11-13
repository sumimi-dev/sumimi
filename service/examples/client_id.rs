//! The client brings its own id. What does that cost?
//!
//! Three ways to hold the visitor identity when the site sends a `user_id`:
//!
//! | design | the identity column | the client's own id |
//! | --- | --- | --- |
//! | `fixed` | BIGINT — every visitor is a hash | dropped |
//! | `fixed + kept` | BIGINT — every visitor is a hash | kept in its own column |
//! | `one column` | VARCHAR — the client's id as-is, ours as base64 | mixed in |
//!
//! The traffic: 120 000 visitors, a third of them logged in.

use std::path::PathBuf;
use std::time::Instant;

const ROWS: i64 = 2_000_000;
const VISITORS: i64 = 120_000;

/// The columns every design shares.
const COMMON: &str = "website_id VARCHAR, visit_id BIGINT, event_type VARCHAR, \
     browser VARCHAR, browser_version VARCHAR, os VARCHAR, os_version VARCHAR, \
     device VARCHAR, screen VARCHAR, language VARCHAR, country VARCHAR, region VARCHAR, \
     city VARCHAR, latitude DOUBLE, longitude DOUBLE, url VARCHAR, url_query VARCHAR, title VARCHAR";

const SUMMARY: &str = "SELECT COUNT(*) FILTER (WHERE event_type NOT IN ('event', 'identify')),
                              COUNT(DISTINCT visitor_id),
                              COUNT(DISTINCT visit_id)
                       FROM event WHERE website_id = 'w1'";

const BREAKDOWN: &str = "SELECT COALESCE(browser, 'unknown') AS name, COUNT(DISTINCT visitor_id) AS value
                          FROM event WHERE website_id = 'w1' AND event_type NOT IN ('event', 'identify')
                          GROUP BY name ORDER BY value DESC LIMIT 50";

/// FNV-1a, with a seed — SQLite has no hash of its own.
fn fnv(text: &str, seed: u64) -> u64 {
    let mut hash = seed;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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

/// Base64 without the padding — eleven characters for eight bytes.
fn register_b64(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sumimi_b64",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<String> {
            let value = ctx.get::<i64>(0)?;
            let bytes = value.to_le_bytes();
            const ALPHABET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
                out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
                if chunk.len() > 1 {
                    out.push(ALPHABET[(n >> 6 & 63) as usize] as char);
                }
                if chunk.len() > 2 {
                    out.push(ALPHABET[(n & 63) as usize] as char);
                }
            }
            Ok(out)
        },
    )
}

/// The three designs: the columns, and the SQL that fills them.
fn designs(engine: &str) -> Vec<(&'static str, String, String)> {
    let visitor = if engine == "duckdb" {
        format!("(i % {VISITORS})")
    } else {
        format!("(x % {VISITORS})")
    };
    let logged_in = format!("({visitor} % 10) < 3");
    let address = if engine == "duckdb" {
        format!("('203.0.113.' || CAST({visitor} % 40000 AS VARCHAR))")
    } else {
        format!("('203.0.113.' || CAST({visitor} % 40000 AS TEXT))")
    };
    let divide = if engine == "duckdb" { "//" } else { "/" };
    let agent = format!("CASE ({visitor} {divide} 40000) % 4 WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36' \
         WHEN 1 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36' \
         WHEN 2 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15' \
         ELSE 'Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0' END");
    let user_id = if engine == "duckdb" {
        format!(
            "CASE WHEN {logged_in} THEN CAST(substr(sha256('user-' || {visitor}), 1, 8) || '-' || \
             substr(sha256('user-' || {visitor}), 9, 4) || '-' || \
             substr(sha256('user-' || {visitor}), 13, 4) || '-' || \
             substr(sha256('user-' || {visitor}), 17, 4) || '-' || \
             substr(sha256('user-' || {visitor}), 21, 12) AS VARCHAR) ELSE NULL END"
        )
    } else {
        format!(
            "CASE WHEN {logged_in} THEN (substr(lower(hex(sumimi_hash8('user-' || {visitor}))), 1, 8) || '-' || \
             substr(lower(hex(sumimi_hash8('user-' || {visitor}))), 9, 4) || '-4' || \
             substr(lower(hex(sumimi_hash8('user-' || {visitor}))), 14, 3) || '-8' || \
             substr(lower(hex(sumimi_hash8('user-' || {visitor}))), 18, 3) || '-' || \
             substr(lower(hex(sumimi_hash8('user-' || {visitor}))), 21, 12)) ELSE NULL END"
        )
    };
    // The identity: logged-in visitors are hashed from their own id, everyone
    // else from the address and the agent.
    let hash_of = if engine == "duckdb" {
        format!("hash({address} || chr(31) || {agent})")
    } else {
        format!("sumimi_hash8({address} || char(31) || {agent})")
    };
    let hash_of_user = if engine == "duckdb" {
        format!("hash('u' || {visitor})")
    } else {
        format!("sumimi_hash8('u' || {visitor})")
    };

    match engine {
        "duckdb" => vec![
            (
                "fixed (BIGINT only)",
                format!("visitor_id UBIGINT, {COMMON}"),
                format!(
                    "SELECT CASE WHEN {logged_in} THEN {hash_of_user} ELSE {hash_of} END AS visitor_id, \
                     'w1', i, 'pageview', 'Chrome', '120', 'macOS', '10', 'desktop', '1920x1080', \
                     'en-US', 'DE', 'Berlin', 'Berlin', 52.52, 13.405, '/blog/post', '', 'A title' \
                     FROM generate_series(1, {ROWS}) t(i)"
                ),
            ),
            (
                "fixed + the client's id kept",
                format!("visitor_id UBIGINT, user_id VARCHAR, {COMMON}"),
                format!(
                    "SELECT CASE WHEN {logged_in} THEN {hash_of_user} ELSE {hash_of} END AS visitor_id, \
                     {user_id} AS user_id, \
                     'w1', i, 'pageview', 'Chrome', '120', 'macOS', '10', 'desktop', '1920x1080', \
                     'en-US', 'DE', 'Berlin', 'Berlin', 52.52, 13.405, '/blog/post', '', 'A title' \
                     FROM generate_series(1, {ROWS}) t(i)"
                ),
            ),
            (
                "one column (text)",
                format!("visitor_id VARCHAR, {COMMON}"),
                format!(
                    "SELECT CASE WHEN {logged_in} THEN {user_id} ELSE \
                     rtrim(base64(unhex(lpad(hex({hash_of}), 16, '0'))), '=') END AS visitor_id, \
                     'w1', i, 'pageview', 'Chrome', '120', 'macOS', '10', 'desktop', '1920x1080', \
                     'en-US', 'DE', 'Berlin', 'Berlin', 52.52, 13.405, '/blog/post', '', 'A title' \
                     FROM generate_series(1, {ROWS}) t(i)"
                ),
            ),
        ],
        _ => vec![
            (
                "fixed (BIGINT only)",
                format!("visitor_id INTEGER, {COMMON}"),
                format!(
                    "SELECT CASE WHEN {logged_in} THEN {hash_of_user} ELSE {hash_of} END AS visitor_id, \
                     'w1', x, 'pageview', 'Chrome', '120', 'macOS', '10', 'desktop', '1920x1080', \
                     'en-US', 'DE', 'Berlin', 'Berlin', 52.52, 13.405, '/blog/post', '', 'A title' \
                     FROM (WITH RECURSIVE seq(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM seq WHERE x < {ROWS}) \
                     SELECT x FROM seq) t"
                ),
            ),
            (
                "fixed + the client's id kept",
                format!("visitor_id INTEGER, user_id VARCHAR, {COMMON}"),
                format!(
                    "SELECT CASE WHEN {logged_in} THEN {hash_of_user} ELSE {hash_of} END AS visitor_id, \
                     {user_id} AS user_id, \
                     'w1', x, 'pageview', 'Chrome', '120', 'macOS', '10', 'desktop', '1920x1080', \
                     'en-US', 'DE', 'Berlin', 'Berlin', 52.52, 13.405, '/blog/post', '', 'A title' \
                     FROM (WITH RECURSIVE seq(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM seq WHERE x < {ROWS}) \
                     SELECT x FROM seq) t"
                ),
            ),
            (
                "one column (text)",
                format!("visitor_id VARCHAR, {COMMON}"),
                format!(
                    "SELECT CASE WHEN {logged_in} THEN {user_id} ELSE \
                     sumimi_b64({hash_of}) END AS visitor_id, \
                     'w1', x, 'pageview', 'Chrome', '120', 'macOS', '10', 'desktop', '1920x1080', \
                     'en-US', 'DE', 'Berlin', 'Berlin', 52.52, 13.405, '/blog/post', '', 'A title' \
                     FROM (WITH RECURSIVE seq(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM seq WHERE x < {ROWS}) \
                     SELECT x FROM seq) t"
                ),
            ),
        ],
    }
}

/// "How many logged-in visitors" — the query the owner would write.
fn logged_in_sql(engine: &str, design: &str) -> Option<String> {
    match (engine, design) {
        ("duckdb", "fixed (BIGINT only)") => Some(
            "SELECT CAST(count(DISTINCT visitor_id) AS VARCHAR) FROM event \
             WHERE visitor_id IN (SELECT hash('u' || (i % 120000)) FROM generate_series(0, 119999) t(i))"
                .to_string(),
        ),
        ("duckdb", "fixed + the client's id kept") => Some(
            "SELECT CAST(count(DISTINCT user_id) AS VARCHAR) FROM event WHERE user_id IS NOT NULL"
                .to_string(),
        ),
        ("duckdb", _) => Some(
            "SELECT CAST(count(DISTINCT visitor_id) AS VARCHAR) FROM event WHERE length(visitor_id) > 20"
                .to_string(),
        ),
        (_, "fixed (BIGINT only)") => Some(
            "SELECT CAST(count(DISTINCT visitor_id) AS TEXT) FROM event \
             WHERE visitor_id IN (SELECT sumimi_hash8('u' || (x % 120000)) FROM \
             (WITH RECURSIVE seq(x) AS (SELECT 0 UNION ALL SELECT x+1 FROM seq WHERE x < 119999) SELECT x FROM seq) t)"
                .to_string(),
        ),
        (_, "fixed + the client's id kept") => Some(
            "SELECT CAST(count(DISTINCT user_id) AS TEXT) FROM event WHERE user_id IS NOT NULL"
                .to_string(),
        ),
        (_, _) => Some(
            "SELECT CAST(count(DISTINCT visitor_id) AS TEXT) FROM event WHERE length(visitor_id) > 20"
                .to_string(),
        ),
    }
}

fn main() -> anyhow::Result<()> {
    println!("{ROWS} rows, {VISITORS} visitors, a third of them logged in\n");

    for (engine, label) in [("duckdb", "DuckDB"), ("sqlite", "SQLite")] {
        println!("== {label} ==");
        println!(
            "  {:<32} {:>10} {:>12} {:>12} {:>10} {:>14}",
            "the identity", "B/row", "summary", "browsers", "visitors", "logged in"
        );

        for (name, columns, insert) in designs(engine) {
            let path = PathBuf::from(format!("/tmp/client-id-{engine}.db"));
            let _ = std::fs::remove_file(&path);

            if engine == "duckdb" {
                let conn = duckdb::Connection::open(&path)?;
                conn.execute_batch(&format!("CREATE TABLE event ({columns});"))?;
                conn.execute_batch(&format!("INSERT INTO event {insert};"))?;
                conn.execute_batch("CHECKPOINT")?;
                drop(conn);
            } else {
                let conn = rusqlite::Connection::open(&path)?;
                register_hash8(&conn)?;
                register_b64(&conn)?;
                conn.execute_batch(&format!("CREATE TABLE event ({columns});"))?;
                conn.execute_batch(&format!("INSERT INTO event {insert};"))?;
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
                drop(conn);
            }

            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

            let mut summary_ms = f64::MAX;
            let mut browsers_ms = f64::MAX;
            let mut visitors = 0i64;
            for _ in 0..3 {
                if engine == "duckdb" {
                    let conn = duckdb::Connection::open(&path)?;
                    let started = Instant::now();
                    let _: i64 = conn.query_row(SUMMARY, [], |r| r.get(0))?;
                    summary_ms = summary_ms.min(started.elapsed().as_secs_f64() * 1000.0);

                    let started = Instant::now();
                    {
                        let mut stmt = conn.prepare(BREAKDOWN)?;
                        let mut rows = stmt.query([])?;
                        while rows.next()?.is_some() {}
                    }
                    browsers_ms = browsers_ms.min(started.elapsed().as_secs_f64() * 1000.0);

                    visitors = conn.query_row(SUMMARY, [], |r| r.get::<_, i64>(1))?;
                } else {
                    let conn = rusqlite::Connection::open(&path)?;
                    let started = Instant::now();
                    let _: i64 = conn.query_row(SUMMARY, [], |r| r.get(0))?;
                    summary_ms = summary_ms.min(started.elapsed().as_secs_f64() * 1000.0);

                    let started = Instant::now();
                    {
                        let mut stmt = conn.prepare(BREAKDOWN)?;
                        let mut rows = stmt.query([])?;
                        while rows.next()?.is_some() {}
                    }
                    browsers_ms = browsers_ms.min(started.elapsed().as_secs_f64() * 1000.0);

                    visitors = conn.query_row(SUMMARY, [], |r| r.get::<_, i64>(1))?;
                }
            }

            let logged_in = if let Some(sql) = logged_in_sql(engine, name) {
                let value: String = if engine == "duckdb" {
                    let conn = duckdb::Connection::open(&path)?;
                    conn.query_row(&sql, [], |r| r.get(0))?
                } else {
                    let conn = rusqlite::Connection::open(&path)?;
                    register_hash8(&conn)?;
                    conn.query_row(&sql, [], |r| r.get(0))?
                };
                value
            } else {
                "-".to_string()
            };

            println!(
                "  {:<32} {:>10.1} {:>10.1}ms {:>10.1}ms {:>10} {:>14}",
                name,
                bytes as f64 / ROWS as f64,
                summary_ms,
                browsers_ms,
                visitors,
                logged_in
            );
            let _ = std::fs::remove_file(&path);
        }
        println!();
    }
    Ok(())
}
