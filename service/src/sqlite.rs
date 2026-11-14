//! The SQLite backend — the alternative to DuckDB.
//!
//! SQLite is a row store: it stores more bytes than DuckDB for the same data, but it
//! is a single file with no server, it allows many readers next to one writer, and it
//! is available everywhere. For teams that already run everything on SQLite — or that
//! want a backend with no surprises — this is the backend to use.
//!
//! The **same** [`Storage`](crate::storage::Storage) trait as DuckDB, the same
//! reports, the same numbers: the conformance suite runs against both.
//!
//! **One fact table**, exactly like the DuckDB backend: every page view is one row
//! in `event`, and the sessions and visitors are derived from it. Timestamps are
//! stored as **epoch seconds** (INTEGER), which is SQLite's natural representation
//! and makes range filters a plain integer comparison.
//!
//! The SQL is SQLite's dialect on purpose — bucketing, distinct counting and the
//! "the row of the newest hit" lookups all differ between engines.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::models::{
    BreakdownRow, EventRow, Hit, MetricsSummary, MetricValue, Revenue, SessionRow, Website,
};
use crate::pool::{Pool, Pooled};
use crate::query::{Dialect, Granularity, Param};
use crate::storage::{
    group_sessions, session_scan_limit, Dimension, ReportQuery, SessionEvent, Storage, StorageStats,
    SummaryMetrics, WebsitePatch,
};

/// How SQLite spells the kinds.
///
/// **A uuid is text.** SQLite has no uuid type, so the id is written as its
/// text — the same way the session and the visitor's id are.
struct Sqlite;

impl crate::schema::Types for Sqlite {
    fn row_id(&self) -> String {
        // The rowid alias: SQLite counts the rows itself.
        "INTEGER PRIMARY KEY".to_string()
    }
    fn text(&self) -> String {
        "TEXT".to_string()
    }
    fn uuid(&self) -> String {
        "TEXT".to_string()
    }
    fn int(&self) -> String {
        "INTEGER".to_string()
    }
    fn small(&self) -> String {
        "INTEGER".to_string()
    }
    fn real(&self) -> String {
        "REAL".to_string()
    }
    fn time(&self) -> String {
        // Microseconds since the epoch — an integer comparison.
        "INTEGER".to_string()
    }
}

/// The tables of the schema, with their columns.
///
/// **The columns are the shared list** (see [`crate::schema`]) — one list for
/// every engine, spelled here in SQLite's dialect.
const TABLES: &[(&str, &[crate::schema::Column])] = &[
    ("event", crate::schema::EVENT),
    ("revenue", crate::schema::REVENUE),
    ("website", crate::schema::WEBSITE),
    ("meta", crate::schema::META),
];

/// How SQLite spells the things that differ.
///
/// Everything else — the columns, the conditions, the counts — is in the
/// shared queries (see [`crate::query`]).
pub struct SqliteDialect;

impl Dialect for SqliteDialect {
    /// SQLite's parameters are numbered: `?1`, `?2`, …
    fn placeholder(&self, n: usize) -> String {
        format!("?{n}")
    }

    /// The moments are microseconds already.
    fn epoch_us(&self, expr: &str) -> String {
        expr.to_string()
    }

    /// SQLite has no `GREATEST`; the scalar `MAX` is the same thing.
    fn greatest(&self, a: &str, b: &str) -> String {
        format!("MAX({a}, {b})")
    }

    fn cast_double(&self, expr: &str) -> String {
        format!("CAST({expr} AS REAL)")
    }

    /// `strftime` reads **seconds**, so the microseconds are divided first.
    fn bucket(&self, granularity: Granularity, expr: &str) -> String {
        let format = match granularity {
            Granularity::Hour => "%Y-%m-%dT%H:00:00Z",
            Granularity::Day => "%Y-%m-%dT00:00:00Z",
        };
        format!("strftime('{format}', {expr} / 1000000.0, 'unixepoch')")
    }
}

/// A row of SQLite, read by the name of its column.
pub struct SqliteRow<'a>(pub &'a rusqlite::Row<'a>);

