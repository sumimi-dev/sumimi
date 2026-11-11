//! Collection endpoints: `POST /api/send` (Umami v2) and `POST /api/collect` (legacy).

use axum::extract::{ConnectInfo, Extension, Query, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::net::SocketAddr;

use crate::api::AppState;
use crate::error::ApiError;
use crate::models::{normalise, Hit, HitKind, Payload, SendRequest};
use crate::ua;

/// Umami v2 collection endpoint.
pub async fn send(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    body: Result<Json<SendRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(request) = body.map_err(|rejection| {
        // A body that is too large is not a malformed body: it is a `413`.
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::PayloadTooLarge
        } else {
            ApiError::BadRequest("invalid json body".into())
        }
    })?;
    let mut raw = request.into_payload();

    // The website comes out of the payload — **moved**, not copied ✓.
    let website_key = raw
        .website
        .take()
        .ok_or_else(|| ApiError::BadRequest("missing website".into()))?;

    // Umami v2 sends pageviews as `type: "event"` *without* a name; a custom event
    // carries a name. Both shapes are accepted here.
    let kind = resolve_kind(raw.kind.as_deref(), raw.name.is_some());

    let hit = build_hit(
        &state,
        &headers,
        peer.as_ref().map(|p| &p.0),
        &website_key,
        kind,
        raw,
    )?;
    let taken = state.writer.push(hit);

    // The server is over capacity: the event was **not** stored. The client is
    // told — a `200` here would say "everything is fine" while the data is
    // being lost. `503` with a `Retry-After` is the honest answer: send it
    // again in a moment.
    if !taken {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::RETRY_AFTER, "1")],
        )
            .into_response());
    }

    Ok(StatusCode::OK.into_response())
}

/// Legacy Umami v1 collection endpoint (query parameters).
pub async fn collect(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Query(mut params): Query<CollectParams>,
) -> Result<Response, ApiError> {
    let website_key = params
        .id
        .take()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::BadRequest("missing website id".into()))?;

    let kind = resolve_kind(params.r#type.as_deref(), params.name.is_some());

    // The fields go straight into the payload ✓ ⇒ ⇒ no map is built, and no
    // field is copied twice ✓ ⇒ ⇒ the same rule as the v2 endpoint: trimmed
    // and empty is nothing ✓.
    let raw = Payload {
        url: normalise(params.url),
        referrer: normalise(params.referrer),
        name: normalise(params.name),
        hostname: normalise(params.hostname),
        language: normalise(params.language),
        screen: normalise(params.screen),
        title: normalise(params.title),
        session_id: normalise(params.session_id),
        // Legacy trackers send `data` as base64-encoded JSON.
        data: params
            .data
            .and_then(|data| base64_decode(&data).ok())
            .and_then(|decoded| serde_json::from_slice::<serde_json::Value>(&decoded).ok()),
        kind: Some(kind_name(kind).to_string()),
        ..Default::default()
    };

    let hit = build_hit(
        &state,
        &headers,
        peer.as_ref().map(|p| &p.0),
        &website_key,
        kind,
        raw,
    )?;
    let taken = state.writer.push(hit);

    // The server is over capacity: the event was **not** stored. The client is
    // told — a `200` here would say "everything is fine" while the data is
    // being lost. `503` with a `Retry-After` is the honest answer: send it
    // again in a moment.
    if !taken {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::RETRY_AFTER, "1")],
        )
            .into_response());
    }

    Ok(StatusCode::OK.into_response())
}

/// Query parameters accepted by the legacy endpoint.
#[derive(Debug, Deserialize)]
pub struct CollectParams {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub referrer: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub screen: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub data: Option<String>,
}

/// Decide what kind of hit this is.
///
/// `type: "event"` without a name is a pageview (that is how the Umami v2 tracker
/// sends them); with a name it is a custom event.
fn resolve_kind(kind: Option<&str>, has_name: bool) -> HitKind {
    match kind {
        Some("screenview") => HitKind::Screenview,
        Some("identify") => HitKind::Identify,
        Some("event") | None => {
            if has_name {
                HitKind::Event
            } else {
                HitKind::Pageview
            }
        }
        Some(_) => HitKind::Pageview,
    }
}

