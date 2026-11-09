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
use uuid::Uuid;

use crate::models::{
    BreakdownRow, EventRow, Hit, MetricsSummary, MetricValue, Revenue, SessionRow, Website,
};
use crate::pool::{Pool, Pooled};
use crate::storage::{
    group_sessions, session_scan_limit, Dimension, ReportQuery, SessionEvent, Storage, StorageStats,
    SummaryMetrics, WebsitePatch, PAGEVIEWS,
};

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
        let conn = Connection::open(path)?;
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

        // The migration needs to turn the old text ids into 16-byte UUIDs, and
        // SQLite has no hash of its own — so it borrows one from us.
        register_uuid_function(conn)?;

        conn.execute_batch(
            r#"
-- One row per page view (or custom event). The field set mirrors Umami's
-- `website_event` row for row. No primary key beyond the row id: this table is
-- append only and is only ever read as a filtered scan.
CREATE TABLE IF NOT EXISTS event (
    id              INTEGER PRIMARY KEY,
    website_id      TEXT NOT NULL,
    session_id      BLOB NOT NULL,
    visitor_id      BLOB NOT NULL,
    visit_id        BLOB,
    created_at      INTEGER NOT NULL,

    url             TEXT,
    url_query       TEXT,
    title           TEXT,

    referrer        TEXT,
    referrer_path   TEXT,
    referrer_query  TEXT,
    referrer_name   TEXT,
    hostname        TEXT,
    channel         TEXT,

    browser         TEXT,
    browser_version TEXT,
    os              TEXT,
    os_version      TEXT,
    device          TEXT,
    screen          TEXT,
    language        TEXT,
    country         TEXT,
    region          TEXT,
    city            TEXT,
    latitude        REAL,
    longitude       REAL,

    utm_source      TEXT,
    utm_medium      TEXT,
    utm_campaign    TEXT,
    utm_content     TEXT,
    utm_term        TEXT,

    gclid           TEXT,
    fbclid          TEXT,
    msclkid         TEXT,
    ttclid          TEXT,
    lifatid         TEXT,
    twclid          TEXT,

    lcp             REAL,
    inp             REAL,
    cls             REAL,
    fcp             REAL,
    ttfb            REAL,

    event_type      TEXT,
    event_name      TEXT,
    tag             TEXT,
    data            TEXT
);
CREATE INDEX IF NOT EXISTS event_website_created ON event (website_id, created_at);

CREATE TABLE IF NOT EXISTS revenue (
    id          INTEGER PRIMARY KEY,
    website_id  TEXT NOT NULL,
    session_id  BLOB,
    event_id    INTEGER,
    event_name  TEXT,
    currency    TEXT NOT NULL DEFAULT 'USD',
    revenue     REAL NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS revenue_website_created ON revenue (website_id, created_at);

CREATE TABLE IF NOT EXISTS website (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    domain          TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    retention_days  INTEGER NOT NULL DEFAULT 0,
    country_header  TEXT
);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);

"#,
        )?;

        migrate(conn)?;
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

/// Schema version of the SQLite database.
///
/// * v3 — the event-row model, the ids as text.
/// * v4 — the session and visitor ids are **UUIDs**: 16 bytes, not a string.
/// * v5 — the traffic channel is stored with the event.
/// * v6 — the visit id, the referrer's display name and the coordinates.
/// * v7 — the timestamps are **microseconds**, not seconds.
const SCHEMA_VERSION: i64 = 7;

/// Teach SQLite how to turn an old text id into a UUID.
///
/// SQLite has no hash of its own, so the migration borrows one from us: the same
/// SHA-256 the DuckDB migration uses, so the two engines agree on what an old id
/// becomes.
fn register_uuid_function(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "sumimi_uuid",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<Option<Vec<u8>>> {
            let value = ctx.get::<Option<rusqlite::types::Value>>(0)?;
            Ok(match value {
                None | Some(rusqlite::types::Value::Null) => None,
                // Already a UUID.
                Some(rusqlite::types::Value::Blob(bytes)) if bytes.len() == 16 => Some(bytes),
                Some(rusqlite::types::Value::Text(text)) => {
                    Some(crate::ua::uuid_from_text(&text).as_bytes().to_vec())
                }
                Some(other) => Some(
                    crate::ua::uuid_from_text(&format!("{other:?}"))
                        .as_bytes()
                        .to_vec(),
                ),
            })
        },
    )?;
    Ok(())
}

