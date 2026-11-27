//! **How big is a 7-day shard?** Broken down by site size.
//!
//! The shard length is settled (7 days — a natural week, #163 §4.6.12–.14), so
//! the size of one shard follows from two numbers: how many events the site
//! has in a week, and **how many bytes an event takes**.
//!
//! The byte size of an event depends on the shape of the row — and the shape
//! depends on the privacy configuration:
//!
//! - **thin** — the default configuration (#145): no raw IP, no user agent,
//!   only the dimensions the server fills in;
//! - **full** — everything the server can fill in: geo, UTM, page title, the
//!   web-vitals, a named event with a JSON payload;
//! - **raw** — full, plus the raw IP and the user agent (the setting that is
//!   **off** by default).
//!
//! ```bash
//! cargo run --release --example parquet_shard_size -p sumimi-service
//! ```

use duckdb::Connection;

fn event_ddl(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE website_event ({ddl});",
        ddl = sumimi_service::db::event_columns_ddl(),
    ))?;
    Ok(())
}

/// Fill the table with `rows` rows in the shape of real traffic.
fn fill(conn: &Connection, rows: i64, shape: &str) -> anyhow::Result<()> {
    let uuid = "CAST(substr(md5(i::VARCHAR), 1, 8) || '-' || substr(md5(i::VARCHAR), 9, 4) || '-' || \
                substr(md5(i::VARCHAR), 13, 4) || '-' || substr(md5(i::VARCHAR), 17, 4) || '-' || \
                substr(md5(i::VARCHAR), 21, 12) AS UUID)";
    let mut values: Vec<(&str, String)> = vec![
        // The 128-bit v8 id: the instance half (constant) and the counter.
        (
            "event_id",
            "CAST(from_hex('1234567890abcdef1234' || lpad(to_hex(i), 12, '0')) AS UUID)".to_string(),
        ),
        ("website_id", "'w1'".to_string()),
        ("visit_id", uuid.to_string()),
        ("session_id", uuid.to_string()),
        (
            "created_at",
            "TIMESTAMP '2026-01-01' + INTERVAL (i % 7) DAY + INTERVAL (i % 86400) SECOND".to_string(),
        ),
        ("url_path", "'/product/' || (i % 200)".to_string()),
        ("hostname", "'shop.example.com'".to_string()),
        ("device", "CASE WHEN i % 3 = 0 THEN 'mobile' ELSE 'desktop' END".to_string()),
        ("browser", "CASE WHEN i % 4 = 0 THEN 'safari' ELSE 'chrome' END".to_string()),
        ("os", "CASE WHEN i % 5 = 0 THEN 'ios' ELSE 'macos' END".to_string()),
        ("country", "CASE WHEN i % 7 = 0 THEN 'DE' WHEN i % 11 = 0 THEN 'JP' ELSE 'US' END".to_string()),
        ("language", "'en'".to_string()),
        ("referrer_name", "CASE WHEN i % 6 = 0 THEN 'direct' ELSE 'google' END".to_string()),
        ("event_type", "'pageview'".to_string()),
        // The raw columns are NOT NULL with a default of '' — the default
        // privacy configuration (#145) stores nothing there.
        ("ip", "''".to_string()),
        ("ip_prefix", "''".to_string()),
        ("user_agent", "''".to_string()),
    ];
    if shape != "thin" {
        values.extend(vec![
            ("url_query", "'utm_source=google&utm_medium=cpc&utm_campaign=spring_sale&gclid=Ci0KC'".to_string()),
            ("page_title", "'Buy widget ' || (i % 200) || ' — our best seller'".to_string()),
            ("referrer_domain", "'www.google.com'".to_string()),
            ("referrer_path", "'/search'".to_string()),
            ("referrer_query", "'q=widget+price&hl=en'".to_string()),
            ("browser_version", "'120.0.6099.109'".to_string()),
            ("os_version", "'14.2.1'".to_string()),
            ("screen", "'1920x1080'".to_string()),
            ("region", "'California'".to_string()),
            ("city", "'San Francisco'".to_string()),
            ("latitude", "37.7749".to_string()),
            ("longitude", "-122.4194".to_string()),
            ("utm_source", "'google'".to_string()),
            ("utm_medium", "'cpc'".to_string()),
            ("utm_campaign", "'spring_sale'".to_string()),
            ("utm_content", "'ad-variant-b'".to_string()),
            ("utm_term", "'widget'".to_string()),
            ("gclid", "'Ci0KCx'".to_string()),
            ("channel", "'Organic Search'".to_string()),
            ("event_name", "'add_to_cart'".to_string()),
            ("tag", "'spring-sale'".to_string()),
            ("data", "'{\"product\":\"widget-42\",\"price\":19.99,\"qty\":1}'".to_string()),
            ("session_start", "TIMESTAMP '2026-01-01'".to_string()),
            ("lcp", "1.234".to_string()),
            ("inp", "0.087".to_string()),
            ("cls", "0.004".to_string()),
            ("fcp", "0.912".to_string()),
            ("ttfb", "0.234".to_string()),
            ("user_id", uuid.to_string()),
        ]);
    }
    if shape == "raw" {
        // Real cardinality: many IPs, a handful of user agents — not one value
        // for the whole table (that would compress away).
        values.extend(vec![
            ("ip", "'203.0.113.' || CAST(i % 4096 AS VARCHAR)".to_string()),
            ("ip_prefix", "'203.0.' || CAST(i % 16 AS VARCHAR) || '.0'".to_string()),
            (
                "user_agent",
                "CASE i % 5 WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36' \
                 WHEN 1 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1' \
                 WHEN 2 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36' \
                 WHEN 3 THEN 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/119.0.0.0 Safari/537.36' \
                 ELSE 'Mozilla/5.0 (iPad; CPU OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1' END".to_string(),
            ),
        ]);
    }
    let columns = sumimi_service::db::event_columns();
    let mut exprs = Vec::with_capacity(columns.len());
    for column in &columns {
        // The last entry wins — the raw shape overrides the default ''.
        let expression = values
            .iter()
            .rev()
            .find(|(name, _)| name == column)
            .map(|(_, expression)| expression.as_str())
            .unwrap_or("NULL");
        exprs.push(expression);
    }
    conn.execute_batch(&format!(
        "INSERT INTO website_event ({}) SELECT {} FROM range(0, {rows}) t(i);",
        columns.join(", "),
        exprs.join(", "),
    ))?;
    Ok(())
}

