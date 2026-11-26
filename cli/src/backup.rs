//! Parquet export and archive reading.
//!
//! The hot database is a DuckDB file that the server holds an exclusive lock on — you
//! cannot copy it while the service runs, and copying a 400 GB file is not a backup
//! strategy anyway. So the cold tier is **Parquet**: one file per day per table,
//! written once, never rewritten, readable by anything (DuckDB, Spark, Polars, pandas).
//!
//! ```text
//! sumimi backup --out ./archive                 # write the archive
//! sumimi summary --parquet './archive/*'        # analyse the archive
//! ```
//!
//! **The archive is flat.** The hot instance is not: since v12 every site has a
//! file of its own for its facts. The archive is the other way round — one set
//! of files for the whole instance — so the export **joins the sites back
//! together** (see [`flat_view`]) and writes what a reader of the archive
//! expects. The layout of the archive does not change with the layout of the
//! hot tier: the readers of an archive are many, and they all know one layout.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::NaiveDate;
use duckdb::Connection;

/// What an export produced.
#[derive(Debug, Default)]
pub struct BackupStats {
    /// How many day-files were written.
    pub files: usize,
    /// How many rows were written.
    pub rows: i64,
    /// The directory the archive was written to.
    pub directory: PathBuf,
}

/// The name of the view the export reads.
///
/// The archive is flat and the hot tier is not, so the sites are joined back
/// together under one name before they are written.
const FLAT_EVENTS: &str = "sumimi_flat_events";
/// The name of the view of the revenue.
const FLAT_REVENUE: &str = "sumimi_flat_revenue";
/// The name of the view of the **properties**.
///
/// The properties are a table of their own ⇒ ⇒ the archive holds them as one
/// too: an event without its properties is not the same event.
const FLAT_EVENT_DATA: &str = "sumimi_flat_event_data";

/// Write the hot database to a Parquet archive, partitioned by day.
///
/// One pass per table: DuckDB's `PARTITION_BY (day)` writes one directory per day
/// (`sessions/day=2026-11-03/…`) in a single scan. Scanning per day would be O(days).
///
/// The archive is additive and self-describing: the partition column is part of the
/// file, so any reader (DuckDB, Spark, Polars, pandas) can read it without knowing
/// our layout.
pub fn export(
    conn: &Connection,
    out: &Path,
    website: Option<&str>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    skip_existing: bool,
) -> Result<BackupStats> {
    let mut stats = BackupStats {
        directory: out.to_path_buf(),
        ..Default::default()
    };

    // **The sites are joined back together.** The hot instance holds a file per
    // site; the archive is one flat set of files. So a view is made of all the
    // sites' tables, and the export writes that view — one scan, one pass, and
    // the archive keeps the layout its readers know.
    let catalogs = site_catalogs(conn)?;

    for (table, view, sub) in [
        ("website_event", FLAT_EVENTS, "events"),
        ("event_data", FLAT_EVENT_DATA, "event_data"),
        ("revenue", FLAT_REVENUE, "revenue"),
    ] {
        let directory = out.join(sub);
        std::fs::create_dir_all(&directory)?;

        if skip_existing && has_files(&directory) {
            continue;
        }

        // A table with no rows is not an error: a site without custom events simply
        // has no event archive.
        let Some(flat) = flat_view(conn, &catalogs, table, view)? else {
            continue;
        };

        let (min, max) = match range_of(conn, &flat, website) {
            Ok(range) => range,
            Err(_) => continue,
        };
        let first = from.unwrap_or(min);
        let last = to.unwrap_or(max);

        let filter = match website {
            Some(_) => "website_id = ? AND created_at >= ? AND created_at < ?",
            None => "created_at >= ? AND created_at < ?",
        };

        let count_sql = format!("SELECT count(*) FROM {flat} WHERE {filter}");
        let count: i64 = match website {
            Some(id) => conn.query_row(
                &count_sql,
                duckdb::params![id, start_of(first), start_of(last + chrono::Duration::days(1))],
                |row| row.get(0),
            )?,
            None => conn.query_row(
                &count_sql,
                duckdb::params![start_of(first), start_of(last + chrono::Duration::days(1))],
                |row| row.get(0),
            )?,
        };

        if count == 0 {
            continue;
        }

        // One scan, partitioned by day.
        let copy_sql = format!(
            "COPY (
                 SELECT *, strftime(created_at, '%Y-%m-%d') AS day
                 FROM {flat}
                 WHERE {filter}
             ) TO ? (FORMAT PARQUET, COMPRESSION ZSTD, PARTITION_BY (day))"
        );

        match website {
            Some(id) => conn.execute(
                &copy_sql,
                duckdb::params![
                    id,
                    start_of(first),
                    start_of(last + chrono::Duration::days(1)),
                    directory.to_string_lossy()
                ],
            )?,
            None => conn.execute(
                &copy_sql,
                duckdb::params![
                    start_of(first),
                    start_of(last + chrono::Duration::days(1)),
                    directory.to_string_lossy()
                ],
            )?,
        };

        stats.rows += count;
        stats.files += count_partitions(&directory);
    }

    // The website table is tiny; keep it so an archive can be listed.
    let websites_file = out.join("websites.parquet");
    if !websites_file.exists() {
        conn.execute(
            "COPY (SELECT * FROM website) TO ? (FORMAT PARQUET, COMPRESSION ZSTD)",
            [&websites_file.to_string_lossy()],
        )
        .context("writing the website table")?;
    }

    Ok(stats)
}

