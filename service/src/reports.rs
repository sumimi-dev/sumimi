//! The DuckDB backend.
//!
//! Every report and every write goes through here. The SQL is DuckDB-specific on
//! purpose — timestamps, date bucketing, distinct counting and upsert syntax all
//! differ between engines, so each backend implements the trait in its own dialect.
//!
//! The SQLite and PostgreSQL backends live in the private `sumimi-pro` repository.

use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::Result;
use chrono::NaiveDateTime;
use duckdb::types::Value;
use duckdb::Connection;

use crate::models::{BreakdownRow, EventRow, Hit, MetricsSummary, MetricValue, SessionRow, Website};
use crate::pool::Db;
use crate::query::{Dialect, Granularity, Param};
use crate::storage::{
    group_sessions, session_scan_limit, Dimension, ReportQuery, SessionEvent, Storage, StorageStats,
    SummaryMetrics, WebsitePatch,
};

/// How DuckDB spells the things that differ.
///
/// Everything else — the columns, the conditions, the counts — is in the
/// shared queries (see [`crate::query`]).
struct Duck;

impl Dialect for Duck {
    /// DuckDB's parameters are positional: the number does not matter.
    fn placeholder(&self, _n: usize) -> String {
        "?".to_string()
    }

    /// The moments are timestamps here, so they have to be counted.
    fn epoch_us(&self, expr: &str) -> String {
        format!("epoch_us({expr})")
    }

    fn greatest(&self, a: &str, b: &str) -> String {
        format!("GREATEST({a}, {b})")
    }

    fn cast_double(&self, expr: &str) -> String {
        format!("CAST({expr} AS DOUBLE)")
    }

    fn bucket(&self, granularity: Granularity, expr: &str) -> String {
        let format = match granularity {
            Granularity::Hour => "%Y-%m-%dT%H:00:00Z",
            Granularity::Day => "%Y-%m-%dT00:00:00Z",
        };
        format!(
            "strftime(date_trunc('{}', {expr}), '{format}')",
            granularity.name()
        )
    }
}

/// A row of DuckDB, read by the name of its column.
struct DuckRow<'a>(&'a duckdb::Row<'a>);

impl crate::row::Row for DuckRow<'_> {
    fn text(&self, name: &str) -> Result<Option<String>> {
        Ok(self.0.get::<_, Option<String>>(name)?)
    }
    fn int(&self, name: &str) -> Result<Option<i64>> {
        Ok(self.0.get::<_, Option<i64>>(name)?)
    }
    fn float(&self, name: &str) -> Result<Option<f64>> {
        Ok(self.0.get::<_, Option<f64>>(name)?)
    }
    fn uuid(&self, name: &str) -> Result<Option<uuid::Uuid>> {
        Ok(self.0.get::<_, Option<uuid::Uuid>>(name)?)
    }
    fn time(&self, name: &str) -> Result<Option<NaiveDateTime>> {
        Ok(self.0.get::<_, Option<NaiveDateTime>>(name)?)
    }
}

/// The values of a statement, as DuckDB wants them.
fn values(statement: &crate::query::Statement) -> Vec<Value> {
    statement.values.iter().map(value).collect()
}

/// One value, as DuckDB wants it.
fn value(param: &Param) -> Value {
    match param {
        Param::Text(text) => Value::Text(text.clone()),
        Param::Int(number) => Value::BigInt(*number),
        Param::Float(number) => Value::Double(*number),
        // A uuid is bound as its text — the same way the appender binds it.
        Param::Uuid(id) => Value::Text(id.to_string()),
        Param::Time(moment) => timestamp(*moment),
        Param::Null => Value::Null,
    }
}