impl crate::row::Row for SqliteRow<'_> {
    fn text(&self, name: &str) -> Result<Option<String>> {
        Ok(self.0.get::<_, Option<String>>(name)?)
    }
    fn int(&self, name: &str) -> Result<Option<i64>> {
        Ok(self.0.get::<_, Option<i64>>(name)?)
    }
    fn float(&self, name: &str) -> Result<Option<f64>> {
        Ok(self.0.get::<_, Option<f64>>(name)?)
    }
    /// A uuid is stored as its text.
    fn uuid(&self, name: &str) -> Result<Option<uuid::Uuid>> {
        Ok(self
            .0
            .get::<_, Option<String>>(name)?
            .and_then(|text| uuid::Uuid::parse_str(&text).ok()))
    }
    /// A moment is stored as microseconds since the epoch.
    fn time(&self, name: &str) -> Result<Option<chrono::NaiveDateTime>> {
        Ok(self.0.get::<_, Option<i64>>(name)?.map(from_ts))
    }
}

/// The values of a statement, as SQLite wants them.
fn values(statement: &crate::query::Statement) -> Vec<Box<dyn rusqlite::ToSql>> {
    statement
        .values
        .iter()
        .map(|param| -> Box<dyn rusqlite::ToSql> {
            match param {
            Param::Text(text) => Box::new(text.clone()),
            Param::Int(number) => Box::new(*number),
            Param::Float(number) => Box::new(*number),
            Param::Uuid(id) => Box::new(id.to_string()),
            Param::Time(moment) => Box::new(ts(*moment)),
            Param::Null => Box::new(Option::<String>::None),
            }
        })
        .collect()
}

/// The SQLite backend.
///
/// **One writer, a pool of readers.** SQLite in WAL mode lets many readers work
/// while the writer is busy — the opposite of DuckDB, which takes an exclusive
/// lock on the file. So the dashboard's requests each take a connection from the
/// pool, and a slow report does not hold up the writer.
pub struct SqliteBackend {
    /// The one connection that writes.
    writer: Mutex<Connection>,
    /// The connections the requests read on. `None` for an in-memory database:
    /// every `open_in_memory()` is a *different* database, so there is only the
    /// one connection.
    readers: Option<Pool<Connection>>,
    path: PathBuf,
}

impl SqliteBackend {
    /// Open (or create) a database and make sure the schema exists.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // An older database is **replaced**, not migrated — the same rule as the
        // DuckDB backend (`pool.rs`): the session id's shape changed, and the
        // rows of an old file cannot be carried over. The old file is set aside
        // under its version number, so nothing is destroyed; the analysis data
        // and the site registrations are simply not carried over.
        let mut conn = Connection::open(path)?;
        let version = read_schema_version(&conn)?;
        if version > 0 && version < SCHEMA_VERSION {
            let backup = crate::db::versioned_backup_path(path, version);
            tracing::warn!(
                "the database {} was written by schema v{version}: it is replaced by an empty v{} one — \
                 the analysis data and the site registrations are not carried over; \
                 the old file is kept as {}",
                path.display(),
                SCHEMA_VERSION,
                backup.display()
            );
            // Fold the write-ahead log into the file first, so the copy that is
            // kept aside really holds everything the old database had.
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
            drop(conn);
            std::fs::rename(path, &backup).map_err(|e| {
                anyhow::anyhow!(
                    "setting the old database {} aside as {}: {e}",
                    path.display(),
                    backup.display()
                )
            })?;
            // The log and the index belong to the old file.
            for suffix in ["-wal", "-shm"] {
                let mut name = path.file_name().unwrap_or_default().to_os_string();
                name.push(suffix);
                let _ = std::fs::remove_file(path.with_file_name(name));
            }
            conn = Connection::open(path)?;
        }

