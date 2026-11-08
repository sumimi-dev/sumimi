//! Wire types: the Umami-compatible payloads we accept and the shapes we return.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A collected hit — one row in the `event` table.
///
/// The field set mirrors Umami's `website_event`: we store everything Umami stores, so a
/// user who switches from Umami does not lose a single dimension. The storage engine
/// (DuckDB) is what makes that affordable.
///
/// Every field is optional except the identity of the hit: `website_id`, `session_id`,
/// `visitor_id` and `created_at`.
#[derive(Debug, Clone, Default)]
pub struct Hit {
    /// The website the hit belongs to.
    pub website_id: String,
    /// The session the hit belongs to.
    pub session_id: Uuid,
    /// The visitor, stable across days (Umami's `distinct_id`).
    pub visitor_id: Uuid,
    /// The visit: a half-hour window inside the session.
    ///
    /// The reference implementation has both a session and a visit, and the
    /// visit is the one its channel report counts. We keep both, so the two
    /// numbers can be compared.
    pub visit_id: Option<Uuid>,
    /// Retention window of the website in days (`0` = keep forever).
    pub retention_days: i32,
    /// What kind of hit this is.
    pub kind: HitKind,
    /// When it happened.
    pub created_at: chrono::NaiveDateTime,

    // --- the page ---
    /// The path of the page.
    pub url: Option<String>,
    /// The query string of the page.
    pub url_query: Option<String>,
    /// The title of the page.
    pub title: Option<String>,

    // --- where it came from ---
    /// The host of the referrer.
    pub referrer: Option<String>,
    /// The path of the referrer.
    pub referrer_path: Option<String>,
    /// The query string of the referrer.
    pub referrer_query: Option<String>,
    /// The display name of the referrer ("Hacker News"), when it has one.
    pub referrer_name: Option<String>,
    /// The hostname of the site.
    pub hostname: Option<String>,
    /// The traffic channel: direct, Search, Social, email, paidAds, …
    pub channel: Option<String>,

    // --- the visitor's environment ---
    /// The browser family.
    pub browser: Option<String>,
    /// The browser version.
    pub browser_version: Option<String>,
    /// The operating system family.
    pub os: Option<String>,
    /// The operating system version.
    pub os_version: Option<String>,
    /// The device class.
    pub device: Option<String>,
    /// The viewport size.
    pub screen: Option<String>,
    /// The interface language.
    pub language: Option<String>,
    /// The country.
    pub country: Option<String>,
    /// The region.
    pub region: Option<String>,
    /// The city.
    pub city: Option<String>,
    /// The latitude of the visitor.
    ///
    /// The IP address itself is never stored — only where it points to.
    pub latitude: Option<f64>,
    /// The longitude of the visitor.
    pub longitude: Option<f64>,

    // --- marketing ---
    /// `utm_source`.
    pub utm_source: Option<String>,
    /// `utm_medium`.
    pub utm_medium: Option<String>,
    /// `utm_campaign`.
    pub utm_campaign: Option<String>,
    /// `utm_content`.
    pub utm_content: Option<String>,
    /// `utm_term`.
    pub utm_term: Option<String>,

    // --- ad click ids ---
    /// Google's click id.
    pub gclid: Option<String>,
    /// Facebook's click id.
    pub fbclid: Option<String>,
    /// Microsoft's click id.
    pub msclkid: Option<String>,
    /// TikTok's click id.
    pub ttclid: Option<String>,
    /// LinkedIn's click id.
    pub lifatid: Option<String>,
    /// X's (Twitter's) click id.
    pub twclid: Option<String>,

    // --- how fast the page was ---
    /// Largest Contentful Paint, in milliseconds.
    pub lcp: Option<f64>,
    /// Interaction to Next Paint, in milliseconds.
    pub inp: Option<f64>,
    /// Cumulative Layout Shift.
    pub cls: Option<f64>,
    /// First Contentful Paint, in milliseconds.
    pub fcp: Option<f64>,
    /// Time to First Byte, in milliseconds.
    pub ttfb: Option<f64>,

