//! **Which id shape? One packed column, or a run id and a sequence?**
//!
//! The question behind the question: *the id exists so that the rows point at
//! each other* — and the id's shape decides how big the rows are, how the
//! references look, and what the database has to promise about uniqueness.
//!
//! Three things are measured here:
//!
//! 1. **the widths** — the packed id is a 63-bit snowflake; the split is a
//!    64-bit run id and a 32- or 64-bit sequence;
//! 2. **the run id's cardinality** — how many different run ids appear in one
//!    file decides how well the column compresses;
//! 3. **what DuckDB actually does** — because the hot tier is a DuckDB file,
//!    and a DuckDB limitation would decide the shape on its own.
//!
//! ```bash
//! cargo run --release --example id_design -p sumimi-service
//! ```

use duckdb::Connection;

/// The packed snowflake: 41 bits of milliseconds, 10 bits of instance,
/// 12 bits of sequence. 63 bits, so it fits a signed BIGINT.
const PACKED: &str = "((i // 1000) * 4194304 + (7 * 4096) + (i % 1000))";
/// A run id: one per run of writes. Here: one per 1 000 rows.
const RUN64: &str = "(i // 1000) * 1000003 + 7";
/// The sequence inside the run.
const SEQ64: &str = "i";
const SEQ32: &str = "(i % 4000000000)";
/// A random run id per row — the worst case for a run id column.
const RUN_RANDOM: &str = "(hash(i) % 1000000007)";
/// The same run id, as a 128-bit UUID.
const RUN_UUID: &str = "CAST(substr(lpad(to_hex(i), 12, '0'), 1, 8) || '-' || \
     substr(lpad(to_hex(i), 12, '0'), 9, 4) || '-4000-8000-' || \
     substr(md5(i::VARCHAR), 1, 12) AS UUID)";
/// The rest of the event row — the same in every variant.
const VISIT: &str = "CAST(substr(md5(i::VARCHAR), 1, 8) || '-' || substr(md5(i::VARCHAR), 9, 4) || '-' || \
     substr(md5(i::VARCHAR), 13, 4) || '-' || substr(md5(i::VARCHAR), 17, 4) || '-' || \
     substr(md5(i::VARCHAR), 21, 12) AS UUID)";
const CREATED: &str = "TIMESTAMP '2026-01-01' + INTERVAL (i // 1000) SECOND";
const PATH: &str = "'/product/' || (i % 200)";
const HOST: &str = "'shop.example.com'";

/// The id columns of one variant: the name and the expression.
type Columns = Vec<(&'static str, &'static str)>;

/// The row every variant writes: its id columns, then the rest of the event.
fn select_of(id_columns: &Columns) -> String {
    let mut parts: Vec<String> = id_columns
        .iter()
        .map(|(name, expression)| format!("{expression} AS {name}"))
        .collect();
    parts.push(VISIT.to_string());
    parts.push(format!("{CREATED} AS created_at"));
    parts.push(format!("{PATH} AS url_path"));
    parts.push(HOST.to_string());
    parts.join(", ")
}

fn parquet_bytes(conn: &Connection, sql: &str, out: &std::path::Path) -> anyhow::Result<f64> {
    let _ = std::fs::remove_file(out);
    conn.execute_batch(&format!(
        "COPY ({sql}) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
        out.display()
    ))?;
    Ok(std::fs::metadata(out).map(|m| m.len()).unwrap_or(0) as f64)
}

/// The hot tier: a DuckDB file with **only** the table — nothing else in it,
/// so the id's own cost is what shows.
fn duckdb_bytes(dir: &std::path::Path, name: &str, ddl: &str, insert: &str) -> anyhow::Result<f64> {
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    let conn = Connection::open(&path)?;
    conn.execute_batch(ddl)?;
    conn.execute_batch(insert)?;
    conn.execute_batch("CHECKPOINT;")?;
    drop(conn);
    Ok(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64)
}

