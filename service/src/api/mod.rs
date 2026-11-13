//! The HTTP API.
//!
//! Layout mirrors the Umami v2 API so the existing dashboard (and the Sumimi frontend)
//! can talk to it unchanged.

pub mod admin;
pub mod auth;
pub mod collect;
pub mod health;
pub mod sessions;
pub mod stats;
pub mod websites;

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::HeaderName;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};

use crate::config::Config;
use crate::error::ApiError;
use crate::storage::Storage;
use crate::writer::Writer;

/// The cross-origin rules of the collection endpoints.
///
/// **The collection is public**, so it is the one place where a page on
/// another origin has to be able to post. Without these headers the browser
/// sends a preflight, we do not answer it, and the hit is dropped before it
/// reaches us — silently, because the tracker swallows the error.
///
/// The headers the client carries its state in are allowed as well: ours
/// first, then Umami's — the Umami SDK is meant to work against us as it is.
///
/// The layer sits on the collection routes only: the rest of the API is
/// authenticated, and a cross-origin reader of it would be a reader of
/// somebody else's data.
fn collection_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([axum::http::Method::POST, axum::http::Method::OPTIONS])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            HeaderName::from_static("x-sumimi-cache"),
            HeaderName::from_static("x-umami-cache"),
            HeaderName::from_static("x-sumimi-website-id"),
            HeaderName::from_static("x-umami-website-id"),
            HeaderName::from_static("x-sumimi-hostname"),
            HeaderName::from_static("x-umami-hostname"),
        ])
        .max_age(std::time::Duration::from_secs(86_400))
}

/// Metadata we need on the hot path (collection).
#[derive(Debug, Clone)]
pub struct WebsiteMeta {
    pub id: String,
    pub domain: Option<String>,
    pub retention_days: i32,
    pub country_header: Option<String>,
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    /// The storage backend (DuckDB, SQLite or PostgreSQL).
    pub storage: Arc<dyn Storage>,
    /// The instance store: the accounts, the log, the cache and the rest — the
    /// small, transactional half of an instance. It is a **separate** store from
    /// the events, so the accounts survive an analytics rebuild.
    pub instance: Arc<dyn crate::instance::Instance>,
    /// The single writer.
    pub writer: Writer,
    /// Runtime configuration.
    pub config: Arc<Config>,
    /// Secret used to sign JWTs and to derive session ids.
    pub jwt_secret: Arc<String>,
    /// Process start time.
    pub started_at: Instant,
    /// Cache of website metadata (id or domain -> meta).
    website_cache: Arc<RwLock<HashMap<String, WebsiteMeta>>>,
    /// When the cache was last refreshed (unix seconds).
    cache_stamp: Arc<AtomicU64>,
    /// The geolocation resolver: where an IP address is.
    ///
    /// The open-source build has none — it uses the country header a proxy
    /// sends. The private edition adds one.
    pub geo: Option<Arc<dyn crate::geo::Resolver>>,
    /// The admission gate: what may be in flight on the collection path, and how
    /// fast it may come in.
    pub gate: Arc<crate::limits::Gate>,
    /// The sessions of the visitors who are active right now.
    ///
    /// The cut between two sessions is made here, when the event arrives — so
    /// the row carries the session it belongs to, and no report has to work it
    /// out again.
    pub sessions: Arc<crate::session::Sessions>,
}

impl AppState {
    /// Build the state.
    pub fn new(
        storage: Arc<dyn Storage>,
        instance: Arc<dyn crate::instance::Instance>,
        writer: Writer,
        config: Arc<Config>,
        jwt_secret: Arc<String>,
    ) -> Self {
        // The gate is built from the configuration before it is moved into the
        // state.
        let gate = Arc::new(crate::limits::Gate::new(config.limits()));
        // The sessions, likewise: the idle timeout decides where one session
        // ends and the next begins.
        let sessions = Arc::new(crate::session::Sessions::new(
            std::time::Duration::from_secs(config.session_timeout_seconds.max(0) as u64),
            config.session_capacity,
        ));
        Self {
            storage,
            instance,
            writer,
            config,
            jwt_secret,
            started_at: Instant::now(),
            website_cache: Arc::new(RwLock::new(HashMap::new())),
            cache_stamp: Arc::new(AtomicU64::new(0)),
            geo: None,
            gate,
            sessions,
        }
    }