    // --- custom events ---
    /// The event name.
    pub name: Option<String>,
    /// The event tag.
    pub tag: Option<String>,
    /// Free-form event data.
    pub data: Option<String>,
}

/// What kind of hit this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HitKind {
    /// A page view.
    #[default]
    Pageview,
    /// A custom event.
    Event,
    /// A screen view (mobile).
    Screenview,
    /// An identify call (attaches data to the session).
    Identify,
}

impl HitKind {
    /// Parse the Umami `type` discriminator.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "event" => Some(HitKind::Event),
            "screenview" => Some(HitKind::Screenview),
            "identify" => Some(HitKind::Identify),
            _ => None,
        }
    }

    /// The value stored in `event_type`.
    pub fn as_str(self) -> &'static str {
        match self {
            HitKind::Pageview => "pageview",
            HitKind::Event => "event",
            HitKind::Screenview => "screenview",
            HitKind::Identify => "identify",
        }
    }
}

/// A revenue record: what a session or an event earned.
#[derive(Debug, Clone)]
pub struct Revenue {
    /// The website the revenue belongs to.
    pub website_id: String,
    /// The session that earned it.
    pub session_id: Option<Uuid>,
    /// The event that earned it.
    pub event_id: Option<i64>,
    /// The name of the revenue source (`carbon`, `adsense`, …).
    pub event_name: Option<String>,
    /// The currency the amount is in.
    pub currency: String,
    /// The amount.
    pub revenue: f64,
    /// When it was earned.
    pub created_at: chrono::NaiveDateTime,
}

/// Body of `POST /api/send` (Umami v2 shape).
#[derive(Debug, Deserialize)]
pub struct SendRequest {
    /// Website id or domain. May also live inside `payload`.
    #[serde(default)]
    pub website: Option<String>,
    /// `event`, `screenview` or `identify`.
    #[serde(default)]
    pub r#type: Option<String>,
    /// The hit payload.
    #[serde(default, deserialize_with = "optional_payload")]
    pub payload: Option<Payload>,
}

impl SendRequest {
    /// The payload, with the website filled in.
    ///
    /// **There is no tree any more** ✓: the payload is deserialized
    /// **straight into the fields** the collector reads ✓ ⇒ ⇒ the body is
    /// parsed **once** ✓ ⇒ ⇒ the fields nobody reads are never built ✓ ⇒ ⇒
    /// ⇒ 679 bytes per request of `serde_json::Value` tree — and every copy
    /// out of it — are gone ✓ (see `MEMORY.md` §6 ✓).
    pub fn into_payload(self) -> Payload {
        let SendRequest {
            website,
            r#type,
            payload,
        } = self;
        let mut payload = payload.unwrap_or_default();
        if payload.website.is_none() {
            payload.website = website;
        }
        payload.kind = r#type;
        payload
    }
}