        Self::prepare(&conn)?;
        Ok(Self {
            writer: Mutex::new(conn),
            readers: Some(Pool::new(4)),
            path: path.to_path_buf(),
        })
    }

    /// Open an in-memory database (tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::prepare(&conn)?;
        Ok(Self {
            writer: Mutex::new(conn),
            readers: None,
            path: PathBuf::from(":memory:"),
        })
    }

    /// The pragmas and the schema.
    fn prepare(conn: &Connection) -> Result<()> {
        // WAL lets readers work while the writer is busy — the opposite of DuckDB,
        // which takes an exclusive lock on the file.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // The schema is **the shared list** (see [`crate::schema`]) in SQLite's
        // dialect — one row per page view (or custom event), the field set
        // mirroring Umami's `website_event` row for row.
        let mut ddl = String::new();
        for (table, columns) in TABLES {
            ddl.push_str(&crate::schema::table_ddl(table, columns, &Sqlite));
            ddl.push('\n');
        }
        // The two indexes the reports read by.
        ddl.push_str(
            "CREATE INDEX IF NOT EXISTS event_website_created ON event (website_id, created_at);\n\
             CREATE INDEX IF NOT EXISTS revenue_website_created ON revenue (website_id, created_at);\n",
        );
        conn.execute_batch(&ddl)?;

        write_schema_version(conn)?;
        Ok(())
    }

    /// Run `f` with the writer's connection.
    pub fn with_connection<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut conn)
    }

    /// Run `f` with the writer's connection.
    fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        self.with_connection(f)
    }

    /// Move the write-ahead log into the database file.
    ///
    /// SQLite keeps the recent writes in a separate file (`…-wal`) until it is
    /// checkpointed. That is what makes it fast, but it means **the file on disk
    /// is not the whole database** — a copy of it can be missing the newest rows.
    /// Checkpointing is what a backup does before it copies the file.
    pub fn checkpoint(&self) -> Result<()> {
        self.with_connection(|conn| {
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
            Ok(())
        })
    }

    /// Run `f` with a connection from the reader pool.
    ///
    /// This is what the dashboard uses: the request takes a connection, reads and
    /// gives it back. Two requests do not wait for each other.
    pub fn with_reader<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let Some(readers) = &self.readers else {
            // No pool: an in-memory database has exactly one connection.
            let conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            return f(&conn);
        };
        // A SQLite reader gets a fresh snapshot on every read, so a pooled
        // connection is never stale.
        let connection = readers.take(|| self.open_reader())?;
        let pooled = Pooled {
            pool: readers,
            connection: Some(connection),
        };
        f(pooled.get())
    }

    /// Open one more connection to the same file.
    fn open_reader(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)?;
        Self::prepare(&conn)?;
        Ok(conn)
    }
}

/// Epoch seconds of a timestamp.
/// A timestamp, in **microseconds** since the epoch.
///
/// **Microseconds, not seconds.** The seconds lost the fraction of a second,
/// and with it two things that have to be the same on both engines: the
/// timestamp the API serves (DuckDB keeps the microseconds) and the boundary
/// of a window — an event in the same second as the window's end fell
/// outside it here and inside it there.
fn ts(value: chrono::NaiveDateTime) -> i64 {
    value.and_utc().timestamp_micros()
}

/// A UUID out of the text the column holds.
///
/// SQLite has no UUID type, so the ids are stored as their text — the same
/// text they are shown in.
/// Schema version of the SQLite database.
///
/// * v3 — the event-row model, the ids as text.
/// * v4 — the session and visitor ids are **UUIDs**: 16 bytes, not a string.
/// * v5 — the traffic channel is stored with the event.
/// * v6 — the visit id, the referrer's display name and the coordinates.
/// * v7 — the timestamps are **microseconds**, not seconds.
/// * v8 — the **session id is eight bytes**, not sixteen: the segment's start is
///   in the high half, so a report anchors a session's duration on the moment
///   it really started without a second column.
/// * v9 — the **visitor id is not stored**. The address (the raw one and its
///   coarse prefix) and the raw user agent are stored instead, and a visitor
///   is the pair (the prefix, the user agent).
/// * v10 — **both ids are sixteen bytes** (a UUID), like Umami's: the
///   `visitor_id` is stored again, and the `visit_id` is a UUID too. Sixteen
///   bytes have no room for the segment's start, so the start is stored
///   **beside** the id (`session_start`) — it is a fact, and a fact that is
///   worked out of a bucket is not the fact.
const SCHEMA_VERSION: i64 = 10;

