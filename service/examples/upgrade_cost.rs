//! What does it cost to widen an id later?
//!
//! The upgrade path is: add a wider column, fill it in the background, switch
//! the reports over, drop the old one. This measures the two expensive-looking
//! steps — `ALTER TABLE ... ADD COLUMN` and the backfill — and whether the
//! two counting queries can run at the same time.
//!
//! ```bash
//! cargo run --release --example upgrade_cost -p sumimi-service -- 2000000
//! ```

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::pool::Db;

fn build(path: &PathBuf, rows: i64) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let db = Db::open(path, "256MB")?;
    db.with_read(|conn| {
        conn.execute_batch(
            "CREATE TABLE probe (
                 id BIGINT, website_id VARCHAR, visit_id BIGINT, visitor_id BIGINT,
                 user_id VARCHAR, created_at TIMESTAMP, url VARCHAR, title VARCHAR,
                 browser VARCHAR, country VARCHAR);",
        )?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_write(|conn| {
        conn.execute_batch(&format!(
            "INSERT INTO probe
             SELECT i AS id, 'w1', ((i // 120000) // 4)::BIGINT << 32 | (hash('v' || (i % 120000)) % 4294967296),
                CAST(hash('v' || (i % 120000)) % 9223372036854775807 AS BIGINT),
                CASE WHEN i % 3 = 0 THEN 'user-' || (i % 120000) ELSE NULL END,
                TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND,
                '/blog/post-' || (i % 5000), 'A title ' || (i % 1000),
                CASE (i % 120000) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                     WHEN 2 THEN 'Firefox' ELSE 'Edge' END,
                'DE'
             FROM generate_series(1, {rows}) t(i)"
        ))?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let path = PathBuf::from("/tmp/upgrade-cost.duckdb");
    build(&path, rows)?;

    let before = std::fs::metadata(&path)?.len();
    println!("{rows} rows, the file is {:.2} MB\n", before as f64 / 1_048_576.0);

    // 1. Add the wider column.
    let db = Db::open(&path, "256MB")?;
    let started = Instant::now();
    db.with_write(|conn| {
        conn.execute_batch("ALTER TABLE probe ADD COLUMN visitor_id16 HUGEINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    let alter = started.elapsed().as_secs_f64() * 1000.0;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    drop(db);
    let after_add = std::fs::metadata(&path)?.len();
    println!("ALTER TABLE ADD COLUMN      : {alter:>8.1}ms  the file {:.2} MB", after_add as f64 / 1_048_576.0);

    // 2. The backfill.
    let db = Db::open(&path, "256MB")?;
    let started = Instant::now();
    db.with_write(|conn| {
        conn.execute_batch(
            "UPDATE probe SET visitor_id16 = \
             CAST(hash('v16-' || visitor_id::VARCHAR) % 170141183460469231731687303715884105727 AS HUGEINT) \
             WHERE visitor_id16 IS NULL",
        )?;
        Ok::<_, anyhow::Error>(())
    })?;
    let backfill = started.elapsed().as_secs_f64() * 1000.0;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    drop(db);
    let after_fill = std::fs::metadata(&path)?.len();
    println!("the backfill (UPDATE)       : {backfill:>8.1}ms  the file {:.2} MB", after_fill as f64 / 1_048_576.0);
    println!("  ⇒ the add column is free; the backfill costs {backfill:.0} ms and {extra} MB\n",
        extra = (after_fill - after_add) as f64 / 1_048_576.0);

    // 3. The old data is still there.
    let db = Db::open(&path, "256MB")?;
    let (rows_now, visitors, wide): (i64, i64, i64) = db.with_read(|conn| {
        let rows: i64 = conn.query_row("SELECT count(*) FROM probe", [], |r| r.get(0))?;
        let visitors: i64 = conn.query_row("SELECT count(DISTINCT visitor_id) FROM probe", [], |r| r.get(0))?;
        let wide: i64 = conn.query_row("SELECT count(DISTINCT visitor_id16) FROM probe", [], |r| r.get(0))?;
        Ok::<_, anyhow::Error>((rows, visitors, wide))
    })?;
    println!("after the upgrade: {rows_now} rows, {visitors} visitors, {wide} wide ids");

    // 4. Two counting queries, one after the other, then at the same time.
    let anon = "SELECT count(DISTINCT visitor_id) FROM probe WHERE user_id IS NULL";
    let logged = "SELECT count(DISTINCT user_id) FROM probe WHERE user_id IS NOT NULL";
    let db = Db::open(&path, "256MB")?;
    let started = Instant::now();
    let _: i64 = db.with_read(|conn| {
        let v: i64 = conn.query_row(anon, [], |r| r.get(0))?;
        Ok::<_, anyhow::Error>(v)
    })?;
    let _: i64 = db.with_read(|conn| {
        let v: i64 = conn.query_row(logged, [], |r| r.get(0))?;
        Ok::<_, anyhow::Error>(v)
    })?;
    let sequential = started.elapsed().as_secs_f64() * 1000.0;

    let db = Db::open(&path, "256MB")?;
    let started = Instant::now();
    let other = Db::open(&path, "256MB")?;
    let handle = std::thread::spawn(move || {
        let _: i64 = other.with_read(|conn| {
            let v: i64 = conn.query_row(logged, [], |r| r.get(0))?;
            Ok::<_, anyhow::Error>(v)
        })?;
        Ok::<_, anyhow::Error>(())
    });
    let _: i64 = db.with_read(|conn| {
        let v: i64 = conn.query_row(anon, [], |r| r.get(0))?;
        Ok::<_, anyhow::Error>(v)
    })?;
    let _ = handle.join();
    let concurrent = started.elapsed().as_secs_f64() * 1000.0;

    println!("\nthe two counts, one after the other: {sequential:.1}ms");
    println!("the two counts, at the same time  : {concurrent:.1}ms");
    let _ = std::fs::remove_file(&path);
    Ok(())
}