/// The payload of a collection request — **the fields, not a tree**.
///
/// Every field is the one the collector reads, and every field arrives
/// **already normalised** ✓: trimmed ✓, empty dropped ✓ ⇒ ⇒ the collector
/// never has to copy a field out to trim it ✓ ⇒ ⇒ ⇒ the fields that nobody
/// reads are never even built ✓ ✓.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Payload {
    /// The website the hit belongs to.
    #[serde(default, deserialize_with = "optional_text")]
    pub website: Option<String>,
    /// The path of the page.
    #[serde(default, deserialize_with = "optional_text")]
    pub url: Option<String>,
    /// The title of the page.
    #[serde(default, deserialize_with = "optional_text")]
    pub title: Option<String>,
    /// The referrer.
    #[serde(default, deserialize_with = "optional_text")]
    pub referrer: Option<String>,
    /// The event name.
    #[serde(default, deserialize_with = "optional_text")]
    pub name: Option<String>,
    /// The event tag.
    #[serde(default, deserialize_with = "optional_text")]
    pub tag: Option<String>,
    /// The hostname of the site.
    #[serde(default, deserialize_with = "optional_text")]
    pub hostname: Option<String>,
    /// The interface language.
    #[serde(default, deserialize_with = "optional_text")]
    pub language: Option<String>,
    /// The viewport size.
    #[serde(default, deserialize_with = "optional_text")]
    pub screen: Option<String>,
    /// The region of the visitor.
    #[serde(default, deserialize_with = "optional_text")]
    pub region: Option<String>,
    /// The city of the visitor.
    #[serde(default, deserialize_with = "optional_text")]
    pub city: Option<String>,
    /// The client's own session id.
    #[serde(default, deserialize_with = "optional_text", rename = "sessionId")]
    pub session_id: Option<String>,
    /// Largest Contentful Paint, in milliseconds.
    #[serde(default, deserialize_with = "optional_number")]
    pub lcp: Option<f64>,
    /// Interaction to Next Paint, in milliseconds.
    #[serde(default, deserialize_with = "optional_number")]
    pub inp: Option<f64>,
    /// Cumulative Layout Shift.
    #[serde(default, deserialize_with = "optional_number")]
    pub cls: Option<f64>,
    /// First Contentful Paint, in milliseconds.
    #[serde(default, deserialize_with = "optional_number")]
    pub fcp: Option<f64>,
    /// Time to First Byte, in milliseconds.
    #[serde(default, deserialize_with = "optional_number")]
    pub ttfb: Option<f64>,
    /// The custom event's data — free-form, so it stays a `Value`.
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    /// The kind of the hit.
    ///
    /// Not a payload field: it comes from the request's `type` ✓.
    #[serde(skip)]
    pub kind: Option<String>,
}

impl Payload {
    /// The `data` object (custom event properties) — **taken**, not copied.
    ///
    /// A string is moved out of the payload ✓ ⇒ ⇒ ⇒ no copy ✓; an object is
    /// written as JSON ✓ (that is what the event column holds ✓).
    pub fn take_data(&mut self) -> Option<String> {
        match self.data.take() {
            Some(serde_json::Value::String(text)) => Some(text),
            Some(serde_json::Value::Null) | None => None,
            Some(other) => Some(other.to_string()),
        }
    }
}

/// The rule for a text field: **trimmed**, and empty is nothing.
///
/// The rule is the same everywhere ✓ ⇒ ⇒ the collector never has to trim a
/// field it has already got ✓ ⇒ ⇒ ⇒ and a blank title is no title ✓.
pub fn normalise(text: Option<String>) -> Option<String> {
    text.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else if trimmed.len() == text.len() {
            Some(text)
        } else {
            Some(trimmed.to_string())
        }
    })
}

/// A text field that arrives as a string — **or** as something else.
///
/// A field of the wrong type is **ignored**, not an error ✓: the trackers
/// differ ✓ ⇒ ⇒ and one odd field must not lose the whole event ✓.
fn optional_text<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::String(text)) => normalise(Some(text)),
        _ => None,
    })
}

/// A number that arrives as a JSON number **or** as a string.
///
/// The tracker sends the vitals as strings ✓ (`String(metrics.lcp)` ✓ — see
/// `js/sumimi.js` ✓) ⇒ ⇒ ⇒ both are read ✓.
fn optional_number<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::Number(number)) => number.as_f64(),
        Some(serde_json::Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    })
}

/// The payload — **an object**, or nothing.
///
/// A payload that is not an object is **ignored**, not an error ✓: the
/// request still says which website it is for ✓ ⇒ ⇒ and the event is worth
/// keeping ✓.
fn optional_payload<'de, D>(deserializer: D) -> Result<Option<Payload>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct PayloadVisitor;

    impl<'de> serde::de::Visitor<'de> for PayloadVisitor {
        type Value = Option<Payload>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a payload object")
        }

        fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            // The struct, straight out of the map — **no tree in between** ✓.
            serde::Deserialize::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                .map(Some)
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(self)
        }

        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }
    }

    deserializer.deserialize_option(PayloadVisitor)
}

