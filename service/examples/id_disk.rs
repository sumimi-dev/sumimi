//! **How much disk does an event take?**
//!
//! With the 128-bit id (a 40-bit counter in bytes 11-15, the instance in
//! bytes 0-10), three configurations:
//!
//! - **thin** — the default privacy configuration (#145): no raw IP, no user
//!   agent, only the dimensions the server fills in;
//! - **full** — everything the server can fill in;
//! - **raw** — full, plus the raw IP and the user agent (off by default).
//!
//! And then: how much disk is it when the counter's whole space is used?
//!
//! ```bash
//! cargo run --release --example id_disk -p sumimi-service
//! ```

use duckdb::Connection;

/// The instance part — one value for the whole instance.
const INSTANCE: &str = "'1234567890ABCDEF0123456789ABCDEF'";

/// The 128-bit id: the instance in bytes 0-10 (with the version nibble and
/// the variant bits), the counter in the last six bytes (big-endian).
fn id_expression() -> String {
    format!(
        "CAST(from_hex(
            substr({instance}, 1, 12) || '8' || substr({instance}, 14, 3) || '8' || \
            substr({instance}, 18, 3) || lpad(to_hex(i), 12, '0')
         ) AS UUID)",
        instance = INSTANCE
    )
}

/// **The id the engine hands out** — a plain sequence.
///
/// This is what the id was before it was ours: the engine counts the rows.
fn rowid_expression() -> String {
    "CAST(i AS BIGINT)".to_string()
}

/// **The packed snowflake** — 63 bits: 41 of milliseconds, 10 of instance,
/// 12 of sequence.
fn snowflake_expression() -> String {
    "CAST(((i // 1000) * 4194304 + (7 * 4096) + (i % 1000)) AS BIGINT)".to_string()
}

