//! **How cheap can an identity id be?**
//!
//! The identity columns (`session_id` · `visitor_id`) are the two most
//! expensive columns in the row: sixteen random bytes each ⇒ **33.8 B/row**
//! of the ~49 B row (measured: `column_cost`).
//!
//! **The idea** (see the mneme issue #165): the random part does not
//! compress — but it can sit in the **low bytes**, with a **constant marker**
//! in the high ones. The marker costs nothing (a dictionary eats it), the
//! entropy's width is readable from the marker, and the column's type never
//! changes ⇒ the "widen the column on upgrade" trap (a 47% slower distinct
//! count, #108 §4.4) never happens.
//!
//! **This measures the room**: every shape, on both tiers, with the disk, the
//! query time, and how the engine stores it.
//!
//! ```bash
//! cargo run --release --example identity_layout -p sumimi-service -- 2000000
//! ```

use std::time::Instant;

use duckdb::Connection;

/// Sixteen random bytes, as hex — the source of every "random" byte here.
fn rnd() -> String {
    "md5(i::VARCHAR)".to_string()
}

/// The self-describing marker: ones down to a single zero, whose position
/// says how wide the entropy is.
fn marker(bytes: usize) -> String {
    let mut out = "f".repeat(bytes * 2 - 2);
    out.push_str("fe");
    out
}

/// The shapes: the label, the DuckDB type, and the expression.
fn shapes() -> Vec<(&'static str, &'static str, String)> {
    let r = rnd();
    vec![
        // Today's shape: sixteen random bytes.
        (
            "16 random bytes (today's UUID)",
            "UUID",
            format!("CAST(from_hex({r}) AS UUID)"),
        ),
        // A constant marker of four bytes, then the entropy.
        (
            "marker 4B + 12 entropy",
            "UUID",
            format!("CAST(from_hex('ffffffff' || substr({r}, 1, 24)) AS UUID)"),
        ),
        (
            "marker 6B + 10 entropy",
            "UUID",
            format!("CAST(from_hex('ffffffffffff' || substr({r}, 1, 20)) AS UUID)"),
        ),
        (
            "marker 8B + 8 entropy",
            "UUID",
            format!("CAST(from_hex('ffffffffffffffff' || substr({r}, 1, 16)) AS UUID)"),
        ),
        // The self-describing marker: ones down to a single zero, whose
        // position says how wide the entropy is.
        (
            "self-describing + 8 BITS of entropy",
            "UUID",
            format!(
                "CAST(from_hex('{}' || substr({r}, 1, 2)) AS UUID)",
                marker(15)
            ),
        ),
        (
            "self-describing + 16 BITS of entropy",
            "UUID",
            format!(
                "CAST(from_hex('{}' || substr({r}, 1, 4)) AS UUID)",
                marker(14)
            ),
        ),
        (
            "self-describing + 32 BITS of entropy",
            "UUID",
            format!(
                "CAST(from_hex('{}' || substr({r}, 1, 8)) AS UUID)",
                marker(12)
            ),
        ),
        // **The self-describing marker with a real id's worth of entropy**:
        // the marker is as long as the entropy is short, and the zero that
        // ends the marker says where the entropy starts.
        (
            "self-describing + 8 BYTES of entropy",
            "UUID",
            format!(
                "CAST(from_hex('{}' || substr({r}, 1, 16)) AS UUID)",
                marker(8)
            ),
        ),
        (
            "self-describing + 10 BYTES of entropy",
            "UUID",
            format!(
                "CAST(from_hex('{}' || substr({r}, 1, 20)) AS UUID)",
                marker(6)
            ),
        ),
        (
            "self-describing + 12 BYTES of entropy",
            "UUID",
            format!(
                "CAST(from_hex('{}' || substr({r}, 1, 24)) AS UUID)",
                marker(4)
            ),
        ),
        // The site's own constant in the high half (the other candidate for
        // the space): the site is one value for the whole file.
        (
            "site 8B + 8 entropy",
            "UUID",
            format!(
                "CAST(from_hex('a1b2c3d4e5f60718' || lpad(to_hex({r}), 16, '0')) AS UUID)"
            ),
        ),
        // The references from #108: eight bytes, two ways.
        ("BIGINT (8 bytes)", "BIGINT", format!("CAST(('0x' || substr({r}, 1, 15))::BIGINT AS BIGINT)")),
        (
            "BLOB (8 bytes)",
            "BLOB",
            format!("CAST(from_hex(substr({r}, 1, 16)) AS BLOB)"),
        ),
    ]
}