/// **One packed column, or two?**
fn widths(dir: &std::path::Path, rows: i64) -> anyhow::Result<()> {
    println!();
    println!("=== the widths ===");
    println!(
        "  {:<32} {:>12} {:>12} {:>12}",
        "the id", "parquet B/row", "duckdb B/row", "the id costs"
    );

    let variants: Vec<(&str, Columns)> = vec![
        ("no id at all", vec![]),
        ("packed: 63 bits (snowflake)", vec![("event_id", PACKED)]),
        (
            "split: 64-bit run + 64-bit seq",
            vec![("run_id", RUN64), ("event_seq", SEQ64)],
        ),
        (
            "split: 64-bit run + 32-bit seq",
            vec![("run_id", RUN64), ("event_seq", SEQ32)],
        ),
        (
            "split: 128-bit run + 64-bit seq",
            vec![("run_id", RUN_UUID), ("event_seq", SEQ64)],
        ),
    ];

    // The floor: the same rows without an id.
    let floor_parquet;
    let floor_duckdb;
    {
        let select = select_of(&Columns::new());
        floor_parquet = parquet_bytes(
            &Connection::open_in_memory()?,
            &format!("SELECT {select} FROM range(0, {rows}) t(i) ORDER BY created_at"),
            &dir.join("w-floor.parquet"),
        )?;
        floor_duckdb = duckdb_bytes(
            dir,
            "w-floor.duckdb",
            "CREATE TABLE t (visit_id UUID, created_at TIMESTAMP, url_path VARCHAR, hostname VARCHAR);",
            &format!("INSERT INTO t SELECT {select} FROM range(0, {rows}) t(i);"),
        )?;
    }

    for (label, id_columns) in &variants {
        let select = select_of(id_columns);
        let ddl = if id_columns.is_empty() {
            "CREATE TABLE t (visit_id UUID, created_at TIMESTAMP, url_path VARCHAR, hostname VARCHAR);".to_string()
        } else {
            let columns: Vec<String> = id_columns
                .iter()
                .map(|(name, _)| {
                    if *name == "event_seq" && label.contains("32-bit") {
                        "event_seq INTEGER".to_string()
                    } else if *name == "run_id" && label.contains("128-bit") {
                        "run_id UUID".to_string()
                    } else {
                        format!("{name} BIGINT")
                    }
                })
                .collect();
            format!(
                "CREATE TABLE t ({}, visit_id UUID, created_at TIMESTAMP, url_path VARCHAR, hostname VARCHAR);",
                columns.join(", ")
            )
        };
        let safe = label.replace([' ', ':', '(', ')'], "_");
        let parquet = parquet_bytes(
            &Connection::open_in_memory()?,
            &format!("SELECT {select} FROM range(0, {rows}) t(i) ORDER BY created_at"),
            &dir.join(format!("w-{safe}.parquet")),
        )?;
        let duck = duckdb_bytes(
            dir,
            &format!("w-{safe}.duckdb"),
            &ddl,
            &format!("INSERT INTO t SELECT {select} FROM range(0, {rows}) t(i);"),
        )?;
        println!(
            "  {:<32} {:>12.2} {:>12.2} {:>12.2}",
            label,
            parquet / rows as f64,
            duck / rows as f64,
            (parquet - floor_parquet) / rows as f64
        );
    }
    Ok(())
}

/// **How many different run ids are in the file?**
///
/// A run id that is the same for the whole file dictionary-encodes away. One
/// that changes every row is a random 64-bit number — the worst case.
fn run_cardinality(dir: &std::path::Path, rows: i64) -> anyhow::Result<()> {
    println!();
    println!("=== how many run ids in the file (parquet) ===");
    let conn = Connection::open_in_memory()?;

    for (label, divisor) in [
        ("1 run (the whole file)", 0i64),
        ("one run per 10 000 rows", 10_000),
        ("one run per 1 000 rows", 1_000),
        ("one run per row", 1),
    ] {
        let (run, seq) = if divisor == 0 {
            ("0".to_string(), "i".to_string())
        } else {
            (format!("(i // {divisor})"), format!("i % {divisor}"))
        };
        let select = format!(
            "({run} * 1000003 + 7) AS run_id, {seq} AS event_seq, {VISIT} AS visit_id, \
             {CREATED} AS created_at, {PATH} AS url_path, {HOST} AS hostname"
        );
        let out = dir.join(format!("c-{divisor}.parquet"));
        let bytes = parquet_bytes(
            &conn,
            &format!("SELECT {select} FROM range(0, {rows}) t(i) ORDER BY created_at"),
            &out,
        )?;
        println!(
            "  {:<26} {:>8.2} B/row   ({} different run ids)",
            label,
            bytes / rows as f64,
            if divisor == 0 { 1 } else { rows / divisor }
        );
    }

    // The worst case: a random run id per row. Nothing to dictionary against.
    let select = format!(
        "{RUN_RANDOM} AS run_id, {SEQ64} AS event_seq, {VISIT} AS visit_id, \
         {CREATED} AS created_at, {PATH} AS url_path, {HOST} AS hostname"
    );
    let out = dir.join("c-random.parquet");
    let bytes = parquet_bytes(
        &conn,
        &format!("SELECT {select} FROM range(0, {rows}) t(i) ORDER BY created_at"),
        &out,
    )?;
    println!(
        "  {:<26} {:>8.2} B/row   (a random run id per row)",
        "random",
        bytes / rows as f64
    );

    // And the packed id, for the same rows — the thing the split is compared to.
    let select = format!(
        "{PACKED} AS event_id, {VISIT} AS visit_id, {CREATED} AS created_at, \
         {PATH} AS url_path, {HOST} AS hostname"
    );
    let out = dir.join("c-packed.parquet");
    let bytes = parquet_bytes(
        &conn,
        &format!("SELECT {select} FROM range(0, {rows}) t(i) ORDER BY created_at"),
        &out,
    )?;
    println!(
        "  {:<26} {:>8.2} B/row   (the packed id, for comparison)",
        "packed",
        bytes / rows as f64
    );
    Ok(())
}