/// Bring an older database up to date.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    if version > 0 && version < 7 {
        // The timestamps were **seconds**; they are microseconds now — the
        // precision DuckDB keeps. Every stored timestamp is multiplied by a
        // million: the row keeps its moment, and it keeps the fraction of a
        // second it always had.
        conn.execute_batch(
            "UPDATE event SET created_at = created_at * 1000000;
             UPDATE revenue SET created_at = created_at * 1000000;
             UPDATE website SET created_at = created_at * 1000000,
                                updated_at = updated_at * 1000000;",
        )?;
    }

    if version > 0 && version < 6 {
        // The visit id, the referrer's display name and the coordinates were
        // added: the fields the reference implementation has and we did not.
        // SQLite adds a column with an `ALTER TABLE` — no rebuild needed.
        for (column, kind) in [
            ("visit_id", "BLOB"),
            ("referrer_name", "TEXT"),
            ("latitude", "REAL"),
            ("longitude", "REAL"),
        ] {
            if !has_column(conn, "event", column)? {
                conn.execute(&format!("ALTER TABLE event ADD COLUMN {column} {kind}"), [])?;
            }
        }
    }

    if version > 0 && version < 5 {
        // The traffic channel was added: it is worked out once, when the event
        // is collected, and stored with it.
        if !has_column(conn, "event", "channel")? {
            conn.execute("ALTER TABLE event ADD COLUMN channel TEXT", [])?;
        }
    }

    if version > 0 && version < 4 {
        // The session and visitor ids were 36-character strings; they are now
        // 16-byte UUIDs. The conversion is deterministic, so no session is split
        // in two.
        conn.execute_batch(
            "UPDATE event
                SET session_id = sumimi_uuid(session_id),
                    visitor_id = sumimi_uuid(visitor_id);
             UPDATE revenue
                SET session_id = sumimi_uuid(session_id)
                WHERE session_id IS NOT NULL;",
        )?;
    }

    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

/// Whether a table has a column.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Read a UUID out of a row.
///
/// The value is 16 bytes — but a database written before v4 still holds the old
/// text, so that is read too.
fn read_uuid(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    Ok(read_uuid_opt(row, index)?.unwrap_or_default())
}

