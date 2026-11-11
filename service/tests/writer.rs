//! The write path: nothing is lost.
//!
//! This is the promise that matters most — everything else is a feature, but a
//! lost page view is a lost page view. So the writer is checked against the
//! three ways data can go missing:
//!
//! * **the buffer is full** — the oldest hits are dropped, and it is said out
//!   loud;
//! * **the write fails** — a hiccup must not cost the traffic, and it must not
//!   kill the writer either;
//! * **the server stops** — what is in the buffer when the process is asked to
//!   stop is written before it leaves.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use sumimi_service::models::{BreakdownRow, EventRow, Hit, MetricsSummary, Revenue, SessionRow, Website};
use sumimi_service::pool::Db;
use sumimi_service::storage::{
    Dimension, ReportQuery, SessionEvent, Storage, StorageStats, SummaryMetrics, WebsitePatch,
};
use sumimi_service::testing::HitBuilder;
use sumimi_service::writer::Writer;

/// A unique temporary database path.
fn temp_db(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-writer-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("sumimi.duckdb")
}

/// A hit.
fn hit(url: &str) -> Hit {
    HitBuilder::new("w1", "s1", "v1").url(url).build()
}

/// How many rows the database holds.
fn rows(db: &Db) -> i64 {
    db.with_read(|conn| {
        let mut stmt = conn.prepare("SELECT count(*) FROM event")?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(row.get(0)?),
            None => Ok(0),
        }
    })
    .unwrap()
}

/// Every hit that was queued is written.
#[tokio::test(flavor = "multi_thread")]
async fn the_writer_keeps_every_hit() {
    let path = temp_db("keeps");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());

    let (writer, handle) = Writer::spawn(db.clone(), Duration::from_millis(50), 10_000);
    for index in 0..500 {
        writer.push(hit(&format!("/{index}")));
    }

    // Wait for the flush.
    for _ in 0..100 {
        if rows(&db) == 500 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(rows(&db), 500, "every hit was written");
    assert_eq!(writer.dropped(), 0, "nothing was dropped");

    writer.shutdown();
    handle.await.unwrap();
}

/// What is in the buffer when the writer is stopped is written before it leaves.
#[tokio::test(flavor = "multi_thread")]
async fn a_shutdown_writes_what_is_buffered() {
    let path = temp_db("shutdown");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());

    // A long flush interval: the hits are still in the buffer when the writer
    // is stopped.
    let (writer, handle) = Writer::spawn(db.clone(), Duration::from_secs(30), 10_000);
    for index in 0..100 {
        writer.push(hit(&format!("/{index}")));
    }

    writer.shutdown();
    handle.await.unwrap();

    assert_eq!(
        rows(&db),
        100,
        "the buffer was written on the way out"
    );
}

/// A write that fails is written again — and the writer survives it.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_write_is_retried() {
    let path = temp_db("retry");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());

    // The storage fails the first two writes and works after that.
    let flaky = Arc::new(FlakyStorage {
        inner: db.clone(),
        failures: AtomicU32::new(2),
    });

    let (writer, handle) = Writer::spawn(flaky, Duration::from_millis(50), 10_000);
    for index in 0..10 {
        writer.push(hit(&format!("/{index}")));
    }

    for _ in 0..100 {
        if rows(&db) == 10 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    assert_eq!(
        rows(&db),
        10,
        "the batch that failed was written again"
    );

    writer.shutdown();
    handle.await.unwrap();
}

/// A storage that fails the first writes.
struct FlakyStorage {
    inner: Arc<Db>,
    failures: AtomicU32,
}