/// **What does a reference to an event cost?**
///
/// The EAV table has one row per (event, dimension) — so the same id repeats
/// across that event's rows.
fn references(dir: &std::path::Path, events: i64, dims: i64) -> anyhow::Result<()> {
    println!();
    println!("=== what a reference to an event costs ===");
    let conn = Connection::open_in_memory()?;

    for (label, select) in [
        (
            "packed: one BIGINT",
            format!(
                "{PACKED} AS website_event_id, 'product_' || CAST(d % 4 AS VARCHAR) AS event_key, \
                 CAST((i * 7 + d) % 1000 AS VARCHAR) AS event_value"
            ),
        ),
        (
            "split: two columns",
            format!(
                "((i // 1000) * 1000003 + 7) AS run_id, i AS website_event_seq, \
                 'product_' || CAST(d % 4 AS VARCHAR) AS event_key, \
                 CAST((i * 7 + d) % 1000 AS VARCHAR) AS event_value"
            ),
        ),
    ] {
        let out = dir.join(format!("e-{}.parquet", label.replace([' ', ':'], "_")));
        let bytes = parquet_bytes(
            &conn,
            &format!(
                "SELECT {select} FROM range(0, {events}) t(i), range(0, {dims}) u(d)"
            ),
            &out,
        )?;
        let rows = events * dims;
        println!(
            "  {:<24} {:>8.3} B/row   ({:.2} B per event, {} rows)",
            label,
            bytes / rows as f64,
            bytes / events as f64,
            rows
        );
    }
    Ok(())
}

