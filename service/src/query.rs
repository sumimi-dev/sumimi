//! The queries — **one** set of statements, three dialects.
//!
//! The reports are the 口径: the numbers the site owner sees. They are written
//! down **once**, here, and each engine runs them in its own dialect.
//!
//! **Why this is not a copy.** A statement that exists in two places is a
//! statement that answers two questions: the day the 口径 changes, one of them
//! is changed and the other is not, and the two engines disagree about the
//! numbers — with nothing to say so. That is not a theory: the PostgreSQL
//! backend (in the private repository) kept the visitor count of v9 and the
//! session-length anchor of v8 long after the other two engines had moved to
//! v10, and its report of the page views over time was reading microseconds
//! as if they were seconds.
//!
//! ⇒ ⇒ So the statement is here, and what an engine has to say is **how it
//! spells things**: the placeholder, the moment, the larger of two values, the
//! bucket. Everything else — the columns, the conditions, the counts — is
//! written once, and a change to the 口径 is a change to one file.

use chrono::NaiveDateTime;
use uuid::Uuid;

use crate::storage::{Dimension, ReportQuery, PAGEVIEWS};

/// A parameter of a statement, in the shape every engine understands.
///
/// The engine turns it into its own value: DuckDB wants a timestamp, SQLite
/// and PostgreSQL want microseconds since the epoch.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    /// Text.
    Text(String),
    /// A whole number.
    Int(i64),
    /// A number with a fraction.
    Float(f64),
    /// A sixteen-byte id.
    Uuid(Uuid),
    /// A moment in time.
    Time(NaiveDateTime),
    /// Nothing at all.
    Null,
}

/// How an engine spells the things that differ.
///
/// The list is short **on purpose**: it is the whole of what the engines do
/// not agree on. Everything else — the columns, the conditions, the counts —
/// is in the statements below, and is the same for everybody.
pub trait Dialect {
    /// The n-th placeholder: `?`, `?1`, `$1`.
    fn placeholder(&self, n: usize) -> String;

    /// A moment, in microseconds since the epoch.
    ///
    /// DuckDB keeps the moments as timestamps, so it has to count the
    /// microseconds; the other two keep the microseconds already.
    fn epoch_us(&self, expr: &str) -> String;

    /// The larger of two values.
    fn greatest(&self, a: &str, b: &str) -> String;

    /// A number with a fraction.
    fn cast_double(&self, expr: &str) -> String;

    /// The bucket a moment falls into.
    fn bucket(&self, granularity: Granularity, expr: &str) -> String;

    /// The **fact table**, as this engine names it.
    ///
    /// **The name is the engine's business; the columns are not.** The
    /// engines that keep one file per site put the site's file in front of
    /// the table (`s_1a2b….website_event`), the ones that keep everything in
    /// one database do not (`event`).
    ///
    /// ⇒ ⇒ The statement above says `FROM {table}` and the dialect fills it
    /// in ⇒ ⇒ ⇒ the **口径** does not know where the rows live ⇒ ⇒ ⇒ the
    /// shape of the storage is free to change without touching a single
    /// number.
    fn event_table(&self, website_id: &str) -> String;

    /// The **revenue table**, as this engine names it.
    fn revenue_table(&self, website_id: &str) -> String;
}

/// How fine the buckets of a time series are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// One hour.
    Hour,
    /// One day.
    Day,
}

impl Granularity {
    /// The name the engines spell it with.
    pub fn name(self) -> &'static str {
        match self {
            Granularity::Hour => "hour",
            Granularity::Day => "day",
        }
    }
}

/// The conditions of a report — and the values that go with them.
///
/// The clause and the values are built together, so a backend cannot ask for
/// one and forget the other.
pub struct Filters {
    clauses: Vec<String>,
    values: Vec<Param>,
}

impl Filters {
    /// The filters of a **report**: the marked traffic is left out.
    ///
    /// The bots, the developer's own traffic, and whatever else the site owner
    /// tagged are not part of the numbers — the dashboard can ask for them.
    pub fn new(query: &ReportQuery, dialect: &dyn Dialect) -> Self {
        let mut filters = Filters::logs(query, dialect);
        // The marked traffic — the bots, the developer's own traffic — is not
        // part of the numbers, unless the dashboard asks for its kinds.
        let (clause, values) =
            not_marked(&query.include_markers, dialect, filters.values.len());
        filters.clauses.push(clause);
        filters.values.extend(values);
        filters
    }