/// The file holds the column and nothing else ⇒ what the column costs.
fn duckdb_bytes(dir: &std::path::Path, name: &str, kind: &str, expression: &str, rows: i64) -> anyhow::Result<(f64, String)> {
    let path = dir.join(format!("{name}.duckdb"));
    let _ = std::fs::remove_file(&path);
    let mut how = String::new();
    let bytes = {
        let conn = Connection::open(&path)?;
        conn.execute_batch(&format!("CREATE TABLE t (id {kind});"))?;
        conn.execute_batch(&format!(
            "INSERT INTO t SELECT {expression} FROM range(0, {rows}) t(i);"
        ))?;
        conn.execute_batch("CHECKPOINT;")?;
        let mut stmt =
            conn.prepare("SELECT column_name, compression FROM pragma_storage_info('t');")?;
        let mut out = stmt.query([])?;
        let mut seen: Vec<String> = Vec::new();
        while let Some(row) = out.next()? {
            let column: String = row.get(0)?;
            let compression: String = row.get(1)?;
            let tag = format!("{column}={compression}");
            if !seen.contains(&tag) {
                seen.push(tag);
            }
        }
        how = seen.join(", ");
        drop(conn);
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    };
    Ok((bytes as f64 / rows as f64, how))
}

/// How long does the visitor count take — the query that matters (#108).
fn distinct_ms(dir: &std::path::Path, name: &str, kind: &str, expression: &str, rows: i64) -> anyhow::Result<f64> {
    let path = dir.join(format!("{name}-q.duckdb"));
    let _ = std::fs::remove_file(&path);
    let conn = Connection::open(&path)?;
    conn.execute_batch(&format!("CREATE TABLE t (id {kind});"))?;
    conn.execute_batch(&format!(
        "INSERT INTO t SELECT {expression} FROM range(0, {rows}) t(i);"
    ))?;
    let started = Instant::now();
    let distinct: i64 = conn.query_row("SELECT count(DISTINCT id) FROM t", [], |row| row.get(0))?;
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    drop(conn);
    let _ = std::fs::remove_file(&path);
    let _ = distinct;
    Ok(elapsed)
}

/// The same shapes in Parquet — the cold tier.
fn parquet_bytes(dir: &std::path::Path, name: &str, expression: &str, rows: i64) -> anyhow::Result<f64> {
    let path = dir.join(format!("{name}.parquet"));
    let _ = std::fs::remove_file(&path);
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(&format!(
        "COPY (SELECT {expression} AS id FROM range(0, {rows}) t(i)) TO '{}' \
         (FORMAT PARQUET, COMPRESSION ZSTD);",
        path.display()
    ))?;
    Ok(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / rows as f64)
}

/// The dashboard's three numbers (the same query #108 measured).
const SUMMARY: &str = "SELECT COUNT(*) FILTER (WHERE event_type NOT IN ('event', 'identify')),
                              COUNT(DISTINCT session_id),
                              COUNT(DISTINCT visitor_id)
                       FROM probe WHERE website_id = 'w1'";

/// The audience breakdown — the visitors per browser.
const BREAKDOWN: &str = "SELECT COALESCE(browser, 'unknown') AS name, COUNT(DISTINCT session_id) AS value
                          FROM probe WHERE website_id = 'w1' AND event_type NOT IN ('event', 'identify')
                          GROUP BY name ORDER BY value DESC LIMIT 50";