/// The catalogs that hold a site's facts.
///
/// **Every site's file is attached under the site's own name**, so the
/// catalogs of the connection are the sites of the instance — and the facts
/// are read out of them by their alias. The main database is one of them: it
/// is the flat shape (the archive, the tools that measure things), and its
/// facts are read plain.
fn site_catalogs(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT database_name FROM duckdb_databases() \
         WHERE database_name NOT IN ('system', 'temp') ORDER BY database_name",
    )?;
    let mut rows = stmt.query([])?;
    let mut catalogs = Vec::new();
    while let Some(row) = rows.next()? {
        catalogs.push(row.get(0)?);
    }
    Ok(catalogs)
}

/// Make a view of every site's table, and say what it is called.
///
/// **The sites are joined back together** — the hot instance holds a file per
/// site, the archive is one flat set of files, so the export reads a view
/// that is all of them at once. That keeps the export to **one scan**: a
/// copy per site and a merge afterwards would write every row twice and
/// would have to be merged by hand.
///
/// ⇒ ⇒ `None` when there is no site with that table at all — an instance
/// without a single site has nothing to archive, and that is not an error.
fn flat_view(
    conn: &Connection,
    catalogs: &[String],
    table: &str,
    view: &str,
) -> Result<Option<String>> {
    // The catalogs that hold the table.
    let mut sources = Vec::new();
    for catalog in catalogs {
        let found: i64 = conn.query_row(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_name = ? AND table_catalog = ?",
            duckdb::params![table, catalog],
            |row| row.get(0),
        )?;
        if found == 0 {
            continue;
        }
        // The main database's tables are written plain; the others carry the
        // catalog in front of them.
        let name = if catalog == "main_db" {
            table.to_string()
        } else {
            format!("{catalog}.{table}")
        };
        sources.push(name);
    }

    if sources.is_empty() {
        return Ok(None);
    }

    let sql = format!(
        "CREATE OR REPLACE VIEW {view} AS {}",
        sources
            .iter()
            .map(|source| format!("SELECT * FROM {source}"))
            .collect::<Vec<_>>()
            .join(" UNION ALL ")
    );
    conn.execute_batch(&sql)
        .with_context(|| format!("joining the sites' {table} tables"))?;
    Ok(Some(view.to_string()))
}

/// Midnight of a day.
fn start_of(day: NaiveDate) -> chrono::NaiveDateTime {
    day.and_hms_opt(0, 0, 0).unwrap()
}

/// Does a directory hold any parquet file?
fn has_files(directory: &Path) -> bool {
    fn walk(directory: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return false;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.is_dir() {
                if walk(&path) {
                    return true;
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("parquet") {
                return true;
            }
        }
        false
    }
    walk(directory)
}

/// How many day-partitions were written?
fn count_partitions(directory: &Path) -> usize {
    std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_dir())
                .count()
        })
        .unwrap_or(0)
}

/// The first and last day that have data in a table.
fn range_of(
    conn: &Connection,
    table: &str,
    website: Option<&str>,
) -> Result<(NaiveDate, NaiveDate)> {
    let (min, max): (Option<chrono::NaiveDateTime>, Option<chrono::NaiveDateTime>) = match website {
        Some(id) => {
            let mut stmt = conn.prepare(&format!(
                "SELECT min(created_at), max(created_at) FROM {table} WHERE website_id = ?"
            ))?;
            let mut rows = stmt.query([id])?;
            match rows.next()? {
                Some(row) => (row.get(0)?, row.get(1)?),
                None => (None, None),
            }
        }
        None => {
            let mut stmt =
                conn.prepare(&format!("SELECT min(created_at), max(created_at) FROM {table}"))?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => (row.get(0)?, row.get(1)?),
                None => (None, None),
            }
        }
    };

    let min = min
        .map(|t| t.date())
        .ok_or_else(|| anyhow!("there is no data to archive"))?;
    let max = max
        .map(|t| t.date())
        .ok_or_else(|| anyhow!("there is no data to archive"))?;
    Ok((min, max))
}

/// Open a Parquet archive as a database the reports can query.
///
/// The archive is read through `read_parquet()` — it is never imported into the hot
/// database, so analysing ten years of history costs nothing but the query itself.
///
/// **The layout of an archive is the service's business** — the archive is flat
/// (one set of files for the whole instance) and the service says so when it
/// opens it, so the archive and the hot tier cannot disagree about it.
pub fn open_archive(pattern: &str) -> Result<sumimi_service::pool::Db> {
    sumimi_service::pool::Db::open_archive(pattern)
}