    /// Add the geolocation resolver.
    ///
    /// The resolver is looked up on the collection path, so it has to be fast:
    /// an in-memory table, not a query.
    pub fn with_geo(mut self, resolver: Arc<dyn crate::geo::Resolver>) -> Self {
        self.geo = Some(resolver);
        self
    }

    /// Look up a website by id or domain, with a short-lived cache.
    ///
    /// The cache keeps the collection path off the database: a hit only needs the
    /// website's retention policy and geo header.
    pub fn website_meta(&self, key: &str) -> Result<WebsiteMeta, ApiError> {
        let now = chrono::Utc::now().timestamp() as u64;
        let stamp = self.cache_stamp.load(Ordering::Relaxed);
        if now.saturating_sub(stamp) > 30 {
            self.refresh_website_cache();
        }

        if let Some(meta) = self
            .website_cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
        {
            return Ok(meta.clone());
        }

        // A website created seconds ago may not be cached yet.
        self.refresh_website_cache();
        self.website_cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
            .ok_or_else(|| ApiError::BadRequest(format!("unknown website: {key}")))
    }

    /// Reload the website cache.
    fn refresh_website_cache(&self) {
        let websites = self
            .storage
            .websites()
            .unwrap_or_default()
            .into_iter()
            .map(|website| WebsiteMeta {
                id: website.id,
                domain: website.domain,
                retention_days: website.retention_days,
                country_header: website.country_header,
            })
            .collect::<Vec<_>>();

        let mut cache = self.website_cache.write().unwrap_or_else(|e| e.into_inner());
        cache.clear();
        for website in websites {
            cache.insert(website.id.clone(), website.clone());
            if let Some(domain) = website.domain.as_ref() {
                cache.insert(domain.to_lowercase(), website);
            }
        }
        drop(cache);
        self.cache_stamp
            .store(chrono::Utc::now().timestamp() as u64, Ordering::Relaxed);
    }
}

/// The routes that answer without a token.
///
/// The tracker posts here — it cannot hold a token, it runs on somebody else's
/// page — and the dashboard's own files are public. Everything else needs one.
fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/api/send" | "/api/collect" | "/api/heartbeat" | "/api/login"
    ) || path == "/sumimi.js"
        || path == "/"
        || path.starts_with("/app/")
        || path == "/docs"
        || path.starts_with("/docs/")
}

/// Refuse every request that does not carry a valid token.
///
/// One place, so a new route cannot forget it.
async fn require_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if is_public(request.uri().path()) {
        return next.run(request).await;
    }

    let header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let Some(header) = header else {
        return ApiError::Unauthorized.into_response();
    };
    let token = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .unwrap_or(header)
        .trim();

    if crate::auth::decode_token(&state.jwt_secret, token).is_err() {
        return ApiError::Unauthorized.into_response();
    }

    next.run(request).await
}