impl Storage for Db {
    fn backend(&self) -> &'static str {
        "duckdb"
    }

    fn websites(&self) -> Result<Vec<Website>> {
        self.with_read(websites)
    }

    fn website(&self, id: &str) -> Result<Option<Website>> {
        self.with_read(|conn| website(conn, id))
    }

    fn create_website(
        &self,
        name: &str,
        domain: Option<&str>,
        retention_days: i32,
        country_header: Option<&str>,
    ) -> Result<Website> {
        self.with_write(|conn| create_website(conn, name, domain, retention_days, country_header))
    }

    fn update_website(&self, id: &str, patch: &WebsitePatch) -> Result<Website> {
        self.with_write(|conn| update_website(conn, id, patch))
    }

    fn delete_website(&self, id: &str) -> Result<bool> {
        self.with_write(|conn| {
            // The event table is the only fact table: sessions and visitors are
            // derived from it, so deleting the events deletes them too.
            conn.execute("DELETE FROM event WHERE website_id = ?", [id])?;
            conn.execute("DELETE FROM revenue WHERE website_id = ?", [id])?;
            let removed = conn.execute("DELETE FROM website WHERE id = ?", [id])?;
            Ok(removed > 0)
        })
    }

    fn summary(&self, query: &ReportQuery) -> Result<SummaryMetrics> {
        self.with_read(|conn| summary(conn, query))
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
        self.with_read(|conn| breakdown(conn, query, dimension))
    }

    fn timeseries(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
        self.with_read(|conn| timeseries(conn, query))
    }

    fn events(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
        self.with_read(|conn| events(conn, query))
    }

    fn query_strings(&self, query: &ReportQuery) -> Result<Vec<(String, i64)>> {
        self.with_read(|conn| query_strings(conn, query))
    }

    fn event_log(&self, query: &ReportQuery) -> Result<Vec<EventRow>> {
        self.with_read(|conn| event_log(conn, query))
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
        self.with_read(|conn| recent_events(conn, query, limit))
    }

    fn write_batch(&self, hits: &[Hit]) -> Result<()> {
        self.with_write(|conn| write_batch(conn, hits, &self.next_id))
    }

    fn write_revenue(&self, records: &[crate::models::Revenue]) -> Result<()> {
        self.with_write(|conn| write_revenue(conn, records))
    }

    fn revenue(&self, query: &ReportQuery) -> Result<Vec<crate::models::Revenue>> {
        self.with_read(|conn| revenue(conn, query))
    }

    fn prune(&self, website: Option<&str>) -> Result<(u64, u64)> {
        self.with_write(|conn| prune(conn, website))
    }

    fn delete_before(
        &self,
        before: chrono::NaiveDateTime,
        website: Option<&str>,
    ) -> Result<(u64, u64)> {
        self.with_write(|conn| delete_before(conn, before, website))
    }

    fn erase(&self, website: &str) -> Result<u64> {
        self.with_write(|conn| {
            let revenue = conn.execute("DELETE FROM revenue WHERE website_id = ?", [website])?;
            let events = conn.execute("DELETE FROM event WHERE website_id = ?", [website])?;
            let websites = conn.execute("DELETE FROM website WHERE id = ?", [website])?;
            Ok((revenue + events + websites) as u64)
        })
    }


    fn stats(&self) -> Result<StorageStats> {
        let websites = self.with_read(|conn| {
            let mut stmt = conn.prepare("SELECT count(*) FROM website")?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => Ok(row.get::<_, i64>(0)?),
                None => Ok(0),
            }
        })?;

        let (memory_bytes, memory_limit, threads) = self.with_read(|conn| {
            // `duckdb_memory()` has **one row per tag** — the buffer manager, the
            // hash tables, the string heap, … — so reading one row reads one
            // tag, not the total. The sum is the number that answers "how much
            // does the engine hold?".
            let usage: i64 = conn
                .query_row(
                    "SELECT coalesce(sum(memory_usage_bytes), 0) FROM duckdb_memory()",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);

            let limit: String = conn
                .query_row(
                    "SELECT CAST(current_setting('memory_limit') AS VARCHAR)",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or_else(|_| "unknown".to_string());
            let threads: i64 = conn
                .query_row("SELECT current_setting('threads')", [], |r| r.get(0))
                .unwrap_or(0);
            Ok((usage, limit, threads))
        })?;

        let size_bytes = std::fs::metadata(self.path()).map(|m| m.len() as i64).unwrap_or(0);

        Ok(StorageStats {
            backend: "duckdb".to_string(),
            memory_bytes,
            memory_limit,
            threads,
            size_bytes,
            websites,
        })
    }
}

// --------------------------------------------------------------------------------------
// Websites
// --------------------------------------------------------------------------------------

fn websites(conn: &Connection) -> Result<Vec<Website>> {
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
            created_at: row.get(3)?,
            updated_at: row.get(4)?,
            retention_days: row.get(5)?,
            country_header: row.get(6)?,
        });
    }
    Ok(out)
}

fn website(conn: &Connection, id: &str) -> Result<Option<Website>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, domain, created_at, updated_at, retention_days, country_header
         FROM website WHERE id = ?",
    )?;
    let mut rows = stmt.query([id])?;
    match rows.next()? {
        Some(row) => Ok(Some(Website {
            id: row.get(0)?,
            name: row.get(1)?,
            domain: row.get(2)?,
            created_at: row.get(3)?,
            updated_at: row.get(4)?,
            retention_days: row.get(5)?,
            country_header: row.get(6)?,
        })),
        None => Ok(None),
    }
}

