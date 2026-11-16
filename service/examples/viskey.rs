//! What the visitor count costs, in its three shapes.
//!
//! The visitor is the pair (the address prefix, the user agent) — but a pair
//! of *strings* has to be hashed on every row. This measures the three ways
//! of counting it, on the same data.
use std::time::Instant;
use sumimi_service::pool::Db;
use sumimi_service::storage::Storage;

fn main() -> anyhow::Result<()> {
    let path = std::path::PathBuf::from("/tmp/viskey.duckdb");
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path, "256MB")?;
    db.create_website("Example", Some("example.com"), 0, None)?;
    db.write_batch(&sumimi_service::testing::traffic(2_000_000))?;

    // The facts live in the site's own file (one file per site), so the
    // queries name the alias that file is attached under.
    let alias = db.ensure_site("w1")?;
    let table = format!("{alias}.website_event");

    let variants: Vec<(&str, String)> = vec![
        (
            "the pair (prefix, agent)",
            format!("SELECT COUNT(DISTINCT (ip_prefix, user_agent)) FROM {table} WHERE website_id='w1'"),
        ),
        (
            "the pair (prefix, an int)",
            format!("SELECT COUNT(DISTINCT (ip_prefix, event_type)) FROM {table} WHERE website_id='w1'"),
        ),
        (
            "a group by the pair",
            format!("SELECT COUNT(*) FROM (SELECT 1 FROM {table} WHERE website_id='w1' GROUP BY ip_prefix, user_agent)"),
        ),
    ];
    for (label, sql) in variants {
        let mut best = f64::MAX;
        let mut value = 0i64;
        for _ in 0..3 {
            let t = Instant::now();
            value = db.with_read(|conn| {
                let v: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
                Ok(v)
            })?;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if ms < best {
                best = ms;
            }
        }
        println!("{label:<26} {best:>8.1} ms  => {value}");
    }
    Ok(())
}