/// Build the router.
pub fn router(state: AppState) -> Router {
    // The collection path is the only one whose size the traffic decides, so it
    // is the only one with a gate: the body limit and the admission control
    // both sit **in front of** the handler, so a request that is not admitted
    // never costs any memory.
    let body_limit = state.config.max_body_bytes;
    Router::new()
        // --- collection (public) ---
        .route(
            "/api/send",
            post(collect::send)
                .layer::<CorsLayer, Infallible>(collection_cors())
                .layer(axum::extract::DefaultBodyLimit::max(body_limit))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    crate::limits::admit,
                )),
        )
        .route(
            "/api/collect",
            post(collect::collect)
                .layer::<CorsLayer, Infallible>(collection_cors())
                .layer(axum::extract::DefaultBodyLimit::max(body_limit))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    crate::limits::admit,
                )),
        )
        .route("/api/heartbeat", get(health::heartbeat))
        // --- auth ---
        .route("/api/login", post(auth::login))
        .route("/api/auth/refresh", post(auth::refresh))
        .route("/api/me", get(auth::me))
        // --- websites ---
        .route(
            "/api/websites",
            get(websites::list).post(websites::create),
        )
        .route(
            "/api/websites/{id}",
            get(websites::get)
                .put(websites::update)
                .delete(websites::delete),
        )
        // --- stats ---
        .route("/api/websites/{id}/stats", get(stats::summary))
        .route("/api/websites/{id}/stats/metrics", get(stats::metrics))
        .route(
            "/api/websites/{id}/stats/timeseries",
            get(stats::timeseries),
        )
        .route("/api/websites/{id}/stats/pages", get(stats::pages))
        .route(
            "/api/websites/{id}/stats/referrers",
            get(stats::referrers),
        )
        .route("/api/websites/{id}/stats/browsers", get(stats::browsers))
        .route("/api/websites/{id}/stats/os", get(stats::os))
        .route("/api/websites/{id}/stats/devices", get(stats::devices))
        .route(
            "/api/websites/{id}/stats/countries",
            get(stats::countries),
        )
        .route(
            "/api/websites/{id}/stats/languages",
            get(stats::languages),
        )
        .route("/api/websites/{id}/stats/screens", get(stats::screens))
        .route("/api/websites/{id}/stats/events", get(stats::events))
        .route("/api/websites/{id}/stats/channels", get(stats::channels))
        .route("/api/websites/{id}/stats/regions", get(stats::regions))
        .route("/api/websites/{id}/stats/cities", get(stats::cities))
        .route("/api/websites/{id}/stats/hosts", get(stats::hosts))
        .route("/api/websites/{id}/stats/tags", get(stats::tags))
        .route(
            "/api/websites/{id}/stats/query-parameters",
            get(stats::query_parameters),
        )
        .route("/api/websites/{id}/stats/urls", get(stats::urls))
        // --- raw data ---
        .route("/api/websites/{id}/sessions", get(sessions::list))
        .route("/api/websites/{id}/events", get(sessions::events))
        // --- the control plane ---
        .route(
            "/api/logs",
            get(admin::logs).post(admin::write_log),
        )
        .route("/api/gsc/import", post(admin::import_gsc))
        .route("/api/websites/{id}/gsc", get(admin::gsc_rows))
        .route("/api/websites/{id}/gsc/imports", get(admin::gsc_imports))
        .route(
            "/api/snapshots",
            get(admin::snapshots).post(admin::save_snapshot),
        )
        .route(
            "/api/snapshots/{name}",
            delete(admin::delete_snapshot),
        )
        .route("/api/snapshots/{name}/run", post(admin::run_snapshot))
        .route(
            "/api/chat/sessions",
            get(admin::chat_sessions).post(admin::start_chat),
        )
        .route("/api/chat/sessions/{id}", get(admin::chat_messages))
        .route(
            "/api/chat/sessions/{id}/messages",
            post(admin::say),
        )
        .route(
            "/api/chat/sessions/{id}/ask",
            post(admin::ask),
        )
        // The one-shot question: the analyst, without the conversation.
        .route(
            "/api/websites/{id}/ask",
            post(admin::ask_website),
        )
        .route(
            "/api/cache",
            get(admin::cache_stats).delete(admin::purge_cache),
        )
        // --- the uptime ---
        .route(
            "/api/websites/{id}/uptime",
            get(admin::uptime_history).post(admin::check_uptime),
        )
        .route(
            "/api/websites/{id}/uptime/check",
            post(admin::check_all_uptime),
        )
        .route(
            "/api/websites/{id}/uptime/endpoints",
            get(admin::uptime_endpoints)
                .post(admin::add_uptime_endpoint)
                .delete(admin::remove_uptime_endpoint),
        )
        // --- the teams ---
        .route(
            "/api/teams",
            get(admin::teams).post(admin::create_team),
        )
        .route(
            "/api/teams/{id}",
            get(admin::team).delete(admin::delete_team),
        )
        .route(
            "/api/teams/{id}/members",
            get(admin::members).post(admin::add_member),
        )
        .route(
            "/api/teams/{id}/members/{username}",
            delete(admin::remove_member),
        )
        // --- static ---
        .route("/sumimi.js", get(crate::static_files::tracker))
        // The old URL still works: the tracker is in other people's HTML, and
        // changing the URL would break every site that has it.
        .route("/sumami.js", get(crate::static_files::tracker))
        .route("/app/{*path}", get(crate::static_files::app_asset))
        .route("/docs/{*path}", get(crate::static_files::docs_asset))
        // The directory itself: "/docs/" is what a link to the folder looks like.
        .route("/docs/", get(crate::static_files::docs_index))
        .route("/docs", get(crate::static_files::docs_index))
        .route("/", get(crate::static_files::index))
        .fallback(get(crate::static_files::not_found))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .with_state(state)
}