/// Read a UUID that may be missing.
fn read_uuid_opt(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<Uuid>> {
    match row.get_ref(index)? {
        rusqlite::types::ValueRef::Null => Ok(None),
        rusqlite::types::ValueRef::Blob(bytes) if bytes.len() == 16 => {
            let mut out = [0u8; 16];
            out.copy_from_slice(bytes);
            Ok(Some(Uuid::from_bytes(out)))
        }
        rusqlite::types::ValueRef::Text(text) => Ok(Some(crate::ua::uuid_from_text(
            &String::from_utf8_lossy(text),
        ))),
        _ => Err(rusqlite::Error::InvalidColumnType(
            index,
            "session_id".to_string(),
            rusqlite::types::Type::Blob,
        )),
    }
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

/// The WHERE clause and its parameters.
struct Filters {
    clauses: Vec<String>,
    values: Vec<rusqlite::types::Value>,
}

impl Filters {
    fn new(query: &ReportQuery) -> Self {
        let mut filters = Filters {
            clauses: vec!["website_id = ?".to_string()],
            values: vec![query.website_id.clone().into()],
        };
        filters.push_value("created_at >= ?", ts(query.range.start));
        filters.push_value("created_at < ?", ts(query.range.end));

        for (column, value) in query.dimensions.filters() {
            if let Some(value) = value.as_ref().filter(|v| !v.is_empty()) {
                filters.clauses.push(format!("{column} = ?"));
                filters.values.push(value.clone().into());
            }
        }

        filters
    }

    fn push_value(&mut self, clause: &str, value: i64) {
        self.clauses.push(clause.to_string());
        self.values.push(value.into());
    }

    fn clause(&self) -> String {
        self.clauses.join(" AND ")
    }

    fn params(&self) -> Vec<&dyn rusqlite::ToSql> {
        self.values
            .iter()
            .map(|v| v as &dyn rusqlite::ToSql)
            .collect()
    }
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
    id, website_id, session_id, visitor_id, visit_id, created_at,
    url, url_query, title,
    referrer, referrer_path, referrer_query, referrer_name, hostname, channel,
    browser, browser_version, os, os_version, device, screen, language,
    country, region, city, latitude, longitude,
    utm_source, utm_medium, utm_campaign, utm_content, utm_term,
    gclid, fbclid, msclkid, ttclid, lifatid, twclid,
    lcp, inp, cls, fcp, ttfb,
    event_type, event_name, tag, data
) VALUES (
    NULL, ?1, ?2, ?3, ?4, ?5,
    ?6, ?7, ?8,
    ?9, ?10, ?11, ?12, ?13, ?14,
    ?15, ?16, ?17, ?18, ?19, ?20, ?21,
    ?22, ?23, ?24, ?25, ?26,
    ?27, ?28, ?29, ?30, ?31,
    ?32, ?33, ?34, ?35, ?36, ?37,
    ?38, ?39, ?40, ?41, ?42,
    ?43, ?44, ?45, ?46
)"#,
            )?;
            for hit in &kept {
                insert.execute(params![
                    hit.website_id,
                    hit.session_id.as_bytes().as_slice(),
                    hit.visitor_id.as_bytes().as_slice(),
                    hit.visit_id.map(|id| id.as_bytes().to_vec()),
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
                "INSERT INTO revenue (id, website_id, session_id, event_id, event_name, currency, revenue, created_at)
                 VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for record in records {
                insert.execute(params![
                    record.website_id,
                    record.session_id.map(|id| id.as_bytes().to_vec()),
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
    let filters = Filters::new(query);

    // The page views are counted without the custom events; the visitors and
    // the visits are counted over everything.
    let sql = format!(
        "SELECT COUNT(*) FILTER (WHERE {pageviews}),
                COUNT(DISTINCT visitor_id),
                COUNT(DISTINCT session_id)
         FROM event WHERE {filters}",
        pageviews = PAGEVIEWS,
        filters = filters.clause()
    );
    let mut stmt = conn.prepare(&sql)?;
    let (pageviews, visitors, views) = match stmt.query(filters.params().as_slice())?.next()? {
        Some(row) => (row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?),
        None => (0, 0, 0),
    };
    drop(stmt);

    // Bounces (sessions with a single **page view**) and the average session
    // length. A session that fired a custom event is not a bounce.
    let sql = format!(
        "SELECT COUNT(*) FILTER (WHERE n = 1 AND custom = 0),
                COALESCE(AVG(d), 0)
         FROM (
             SELECT session_id,
                    COUNT(*) FILTER (WHERE {pageviews}) AS n,
                    MAX(CASE WHEN event_type = 'event' THEN 1 ELSE 0 END) AS custom,
                    (MAX(created_at) FILTER (WHERE {pageviews})
                        - MIN(created_at) FILTER (WHERE {pageviews})) / 1000000.0 AS d
             FROM event WHERE {filters}
             GROUP BY session_id
         )",
        pageviews = PAGEVIEWS,
        filters = filters.clause()
    );
    let mut stmt = conn.prepare(&sql)?;
    let (bounces, duration) = match stmt.query(filters.params().as_slice())?.next()? {
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
    let filters = Filters::new(query);

    // Content dimensions count views, audience dimensions count visitors.
    let aggregate = match dimension.aggregate() {
        "views" => "COUNT(*)",
        _ => "COUNT(DISTINCT visitor_id)",
    };

    let sql = format!(
        "SELECT COALESCE({column}, 'unknown') AS name, {aggregate} AS value
         FROM event WHERE {where_clause} AND {pageviews}
         GROUP BY name
         ORDER BY value DESC, name ASC
         LIMIT {limit}",
        column = dimension.column(),
        aggregate = aggregate,
        where_clause = filters.clause(),
        pageviews = PAGEVIEWS,
        limit = query.limit.clamp(1, 1000),
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(filters.params().as_slice())?;
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
    let filters = Filters::new(query);

    let sql = format!(
        "SELECT url_query, COUNT(*) AS value FROM event
         WHERE {where_clause} AND {pageviews} AND url_query IS NOT NULL AND url_query <> ''
         GROUP BY url_query ORDER BY value DESC LIMIT 1000",
        where_clause = filters.clause(),
        pageviews = PAGEVIEWS,
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(filters.params().as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

/// The page views over time.
fn timeseries(conn: &Connection, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
    let filters = Filters::new(query);

    // Bucket by hour for short windows, by day for long ones.
    let format = if (query.range.end - query.range.start).num_seconds() <= 2 * 24 * 3600 {
        "%Y-%m-%dT%H:00:00Z"
    } else {
        "%Y-%m-%dT00:00:00Z"
    };

    let sql = format!(
        // `strftime` with the `unixepoch` modifier reads **seconds**; the
        // timestamps are microseconds.
        "SELECT strftime('{format}', created_at / 1000000.0, 'unixepoch') AS bucket, COUNT(*) AS value
         FROM event WHERE {where_clause} AND {pageviews}
         GROUP BY bucket
         ORDER BY bucket ASC",
        where_clause = filters.clause(),
        pageviews = PAGEVIEWS,
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(filters.params().as_slice())?;
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
    let filters = Filters::new(query);

    let sql = format!(
        "SELECT COALESCE(event_name, 'unknown') AS name, COUNT(*) AS value
         FROM event WHERE {where_clause} AND event_type = 'event'
         GROUP BY name
         ORDER BY value DESC, name ASC",
        where_clause = filters.clause(),
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(filters.params().as_slice())?;
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
    let filters = Filters::new(query);

    let sql = format!(
        "SELECT id, website_id, session_id, created_at, event_name, url, hostname, channel, data,
                referrer, referrer_path, referrer_query, country, city, browser, os,
                utm_source, utm_medium, utm_campaign, lcp, inp, cls, device
         FROM event WHERE {where_clause}
         ORDER BY created_at DESC, id DESC
         LIMIT {limit} OFFSET {offset}",
        where_clause = filters.clause(),
        limit = query.limit.clamp(1, 1000),
        offset = query.offset.max(0),
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(filters.params().as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(EventRow {
            id: row.get::<_, i64>(0)?.to_string(),
            website_id: row.get(1)?,
            session_id: read_uuid_opt(row, 2)?.map(|id| id.to_string()),
            created_at: from_ts(row.get(3)?),
            name: row.get(4)?,
            url: row.get(5)?,
            hostname: row.get(6)?,
            channel: row.get(7)?,
            data: row.get(8)?,
            referrer: row.get(9)?,
            referrer_path: row.get(10)?,
            referrer_query: row.get(11)?,
            country: row.get(12)?,
            city: row.get(13)?,
            browser: row.get(14)?,
            os: row.get(15)?,
            device: row.get(22)?,
            utm_source: row.get(16)?,
            utm_medium: row.get(17)?,
            utm_campaign: row.get(18)?,
            lcp: row.get(19)?,
            inp: row.get(20)?,
            cls: row.get(21)?,
        });
    }
    Ok(out)
}

/// The most recent events of a window, newest first.
///
/// The session log is built from these: the log reads the last few thousand
/// rows, not the whole window, so its cost does not grow with the size of the
/// database.
fn recent_events(conn: &Connection, query: &ReportQuery, limit: i64) -> Result<Vec<SessionEvent>> {
    let filters = Filters::new(query);
    let sql = format!(
        "SELECT id, session_id, visitor_id, created_at, url, title, referrer, referrer_path,
                hostname, channel, browser, os, device, country, region, city, language, screen
         FROM event WHERE {where_clause}
         ORDER BY created_at DESC, id DESC
         LIMIT {limit}",
        where_clause = filters.clause(),
        limit = limit.clamp(1, 1_000_000),
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(filters.params().as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(SessionEvent {
            id: row.get(0)?,
            session_id: read_uuid(row, 1)?,
            visitor_id: read_uuid_opt(row, 2)?,
            created_at: from_ts(row.get(3)?),
            url: row.get(4)?,
            title: row.get(5)?,
            referrer: row.get(6)?,
            referrer_path: row.get(7)?,
            hostname: row.get(8)?,
            channel: row.get(9)?,
            browser: row.get(10)?,
            os: row.get(11)?,
            device: row.get(12)?,
            country: row.get(13)?,
            region: row.get(14)?,
            city: row.get(15)?,
            language: row.get(16)?,
            screen: row.get(17)?,
        });
    }
    Ok(out)
}

/// The revenue of a website over a window.
fn revenue(conn: &Connection, query: &ReportQuery) -> Result<Vec<Revenue>> {
    let filters = Filters::new(query);

    let sql = format!(
        "SELECT id, website_id, session_id, event_id, event_name, currency, revenue, created_at
         FROM revenue WHERE {where_clause}
         ORDER BY created_at DESC, id DESC
         LIMIT {limit} OFFSET {offset}",
        where_clause = filters.clause(),
        limit = query.limit.clamp(1, 1000),
        offset = query.offset.max(0),
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(filters.params().as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(Revenue {
            website_id: row.get(1)?,
            session_id: read_uuid_opt(row, 2)?,
            event_id: row.get(3)?,
            event_name: row.get(4)?,
            currency: row.get(5)?,
            revenue: row.get(6)?,
            created_at: from_ts(row.get(7)?),
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