    /// The filters of a **log**: everything is there.
    ///
    /// The session list and the event list are logs — they show what happened,
    /// tagged or not. The reports about the tags are logs too: they are the
    /// report *about* the tagged traffic.
    pub fn logs(query: &ReportQuery, dialect: &dyn Dialect) -> Self {
        let mut filters = Filters {
            clauses: vec![format!("website_id = {}", dialect.placeholder(1))],
            values: vec![Param::Text(query.website_id.clone())],
        };
        filters.push_time("created_at >= ", query.range.start, dialect);
        filters.push_time("created_at < ", query.range.end, dialect);

        for (column, value) in query.dimensions.filters() {
            if let Some(value) = value.as_ref().filter(|v| !v.is_empty()) {
                filters.push_text(column, value, dialect);
            }
        }

        filters
    }

    /// Add a condition on a moment.
    fn push_time(&mut self, column: &str, value: NaiveDateTime, dialect: &dyn Dialect) {
        self.clauses.push(format!(
            "{column}{}",
            dialect.placeholder(self.values.len() + 1)
        ));
        self.values.push(Param::Time(value));
    }

    /// Add a condition on a text.
    fn push_text(&mut self, column: &str, value: &str, dialect: &dyn Dialect) {
        self.clauses.push(format!(
            "{column} = {}",
            dialect.placeholder(self.values.len() + 1)
        ));
        self.values.push(Param::Text(value.to_string()));
    }

    /// The SQL condition.
    pub fn clause(&self) -> String {
        self.clauses.join(" AND ")
    }

    /// The parameters, in order.
    pub fn values(&self) -> &[Param] {
        &self.values
    }
}

/// The condition that leaves the **marked** traffic out of the numbers, and
/// the values that go with it.
///
/// `included` is the kinds the dashboard asked for — a group stands for the
/// kinds in it, so picking a group picks all of them. `start` is how many
/// values are already in the statement: the placeholders are numbered from
/// there, so the whole statement is numbered once.
fn not_marked(included: &[String], dialect: &dyn Dialect, start: usize) -> (String, Vec<Param>) {
    let all = crate::storage::marker_tags();
    let included = crate::storage::expand_markers(included);

    let mut clause = String::from("tag IS NULL");
    let mut values: Vec<Param> = Vec::new();

    if !all.is_empty() {
        clause.push_str(" OR (");
        for (index, _) in all.iter().enumerate() {
            if index > 0 {
                clause.push_str(" AND ");
            }
            clause.push_str(&format!(
                "tag <> {}",
                dialect.placeholder(start + values.len() + 1)
            ));
            values.push(Param::Text((*all[index]).to_string()));
        }
        clause.push(')');
    }

    if !included.is_empty() {
        clause.push_str(" OR tag IN (");
        for (index, _) in included.iter().enumerate() {
            if index > 0 {
                clause.push_str(", ");
            }
            clause.push_str(&dialect.placeholder(start + values.len() + 1));
            values.push(Param::Text(included[index].clone()));
        }
        clause.push(')');
    }

    // **The parentheses are not decoration.** The clause is joined to the
    // others with `AND`, and `AND` binds tighter than `OR` — so an unbracketed
    // `tag IS NULL OR (...)` turns the whole condition into
    // `(... AND tag IS NULL) OR (...)`: the second half has no website and no
    // time in it, and every unmarked row of the database gets counted.
    //
    // ⇒ ⇒ The clause is bracketed, so it is one condition among the others.
    (format!("({clause})"), values)
}

/// A statement and the values that go with it.
pub struct Statement {
    /// The SQL.
    pub sql: String,
    /// The values, in the order the placeholders ask for them.
    pub values: Vec<Param>,
}

impl Statement {
    /// A statement with no values of its own.
    fn of(sql: String, values: Vec<Param>) -> Self {
        Self { sql, values }
    }
}

