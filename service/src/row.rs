//! The rows — read **by name**, once.
//!
//! A row of a report is read into a model, and the model is the same for every
//! engine. So the reading is written down **once**, here, and each engine only
//! has to say how a column of its own is fetched.
//!
//! **Why by name.** A row read by position is a row read by a number that
//! somebody has to keep in their head: add a column to the statement and every
//! number after it moves. The numbers are then wrong in a way that nothing
//! complains about — the report answers, it just answers about another column.
//!
//! That is not a theory either: the PostgreSQL backend read the event's type
//! where the tag was and the tag where the data was, and the parity check
//! against the reference implementation was what found it. **By name** there is
//! no number to get wrong: a column that is not there is an error, not a
//! different value.

use anyhow::Result;
use chrono::NaiveDateTime;
use uuid::Uuid;

use crate::models::{EventRow, Hit, HitKind};
use crate::session::{format_visit_id, VisitId};
use crate::storage::SessionEvent;

/// A row of a report, read by the name of its column.
pub trait Row {
    /// The text of a column.
    fn text(&self, name: &str) -> Result<Option<String>>;

    /// A whole number.
    fn int(&self, name: &str) -> Result<Option<i64>>;

    /// A number with a fraction.
    fn float(&self, name: &str) -> Result<Option<f64>>;

    /// A sixteen-byte id.
    fn uuid(&self, name: &str) -> Result<Option<Uuid>>;

    /// A moment in time.
    fn time(&self, name: &str) -> Result<Option<NaiveDateTime>>;
}

/// The row of the **event log**.
pub fn event_log_row(row: &dyn Row) -> Result<EventRow> {
    Ok(EventRow {
        id: row.int("id")?.unwrap_or_default().to_string(),
        website_id: row.text("website_id")?.unwrap_or_default(),
        session_id: row.uuid("visit_id")?.map(format_visit_id),
        created_at: row.time("created_at")?.unwrap_or_default(),
        name: row.text("event_name")?,
        url: row.text("url")?,
        hostname: row.text("hostname")?,
        channel: row.text("channel")?,
        data: row.text("data")?,
        referrer: row.text("referrer")?,
        referrer_path: row.text("referrer_path")?,
        referrer_query: row.text("referrer_query")?,
        country: row.text("country")?,
        city: row.text("city")?,
        browser: row.text("browser")?,
        os: row.text("os")?,
        device: row.text("device")?,
        utm_source: row.text("utm_source")?,
        utm_medium: row.text("utm_medium")?,
        utm_campaign: row.text("utm_campaign")?,
        lcp: row.float("lcp")?,
        inp: row.float("inp")?,
        cls: row.float("cls")?,
    })
}

/// The row of the **session log** — the event the session is built from.
pub fn session_event_row(row: &dyn Row) -> Result<SessionEvent> {
    Ok(SessionEvent {
        id: row.int("id")?.unwrap_or_default(),
        visit_id: row.uuid("visit_id")?.unwrap_or_else(Uuid::nil),
        session_start: row.time("session_start")?,
        ip_prefix: row.text("ip_prefix")?.unwrap_or_default(),
        user_agent: row.text("user_agent")?.unwrap_or_default(),
        created_at: row.time("created_at")?.unwrap_or_default(),
        url: row.text("url")?,
        title: row.text("title")?,
        referrer: row.text("referrer")?,
        referrer_path: row.text("referrer_path")?,
        hostname: row.text("hostname")?,
        channel: row.text("channel")?,
        browser: row.text("browser")?,
        os: row.text("os")?,
        device: row.text("device")?,
        country: row.text("country")?,
        region: row.text("region")?,
        city: row.text("city")?,
        language: row.text("language")?,
        screen: row.text("screen")?,
    })
}

/// The session id of a row, as it is shown.
pub fn visit_id_text(id: VisitId) -> String {
    format_visit_id(id)
}

/// The columns of the **deep read** — the raw rows, field by field.
///
/// The statement that reads them is the shared one (see [`crate::query`]); the
/// list is here so that the three engines ask for the same columns in the
/// same order.
pub const EVENT_COLUMNS: &str = "id, website_id, visit_id, visitor_id, session_start, \
     ip, ip_prefix, user_agent, user_id, created_at, \
     url, url_query, title, \
     referrer, referrer_path, referrer_query, referrer_name, hostname, channel, \
     browser, browser_version, os, os_version, device, screen, language, \
     country, region, city, latitude, longitude, \
     utm_source, utm_medium, utm_campaign, utm_content, utm_term, \
     gclid, fbclid, msclkid, ttclid, lifatid, twclid, \
     lcp, inp, cls, fcp, ttfb, \
     event_type, event_name, tag, data";

/// The row of the **deep read** — a whole event.
///
/// The mapping is here, once: an engine that hands the rows back cannot get
/// the fields mixed up, because it does not know the field order — it only
/// says how a column of its own is fetched.
pub fn hit_row(row: &dyn Row) -> Result<Hit> {
    let event_type = row.text("event_type")?;
    Ok(Hit {
        website_id: row.text("website_id")?.unwrap_or_default(),
        visit_id: row.uuid("visit_id")?.unwrap_or_else(Uuid::nil),
        visitor_id: row.uuid("visitor_id")?.unwrap_or_else(Uuid::nil),
        session_start: row.time("session_start")?,
        ip: row.text("ip")?.unwrap_or_default(),
        ip_prefix: row.text("ip_prefix")?.unwrap_or_default(),
        user_agent: row.text("user_agent")?.unwrap_or_default(),
        user_id: row.uuid("user_id")?,
        retention_days: 0,
        kind: match event_type.as_deref() {
            Some("event") => HitKind::Event,
            Some("screenview") => HitKind::Screenview,
            Some("identify") => HitKind::Identify,
            Some("performance") => HitKind::Performance,
            Some("error") => HitKind::Error,
            _ => HitKind::Pageview,
        },
        created_at: row.time("created_at")?.unwrap_or_default(),
        url: row.text("url")?,
        url_query: row.text("url_query")?,
        title: row.text("title")?,
        referrer: row.text("referrer")?,
        referrer_path: row.text("referrer_path")?,
        referrer_query: row.text("referrer_query")?,
        referrer_name: row.text("referrer_name")?,
        hostname: row.text("hostname")?,
        channel: row.text("channel")?,
        browser: row.text("browser")?,
        browser_version: row.text("browser_version")?,
        os: row.text("os")?,
        os_version: row.text("os_version")?,
        device: row.text("device")?,
        screen: row.text("screen")?,
        language: row.text("language")?,
        country: row.text("country")?,
        region: row.text("region")?,
        city: row.text("city")?,
        latitude: row.float("latitude")?,
        longitude: row.float("longitude")?,
        utm_source: row.text("utm_source")?,
        utm_medium: row.text("utm_medium")?,
        utm_campaign: row.text("utm_campaign")?,
        utm_content: row.text("utm_content")?,
        utm_term: row.text("utm_term")?,
        gclid: row.text("gclid")?,
        fbclid: row.text("fbclid")?,
        msclkid: row.text("msclkid")?,
        ttclid: row.text("ttclid")?,
        lifatid: row.text("lifatid")?,
        twclid: row.text("twclid")?,
        lcp: row.float("lcp")?,
        inp: row.float("inp")?,
        cls: row.float("cls")?,
        fcp: row.float("fcp")?,
        ttfb: row.float("ttfb")?,
        name: row.text("event_name")?,
        tag: row.text("tag")?,
        data: row.text("data")?,
    })
}