/// The schema version the database was written with (`0` for a fresh file).
fn read_schema_version(conn: &Connection) -> Result<i64> {
    // A fresh file has no `meta` table yet — that is not an error, it is a
    // database with nothing in it.
    let has_meta: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
        [],
        |row| row.get(0),
    )?;
    if has_meta == 0 {
        return Ok(0);
    }
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(version.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Write the schema version into the database.
///
/// There is **no migration**: an older database is replaced, not rewritten
/// (see [`SqliteBackend::open`]). So this only says which version the file
/// was written with.
fn write_schema_version(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

/// A timestamp from epoch seconds.
fn from_ts(value: i64) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp_micros(value)
        .map(|d| d.naive_utc())
        .unwrap_or_default()
}

/// Now, as a naive UTC timestamp.
fn crate_now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

impl Storage for SqliteBackend {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    fn websites(&self) -> Result<Vec<Website>> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, domain, created_at, updated_at, retention_days, country_header
                 FROM website ORDER BY created_at DESC",
            )?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(Website {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    domain: row.get(2)?,
                    created_at: from_ts(row.get(3)?),
                    updated_at: from_ts(row.get(4)?),
                    retention_days: row.get(5)?,
                    country_header: row.get(6)?,
                });
            }
            Ok(out)
        })
    }

    fn website(&self, id: &str) -> Result<Option<Website>> {
        self.with_reader(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, name, domain, created_at, updated_at, retention_days, country_header
                     FROM website WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok(Website {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            domain: row.get(2)?,
                            created_at: from_ts(row.get(3)?),
                            updated_at: from_ts(row.get(4)?),
                            retention_days: row.get(5)?,
                            country_header: row.get(6)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    fn create_website(
        &self,
        name: &str,
        domain: Option<&str>,
        retention_days: i32,
        country_header: Option<&str>,
    ) -> Result<Website> {
        let now = crate_now();
        let website = Website {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            domain: domain.map(|d| d.to_string()),
            created_at: now,
            updated_at: now,
            retention_days,
            country_header: country_header.map(|c| c.to_string()),
        };
        self.with(|conn| {
            conn.execute(
                "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    website.id,
                    website.name,
                    website.domain,
                    ts(website.created_at),
                    ts(website.updated_at),
                    website.retention_days,
                    website.country_header
                ],
            )?;
            Ok(())
        })?;
        Ok(website)
    }

    fn update_website(&self, id: &str, patch: &WebsitePatch) -> Result<Website> {
        self.with(|conn| {
            let mut website = conn
                .query_row(
                    "SELECT id, name, domain, created_at, updated_at, retention_days, country_header
                     FROM website WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok(Website {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            domain: row.get(2)?,
                            created_at: from_ts(row.get(3)?),
                            updated_at: from_ts(row.get(4)?),
                            retention_days: row.get(5)?,
                            country_header: row.get(6)?,
                        })
                    },
                )
                .optional()?
                .ok_or_else(|| anyhow::anyhow!("no such website: {id}"))?;

            if let Some(name) = patch.name.as_ref() {
                website.name = name.clone();
            }
            if let Some(domain) = patch.domain.as_ref() {
                // The domain is trimmed and lowercased, exactly like the other
                // backends: a domain is case insensitive, and the tracker
                // matches it case insensitively.
                let value = domain.trim().to_lowercase();
                website.domain = if value.is_empty() { None } else { Some(value) };
            }
            if let Some(retention_days) = patch.retention_days {
                website.retention_days = retention_days;
            }
            if let Some(country_header) = patch.country_header.as_ref() {
                website.country_header = Some(country_header.clone());
            }
            website.updated_at = crate_now();

            conn.execute(
                "UPDATE website SET name = ?2, domain = ?3, updated_at = ?4,
                        retention_days = ?5, country_header = ?6
                 WHERE id = ?1",
                params![
                    website.id,
                    website.name,
                    website.domain,
                    ts(website.updated_at),
                    website.retention_days,
                    website.country_header
                ],
            )?;
            Ok(website)
        })
    }

    fn delete_website(&self, id: &str) -> Result<bool> {
        self.with(|conn| {
            // The event table is the only fact table: the sessions and visitors are
            // derived from it, so deleting the events deletes them too.
            conn.execute("DELETE FROM event WHERE website_id = ?1", params![id])?;
            conn.execute("DELETE FROM revenue WHERE website_id = ?1", params![id])?;
            let removed = conn.execute("DELETE FROM website WHERE id = ?1", params![id])?;
            Ok(removed > 0)
        })
    }

    fn summary(&self, query: &ReportQuery) -> Result<SummaryMetrics> {
        self.with_reader(|conn| summary(conn, query))
    }

    fn summary_with_change(&self, query: &ReportQuery) -> Result<MetricsSummary> {
        let current = self.summary(query)?;
        let mut previous_query = query.clone();
        previous_query.range = query.range.previous();
        let previous = self.summary(&previous_query)?;

        Ok(MetricsSummary {
            pageviews: metric(current.pageviews, previous.pageviews as f64),
            visitors: metric(current.visitors, previous.visitors as f64),
            views: metric(current.views, previous.views as f64),
            bounces: metric(current.bounces, previous.bounces as f64),
            visit_duration: metric(current.visit_duration as i64, previous.visit_duration),
        })
    }

    fn breakdown(&self, query: &ReportQuery, dimension: Dimension) -> Result<Vec<BreakdownRow>> {
        self.with_reader(|conn| breakdown(conn, query, dimension))
    }

    fn timeseries(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
        self.with_reader(|conn| timeseries(conn, query))
    }

    fn events(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
        self.with_reader(|conn| events(conn, query))
    }

    fn query_strings(&self, query: &ReportQuery) -> Result<Vec<(String, i64)>> {
        self.with_reader(|conn| query_strings(conn, query))
    }

    fn event_log(&self, query: &ReportQuery) -> Result<Vec<EventRow>> {
        self.with_reader(|conn| event_log(conn, query))
    }

    fn sessions(&self, query: &ReportQuery) -> Result<Vec<SessionRow>> {
        let events = self.recent_events(query, session_scan_limit(query.limit))?;
        Ok(group_sessions(
            &query.website_id,
            &events,
            query.limit.clamp(1, 1000),
            query.offset,
        ))
    }

    fn recent_events(&self, query: &ReportQuery, limit: i64) -> Result<Vec<SessionEvent>> {
        self.with_reader(|conn| recent_events(conn, query, limit))
    }

    fn write_batch(&self, hits: &[Hit]) -> Result<()> {
        let now = crate_now();
        let kept: Vec<&Hit> = hits
            .iter()
            .filter(|hit| {
                if hit.retention_days > 0 {
                    let cutoff = now - chrono::Duration::days(hit.retention_days as i64);
                    hit.created_at >= cutoff
                } else {
                    true
                }
            })
            .collect();

        self.with(|conn| {
            let tx = conn.transaction()?;

            // Prepare once: re-preparing per row dominated the write path.
            let mut insert = tx.prepare_cached(
                r#"
INSERT INTO event (
    id, website_id, visit_id, ip, ip_prefix, user_agent, user_id, created_at,
    url, url_query, title,
    referrer, referrer_path, referrer_query, referrer_name, hostname, channel,
    browser, browser_version, os, os_version, device, screen, language,
    country, region, city, latitude, longitude,
    utm_source, utm_medium, utm_campaign, utm_content, utm_term,
    gclid, fbclid, msclkid, ttclid, lifatid, twclid,
    lcp, inp, cls, fcp, ttfb,
    event_type, event_name, tag, data,
    visitor_id, session_start
) VALUES (
    NULL, ?1, ?2, ?3, ?4, ?5,
    ?6, ?7, ?8,
    ?9, ?10, ?11, ?12, ?13, ?14,
    ?15, ?16, ?17, ?18, ?19, ?20, ?21,
    ?22, ?23, ?24, ?25, ?26,
    ?27, ?28, ?29, ?30, ?31,
    ?32, ?33, ?34, ?35, ?36, ?37,
    ?38, ?39, ?40, ?41, ?42,
    ?43, ?44, ?45, ?46,
    ?47, ?48,
    ?49, ?50
)"#,
            )?;
            for hit in &kept {
                insert.execute(params![
                    hit.website_id,
                    hit.visit_id.to_string(),
                    hit.ip,
                    hit.ip_prefix,
                    hit.user_agent,
                    hit.user_id.map(|id| id.to_string()),
                    ts(hit.created_at),
                    hit.url,
                    hit.url_query,
                    hit.title,
                    hit.referrer,
                    hit.referrer_path,
                    hit.referrer_query,
                    hit.referrer_name,
                    hit.hostname,
                    hit.channel,
                    hit.browser,
                    hit.browser_version,
                    hit.os,
                    hit.os_version,
                    hit.device,
                    hit.screen,
                    hit.language,
                    hit.country,
                    hit.region,
                    hit.city,
                    hit.latitude,
                    hit.longitude,
                    hit.utm_source,
                    hit.utm_medium,
                    hit.utm_campaign,
                    hit.utm_content,
                    hit.utm_term,
                    hit.gclid,
                    hit.fbclid,
                    hit.msclkid,
                    hit.ttclid,
                    hit.lifatid,
                    hit.twclid,
                    hit.lcp,
                    hit.inp,
                    hit.cls,
                    hit.fcp,
                    hit.ttfb,
                    hit.kind.as_str(),
                    hit.name,
                    hit.tag,
                    hit.data,
                    hit.visitor_id.to_string(),
                    hit.session_start.map(ts),
                ])?;
            }
            drop(insert);

            tx.commit()?;
            Ok(())
        })
    }

    fn write_revenue(&self, records: &[Revenue]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        self.with(|conn| {
            let tx = conn.transaction()?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO revenue (id, website_id, visit_id, event_id, event_name, currency, revenue, created_at)
                 VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for record in records {
                insert.execute(params![
                    record.website_id,
                    record.visit_id.map(|id| id.to_string()),
                    record.event_id,
                    record.event_name,
                    record.currency,
                    record.revenue,
                    ts(record.created_at),
                ])?;
            }
            drop(insert);
            tx.commit()?;
            Ok(())
        })
    }

    fn revenue(&self, query: &ReportQuery) -> Result<Vec<Revenue>> {
        self.with_reader(|conn| revenue(conn, query))
    }

    fn prune(&self, website: Option<&str>) -> Result<(u64, u64)> {
        self.with(|conn| {
            let now = crate_now();
            let mut stmt = conn.prepare(
                "SELECT id, retention_days FROM website WHERE (?1 IS NULL OR id = ?1)",
            )?;
            let rows = stmt
                .query_map(params![website], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);

            let mut events = 0u64;
            let mut revenue = 0u64;
            for (id, retention_days) in rows {
                if retention_days <= 0 {
                    continue;
                }
                let cutoff = now - chrono::Duration::days(retention_days);
                events += conn.execute(
                    "DELETE FROM event WHERE website_id = ?1 AND created_at < ?2",
                    params![id, ts(cutoff)],
                )? as u64;
                revenue += conn.execute(
                    "DELETE FROM revenue WHERE website_id = ?1 AND created_at < ?2",
                    params![id, ts(cutoff)],
                )? as u64;
            }
            Ok((events, revenue))
        })
    }

    fn delete_before(
        &self,
        before: chrono::NaiveDateTime,
        website: Option<&str>,
    ) -> Result<(u64, u64)> {
        self.with(|conn| {
            let mut events = 0u64;
            let mut revenue = 0u64;
            if let Some(website) = website {
                events += conn.execute(
                    "DELETE FROM event WHERE website_id = ?1 AND created_at < ?2",
                    params![website, ts(before)],
                )? as u64;
                revenue += conn.execute(
                    "DELETE FROM revenue WHERE website_id = ?1 AND created_at < ?2",
                    params![website, ts(before)],
                )? as u64;
            } else {
                events += conn.execute(
                    "DELETE FROM event WHERE created_at < ?1",
                    params![ts(before)],
                )? as u64;
                revenue += conn.execute(
                    "DELETE FROM revenue WHERE created_at < ?1",
                    params![ts(before)],
                )? as u64;
            }
            Ok((events, revenue))
        })
    }

    fn erase(&self, website: &str) -> Result<u64> {
        self.with(|conn| {
            let mut removed = 0u64;
            removed += conn.execute("DELETE FROM revenue WHERE website_id = ?1", params![website])? as u64;
            removed += conn.execute("DELETE FROM event WHERE website_id = ?1", params![website])? as u64;
            removed += conn.execute("DELETE FROM website WHERE id = ?1", params![website])? as u64;
            Ok(removed)
        })
    }


    fn stats(&self) -> Result<StorageStats> {
        let websites = self.with_reader(|conn| {
            let mut stmt = conn.prepare("SELECT count(*) FROM website")?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => Ok(row.get::<_, i64>(0)?),
                None => Ok(0),
            }
        })?;

        let size_bytes = std::fs::metadata(&self.path)
            .map(|m| m.len() as i64)
            .unwrap_or(0);

        Ok(StorageStats {
            backend: "sqlite".to_string(),
            memory_bytes: 0,
            memory_limit: "0B".to_string(),
            threads: 1,
            size_bytes,
            websites,
        })
    }
}

