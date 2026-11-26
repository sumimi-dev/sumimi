//! The DuckDB backend.
//!
//! Every report and every write goes through here. The SQL is DuckDB-specific on
//! purpose — timestamps, date bucketing, distinct counting and upsert syntax all
//! differ between engines, so each backend implements the trait in its own dialect.
//!
//! The SQLite and PostgreSQL backends live in the private `sumimi-pro` repository.


use anyhow::Result;
use chrono::NaiveDateTime;
use duckdb::types::Value;
use duckdb::Connection;

use crate::models::{BreakdownRow, EventRow, Hit, MetricsSummary, MetricValue, SessionRow, Website};
use crate::pool::Db;
use crate::query::{Dialect, Granularity, Param};
use crate::storage::{
    group_sessions, session_scan_limit, Dimension, Layout, ReportQuery, SessionEvent, Storage,
    StorageStats, SummaryMetrics, WebsitePatch,
};

/// How DuckDB spells the things that differ.
///
/// Everything else — the columns, the conditions, the counts — is in the
/// shared queries (see [`crate::query`]).
///
/// **The table's name is here too**, because where the rows live is the
/// engine's business: in the shape of an instance every site has a file of
/// its own, so the table is written `s_….website_event`; in the flat shape
/// (the archive, the tools that measure things) it is written plain.
pub struct Duck {
    /// How the fact tables are reached.
    layout: Layout,
}

impl Duck {
    /// The dialect of the given shape.
    pub fn new(layout: Layout) -> Self {
        Self { layout }
    }

    /// The catalog a site's tables live in.
    fn catalog(&self, website_id: &str) -> Option<String> {
        match self.layout {
            Layout::PerSite => Some(crate::db::site_alias(website_id)),
            Layout::Flat => None,
        }
    }
}

impl Dialect for Duck {
    /// DuckDB has a uuid type and prints it in the standard form.
    fn event_id_text(&self, expr: &str) -> String {
        format!("{expr}::VARCHAR")
    }

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

    fn event_table(&self, website_id: &str) -> String {
        match self.catalog(website_id) {
            Some(catalog) => format!("{catalog}.website_event"),
            None => "website_event".to_string(),
        }
    }

    fn revenue_table(&self, website_id: &str) -> String {
        match self.catalog(website_id) {
            Some(catalog) => format!("{catalog}.revenue"),
            None => "revenue".to_string(),
        }
    }
}

