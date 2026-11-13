//! The tracker and the server have to agree on the payload.
//!
//! The tracker is one plain JavaScript file and the server parses its JSON in
//! Rust. Nothing connects the two at compile time: if the tracker renames a
//! field, the server silently stores `NULL` and the column is gone — no error,
//! no warning, just an empty dashboard.
//!
//! So the two ends are checked against each other here: every field the server
//! reads must be a field the tracker sends.

use std::collections::BTreeSet;

/// The tracker source.
fn tracker() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("js/sumimi.js");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("reading the tracker {}: {err}", path.display()))
}

/// The dashboard source.
///
/// The dashboard lives in the repository, not in this crate — so the check
/// that the two agree is **skipped** where the dashboard is not there (the
/// published crate, for one).
fn dashboard() -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../js/sumimi-app.js");
    std::fs::read_to_string(path).ok()
}

/// The body of one function of the tracker.
fn function_body(source: &str, name: &str) -> String {
    let start = source
        .find(&format!("function {name}("))
        .unwrap_or_else(|| panic!("the tracker has no function {name}"));
    let mut depth = 0usize;
    for (index, character) in source[start..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return source[start..start + index].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("the function {name} is not closed");
}

/// The payload keys the tracker sends.
///
/// The keys of the object `basePayload` builds, plus the ones it assigns to it
/// afterwards.
fn tracker_keys() -> BTreeSet<String> {
    let source = tracker();
    let body = function_body(&source, "basePayload");
    let mut keys = BTreeSet::new();

    // The object literal: `website: …`, `url: …`, …
    for line in body.lines() {
        let line = line.trim();
        if let Some((key, _)) = line.split_once(':') {
            let key = key.trim();
            if !key.is_empty()
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !key.chars().next().unwrap().is_ascii_digit()
            {
                keys.insert(key.to_string());
            }
        }
    }

    // The fields assigned after the literal: `payload.lcp = …`, and the ones
    // behind a condition: `if (tag) payload.tag = tag;`.
    for name in ["basePayload", "track", "identify", "vitalsPayload"] {
        let body = function_body(&source, name);
        for line in body.lines() {
            for part in line.split("payload.").skip(1) {
                let key: String = part
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !key.is_empty() {
                    keys.insert(key);
                }
            }
        }
    }

    keys
}

/// The payload fields the server reads out of a hit.
///
/// These are the fields `build_hit` looks up. A field that is not in the
/// tracker's payload is stored as `NULL` — silently.
fn server_fields() -> &'static [&'static str] {
    &[
        "website",
        "url",
        "referrer",
        "title",
        "hostname",
        "language",
        "screen",
        "name",
        "tag",
        "data",
        "lcp",
        "inp",
        "cls",
        "fcp",
        "ttfb",
    ]
}

/// The tracker sends every field the server reads.
#[test]
fn the_tracker_sends_every_field_the_server_reads() {
    let keys = tracker_keys();
    let mut missing: Vec<&str> = Vec::new();
    for field in server_fields() {
        if !keys.contains(*field) {
            missing.push(field);
        }
    }
    assert!(
        missing.is_empty(),
        "the tracker does not send {} — the server would store NULL for it",
        missing.join(", ")
    );
}

/// The server reads every field the tracker sends.
///
/// The other way round: a field the tracker sends and the server ignores is a
/// field somebody spent time on for nothing.
#[test]
fn the_server_reads_every_field_the_tracker_sends() {
    let keys = tracker_keys();
    // The fields that are not part of the payload: the code around it.
    let not_payload = [
        "www", "http", "https", "true", "false", "null", "undefined", "utf",
    ];
    let mut ignored: Vec<String> = Vec::new();
    for key in &keys {
        if server_fields().contains(&key.as_str()) {
            continue;
        }
        if not_payload.contains(&key.as_str()) {
            continue;
        }
        ignored.push(key.clone());
    }
    assert!(
        ignored.is_empty(),
        "the tracker sends {} but the server never reads them",
        ignored.join(", ")
    );
}

