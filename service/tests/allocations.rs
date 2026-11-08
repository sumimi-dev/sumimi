//! How much memory does **one request** cost?
//!
//! The instrument is a **counting global allocator**: it wraps the system
//! allocator and counts the bytes. The test drives the real collection path
//! and divides.
//!
//! It needs `unsafe` — that is what implementing `GlobalAlloc` takes — and
//! this is a **measurement instrument**, not product code: the library crates
//! keep `#![forbid(unsafe_code)]` (see `AGENTS.md` §5).
//!
//! Why it matters: the peak resident set grows by about **7.8 MB per 1 000
//! requests a second** (`MEMORY.md` §6) ⇒ **7.8 KB per request** ⇒ while the
//! body itself is **134 bytes** ⇒ so almost all of it is *implementation* —
//! copies, trees and strings that the request does not need to keep.
//!
//! The numbers are printed, and the test fails if the per-request cost leaves
//! the room the deployment reference assumes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use sumimi_service::models::HitKind;

/// The bytes allocated so far.
static ALLOCATED: AtomicU64 = AtomicU64::new(0);

/// How many allocations of each size — **by the power of two** of their size.
///
/// The histogram is what points at the culprit: a request that allocates
/// 20 KB is doing something with a 16 KB buffer, and the histogram says so.
const BUCKETS: usize = 40;
static COUNTS: [AtomicU64; BUCKETS] = [const { AtomicU64::new(0) }; BUCKETS];
static BYTES: [AtomicU64; BUCKETS] = [const { AtomicU64::new(0) }; BUCKETS];

/// The bucket a size falls into: the power of two above it.
fn bucket(size: usize) -> usize {
    (usize::BITS - size.leading_zeros()) as usize
}

/// The counting allocator.
struct Counting;

// SAFETY: every method forwards to the system allocator unchanged; the only
// thing this adds is the counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        let bucket = bucket(layout.size());
        COUNTS[bucket].fetch_add(1, Ordering::Relaxed);
        BYTES[bucket].fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(new_size as u64, Ordering::Relaxed);
        let bucket = bucket(new_size);
        COUNTS[bucket].fetch_add(1, Ordering::Relaxed);
        BYTES[bucket].fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The measurements share one counter, so they take turns.
///
/// The **asynchronous** mutex, because the guard is held across an `await` ✓
/// (a `std` mutex would block the runtime ✓).
static MEASURING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Take the counter for the length of a measurement.
async fn measuring() -> tokio::sync::MutexGuard<'static, ()> {
    MEASURING.lock().await
}

/// The bytes allocated since the last reading.
fn allocated() -> u64 {
    ALLOCATED.swap(0, Ordering::Relaxed)
}

/// Forget the histogram.
fn forget_histogram() {
    for bucket in 0..BUCKETS {
        COUNTS[bucket].store(0, Ordering::Relaxed);
        BYTES[bucket].store(0, Ordering::Relaxed);
    }
}

/// The histogram, biggest buckets first: `size × count`.
fn histogram(rounds: u64) -> Vec<(usize, u64, u64)> {
    let mut rows: Vec<(usize, u64, u64)> = (0..BUCKETS)
        .filter_map(|bucket| {
            let count = COUNTS[bucket].load(Ordering::Relaxed);
            if count == 0 {
                return None;
            }
            let bytes = BYTES[bucket].load(Ordering::Relaxed);
            Some((1usize << bucket.saturating_sub(1), count / rounds.max(1), bytes / rounds.max(1)))
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.2));
    rows
}