/// The name of a hit kind.
fn kind_name(kind: HitKind) -> &'static str {
    match kind {
        HitKind::Pageview => "pageview",
        HitKind::Event => "event",
        HitKind::Screenview => "screenview",
        HitKind::Identify => "identify",
    }
}

/// Turn a request into a [`Hit`].
///
/// The field set mirrors Umami's: the page and its query string, the referrer (host,
/// path and query), the marketing parameters, the ad click ids and the web vitals all
/// come out of the payload.
/// Build the hit a collection request describes.
///
/// **The payload is typed** ✓ ⇒ ⇒ every field is already trimmed and already
/// owned by the payload ✓ ⇒ ⇒ ⇒ and this function only has to **move** the
/// ones it keeps ✓ ⇒ ⇒ ⇒ not one copy ✓ (see `MEMORY.md` §6 ✓).
///
/// **Public for the measurement** ✓ — `service/tests/allocations.rs` prices
/// this stage against the others ✓ (the collection path is where a request's
/// memory goes ✓ — see `MEMORY.md` §6 ✓).
pub fn build_hit(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<&ConnectInfo<SocketAddr>>,
    website_key: &str,
    kind: HitKind,
    mut raw: Payload,
) -> Result<Hit, ApiError> {
    let meta = state.website_meta(website_key)?;

    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let info = ua::classify(&user_agent);

    let ip = client_ip(headers, peer);
    let ip_prefix = ua::ip_prefix(&ip);

    // Visitor id: the derivation without a time bucket, so it survives across
    // days (Umami's `distinct_id`). The raw IP is never stored — only a prefix
    // feeds the HMAC.
    let visitor_id = ua::derive_visit_id(
        state.jwt_secret.as_bytes(),
        &ip_prefix,
        &user_agent,
        &meta.id,
        0,
    );

    // The session: **the client's own id if it sent one**, otherwise the one
    // the server works out.
    //
    // The client's token is never stored verbatim — it is hashed into the id,
    // so the column is always the same size and the raw token never reaches
    // the disk. What the server sends is what the client will keep sending,
    // so the session lasts as long as the client says it does.
    //
    // When the client sends nothing, the server decides: the cut is made here,
    // when the event arrives, so the row carries the session it belongs to and
    // no report has to work out where the gaps were. The id is a pure function
    // of the visitor and the moment the session started — no secret is needed,
    // because the visitor id is already one-way, so an offline import can work
    // it out for itself.
    let created_at = crate::db::now();
    let visit_id = match raw.session_id.as_deref() {
        Some(client_id) if is_safe_client_id(client_id) => ua::uuid_from_text(client_id),
        _ => state
            .sessions
            .session(visitor_id, created_at.and_utc().timestamp_micros()),
    };

    // The identity the site owner knows the visitor by — their own user id, a
    // tenant id, a device id (Umami's `distinct_id` column). The client sends it
    // as `id`; we hash it, so the raw token never reaches the disk.
    //
    // Mixing it into the visitor id (so an identified visitor is not merged with
    // the anonymous ones) is the next step — see sumimi#39.
    let user_id = match raw.id.as_deref() {
        Some(client_id) if is_safe_client_id(client_id) => Some(ua::uuid_from_text(client_id)),
        _ => None,
    };

    let created_at = crate::db::now();

    // The custom event's data — moved out of the payload ✓ (it is taken
    // first, because the fields below are moved out one by one ✓).
    let data = raw.take_data();

    // The page: path and query, kept apart like Umami keeps them.
    let (url, url_query, params) = match raw.url {
        Some(url) => split_url(url),
        None => (None, None, Vec::new()),
    };
    // The query string is stored by default — Umami stores it, and it is where the
    // marketing parameters live. `SUMIMI_KEEP_QUERY=0` keeps the parameters but
    // drops the raw query string.
    let url_query = if state.config.keep_query { url_query } else { None };
    let param = |name: &str| -> Option<&str> {
        params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };

    // The referrer: host, path and query. The breakdown groups by the host, which
    // is what Umami shows; the path and the query are the extra we keep.
    // `SUMIMI_STORE_FULL_REFERRER=0` keeps the host alone.
    let (referrer, referrer_path, referrer_query) = match raw.referrer {
        Some(referrer) => {
            let (host, path, query) = split_referrer(referrer);
            // The `www.` is dropped, the way the reference implementation drops
            // it: `www.example.com` and `example.com` are the same site, and
            // the referrer breakdown has to show one name for them.
            let host = host.map(|host| {
                let host = crate::channels::strip_www(&host);
                host.to_string()
            });
            if state.config.store_full_referrer {
                (host, path, query)
            } else {
                (host, None, None)
            }
        }
        None => (None, None, None),
    };

    // The display name of the referrer ("Hacker News"), when it has one.
    let referrer_name = crate::channels::referrer_name(referrer.as_deref());

    // The country: the header first — a proxy in front of the server knows it
    // exactly — and the resolver second.
    let country_header = meta
        .country_header
        .clone()
        .or_else(|| state.config.country_header.clone());
    let mut country = country_header
        .as_ref()
        .and_then(|header| headers.get(header.as_str()))
        .and_then(|v| v.to_str().ok())
        .and_then(crate::geo::country_from_header);
    let mut region = raw.region;
    let mut city = raw.city;
    let mut latitude = None;
    let mut longitude = None;

    if country.is_none() {
        let location = state.geo.as_ref().and_then(|resolver| {
            ip.parse::<std::net::IpAddr>()
                .ok()
                .and_then(|address| resolver.lookup(address))
        });
        if let Some(location) = location {
            country = location.country;
            if region.is_none() {
                region = location.region;
            }
            if city.is_none() {
                city = location.city;
            }
            latitude = location.latitude;
            longitude = location.longitude;
        }
    }

    // The channel: the bucket the visit falls into. It is worked out once,
    // here, and stored with the event.
    let hostname = raw.hostname;
    let channel = Some(
        crate::channels::channel(&crate::channels::ChannelInput::new(
            referrer.as_deref(),
            url_query.as_deref(),
            param("utm_medium"),
            param("utm_source"),
            hostname.as_deref(),
        ))
        .to_string(),
    );

    // The marketing parameters and the ad click ids: **one pass** over the
    // query, and the values are **moved** out of it — not copied out one by
    // one ✓.
    let mut utm_source = None;
    let mut utm_medium = None;
    let mut utm_campaign = None;
    let mut utm_content = None;
    let mut utm_term = None;
    let mut gclid = None;
    let mut fbclid = None;
    let mut msclkid = None;
    let mut ttclid = None;
    let mut lifatid = None;
    let mut twclid = None;
    for (key, value) in params {
        match key.as_str() {
            "utm_source" => utm_source = Some(value),
            "utm_medium" => utm_medium = Some(value),
            "utm_campaign" => utm_campaign = Some(value),
            "utm_content" => utm_content = Some(value),
            "utm_term" => utm_term = Some(value),
            "gclid" => gclid = Some(value),
            "fbclid" => fbclid = Some(value),
            "msclkid" => msclkid = Some(value),
            "ttclid" => ttclid = Some(value),
            "lifatid" => lifatid = Some(value),
            "twclid" => twclid = Some(value),
            _ => {}
        }
    }

    Ok(Hit {
        website_id: meta.id,
        visit_id,
        visitor_id,
        user_id,
        retention_days: meta.retention_days,
        kind,
        created_at,

        url,
        url_query,
        title: if state.config.store_titles { raw.title } else { None },

        referrer,
        referrer_path,
        referrer_query,
        referrer_name,
        hostname,
        channel,

        browser: info.browser,
        browser_version: info.browser_version,
        os: info.os,
        os_version: info.os_version,
        device: info.device,
        screen: raw.screen,
        language: raw.language,
        country,
        region,
        city,
        latitude,
        longitude,

        utm_source,
        utm_medium,
        utm_campaign,
        utm_content,
        utm_term,

        gclid,
        fbclid,
        msclkid,
        ttclid,
        lifatid,
        twclid,

        lcp: raw.lcp,
        inp: raw.inp,
        cls: raw.cls,
        fcp: raw.fcp,
        ttfb: raw.ttfb,

        name: raw.name,
        tag: raw.tag,
        data,
    })
}