/// The web vitals are document-level: they are sent **once per document**, as
/// a hit of their own, so a route change cannot skew an average.
#[test]
fn the_vitals_go_out_once_per_document() {
    let source = tracker();

    // They are a hit of their own — the same shape Umami uses.
    assert!(
        source.contains("send('performance', vitalsPayload(), true);"),
        "the web vitals must be sent as a hit of their own"
    );
    // Once per document.
    assert!(
        source.contains("var vitalsSent = false;"),
        "the web vitals must be sent once"
    );
    assert!(
        source.contains("if (vitalsSent) return;"),
        "the web vitals must not be sent twice"
    );
}

/// The page view goes out as soon as the page is there: the answer carries the
/// token, and the token is what keeps the next hit in the same session.
#[test]
fn the_page_view_goes_out_at_once() {
    let source = tracker();

    assert!(
        source.contains("sendPageview();"),
        "the page view must be sent when the page is there"
    );
    // A beacon brings no answer back, so the page view does not use one.
    assert!(
        source.contains("function send(type, payload, beacon)"),
        "the send must be able to choose between the two"
    );
    assert!(
        source.contains("if (beacon && navigatorRef.sendBeacon)"),
        "only the hits that do not need an answer may use a beacon"
    );
}

/// The client carries the token the server gave it.
#[test]
fn the_client_carries_the_token() {
    let source = tracker();

    // It is kept in a variable — not in a cookie, not in localStorage.
    assert!(
        source.contains("var cache = null;"),
        "the token must live in a variable of the tracker"
    );
    assert!(
        !source.contains("localStorage.setItem") && !source.contains("localStorage.getItem"),
        "the token must not be written to localStorage"
    );
    assert!(
        !source.contains("document.cookie"),
        "the token must not be written to a cookie"
    );
    // It goes out with every hit, and the answer replaces it.
    assert!(
        source.contains("headers['x-sumimi-cache'] = cache;"),
        "the token must be sent with every hit"
    );
    assert!(
        source.contains("if (data && data.cache) cache = data.cache;"),
        "the token of the answer must replace the one we have"
    );
}

/// **The site owner decides** whether the visitor's signal is honoured.
///
/// The tracker sends by default; `data-do-not-track="true"` asks it to honour
/// what the visitor says. We do not decide for the site owner (D-040) — and
/// Umami does the same.
#[test]
fn the_site_owner_decides_about_dnt() {
    let source = tracker();

    assert!(
        source.contains("data-do-not-track"),
        "the tracker must let the site owner ask for the visitor's signal to be honoured"
    );
    assert!(
        source.contains("if (script.getAttribute('data-do-not-track') === 'true') {"),
        "the visitor's signal must only be read when the owner asks for it"
    );
}

/// The kinds of marked traffic the dashboard offers and the kinds the server
/// marks must be the same list.
///
/// The dashboard is a separate file from the server: if one of them learns a
/// kind and the other does not, the switch quietly does nothing.
#[test]
fn the_dashboard_knows_the_kinds_the_server_marks() {
    let Some(source) = dashboard() else {
        // No dashboard here (the published crate): nothing to compare.
        return;
    };
    let start = source
        .find("var MARKER_GROUPS = [")
        .expect("the dashboard lists the kinds of marked traffic");
    let end = start + source[start..]
        .find("];")
        .expect("the list is closed")
        + 2;
    let block = &source[start..end];

    let kinds: Vec<&str> = block
        .lines()
        .filter_map(|line| line.split("kind: '").nth(1))
        .filter_map(|rest| rest.split('\'').next())
        .collect();

    // The server's names: the groups and the kinds in them.
    let mut theirs: Vec<&str> = Vec::new();
    for (group, kinds) in sumimi_service::storage::MARKER_GROUPS.iter() {
        theirs.push(*group);
        theirs.extend(kinds.iter().copied());
    }

    let mut ours = kinds.clone();
    ours.sort();
    theirs.sort();
    assert_eq!(
        ours, theirs,
        "the dashboard and the server must know the same kinds"
    );
}
