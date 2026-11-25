//! **How should the 128 bits be cut?**
//!
//! The id is 16 bytes: *which instance* and *which event*. The question is
//! where the cut goes — and whether the cut changes how well it compresses.
//!
//! Three things are measured:
//!
//! 1. **the split point** — 64 + 64 (both halves whole bytes), 60 + 62 (the
//!    v8-conformant layout: the version nibble and the variant bits take
//!    six bits out of the middle), 60 + 68 (the cut is *not* on a byte
//!    boundary ⇒ one byte holds both the instance and the counter);
//! 2. **which half holds the counter** — and in which byte order the
//!    counter is written (the byte that changes every row is either at the
//!    front of the half or at the back);
//! 3. **the cost of it** — in the hot tier (a DuckDB file) and in the cold
//!    tier (Parquet).
//!
//! ```bash
//! cargo run --release --example id_layout -p sumimi-service
//! ```

use duckdb::Connection;

/// The instance part — one value for the whole instance (16 hex characters).
const INSTANCE: &str = "1234567890ABCDEF";

/// The 32 hex characters of a 16-byte value, as a UUID.
///
/// `hex` is the expression that produces the 32 characters.
fn uuid_text(hex: &str) -> String {
    // From the raw bytes: the UUID *text* form has fixed field boundaries,
    // so a cut that is not on a byte boundary can only be written this way.
    format!("CAST(from_hex({hex}) AS UUID)", hex = hex)
}

/// The layouts: the label, the 32 hex characters, and what it means.
///
/// The counter's width is the question: it has to hold every event the
/// instance (or the site) will ever write, and the instance half gets the
/// rest.
fn layouts() -> Vec<(&'static str, String, String)> {
    // The counter, big-endian, in the bytes it occupies.
    let seq16 = "lpad(to_hex(i), 16, '0')".to_string();
    let seq14 = "lpad(to_hex(i), 14, '0')".to_string();
    let seq12 = "lpad(to_hex(i), 12, '0')".to_string();
    let seq10 = "lpad(to_hex(i), 10, '0')".to_string();

    // The instance half, with the version nibble (byte 6) and the variant
    // bits (byte 8) forced into it.
    let instance = |width: usize| {
        let full = format!(
            "'{}'",
            &"1234567890ABCDEF0123456789ABCDEF"[..width]
        );
        format!(
            "substr({full}, 1, 12) || '8' || substr({full}, 14, 3) || '8' || substr({full}, 18, {})",
            width - 17
        )
    };

    vec![
        (
            "64+64 · counter in bytes 8-15 (the baseline)",
            format!("'1234567890ABCDEF' || {seq16}"),
            "the counter fills the low half; the instance is bytes 0-7".to_string(),
        ),
        (
            "48+76 · counter in bytes 10-15 (v8)",
            format!("{} || {seq12}", instance(20)),
            "the counter is 48 bits, byte-aligned, its low byte last".to_string(),
        ),
        (
            "56+72 · counter in bytes 9-15 (v8)",
            format!("{} || {seq14}", instance(18)),
            "the counter is 56 bits; it straddles the middle of the 16 bytes"
                .to_string(),
        ),
        (
            "40+88 · counter in bytes 11-15 (v8)",
            format!("{} || {seq10}", instance(22)),
            "the counter is 40 bits".to_string(),
        ),
        (
            "40+64 · counter in bytes 8-15, the top 24 bits zero",
            format!(
                "substr('1234567890ABCDEF', 1, 12) || '8' || substr('1234567890ABCDEF', 14, 3) || \
                 '000000' || {seq10}"
            ),
            "the counter fills the low half but uses only 40 bits (not a legal v8: the variant is 0)"
                .to_string(),
        ),
        (
            "80+48 · the instance is 80 free bits (bytes 0-9)",
            // Nothing forced: the version nibble and the variant bits are
            // whatever the instance's random bits happen to be.
            format!("'1234567890ABCDEF0123' || {seq12}"),
            "80 bits of instance + 48 bits of counter = all 128 ⇒ not a legal v8".to_string(),
        ),
        (
            "74+48 · the same, but version = 8 and variant = 0b10",
            format!("{} || {seq12}", instance(20)),
            "the same 80 bits, six of them pinned ⇒ a legal v8".to_string(),
        ),
        (
            "48+64 · counter in bytes 8-15, the top 16 bits zero",
            format!(
                "substr('1234567890ABCDEF', 1, 12) || '8' || substr('1234567890ABCDEF', 14, 3) || \
                 '0000' || {seq12}"
            ),
            "the counter fills the low half but uses only 48 bits".to_string(),
        ),
    ]
}

