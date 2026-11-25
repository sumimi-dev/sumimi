//! **Which id shape? A careful comparison.**
//!
//! Four candidates, measured on the same rows:
//!
//! 1. **the packed snowflake** — 63 bits in a `BIGINT` (what we do today);
//! 2. **128 bits as a number** — the instance id and the sequence, packed
//!    into one value (`HUGEINT` in DuckDB);
//! 3. **128 bits as a UUID** — the same two parts, in the UUID layout
//!    (**all 128 bits kept — no version nibble forced**);
//! 4. **a plain UUIDv7** — time-ordered, random tail (the "no instance
//!    part at all" option).
//!
//! Plus the two questions that decide it:
//!
//! - **can the UUID type keep all 128 bits?** (the version nibble and the
//!   variant bits are a convention — does the storage keep them?)
//! - **what does the whole event row cost?** (the id column alone is not
//!   the number that matters)
//!
//! ```bash
//! cargo run --release --example id_128 -p sumimi-service
//! ```

use duckdb::Connection;

/// The instance part — one value for the whole instance (16 hex characters).
const INSTANCE_HEX: &str = "1234567890ABCDEF";
/// The same instance part as a number (the high 64 bits).
const INSTANCE_BITS: i64 = 1_311_768_467_463_790_320;

/// The 32 hex characters of the 128-bit id: the instance, then the sequence.
fn hex_of() -> String {
    format!("lpad('{INSTANCE_HEX}', 16, '0') || lpad(to_hex(i), 16, '0')")
}

/// The same 128 bits in the UUID layout — **every bit kept**: the text is
/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, and nothing is forced into the
/// version nibble or the variant bits.
fn uuid_of(hex: &str) -> String {
    format!(
        "CAST(substr({hex}, 1, 8) || '-' || substr({hex}, 9, 4) || '-' || substr({hex}, 13, 4) || '-' || \
         substr({hex}, 17, 4) || '-' || substr({hex}, 21, 12) AS UUID)",
        hex = hex
    )
}

/// **Can the UUID type keep all 128 bits?**
fn bits() -> anyhow::Result<()> {
    println!();
    println!("=== can the UUID type keep all 128 bits? ===");
    let conn = Connection::open_in_memory()?;

    // 1. The text form: does the parser keep the bits it is given?
    for text in [
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
        "0102030405060708090a0b0c0d0e0f10",
        "1234567890abcdef1234567890abcdef",
    ] {
        let kept: String = conn.query_row(
            &format!("SELECT replace(CAST('{text}' AS UUID)::VARCHAR, '-', '')"),
            [],
            |r| r.get(0),
        )?;
        println!(
            "  {text}  ⇒  {kept}   {}",
            if kept == text.replace('-', "") {
                "all 128 bits kept"
            } else {
                "CHANGED"
            }
        );
    }

    // 2. From raw bytes: is anything forced?
    let kept: String = conn.query_row(
        "SELECT replace(CAST(from_hex('0102030405060708090a0b0c0d0e0f10') AS UUID)::VARCHAR, '-', '')",
        [],
        |r| r.get(0),
    )?;
    println!(
        "  from 16 raw bytes  ⇒  {kept}   {}",
        if kept == "0102030405060708090a0b0c0d0e0f10" {
            "the bytes are kept as they are"
        } else {
            "something was forced"
        }
    );

    // 3. Through Parquet and back.
    let dir = std::env::temp_dir().join("sumimi-id-128-bits");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("u.parquet");
    conn.execute_batch(&format!(
        "COPY (SELECT CAST('0102030405060708090a0b0c0d0e0f10' AS UUID) AS id) TO '{}' (FORMAT PARQUET);",
        file.display()
    ))?;
    let back: String = conn.query_row(
        &format!(
            "SELECT replace(id::VARCHAR, '-', '') FROM read_parquet('{}')",
            file.display()
        ),
        [],
        |r| r.get(0),
    )?;
    println!(
        "  through Parquet    ⇒  {back}   {}",
        if back == "0102030405060708090a0b0c0d0e0f10" {
            "kept"
        } else {
            "CHANGED"
        }
    );
    Ok(())
}