/// A size a human can read.
fn human(bytes: f64) -> String {
    if bytes < 1024.0 * 1024.0 {
        format!("{:.0} KB", bytes / 1024.0)
    } else if bytes < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", bytes / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes / (1024.0 * 1024.0 * 1024.0))
    }
}

/// **What does the event id cost in storage?**
///
/// Ours is the **128-bit v8 id** (the instance half and a counter); the
/// alternatives are what the other shapes would cost on the same rows.
///
/// The UUID question is really two questions:
/// - **UUIDv4 (random)** — 16 bytes that do not compress;
/// - **UUIDv7 / v8 (time-ordered)** — 16 bytes whose first 48 bits are a
///   timestamp, so the values increase with time and delta-encode like a
///   sequence does.
///
/// The measurement writes the same rows with each id and compares the file.
fn event_id_cost(dir: &std::path::Path) -> anyhow::Result<()> {
    println!();
    println!("=== the event id in storage ===");

    // The same schema, but with `event_id` as a UUID — the shape Umami uses.
    let uuid_ddl = sumimi_service::db::event_columns_ddl();

    let db = dir.join("idcost.duckdb");
    let _ = std::fs::remove_file(&db);
    let conn = Connection::open(&db)?;
    conn.execute_batch(&format!("CREATE TABLE website_event ({uuid_ddl});"))?;

    let rows: i64 = 1_000_000;
    // A time-ordered UUID: the first 48 bits are a millisecond timestamp, the
    // rest is random. v7 and v8 differ only in the version nibble.
    let time_uuid = |version: &str| {
        format!(
            "CAST(substr(lpad(to_hex(1750000000000 + i), 12, '0'), 1, 8) || '-' || \
                    substr(lpad(to_hex(1750000000000 + i), 12, '0'), 9, 4) || '-{version}000-8000-' || \
                    substr(md5(i::VARCHAR), 1, 12) AS UUID)"
        )
    };

    // The id of every shape, and the type it needs.
    for (label, id_expr, kind) in [
        // **Ours**: the instance half (constant) and the counter (dense),
        // sixteen bytes.
        (
            "the 128-bit v8 id (today)",
            "CAST(from_hex('1234567890abcdef1234' || lpad(to_hex(i), 12, '0')) AS UUID)",
            "uuid",
        ),
        ("UUID v4 (random)", "uuid()", "uuid"),
        ("UUID v7 (time-ordered)", time_uuid("7").as_str(), "uuid"),
        ("UUID v8 (time-ordered)", time_uuid("8").as_str(), "uuid"),
        // The snowflake of D-059: 41 bits of milliseconds, 10 bits of
        // instance, 12 bits of counter. 1 000 events per millisecond.
        (
            "the packed snowflake (D-059)",
            "CAST(((1766000000000 + (i // 1000)) * 4194304 + (7 * 4096) + (i % 1000)) AS BIGINT)",
            "bigint",
        ),
        // A random 64-bit id: 8 bytes, no coordination, no state — but it does
        // not compress (nothing to delta against).
        (
            "a random 64-bit id",
            "CAST((i * 2654435761 + 88172645463325252) % 18446744073709551616 AS BIGINT)",
            "bigint",
        ),
    ] {
        // The last variant replaces the two UUID columns with short strings,
        // to see what the row's UUIDs cost in total.
        let short_ids = false;
        // The BIGINT variants need the id column to be a BIGINT; the UUID
        // ones a UUID.
        let is_bigint = kind == "bigint";
        let ddl = if short_ids {
            sumimi_service::db::event_columns_ddl()
                .replace("visit_id UUID", "visit_id VARCHAR")
                .replace("session_id UUID", "session_id VARCHAR")
        } else if is_bigint {
            // The id column has to be able to hold a BIGINT.
            uuid_ddl.replace("event_id UUID NOT NULL", "event_id BIGINT NOT NULL")
        } else {
            uuid_ddl.clone()
        };
        conn.execute_batch("DROP TABLE IF EXISTS website_event;")?;
        conn.execute_batch(&format!("CREATE TABLE website_event ({ddl});"))?;
        let columns = sumimi_service::db::event_columns();
        let exprs: Vec<String> = columns
            .iter()
            .map(|c| {
                if c == "event_id" {
                    id_expr.to_string()
                } else if c == "website_id" {
                    "'11111111-1111-1111-1111-111111111111'".to_string()
                } else if c == "visit_id" || c == "session_id" {
                    if short_ids {
                        "substr(md5(i::VARCHAR), 1, 8)".to_string()
                    } else {
                        "CAST(substr(md5(i::VARCHAR), 1, 8) || '-' || substr(md5(i::VARCHAR), 9, 4) || '-' || \
                     substr(md5(i::VARCHAR), 13, 4) || '-' || substr(md5(i::VARCHAR), 17, 4) || '-' || \
                     substr(md5(i::VARCHAR), 21, 12) AS UUID)".to_string()
                    }
                } else if c == "created_at" {
                    "TIMESTAMP '2026-01-01' + INTERVAL (i // 1000) SECOND".to_string()
                } else if c == "url_path" {
                    "'/product/' || (i % 200)".to_string()
                } else if c == "url_query" {
                    "'utm_source=google&utm_medium=cpc&utm_campaign=spring'".to_string()
                } else if c == "hostname" {
                    "'shop.example.com'".to_string()
                } else if c == "page_title" {
                    "'Buy widget ' || (i % 200) || ' — our best seller'".to_string()
                } else if c == "referrer_name" || c == "referrer_domain" {
                    "'google'".to_string()
                } else if c == "device" {
                    "'desktop'".to_string()
                } else if c == "browser" {
                    "'chrome'".to_string()
                } else if c == "os" {
                    "'macos'".to_string()
                } else if c == "country" {
                    "'US'".to_string()
                } else if c == "language" {
                    "'en'".to_string()
                } else if c == "event_type" {
                    "1".to_string()
                } else if c == "ip" || c == "ip_prefix" || c == "user_agent" {
                    "''".to_string()
                } else {
                    "NULL".to_string()
                }
            })
            .collect();
        conn.execute_batch(&format!(
            "INSERT INTO website_event ({}) SELECT {} FROM range(0, {rows}) t(i);",
            columns.join(", "),
            exprs.join(", ")
        ))?;

        let parquet = dir.join(format!("id-{}.parquet", label.replace([' ', '(', ')'], "_")));
        let _ = std::fs::remove_file(&parquet);
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM website_event ORDER BY created_at) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            parquet.display()
        ))?;
        let parquet_size = std::fs::metadata(&parquet).map(|m| m.len()).unwrap_or(0);

        // The hot tier: the site's own DuckDB file.
        let duck = dir.join(format!("id-{}.duckdb", label.replace([' ', '(', ')'], "_")));
        let _ = std::fs::remove_file(&duck);
        let other = Connection::open(&duck)?;
        other.execute_batch(&format!("CREATE TABLE website_event ({ddl});"))?;
        other.execute_batch(&format!(
            "INSERT INTO website_event ({}) SELECT {} FROM range(0, {rows}) t(i);",
            columns.join(", "),
            exprs.join(", ")
        ))?;
        other.execute_batch("CHECKPOINT;")?;
        drop(other);
        let duck_size = std::fs::metadata(&duck).map(|m| m.len()).unwrap_or(0);

        println!(
            "  {:<24} parquet {:>7.1} B/row   duckdb {:>7.1} B/row",
            label,
            parquet_size as f64 / rows as f64,
            duck_size as f64 / rows as f64
        );
    }
    Ok(())
}

