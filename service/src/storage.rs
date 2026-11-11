//! The storage abstraction.
//!
//! Sumimi's reports are defined once and implemented once **per backend**. This module
//! holds the trait and the query types that every backend speaks; the SQL itself lives
//! in the backend, because SQL is not portable across engines (timestamps, date
//! bucketing, distinct counting and upsert syntax all differ).
//!
//! The DuckDB backend is in [`crate::reports`]. The SQLite and PostgreSQL backends
//! live in the private `sumimi-pro` repository, under `pro/storage/`.
//!
//! A backend must produce **exactly** the same numbers as the others — that invariant
//! is what the cross-backend tests assert.

use anyhow::Result;
use chrono::NaiveDateTime;
use uuid::Uuid;

use crate::models::{
    BreakdownRow, EventRow, MetricsSummary, SessionRow, Website,
};

/// A time window (start inclusive, end exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    /// Start of the window.
    pub start: NaiveDateTime,
    /// End of the window.
    pub end: NaiveDateTime,
}

impl TimeRange {
    /// The window immediately before this one, of the same length.
    pub fn previous(&self) -> TimeRange {
        let span = self.end - self.start;
        TimeRange {
            start: self.start - span,
            end: self.start,
        }
    }

    /// The default window: the last 7 days.
    pub fn last_days(days: i64) -> TimeRange {
        let end = crate::db::now();
        TimeRange {
            start: end - chrono::Duration::days(days),
            end,
        }
    }
}

/// The dimension filters (Umami-compatible query parameters).
#[derive(Debug, Clone, Default)]
pub struct Dimensions {
    /// Only this page.
    pub url: Option<String>,
    /// Only this referrer.
    pub referrer: Option<String>,
    /// Only this browser.
    pub browser: Option<String>,
    /// Only this operating system.
    pub os: Option<String>,
    /// Only this device class.
    pub device: Option<String>,
    /// Only this country.
    pub country: Option<String>,
}

impl Dimensions {
    /// The filters a report applies: every dimension and its value.
    ///
    /// The list lives here so that the three backends filter on **the same
    /// dimensions** — a filter one engine knows and another does not is a
    /// number that differs between them.
    pub fn filters(&self) -> [(&'static str, &Option<String>); 6] {
        [
            ("url", &self.url),
            ("referrer", &self.referrer),
            ("browser", &self.browser),
            ("os", &self.os),
            ("device", &self.device),
            ("country", &self.country),
        ]
    }
}

/// Everything a report needs.
#[derive(Debug, Clone)]
pub struct ReportQuery {
    /// The website the report is about.
    pub website_id: String,
    /// The time window.
    pub range: TimeRange,
    /// The dimension filters.
    pub dimensions: Dimensions,
    /// How many rows to return.
    pub limit: i64,
    /// How many rows to skip.
    pub offset: i64,
}

impl ReportQuery {
    /// A query for one website and one window.
    pub fn new(website_id: &str, range: TimeRange) -> Self {
        Self {
            website_id: website_id.to_string(),
            range,
            dimensions: Dimensions::default(),
            limit: 50,
            offset: 0,
        }
    }
}

/// The condition that selects the **page views**.
///
/// A custom event is not a page view, and neither is an identify call. The
/// reference implementation excludes them from every pageview metric
/// (`event_type NOT IN (2, 5)`), and so do we — otherwise a site with a lot
/// of custom events would show them as pages, and its "bounce rate" would be
/// wrong (a session that only fired a custom event is not a bounce, but it
/// would look like one).
///
/// The **logs** — the session list and the event list — keep everything: they
/// are logs.
///
/// The three backends speak the same SQL here (`'…'` quotes work in all of
/// them), so the rule lives in one place: a metric that means "page views"
/// means the same thing in all of them.
pub const PAGEVIEWS: &str = "event_type NOT IN ('event', 'identify')";

/// The dimensions a breakdown can be computed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    /// The page path.
    Url,
    /// The referring page.
    Referrer,
    /// The browser family.
    Browser,
    /// The operating system family.
    Os,
    /// The device class.
    Device,
    /// The country.
    Country,
    /// The interface language.
    Language,
    /// The viewport size.
    Screen,
    /// The traffic channel: direct, Search, Social, email, paidAds, …
    Channel,
    /// The region of the visitor.
    Region,
    /// The city of the visitor.
    City,
    /// The hostname of the site the visit was on.
    Host,
    /// The tag of a custom event.
    Tag,
}