/// The candidates: the label, the type of the id column, the id expression,
/// and how many bits of the id are actually usable.
fn candidates(rows: i64) -> Vec<(&'static str, &'static str, String, String)> {
    let hex = hex_of();
    vec![
        (
            "the packed snowflake (63 bits)",
            "BIGINT",
            format!(
                "SELECT ((i // 1000) * 4194304 + (7 * 4096) + (i % 1000)) AS event_id \
                 FROM range(0, {rows}) t(i)"
            ),
            "63 of 63 (41 ms + 10 instance + 12 seq)".to_string(),
        ),
        (
            "128 bits as a number (HUGEINT)",
            "HUGEINT",
            format!(
                "SELECT ({INSTANCE_BITS}::HUGEINT << 64) | (i::HUGEINT) AS event_id \
                 FROM range(0, {rows}) t(i)"
            ),
            "128 of 128".to_string(),
        ),
        (
            "128 bits as a UUID (all bits kept)",
            "UUID",
            format!(
                "SELECT {uuid} AS event_id FROM range(0, {rows}) t(i)",
                uuid = uuid_of(&hex)
            ),
            "128 of 128".to_string(),
        ),
        (
            "a plain UUIDv7 (no instance part)",
            "UUID",
            format!(
                "SELECT CAST(substr(lpad(to_hex(1750000000000 + i), 12, '0'), 1, 8) || '-' || \
                 substr(lpad(to_hex(1750000000000 + i), 12, '0'), 9, 4) || '-7000-8000-' || \
                 substr(md5(i::VARCHAR), 1, 12) AS UUID) AS event_id FROM range(0, {rows}) t(i)"
            ),
            "122 of 128 (the version and the variant take 6)".to_string(),
        ),
    ]
}

/// **What does the whole event row cost?**
///
/// The id column alone is not the number that matters — the row is.
fn full_row(dir: &std::path::Path, rows: i64) -> anyhow::Result<()> {
    println!();
    println!("=== the whole event row (the real 51 columns) ===");

    let columns = sumimi_service::db::event_columns();
    let ddl = sumimi_service::db::event_columns_ddl();

    // The rows every candidate writes: the real shape, only the id changes.
    let body: Vec<(String, String)> = columns
        .iter()
        .map(|column| {
            let expression = match column.as_str() {
                "event_id" => String::new(), // filled in per candidate
                "website_id" => "'11111111-1111-1111-1111-111111111111'".to_string(),
                "visit_id" | "session_id" | "user_id" => {
                    "CAST(substr(md5(i::VARCHAR), 1, 8) || '-' || substr(md5(i::VARCHAR), 9, 4) || '-' || \
                     substr(md5(i::VARCHAR), 13, 4) || '-' || substr(md5(i::VARCHAR), 17, 4) || '-' || \
                     substr(md5(i::VARCHAR), 21, 12) AS UUID)".to_string()
                }
                "created_at" => "TIMESTAMP '2026-01-01' + INTERVAL (i // 1000) SECOND".to_string(),
                "url_path" => "'/product/' || (i % 200)".to_string(),
                "url_query" => "'utm_source=google&utm_medium=cpc&utm_campaign=spring'".to_string(),
                "hostname" => "'shop.example.com'".to_string(),
                "page_title" => "'Buy widget ' || (i % 200) || ' — our best seller'".to_string(),
                "referrer_domain" => "'www.google.com'".to_string(),
                "referrer_path" => "'/search'".to_string(),
                "referrer_query" => "'q=widget+price'".to_string(),
                "browser" => "'chrome'".to_string(),
                "browser_version" => "'120.0.6099.109'".to_string(),
                "os" => "'macos'".to_string(),
                "os_version" => "'14.2.1'".to_string(),
                "device" => "'desktop'".to_string(),
                "screen" => "'1920x1080'".to_string(),
                "country" => "'US'".to_string(),
                "region" => "'California'".to_string(),
                "city" => "'San Francisco'".to_string(),
                "latitude" | "longitude" | "lcp" | "inp" | "cls" | "fcp" | "ttfb" => {
                    "1.234".to_string()
                }
                "language" => "'en'".to_string(),
                "referrer_name" => "'google'".to_string(),
                "event_type" => "'pageview'".to_string(),
                "event_name" => "'add_to_cart'".to_string(),
                "tag" => "'spring-sale'".to_string(),
                "data" => "'{\"product\":\"widget-42\",\"price\":19.99,\"qty\":1}'".to_string(),
                "session_start" => "TIMESTAMP '2026-01-01'".to_string(),
                "utm_source" => "'google'".to_string(),
                "utm_medium" => "'cpc'".to_string(),
                "utm_campaign" => "'spring_sale'".to_string(),
                "utm_content" => "'ad-variant-b'".to_string(),
                "utm_term" => "'widget'".to_string(),
                "gclid" => "'Ci0KCx'".to_string(),
                "channel" => "'Organic Search'".to_string(),
                "ip" | "ip_prefix" | "user_agent" => "''".to_string(),
                _ => "NULL".to_string(),
            };
            (column.clone(), expression)
        })
        .collect();

    for (label, kind, statement, bits) in candidates(rows) {
        // The id expression, without the "FROM ..." tail.
        let id_expression = statement
            .split(" AS event_id")
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches("SELECT ")
            .to_string();
        // The id column has to be able to hold the id.
        let ddl = ddl.replace("event_id UUID NOT NULL", &format!("event_id {kind} NOT NULL"));

        let expressions: Vec<String> = body
            .iter()
            .map(|(column, expression)| {
                if column == "event_id" {
                    format!("{id_expression} AS event_id")
                } else {
                    format!("{expression} AS {column}")
                }
            })
            .collect();

        // Parquet.
        let out = dir.join(format!("row-{}.parquet", label.replace([' ', '(', ')', '·'], "_")));
        let _ = std::fs::remove_file(&out);
        let conn = Connection::open_in_memory()?;
        let source_min: String = conn.query_row(
            &format!("SELECT min(event_id)::VARCHAR FROM ({statement})"),
            [],
            |r| r.get(0),
        )?;
        let result = conn.execute_batch(&format!(
            "COPY (SELECT {} FROM range(0, {rows}) t(i) ORDER BY created_at) TO '{}' \
             (FORMAT PARQUET, COMPRESSION ZSTD);",
            expressions.join(", "),
            out.display()
        ));
        let parquet = match result {
            Ok(_) => std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) as f64 / rows as f64,
            Err(e) => {
                println!(
                    "  {label:<38} parquet cannot: {}",
                    e.to_string().lines().next().unwrap_or("")
                );
                continue;
            }
        };
        // Did the id survive the trip to Parquet?
        let back_min: String = conn.query_row(
            &format!(
                "SELECT min(event_id)::VARCHAR FROM read_parquet('{}')",
                out.display()
            ),
            [],
            |r| r.get(0),
        )?;
        let survived = back_min == source_min;

        // DuckDB.
        let path = dir.join(format!("row-{}.duckdb", label.replace([' ', '(', ')', '·'], "_")));
        let _ = std::fs::remove_file(&path);
        let other = Connection::open(&path)?;
        // The 128-bit candidates need a column that can hold 128 bits.
        let ddl = ddl.replace("event_id UUID NOT NULL", &format!("event_id {kind} NOT NULL"));
        other.execute_batch(&format!("CREATE TABLE website_event ({ddl});"))?;
        other.execute_batch(&format!(
            "INSERT INTO website_event ({}) SELECT {} FROM range(0, {rows}) t(i);",
            columns.join(", "),
            expressions.join(", ")
        ))?;
        other.execute_batch("CHECKPOINT;")?;
        drop(other);
        let duckdb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / rows as f64;

        println!(
            "  {:<38} parquet {:>6.2} B/row   duckdb {:>6.2} B/row   ({})   {}",
            label,
            parquet,
            duckdb,
            bits,
            if survived {
                "the id survived"
            } else {
                "** THE ID IS BROKEN **"
            }
        );
    }
    Ok(())
}