/// **What does a reference to an event cost in the EAV table?**
///
/// The EAV table has one row per (event, dimension) — so the same event id
/// repeats across that event's dimension rows. Parquet dictionary-encodes it
/// ⇒ ⇒ the reference does **not** cost its full width.
fn eav_reference_cost(dir: &std::path::Path) -> anyhow::Result<()> {
    println!();
    println!("=== what a reference to an event costs in the EAV table ===");

    let db = dir.join("eav.duckdb");
    let _ = std::fs::remove_file(&db);
    let conn = Connection::open(&db)?;

    let events: i64 = 1_000_000;
    let dims: i64 = 10;
    for (label, id_expr) in [
        ("BIGINT (today)", "CAST(e AS BIGINT)"),
        ("Snowflake 64", "CAST(((1766000000000 + (e // 1000)) * 4194304 + (7 * 4096) + (e % 1000)) AS BIGINT)"),
        ("UUID v7", "CAST(substr(lpad(to_hex(1766000000000 + (e // 1000)), 12, '0'), 1, 8) || '-' || \
                    substr(lpad(to_hex(1766000000000 + (e // 1000)), 12, '0'), 9, 4) || '-7000-8000-' || \
                    substr(md5(e::VARCHAR), 1, 12) AS UUID)"),
    ] {
        conn.execute_batch("DROP TABLE IF EXISTS event_data;")?;
        conn.execute_batch(&format!(
            "CREATE TABLE event_data (website_event_id {}, event_key VARCHAR, event_value VARCHAR);",
            if label.starts_with("UUID") { "UUID" } else { "BIGINT" }
        ))?;
        conn.execute_batch(&format!(
            "INSERT INTO event_data
             SELECT {id_expr}, 'product_' || CAST(d % 4 AS VARCHAR), CAST((e * 7 + d) % 1000 AS VARCHAR)
             FROM range(0, {events}) t(e), range(0, {dims}) u(d);",
        ))?;
        let out = dir.join(format!("eav-{}.parquet", label.replace([' ', '(', ')'], "_")));
        let _ = std::fs::remove_file(&out);
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM event_data) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            out.display()
        ))?;
        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        let rows = events * dims;
        println!(
            "  {:<16} {:>8} B/row   ({:.1} B per reference, {} rows)",
            label,
            size as f64 / rows as f64,
            size as f64 / events as f64,
            rows
        );
    }
    Ok(())
}