/// Split a URL into its path, its query string and its parameters.
///
/// **The path is moved, not copied** ✓: a URL without a query and without a
/// fragment *is* its path ⇒ ⇒ ⇒ so the string the tracker sent becomes the
/// string the event carries ✓ ⇒ ⇒ ⇒ no copy ✓ (see `MEMORY.md` §6 ✓).
fn split_url(url: String) -> (Option<String>, Option<String>, Vec<(String, String)>) {
    let without_fragment = match url.split_once('#') {
        Some((before, _)) => before,
        None => url.as_str(),
    };
    match without_fragment.split_once('?') {
        Some((path, query)) => (
            Some(path.to_string()),
            Some(query.to_string()),
            parse_query(query),
        ),
        // No query and no fragment: the URL **is** the path ✓ ⇒ ⇒ ⇒ move it ✓.
        None if without_fragment.len() == url.len() => (Some(url), None, Vec::new()),
        None => (Some(without_fragment.to_string()), None, Vec::new()),
    }
}

/// Split a referrer into its host, path and query.
fn split_referrer(referrer: String) -> (Option<String>, Option<String>, Option<String>) {
    let without_scheme = referrer
        .strip_prefix("https://")
        .or_else(|| referrer.strip_prefix("http://"))
        .unwrap_or(&referrer);

    let (host, rest) = match without_scheme.split_once('/') {
        Some((host, rest)) => (host, Some(rest)),
        None => (without_scheme, None),
    };

    let (path, query) = match rest {
        Some(rest) => match rest.split_once('?') {
            Some((path, query)) => (Some(path.to_string()), Some(query.to_string())),
            None => (Some(rest.to_string()), None),
        },
        None => (None, None),
    };

    (
        Some(host.to_string()),
        path,
        query,
    )
}

