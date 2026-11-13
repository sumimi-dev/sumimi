//! What do the empty columns cost — NULL, or an empty string?
//!
//! The raw IP, the prefix and the user agent are stored only when the owner
//! asks for it. When they are not, the columns are there and empty. "Empty"
//! can mean two things, and they do not cost the same: a **NULL** (the value
//! is not there) or an **empty string** (the value is there and it is "").
//!
//! This measures both, on the same rows, in both engines.
use std::path::PathBuf;

const ROWS: i64 = 200_000;

fn size(path: &PathBuf) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// The rows: the same in every variant.
fn rows(with_values: bool) -> String {
    // The addresses vary (a visitor per address, a network per prefix) and
    // the agent has a handful of values — the shape real traffic has.
    let (ip, prefix, agent) = if with_values {
        (
            "'203.0.113.' || CAST(i % 40000 AS TEXT)",
            "'203.0.113.' || CAST(i % 400 AS TEXT)",
            "CASE i % 4 WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36' \
             WHEN 1 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36' \
             WHEN 2 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15' \
             ELSE 'Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0' END",
        )
    } else {
        ("NULL", "NULL", "NULL")
    };
    format!(
        "SELECT i, 'w1', i, {ip}, {prefix}, {agent}, 'Chrome', '120', 'macOS', '10', 'desktop', \
         '1920x1080', 'en-US', 'DE', 'Berlin', 'Berlin', 52.52, 13.405, '/blog/post', \
         'utm_source=twitter', 'A reasonably long article title' \
         FROM generate_series(1, {ROWS}) t(i)"
    )
}

/// The rows without the three columns.
fn rows_without() -> String {
    format!(
        "SELECT i, 'w1', i, 'Chrome', '120', 'macOS', '10', 'desktop', \
         '1920x1080', 'en-US', 'DE', 'Berlin', 'Berlin', 52.52, 13.405, '/blog/post', \
         'utm_source=twitter', 'A reasonably long article title' \
         FROM generate_series(1, {ROWS}) t(i)"
    )
}

/// The table: the same in every variant.
const COLUMNS: &str = "id INTEGER, website_id TEXT, visit_id INTEGER, ip TEXT, ip_prefix TEXT, \
     user_agent TEXT, browser TEXT, browser_version TEXT, os TEXT, os_version TEXT, \
     device TEXT, screen TEXT, language TEXT, country TEXT, region TEXT, city TEXT, \
     latitude DOUBLE, longitude DOUBLE, url TEXT, url_query TEXT, title TEXT";

/// The same table without the three columns at all.
const COLUMNS_WITHOUT: &str = "id INTEGER, website_id TEXT, visit_id INTEGER, \
     browser TEXT, browser_version TEXT, os TEXT, os_version TEXT, \
     device TEXT, screen TEXT, language TEXT, country TEXT, region TEXT, city TEXT, \
     latitude DOUBLE, longitude DOUBLE, url TEXT, url_query TEXT, title TEXT";

fn main() -> anyhow::Result<()> {
    // --- DuckDB ---
    let mut duck = Vec::new();
    for (label, with_values) in [
        ("filled", true),
        ("empty string", false),
        ("null", false),
        ("no columns", false),
    ] {
        let path = PathBuf::from(format!(
            "/tmp/nullcost-duck-{}.duckdb",
            label.replace(' ', "-")
        ));
        let _ = std::fs::remove_file(&path);
        let conn = duckdb::Connection::open(&path)?;
        let columns = if label == "no columns" {
            COLUMNS_WITHOUT
        } else {
            COLUMNS
        };
        conn.execute_batch(&format!("CREATE TABLE event ({columns});"))?;
        // The empty-string variant: the values are there and they are "".
        let sql = if label == "no columns" {
            rows_without()
        } else if label == "empty string" {
            rows(true)
                .replace("'203.0.113.' || CAST(i % 40000 AS TEXT)", "''")
                .replace("'203.0.113.' || CAST(i % 400 AS TEXT)", "''")
                .replace(
                    "CASE i % 4 WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36' \
             WHEN 1 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36' \
             WHEN 2 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15' \
             ELSE 'Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0' END",
                    "''",
                )
        } else {
            rows(with_values)
        };
        conn.execute_batch(&format!("INSERT INTO event {sql};"))?;
        conn.execute_batch("CHECKPOINT")?;
        drop(conn);
        duck.push((label, size(&path), path));
    }

    // --- SQLite ---
    let mut lite = Vec::new();
    for (label, with_values) in [
        ("filled", true),
        ("empty string", false),
        ("null", false),
        ("no columns", false),
    ] {
        let path = PathBuf::from(format!(
            "/tmp/nullcost-lite-{}.sqlite",
            label.replace(' ', "-")
        ));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path)?;
        let columns = if label == "no columns" {
            COLUMNS_WITHOUT
        } else {
            COLUMNS
        };
        conn.execute_batch(&format!("CREATE TABLE event ({columns});"))?;
        let sql = if label == "no columns" {
            rows_without()
        } else if label == "empty string" {
            rows(true)
                .replace("'203.0.113.' || CAST(i % 40000 AS TEXT)", "''")
                .replace("'203.0.113.' || CAST(i % 400 AS TEXT)", "''")
                .replace(
                    "CASE i % 4 WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36' \
             WHEN 1 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36' \
             WHEN 2 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15' \
             ELSE 'Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0' END",
                    "''",
                )
        } else {
            rows(with_values)
        };
        // SQLite has no generate_series: a recursive counter stands in.
        let sql = sql.replace(
            &format!("FROM generate_series(1, {ROWS}) t(i)"),
            &format!(
                "FROM (WITH RECURSIVE seq(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM seq WHERE x < {ROWS}) \
                 SELECT x AS i FROM seq) t"
            ),
        );
        conn.execute_batch(&format!("INSERT INTO event {sql};"))?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(conn);
        lite.push((label, size(&path), path));
    }

    let n = ROWS as f64;
    for (name, rows) in [("DuckDB", &duck), ("SQLite", &lite)] {
        println!("{name} — {ROWS} rows");
        let base = rows[0].1 as f64 / n;
        for (label, bytes, _) in rows.iter() {
            let per_row = *bytes as f64 / n;
            println!(
                "  {label:<13} {:>12} bytes  {per_row:>7.1} B/row  ({:+.1} vs filled)",
                bytes,
                per_row - base
            );
        }
        println!();
    }
    Ok(())
}