/// A website as returned by the API (camelCase, like Umami).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Website {
    pub id: String,
    pub name: String,
    pub domain: Option<String>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
    pub retention_days: i32,
    pub country_header: Option<String>,
}

/// Request body for creating a website.
#[derive(Debug, Deserialize)]
pub struct CreateWebsite {
    pub name: String,
    #[serde(default)]
    pub domain: Option<String>,
}

/// Request body for updating a website.
#[derive(Debug, Deserialize)]
pub struct UpdateWebsite {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub retention_days: Option<i32>,
    #[serde(default)]
    pub country_header: Option<String>,
}

/// Login request.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// The user object returned after login.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    pub username: String,
    pub role: String,
    pub created_at: chrono::NaiveDateTime,
}

/// Login response (`{ token, user }`, like Umami).
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub user: User,
}

/// A metric with its period-over-period change.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricValue {
    pub value: i64,
    pub change: f64,
}

/// Summary metrics, Umami-shaped.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsSummary {
    pub pageviews: MetricValue,
    pub visitors: MetricValue,
    pub views: MetricValue,
    pub bounces: MetricValue,
    pub visit_duration: MetricValue,
}

/// One row of a breakdown.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakdownRow {
    /// The dimension value (url, referrer, browser name, ...).
    pub name: String,
    pub value: i64,
    /// Page URL for the `pages` breakdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Bucket start for the `timeseries` breakdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

/// A session row — derived from the page views of that session.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRow {
    /// The session id.
    pub id: String,
    /// The website it belongs to.
    pub website_id: String,
    /// When the session started.
    pub created_at: chrono::NaiveDateTime,
    /// When the session was last seen.
    pub updated_at: chrono::NaiveDateTime,
    /// The hostname of the site.
    pub hostname: Option<String>,
    /// The traffic channel: direct, Search, Social, email, paidAds, …
    pub channel: Option<String>,
    /// The last page of the session.
    pub url: Option<String>,
    /// The title of the last page.
    pub title: Option<String>,
    /// The referrer host.
    pub referrer: Option<String>,
    /// The referrer path.
    pub referrer_path: Option<String>,
    /// The first page of the session.
    pub entry_url: Option<String>,
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
    /// How many page views the session had.
    pub views: i64,
    /// How long the session lasted, in seconds.
    pub duration: i64,
}

/// One row of the event log.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRow {
    /// The event id.
    pub id: String,
    /// The website it belongs to.
    pub website_id: String,
    /// The session it belongs to.
    pub session_id: Option<String>,
    /// When it happened.
    pub created_at: chrono::NaiveDateTime,
    /// The event name (`pageview` for a plain page view).
    pub name: Option<String>,
    /// The page.
    pub url: Option<String>,
    /// The hostname of the site.
    pub hostname: Option<String>,
    /// The traffic channel: direct, Search, Social, email, paidAds, …
    pub channel: Option<String>,
    /// Free-form event data.
    pub data: Option<String>,
    /// The referrer host.
    pub referrer: Option<String>,
    /// The referrer path.
    pub referrer_path: Option<String>,
    /// The referrer query.
    pub referrer_query: Option<String>,
    /// The country.
    pub country: Option<String>,
    /// The city.
    pub city: Option<String>,
    /// The browser family.
    pub browser: Option<String>,
    /// The operating system.
    pub os: Option<String>,
    /// The device class.
    pub device: Option<String>,
    /// `utm_source`.
    pub utm_source: Option<String>,
    /// `utm_medium`.
    pub utm_medium: Option<String>,
    /// `utm_campaign`.
    pub utm_campaign: Option<String>,
    /// Largest Contentful Paint, in milliseconds.
    pub lcp: Option<f64>,
    /// Interaction to Next Paint, in milliseconds.
    pub inp: Option<f64>,
    /// Cumulative Layout Shift.
    pub cls: Option<f64>,
}

