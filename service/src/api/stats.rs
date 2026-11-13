//! Statistics endpoints.
//!
//! Every handler here is a thin wrapper around [`crate::reports`] — the same code the
//! CLI runs. The metric semantics are defined in exactly one place:
//!
//!   * `pageviews`    — total page views
//!   * `visitors`     — unique visitors (daily-rotating id)
//!   * `views`        — number of sessions (visits)
//!   * `bounces`      — sessions with exactly one page view
//!   * `visitDuration` — average session duration in seconds
//!
//! The reports are also **cached** in the instance store. Every cached answer
//! carries the version of the data it was computed from and the moment it was
//! computed:
//!
//! * while the version still matches, the answer is exactly right;
//! * after a write, the answer may still be used for as long as the stale
//!   window allows (`SUMIMI_CACHE_STALE_SECS`, 30 seconds by default).
//!
//! The window is why the cache is worth having. The writer flushes every
//! couple of seconds, so a cache that only accepted the current version would
//! never hit on a busy site — and the dashboard would run the same query over
//! and over. With the window, the query runs once, and the numbers on the
//! screen are at most half a minute old.

use axum::extract::{Path, Query, State};
use axum::Json;

use crate::api::AppState;
use crate::error::ApiError;
use crate::models::{ReportKind, ReportSpec};
use crate::storage::{Dimension, ReportQuery, TimeRange};

/// Resolve the requested window (Umami sends unix milliseconds).
pub(crate) fn resolve_range(query: &crate::models::StatsQuery) -> TimeRange {
    let end = query
        .end_at
        .map(|ms| chrono::DateTime::from_timestamp_millis(ms).map(|d| d.naive_utc()))
        .unwrap_or_else(|| Some(crate::db::now()))
        .unwrap_or_else(crate::db::now);

    let start = query
        .start_at
        .map(|ms| chrono::DateTime::from_timestamp_millis(ms).map(|d| d.naive_utc()))
        .unwrap_or_else(|| Some(end - chrono::Duration::days(7)))
        .unwrap_or_else(|| end - chrono::Duration::days(7));

    TimeRange { start, end }
}