/// The summary of a window: the page views, the visitors and the visits.
///
/// **The rule is Umami's** (since v12): the visitors and the visits are
/// counted over the groups that have **at least one page view** — so a
/// visitor whose only event was a custom event is not a visitor, and the
/// visit it was in is not a visit.
///
/// ⇒ ⇒ That is what the reference implementation's statement does (its
/// `having c > 0`), and it is the same rule spelled differently: **a group
/// with a page view is a group with a page-view row** ⇒ ⇒ ⇒ so counting the
/// distinct ids over the page-view rows counts exactly the same ids ⇒ ⇒ ⇒
/// and the two datasets agree on every number.
///
/// **The two ids are the ones Umami counts.** The visitors are the distinct
/// `session_id` (our visitor — Umami's name for the same thing) and the
/// visits are the distinct `visit_id` (our session). The names are the
/// reference implementation's, so the two statements are the same statement.
pub fn summary(query: &ReportQuery, dialect: &dyn Dialect) -> Statement {
    let filters = Filters::new(query, dialect);
    let sql = format!(
        "SELECT COUNT(*) FILTER (WHERE {pageviews}),
                COUNT(DISTINCT CASE WHEN {pageviews} THEN session_id END),
                COUNT(DISTINCT CASE WHEN {pageviews} THEN visit_id END)
         FROM {table} WHERE {filters}",
        pageviews = PAGEVIEWS,
        table = dialect.event_table(&query.website_id),
        filters = filters.clause(),
    );
    Statement::of(sql, filters.values().to_vec())
}

/// The bounces and the average session length of a window.
///
/// A bounce is a session with a single **page view** that did not fire a
/// custom event — the same rule the reference implementation uses.
///
/// The **anchor** of the length is the moment the session started, and it is
/// the `session_start` the row carries — so it is the *algorithmic* start,
/// not the first event the report happened to see. A session the client named
/// has no start (there is no moment to remember), so those fall back to the
/// span between the first and the last page view of the group.
pub fn summary_sessions(query: &ReportQuery, dialect: &dyn Dialect) -> Statement {
    let filters = Filters::new(query, dialect);
    // The span of the session: from its start to its last page view.
    let last = dialect.epoch_us(&format!(
        "MAX(created_at) FILTER (WHERE {PAGEVIEWS})"
    ));
    let first = dialect.epoch_us(&format!(
        "MIN(created_at) FILTER (WHERE {PAGEVIEWS})"
    ));
    let start = dialect.epoch_us("MIN(session_start)");
    let span = format!(
        "{last} - CASE WHEN MIN(session_start) IS NULL THEN {first} ELSE {start} END"
    );
    let sql = format!(
        "SELECT COUNT(*) FILTER (WHERE n = 1 AND custom = 0),
                {average}
         FROM (
             SELECT visit_id,
                    COUNT(*) FILTER (WHERE {pageviews}) AS n,
                    MAX(CASE WHEN event_type = 'event' THEN 1 ELSE 0 END) AS custom,
                    CASE
                        WHEN MAX(created_at) FILTER (WHERE {pageviews}) IS NULL THEN NULL
                        ELSE {greatest} / 1000000.0
                    END AS d
             FROM {table} WHERE {filters}
             GROUP BY visit_id
         )",
        average = dialect.cast_double("COALESCE(AVG(d), 0)"),
        greatest = dialect.greatest(&span, "0"),
        pageviews = PAGEVIEWS,
        table = dialect.event_table(&query.website_id),
        filters = filters.clause(),
    );
    Statement::of(sql, filters.values().to_vec())
}

/// The breakdown of one dimension.
///
/// Content dimensions count views, audience dimensions count visitors.
pub fn breakdown(
    query: &ReportQuery,
    dimension: Dimension,
    dialect: &dyn Dialect,
) -> Statement {
    // The breakdown of the **tags** is the report *about* the tagged traffic:
    // it has to see it.
    let filters = if dimension == Dimension::Tag {
        Filters::logs(query, dialect)
    } else {
        Filters::new(query, dialect)
    };

    // The visitors are counted by their **id** — the hash of the address and
    // the agent. It is stored, so the count does not depend on the raw
    // address being stored too (it is not, by default). The column is
    // `session_id` (Umami's name for the visitor).
    let aggregate = match dimension.aggregate() {
        "views" => "COUNT(*)",
        _ => "COUNT(DISTINCT session_id)",
    };

    let sql = format!(
        "SELECT COALESCE({column}, 'unknown') AS name, {aggregate} AS value
         FROM {table} WHERE {where_clause} AND {pageviews}
         GROUP BY name
         ORDER BY value DESC, name ASC
         LIMIT {limit}",
        column = dimension.column(),
        aggregate = aggregate,
        table = dialect.event_table(&query.website_id),
        where_clause = filters.clause(),
        pageviews = PAGEVIEWS,
        limit = query.limit.clamp(1, 1000),
    );
    Statement::of(sql, filters.values().to_vec())
}