/// Heartbeat / health payload.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat {
    pub ok: bool,
    pub version: &'static str,
    pub uptime_seconds: u64,
    pub queued: u64,
    pub dropped: u64,
    pub websites: i64,
    /// What DuckDB's buffer manager currently holds in memory.
    pub duckdb_memory_bytes: i64,
    /// The configured DuckDB memory limit (e.g. `256MB`).
    pub duckdb_memory_limit: String,
    /// The limit DuckDB itself reports (read back from DuckDB, not from our config).
    pub duckdb_memory_limit_effective: String,
    /// How many threads DuckDB may use.
    pub duckdb_threads: i64,
    /// How many batches have been written.
    pub flushes: u64,
    /// The median write duration, in milliseconds.
    pub flush_p50_ms: u64,
    /// The 99th percentile of the write durations, in milliseconds.
    pub flush_p99_ms: u64,
    /// The slowest write, in milliseconds.
    pub flush_max_ms: u64,
    /// The size of the database file on disk.
    pub database_size_bytes: i64,
    /// How many collection requests are being read or parsed right now.
    pub inflight_requests: u64,
    /// How many bytes are being read or parsed right now.
    pub inflight_bytes: u64,
    /// How many requests the admission gate refused by its rate limit.
    pub refused_rate: u64,
    /// How many requests the admission gate refused because too many were in flight.
    pub refused_inflight: u64,
    /// How many requests the admission gate refused because the body was too large.
    pub refused_body: u64,
    /// How many requests a second the admission gate admits (`0` = no limit).
    pub admit_rps: u64,
    /// The resident set of the server's process right now, in bytes.
    ///
    /// On Linux this is the exact number from `/proc`; elsewhere it is the
    /// portable approximation (`ps`), which counts the shared pages of every
    /// mapped binary differently — for information, not for the budget.
    pub rss_bytes: Option<u64>,
    /// The **peak** resident set, in bytes: the high-water mark, which is the
    /// number the memory budgets are asserted against.
    ///
    /// `None` off Linux: only `/proc` has the exact high-water mark.
    pub peak_rss_bytes: Option<u64>,
    /// What machine this is — the CPU, the CPUs, the memory.
    ///
    /// So a load test can stamp its report with the machine that was
    /// measured, and not with the machine that measured it.
    pub machine: crate::machine::Machine,
}

/// Query parameters shared by the stats endpoints (Umami-compatible names).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsQuery {
    /// Unix milliseconds, inclusive.
    #[serde(default)]
    pub start_at: Option<i64>,
    /// Unix milliseconds, exclusive.
    #[serde(default)]
    pub end_at: Option<i64>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub referrer: Option<String>,
    #[serde(default)]
    pub browser: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// Which report to run.
///
/// The names are the ones the API uses (`pages`, `referrers`, …), so a saved
/// report is readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReportKind {
    /// The summary metrics.
    Summary,
    /// The page views over time.
    Timeseries,
    /// The top pages.
    Pages,
    /// The traffic sources.
    Referrers,
    /// The browser breakdown.
    Browsers,
    /// The operating system breakdown.
    Os,
    /// The device breakdown.
    Devices,
    /// The country breakdown.
    Countries,
    /// The language breakdown.
    Languages,
    /// The screen size breakdown.
    Screens,
    /// The custom events.
    Events,
    /// The traffic channels.
    Channels,
    /// The regions of the visitors.
    Regions,
    /// The cities of the visitors.
    Cities,
    /// The hostnames of the sites.
    Hosts,
    /// The tags of the custom events.
    Tags,
    /// The query parameters of the visited pages.
    QueryParameters,
    /// The session log.
    Sessions,
    /// The raw event log.
    EventLog,
}

