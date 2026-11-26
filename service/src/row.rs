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

use std::collections::HashMap;

use anyhow::Result;
use chrono::NaiveDateTime;
use uuid::Uuid;

use crate::models::{EventRow, Hit, HitKind, Property, PropertyValue};
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

    /// **A byte string** — the visitor's id, in the width it has.
    fn blob(&self, name: &str) -> Result<Option<Vec<u8>>>;

    /// **The event id** — the id of an event, in the shape the engine keeps it.
    ///
    /// DuckDB has a uuid type and hands the id over as one; SQLite has not,
    /// so the id comes back as the **sixteen bytes** the column holds.
    ///
    /// ⇒ ⇒ Both are the same id: the property rows and the event rows are
    /// matched by it, and a mismatch would lose a property.
    fn event_id(&self, name: &str) -> Result<Option<Uuid>>;

    /// A moment in time.
    fn time(&self, name: &str) -> Result<Option<NaiveDateTime>>;
}

/// The row of the **event log**.
pub fn event_log_row(row: &dyn Row) -> Result<EventRow> {
    Ok(EventRow {
        id: row.text("event_id")?.unwrap_or_default(),
        website_id: row.text("website_id")?.unwrap_or_default(),
        // The two ids, named the way Umami names them: the `session_id` is
        // the visitor, the `visit_id` is the stretch of activity it is in.
        // The visitor's id: the hex of its bytes, so the text says how wide
        // the id is (sixteen characters are eight bytes).
        session_id: row
            .blob("session_id")?
            .map(|bytes| crate::ua::format_visitor_id(&bytes)),
        visit_id: row.uuid("visit_id")?.map(format_visit_id),
        created_at: row.time("created_at")?.unwrap_or_default(),
        name: row.text("event_name")?,
        url: row.text("url_path")?,
        hostname: row.text("hostname")?,
        channel: row.text("channel")?,
        // The properties are not in the fact table any more — they are read
        // from the table of their own and put here by the caller.
        data: None,
        referrer: row.text("referrer_domain")?,
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
        id: row.text("event_id")?.unwrap_or_default(),
        visit_id: row.uuid("visit_id")?.unwrap_or_else(Uuid::nil),
        session_start: row.time("session_start")?,
        ip_prefix: row.text("ip_prefix")?.unwrap_or_default(),
        user_agent: row.text("user_agent")?.unwrap_or_default(),
        created_at: row.time("created_at")?.unwrap_or_default(),
        url: row.text("url_path")?,
        title: row.text("page_title")?,
        referrer: row.text("referrer_domain")?,
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

/// One row of the **property table** — a property of an event.
pub struct PropertyRow {
    /// The event it belongs to.
    pub event_id: Uuid,
    /// The property itself.
    pub property: Property,
}

/// The row of the **property table** — one property of one event.
///
/// The value is in three columns — text, number, date — and the `data_type`
/// says which one holds it ⇒ ⇒ the value is read from that column and no
/// other, so a number stays a number and a date stays a date.
pub fn property_row(row: &dyn Row) -> Result<PropertyRow> {
    let key = row.text("data_key")?.unwrap_or_default();
    let value = match row.text("data_type")?.as_deref() {
        Some("number") => PropertyValue::Number(row.float("number_value")?.unwrap_or_default()),
        Some("date") => match row.time("date_value")? {
            Some(moment) => PropertyValue::Date(moment),
            None => PropertyValue::Text(row.text("string_value")?.unwrap_or_default()),
        },
        _ => PropertyValue::Text(row.text("string_value")?.unwrap_or_default()),
    };
    Ok(PropertyRow {
        event_id: row.event_id("event_id")?.unwrap_or_else(Uuid::nil),
        property: Property { key, value },
    })
}

/// The properties of the events, grouped by the event they belong to.
///
/// The keys are the event ids, the values are the properties **in the order
/// they were written** — the order the write path keeps them in, so the deep
/// read gives back what was put in.
pub fn group_properties(rows: Vec<PropertyRow>) -> HashMap<Uuid, Vec<Property>> {
    let mut out: HashMap<Uuid, Vec<Property>> = HashMap::new();
    for row in rows {
        out.entry(row.event_id).or_default().push(row.property);
    }
    out
}

/// The properties of an event, as the JSON the API shows.
///
/// **The shape is the one the old `data` column had**: a JSON **object**, the
/// keys are the property keys and the values are the property values — a
/// number is a number and not text.
///
/// ⇒ ⇒ The order of the properties does not matter: a JSON object is not
/// ordered, and `serde_json` writes the keys in the order it keeps them
/// (sorted, without the `preserve_order` feature) ⇒ ⇒ the two engines write
/// the same string for the same properties.
pub fn properties_json(properties: &[Property]) -> Option<String> {
    if properties.is_empty() {
        return None;
    }
    let mut map = serde_json::Map::new();
    for property in properties {
        let value = match &property.value {
            PropertyValue::Text(text) => serde_json::Value::String(text.clone()),
            PropertyValue::Number(number) => serde_json::Value::from(*number),
            // A date is a **string** — the same text the API serves its
            // timestamps in (see `chrono`'s own serialization of a moment).
            PropertyValue::Date(moment) => {
                serde_json::Value::String(moment.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
            }
        };
        map.insert(property.key.clone(), value);
    }
    Some(serde_json::Value::Object(map).to_string())
}

/// The columns of the **deep read** — the raw rows, field by field.
///
/// The statement that reads them is the shared one (see [`crate::query`]); the
/// list is here so that the three engines ask for the same columns in the
/// same order.
pub const EVENT_COLUMNS: &str = "event_id, website_id, visit_id, session_id, session_start, \
     ip, ip_prefix, user_agent, user_id, created_at, \
     url_path, url_query, page_title, \
     referrer_domain, referrer_path, referrer_query, referrer_name, hostname, channel, \
     browser, browser_version, os, os_version, device, screen, language, \
     country, region, city, latitude, longitude, \
     utm_source, utm_medium, utm_campaign, utm_content, utm_term, \
     gclid, fbclid, msclkid, ttclid, li_fat_id, twclid, \
     lcp, inp, cls, fcp, ttfb, \
     event_type, event_name, tag, ver";

/// The row of the **deep read** — a whole event.
///
/// The mapping is here, once: an engine that hands the rows back cannot get
/// the fields mixed up, because it does not know the field order — it only
/// says how a column of its own is fetched.
///
/// The **properties** are not in the fact table — they are rows of the table
/// of their own ⇒ ⇒ they are handed in, already read, because a row of the
/// fact table cannot read a row of another table.
pub fn hit_row(row: &dyn Row, properties: Vec<Property>) -> Result<Hit> {
    let event_type = row.text("event_type")?;
    Ok(Hit {
        website_id: row.text("website_id")?.unwrap_or_default(),
        visit_id: row.uuid("visit_id")?.unwrap_or_else(Uuid::nil),
        // The visitor — the column is `session_id`, the name Umami gives it.
        //
        // The column holds the **first eight bytes** of the visitor's hash, so
        // the id is put back into sixteen bytes (the rest are zero) — which is
        // enough for the reports, and the first eight bytes are what the id
        // is made of.
        visitor_id: row
            .blob("session_id")?
            .map(|bytes| {
                let mut full = [0u8; 16];
                let width = bytes.len().min(16);
                full[..width].copy_from_slice(&bytes[..width]);
                Uuid::from_bytes(full)
            })
            .unwrap_or_else(Uuid::nil),
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
        url: row.text("url_path")?,
        url_query: row.text("url_query")?,
        title: row.text("page_title")?,
        referrer: row.text("referrer_domain")?,
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
        lifatid: row.text("li_fat_id")?,
        twclid: row.text("twclid")?,
        lcp: row.float("lcp")?,
        inp: row.float("inp")?,
        cls: row.float("cls")?,
        fcp: row.float("fcp")?,
        ttfb: row.float("ttfb")?,
        name: row.text("event_name")?,
        tag: row.text("tag")?,
        // The properties are rows of the table of their own — the caller
        // read them and handed them in.
        properties,
        ver: row.text("ver")?,
    })
}