/// The page views over time.
pub fn timeseries(query: &ReportQuery, dialect: &dyn Dialect) -> Statement {
    let filters = Filters::new(query, dialect);

    // Bucket by hour for short windows, by day for long ones.
    let granularity = if (query.range.end - query.range.start).num_seconds() <= 2 * 24 * 3600 {
        Granularity::Hour
    } else {
        Granularity::Day
    };

    let sql = format!(
        "SELECT {bucket} AS bucket, COUNT(*) AS value
         FROM {table} WHERE {where_clause} AND {pageviews}
         GROUP BY bucket
         ORDER BY bucket ASC",
        bucket = dialect.bucket(granularity, "created_at"),
        table = dialect.event_table(&query.website_id),
        where_clause = filters.clause(),
        pageviews = PAGEVIEWS,
    );
    Statement::of(sql, filters.values().to_vec())
}

/// The custom events, grouped by name.
pub fn events(query: &ReportQuery, dialect: &dyn Dialect) -> Statement {
    let filters = Filters::new(query, dialect);
    let sql = format!(
        "SELECT COALESCE(event_name, 'unknown') AS name, COUNT(*) AS value
         FROM {table}
         WHERE {where_clause} AND event_type = 'event'
         GROUP BY name
         ORDER BY value DESC, name ASC
         LIMIT {limit}",
        table = dialect.event_table(&query.website_id),
        where_clause = filters.clause(),
        limit = query.limit.clamp(1, 1000),
    );
    Statement::of(sql, filters.values().to_vec())
}

/// The query strings of the page views.
pub fn query_strings(query: &ReportQuery, dialect: &dyn Dialect) -> Statement {
    let filters = Filters::new(query, dialect);
    let sql = format!(
        "SELECT url_query, COUNT(*) AS value FROM {table}
         WHERE {where_clause} AND {pageviews} AND url_query IS NOT NULL AND url_query <> ''
         GROUP BY url_query ORDER BY value DESC LIMIT 1000",
        table = dialect.event_table(&query.website_id),
        where_clause = filters.clause(),
        pageviews = PAGEVIEWS,
    );
    Statement::of(sql, filters.values().to_vec())
}

/// The raw event log.
pub fn event_log(query: &ReportQuery, dialect: &dyn Dialect) -> Statement {
    let filters = Filters::logs(query, dialect);
    let sql = format!(
        "SELECT event_id, website_id, visit_id, session_id, created_at, event_name, url_path, hostname, channel, data,
                referrer_domain, referrer_path, referrer_query, country, city, browser, os,
                utm_source, utm_medium, utm_campaign, lcp, inp, cls, device
         FROM {table} WHERE {where_clause}
         ORDER BY created_at DESC, event_id DESC
         LIMIT {limit} OFFSET {offset}",
        table = dialect.event_table(&query.website_id),
        where_clause = filters.clause(),
        limit = query.limit.clamp(1, 1000),
        offset = query.offset.max(0),
    );
    Statement::of(sql, filters.values().to_vec())
}

/// The most recent events of a window, newest first.
///
/// This is what the session log is built from: the log reads the last few
/// thousand rows, not the whole window, so its cost does not grow with the
/// size of the database.
pub fn recent_events(query: &ReportQuery, limit: i64, dialect: &dyn Dialect) -> Statement {
    let filters = Filters::logs(query, dialect);
    let sql = format!(
        "SELECT event_id, visit_id, session_start, created_at, url_path, page_title, referrer_domain, referrer_path,
                hostname, channel, browser, os, device, country, region, city, language, screen,
                ip_prefix, user_agent
         FROM {table} WHERE {where_clause}
         ORDER BY created_at DESC, event_id DESC
         LIMIT {limit}",
        table = dialect.event_table(&query.website_id),
        where_clause = filters.clause(),
        limit = limit.clamp(1, 1_000_000),
    );
    Statement::of(sql, filters.values().to_vec())
}