impl Dimension {
    /// The column in the `session` table.
    pub fn column(self) -> &'static str {
        match self {
            Dimension::Url => "url",
            Dimension::Referrer => "referrer",
            Dimension::Browser => "browser",
            Dimension::Os => "os",
            Dimension::Device => "device",
            Dimension::Country => "country",
            Dimension::Language => "language",
            Dimension::Screen => "screen",
            Dimension::Channel => "channel",
            Dimension::Region => "region",
            Dimension::City => "city",
            Dimension::Host => "hostname",
            Dimension::Tag => "tag",
        }
    }

    /// How the rows of a breakdown are counted.
    ///
    /// Content dimensions (pages, referrers, hosts, tags) count views; audience
    /// dimensions (browser, OS, device, country, region, city, language, screen,
    /// channel) count unique visitors.
    pub fn aggregate(self) -> &'static str {
        match self {
            Dimension::Url | Dimension::Referrer | Dimension::Host | Dimension::Tag => "views",
            _ => "visitors",
        }
    }

    /// Every dimension, in the order the API exposes them.
    pub fn all() -> [Dimension; 13] {
        [
            Dimension::Url,
            Dimension::Referrer,
            Dimension::Browser,
            Dimension::Os,
            Dimension::Device,
            Dimension::Country,
            Dimension::Language,
            Dimension::Screen,
            Dimension::Channel,
            Dimension::Region,
            Dimension::City,
            Dimension::Host,
            Dimension::Tag,
        ]
    }
}

/// The raw metric values for a window.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct SummaryMetrics {
    /// Total page views.
    pub pageviews: i64,
    /// Unique visitors (daily-rotating id).
    pub visitors: i64,
    /// Number of sessions.
    pub views: i64,
    /// Sessions with a single page view.
    pub bounces: i64,
    /// Average session duration in seconds.
    pub visit_duration: f64,
}

/// What a backend reports about itself.
#[derive(Debug, Clone, Default)]
pub struct StorageStats {
    /// The backend's name (`duckdb`, `sqlite`, `postgres`).
    pub backend: String,
    /// How much memory the engine currently holds, in bytes.
    pub memory_bytes: i64,
    /// The engine's memory ceiling, as reported by the engine itself.
    pub memory_limit: String,
    /// How many threads the engine may use.
    pub threads: i64,
    /// The size of the database on disk, in bytes.
    pub size_bytes: i64,
    /// How many websites are stored.
    pub websites: i64,
}

/// One query parameter of the visited pages.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryParameterRow {
    /// The name of the parameter (`utm_source`).
    pub name: String,
    /// Its value (`twitter`).
    pub value: String,
    /// How often the pair was seen.
    pub count: i64,
}

/// The query parameters of a set of query strings.
///
/// The parameters are split **here**, not in the database: the three engines
/// do not agree on how to split a query string, and the answer has to be the
/// same in all of them.
pub fn parse_query_parameters(rows: &[(String, i64)]) -> Vec<QueryParameterRow> {
    let mut out: Vec<QueryParameterRow> = Vec::new();

    for (query, count) in rows {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (name, value) = match pair.split_once('=') {
                Some((name, value)) => (name, value),
                None => (pair, ""),
            };
            if name.is_empty() {
                continue;
            }

            match out
                .iter_mut()
                .find(|row| row.name == name && row.value == value)
            {
                Some(row) => row.count += count,
                None => out.push(QueryParameterRow {
                    name: name.to_string(),
                    value: value.to_string(),
                    count: *count,
                }),
            }
        }
    }

    out.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
    out
}

