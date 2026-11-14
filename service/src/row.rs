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

use crate::models::EventRow;
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