/// The columns of the event row, filled for one configuration.
fn row_expressions(shape: &str, id: &str) -> Vec<(String, String)> {
    let mut columns: Vec<(String, String)> = vec![
        ("event_id".into(), id.to_string()),
        ("website_id".into(), "'11111111-1111-1111-1111-111111111111'".into()),
        (
            "visit_id".into(),
            "CAST(substr(md5(i::VARCHAR), 1, 8) || '-' || substr(md5(i::VARCHAR), 9, 4) || '-' || \
             substr(md5(i::VARCHAR), 13, 4) || '-' || substr(md5(i::VARCHAR), 17, 4) || '-' || \
             substr(md5(i::VARCHAR), 21, 12) AS UUID)".into(),
        ),
        (
            "session_id".into(),
            "CAST(substr(md5(i::VARCHAR || 's'), 1, 8) || '-' || substr(md5(i::VARCHAR || 's'), 9, 4) || '-' || \
             substr(md5(i::VARCHAR || 's'), 13, 4) || '-' || substr(md5(i::VARCHAR || 's'), 17, 4) || '-' || \
             substr(md5(i::VARCHAR || 's'), 21, 12) AS UUID)".into(),
        ),
        (
            "created_at".into(),
            "TIMESTAMP '2026-01-01' + INTERVAL (i // 1000) SECOND".into(),
        ),
        ("url_path".into(), "'/product/' || (i % 200)".into()),
        ("hostname".into(), "'shop.example.com'".into()),
        ("device".into(), "CASE WHEN i % 3 = 0 THEN 'mobile' ELSE 'desktop' END".into()),
        ("browser".into(), "CASE WHEN i % 4 = 0 THEN 'safari' ELSE 'chrome' END".into()),
        ("os".into(), "CASE WHEN i % 5 = 0 THEN 'ios' ELSE 'macos' END".into()),
        ("country".into(), "CASE WHEN i % 7 = 0 THEN 'DE' WHEN i % 11 = 0 THEN 'JP' ELSE 'US' END".into()),
        ("language".into(), "'en'".into()),
        ("referrer_name".into(), "CASE WHEN i % 6 = 0 THEN 'direct' ELSE 'google' END".into()),
        ("event_type".into(), "'pageview'".into()),
        ("ip".into(), "''".into()),
        ("ip_prefix".into(), "''".into()),
        ("user_agent".into(), "''".into()),
    ];
    if shape != "thin" {
        columns.extend(vec![
            ("url_query".into(), "'utm_source=google&utm_medium=cpc&utm_campaign=spring_sale&gclid=Ci0KC'".into()),
            ("page_title".into(), "'Buy widget ' || (i % 200) || ' — our best seller'".into()),
            ("referrer_domain".into(), "'www.google.com'".into()),
            ("referrer_path".into(), "'/search'".into()),
            ("referrer_query".into(), "'q=widget+price&hl=en'".into()),
            ("browser_version".into(), "'120.0.6099.109'".into()),
            ("os_version".into(), "'14.2.1'".into()),
            ("screen".into(), "'1920x1080'".into()),
            ("region".into(), "'California'".into()),
            ("city".into(), "'San Francisco'".into()),
            ("latitude".into(), "'37.7749'".into()),
            ("longitude".into(), "'-122.4194'".into()),
            ("utm_source".into(), "'google'".into()),
            ("utm_medium".into(), "'cpc'".into()),
            ("utm_campaign".into(), "'spring_sale'".into()),
            ("utm_content".into(), "'ad-variant-b'".into()),
            ("utm_term".into(), "'widget'".into()),
            ("gclid".into(), "'Ci0KCx'".into()),
            ("channel".into(), "'Organic Search'".into()),
            ("event_name".into(), "'add_to_cart'".into()),
            ("tag".into(), "'spring-sale'".into()),
            ("data".into(), "'{\"product\":\"widget-42\",\"price\":19.99,\"qty\":1}'".into()),
            ("session_start".into(), "TIMESTAMP '2026-01-01'".into()),
            ("lcp".into(), "'1.234'".into()),
            ("inp".into(), "'0.087'".into()),
            ("cls".into(), "'0.004'".into()),
            ("fcp".into(), "'0.912'".into()),
            ("ttfb".into(), "'0.234'".into()),
            (
                "user_id".into(),
                "CAST(substr(md5(i::VARCHAR || 'u'), 1, 8) || '-' || substr(md5(i::VARCHAR || 'u'), 9, 4) || '-' || \
                 substr(md5(i::VARCHAR || 'u'), 13, 4) || '-' || substr(md5(i::VARCHAR || 'u'), 17, 4) || '-' || \
                 substr(md5(i::VARCHAR || 'u'), 21, 12) AS UUID)".into(),
            ),
        ]);
    }
    if shape == "raw" {
        columns.retain(|(name, _)| name != "ip" && name != "ip_prefix" && name != "user_agent");
        columns.push((
            "ip".into(),
            "'203.0.113.' || CAST(i % 4096 AS VARCHAR)".into(),
        ));
        columns.push(("ip_prefix".into(), "'203.0.' || CAST(i % 16 AS VARCHAR) || '.0'".into()));
        columns.push((
            "user_agent".into(),
            "CASE i % 5 WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36' \
             WHEN 1 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1' \
             WHEN 2 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36' \
             WHEN 3 THEN 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/119.0.0.0 Safari/537.36' \
             ELSE 'Mozilla/5.0 (iPad; CPU OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1' END".into(),
        ));
    }
    columns
}

