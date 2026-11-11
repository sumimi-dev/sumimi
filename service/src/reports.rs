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
use crate::storage::{
    group_sessions, session_scan_limit, Dimension, ReportQuery, SessionEvent, Storage, StorageStats,
    SummaryMetrics, WebsitePatch, PAGEVIEWS,
};

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
// --------------------------------------------------------------------------------------

/// A WHERE clause and its parameters.
struct Filters {
    clauses: Vec<String>,
    values: Vec<Value>,
}

impl Filters {
    /// Start from a website and a time window.
    fn new(query: &ReportQuery) -> Self {
        let mut filters = Filters {
            clauses: vec!["website_id = ?".to_string()],
            values: vec![Value::Text(query.website_id.clone())],
        };
        filters.push_value("created_at >= ?", timestamp(query.range.start));
        filters.push_value("created_at < ?", timestamp(query.range.end));

        for (column, value) in query.dimensions.filters() {
            if let Some(value) = value.as_ref().filter(|v| !v.is_empty()) {
                filters.push_text(&format!("{column} = ?"), value);
            }
        }

        filters
    }

    /// Add a parameterised condition.
    fn push_value(&mut self, clause: &str, value: Value) {
        self.clauses.push(clause.to_string());
        self.values.push(value);
    }

    /// Add a text parameterised condition.
    fn push_text(&mut self, clause: &str, value: &str) {
        self.push_value(clause, Value::Text(value.to_string()));
    }

    /// The SQL condition.
    fn clause(&self) -> String {
        self.clauses.join(" AND ")
    }

    /// The parameters, in order.
    fn params(&self) -> Vec<&dyn duckdb::types::ToSql> {
        self.values
            .iter()
            .map(|v| v as &dyn duckdb::types::ToSql)
            .collect()
    }
}

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
    let filters = Filters::new(query);

    // The page views are counted without the custom events; the visitors and
    // the visits are counted over everything — a visitor who only fired a
    // custom event is still a visitor.
    let sql = format!(
        "SELECT COUNT(*) FILTER (WHERE {pageviews}),
                COUNT(DISTINCT visitor_id),
                COUNT(DISTINCT visit_id)
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
             SELECT visit_id,
                    COUNT(*) FILTER (WHERE {pageviews}) AS n,
                    MAX(CASE WHEN event_type = 'event' THEN 1 ELSE 0 END) AS custom,
                    epoch(MAX(created_at) FILTER (WHERE {pageviews})
                        - MIN(created_at) FILTER (WHERE {pageviews})) AS d
             FROM event WHERE {filters}
             GROUP BY visit_id
         )",
        pageviews = PAGEVIEWS,
        filters = filters.clause()
    );
    let mut stmt = conn.prepare(&sql)?;
    let (bounces, duration) = match stmt.query(filters.params().as_slice())?.next()? {
        Some(row) => (row.get::<_, i64>(0)?, row.get::<_, f64>(1)?),
        None => (0, 0.0),
    };

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

/// Page views over time.
fn timeseries(conn: &Connection, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
    let filters = Filters::new(query);

    // Bucket by hour for short windows, by day for long ones.
    let (truncate, format) =
        if (query.range.end - query.range.start).num_seconds() <= 2 * 24 * 3600 {
            ("hour", "%Y-%m-%dT%H:00:00Z")
        } else {
            ("day", "%Y-%m-%dT00:00:00Z")
        };

    let sql = format!(
        "SELECT strftime(date_trunc('{truncate}', created_at), '{format}') AS bucket, COUNT(*) AS value
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

/// Custom events, grouped by name.
fn events(conn: &Connection, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
    let filters = Filters::new(query);
    let sql = format!(
        "SELECT COALESCE(event_name, 'unknown') AS name, COUNT(*) AS value
         FROM event
         WHERE {where_clause} AND event_type = 'event'
         GROUP BY name
         ORDER BY value DESC, name ASC
         LIMIT {limit}",
        where_clause = filters.clause(),
        limit = query.limit.clamp(1, 1000),
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
        "SELECT id, website_id, visit_id, created_at, event_name, url, hostname, channel, data,
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
            session_id: row.get::<_, Option<uuid::Uuid>>(2)?.map(|id| id.to_string()),
            created_at: row.get(3)?,
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
/// This is what the session log is built from: the log reads the last few
/// thousand rows, not the whole window, so its cost does not grow with the size
/// of the database.
fn recent_events(conn: &Connection, query: &ReportQuery, limit: i64) -> Result<Vec<SessionEvent>> {
    let filters = Filters::new(query);
    let sql = format!(
        "SELECT id, visit_id, visitor_id, created_at, url, title, referrer, referrer_path,
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
            visit_id: row.get(1)?,
            visitor_id: row.get::<_, Option<uuid::Uuid>>(2)?,
            created_at: row.get(3)?,
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
fn revenue(conn: &Connection, query: &ReportQuery) -> Result<Vec<crate::models::Revenue>> {
    let filters = Filters::new(query);
    let sql = format!(
        "SELECT website_id, visit_id, event_id, event_name, currency, revenue, created_at
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
        out.push(crate::models::Revenue {
            website_id: row.get(0)?,
            visit_id: row.get::<_, Option<uuid::Uuid>>(1)?,
            event_id: row.get(2)?,
            event_name: row.get(3)?,
            currency: row.get(4)?,
            revenue: row.get(5)?,
            created_at: row.get(6)?,
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