/// A change to a website.
#[derive(Debug, Clone, Default)]
pub struct WebsitePatch {
    /// A new name.
    pub name: Option<String>,
    /// A new domain.
    pub domain: Option<String>,
    /// A new retention window, in days.
    pub retention_days: Option<i32>,
    /// A new country header.
    pub country_header: Option<String>,
}
/// One event of the session log: the fields a session row is built from.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    /// The row id — the tiebreaker when two events share a timestamp.
    pub id: i64,
    /// The visit the event belongs to.
    pub visit_id: Uuid,
    /// The rotating visitor id.
    pub visitor_id: Option<Uuid>,
    /// When it happened.
    pub created_at: NaiveDateTime,
    /// The page.
    pub url: Option<String>,
    /// The page title.
    pub title: Option<String>,
    /// The referring host.
    pub referrer: Option<String>,
    /// The referring path.
    pub referrer_path: Option<String>,
    /// The hostname of the site.
    pub hostname: Option<String>,
    /// The traffic channel: direct, Search, Social, email, paidAds, …
    pub channel: Option<String>,
    /// The browser family.
    pub browser: Option<String>,
    /// The operating system.
    pub os: Option<String>,
    /// The device class.
    pub device: Option<String>,
    /// The country.
    pub country: Option<String>,
    /// The region.
    pub region: Option<String>,
    /// The city.
    pub city: Option<String>,
    /// The interface language.
    pub language: Option<String>,
    /// The viewport size.
    pub screen: Option<String>,
}

/// How many events the session log reads.
///
/// The session log is a **log**: it shows the most recent sessions, not a
/// summary of the whole window. Aggregating every event of a month to show the
/// last fifty sessions is wasted work — the answer only depends on the last
/// few thousand events.
///
/// So the log is derived from the most recent events, and the cost of the log is
/// the cost of that scan, not the cost of the table. Fifty sessions need about
/// fifty events each, so the budget is fifty times the number of rows asked
/// for, with a floor and a ceiling.
pub fn session_scan_limit(limit: i64) -> i64 {
    (limit.max(1) * 50).clamp(500, 200_000)
}

/// Group the most recent events into the rows of the session log.
///
/// The rules are the same on every backend, because this is where they live:
///
///   * the **last** page of the session is the `url` and the `title`;
///   * the **first** page is the `entry_url`;
///   * the referrer is the one the session **started** with;
///   * the visitor's environment (browser, country, screen, …) is the one of the
///     **newest** event that carries it — those do not change within a session;
///   * a session that is only half in the scan is reported with the views it has
///     in the scan, which is the price of a bounded scan.
pub fn group_sessions(
    website_id: &str,
    events: &[SessionEvent],
    limit: i64,
    offset: i64,
) -> Vec<SessionRow> {
    // The events arrive newest first; the grouping needs them oldest first.
    let mut by_session: std::collections::HashMap<Uuid, Vec<&SessionEvent>> =
        std::collections::HashMap::new();
    for event in events {
        by_session
            .entry(event.visit_id)
            .or_default()
            .push(event);
    }

    let mut rows: Vec<SessionRow> = by_session
        .into_iter()
        .map(|(visit_id, mut group)| {
            group.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            let first = group.first().expect("a group is never empty");
            let last = group.last().expect("a group is never empty");

            let newest_with = |pick: fn(&SessionEvent) -> Option<&String>| -> Option<String> {
                group
                    .iter()
                    .rev()
                    .find_map(|event| pick(event).cloned())
            };
            let oldest_with = |pick: fn(&SessionEvent) -> Option<&String>| -> Option<String> {
                group.iter().find_map(|event| pick(event).cloned())
            };

            SessionRow {
                id: visit_id.to_string(),
                website_id: website_id.to_string(),
                created_at: first.created_at,
                updated_at: last.created_at,
                hostname: newest_with(|event| event.hostname.as_ref()),
                url: last.url.clone(),
                title: newest_with(|event| event.title.as_ref()),
                referrer: oldest_with(|event| event.referrer.as_ref()),
                referrer_path: oldest_with(|event| event.referrer_path.as_ref()),
                channel: oldest_with(|event| event.channel.as_ref()),
                entry_url: first.url.clone(),
                browser: newest_with(|event| event.browser.as_ref()),
                os: newest_with(|event| event.os.as_ref()),
                device: newest_with(|event| event.device.as_ref()),
                country: newest_with(|event| event.country.as_ref()),
                region: newest_with(|event| event.region.as_ref()),
                city: newest_with(|event| event.city.as_ref()),
                language: newest_with(|event| event.language.as_ref()),
                screen: newest_with(|event| event.screen.as_ref()),
                views: group.len() as i64,
                duration: (last.created_at - first.created_at).num_seconds(),
            }
        })
        .collect();

    // The newest session first; the id breaks the tie, so the order never
    // depends on the order the rows came back in.
    rows.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });

    let offset = offset.max(0) as usize;
    rows.into_iter().skip(offset).take(limit.max(0) as usize).collect()
}
/// A storage backend.
pub trait Storage: Send + Sync {
    /// The backend's name.
    fn backend(&self) -> &'static str;