/// **What does a reference to an event cost?**
fn references(dir: &std::path::Path, rows: i64) -> anyhow::Result<()> {
    println!();
    println!("=== what a reference to an event costs (the EAV rows) ===");
    let conn = Connection::open_in_memory()?;

    for (label, statement) in [
        (
            "the packed snowflake (63 bits)",
            format!(
                "SELECT ((i // 1000) * 4194304 + (7 * 4096) + (i % 1000)) AS website_event_id, \
                 'product_' || CAST(d % 4 AS VARCHAR) AS event_key, \
                 CAST((i * 7 + d) % 1000 AS VARCHAR) AS event_value \
                 FROM range(0, {rows}) t(i), range(0, 10) u(d)"
            ),
        ),
        (
            "128 bits as a UUID",
            format!(
                "SELECT {uuid} AS website_event_id, \
                 'product_' || CAST(d % 4 AS VARCHAR) AS event_key, \
                 CAST((i * 7 + d) % 1000 AS VARCHAR) AS event_value \
                 FROM range(0, {rows}) t(i), range(0, 10) u(d)",
                uuid = uuid_of(&hex_of())
            ),
        ),
    ] {
        let out = dir.join(format!("e-{}.parquet", label.replace([' ', '(', ')'], "_")));
        let _ = std::fs::remove_file(&out);
        let bytes = {
            conn.execute_batch(&format!(
                "COPY ({statement}) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
                out.display()
            ))?;
            std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) as f64
        };
        println!(
            "  {:<34} {:>8.3} B/row   ({:.2} B per event, {} rows)",
            label,
            bytes / (rows * 10) as f64,
            bytes / rows as f64,
            rows * 10
        );
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);

    println!("(measuring with {rows} rows)");
    println!("(the instance part is ONE value for the whole instance; the sequence is a dense counter)");

    let dir = std::env::temp_dir().join("sumimi-id-128");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    bits()?;
    full_row(&dir, rows)?;
    references(&dir, rows)?;

    Ok(())
}