fn human(bytes: f64) -> String {
    if bytes < 1024.0 * 1024.0 {
        format!("{:.0} KB", bytes / 1024.0)
    } else if bytes < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", bytes / (1024.0 * 1024.0))
    } else if bytes < 1024.0 * 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", bytes / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.2} TB", bytes / (1024.0 * 1024.0 * 1024.0 * 1024.0))
    }
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);

    println!("(measuring with {rows} rows, the 128-bit id with a 40-bit counter)");

    let dir = std::env::temp_dir().join("sumimi-id-disk");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let conn = Connection::open_in_memory()?;
    // The id is 128 bits now ⇒ the column has to be able to hold it.
    let ddl = sumimi_service::db::event_columns_ddl()
        .replace("event_id BIGINT NOT NULL", "event_id UUID NOT NULL");
    let all_columns = sumimi_service::db::event_columns();

    let mut per_row: Vec<(String, f64, f64)> = Vec::new();
    println!();
    println!("=== one event row ===");
    println!(
        "  {:<6} {:>14} {:>14}",
        "shape", "parquet B/row", "duckdb B/row"
    );
    for (id_label, id_expression) in [
        ("the engine's rowid", rowid_expression()),
        ("the packed snowflake", snowflake_expression()),
        ("the 128-bit v8 id", id_expression()),
    ] {
      for shape in ["thin", "full", "raw"] {
        let expressions = row_expressions(shape, &id_expression);
        let select: Vec<String> = expressions
            .iter()
            .map(|(name, expression)| format!("{expression} AS {name}"))
            .collect();
        // Every column of the schema, in the schema's order: the ones this
        // configuration fills, and NULL for the rest.
        let filled: Vec<(String, String)> = expressions.clone();
        let all: Vec<String> = all_columns
            .iter()
            .map(|column| {
                match filled.iter().find(|(name, _)| name == column) {
                    Some((_, expression)) => format!("{expression} AS {column}"),
                    None => format!("NULL AS {column}"),
                }
            })
            .collect();

        // The id column's type follows the shape.
        let id_type = if id_expression.starts_with("CAST(i AS BIGINT)") || id_expression.starts_with("CAST(((")
        {
            "BIGINT"
        } else {
            "UUID"
        };
        let ddl = ddl.replace("event_id UUID NOT NULL", &format!("event_id {id_type} NOT NULL"));

        // Parquet.
        let out = dir.join(format!("{shape}-{}.parquet", id_label.replace([' ', '\''], "_")));
        let _ = std::fs::remove_file(&out);
        conn.execute_batch(&format!(
            "COPY (SELECT {} FROM range(0, {rows}) t(i) ORDER BY created_at) TO '{}' \
             (FORMAT PARQUET, COMPRESSION ZSTD);",
            select.join(", "),
            out.display()
        ))?;
        let parquet = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) as f64 / rows as f64;

        // DuckDB.
        let path = dir.join(format!("{shape}-{}.duckdb", id_label.replace([' ', '\''], "_")));
        let _ = std::fs::remove_file(&path);
        let other = Connection::open(&path)?;
        other.execute_batch(&format!("CREATE TABLE website_event ({ddl});"))?;
        other.execute_batch(&format!(
            "INSERT INTO website_event ({}) SELECT {} FROM range(0, {rows}) t(i);",
            all_columns.join(", "),
            all.join(", ")
        ))?;
        other.execute_batch("CHECKPOINT;")?;
        drop(other);
        let duckdb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / rows as f64;

        println!(
            "  {:<24} {:<6} {:>14.2} {:>14.2}",
            id_label, shape, parquet, duckdb
        );
        if id_label == "the engine's rowid" {
            per_row.push((shape.to_string(), parquet, duckdb));
        }
      }
    }

    // ---- the counter's whole space ----
    println!();
    println!("=== the counter's whole space, filled ===");
    let thin = per_row[0].1;
    let full = per_row[1].1;
    for (bits, capacity) in [(40u32, 1_099_511_627_776u64), (48, 281_474_976_710_656)] {
        println!(
            "  {bits} 位 ⇒ {capacity} 个事件 ⇒ 瘦形状 {} · 全形状 {}",
            human(capacity as f64 * thin),
            human(capacity as f64 * full)
        );
    }

    // ---- a site, a year ----
    println!();
    println!("=== 一个站，一年 ===");
    println!(
        "  {:>12}  {:>12}  {:>12}",
        "events/day", "瘦形状", "全形状"
    );
    for per_day in [10i64, 100, 1_000, 10_000, 100_000, 1_000_000] {
        let year = per_day as f64 * 365.0;
        println!(
            "  {:>12}  {:>12}  {:>12}",
            format!("{per_day}"),
            human(year * thin),
            human(year * full)
        );
    }
    Ok(())
}
