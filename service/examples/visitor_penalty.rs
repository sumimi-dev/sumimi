//! What does the visitor identity cost — on disk **and** in the report?
//!
//! Three ways to hold the same visitor identity, in the **real** table with
//! the **real** queries:
//!
//! | variant | what the row carries |
//! | --- | --- |
//! | `the pair` | the address prefix and the user agent |
//! | `a 16-byte hash` | one UUID, derived from the pair |
//! | `the pair + a key` | the pair **and** one number derived from it |
//!
//! The pair is what the identity *is*; the hash and the key are what the
//! report counts over. The question is what each costs.

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::db::SCHEMA_VERSION;
use sumimi_service::pool::Db;
use sumimi_service::storage::Storage;

/// The summary's first query: the three numbers at the top of the dashboard.
const SUMMARY: &str = "SELECT COUNT(*) FILTER (WHERE event_type NOT IN ('event', 'identify')),
                              COUNT(DISTINCT {key}),
                              COUNT(DISTINCT visit_id)
                       FROM event WHERE website_id = 'w1'";

/// The summary's second query: the bounces and the average session length.
const BOUNCES: &str = "SELECT COUNT(*) FILTER (WHERE n = 1 AND custom = 0),
                               COALESCE(AVG(d), 0)
                        FROM (
                            SELECT visit_id,
                                   COUNT(*) FILTER (WHERE event_type NOT IN ('event', 'identify')) AS n,
                                   MAX(CASE WHEN event_type = 'event' THEN 1 ELSE 0 END) AS custom,
                                   CASE
                                       WHEN MAX(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify')) IS NULL THEN NULL
                                       WHEN visit_id < 0
                                           THEN GREATEST(
                                               date_diff('microsecond',
                                                   MIN(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify')),
                                                   MAX(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify'))
                                               ), 0) / 1000000.0
                                       ELSE GREATEST(
                                               epoch_us(MAX(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify')))
                                               - (visit_id >> 32) * 1000000, 0) / 1000000.0
                                   END AS d
                            FROM event WHERE website_id = 'w1'
                            GROUP BY visit_id
                        )";

/// The breakdown of an audience dimension — the visitors per browser.
const BREAKDOWN: &str = "SELECT COALESCE(browser, 'unknown') AS name, COUNT(DISTINCT {key}) AS value
                          FROM event WHERE website_id = 'w1' AND event_type NOT IN ('event', 'identify')
                          GROUP BY name ORDER BY value DESC LIMIT 50";

/// The SQL that makes a UUID out of the pair.
fn uuid_of_pair() -> String {
    let digest = "sha256(ip_prefix || chr(31) || user_agent)";
    format!(
        "CAST(substr({digest}, 1, 8) || '-' || substr({digest}, 9, 4) || '-' || \
         substr({digest}, 13, 4) || '-' || substr({digest}, 17, 4) || '-' || \
         substr({digest}, 21, 12) AS UUID)"
    )
}

/// The three variants: the columns the file has, and the key the report counts.
fn variants() -> Vec<(&'static str, String, &'static str, bool)> {
    let uuid = uuid_of_pair();
    vec![
        (
            "the pair (prefix + agent)",
            "CREATE TABLE v.event AS SELECT * FROM event;".to_string(),
            "(ip_prefix, user_agent)",
            true,
        ),
        (
            "a 16-byte hash",
            format!(
                "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                 {uuid} AS visitor_id FROM event;"
            ),
            "visitor_id",
            false,
        ),
        (
            "the pair + an 8-byte key",
            "CREATE TABLE v.event AS SELECT *, hash(ip_prefix || chr(31) || user_agent) AS visitor_key FROM event;".to_string(),
            "visitor_key",
            true,
        ),
    ]
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);

    // The source: the real table, the real traffic.
    let source = PathBuf::from("/tmp/visitor-penalty-source.duckdb");
    let _ = std::fs::remove_file(&source);
    let db = Db::open(&source, "256MB")?;
    db.create_website("Example", Some("example.com"), 0, None)?;
    db.write_batch(&sumimi_service::testing::traffic(rows))?;
    println!("{rows} rows, schema v{SCHEMA_VERSION}\n");
    println!(
        "  {:<28} {:>10} {:>11} {:>11} {:>11}",
        "the identity", "B/row", "summary", "bounces", "browsers"
    );

    for (label, columns, key, has_pair) in variants() {
        // A clean file with this shape — written once, so the size is the size.
        let path = PathBuf::from(format!(
            "/tmp/visitor-penalty-{}.duckdb",
            label.replace([' ', '(', ')', '+'], "_")
        ));
        let _ = std::fs::remove_file(&path);
        db.with_read(|conn| {
            conn.execute_batch(&format!("ATTACH '{}' AS v;", path.display()))?;
            conn.execute_batch(&columns)?;
            conn.execute_batch("DETACH v;")?;
            Ok::<_, anyhow::Error>(())
        })?;

        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let file = Db::open(&path, "256MB")?;

        let summary = SUMMARY.replace("{key}", key);
        let breakdown = BREAKDOWN.replace("{key}", key);
        // The same query, counting the pair itself — only where the pair is
        // still in the file.
        let pair_summary = if has_pair {
            Some(SUMMARY.replace("{key}", "(ip_prefix, user_agent)"))
        } else {
            None
        };
        let pair_breakdown = if has_pair {
            Some(BREAKDOWN.replace("{key}", "(ip_prefix, user_agent)"))
        } else {
            None
        };
        let mut pair_ms = f64::MAX;
        let mut pair_browsers_ms = f64::MAX;
        if let Some(sql) = &pair_summary {
            for _ in 0..3 {
                let started = Instant::now();
                file.with_read(|conn| {
                    let _: i64 = conn.query_row(sql, [], |r| r.get(0))?;
                    Ok::<_, anyhow::Error>(())
                })?;
                pair_ms = pair_ms.min(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        if let Some(sql) = &pair_breakdown {
            for _ in 0..3 {
                let started = Instant::now();
                file.with_read(|conn| {
                    let mut stmt = conn.prepare(sql)?;
                    let mut rows = stmt.query([])?;
                    while rows.next()?.is_some() {}
                    Ok::<_, anyhow::Error>(())
                })?;
                pair_browsers_ms = pair_browsers_ms.min(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        // The number of visitors each way has to be the same.
        let counted: i64 = file.with_read(|conn| {
            let value: i64 = conn.query_row(&summary, [], |r| r.get(1))?;
            Ok::<_, anyhow::Error>(value)
        })?;
        let mut summary_ms = f64::MAX;
        let mut bounces_ms = f64::MAX;
        let mut breakdown_ms = f64::MAX;
        for _ in 0..3 {
            let started = Instant::now();
            file.with_read(|conn| {
                let _: i64 = conn.query_row(&summary, [], |r| r.get(0))?;
                Ok::<_, anyhow::Error>(())
            })?;
            summary_ms = summary_ms.min(started.elapsed().as_secs_f64() * 1000.0);

            let started = Instant::now();
            file.with_read(|conn| {
                let _: i64 = conn.query_row(BOUNCES, [], |r| r.get(0))?;
                Ok::<_, anyhow::Error>(())
            })?;
            bounces_ms = bounces_ms.min(started.elapsed().as_secs_f64() * 1000.0);

            let started = Instant::now();
            file.with_read(|conn| {
                let mut stmt = conn.prepare(&breakdown)?;
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
                Ok::<_, anyhow::Error>(())
            })?;
            breakdown_ms = breakdown_ms.min(started.elapsed().as_secs_f64() * 1000.0);
        }

        println!(
            "  {:<28} {:>10.1} {:>10.1}ms {:>10.1}ms {:>10.1}ms  visitors {:>6}",
            label,
            bytes as f64 / rows as f64,
            summary_ms,
            bounces_ms,
            breakdown_ms,
            counted
        );
        if pair_ms < f64::MAX {
            println!(
                "  {:<28} {:>10} {:>10.1}ms {:>10} {:>10.1}ms",
                "  (counting the pair here)", "", pair_ms, "", pair_browsers_ms
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    Ok(())
}
