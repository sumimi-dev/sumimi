//! Static assets: the tracker, the dashboard shell and the frontend files.
//!
//! The backend deliberately serves a **very small HTML document** that only references
//! the frontend JavaScript. The frontend can therefore live on a CDN (or be replaced by
//! a newer build) without touching the backend binary — and the binary itself stays tiny
//! because the frontend is never embedded in it.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::api::AppState;

/// The tracker, embedded in the binary so it is always available at `/sumimi.js`.
pub const TRACKER_JS: &str = include_str!("../js/sumimi.js");

// The documentation site, embedded in the binary by `build.rs`.
//
// A server installed with `cargo install` has no `docs/` directory next to it —
// this is what makes `/docs` work anyway.
include!(concat!(env!("OUT_DIR"), "/docs.rs"));

/// The dashboard shell. Deliberately tiny: everything else lives in the frontend JS.
const APP_SHELL: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Sumimi</title>
<link rel="stylesheet" href="{base}/sumimi.css">
</head>
<body>
<div id="app"><noscript>Sumimi needs JavaScript for the dashboard. The API works without it.</noscript></div>
<script src="{base}/sumimi-app.js" defer></script>
</body>
</html>
"#;

/// `GET /` — the dashboard shell.
pub async fn index(State(state): State<AppState>) -> Response {
    // The frontend is served from a CDN by default; `SUMIMI_APP_URL=/app` serves it
    // from the backend itself (air-gapped installs).
    let base = state
        .config
        .app_url
        .clone()
        .unwrap_or_else(|| crate::config::CDN_APP_URL.to_string());
    let html = APP_SHELL.replace("{base}", &base);

    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

/// `GET /sumimi.js` — the tracker.
pub async fn tracker() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        TRACKER_JS,
    )
        .into_response()
}

/// `GET /app/{*path}` — the frontend files (served from the `js/` directory).
pub async fn app_asset(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    // Never escape the asset directory.
    if path.contains("..") {
        return not_found().await;
    }

    let file = state.config.js_dir.join(&path);
    let file = match tokio::fs::canonicalize(&file).await {
        Ok(path) => path,
        Err(_) => return not_found().await,
    };
    let root = match tokio::fs::canonicalize(&state.config.js_dir).await {
        Ok(path) => path,
        Err(_) => return not_found().await,
    };
    if !file.starts_with(&root) {
        return not_found().await;
    }

    match tokio::fs::read(&file).await {
        Ok(bytes) => {
            let mime = mime_for(&path);
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "public, max-age=60"),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => not_found().await,
    }
}

/// `GET /docs` — the documentation index.
pub async fn docs_index(State(state): State<AppState>) -> Response {
    match read_docs(&state, "index.html").await {
        Some((bytes, _)) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            bytes,
        )
            .into_response(),
        None => not_found().await,
    }
}

/// `GET /docs/{*path}` — the documentation site.
pub async fn docs_asset(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    if path.contains("..") {
        return not_found().await;
    }

    match read_docs(&state, &path).await {
        Some((bytes, name)) => {
            // The content type follows the **file** that was read, not the
            // request: a directory is served as its `index.html`.
            let mime = mime_for(&name);
            (
                [(header::CONTENT_TYPE, mime)],
                bytes,
            )
                .into_response()
        }
        None => not_found().await,
    }
}

/// Read a file of the documentation site.
///
/// Returns the bytes and the name of the file they came from — a directory is
/// served as its `index.html`, and the content type has to say so.
///
/// The **on-disk** copy comes first: the release archive ships the documentation
/// next to the executable, and a user may have edited it. The copy **embedded in
/// the binary** is the fallback, so a server installed with `cargo install`
/// still has a documentation site.
async fn read_docs(state: &AppState, path: &str) -> Option<(Vec<u8>, String)> {
    let relative = path.trim_start_matches('/').trim_end_matches('/');

    let mut file = state.config.docs_dir.join(relative);
    let mut name = relative.to_string();
    if is_dir(&file).await {
        file = file.join("index.html");
        name = format!("{relative}/index.html");
    }
    if let Ok(bytes) = tokio::fs::read(&file).await {
        return Some((bytes, name));
    }

    // The embedded copy, file by file.
    if let Some(bytes) = embedded(relative) {
        return Some((bytes, name));
    }
    let index = format!("{relative}/index.html");
    embedded(&index).map(|bytes| (bytes, index))
}

/// A file of the embedded documentation site.
fn embedded(path: &str) -> Option<Vec<u8>> {
    DOCS.iter()
        .find(|(name, _)| *name == path)
        .map(|(_, bytes)| bytes.to_vec())
}

/// Whether a path is a directory.
async fn is_dir(path: &std::path::Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

/// The 404 page.
pub async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"error":"not found","status":404}"#,
    )
        .into_response()
}

/// Guess the content type from the file extension.
fn mime_for(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".js") || lower.ends_with(".mjs") {
        "application/javascript; charset=utf-8"
    } else if lower.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if lower.ends_with(".html") || lower.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".ico") {
        "image/x-icon"
    } else if lower.ends_with(".woff2") {
        "font/woff2"
    } else {
        "application/octet-stream"
    }
}
