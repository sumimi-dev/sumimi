//! The documentation site — embedded in the binary, served at `/docs`.
//!
//! The point of these tests: a server installed with `cargo install` has **no
//! `docs/` directory next to it**, and `/docs` still has to work. The
//! documentation is embedded in the binary by `build.rs`, and the on-disk copy
//! — the one the release archive ships — still wins when it is there.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use sumimi_service::api::{self, AppState};
use sumimi_service::config::{Config, Paths};
use sumimi_service::pool::Db;
use sumimi_service::writer::Writer;

/// Stops the writer thread when the test ends — **even when it panics**.
struct WriterGuard(sumimi_service::writer::Writer);

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

/// A unique temporary directory.
///
/// The tests run in parallel, and the process id alone is not unique enough:
/// two of them would share a path, and one would delete the other's database
/// while it was open. The counter makes every call its own directory.
fn temp_dir(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "sumimi-docs-{name}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build the application with the given documentation directory.
fn app(docs_dir: PathBuf) -> (axum::Router, WriterGuard) {
    let dir = temp_dir("app");
    let path = dir.join("sumimi.duckdb");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());

    let mut config = Config::from_env_and_flags(
        Paths::new(Some(dir.clone()), Some(path.clone()), "duckdb"),
        None,
        None,
        None,
        None,
        None,
        Some(docs_dir),
    )
    .unwrap();
    config.jwt_secret = "test-secret".to_string();

    let (writer, _handle) = Writer::spawn(db.clone(), Duration::from_millis(200), 1000);
    let guard = WriterGuard(writer.clone());

    let instance = Arc::new(sumimi_service::instance::SqliteInstance::open_in_memory().unwrap());

    let state = AppState::new(
        db.clone(),
        instance,
        writer.clone(),
        Arc::new(config),
        Arc::new("test-secret".to_string()),
    );

    (api::router(state), guard)
}

/// Ask for a path and return the status, the content type and the body.
async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String, String) {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).to_string(),
    )
}

/// The documentation is served even when there is no documentation directory.
#[tokio::test]
async fn the_documentation_is_served_without_a_directory_on_disk() {
    // A directory that does not exist — what `cargo install` leaves behind.
    let missing = temp_dir("missing").join("not-there");
    let (app, _guard) = app(missing);

    // The index.
    let (status, content_type, body) = get(&app, "/docs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/html"), "{content_type}");
    assert!(
        body.to_lowercase().contains("<html") || body.to_lowercase().contains("<!doctype"),
        "the index is not HTML"
    );

    // A page of the site.
    let (status, content_type, body) = get(&app, "/docs/docs/install.html").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/html"), "{content_type}");
    assert!(body.contains("Sumimi"), "the page is not the one we wrote");

    // The documentation section, and the blog.
    let (status, content_type, body) = get(&app, "/docs/docs/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/html"), "{content_type}");
    assert!(body.contains("Documentation"), "the section page is not the one we wrote");

    let (status, content_type, body) = get(&app, "/docs/blog/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/html"), "{content_type}");
    assert!(body.contains("Blog"), "the blog index is not the one we wrote");

    // The stylesheet.
    let (status, content_type, body) = get(&app, "/docs/assets/style.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/css"), "{content_type}");
    assert!(!body.is_empty());

    // A page that does not exist — and not the embedded one either.
    let (status, _, _) = get(&app, "/docs/nope.html").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The documentation on disk wins — a user may have edited it.
#[tokio::test]
async fn the_documentation_on_disk_wins() {
    let dir = temp_dir("disk");
    std::fs::write(dir.join("index.html"), "<!doctype html><title>mine</title>").unwrap();
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("assets/style.css"), "/* mine */").unwrap();

    let (app, _guard) = app(dir.clone());

    let (status, _, body) = get(&app, "/docs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("mine"), "the on-disk copy did not win: {body}");

    let (status, _, body) = get(&app, "/docs/assets/style.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("mine"), "the on-disk copy did not win: {body}");
}

/// The embedded copy is the fallback, file by file.
#[tokio::test]
async fn every_documentation_file_is_embedded() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
    let mut on_disk = Vec::new();
    for entry in walk(&docs) {
        let name = entry
            .strip_prefix(&docs)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        on_disk.push(name);
    }
    on_disk.sort();

    // The embedded table is private to the module, so the check goes through
    // the server: every file on disk has to be served.
    let missing = temp_dir("missing2").join("not-there");
    let (app, _guard) = app(missing);

    for name in &on_disk {
        let (status, _, body) = get(&app, &format!("/docs/{name}")).await;
        assert_eq!(status, StatusCode::OK, "{name} is not served");
        assert!(!body.is_empty(), "{name} is empty");
    }
}

/// Every file under a directory.
fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return files,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}