/// **What does the id column cost in the hot tier?**
///
/// The hot tier is a DuckDB file. The table here holds **only** the id
/// column, so what the file grows by is the id's own cost — and DuckDB's
/// `storage_info` says which compression it used.
fn duckdb_order(dir: &std::path::Path, rows: i64) -> anyhow::Result<()> {
    println!();
    println!("=== the id column alone, in the hot tier (a DuckDB file) ===");

    let variants: Vec<(&str, &str, String)> = vec![
        ("the table with no rows in it", "", "CREATE TABLE t (id BIGINT);".to_string()),
        (
            "consecutive ids (i)",
            "i",
            "CREATE TABLE t (id BIGINT);".to_string(),
        ),
        (
            "the packed snowflake",
            PACKED,
            "CREATE TABLE t (id BIGINT);".to_string(),
        ),
        (
            "the packed snowflake, random order",
            PACKED,
            "CREATE TABLE t (id BIGINT);".to_string(),
        ),
        (
            "random 64-bit ids",
            "hash(i) % 1000000007",
            "CREATE TABLE t (id BIGINT);".to_string(),
        ),
        (
            "the run id alone (1 000 per file)",
            RUN64,
            "CREATE TABLE t (id BIGINT);".to_string(),
        ),
        (
            "the sequence alone",
            SEQ64,
            "CREATE TABLE t (id BIGINT);".to_string(),
        ),
        (
            "the run id and the sequence",
            "",
            "CREATE TABLE t (run_id BIGINT, event_seq BIGINT);".to_string(),
        ),
    ];

    let mut measured: Vec<(String, u64, String)> = Vec::new();
    for (label, values, ddl) in &variants {
        let random_order = label.contains("random order");
        let select = if values.is_empty() && ddl.contains("run_id") {
            format!("{RUN64} AS run_id, {SEQ64} AS event_seq")
        } else if values.is_empty() {
            String::new()
        } else {
            format!("{values} AS id")
        };
        let insert = if select.is_empty() {
            String::new()
        } else {
            format!(
                "INSERT INTO t SELECT {select} FROM range(0, {rows}) t(i) {};",
                if random_order { "ORDER BY random()" } else { "" }
            )
        };

        let path = dir.join(format!("o-{}.duckdb", label.replace([' ', ','], "_")));
        let _ = std::fs::remove_file(&path);
        let mut how = String::new();
        let bytes = {
            let conn = Connection::open(&path)?;
            conn.execute_batch(ddl)?;
            if !insert.is_empty() {
                conn.execute_batch(&insert)?;
            }
            conn.execute_batch("CHECKPOINT;")?;

            if !select.is_empty() {
                let mut stmt =
                    conn.prepare("SELECT column_name, compression FROM pragma_storage_info('t');")?;
                let mut rows = stmt.query([])?;
                let mut seen: Vec<String> = Vec::new();
                while let Some(row) = rows.next()? {
                    let column: String = row.get(0)?;
                    let compression: String = row.get(1)?;
                    let tag = format!("{column}={compression}");
                    if !seen.contains(&tag) {
                        seen.push(tag);
                    }
                }
                how = seen.join(", ");
            }
            drop(conn);
            std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
        };
        measured.push((label.to_string(), bytes, how));
    }

    let floor = measured[0].1;
    println!(
        "  {:<40} {:>10} {:>10}  {}",
        "the id column", "B/row", "it costs", "how DuckDB stores it"
    );
    for (label, bytes, how) in &measured {
        println!(
            "  {:<40} {:>10.2} {:>10.2}  {}",
            label,
            *bytes as f64 / rows as f64,
            (*bytes as f64 - floor as f64) / rows as f64,
            how
        );
    }
    Ok(())
}

/// **What does the id cost at different traffic rates?**
///
/// The packed id changes its high bits every time the millisecond moves on —
/// and how often that happens depends on how many events a millisecond holds.
/// The split's sequence never jumps. This measures both at the same densities.
fn rate(dir: &std::path::Path, rows: i64) -> anyhow::Result<()> {
    println!();
    println!("=== the id's cost at different traffic rates (parquet) ===");
    let conn = Connection::open_in_memory()?;

    let floor = parquet_bytes(
        &conn,
        &format!(
            "SELECT {VISIT} AS visit_id, {CREATED} AS created_at, {PATH} AS url_path, {HOST} AS hostname \
             FROM range(0, {rows}) t(i) ORDER BY created_at"
        ),
        &dir.join("r-floor.parquet"),
    )?;

    for per_ms in [10i64, 100, 1_000, 10_000] {
        // The packed id: the millisecond moves on every `per_ms` events.
        let packed = format!("(i // {per_ms}) * 4194304 + (7 * 4096) + (i % {per_ms})");
        // The split: the run moves on every `per_ms` events, the sequence
        // counts inside the run and never jumps.
        let split = format!("(i // {per_ms}) AS run_id, (i % {per_ms}) AS event_seq");

        let mut costs = Vec::new();
        for (label, columns) in [
            ("packed", format!("{packed} AS event_id")),
            ("split", split),
        ] {
            let bytes = parquet_bytes(
                &conn,
                &format!(
                    "SELECT {columns}, {VISIT} AS visit_id, {CREATED} AS created_at, {PATH} AS url_path, {HOST} AS hostname \
                     FROM range(0, {rows}) t(i) ORDER BY created_at"
                ),
                &dir.join(format!("r-{per_ms}-{label}.parquet")),
            )?;
            costs.push((label, (bytes - floor) / rows as f64));
        }
        println!(
            "  {:>7} events per millisecond:   packed {:>5.2} B/row   split {:>5.2} B/row",
            per_ms, costs[0].1, costs[1].1
        );
    }
    Ok(())
}