/// A row of DuckDB, read by the name of its column.
pub struct DuckRow<'a>(pub &'a duckdb::Row<'a>);

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
    fn blob(&self, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get::<_, Option<Vec<u8>>>(name)?)
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
        let site = self.with_write(|conn| {
            create_website(conn, name, domain, retention_days, country_header)
        })?;
        // The site gets a file of its own — attached, and created if it is not
        // there yet, so the operator sees one file per site.
        self.ensure_site(&site.id)?;
        Ok(site)
    }

    fn update_website(&self, id: &str, patch: &WebsitePatch) -> Result<Website> {
        self.with_write(|conn| update_website(conn, id, patch))
    }

    fn delete_website(&self, id: &str) -> Result<bool> {
        // **A site that is not there is not an error** — it is nothing to do.
        // ⇒ ⇒ The question is asked *before* the facts are touched: they live
        // in the site's own file, and that file is gone with the site ⇒ ⇒ ⇒
        // a second delete would be talking about a file that is not there.
        if self.with_read(|conn| website(conn, id))?.is_none() {
            return Ok(false);
        }
        let removed = self.with_write(|conn| {
            // The fact table is the only fact table: sessions and visitors are
            // derived from it, so deleting the events deletes them too.
            let (events, revenue) = erase_site(conn, id, self.layout())?;
            let _ = (events, revenue);
            let removed = conn.execute("DELETE FROM website WHERE id = ?", [id])?;
            Ok(removed > 0)
        })?;
        // The site's file is empty now — take it away.
        self.drop_site(id)?;
        Ok(removed)
    }

    fn summary(&self, query: &ReportQuery) -> Result<SummaryMetrics> {
        self.ensure_site(&query.website_id)?;
        self.with_read(|conn| summary(conn, query, self.layout()))
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
        self.ensure_site(&query.website_id)?;
        self.with_read(|conn| breakdown(conn, query, dimension, self.layout()))
    }

    fn timeseries(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
        self.ensure_site(&query.website_id)?;
        self.with_read(|conn| timeseries(conn, query, self.layout()))
    }

    fn events(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
        self.ensure_site(&query.website_id)?;
        self.with_read(|conn| events(conn, query, self.layout()))
    }

    fn query_strings(&self, query: &ReportQuery) -> Result<Vec<(String, i64)>> {
        self.ensure_site(&query.website_id)?;
        self.with_read(|conn| query_strings(conn, query, self.layout()))
    }

    fn event_log(&self, query: &ReportQuery) -> Result<Vec<EventRow>> {
        self.ensure_site(&query.website_id)?;
        self.with_read(|conn| event_log(conn, query, self.layout()))
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
        self.ensure_site(&query.website_id)?;
        self.with_read(|conn| recent_events(conn, query, limit, self.layout()))
    }

    fn next_visit_id(&self) -> uuid::Uuid {
        self.visit_ids.next()
    }

    fn write_batch(&self, hits: &[Hit]) -> Result<()> {
        // Every site in the batch gets its file attached before the rows go
        // into it.
        for hit in hits {
            self.ensure_site(&hit.website_id)?;
        }
        self.with_write(|conn| write_batch(conn, hits, &self.event_ids, self.layout()))
    }

    fn write_revenue(&self, records: &[crate::models::Revenue]) -> Result<()> {
        for record in records {
            self.ensure_site(&record.website_id)?;
        }
        self.with_write(|conn| write_revenue(conn, records, self.layout()))
    }

    fn revenue(&self, query: &ReportQuery) -> Result<Vec<crate::models::Revenue>> {
        self.ensure_site(&query.website_id)?;
        self.with_read(|conn| revenue(conn, query, self.layout()))
    }

    fn prune(&self, website: Option<&str>) -> Result<(u64, u64)> {
        self.with_write(|conn| prune(conn, website, self.layout()))
    }

    fn delete_before(
        &self,
        before: chrono::NaiveDateTime,
        website: Option<&str>,
    ) -> Result<(u64, u64)> {
        self.with_write(|conn| delete_before(conn, before, website, self.layout()))
    }

    fn erase(&self, id: &str) -> Result<u64> {
        // A site that is not there has nothing to erase (see `delete_website`).
        //
        // ⇒ ⇒ The parameter is named `id`, not `website`: the free function
        // `website` is the one that looks a site up, and a parameter of that
        // name would shadow it.
        if self.with_read(|conn| website(conn, id))?.is_none() {
            return Ok(0);
        }
        let removed = self.with_write(|conn| {
            let (events, revenue) = erase_site(conn, id, self.layout())?;
            let websites = conn.execute("DELETE FROM website WHERE id = ?", [id])?;
            Ok(revenue + events + websites as u64)
        })?;
        self.drop_site(id)?;
        Ok(removed)
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

        // The size of the **instance**: the main file and every site's file.
        let size_bytes: i64 = crate::db::instance_files(self.path(), "duckdb")
            .iter()
            .filter_map(|file| std::fs::metadata(file).ok())
            .map(|meta| meta.len() as i64)
            .sum();

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

/// The tables of a site, as the statements name them.
fn tables(layout: Layout, website_id: &str) -> (String, String) {
    match layout {
        Layout::PerSite => {
            let alias = crate::db::site_alias(website_id);
            (format!("{alias}.website_event"), format!("{alias}.revenue"))
        }
        Layout::Flat => ("website_event".to_string(), "revenue".to_string()),
    }
}

/// Delete everything a site stored.
///
/// ⇒ ⇒ The events and the revenue are in the site's own file; the site's row
/// is in the main file and is not touched here.
fn erase_site(conn: &Connection, website_id: &str, layout: Layout) -> Result<(u64, u64)> {
    let (events, revenue) = tables(layout, website_id);
    let events = conn.execute(
        &format!("DELETE FROM {events} WHERE website_id = ?"),
        [website_id],
    )? as u64;
    let revenue = conn.execute(
        &format!("DELETE FROM {revenue} WHERE website_id = ?"),
        [website_id],
    )? as u64;
    Ok((events, revenue))
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
fn summary(
    conn: &Connection,
    query: &ReportQuery,
    layout: Layout,
) -> Result<SummaryMetrics> {
    // The statement is the shared one (see `crate::query`): the page views,
    // the visitors and the visits.
    let statement = crate::query::summary(query, &Duck::new(layout));
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
    let statement = crate::query::summary_sessions(query, &Duck::new(layout));
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
    layout: Layout,
) -> Result<Vec<BreakdownRow>> {
    let statement = crate::query::breakdown(query, dimension, &Duck::new(layout));

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
fn query_strings(
    conn: &Connection,
    query: &ReportQuery,
    layout: Layout,
) -> Result<Vec<(String, i64)>> {
    let statement = crate::query::query_strings(query, &Duck::new(layout));

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(duckdb::params_from_iter(values(&statement)))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

/// Page views over time.
fn timeseries(
    conn: &Connection,
    query: &ReportQuery,
    layout: Layout,
) -> Result<Vec<BreakdownRow>> {
    let statement = crate::query::timeseries(query, &Duck::new(layout));

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
fn events(
    conn: &Connection,
    query: &ReportQuery,
    layout: Layout,
) -> Result<Vec<BreakdownRow>> {
    let statement = crate::query::events(query, &Duck::new(layout));

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
fn event_log(
    conn: &Connection,
    query: &ReportQuery,
    layout: Layout,
) -> Result<Vec<EventRow>> {
    let statement = crate::query::event_log(query, &Duck::new(layout));

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
fn recent_events(
    conn: &Connection,
    query: &ReportQuery,
    limit: i64,
    layout: Layout,
) -> Result<Vec<SessionEvent>> {
    let statement = crate::query::recent_events(query, limit, &Duck::new(layout));

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(duckdb::params_from_iter(values(&statement)))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(crate::row::session_event_row(&DuckRow(row))?);
    }
    Ok(out)
}

/// The revenue of a website over a window.
fn revenue(
    conn: &Connection,
    query: &ReportQuery,
    layout: Layout,
) -> Result<Vec<crate::models::Revenue>> {
    let statement = crate::query::revenue(query, &Duck::new(layout));

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(duckdb::params_from_iter(values(&statement)))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let row = DuckRow(row);
        out.push(crate::models::Revenue {
            website_id: crate::row::Row::text(&row, "website_id")?.unwrap_or_default(),
            visit_id: crate::row::Row::uuid(&row, "visit_id")?,
            event_id: crate::row::Row::text(&row, "event_id")?,
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
/// The fact table is append only: every hit becomes one row and nothing is ever
/// updated. Sessions, visitors and page views are derived from it.
///
/// **The rows go into the site they belong to** — one file per site — so a
/// batch that spans two sites is written to two files.
fn write_batch(
    conn: &Connection,
    batch: &[Hit],
    event_ids: &crate::event_id::Generator,
    layout: Layout,
) -> Result<()> {
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

    // The hits, grouped by the site they belong to — in the order the sites
    // came in.
    let mut by_site: Vec<(String, Vec<&Hit>)> = Vec::new();
    for hit in kept {
        match by_site.iter_mut().find(|(id, _)| id == &hit.website_id) {
            Some((_, hits)) => hits.push(hit),
            None => by_site.push((hit.website_id.clone(), vec![hit])),
        }
    }

    for (website_id, hits) in &by_site {
        // The appender does not apply column defaults, so the id is assigned here.
        // The ids come from the generator in memory: asking the database for the next
        // one (`MAX(event_id) + 1`) costs 0.24 ms per batch, and there is no primary
        // key on the fact table to enforce uniqueness (see `db.rs`) — the generator
        // is what keeps the ids unique, and it keeps them growing.
        let mut appender = match layout {
            Layout::PerSite => conn.appender_to_catalog_and_db(
                "website_event",
                &crate::db::site_alias(website_id),
                "main",
            )?,
            Layout::Flat => conn.appender("website_event")?,
        };
        // The properties go into the table of their own.
        //
        // **The rows of one event are written next to each other** — that is
        // what makes the context columns cheap: they are the same for every
        // row of the event, so the engine's run-length code eats them.
        let mut properties = match layout {
            Layout::PerSite => Some(conn.appender_to_catalog_and_db(
                "event_data",
                &crate::db::site_alias(website_id),
                "main",
            )?),
            Layout::Flat => Some(conn.appender("event_data")?),
        };
        for hit in hits {
            let id = event_ids.next();
            let kind = hit.kind.as_str();
            // The visitor's id, in the width the column holds it: **the first
            // eight bytes of the hash** (see `crate::ua::visitor_bytes`).
            let session = crate::ua::visitor_bytes(&hit.visitor_id);
            let row = crate::db::event_row(&id, hit, &kind, &session);
            appender.append_row(&row[..])?;

            if hit.properties.is_empty() {
                continue;
            }
            let properties = properties.as_mut().expect("the appender is there");
            for property in &hit.properties {
                let (text, number, date): (Option<String>, Option<f64>, Option<chrono::NaiveDateTime>) =
                    match &property.value {
                        crate::models::PropertyValue::Text(value) => {
                            (Some(value.clone()), None, None)
                        }
                        crate::models::PropertyValue::Number(value) => (None, Some(*value), None),
                        crate::models::PropertyValue::Date(value) => (None, None, Some(*value)),
                    };
                let data_type: &'static str = property.value.data_type();
                let row: [&dyn duckdb::ToSql; 11] = [
                    &id,
                    &website_id,
                    &session,
                    &hit.url,
                    &hit.name,
                    &hit.created_at,
                    &property.key,
                    &text,
                    &number,
                    &date,
                    &data_type,
                ];
                properties.append_row(&row[..])?;
            }
        }
        appender.flush()?;
        if let Some(mut properties) = properties {
            properties.flush()?;
        }
    }
    Ok(())
}

/// Append revenue records.
///
/// **The records go into the site they belong to** — one file per site.
fn write_revenue(
    conn: &Connection,
    records: &[crate::models::Revenue],
    layout: Layout,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }

    // The records, grouped by the site they belong to.
    let mut by_site: Vec<(String, Vec<&crate::models::Revenue>)> = Vec::new();
    for record in records {
        match by_site.iter_mut().find(|(id, _)| id == &record.website_id) {
            Some((_, group)) => group.push(record),
            None => by_site.push((record.website_id.clone(), vec![record])),
        }
    }

    for (website_id, records) in &by_site {
        let (events_table, revenue_table) = tables(layout, website_id);
        let _ = events_table;
        let next_id: i64 = conn.query_row(
            &format!("SELECT COALESCE(MAX(id), 0) + 1 FROM {revenue_table}"),
            [],
            |row| row.get(0),
        )?;

        let mut appender = match layout {
            Layout::PerSite => conn.appender_to_catalog_and_db(
                "revenue",
                &crate::db::site_alias(website_id),
                "main",
            )?,
            Layout::Flat => conn.appender("revenue")?,
        };
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
    }
    Ok(())
}

/// Delete everything older than a moment.
fn delete_before(
    conn: &Connection,
    before: chrono::NaiveDateTime,
    website: Option<&str>,
    layout: Layout,
) -> Result<(u64, u64)> {
    let mut events = 0u64;
    let mut revenue = 0u64;

    // The sites to touch: one, or all of them.
    let sites: Vec<String> = match website {
        Some(id) => vec![id.to_string()],
        None => crate::db::site_ids(conn)?,
    };

    for id in &sites {
        let (events_table, revenue_table) = tables(layout, id);
        revenue += conn.execute(
            &format!("DELETE FROM {revenue_table} WHERE website_id = ? AND created_at < ?"),
            duckdb::params![id, before],
        )? as u64;
        events += conn.execute(
            &format!("DELETE FROM {events_table} WHERE website_id = ? AND created_at < ?"),
            duckdb::params![id, before],
        )? as u64;
    }
    Ok((events, revenue))
}

/// Delete data that is older than the retention window of its website.
fn prune(conn: &Connection, website: Option<&str>, layout: Layout) -> Result<(u64, u64)> {
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
        let (events_table, revenue_table) = tables(layout, id);
        revenue += conn.execute(
            &format!("DELETE FROM {revenue_table} WHERE website_id = ? AND created_at < ?"),
            duckdb::params![id, *cutoff],
        )? as u64;
        events += conn.execute(
            &format!("DELETE FROM {events_table} WHERE website_id = ? AND created_at < ?"),
            duckdb::params![id, *cutoff],
        )? as u64;
    }
    Ok((events, revenue))
}