    /// List the websites.
    fn websites(&self) -> Result<Vec<Website>>;

    /// Load one website.
    fn website(&self, id: &str) -> Result<Option<Website>>;

    /// Create a website.
    fn create_website(
        &self,
        name: &str,
        domain: Option<&str>,
        retention_days: i32,
        country_header: Option<&str>,
    ) -> Result<Website>;

    /// Update a website.
    fn update_website(&self, id: &str, patch: &WebsitePatch) -> Result<Website>;

    /// Delete a website and all of its data. Returns whether it existed.
    fn delete_website(&self, id: &str) -> Result<bool>;

    /// The summary metrics for a window.
    fn summary(&self, query: &ReportQuery) -> Result<SummaryMetrics>;

    /// The summary metrics plus the period-over-period change.
    fn summary_with_change(&self, query: &ReportQuery) -> Result<MetricsSummary>;

    /// Break a website down over one dimension.
    fn breakdown(&self, query: &ReportQuery, dimension: Dimension) -> Result<Vec<BreakdownRow>>;

    /// Page views over time.
    fn timeseries(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>>;

    /// Custom events, grouped by name.
    fn events(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>>;

    /// The query strings of the visited pages, with how often each was seen.
    ///
    /// The parameters are split by [`parse_query_parameters`] — in Rust, so
    /// that every engine answers the same.
    fn query_strings(&self, query: &ReportQuery) -> Result<Vec<(String, i64)>>;

    /// The raw event log.
    fn event_log(&self, query: &ReportQuery) -> Result<Vec<EventRow>>;

    /// The session log.
    fn sessions(&self, query: &ReportQuery) -> Result<Vec<SessionRow>>;

    /// The most recent events of a window, newest first.
    ///
    /// The session log is built from these — see [`group_sessions`]. A backend
    /// only has to read the last few thousand rows, not the whole window.
    fn recent_events(&self, query: &ReportQuery, limit: i64) -> Result<Vec<SessionEvent>>;

    /// Persist a batch of hits.
    fn write_batch(&self, hits: &[crate::models::Hit]) -> Result<()>;

    /// Persist a batch of revenue records.
    fn write_revenue(&self, records: &[crate::models::Revenue]) -> Result<()>;

    /// The revenue of a website over a window.
    fn revenue(&self, query: &ReportQuery) -> Result<Vec<crate::models::Revenue>>;

    /// Delete data that is older than the retention window of its website.
    ///
    /// Returns how many **events** and how many **revenue records** were removed.
    /// The two numbers mean the same thing on every backend.
    fn prune(&self, website: Option<&str>) -> Result<(u64, u64)>;

    /// Delete everything older than a moment. Used by the archival job once the cold
    /// tier holds the data.
    ///
    /// Returns how many **events** and how many **revenue records** were removed.
    fn delete_before(
        &self,
        before: NaiveDateTime,
        website: Option<&str>,
    ) -> Result<(u64, u64)>;

    /// Delete a website and all of its data. Returns the number of rows removed.
    fn erase(&self, website: &str) -> Result<u64>;

    /// What the backend reports about itself.
    fn stats(&self) -> Result<StorageStats>;
}