/// Parse a query string into its parameters.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((key, value)) => (decode_component(key), decode_component(value)),
            None => (decode_component(part), String::new()),
        })
        .collect()
}

/// Decode a percent-encoded query component.
fn decode_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The client IP: prefer the proxy header, fall back to the socket address.
fn client_ip(headers: &HeaderMap, peer: Option<&ConnectInfo<SocketAddr>>) -> String {
    for header in ["x-forwarded-for", "x-real-ip", "cf-connecting-ip"] {
        if let Some(value) = headers.get(header).and_then(|v| v.to_str().ok()) {
            if let Some(first) = value.split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }
    match peer {
        Some(ConnectInfo(addr)) => addr.ip().to_string(),
        None => "0.0.0.0".to_string(),
    }
}

/// Accept only ids that look like a UUID or a simple token (never user input verbatim).
fn is_safe_client_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Decode standard base64 (no external dependency).
fn base64_decode(input: &str) -> Result<Vec<u8>, ()> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, c) in TABLE.iter().enumerate() {
        lookup[*c as usize] = i as u8;
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;

    for byte in input.bytes() {
        if byte == b'=' || byte == b'\n' || byte == b'\r' {
            continue;
        }
        let value = lookup[byte as usize];
        if value == 255 {
            return Err(());
        }
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_page_into_a_path_and_a_query() {
        // The query string is stored by default — Umami stores it, and it is where
        // the marketing parameters live. `SUMIMI_KEEP_QUERY=0` drops it later.
        let (url, query, params) = split_url("/a/b?token=secret&utm_source=twitter".to_string());
        assert_eq!(url.as_deref(), Some("/a/b"));
        assert_eq!(query.as_deref(), Some("token=secret&utm_source=twitter"));
        assert_eq!(
            params,
            vec![
                ("token".to_string(), "secret".to_string()),
                ("utm_source".to_string(), "twitter".to_string())
            ]
        );

        let (url, query, params) = split_url("/a/b".to_string());
        assert_eq!(url.as_deref(), Some("/a/b"));
        assert_eq!(query, None);
        assert!(params.is_empty());
    }

    #[test]
    fn rejects_unsafe_client_ids() {
        assert!(is_safe_client_id("0f8fad5b-d9cb-469f-a165-70867728950e"));
        assert!(!is_safe_client_id("bad id; DROP TABLE"));
        assert!(!is_safe_client_id(""));
    }

    #[test]
    fn decodes_base64() {
        let decoded = base64_decode("eyJhIjoxfQ==").unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "{\"a\":1}");
    }
}