/// Build the shared report query from the request parameters.
pub(crate) fn report_query(id: &str, query: &crate::models::StatsQuery) -> ReportQuery {
    let mut report = ReportQuery::new(id, resolve_range(query));
    report.dimensions = crate::storage::Dimensions {
        url: query.url.clone(),
        referrer: query.referrer.clone(),
        browser: query.browser.clone(),
        os: query.os.clone(),
        device: query.device.clone(),
        country: query.country.clone(),
    };
    // The marked traffic is not part of the numbers unless the dashboard asks
    // for its kinds.
    report.include_markers = query
        .markers
        .as_deref()
        .map(|markers| {
            markers
                .split(',')
                .map(str::trim)
                .filter(|marker| !marker.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    report.limit = query.limit.unwrap_or(50).clamp(1, 1000);
    report.offset = query.offset.unwrap_or(0).max(0);
    report
}

/// Run a report and return its JSON.
///
/// **One place** decides what each report is: the stats endpoints, the saved
/// reports (snapshots) and the cache all come through here, so they cannot
/// disagree about what "the top pages" means.
pub fn run_report(
    storage: &dyn crate::storage::Storage,
    spec: &ReportSpec,
) -> Result<serde_json::Value, ApiError> {
    let query = report_query(&spec.website, &spec.query);
    let value = match spec.report {
        ReportKind::Summary => serde_json::to_value(storage.summary_with_change(&query)?)?,
        ReportKind::Timeseries => serde_json::to_value(storage.timeseries(&query)?)?,
        ReportKind::Pages => serde_json::to_value(storage.breakdown(&query, Dimension::Url)?)?,
        ReportKind::Referrers => {
            serde_json::to_value(storage.breakdown(&query, Dimension::Referrer)?)?
        }
        ReportKind::Browsers => {
            serde_json::to_value(storage.breakdown(&query, Dimension::Browser)?)?
        }
        ReportKind::Os => serde_json::to_value(storage.breakdown(&query, Dimension::Os)?)?,
        ReportKind::Devices => {
            serde_json::to_value(storage.breakdown(&query, Dimension::Device)?)?
        }
        ReportKind::Countries => {
            serde_json::to_value(storage.breakdown(&query, Dimension::Country)?)?
        }
        ReportKind::Languages => {
            serde_json::to_value(storage.breakdown(&query, Dimension::Language)?)?
        }
        ReportKind::Screens => {
            serde_json::to_value(storage.breakdown(&query, Dimension::Screen)?)?
        }
        ReportKind::Channels => {
            serde_json::to_value(storage.breakdown(&query, Dimension::Channel)?)?
        }
        ReportKind::Regions => {
            serde_json::to_value(storage.breakdown(&query, Dimension::Region)?)?
        }
        ReportKind::Cities => {
            serde_json::to_value(storage.breakdown(&query, Dimension::City)?)?
        }
        ReportKind::Hosts => {
            serde_json::to_value(storage.breakdown(&query, Dimension::Host)?)?
        }
        ReportKind::Tags => {
            serde_json::to_value(storage.breakdown(&query, Dimension::Tag)?)?
        }
        ReportKind::QueryParameters => serde_json::to_value(
            crate::storage::parse_query_parameters(&storage.query_strings(&query)?),
        )?,
        ReportKind::Events => serde_json::to_value(storage.events(&query)?)?,
        ReportKind::Sessions => serde_json::to_value(storage.sessions(&query)?)?,
        ReportKind::EventLog => serde_json::to_value(storage.event_log(&query)?)?,
    };
    Ok(value)
}

/// The cache key of a report.
fn cache_key(spec: &ReportSpec) -> String {
    // The key is the report and everything that changes its answer. It is only
    // ever used as a key, so it does not have to be pretty.
    format!(
        "report:{:?}:{}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}",
        spec.report,
        spec.website,
        spec.query.start_at,
        spec.query.end_at,
        spec.query.url,
        spec.query.referrer,
        spec.query.browser,
        spec.query.os,
        spec.query.device,
        spec.query.country,
        spec.query.event,
        spec.query.query,
        spec.query.limit,
        spec.query.offset,
        // The kinds of marked traffic the report counts: they change the
        // answer, so they are part of the key.
        spec.query.markers,
    )
}

/// Run a report, through the cache.
///
/// The cached answer carries **the version of the data it was computed from**
/// and **the moment it was computed**. Two things make it usable:
///
/// * the version still matches — the data has not changed, so the answer is
///   exactly right;
/// * the answer is not too old — the writer flushes every couple of seconds,
///   so on a busy site the version changes all the time and a cache that only
///   accepted the current version would never hit. The window is
///   `SUMIMI_CACHE_STALE_SECS` (30 seconds by default, `0` for "only while the
///   data has not changed").
pub fn cached_report(
    state: &AppState,
    spec: &ReportSpec,
) -> Result<serde_json::Value, ApiError> {
    let key = cache_key(spec);
    let version = state.writer.version().load(std::sync::atomic::Ordering::Relaxed);
    let stale = state.config.cache_stale_secs;

    if let Some(cached) = state.instance.cache_get(&key)? {
        if let Some(value) = decode(&cached, version, stale) {
            return Ok(value);
        }
    }

    let value = run_report(state.storage.as_ref(), spec)?;
    let payload = serde_json::to_string(&value)?;
    // The entry outlives the window it is served in, so a stale answer can
    // still be found.
    let _ = state.instance.cache_put(
        &key,
        &encode(version, now_millis(), &payload),
        Some(stale.max(0) + 3600),
    );
    Ok(value)
}

/// The cached answer: `<version>|<written at>|<json>`.
fn encode(version: u64, written_at: i64, payload: &str) -> String {
    format!("{version}|{written_at}|{payload}")
}

/// Read a cached answer, if it may still be used.
///
/// The version is the exact answer: while the data has not changed, the answer
/// is right. The window is the practical one: an answer that is not too old is
/// good enough for a dashboard, and it saves the query.
fn decode(cached: &str, version: u64, max_stale_secs: i64) -> Option<serde_json::Value> {
    let mut parts = cached.splitn(3, '|');
    let cached_version = parts.next()?.parse::<u64>().ok()?;
    let written_at = parts.next()?.parse::<i64>().ok()?;
    let payload = parts.next()?;

    if cached_version == version {
        return serde_json::from_str(payload).ok();
    }

    if max_stale_secs > 0 {
        let age = (now_millis() - written_at) / 1000;
        if (0..=max_stale_secs).contains(&age) {
            return serde_json::from_str(payload).ok();
        }
    }

    None
}

/// Now, in milliseconds.
fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `GET /api/websites/{id}/stats` — the summary metrics.
pub async fn summary(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let spec = ReportSpec {
        website: id,
        report: ReportKind::Summary,
        query,
    };
    Ok(Json(cached_report(&state, &spec)?))
}

/// `GET /api/websites/{id}/stats/metrics` — the same metrics, without the change.
pub async fn metrics(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let spec = ReportSpec {
        website: id,
        report: ReportKind::Summary,
        query,
    };
    let value = cached_report(&state, &spec)?;
    // The same numbers, but without the period-over-period change.
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        for key in [
            "pageviews",
            "visitors",
            "views",
            "bounces",
            "visitDuration",
        ] {
            if let Some(metric) = object.get_mut(key).and_then(|m| m.as_object_mut()) {
                metric.insert("change".to_string(), serde_json::Value::from(0.0));
            }
        }
    }
    Ok(Json(value))
}

/// `GET /api/websites/{id}/stats/timeseries` — page views over time.
pub async fn timeseries(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Timeseries).await
}

/// `GET /api/websites/{id}/stats/pages` — top pages.
pub async fn pages(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Pages).await
}