/// The revenue of a window.
///
/// The revenue is written by the site owner, not by the tracker — so it has no
/// marker, and the `revenue` table has no `tag` column to leave the marked
/// traffic out by. ⇒ ⇒ The filters of a **log**, then.
pub fn revenue(query: &ReportQuery, dialect: &dyn Dialect) -> Statement {
    let filters = Filters::logs(query, dialect);
    let sql = format!(
        "SELECT id, website_id, visit_id, event_id, event_name, currency, revenue, created_at
         FROM {table} WHERE {where_clause}
         ORDER BY created_at DESC, id DESC
         LIMIT {limit} OFFSET {offset}",
        table = dialect.revenue_table(&query.website_id),
        where_clause = filters.clause(),
        limit = query.limit.clamp(1, 1000),
        offset = query.offset.max(0),
    );
    Statement::of(sql, filters.values().to_vec())
}


/// The raw rows of a website — the deep read.
///
/// The field-level check reads the rows as they are stored, so the columns it
/// asks for are the shared list (see [`crate::row::EVENT_COLUMNS`]).
pub fn event_rows(website_id: &str, dialect: &dyn Dialect) -> Statement {
    let sql = format!(
        "SELECT {columns} FROM {table} WHERE website_id = {website} ORDER BY event_id ASC",
        columns = crate::row::EVENT_COLUMNS,
        table = dialect.event_table(website_id),
        website = dialect.placeholder(1),
    );
    Statement::of(sql, vec![Param::Text(website_id.to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::TimeRange;

    /// A dialect that numbers its placeholders — the one PostgreSQL uses.
    struct Numbered;

    impl Dialect for Numbered {
        fn placeholder(&self, n: usize) -> String {
            format!("${n}")
        }
        fn epoch_us(&self, expr: &str) -> String {
            expr.to_string()
        }
        fn greatest(&self, a: &str, b: &str) -> String {
            format!("GREATEST({a}, {b})")
        }
        fn cast_double(&self, expr: &str) -> String {
            format!("CAST({expr} AS DOUBLE PRECISION)")
        }
        fn bucket(&self, granularity: Granularity, expr: &str) -> String {
            format!("bucket({granularity:?}, {expr})")
        }
        fn event_table(&self, _website_id: &str) -> String {
            "event".to_string()
        }
        fn revenue_table(&self, _website_id: &str) -> String {
            "revenue".to_string()
        }
    }

    #[test]
    fn the_placeholders_are_numbered_in_order() {
        let mut query = ReportQuery::new("site-1", TimeRange::last_days(7));
        query.dimensions.url = Some("/pricing".to_string());
        query.dimensions.browser = Some("Chrome".to_string());

        let filters = Filters::logs(&query, &Numbered);
        assert_eq!(
            filters.clause(),
            "website_id = $1 AND created_at >= $2 AND created_at < $3 AND url_path = $4 AND browser = $5"
        );
        assert_eq!(filters.values().len(), 5);
    }

    #[test]
    fn an_empty_dimension_is_not_a_condition() {
        let mut query = ReportQuery::new("site-1", TimeRange::last_days(7));
        query.dimensions.url = Some(String::new());
        query.dimensions.referrer = Some("https://example.com".to_string());

        let filters = Filters::logs(&query, &Numbered);
        assert_eq!(
            filters.clause(),
            "website_id = $1 AND created_at >= $2 AND created_at < $3 AND referrer_domain = $4"
        );
        assert_eq!(filters.values().len(), 4);
    }

    #[test]
    fn the_placeholders_stay_contiguous() {
        let query = ReportQuery::new("site-1", TimeRange::last_days(7));
        let filters = Filters::logs(&query, &Numbered);
        assert_eq!(
            filters.clause(),
            "website_id = $1 AND created_at >= $2 AND created_at < $3"
        );
        assert_eq!(filters.values().len(), 3);
    }

    #[test]
    fn the_marked_traffic_is_numbered_after_the_rest() {
        let query = ReportQuery::new("site-1", TimeRange::last_days(7));
        let filters = Filters::new(&query, &Numbered);
        // The website, the two moments, and then one placeholder per marker.
        let markers = crate::storage::marker_tags().len();
        assert_eq!(filters.values().len(), 3 + markers);
        assert!(filters.clause().contains(&format!("tag <> ${}", 4)));
    }
}
