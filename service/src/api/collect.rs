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
use crate::token;
use crate::ua;

/// The headers the client carries its state in.
///
/// **Ours first, Umami's second.** Our own tracker sends `x-sumimi-*`; the
/// Umami SDK sends `x-umami-*`, and it is meant to work against us as it is —
/// so both are read, and ours wins when a client sends both.
const CACHE_HEADERS: [&str; 2] = ["x-sumimi-cache", "x-umami-cache"];
const WEBSITE_HEADERS: [&str; 2] = ["x-sumimi-website-id", "x-umami-website-id"];
const HOSTNAME_HEADERS: [&str; 2] = ["x-sumimi-hostname", "x-umami-hostname"];

/// The kind of the developer's own traffic.
pub const DEVELOPER_TAG: &str = "dev";

/// The kind of the traffic sent under `tester`.
pub const TEST_TAG: &str = "test";

/// The ids that mean "this is not a real visitor", and the kind each one is.
///
/// A hit sent under one of them is **marked** with its kind, so it is left out
/// of the numbers — the developer's own clicks would otherwise be the busiest
/// visitor of the site (sumimi#71).
pub const DEVELOPER_IDS: [(&str, &str); 4] = [
    ("developer", DEVELOPER_TAG),
    ("manager", DEVELOPER_TAG),
    ("owner", DEVELOPER_TAG),
    ("tester", TEST_TAG),
];

