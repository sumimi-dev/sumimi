//! Reading every stored row back, field by field.
//!
//! The [`Storage`](crate::storage::Storage) trait exposes the *reports* —
//! the numbers the dashboard shows. That is what a backend has to get right, and
//! the conformance suite checks all of it. But a backend can pass every report and
//! still drop a column on the floor: nobody would ever notice, because no report
//! reads it.
//!
//! This module is the other half: a backend that can also hand back the raw rows,
//! so the suite can compare **every one of the 42 fields** against what was
//! written. It is the "no data loss" check, per backend.

use crate::models::Hit;

/// One stored row.
#[derive(Debug, Clone)]
pub struct StoredRow {
    /// The row id the backend assigned — **the text of the uuid**.
    pub id: String,
    /// The hit, as the backend read it back.
    pub hit: Hit,
}

/// A backend that can hand back every row it stored.
pub trait DeepRead {
    /// Every row of a website, in insertion order.
    fn rows(&self, website_id: &str) -> anyhow::Result<Vec<StoredRow>>;
}

/// Compare two hits field by field and return the differences.
///
/// An empty vector means the two are the same. The comparison is exact: `None` and
/// `Some("")` are **not** the same, and a backend that turns one into the other
/// has lost information.
pub fn diff(a: &Hit, b: &Hit) -> Vec<String> {
    let mut out = Vec::new();
    macro_rules! check {
        ($field:expr, $left:expr, $right:expr $(,)?) => {
            if $left != $right {
                out.push(format!("{}: {:?} != {:?}", $field, $left, $right));
            }
        };
    }

    check!("website_id", &Some(a.website_id.clone()), &Some(b.website_id.clone()));
    check!(
        "visit_id",
        &Some(crate::session::format_visit_id(a.visit_id)),
        &Some(crate::session::format_visit_id(b.visit_id)),
    );
    check!(
        "visitor_id",
        // The column holds the **first eight bytes** of the visitor's hash
        // (see `crate::ua::visitor_bytes`), so the id that comes back is the
        // truncated one ⇒ ⇒ the two are compared in the form they are stored.
        &Some(crate::ua::format_visitor_id(&crate::ua::visitor_bytes(
            &a.visitor_id
        ))),
        &Some(crate::ua::format_visitor_id(&crate::ua::visitor_bytes(
            &b.visitor_id
        ))),
    );
    check!(
        "user_id",
        &a.user_id.map(|id| id.to_string()),
        &b.user_id.map(|id| id.to_string()),
    );
    if a.session_start != b.session_start {
        out.push(format!(
            "session_start: {:?} != {:?}",
            a.session_start, b.session_start
        ));
    }
    if a.kind != b.kind {
        out.push(format!("kind: {:?} != {:?}", a.kind, b.kind));
    }
    check!("ip", &Some(a.ip.clone()), &Some(b.ip.clone()));
    check!("ip_prefix", &Some(a.ip_prefix.clone()), &Some(b.ip_prefix.clone()));
    check!("user_agent", &Some(a.user_agent.clone()), &Some(b.user_agent.clone()));
    check!("url", &a.url, &b.url);
    check!("url_query", &a.url_query, &b.url_query);
    check!("title", &a.title, &b.title);
    check!("referrer", &a.referrer, &b.referrer);
    check!("referrer_path", &a.referrer_path, &b.referrer_path);
    check!("referrer_query", &a.referrer_query, &b.referrer_query);
    check!("referrer_name", &a.referrer_name, &b.referrer_name);
    check!("hostname", &a.hostname, &b.hostname);
    check!("channel", &a.channel, &b.channel);
    check!("name", &a.name, &b.name);
    check!("browser", &a.browser, &b.browser);
    check!("browser_version", &a.browser_version, &b.browser_version);
    check!("os", &a.os, &b.os);
    check!("os_version", &a.os_version, &b.os_version);
    check!("device", &a.device, &b.device);
    check!("screen", &a.screen, &b.screen);
    check!("language", &a.language, &b.language);
    check!("country", &a.country, &b.country);
    check!("region", &a.region, &b.region);
    check!("city", &a.city, &b.city);
    check!("utm_source", &a.utm_source, &b.utm_source);
    check!("utm_medium", &a.utm_medium, &b.utm_medium);
    check!("utm_campaign", &a.utm_campaign, &b.utm_campaign);
    check!("utm_content", &a.utm_content, &b.utm_content);
    check!("utm_term", &a.utm_term, &b.utm_term);
    check!("gclid", &a.gclid, &b.gclid);
    check!("fbclid", &a.fbclid, &b.fbclid);
    check!("msclkid", &a.msclkid, &b.msclkid);
    check!("ttclid", &a.ttclid, &b.ttclid);
    check!("lifatid", &a.lifatid, &b.lifatid);
    check!("twclid", &a.twclid, &b.twclid);
    check!("tag", &a.tag, &b.tag);
    // The properties are the rows of the table of their own — the deep read
    // reads them back with the event.
    if a.properties != b.properties {
        out.push(format!("properties: {:?} != {:?}", a.properties, b.properties));
    }

    if a.created_at != b.created_at {
        out.push(format!("created_at: {} != {}", a.created_at, b.created_at));
    }
    for (field, left, right) in [
        ("lcp", a.lcp, b.lcp),
        ("inp", a.inp, b.inp),
        ("cls", a.cls, b.cls),
        ("fcp", a.fcp, b.fcp),
        ("ttfb", a.ttfb, b.ttfb),
        ("latitude", a.latitude, b.latitude),
        ("longitude", a.longitude, b.longitude),
    ] {
        if left != right {
            out.push(format!("{field}: {left:?} != {right:?}"));
        }
    }

    out
}
