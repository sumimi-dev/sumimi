//! What does the ALTER actually write? A trace of the directory, every 2ms.
//!
//! The upgrade looked free — the database file ends up the same size it
//! started — but something in the directory reaches several gigabytes while
//! it runs. This watches every file in the directory and prints what appears,
//! how big it gets, and when it goes away.
//!
//! ```bash
//! cargo run --release --example alter_trace -p sumimi-service -- 1000000 /tmp/trace/x.duckdb
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use duckdb::Connection;

fn build(path: &PathBuf, rows: i64) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE probe (
             id BIGINT, website_id VARCHAR, visit_id BIGINT, visitor_id BIGINT, user_id VARCHAR,
             created_at TIMESTAMP, url VARCHAR, title VARCHAR, browser VARCHAR, country VARCHAR);",
    )?;
    conn.execute_batch(&format!(
        "INSERT INTO probe
         SELECT i AS id, 'w1' AS website_id,
            (((i // 120000) // 4)::BIGINT << 32) | (hash('v' || (i % 120000)) % 4294967296),
            CAST(hash('v' || (i % 120000)) % 9223372036854775807 AS BIGINT),
            CASE WHEN i % 3 = 0 THEN NULL ELSE 'user-' || (i % 120000) END,
            TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND,
            '/blog/post-' || (i % 5000), 'A title number ' || (i % 1000),
            CASE (i % 120000) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                 WHEN 2 THEN 'Firefox' ELSE 'Edge' END, 'DE'
         FROM generate_series(1, {rows}) t(i)"
    ))?;
    conn.execute_batch("CHECKPOINT")?;
    Ok(())
}

/// Every file in the directory: name and size.
fn snapshot(dir: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((name, len));
        }
    }
    out.sort();
    out
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rows: i64 = args.first().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let path = PathBuf::from(
        args.get(1).cloned().unwrap_or_else(|| "/tmp/trace/x.duckdb".to_string()),
    );
    let dir = path.parent().unwrap_or(Path::new("/tmp")).to_path_buf();
    std::fs::create_dir_all(&dir)?;

    build(&path, rows)?;
    println!("{rows} rows, before the upgrade:");
    for (name, len) in snapshot(&dir) {
        println!("  {name}: {len} bytes");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (stop, dir) = (stop.clone(), dir.clone());
        std::thread::spawn(move || {
            let mut last: Vec<(String, u64)> = Vec::new();
            let started = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                let now = snapshot(&dir);
                if now != last {
                    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                    let mut parts: Vec<String> = Vec::new();
                    for (name, len) in &now {
                        let was = last.iter().find(|(n, _)| n == name).map(|(_, l)| *l);
                        let note = match was {
                            Some(old) if old != *len => format!(" (was {old})",),
                            Some(_) => String::new(),
                            None => " (new)".to_string(),
                        };
                        parts.push(format!("{name} {len}{note}"));
                    }
                    println!("  [{elapsed:>8.1}ms] {}", parts.join(", "));
                    last = now;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };

    std::thread::sleep(Duration::from_millis(50));
    let conn = Connection::open(&path)?;
    let started = Instant::now();
    conn.execute_batch("ALTER TABLE probe ALTER COLUMN visitor_id TYPE HUGEINT")?;
    let alter = started.elapsed().as_secs_f64() * 1000.0;
    println!("the alter: {alter:.1}ms");
    std::thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    conn.execute_batch("CHECKPOINT")?;
    println!("the checkpoint: {:.1}ms", started.elapsed().as_secs_f64() * 1000.0);
    std::thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::Relaxed);
    let _ = watcher.join();

    println!("\nafter the upgrade:");
    for (name, len) in snapshot(&dir) {
        println!("  {name}: {len} bytes");
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}