/// The price of **the HTTP layer itself** — a router with one route that
/// answers `200` and does nothing else.
///
/// Whatever this costs is what every request pays before the collection path
/// even starts: the body buffer, the response, the middleware stack.
#[tokio::test(flavor = "multi_thread")]
async fn the_price_of_the_http_layer() {
    let _measuring = measuring().await;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let router = axum::Router::new().route(
        "/nothing",
        axum::routing::get(|| async { StatusCode::OK }),
    );

    // Warm it up.
    for _ in 0..20 {
        let request = Request::builder().uri("/nothing").body(Body::empty()).unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let _before = allocated(); // the counter is reset by this reading
    for _ in 0..rounds() {
        let request = Request::builder().uri("/nothing").body(Body::empty()).unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let http = allocated() / rounds() as u64;

    println!("the HTTP layer: {http} bytes per request");
}

/// The price of **one push into the queue** — as a function of how long the
/// queue already is.
///
/// This is the mechanism behind "the memory follows the rate": the queue is a
/// `VecDeque`, and **growing it moves every hit in it** ⇒ the longer the
/// queue, the more each request allocates ⇒ the more the allocator keeps ⇒
/// the higher the peak.
#[tokio::test(flavor = "multi_thread")]
async fn the_price_of_a_push() {
    let _measuring = measuring().await;
    use axum::http::HeaderMap;
    use std::sync::Arc;
    use sumimi_service::api::AppState;
    use sumimi_service::config::{Config, Paths};
    use sumimi_service::pool::Db;
    use sumimi_service::writer::Writer;

    let dir = std::env::temp_dir().join(format!("sumimi-alloc-push-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sumimi.duckdb");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());
    db.with_write(|conn| {
        conn.execute(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
             VALUES ('website-1', 'Example', 'example.com', now(), now(), 0, NULL)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let mut config = Config::from_env_and_flags(
        Paths::new(Some(dir.clone()), Some(path.clone()), "duckdb"),
        None, None, None, None, None, None,
    )
    .unwrap();
    config.jwt_secret = "test-secret".to_string();

    // A writer **without a thread**: nothing drains the queue, so it only
    // grows — and the price of a push can be measured against its real
    // length. (With the thread, the queue would be emptied as fast as it is
    // filled, and the length would never be anything.)
    let writer = Writer::new(200_000, 1_000);
    let state = AppState::new(
        db.clone(),
        Arc::new(sumimi_service::instance::SqliteInstance::open_in_memory().unwrap()),
        writer.clone(),
        Arc::new(config),
        Arc::new("test-secret".to_string()),
    );

    let request: sumimi_service::models::SendRequest = serde_json::from_str(BODY).unwrap();
    let raw = request.into_hit();
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", AGENT.parse().unwrap());

    let hit = api_build_hit(&state, &headers, &raw);

    // The price of a push, at four queue lengths.
    const MEASURE: usize = 200;
    for target in [0u64, 1_000, 10_000, 50_000] {
        while writer.queued() < target {
            writer.push(hit.clone());
        }
        let _before = allocated(); // the counter is reset by this reading
        for _ in 0..MEASURE {
            writer.push(hit.clone());
        }
        let per_push = allocated() / MEASURE as u64;
        println!("  a push into a queue of {target:>6} rows: {per_push:>8} bytes");
    }

    writer.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Build a hit — the helper the push test uses.
fn api_build_hit(
    state: &sumimi_service::api::AppState,
    headers: &axum::http::HeaderMap,
    raw: &sumimi_service::models::RawPayload,
) -> sumimi_service::models::Hit {
    sumimi_service::api::collect::build_hit(
        state,
        headers,
        None,
        "website-1",
        HitKind::Pageview,
        raw,
    )
    .unwrap()
}

/// The body a page view sends.
const BODY: &str = r#"{"website":"website-1","payload":{"url":"/hello","title":"Hello","referrer":"https://news.ycombinator.com/"}}"#;

/// The user agent a browser sends.
const AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// How many times each stage is measured.
///
/// `SUMIMI_ALLOC_ROUNDS` makes it smaller.
fn rounds() -> usize {
    std::env::var("SUMIMI_ALLOC_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000)
}

/// The price of the hit's own construction — the stage where the strings are
/// made.
#[tokio::test(flavor = "multi_thread")]
async fn the_price_of_building_the_hit() {
    let _measuring = measuring().await;
    use axum::http::HeaderMap;
    use std::sync::Arc;
    use std::time::Duration;
    use sumimi_service::api::{self, AppState};
    use sumimi_service::config::{Config, Paths};
    use sumimi_service::pool::Db;
    use sumimi_service::writer::Writer;

    let dir = std::env::temp_dir().join(format!("sumimi-alloc-build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sumimi.duckdb");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());
    db.with_write(|conn| {
        conn.execute(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
             VALUES ('website-1', 'Example', 'example.com', now(), now(), 0, NULL)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let mut config = Config::from_env_and_flags(
        Paths::new(Some(dir.clone()), Some(path.clone()), "duckdb"),
        None, None, None, None, None, None,
    )
    .unwrap();
    config.jwt_secret = "test-secret".to_string();

    let (writer, _handle) = Writer::spawn_with(db.clone(), Duration::from_secs(600), 200_000, 1_000);
    let state = AppState::new(
        db.clone(),
        Arc::new(sumimi_service::instance::SqliteInstance::open_in_memory().unwrap()),
        writer.clone(),
        Arc::new(config),
        Arc::new("test-secret".to_string()),
    );
    let _router = api::router(state.clone());

    let request: sumimi_service::models::SendRequest = serde_json::from_str(BODY).unwrap();
    let raw = request.into_hit();

    let mut headers = HeaderMap::new();
    headers.insert("user-agent", AGENT.parse().unwrap());

    // Warm it up: the first call fills the website cache.
    for _ in 0..5 {
        let hit = api::collect::build_hit(
            &state,
            &headers,
            None,
            "website-1",
            HitKind::Pageview,
            &raw,
        )
        .unwrap();
        std::hint::black_box(hit);
    }

    let _before = allocated(); // the counter is reset by this reading
    for _ in 0..rounds() {
        let hit = api::collect::build_hit(
            &state,
            &headers,
            None,
            "website-1",
            HitKind::Pageview,
            &raw,
        )
        .unwrap();
        std::hint::black_box(hit);
    }
    let building = allocated() / rounds() as u64;

    println!("building the hit: {building} bytes per request");
    println!(
        "  the hit itself is {} bytes",
        std::mem::size_of::<sumimi_service::models::Hit>()
    );

    writer.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The price of one request, stage by stage.
#[tokio::test(flavor = "multi_thread")]
async fn the_price_of_one_request() {
    let _measuring = measuring().await;

    // 1. The parse: the body becomes a **tree** of `serde_json::Value`.
    let _before = allocated(); // the counter is reset by this reading
    for _ in 0..rounds() {
        let request: sumimi_service::models::SendRequest = serde_json::from_str(BODY).unwrap();
        std::hint::black_box(request);
    }
    let parsed = allocated() / rounds() as u64;

    // 2. The clone: `into_hit()` copies **the whole tree** into a new map.
    let _before = allocated(); // the counter is reset by this reading
    for _ in 0..rounds() {
        let request: sumimi_service::models::SendRequest = serde_json::from_str(BODY).unwrap();
        let raw = request.into_hit();
        std::hint::black_box(raw);
    }
    let with_clone = allocated() / rounds() as u64;

    // 3. One field, copied out: `str()` returns an owned `String`.
    let _before = allocated(); // the counter is reset by this reading
    for _ in 0..rounds() {
        let request: sumimi_service::models::SendRequest = serde_json::from_str(BODY).unwrap();
        let raw = request.into_hit();
        let url = raw.str("url");
        std::hint::black_box(url);
    }
    let with_a_field = allocated() / rounds() as u64;

    // 4. The user agent.
    let _before = allocated(); // the counter is reset by this reading
    for _ in 0..rounds() {
        let info = sumimi_service::ua::classify(AGENT);
        std::hint::black_box(info);
    }
    let user_agent = allocated() / rounds() as u64;

    // 5. What the same body would cost **parsed into a typed struct** — the
    //    alternative to the tree: no `Value`, no clone, no per-field copies.
    #[derive(serde::Deserialize)]
    #[allow(dead_code)] // it exists to be parsed: that is the measurement
    struct Typed {
        #[serde(default)]
        website: Option<String>,
        #[serde(default)]
        r#type: Option<String>,
        #[serde(default)]
        payload: Option<TypedPayload>,
    }
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct TypedPayload {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        referrer: Option<String>,
    }
    let _before = allocated(); // the counter is reset by this reading
    for _ in 0..rounds() {
        let typed: Typed = serde_json::from_str(BODY).unwrap();
        std::hint::black_box(typed);
    }
    let typed = allocated() / rounds() as u64;

    println!("the price of one request:");
    println!("  the body is                    {} bytes", BODY.len());
    println!("  parsed into a tree             {parsed} bytes");
    println!("  … and cloned into a map        {} bytes", with_clone - parsed);
    println!("  … and one field copied out     {} bytes", with_a_field - with_clone);
    println!("  the user agent                 {user_agent} bytes");
    println!("  parsed into a typed struct     {typed} bytes");
    println!(
        "  the tree's share               {} bytes",
        with_clone.saturating_sub(typed)
    );

    // The room the deployment reference assumes: 7.8 KB per request. The
    // stages above are only part of it — the writer's batch and the queue are
    // the rest.
    assert!(
        parsed < 4 * 1024,
        "the parse alone must stay under 4 KB per request: {parsed}"
    );
}

/// The whole path: the body in, the hit in the queue, the row in the file.
///
/// This is the number that matters — it is what the deployment reference
/// assumes (7.8 KB per request).
#[tokio::test(flavor = "multi_thread")]
async fn the_price_of_one_request_end_to_end() {
    let _measuring = measuring().await;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use std::time::Duration;
    use sumimi_service::api::{self, AppState};
    use sumimi_service::config::{Config, Paths};
    use sumimi_service::pool::Db;
    use sumimi_service::writer::Writer;
    use tower::ServiceExt;

    let dir = std::env::temp_dir().join(format!("sumimi-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sumimi.duckdb");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());
    db.with_write(|conn| {
        conn.execute(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
             VALUES ('website-1', 'Example', 'example.com', now(), now(), 0, NULL)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let mut config = Config::from_env_and_flags(
        Paths::new(Some(dir.clone()), Some(path.clone()), "duckdb"),
        None, None, None, None, None, None,
    )
    .unwrap();
    config.jwt_secret = "test-secret".to_string();

    let (writer, _handle) = Writer::spawn(db.clone(), Duration::from_millis(200), 1000);
    let state = AppState::new(
        db.clone(),
        Arc::new(sumimi_service::instance::SqliteInstance::open_in_memory().unwrap()),
        writer.clone(),
        Arc::new(config),
        Arc::new("test-secret".to_string()),
    );
    let app = api::router(state);

    let request = |app: &axum::Router| {
        let app = app.clone();
        async move {
            let request = Request::builder()
                .method("POST")
                .uri("/api/send")
                .header("content-type", "application/json")
                .body(Body::from(BODY))
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
    };

    // Warm it up: the first requests fill the caches and open the file.
    for _ in 0..20 {
        request(&app).await;
    }
    while writer.queued() > 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    let _before = allocated(); // the counter is reset by this reading
    for _ in 0..rounds() {
        request(&app).await;
    }
    // The writer's batch is part of the price: wait for it to drain.
    while writer.queued() > 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    let total = allocated() / rounds() as u64;

    println!("the whole path: {total} bytes per request");
    writer.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same path **without the write** — the queue takes the hit and nothing
/// else happens.
///
/// The difference is the write path's share: the batch, the copy into DuckDB,
/// and everything the writer does with a row.
#[tokio::test(flavor = "multi_thread")]
async fn the_price_without_the_write_path() {
    let _measuring = measuring().await;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use std::time::Duration;
    use sumimi_service::api::{self, AppState};
    use sumimi_service::config::{Config, Paths};
    use sumimi_service::pool::Db;
    use sumimi_service::writer::Writer;
    use tower::ServiceExt;

    let dir = std::env::temp_dir().join(format!("sumimi-alloc-nowrite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sumimi.duckdb");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());
    db.with_write(|conn| {
        conn.execute(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
             VALUES ('website-1', 'Example', 'example.com', now(), now(), 0, NULL)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let mut config = Config::from_env_and_flags(
        Paths::new(Some(dir.clone()), Some(path.clone()), "duckdb"),
        None, None, None, None, None, None,
    )
    .unwrap();
    config.jwt_secret = "test-secret".to_string();

    // A writer that never flushes: the hit goes into the queue and stays
    // there, so the measurement is the collection path alone.
    let (writer, _handle) = Writer::spawn_with(db.clone(), Duration::from_secs(600), 200_000, 1_000);
    let state = AppState::new(
        db.clone(),
        Arc::new(sumimi_service::instance::SqliteInstance::open_in_memory().unwrap()),
        writer.clone(),
        Arc::new(config),
        Arc::new("test-secret".to_string()),
    );
    let app = api::router(state);

    for _ in 0..20 {
        let request = Request::builder()
            .method("POST")
            .uri("/api/send")
            .header("content-type", "application/json")
            .body(Body::from(BODY))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let _before = allocated(); // the counter is reset by this reading
    forget_histogram();
    for _ in 0..rounds() {
        let request = Request::builder()
            .method("POST")
            .uri("/api/send")
            .header("content-type", "application/json")
            .body(Body::from(BODY))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let collection = allocated() / rounds() as u64;

    println!("the collection path alone: {collection} bytes per request");
    println!("  the biggest allocations per request:");
    for (size, count, bytes) in histogram(rounds() as u64).into_iter().take(8) {
        println!("    {size:>8} B × {count:>4} = {bytes:>7} B");
    }
    writer.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