/// The whole event row, with the identity columns in one shape.
///
/// The row is the real one (the schema's 51 columns, the full shape) — the
/// only thing that changes between the shapes is the identity.
fn probe_row(session_id: &str, visitor_id: &str) -> String {
    format!(
        "SELECT 'w1' AS website_id,
                {session_id} AS session_id,
                {visitor_id} AS visitor_id,
                TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND AS created_at,
                '/blog/post-' || (i % 5000) AS url_path,
                CASE WHEN i % 7 = 0 THEN 'utm_source=twitter&utm_campaign=spring' ELSE '' END AS url_query,
                'A reasonably long article title number ' || (i % 1000) AS page_title,
                CASE WHEN i % 3 = 0 THEN 'https://www.google.com/search?q=some+term' ELSE '' END AS referrer_domain,
                '' AS referrer_path, 'example.com' AS hostname, 'Search' AS channel,
                CASE (i % 120000) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                     WHEN 2 THEN 'Firefox' ELSE 'Edge' END AS browser,
                '120' AS browser_version, 'macOS' AS os, '10' AS os_version, 'desktop' AS device,
                '1920x1080' AS screen, 'en-US' AS language, 'DE' AS country, 'Berlin' AS region,
                'Berlin' AS city, 52.52 AS latitude, 13.405 AS longitude, 'pageview' AS event_type,
                '' AS ip, '' AS ip_prefix, '' AS user_agent, NULL AS user_id, NULL AS session_start,
                NULL AS visit_id, NULL AS event_name, NULL AS tag, NULL AS data,
                NULL AS fbclid, NULL AS msclkid, NULL AS ttclid, NULL AS li_fat_id, NULL AS twclid,
                NULL AS gclid, NULL AS utm_medium, NULL AS utm_campaign, NULL AS utm_content,
                NULL AS utm_term, NULL AS lcp, NULL AS inp, NULL AS cls, NULL AS fcp, NULL AS ttfb
         FROM range(0, {{ROWS}}) t(i)",
        session_id = session_id,
        visitor_id = visitor_id
    )
}