/// Read a header, ours first and Umami's second.
fn header<'a>(headers: &'a HeaderMap, names: [&str; 2]) -> Option<&'a str> {
    for name in names {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// The body the collection endpoint answers with.
///
/// **Umami's shape, our ids.** The Umami SDK reads `cache` and nothing else —
/// so a client that only knows Umami works against us — while the other two
/// are there for whoever asks.
///
/// Note the naming: `sessionId` is Umami's name for the **visitor** (their
/// "session" is our visitor; their "visit" is our session). The names are
/// kept as they are so the two are the same thing on the wire.
#[derive(Debug, serde::Serialize)]
pub struct SendResponse {
    /// The token the client sends back with the next hit.
    pub cache: String,
    /// The visitor's id.
    #[serde(rename = "sessionId")]
    pub visitor_id: String,
    /// The session's id.
    #[serde(rename = "visitId")]
    pub visit_id: String,
}

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

    // The website comes out of the payload — **moved**, not copied ✓. The
    // header is the fallback: the Umami SDK puts the id in the payload, but a
    // client that only sends the header has to work too.
    let website_key = raw
        .website
        .take()
        .or_else(|| header(&headers, WEBSITE_HEADERS).map(str::to_string))
        .ok_or_else(|| ApiError::BadRequest("missing website".into()))?;

    // Umami v2 sends pageviews as `type: "event"` *without* a name; a custom event
    // carries a name. Both shapes are accepted here.
    let kind = resolve_kind(raw.kind.as_deref(), raw.name.is_some());


    // The token the client carries: the state of the session it is in. **A
    // token that cannot be read is the same as no token at all** — the hit
    // then starts a session, exactly as it did before.
    let claims = header(&headers, CACHE_HEADERS)
        .and_then(|token| token::verify(&state.jwt_secret, token));

    let (hit, claims) = build_hit(
        &state,
        &headers,
        peer.as_ref().map(|p| &p.0),
        &website_key,
        kind,
        raw,
        claims.as_ref(),
    )?;

    // An error report is not an event: it is not stored and it does not count
    // as one. The token still goes back, so the client keeps its session.
    if kind != HitKind::Error {
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
    }

    // The token the client sends back with the next hit. It is signed, not
    // encrypted: the client can read it, it just cannot change it.
    let cache = token::sign(&state.jwt_secret, &claims).map_err(ApiError::internal)?;

    Ok(Json(SendResponse {
        cache,
        visitor_id: claims.visitor_id,
        visit_id: claims.visit_id,
    })
    .into_response())
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
        None,
    )?
    .0;
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
        // Umami sends the web vitals as a hit of their own.
        Some("performance") => HitKind::Performance,
        // Umami sends the errors of a page to the same endpoint.
        Some("error") => HitKind::Error,
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
        HitKind::Performance => "performance",
        HitKind::Error => "error",
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
    token: Option<&token::Claims>,
) -> Result<(Hit, token::Claims), ApiError> {
    let meta = state.website_meta(website_key)?;

    // **The client may say who the visitor is** — Umami's rule. A server-side
    // import knows the visitor's address and browser better than the server
    // does: the server only sees the importer.
    let user_agent = match raw.user_agent.as_deref() {
        Some(agent) if !agent.is_empty() => agent.to_string(),
        _ => headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
    };
    let info = ua::classify(&user_agent);

    let ip = match raw.ip.as_deref() {
        Some(ip) if !ip.is_empty() => ip.to_string(),
        _ => client_ip(headers, peer),
    };
    let ip_prefix = ua::ip_prefix(&ip);

    // The visitor: what the session is cut for. It is a hash of the visitor's
    // address (the whole IPv4 one, the /64 of an IPv6 one), the user agent
    // and the website — **sixteen bytes**, and pure: the same visitor gets the
    // same id on every server.
    let visitor_id = ua::visitor_id(&ua::visitor_address(&ip), &user_agent, &meta.id);

    let clock = crate::db::now();

    // When the event happened. A client may say when it was — for the events
    // that are imported after the fact — but not a time that is not there: the
    // clock is ours, and a time in the future is not a time yet.
    let created_at = match raw.timestamp {
        Some(seconds) => chrono::DateTime::from_timestamp(seconds, 0)
            .map(|moment| moment.naive_utc())
            .filter(|moment| {
                *moment >= chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc()
                    && *moment <= clock + chrono::Duration::minutes(5)
            })
            .unwrap_or(clock),
        None => clock,
    };
    let at = created_at.and_utc().timestamp_micros();
    let now = created_at.and_utc().timestamp();

    // The session: **the client's own token first**, then the client's own id
    // (the older way), and only then the one the server works out.
    //
    // The token is the state: it says which session the client is in, when the
    // session started and when the client was last seen — so the server
    // applies the idle rule to it and keeps no state of its. It is used only
    // when it belongs to this website and to this visitor — a token of
    // somebody else's is not used.
    //
    // The client's own id is never stored verbatim — it is hashed into the id,
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
    let ours = token.filter(|claims| {
        claims.website_id == meta.id && claims.visitor() == Some(visitor_id)
    });
    let (visit_id, session_start) = match ours {
        // The token is ours and the visitor is still active: the session goes
        // on — and it keeps the start it had.
        Some(claims) if now - claims.last <= crate::session::IDLE_TIMEOUT_SECONDS => (
            claims
                .session()
                .unwrap_or_else(|| crate::session::session_id(visitor_id, at)),
            claims.started_at(),
        ),
        // The token is ours, but the visitor has been quiet for longer than the
        // timeout: a new session starts here.
        Some(_) => (
            crate::session::session_id(visitor_id, at),
            Some(created_at),
        ),
        // The older way: the client named the session itself. There is no start
        // to remember — the duration is measured from the first event instead.
        None => match raw.session_id.as_deref() {
            Some(client_id) if is_safe_client_id(client_id) => {
                (crate::session::client_session_id(client_id), None)
            }
            _ => (
                state.sessions.session(visitor_id, at),
                Some(created_at),
            ),
        },
    };

    // The token the client gets back: the session it is in, when it started,
    // and the moment of this hit.
    let claims = token::Claims::new(
        &meta.id,
        visitor_id,
        visit_id,
        session_start
            .map(|start| start.and_utc().timestamp())
            .unwrap_or(now),
        now,
    );

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

    // **The marker.** A tag marks the traffic that is not part of the numbers:
    // the bots, the developer's own traffic, and whatever else the site owner
    // tags. The dashboard can ask for it (`include_tagged`).
    //
    // - a bot carries `bot` — Umami throws its hits away, we keep them and
    //   leave them out of the numbers;
    // - the developer's own traffic carries the id it was sent under
    //   (`developer`, `tester`, `manager`, `owner` — sumimi#71).
    let tag = match raw.id.as_deref() {
        Some(id) => DEVELOPER_IDS
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, kind)| kind.to_string()),
        None => None,
    };
    let tag = match tag {
        Some(tag) => Some(tag),
        None => {
            let agent = raw
                .user_agent
                .as_deref()
                .or_else(|| {
                    headers
                        .get(axum::http::header::USER_AGENT)
                        .and_then(|v| v.to_str().ok())
                })
                .unwrap_or("");
            // A crawler: marked with its kind (`bot`, `bot-ai`, `bot-google`, …).
            ua::bot_kind(agent).map(str::to_string)
        }
    };

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

    // The hostname of the site. It comes out of the payload; the header is the
    // fallback, for the clients that only send that.
    let hostname = raw
        .hostname
        .or_else(|| header(headers, HOSTNAME_HEADERS).map(str::to_string));

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
            // **A site is not a referrer to itself**: Umami never saves the
            // domain of a self-referral, so the traffic of a site does not
            // show up in its own referrer breakdown.
            let self_referral = match (&host, &hostname) {
                (Some(host), Some(hostname)) => {
                    let hostname = crate::channels::strip_www(hostname);
                    host.eq_ignore_ascii_case(hostname)
                }
                _ => false,
            };
            let host = if self_referral { None } else { host };
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
            // Umami's name for it: `li_fat_id`.
            "li_fat_id" | "lifatid" => lifatid = Some(value),
            "twclid" => twclid = Some(value),
            _ => {}
        }
    }

    Ok((Hit {
        website_id: meta.id,
        visit_id,
        visitor_id,
        session_start,
        // **The raw address and the raw agent are not stored by default.**
        //
        // What is stored is the *reading* of them — the country, the region,
        // the city, the browser, the operating system, the device — and the
        // visitor's id, which is the hash of the address and the agent. The
        // raw strings are not needed for any of that.
        //
        // `SUMIMI_STORE_RAW=1` stores them anyway, for the owner who wants
        // the full set of dimensions.
        ip: if state.config.store_raw { ip } else { String::new() },
        ip_prefix: if state.config.store_raw { ip_prefix } else { String::new() },
        user_agent: if state.config.store_raw { user_agent } else { String::new() },
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

        // The client may say what the visitor's browser was — Umami's rule.
        browser: raw.browser.or(info.browser),
        browser_version: info.browser_version,
        os: raw.os.or(info.os),
        os_version: info.os_version,
        device: raw.device.or(info.device),
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
        // The owner's own tag, unless the hit carries a marker.
        tag: tag.or(raw.tag),
        data,
    }, claims))
}