impl ReportKind {
    /// Every report, in the order the API lists them.
    pub fn all() -> [ReportKind; 18] {
        [
            ReportKind::Summary,
            ReportKind::Timeseries,
            ReportKind::Pages,
            ReportKind::Referrers,
            ReportKind::Browsers,
            ReportKind::Os,
            ReportKind::Devices,
            ReportKind::Countries,
            ReportKind::Regions,
            ReportKind::Cities,
            ReportKind::Languages,
            ReportKind::Screens,
            ReportKind::Channels,
            ReportKind::Hosts,
            ReportKind::Events,
            ReportKind::Tags,
            ReportKind::QueryParameters,
            ReportKind::Sessions,
        ]
    }
}

/// A report to run: which website, which report, and the window and the filters.
///
/// This is what the stats endpoints take, what the cache keys on, and what a
/// **snapshot** stores — so a saved report can be run again later.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportSpec {
    /// The website the report is about.
    pub website: String,
    /// Which report to run.
    pub report: ReportKind,
    /// The window and the filters.
    #[serde(flatten)]
    pub query: StatsQuery,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fields are trimmed, and a blank one is nothing.
    #[test]
    fn the_fields_are_trimmed() {
        let request: SendRequest =
            serde_json::from_str(r#"{"payload":{"url":"  /hello  ","title":"   "}}"#).unwrap();
        let payload = request.into_payload();
        assert_eq!(payload.url.as_deref(), Some("/hello"));
        assert_eq!(payload.title, None, "a blank title is no title");
    }

    /// A field of the wrong type is ignored — it does not lose the event.
    #[test]
    fn a_field_of_the_wrong_type_is_ignored() {
        let request: SendRequest =
            serde_json::from_str(r#"{"payload":{"url":123,"title":"Hello"}}"#).unwrap();
        let payload = request.into_payload();
        assert_eq!(payload.url, None);
        assert_eq!(payload.title.as_deref(), Some("Hello"));
    }

    /// A payload that is not an object is ignored, not an error.
    #[test]
    fn a_payload_that_is_not_an_object_is_ignored() {
        for body in [
            r#"{"payload":"text","website":"example.com"}"#,
            r#"{"payload":42,"website":"example.com"}"#,
            r#"{"payload":[1,2],"website":"example.com"}"#,
            r#"{"payload":null,"website":"example.com"}"#,
        ] {
            let request: SendRequest = serde_json::from_str(body).unwrap();
            let payload = request.into_payload();
            assert_eq!(payload.website.as_deref(), Some("example.com"), "{body}");
        }
    }

    /// The vitals arrive as numbers **and** as strings — the tracker sends
    /// strings.
    #[test]
    fn the_vitals_are_read_both_ways() {
        let request: SendRequest = serde_json::from_str(
            r#"{"payload":{"lcp":"1200","inp":34,"cls":0.02,"unknown":true}}"#,
        )
        .unwrap();
        let payload = request.into_payload();
        assert_eq!(payload.lcp, Some(1200.0));
        assert_eq!(payload.inp, Some(34.0));
        assert_eq!(payload.cls, Some(0.02));
    }

    /// The website comes from the payload, or from the request.
    #[test]
    fn the_website_comes_from_the_payload_or_the_request() {
        let request: SendRequest =
            serde_json::from_str(r#"{"website":"top.com","payload":{"website":"in.com"}}"#)
                .unwrap();
        assert_eq!(request.into_payload().website.as_deref(), Some("in.com"));

        let request: SendRequest =
            serde_json::from_str(r#"{"website":"top.com","payload":{}}"#).unwrap();
        assert_eq!(request.into_payload().website.as_deref(), Some("top.com"));
    }

    /// The `data` of a custom event: an object becomes its JSON, a string
    /// stays itself.
    #[test]
    fn the_data_is_read() {
        let mut payload = serde_json::from_str::<SendRequest>(r#"{"payload":{"data":{"a":1}}}"#)
            .unwrap()
            .into_payload();
        assert_eq!(payload.take_data().as_deref(), Some(r#"{"a":1}"#));

        let mut payload =
            serde_json::from_str::<SendRequest>(r#"{"payload":{"data":"plain"}}"#)
                .unwrap()
                .into_payload();
        assert_eq!(payload.take_data().as_deref(), Some("plain"));
    }
}