/// **The split in its best form: one run id for the whole instance.**
///
/// The run id does not have to change per batch — it can be **one value for
/// the whole instance** (one UUID, minted once). Then it is the same for
/// every row of every file, which is the case a dictionary can eat.
///
/// This is the shape that answers the real question: does the split remove
/// the need to *manage* an instance segment?
fn best_split(dir: &std::path::Path, rows: i64) -> anyhow::Result<()> {
    println!();
    println!("=== the split in its best form (one run id for the whole instance) ===");
    let conn = Connection::open_in_memory()?;

    let floor_parquet = parquet_bytes(
        &conn,
        &format!(
            "SELECT {VISIT} AS visit_id, {CREATED} AS created_at, {PATH} AS url_path, {HOST} AS hostname \
             FROM range(0, {rows}) t(i) ORDER BY created_at"
        ),
        &dir.join("b-floor.parquet"),
    )?;

    // One UUID for the whole instance: the same value in every row.
    let one_uuid = "CAST('11111111-2222-3333-4444-555555555555' AS UUID)";
    let variants: Vec<(&str, String, String)> = vec![
        (
            "the packed snowflake",
            format!("{PACKED} AS event_id"),
            "CREATE TABLE t (event_id BIGINT, visit_id UUID, created_at TIMESTAMP, url_path VARCHAR, hostname VARCHAR);".to_string(),
        ),
        (
            "split: 64-bit run id + dense seq",
            format!("(i // 1000) * 1000003 + 7 AS run_id, {SEQ64} AS event_seq"),
            "CREATE TABLE t (run_id BIGINT, event_seq BIGINT, visit_id UUID, created_at TIMESTAMP, url_path VARCHAR, hostname VARCHAR);".to_string(),
        ),
        (
            "split: ONE 128-bit UUID + dense seq",
            format!("{one_uuid} AS run_id, {SEQ64} AS event_seq"),
            "CREATE TABLE t (run_id UUID, event_seq BIGINT, visit_id UUID, created_at TIMESTAMP, url_path VARCHAR, hostname VARCHAR);".to_string(),
        ),
        (
            "split: a fresh UUID per row + dense seq",
            format!("{RUN_UUID} AS run_id, {SEQ64} AS event_seq"),
            "CREATE TABLE t (run_id UUID, event_seq BIGINT, visit_id UUID, created_at TIMESTAMP, url_path VARCHAR, hostname VARCHAR);".to_string(),
        ),
    ];

    for (label, columns, ddl) in &variants {
        let safe = label.replace([' ', ':', '(', ')'], "_");
        let parquet = parquet_bytes(
            &conn,
            &format!(
                "SELECT {columns}, {VISIT} AS visit_id, {CREATED} AS created_at, {PATH} AS url_path, {HOST} AS hostname \
                 FROM range(0, {rows}) t(i) ORDER BY created_at"
            ),
            &dir.join(format!("b-{safe}.parquet")),
        )?;
        let duck = duckdb_bytes(
            dir,
            &format!("b-{safe}.duckdb"),
            ddl,
            &format!(
                "INSERT INTO t SELECT {columns}, {VISIT}, {CREATED}, {PATH}, {HOST} FROM range(0, {rows}) t(i);"
            ),
        )?;
        println!(
            "  {:<40} parquet {:>6.2} B/row   duckdb {:>6.2} B/row   (the id costs parquet {:>5.2})",
            label,
            parquet / rows as f64,
            duck / rows as f64,
            (parquet - floor_parquet) / rows as f64
        );
    }

    // The references, with the run id one value for the whole instance.
    println!("  the references (the EAV rows point at an event):");
    for (label, select) in [
        (
            "packed: one BIGINT",
            format!(
                "{PACKED} AS website_event_id, 'product_' || CAST(d % 4 AS VARCHAR) AS event_key, \
                 CAST((i * 7 + d) % 1000 AS VARCHAR) AS event_value"
            ),
        ),
        (
            "split: UUID + seq",
            format!(
                "{one_uuid} AS run_id, i AS website_event_seq, 'product_' || CAST(d % 4 AS VARCHAR) AS event_key, \
                 CAST((i * 7 + d) % 1000 AS VARCHAR) AS event_value"
            ),
        ),
    ] {
        let out = dir.join(format!("b-e-{}.parquet", label.replace([' ', ':'], "_")));
        let bytes = parquet_bytes(
            &conn,
            &format!("SELECT {select} FROM range(0, {rows}) t(i), range(0, 10) u(d)"),
            &out,
        )?;
        println!(
            "    {:<20} {:>6.3} B/row   ({:.2} B per event)",
            label,
            bytes / (rows * 10) as f64,
            bytes / rows as f64
        );
    }
    Ok(())
}