fn parquet_bytes(conn: &Connection, sql: &str, out: &std::path::Path) -> anyhow::Result<f64> {
    let _ = std::fs::remove_file(out);
    conn.execute_batch(&format!(
        "COPY ({sql}) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
        out.display()
    ))?;
    Ok(std::fs::metadata(out).map(|m| m.len()).unwrap_or(0) as f64)
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);

    println!("(measuring with {rows} rows)");
    println!("(the instance is ONE value for the whole instance; the counter is a dense counter)");

    let dir = std::env::temp_dir().join("sumimi-id-layout");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let conn = Connection::open_in_memory()?;

    // ---- the hot tier: the id column alone, in a DuckDB file ----
    println!();
    println!("=== the hot tier (a DuckDB file): the id column alone ===");
    let floor = {
        let path = dir.join("empty.duckdb");
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path)?;
        conn.execute_batch("CREATE TABLE t (id UUID);")?;
        conn.execute_batch("CHECKPOINT;")?;
        drop(conn);
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    };
    for (label, hex, _) in layouts() {
        let path = dir.join(format!("{}.duckdb", label.replace([' ', '(', ')', '·', '+'], "_")));
        let _ = std::fs::remove_file(&path);
        let mut how = String::new();
        let bytes = {
            let conn = Connection::open(&path)?;
            conn.execute_batch("CREATE TABLE t (id UUID);")?;
            conn.execute_batch(&format!(
                "INSERT INTO t SELECT {uuid} FROM range(0, {rows}) t(i);",
                uuid = uuid_text(&hex)
            ))?;
            conn.execute_batch("CHECKPOINT;")?;
            let mut stmt =
                conn.prepare("SELECT column_name, compression FROM pragma_storage_info('t');")?;
            let mut out_rows = stmt.query([])?;
            let mut seen: Vec<String> = Vec::new();
            while let Some(row) = out_rows.next()? {
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
        println!(
            "  {:<48} {:>8.2} B/row   {:>7.2}   {}",
            label,
            bytes as f64 / rows as f64,
            (bytes as f64 - floor as f64) / rows as f64,
            how
        );
    }

    // ---- the cold tier: the same id column in Parquet ----
    println!();
    println!("=== the cold tier (Parquet): the id column alone ===");
    for (label, hex, note) in layouts() {
        let out = dir.join(format!("{}.parquet", label.replace([' ', '(', ')', '·', '+'], "_")));
        let bytes = parquet_bytes(
            &conn,
            &format!(
                "SELECT {uuid} AS id FROM range(0, {rows}) t(i)",
                uuid = uuid_text(&hex)
            ),
            &out,
        )?;
        // Did the id survive?
        let back: String = conn.query_row(
            &format!(
                "SELECT replace(id::VARCHAR, '-', '') FROM read_parquet('{}') LIMIT 1",
                out.display()
            ),
            [],
            |r| r.get(0),
        )?;
        let first: String = conn.query_row(
            &format!(
                "SELECT replace(({uuid})::VARCHAR, '-', '') FROM range(0, 1) t(i)",
                uuid = uuid_text(&hex)
            ),
            [],
            |r| r.get(0),
        )?;
        println!(
            "  {:<48} {:>8.2} B/row   {}   ({})",
            label,
            bytes / rows as f64,
            if back == first { "kept" } else { "CHANGED" },
            note
        );
    }

    Ok(())
}