fn create_website(
    conn: &Connection,
    name: &str,
    domain: Option<&str>,
    retention_days: i32,
    country_header: Option<&str>,
) -> Result<Website> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::db::now();
    conn.execute(
        "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        duckdb::params![id, name, domain, now, now, retention_days, country_header],
    )?;
    Ok(Website {
        id,
        name: name.to_string(),
        domain: domain.map(str::to_string),
        created_at: now,
        updated_at: now,
        retention_days,
        country_header: country_header.map(str::to_string),
    })
}

fn update_website(conn: &Connection, id: &str, patch: &WebsitePatch) -> Result<Website> {
    let existing = website(conn, id)?
        .ok_or_else(|| anyhow::anyhow!("website not found: {id}"))?;

    let name = patch
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(existing.name);
    let domain = match &patch.domain {
        Some(value) => {
            let value = value.trim().to_lowercase();
            if value.is_empty() {
                None
            } else {
                Some(value)
            }
        }
        None => existing.domain.clone(),
    };
    let retention_days = patch.retention_days.unwrap_or(existing.retention_days);
    let country_header = patch.country_header.clone().or(existing.country_header.clone());
    let now = crate::db::now();

    conn.execute(
        "UPDATE website SET name = ?, domain = ?, retention_days = ?, country_header = ?, updated_at = ?
         WHERE id = ?",
        duckdb::params![name, domain, retention_days, country_header, now, id],
    )?;

    Ok(Website {
        id: id.to_string(),
        name,
        domain,
        created_at: existing.created_at,
        updated_at: now,
        retention_days,
        country_header,
    })
}


// --------------------------------------------------------------------------------------
// Reports
//
// Everything is derived from the `event` table: one row per page view. There is no
// session table to keep in sync, and nothing is stored twice.
//
// The **statements** are the shared ones (see `crate::query`); what is here is
// DuckDB's dialect of them and how DuckDB hands the rows back.

/// Convert a timestamp into a DuckDB parameter.
fn timestamp(value: NaiveDateTime) -> Value {
    let epoch = chrono::DateTime::from_timestamp(0, 0)
        .map(|d| d.naive_utc())
        .unwrap_or_default();
    let micros = (value - epoch).num_microseconds().unwrap_or(0);
    Value::Timestamp(duckdb::types::TimeUnit::Microsecond, micros)
}

/// Build a metric with its period-over-period change.
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