/// **What does DuckDB promise about the id?**
///
/// These are the promises the shape of the id has to cover up for.
fn duckdb_promises() -> anyhow::Result<()> {
    println!();
    println!("=== what DuckDB promises ===");

    let conn = Connection::open_in_memory()?;

    // 1. Does a PRIMARY KEY stop a duplicate?
    conn.execute_batch("CREATE TABLE pk (id BIGINT PRIMARY KEY);")?;
    conn.execute_batch("INSERT INTO pk VALUES (1);")?;
    let duplicate = conn.execute_batch("INSERT INTO pk VALUES (1);");
    println!(
        "  a duplicate with a PRIMARY KEY:  {}",
        match &duplicate {
            Ok(_) => "accepted — the constraint is not enforced".to_string(),
            Err(e) => format!("refused — {e}"),
        }
    );

    // 2. Does a UNIQUE stop a duplicate?
    conn.execute_batch("CREATE TABLE uq (id BIGINT UNIQUE);")?;
    conn.execute_batch("INSERT INTO uq VALUES (1);")?;
    let duplicate = conn.execute_batch("INSERT INTO uq VALUES (1);");
    println!(
        "  a duplicate with a UNIQUE:      {}",
        match &duplicate {
            Ok(_) => "accepted — the constraint is not enforced".to_string(),
            Err(e) => format!("refused — {e}"),
        }
    );

    // 3. Is the rowid a row id that lasts?
    conn.execute_batch("CREATE TABLE r (id BIGINT);")?;
    conn.execute_batch("INSERT INTO r SELECT i FROM range(0, 100) t(i);")?;
    let before: i64 = conn.query_row("SELECT rowid FROM r WHERE id = 50;", [], |row| row.get(0))?;
    conn.execute_batch("DELETE FROM r WHERE id < 50;")?;
    conn.execute_batch("CHECKPOINT;")?;
    let after: i64 = conn.query_row("SELECT rowid FROM r WHERE id = 50;", [], |row| row.get(0))?;
    println!(
        "  the rowid of one row:  {before} before a delete, {after} after — {}",
        if before == after {
            "it stayed"
        } else {
            "it moved ⇒ the rowid is not a row id that lasts"
        }
    );

    // 3b. What does a rewrite do to it?
    conn.execute_batch("CREATE TABLE r2 AS SELECT * FROM r;")?;
    let rewritten: i64 = conn.query_row("SELECT rowid FROM r2 WHERE id = 50;", [], |row| row.get(0))?;
    println!(
        "  the same row after a rewrite (CREATE TABLE ... AS SELECT):  rowid {rewritten} — {}",
        if rewritten == before {
            "it stayed"
        } else {
            "it moved ⇒ the rowid is a place, not an id"
        }
    );

    // 4. Does a sequence hand out the numbers it promises?
    conn.execute_batch("CREATE SEQUENCE s START 1;")?;
    let first: i64 = conn.query_row("SELECT nextval('s');", [], |row| row.get(0))?;
    let second: i64 = conn.query_row("SELECT nextval('s');", [], |row| row.get(0))?;
    println!(
        "  a sequence:  {first}, {second} — it counts, it does not promise a gap-free run"
    );

    // 5. Is there an identity column?
    let identity = conn.execute_batch("CREATE TABLE id_test (id BIGINT GENERATED ALWAYS AS IDENTITY);");
    println!(
        "  GENERATED ALWAYS AS IDENTITY:  {}",
        match &identity {
            Ok(_) => "supported".to_string(),
            Err(e) => format!(
                "not supported — {}",
                e.to_string().lines().next().unwrap_or("")
            ),
        }
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);

    println!("(measuring with {rows} rows)");

    let dir = std::env::temp_dir().join("sumimi-id-design");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    widths(&dir, rows)?;
    run_cardinality(&dir, rows)?;
    references(&dir, rows, 10)?;
    best_split(&dir, rows)?;
    duckdb_order(&dir, rows)?;
    rate(&dir, rows)?;
    duckdb_promises()?;

    Ok(())
}