/// **What does the id cost, and what would two columns cost?**
///
/// The id can be one packed column, or two: a **run id** (the same value for
/// every row a run wrote — so it dictionary-encodes away) and an **event
/// sequence** (a plain counter inside the run).
///
/// The question this answers: does splitting the id buy anything on disk, and
/// what does it cost at the places that *reference* an event?
fn split_id_cost(dir: &std::path::Path) -> anyhow::Result<()> {
    println!();
    println!("=== one packed column, or two? ===");

    let rows: i64 = 1_000_000;
    let db = dir.join("split.duckdb");
    let _ = std::fs::remove_file(&db);
    let conn = Connection::open(&db)?;

    // The same rows, written three ways: no id at all, one packed id, and the
    // id split into a run id and a sequence.
    for (label, ddl, select) in [
        (
            "no event id at all",
            "CREATE TABLE t (website_id VARCHAR, visit_id UUID, session_id UUID, created_at TIMESTAMP, url_path VARCHAR);",
            "SELECT 'w1', visit_id, session_id, created_at, url_path FROM base",
        ),
        (
            "one packed id",
            "CREATE TABLE t (event_id BIGINT, website_id VARCHAR, visit_id UUID, session_id UUID, created_at TIMESTAMP, url_path VARCHAR);",
            "SELECT event_id, 'w1', visit_id, session_id, created_at, url_path FROM base",
        ),
        (
            "two columns (run id + seq)",
            "CREATE TABLE t (run_id UUID, event_seq BIGINT, website_id VARCHAR, visit_id UUID, session_id UUID, created_at TIMESTAMP, url_path VARCHAR);",
            "SELECT run_id, event_seq, 'w1', visit_id, session_id, created_at, url_path FROM base",
        ),
    ] {
        conn.execute_batch("DROP TABLE IF EXISTS base;")?;
        conn.execute_batch("DROP TABLE IF EXISTS t;")?;
        let base = "CREATE TABLE base AS SELECT i AS event_id,
                    CAST('11111111-1111-1111-1111-111111111111' AS UUID) AS run_id,
                    i AS event_seq,
                    CAST(substr(md5(i::VARCHAR), 1, 8) || '-' || substr(md5(i::VARCHAR), 9, 4) || '-' ||
                         substr(md5(i::VARCHAR), 13, 4) || '-' || substr(md5(i::VARCHAR), 17, 4) || '-' ||
                         substr(md5(i::VARCHAR), 21, 12) AS UUID) AS visit_id,
                    CAST(substr(md5(i::VARCHAR || 's'), 1, 8) || '-' || substr(md5(i::VARCHAR || 's'), 9, 4) || '-' ||
                         substr(md5(i::VARCHAR || 's'), 13, 4) || '-' || substr(md5(i::VARCHAR || 's'), 17, 4) || '-' ||
                         substr(md5(i::VARCHAR || 's'), 21, 12) AS UUID) AS session_id,
                    TIMESTAMP '2026-01-01' + INTERVAL (i // 1000) SECOND AS created_at,
                    '/product/' || (i % 200) AS url_path
             FROM range(0, 1000000) s(i);";
        conn.execute_batch(base)?;
        conn.execute_batch(ddl)?;
        conn.execute_batch(&format!("INSERT INTO t {select};"))?;
        let out = dir.join(format!("split-{}.parquet", label.replace(' ', "-")));
        let _ = std::fs::remove_file(&out);
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM t ORDER BY created_at) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            out.display()
        ))?;
        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);

        // The hot tier: the site's own DuckDB file.
        let duck = dir.join(format!("split-{}.duckdb", label.replace(' ', "-")));
        let _ = std::fs::remove_file(&duck);
        let other = Connection::open(&duck)?;
        other.execute_batch(base)?;
        other.execute_batch(ddl)?;
        other.execute_batch(&format!("INSERT INTO t {select};"))?;
        other.execute_batch("CHECKPOINT;")?;
        drop(other);
        let duck_size = std::fs::metadata(&duck).map(|m| m.len()).unwrap_or(0);

        println!(
            "  {:<24} parquet {:>6.1} B/row   duckdb {:>6.1} B/row",
            label,
            size as f64 / rows as f64,
            duck_size as f64 / rows as f64
        );
    }

    // The references: the EAV rows point at an event.
    println!("  the references (the EAV rows point at an event):");
    for (label, ddl, select) in [
        (
            "one packed id",
            "CREATE TABLE e (website_event_id BIGINT, event_key VARCHAR, event_value VARCHAR);",
            "SELECT ((i // 1000) * 4194304 + (7 * 4096) + (i % 1000)), 'product_' || CAST(d % 4 AS VARCHAR), CAST((i * 7 + d) % 1000 AS VARCHAR) FROM range(0, 1000000) t(i), range(0, 10) u(d)",
        ),
        (
            "two columns",
            "CREATE TABLE e (run_id UUID, website_event_seq BIGINT, event_key VARCHAR, event_value VARCHAR);",
            "SELECT CAST('11111111-1111-1111-1111-111111111111' AS UUID), i, 'product_' || CAST(d % 4 AS VARCHAR), CAST((i * 7 + d) % 1000 AS VARCHAR) FROM range(0, 1000000) t(i), range(0, 10) u(d)",
        ),
    ] {
        conn.execute_batch("DROP TABLE IF EXISTS e;")?;
        conn.execute_batch(ddl)?;
        conn.execute_batch(&format!("INSERT INTO e {select};"))?;
        let out = dir.join(format!("split-eav-{}.parquet", label.replace(' ', "-")));
        let _ = std::fs::remove_file(&out);
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM e) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            out.display()
        ))?;
        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        println!(
            "    {:<20} {:>6.3} B/row   ({:.1} B per event)",
            label,
            size as f64 / 10_000_000.0,
            size as f64 / 1_000_000.0
        );
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);

    println!("(measuring with {rows} rows per shape)");

    let dir = std::env::temp_dir().join("sumimi-parquet-shard-size");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let db = dir.join("size.duckdb");
    let conn = Connection::open(&db)?;
    event_ddl(&conn)?;

    // The bytes per row of each shape.
    let mut per_row = Vec::new();
    for shape in ["thin", "full", "raw"] {
        conn.execute_batch("DELETE FROM website_event;")?;
        fill(&conn, rows, shape)?;
        let out = dir.join(format!("{shape}.parquet"));
        let _ = std::fs::remove_file(&out);
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM website_event ORDER BY created_at)
             TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            out.display()
        ))?;
        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        let bytes_per_row = size as f64 / rows as f64;
        per_row.push((shape, bytes_per_row));
        println!(
            "  {:<5} shape: {:>9} B/row   ({:.1} B/event)",
            shape,
            size,
            bytes_per_row
        );
    }

    event_id_cost(&dir)?;
    split_id_cost(&dir)?;
    eav_reference_cost(&dir)?;

    // The size of one 7-day shard, by site size.
    println!();
    println!("=== one 7-day shard ===");
    println!(
        "  {:>12}  {:>14}  {:>14}  {:>14}  {:>16}",
        "events/day", "thin", "full", "raw", "full / year"
    );
    for per_day in [10i64, 100, 1_000, 10_000, 50_000, 100_000, 500_000, 1_000_000] {
        let week = per_day * 7;
        let cells: Vec<String> = per_row
            .iter()
            .map(|(_, b)| human(week as f64 * b))
            .collect();
        let year = 52.0 * week as f64 * per_row[1].1;
        println!(
            "  {:>12}  {:>10}  {:>10}  {:>10}  {:>10}",
            format!("{per_day}"),
            cells[0],
            cells[1],
            cells[2],
            human(year)
        );
    }

    println!();
    println!("（对照：一个空片 1.1 KB · DuckDB 站文件的空底 268 KB）");
    Ok(())
}