/// The whole row in a DuckDB file: how big, and how long the dashboard's
/// queries take.
fn probe(dir: &std::path::Path, name: &str, session_id: &str, visitor_id: &str, rows: i64) -> anyhow::Result<(f64, f64, f64)> {
    let path = dir.join(format!("probe-{name}.duckdb"));
    let _ = std::fs::remove_file(&path);
    let conn = Connection::open(&path)?;
    conn.execute_batch(&format!(
        "CREATE TABLE probe AS {}",
        probe_row(session_id, visitor_id).replace("{ROWS}", &rows.to_string())
    ))?;
    conn.execute_batch("CHECKPOINT;")?;
    drop(conn);
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / rows as f64;

    let conn = Connection::open(&path)?;
    let mut summary = f64::MAX;
    let mut breakdown = f64::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        let _: (i64, i64, i64) = conn.query_row(SUMMARY, [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        summary = summary.min(started.elapsed().as_secs_f64() * 1000.0);
        let started = Instant::now();
        let mut stmt = conn.prepare(BREAKDOWN)?;
        let mut out = stmt.query([])?;
        while out.next()?.is_some() {}
        breakdown = breakdown.min(started.elapsed().as_secs_f64() * 1000.0);
    }
    drop(conn);
    let _ = std::fs::remove_file(&path);
    Ok((bytes, summary, breakdown))
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2_000_000);

    println!("(measuring with {rows} rows)");
    let dir = std::env::temp_dir().join("sumimi-identity-layout");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    println!();
    println!("=== one identity column ===");
    println!(
        "  {:<32} {:>10} {:>12} {:>12}  {}",
        "the shape", "duckdb B/row", "parquet B/row", "distinct ms", "how DuckDB stores it"
    );
    for (label, kind, expression) in shapes() {
        let name = label.replace([' ', '(', ')', '+', '\''], "_");
        let (duckdb, how) = duckdb_bytes(&dir, &name, kind, &expression, rows)?;
        let parquet = parquet_bytes(&dir, &name, &expression, rows)?;
        let ms = distinct_ms(&dir, &name, kind, &expression, rows)?;
        println!(
            "  {:<32} {:>10.2} {:>12.2} {:>12.1}  {}",
            label, duckdb, parquet, ms, how
        );
    }

    // ---- the mix: two widths in one column ----
    println!();
    println!("=== the mix: 90% one width, 10% another (the new trap) ===");
    let r = rnd();
    let wide = format!("CAST(from_hex({r}) AS UUID)");
    let narrow = format!("CAST(from_hex('ffffffff' || substr({r}, 1, 24)) AS UUID)");
    for (label, expression) in [
        ("all one width", wide.clone()),
        (
            "90% narrow + 10% wide",
            format!("CASE WHEN i % 10 = 0 THEN {wide} ELSE {narrow} END"),
        ),
    ] {
        let name = label.replace([' ', '(', ')', '%', '+', '\''], "_");
        let (duckdb, how) = duckdb_bytes(&dir, &name, "UUID", &expression, rows)?;
        let parquet = parquet_bytes(&dir, &name, &expression, rows)?;
        println!(
            "  {:<32} {:>10.2} {:>12.2}  {}",
            label, duckdb, parquet, how
        );
    }

    // ---- the two columns together, in a row ----
    println!();
    println!("=== the two identity columns in one row (the whole point) ===");
    let conn = Connection::open_in_memory()?;
    let ddl = "CREATE TABLE t (session_id UUID, visitor_id UUID, url_path VARCHAR);";
    let variants: Vec<(&str, String)> = vec![
        (
            "today: 16 random bytes each",
            rnd(),
        ),
        (
            "marker 4B + 12 entropy each",
            format!("CAST(from_hex('ffffffff' || substr({r}, 1, 24)) AS UUID)"),
        ),
        (
            "marker 8B + 8 entropy each",
            format!("CAST(from_hex('ffffffffffffffff' || substr({r}, 1, 16)) AS UUID)"),
        ),
    ];
    for (label, expression) in &variants {
        let out = dir.join(format!("both-{}.parquet", label.replace([' ', '(', ')', '+'], "_")));
        let _ = std::fs::remove_file(&out);
        conn.execute_batch(&format!(
            "COPY (SELECT {expression} AS session_id, {expression} AS visitor_id, '/p/' || (i % 200) AS url_path \
             FROM range(0, {rows}) t(i)) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            out.display()
        ))?;
        let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) as f64 / rows as f64;
        println!("  {:<32} {:>8.2} B/row", label, bytes);
    }

    // ---- the whole row, and the dashboard's queries ----
    println!();
    println!("=== the whole event row, and the dashboard's queries ===");
    println!(
        "  {:<32} {:>10} {:>12} {:>12}",
        "the shape", "B/row", "summary ms", "browsers ms"
    );
    let r = rnd();
    let whole: Vec<(&str, String, String)> = vec![
        (
            "today: 16 random bytes each",
            "CAST(from_hex(md5(i::VARCHAR)) AS UUID)".to_string(),
            "CAST(from_hex(md5((i + 1)::VARCHAR)) AS UUID)".to_string(),
        ),
        (
            "marker 4B + 12 entropy each",
            "CAST(from_hex('ffffffff' || substr(md5(i::VARCHAR), 1, 24)) AS UUID)".to_string(),
            "CAST(from_hex('ffffffff' || substr(md5((i + 1)::VARCHAR), 1, 24)) AS UUID)".to_string(),
        ),
        (
            "marker 8B + 8 entropy each",
            "CAST(from_hex('ffffffffffffffff' || substr(md5(i::VARCHAR), 1, 16)) AS UUID)".to_string(),
            "CAST(from_hex('ffffffffffffffff' || substr(md5((i + 1)::VARCHAR), 1, 16)) AS UUID)".to_string(),
        ),
        (
            "self-describing + 8 BYTES of entropy each",
            format!("CAST(from_hex('{}' || substr(md5(i::VARCHAR), 1, 16)) AS UUID)", marker(8)),
            format!(
                "CAST(from_hex('{}' || substr(md5((i + 1)::VARCHAR), 1, 16)) AS UUID)",
                marker(8)
            ),
        ),
        (
            "self-describing + 12 BYTES of entropy each",
            format!("CAST(from_hex('{}' || substr(md5(i::VARCHAR), 1, 24)) AS UUID)", marker(4)),
            format!(
                "CAST(from_hex('{}' || substr(md5((i + 1)::VARCHAR), 1, 24)) AS UUID)",
                marker(4)
            ),
        ),
        (
            "self-describing + 8 entropy each",
            format!("CAST(from_hex('{}' || substr(md5(i::VARCHAR), 1, 2)) AS UUID)", marker(15)),
            format!(
                "CAST(from_hex('{}' || substr(md5((i + 1)::VARCHAR), 1, 2)) AS UUID)",
                marker(15)
            ),
        ),
        (
            "self-describing + 32 entropy each",
            format!("CAST(from_hex('{}' || substr(md5(i::VARCHAR), 1, 8)) AS UUID)", marker(12)),
            format!(
                "CAST(from_hex('{}' || substr(md5((i + 1)::VARCHAR), 1, 8)) AS UUID)",
                marker(12)
            ),
        ),
    ];
    for (label, session_id, visitor_id) in &whole {
        let name = label.replace([' ', '(', ')', '+', '\''], "_");
        let (bytes, summary, breakdown) = probe(&dir, &name, session_id, visitor_id, rows)?;
        println!(
            "  {:<32} {:>10.2} {:>12.1} {:>12.1}",
            label, bytes, summary, breakdown
        );
    }

    Ok(())
}
