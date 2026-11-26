//! **What does the marker's own bits cost?**
//!
//! The scale harness measured the two identity columns at 26.15 B/row with a
//! fixed all-ones marker and 21.85 B/row with the self-describing one (which
//! ends in a byte with a zero bit) — the same entropy, a different constant.
//! So find out how wide the engine thinks each constant makes the column.
//!
//! ```bash
//! cargo run --release --example engine_probe -p sumimi-service
//! ```

use duckdb::Connection;

fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join("sumimi-marker-probe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let rows: i64 = 2_000_000;
    // The high half of the id, and what follows it.
    let markers: Vec<(&str, &str)> = vec![
        ("all ones (fixed)", "ffffffffffffffff"),
        ("all ones, minus one bit", "fffffffffffffffe"),
        ("all zeros", "0000000000000000"),
        ("top bit set", "8000000000000000"),
        ("top bit clear", "7fffffffffffffff"),
        ("a counter (dense)", "0000000000000001"),
    ];

    println!("  {:<24} {:>10} {:>12}  {}", "the high half", "B/row", "bits", "the engine's word");
    for (label, marker) in markers {
        let path = dir.join("t.duckdb");
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path)?;
        conn.execute_batch("CREATE TABLE t (id UUID);")?;
        conn.execute_batch(&format!(
            "INSERT INTO t SELECT CAST(substr('{marker}'||substr(sha256(i::VARCHAR),1,16),1,8)||'-'||
                     substr('{marker}'||substr(sha256(i::VARCHAR),1,16),9,4)||'-'||
                     substr('{marker}'||substr(sha256(i::VARCHAR),1,16),13,4)||'-'||
                     substr('{marker}'||substr(sha256(i::VARCHAR),1,16),17,4)||'-'||
                     substr('{marker}'||substr(sha256(i::VARCHAR),1,16),21,12) AS UUID)
             FROM range(0, {rows}) t(i);"
        ))?;
        conn.execute_batch("CHECKPOINT;")?;
        let bytes = std::fs::metadata(&path)?.len() as f64 / rows as f64;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT compression FROM pragma_storage_info('t') WHERE segment_type != 'VALIDITY'",
        )?;
        let mut rows_out = stmt.query([])?;
        let mut how = String::new();
        while let Some(row) = rows_out.next()? {
            how = row.get(0)?;
        }
        // The bits the engine had to keep: the row's bytes, less the file's
        // own overhead, times eight.
        let bits = (bytes - 0.06) * 8.0;
        println!(
            "  {label:<24} {bytes:>10.2} {bits:>12.1}  {how}",
        );
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}