/// `GET /api/websites/{id}/stats/referrers` — traffic sources.
pub async fn referrers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut value = breakdown(state, id, query, ReportKind::Referrers).await?.0;
    // The dashboard shows the direct traffic as "(direct)".
    if let Some(rows) = value.as_array_mut() {
        for row in rows.iter_mut() {
            if row.get("name").and_then(|n| n.as_str()) == Some("unknown") {
                if let Some(object) = row.as_object_mut() {
                    object.insert("name".to_string(), serde_json::Value::from("(direct)"));
                }
            }
        }
    }
    Ok(Json(value))
}

/// `GET /api/websites/{id}/stats/browsers`
pub async fn browsers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Browsers).await
}

/// `GET /api/websites/{id}/stats/os`
pub async fn os(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Os).await
}

/// `GET /api/websites/{id}/stats/devices`
pub async fn devices(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Devices).await
}

/// `GET /api/websites/{id}/stats/countries`
pub async fn countries(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Countries).await
}

/// `GET /api/websites/{id}/stats/languages`
pub async fn languages(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Languages).await
}

/// `GET /api/websites/{id}/stats/screens`
pub async fn screens(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Screens).await
}

/// `GET /api/websites/{id}/stats/events` — custom events, grouped by name.
pub async fn events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Events).await
}

/// `GET /api/websites/{id}/stats/urls` — the number of distinct URLs visited.
pub async fn urls(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Pages).await
}

/// `GET /api/websites/{id}/stats/channels` — the traffic channels.
pub async fn channels(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Channels).await
}

/// `GET /api/websites/{id}/stats/regions` — the regions of the visitors.
pub async fn regions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Regions).await
}

/// `GET /api/websites/{id}/stats/cities` — the cities of the visitors.
pub async fn cities(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Cities).await
}

/// `GET /api/websites/{id}/stats/hosts` — the hostnames of the sites.
pub async fn hosts(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Hosts).await
}

/// `GET /api/websites/{id}/stats/tags` — the tags of the custom events.
pub async fn tags(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::Tags).await
}

/// `GET /api/websites/{id}/stats/query-parameters` — the query parameters of
/// the visited pages.
pub async fn query_parameters(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::models::StatsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    breakdown(state, id, query, ReportKind::QueryParameters).await
}

/// Shared breakdown handler.
async fn breakdown(
    state: AppState,
    id: String,
    query: crate::models::StatsQuery,
    report: ReportKind,
) -> Result<Json<serde_json::Value>, ApiError> {
    let spec = ReportSpec {
        website: id,
        report,
        query,
    };
    Ok(Json(cached_report(&state, &spec)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kinds of marked traffic change the answer, so they are part of the
    /// cache key — otherwise the answer for one is served for the other.
    #[test]
    fn the_kinds_of_marked_traffic_are_part_of_the_cache_key() {
        let mut spec = ReportSpec {
            website: "w1".to_string(),
            report: ReportKind::Pages,
            query: crate::models::StatsQuery {
                start_at: Some(1),
                end_at: Some(2),
                url: None,
                referrer: None,
                browser: None,
                os: None,
                device: None,
                country: None,
                event: None,
                query: None,
                markers: None,
                limit: None,
                offset: None,
            },
        };

        let without = cache_key(&spec);
        spec.query.markers = Some("bot-google".to_string());
        let with = cache_key(&spec);
        assert_ne!(without, with, "the two answers must not share a key");

        // And two different kinds are two different answers too.
        spec.query.markers = Some("bot-ai".to_string());
        assert_ne!(with, cache_key(&spec));
    }
}