// --------------------------------------------------------------------------------------
// The reports
// --------------------------------------------------------------------------------------

/// Compute the summary metrics for a window.
fn summary(conn: &Connection, query: &ReportQuery) -> Result<SummaryMetrics> {
    // The statement is the shared one (see `crate::query`).
    let statement = crate::query::summary(query, &SqliteDialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();
    let mut stmt = conn.prepare(&statement.sql)?;
    let (pageviews, visitors, views) = match stmt.query(params.as_slice())?.next()? {
        Some(row) => (
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ),
        None => (0, 0, 0),
    };
    drop(stmt);

    // The bounces and the average session length.
    let statement = crate::query::summary_sessions(query, &SqliteDialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();
    let mut stmt = conn.prepare(&statement.sql)?;
    let (bounces, duration) = match stmt.query(params.as_slice())?.next()? {
        Some(row) => (row.get::<_, i64>(0)?, row.get::<_, f64>(1)?),
        None => (0, 0.0),
    };
    drop(stmt);

    Ok(SummaryMetrics {
        pageviews,
        visitors,
        views,
        bounces,
        visit_duration: duration,
    })
}

/// Break a website down over one dimension.
fn breakdown(
    conn: &Connection,
    query: &ReportQuery,
    dimension: Dimension,
) -> Result<Vec<BreakdownRow>> {
    let statement = crate::query::breakdown(query, dimension, &SqliteDialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        out.push(BreakdownRow {
            url: if dimension == Dimension::Url {
                Some(name.clone())
            } else {
                None
            },
            name,
            value: row.get(1)?,
            date: None,
        });
    }
    Ok(out)
}

/// The query strings of the visited pages, with how often each was seen.
fn query_strings(conn: &Connection, query: &ReportQuery) -> Result<Vec<(String, i64)>> {
    let statement = crate::query::query_strings(query, &SqliteDialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

/// The page views over time.
fn timeseries(conn: &Connection, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
    let statement = crate::query::timeseries(query, &SqliteDialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        out.push(BreakdownRow {
            name: name.clone(),
            value: row.get(1)?,
            url: None,
            date: Some(name),
        });
    }
    Ok(out)
}

/// The custom events, grouped by name.
fn events(conn: &Connection, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
    let statement = crate::query::events(query, &SqliteDialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(BreakdownRow {
            name: row.get(0)?,
            value: row.get(1)?,
            url: None,
            date: None,
        });
    }
    Ok(out)
}

/// The raw event log.
fn event_log(conn: &Connection, query: &ReportQuery) -> Result<Vec<EventRow>> {
    let statement = crate::query::event_log(query, &SqliteDialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(crate::row::event_log_row(&SqliteRow(row))?);
    }
    Ok(out)
}

/// The most recent events of a window, newest first.
///
/// The session log is built from these: the log reads the last few thousand
/// rows, not the whole window, so its cost does not grow with the size of the
/// database.
fn recent_events(conn: &Connection, query: &ReportQuery, limit: i64) -> Result<Vec<SessionEvent>> {
    let statement = crate::query::recent_events(query, limit, &SqliteDialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(crate::row::session_event_row(&SqliteRow(row))?);
    }
    Ok(out)
}

/// The revenue of a website over a window.
fn revenue(conn: &Connection, query: &ReportQuery) -> Result<Vec<Revenue>> {
    let statement = crate::query::revenue(query, &SqliteDialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let row = SqliteRow(row);
        out.push(Revenue {
            website_id: crate::row::Row::text(&row, "website_id")?.unwrap_or_default(),
            visit_id: crate::row::Row::uuid(&row, "visit_id")?,
            event_id: crate::row::Row::int(&row, "event_id")?,
            event_name: crate::row::Row::text(&row, "event_name")?,
            currency: crate::row::Row::text(&row, "currency")?.unwrap_or_default(),
            revenue: crate::row::Row::float(&row, "revenue")?.unwrap_or_default(),
            created_at: crate::row::Row::time(&row, "created_at")?.unwrap_or_default(),
        });
    }
    Ok(out)
}

/// A metric with its period-over-period change.
fn metric(value: i64, previous: f64) -> MetricValue {
    let change = if previous == 0.0 {
        if value == 0 {
            0.0
        } else {
            100.0
        }
    } else {
        ((value as f64 - previous) / previous) * 100.0
    };
    MetricValue {
        value,
        change: (change * 10.0).round() / 10.0,
    }
}