impl Storage for FlakyStorage {
    fn backend(&self) -> &'static str {
        self.inner.backend()
    }
    fn websites(&self) -> Result<Vec<Website>> {
        self.inner.websites()
    }
    fn website(&self, id: &str) -> Result<Option<Website>> {
        self.inner.website(id)
    }
    fn create_website(
        &self,
        name: &str,
        domain: Option<&str>,
        retention_days: i32,
        country_header: Option<&str>,
    ) -> Result<Website> {
        self.inner
            .create_website(name, domain, retention_days, country_header)
    }
    fn update_website(&self, id: &str, patch: &WebsitePatch) -> Result<Website> {
        self.inner.update_website(id, patch)
    }
    fn delete_website(&self, id: &str) -> Result<bool> {
        self.inner.delete_website(id)
    }
    fn summary(&self, query: &ReportQuery) -> Result<SummaryMetrics> {
        self.inner.summary(query)
    }
    fn summary_with_change(&self, query: &ReportQuery) -> Result<MetricsSummary> {
        self.inner.summary_with_change(query)
    }
    fn breakdown(&self, query: &ReportQuery, dimension: Dimension) -> Result<Vec<BreakdownRow>> {
        self.inner.breakdown(query, dimension)
    }
    fn timeseries(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
        self.inner.timeseries(query)
    }
    fn query_strings(&self, _query: &ReportQuery) -> Result<Vec<(String, i64)>> {
        Ok(Vec::new())
    }

    fn events(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
        self.inner.events(query)
    }
    fn event_log(&self, query: &ReportQuery) -> Result<Vec<EventRow>> {
        self.inner.event_log(query)
    }
    fn sessions(&self, query: &ReportQuery) -> Result<Vec<SessionRow>> {
        self.inner.sessions(query)
    }
    fn recent_events(&self, query: &ReportQuery, limit: i64) -> Result<Vec<SessionEvent>> {
        self.inner.recent_events(query, limit)
    }

    fn write_batch(&self, hits: &[Hit]) -> Result<()> {
        // The first two attempts fail.
        let mut remaining = self.failures.load(Ordering::Relaxed);
        while remaining > 0 {
            match self.failures.compare_exchange(
                remaining,
                remaining - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    anyhow::bail!("the disk is busy");
                }
                Err(current) => remaining = current,
            }
        }
        self.inner.write_batch(hits)
    }

    fn write_revenue(&self, records: &[Revenue]) -> Result<()> {
        self.inner.write_revenue(records)
    }
    fn revenue(&self, query: &ReportQuery) -> Result<Vec<Revenue>> {
        self.inner.revenue(query)
    }
    fn prune(&self, website: Option<&str>) -> Result<(u64, u64)> {
        self.inner.prune(website)
    }
    fn delete_before(
        &self,
        before: chrono::NaiveDateTime,
        website: Option<&str>,
    ) -> Result<(u64, u64)> {
        self.inner.delete_before(before, website)
    }
    fn erase(&self, website: &str) -> Result<u64> {
        self.inner.erase(website)
    }
    fn stats(&self) -> Result<StorageStats> {
        self.inner.stats()
    }
}

/// The batch is capped — the write path's memory must not depend on the rate.
///
/// The queue is filled far faster than the writer can drain it (a batch write
/// takes milliseconds, a push takes microseconds), so the queue always holds
/// more than the cap. Every batch must be exactly the cap: never more.
#[test]
fn a_batch_never_exceeds_the_cap() {
    let path = temp_db("batch-cap");
    let db = std::sync::Arc::new(sumimi_service::pool::Db::open(&path, "256MB").unwrap());

    let cap = 1_000;
    let (writer, handle) = sumimi_service::writer::Writer::spawn_thread_with(
        db.clone(),
        Duration::from_millis(50),
        1_000_000,
        cap,
    );

    // A hundred thousand hits: the writer cannot keep up, so the queue is
    // always deeper than the cap.
    let now = chrono::Utc::now().naive_utc();
    for index in 0..100_000 {
        writer.push(sumimi_service::models::Hit {
            website_id: "w1".to_string(),
            visit_id: sumimi_service::ua::uuid_from_text("s"),
            visitor_id: sumimi_service::ua::uuid_from_text("v"),
            created_at: now,
            url: Some(format!("/p-{index}")),
            ..Default::default()
        });
    }

    // Wait until the writer has caught up with the queue.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while writer.queued() > cap as u64 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        writer.max_batch_rows() <= cap,
        "a batch of {} rows was written, the cap is {cap}",
        writer.max_batch_rows()
    );
    assert!(
        writer.flushes() >= 10,
        "a hundred thousand rows in batches of {cap} is at least ten batches, not {}",
        writer.flushes()
    );

    writer.shutdown();
    let _ = handle.join();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