/// Compute the summary metrics for a window.
fn summary(conn: &Connection, query: &ReportQuery) -> Result<SummaryMetrics> {
    // The statement is the shared one (see `crate::query`): the page views,
    // the visitors and the visits.
    let statement = crate::query::summary(query, &Duck);
    let mut stmt = conn.prepare(&statement.sql)?;
    let (pageviews, visitors, views) = match stmt.query(duckdb::params_from_iter(values(&statement)))?.next()? {
        Some(row) => (
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ),
        None => (0, 0, 0),
    };
    drop(stmt);

    // The bounces and the average session length.
    let statement = crate::query::summary_sessions(query, &Duck);
    let mut stmt = conn.prepare(&statement.sql)?;
    let (bounces, duration) = match stmt.query(duckdb::params_from_iter(values(&statement)))?.next()? {
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
    let statement = crate::query::breakdown(query, dimension, &Duck);

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(duckdb::params_from_iter(values(&statement)))?;
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
    let statement = crate::query::query_strings(query, &Duck);

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(duckdb::params_from_iter(values(&statement)))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

/// Page views over time.
fn timeseries(conn: &Connection, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
    let statement = crate::query::timeseries(query, &Duck);

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(duckdb::params_from_iter(values(&statement)))?;
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

/// Custom events, grouped by name.
fn events(conn: &Connection, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
    let statement = crate::query::events(query, &Duck);

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(duckdb::params_from_iter(values(&statement)))?;
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
    let statement = crate::query::event_log(query, &Duck);

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(duckdb::params_from_iter(values(&statement)))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(crate::row::event_log_row(&DuckRow(row))?);
    }
    Ok(out)
}

/// The most recent events of a window, newest first.
///
/// This is what the session log is built from: the log reads the last few
/// thousand rows, not the whole window, so its cost does not grow with the size
/// of the database.
fn recent_events(conn: &Connection, query: &ReportQuery, limit: i64) -> Result<Vec<SessionEvent>> {
    let statement = crate::query::recent_events(query, limit, &Duck);

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(duckdb::params_from_iter(values(&statement)))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(crate::row::session_event_row(&DuckRow(row))?);
    }
    Ok(out)
}

/// The revenue of a website over a window.
fn revenue(conn: &Connection, query: &ReportQuery) -> Result<Vec<crate::models::Revenue>> {
    let statement = crate::query::revenue(query, &Duck);

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(duckdb::params_from_iter(values(&statement)))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let row = DuckRow(row);
        out.push(crate::models::Revenue {
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

// --------------------------------------------------------------------------------------
// Writes
// --------------------------------------------------------------------------------------

/// Persist a batch of hits.
///
/// The event table is append only: every hit becomes one row and nothing is ever
/// updated. Sessions, visitors and page views are derived from it.
fn write_batch(conn: &Connection, batch: &[Hit], next_id: &AtomicI64) -> Result<()> {
    let now = crate::db::now();

    // Retention: drop hits that are older than the website's window.
    let kept: Vec<&Hit> = batch
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

    if kept.is_empty() {
        return Ok(());
    }

    // The appender does not apply column defaults, so the id is assigned here.
    // The ids come from the counter in memory: asking the database for the next
    // one (`MAX(id) + 1`) costs 0.24 ms per batch, and there is no primary key
    // on `event` to enforce uniqueness (see `db.rs`) — the single writer and the
    // counter are what keep the ids unique.
    let mut appender = conn.appender("event")?;
    for hit in kept {
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let kind = hit.kind.as_str();
        let row = crate::db::event_row(&id, hit, &kind);
        appender.append_row(&row[..])?;
    }
    appender.flush()?;
    Ok(())
}

/// Append revenue records.
fn write_revenue(conn: &Connection, records: &[crate::models::Revenue]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let next_id: i64 = conn.query_row(
        "SELECT COALESCE(MAX(id), 0) + 1 FROM revenue",
        [],
        |row| row.get(0),
    )?;

    let mut appender = conn.appender("revenue")?;
    for (index, record) in records.iter().enumerate() {
        appender.append_row(duckdb::params![
            next_id + index as i64,
            record.website_id,
            record.visit_id,
            record.event_id,
            record.event_name,
            record.currency,
            record.revenue,
            record.created_at,
        ])?;
    }
    appender.flush()?;
    Ok(())
}

/// Delete everything older than a moment.
fn delete_before(
    conn: &Connection,
    before: chrono::NaiveDateTime,
    website: Option<&str>,
) -> Result<(u64, u64)> {
    let mut events = 0u64;
    let mut revenue = 0u64;
    match website {
        Some(id) => {
            revenue += conn.execute(
                "DELETE FROM revenue WHERE website_id = ? AND created_at < ?",
                duckdb::params![id, before],
            )? as u64;
            events += conn.execute(
                "DELETE FROM event WHERE website_id = ? AND created_at < ?",
                duckdb::params![id, before],
            )? as u64;
        }
        None => {
            revenue += conn.execute(
                "DELETE FROM revenue WHERE created_at < ?",
                duckdb::params![before],
            )? as u64;
            events += conn.execute(
                "DELETE FROM event WHERE created_at < ?",
                duckdb::params![before],
            )? as u64;
        }
    }
    Ok((events, revenue))
}

/// Delete data that is older than the retention window of its website.
fn prune(conn: &Connection, website: Option<&str>) -> Result<(u64, u64)> {
    let mut events = 0u64;
    let mut revenue = 0u64;
    let now = crate::db::now();

    // Collect the (website, cutoff) pairs first: the deletion itself must not run
    // while a cursor is open on the same table.
    let mut cutoffs: Vec<(String, NaiveDateTime)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, retention_days FROM website WHERE (? IS NULL OR id = ?)",
        )?;
        let mut rows = stmt.query(duckdb::params![website, website])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let days: i32 = row.get(1)?;
            if days > 0 {
                cutoffs.push((id, now - chrono::Duration::days(days as i64)));
            }
        }
    }

    for (id, cutoff) in &cutoffs {
        revenue += conn.execute(
            "DELETE FROM revenue WHERE website_id = ? AND created_at < ?",
            duckdb::params![id, *cutoff],
        )? as u64;
        events += conn.execute(
            "DELETE FROM event WHERE website_id = ? AND created_at < ?",
            duckdb::params![id, *cutoff],
        )? as u64;
    }
    Ok((events, revenue))
}