/// Split a URL into its path, its query string and its parameters.
///
/// **The URL is resolved the way Umami resolves it.** The Umami tracker sends
/// the **whole** URL, so the origin comes off and only the path is kept; a
/// relative URL is a path of the site, so it starts at the root; and the
/// fragment belongs to the path (Umami keeps it there).
///
/// **The path is moved, not copied** ✓: a URL that is already a path — no
/// origin, no query, no fragment — *is* its path ⇒ ⇒ ⇒ so the string the
/// tracker sent becomes the string the event carries ✓ ⇒ ⇒ ⇒ no copy ✓
/// (see `MEMORY.md` §6 ✓).
fn split_url(url: String) -> (Option<String>, Option<String>, Vec<(String, String)>) {
    // The fragment comes off first: it is the last thing in a URL.
    let fragment_at = url.find('#');
    let without_fragment = match fragment_at {
        Some(at) => &url[..at],
        None => &url[..],
    };

    // The path and the query string.
    let query_at = without_fragment.find('?');
    let path_end = query_at.unwrap_or(without_fragment.len());
    let (path, query) = match query_at {
        Some(at) => (&without_fragment[..at], Some(&without_fragment[at + 1..])),
        None => (without_fragment, None),
    };

    // The URL may be absolute — the Umami tracker sends the whole thing — and
    // then only the path is kept.
    let origin_at = path.find("://");
    let path = match origin_at {
        Some(at) => {
            let rest = &path[at + 3..];
            match rest.find('/') {
                Some(slash) => &rest[slash..],
                None => "/",
            }
        }
        None => path,
    };

    let params = match query {
        Some(query) => parse_query(query),
        None => Vec::new(),
    };
    let query = query.map(str::to_string);

    // The path, with the fragment on it. The simple case is the common one:
    // the URL was a path already ⇒ **moved**, not copied.
    let simple = origin_at.is_none() && fragment_at.is_none() && path.starts_with('/');
    let path = if simple {
        let mut rest = url;
        rest.truncate(path_end);
        rest
    } else {
        let mut out = String::with_capacity(path.len() + 8);
        if !path.starts_with('/') {
            out.push('/');
        }
        out.push_str(path);
        if let Some(at) = fragment_at {
            out.push('#');
            out.push_str(&url[at + 1..]);
        }
        out
    };

    (Some(path), query, params)
}

/// Split a referrer into its host, path and query.
///
/// A referrer may be a whole URL, or **only a path** — the tracker sends the
/// path of the page the visitor came from when the referrer is on the same
/// site. There is no host in that one (Umami resolves it against the site's
/// own domain, so it counts as a self-referral).
fn split_referrer(referrer: String) -> (Option<String>, Option<String>, Option<String>) {
    let without_scheme = referrer
        .strip_prefix("https://")
        .or_else(|| referrer.strip_prefix("http://"))
        .unwrap_or(&referrer);

    // The path and the query of a path-only referrer.
    let (path, query) = match without_scheme.split_once('?') {
        Some((path, query)) => (Some(path.to_string()), Some(query.to_string())),
        None => (Some(without_scheme.to_string()), None),
    };

    // A path-only referrer: there is no host in it.
    if without_scheme.starts_with('/') {
        return (None, path, query);
    }

    // The host and the rest of it. The path keeps its slash — Umami stores
    // the pathname.
    let (host, path, query) = match without_scheme.split_once('/') {
        Some((host, rest)) => {
            let (path, query) = match rest.split_once('?') {
                Some((path, query)) => (Some(format!("/{path}")), Some(query.to_string())),
                None => (Some(format!("/{rest}")), None),
            };
            (host.to_string(), path, query)
        }
        None => (without_scheme.to_string(), None, None),
    };

    (Some(host), path, query)
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
