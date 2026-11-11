//! The memory budget: put real traffic through the real write path, and see
//! what the process costs.
//!
//! The engine's own number (`duckdb_memoryBytes`) is only part of the story.
//! What a small VPS runs out of is the **whole process's resident set** — the
//! binary's code pages, the writer's queue, the pages the allocator has not
//! handed back, and the engine's share. So the budget is asserted against the
//! process's peak RSS ([`crate::rss::peak_rss`]), not against the engine.
//!
//! The two budgets the product is specified against are in
//! [`crate::config::budget`]. Every run of this module is against one of them.
//!
//! The traffic is **realistic**, not uniform: a repeating set of visitors, a
//! repeating set of pages, real referrers. Uniform data would compress into a
//! dictionary and the measurement would be a lie.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::models::Hit;
use crate::pool::Db;
use crate::rss;
use crate::storage::Storage;
use crate::writer::Writer;

/// How many pages the synthetic site has.
const PAGES: usize = 200;
/// How many distinct visitors.
const VISITORS: usize = 5_000;
/// The referrers the traffic comes from.
const REFERRERS: &[(&str, &str)] = &[
    ("www.google.com", "search"),
    ("news.ycombinator.com", "social"),
    ("t.co", "social"),
    ("mail.google.com", "email"),
    ("example.com", "direct"),
];

/// What one budget run cost.
#[derive(Debug, Clone)]
pub struct BudgetReport {
    /// The engine's memory limit, as configured.
    pub engine_limit: String,
    /// How many threads the engine was given.
    pub threads: usize,
    /// How many events were pushed.
    pub events: usize,
    /// How many batches were written.
    pub flushes: u64,
    /// The write durations, in milliseconds (oldest first).
    pub flush_ms: Vec<u64>,
    /// How long the whole run took, in seconds.
    pub seconds: f64,
    /// What the engine says it is holding, in bytes.
    pub engine_bytes: u64,
    /// The process's peak resident set, in bytes (`None` off Linux).
    ///
    /// This is the **exact** high-water mark, read from `/proc/self/status`.
    /// It is the number the budget is asserted against.
    pub peak_rss: Option<u64>,
    /// The highest resident set seen while sampling, in bytes.
    ///
    /// Available everywhere the process can be measured — a sampled maximum,
    /// so it is a lower bound on the true peak. For information.
    pub peak_rss_sampled: Option<u64>,
    /// The process's resident set right now, in bytes (`None` off Linux).
    pub current_rss: Option<u64>,
    /// How big the data file ended up, in bytes.
    pub disk_bytes: u64,
    /// How many events were dropped because the queue was full.
    pub dropped: u64,
}

impl BudgetReport {
    /// A one-line summary, for a CI log.
    pub fn summary(&self) -> String {
        let mb = |bytes: Option<u64>| match bytes {
            Some(bytes) => format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0)),
            None => "n/a".to_string(),
        };
        format!(
            "engine={} threads={} events={} flushes={} flush p50={}ms p99={}ms max={}ms \
             engine_holds={} peak_rss={} peak_sampled={} disk={:.1} MB dropped={}",
            self.engine_limit,
            self.threads,
            self.events,
            self.flushes,
            self.percentile(50),
            self.percentile(99),
            self.flush_ms.iter().copied().max().unwrap_or(0),
            mb(Some(self.engine_bytes)),
            mb(self.peak_rss),
            mb(self.peak_rss_sampled),
            self.disk_bytes as f64 / (1024.0 * 1024.0),
            self.dropped,
        )
    }

    /// A percentile of the write durations, in milliseconds.
    pub fn percentile(&self, percent: usize) -> u64 {
        if self.flush_ms.is_empty() {
            return 0;
        }
        let mut sorted = self.flush_ms.clone();
        sorted.sort_unstable();
        let index = (percent * sorted.len()) / 100;
        sorted[index.min(sorted.len() - 1)]
    }
}

/// Push `events` events through the real write path and measure what it costs.
///
/// The database is created under `dir`; the caller cleans it up.
pub fn run(engine_limit: &str, events: usize, dir: &Path) -> anyhow::Result<BudgetReport> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    let path = dir.join("sumimi.duckdb");

    let started = Instant::now();
    let db = Arc::new(Db::open_with(&path, engine_limit, "32MB", "2GB", 0)?);
    let website = db.create_website("Budget", Some("example.com"), 0, None)?.id;

    // Watch the process's memory while it runs. On Linux this is the exact
    // high-water mark; elsewhere it is a sampled maximum.
    let watcher = rss::watch_peak(Duration::from_millis(25));

    // The writer's own settings, as the server runs them.
    let (writer, handle) = Writer::spawn_thread(db.clone(), Duration::from_millis(500), 100_000);

    let now = chrono::Utc::now().naive_utc();
    let mut pushed = 0usize;
    while pushed < events {
        let chunk = (events - pushed).min(5_000);
        for index in 0..chunk {
            let number = pushed + index;
            writer.push(hit(&website, number, now));
        }
        pushed += chunk;
        // Let the writer keep up: the queue must not overflow, because a
        // dropped event would make the measurement a lie.
        let mut waited = 0u64;
        while writer.queued() > 2_000 && waited < 30_000 {
            std::thread::sleep(Duration::from_millis(5));
            waited += 5;
        }
    }

    // Wait for the last batch to land.
    let mut waited = 0u64;
    while (writer.queued() > 0 || writer.flushes() == 0) && waited < 60_000 {
        std::thread::sleep(Duration::from_millis(10));
        waited += 10;
    }

    let flushes = writer.flushes();
    let flush_ms = writer.flush_history();
    let dropped = writer.dropped();
    // Stop the writer and wait for it: the last batch has to be on disk before
    // the file is measured.
    writer.shutdown();
    let _ = handle.join();

    let stats = db.stats()?;
    // Checkpoint first: the events are in the write-ahead log until the file
    // is closed, and the disk number has to be the file's size, not the WAL's.
    let _ = db.with_read(|conn| -> anyhow::Result<()> {
        conn.execute_batch("CHECKPOINT")?;
        Ok(())
    });
    let disk_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    // The exact peak on Linux; the sampled maximum everywhere else. Only the
    // exact one is asserted against — the sampled one is for information.
    let peak_rss = rss::peak_rss();
    let peak_rss_sampled = Some(watcher.peak());

    Ok(BudgetReport {
        engine_limit: engine_limit.to_string(),
        threads: stats.threads as usize,
        events,
        flushes,
        flush_ms,
        seconds: started.elapsed().as_secs_f64(),
        engine_bytes: stats.memory_bytes as u64,
        peak_rss,
        peak_rss_sampled,
        current_rss: rss::current_rss_portable(),
        disk_bytes,
        dropped,
    })
}

/// One page view of the synthetic site.
///
/// The fields repeat the way real traffic does: the same pages, the same
/// visitors, the same referrers — so the storage engine can compress what a
/// real one would compress, and the measurement means something.
fn hit(website: &str, number: usize, now: chrono::NaiveDateTime) -> Hit {
    let page = number % PAGES;
    let visitor = number % VISITORS;
    let (referrer, channel) = REFERRERS[number % REFERRERS.len()];
    crate::testing::HitBuilder::new(
        website,
        &format!("session-{}", number / 4),
        &format!("visitor-{visitor}"),
    )
    .at(now)
    .url(format!("/blog/post-{page}"))
    .url_query("utm_source=twitter&utm_medium=cpc")
    .title(format!("Post {page} — the title of the page"))
    .referrer(referrer)
    .channel(channel)
    .referrer_path("/item")
    .referrer_query("?id=1")
    .referrer_name(crate::channels::referrer_name(Some(referrer)).unwrap_or_default())
    .hostname("example.com")
    .browser("Chrome")
    .browser_version("120")
    .os("macOS")
    .os_version("10.15")
    .device("desktop")
    .screen("1920x1080")
    .language("en-US")
    .country("DE")
    .region("Berlin")
    .city("Berlin")
    .latitude(52.52)
    .longitude(13.405)
    .utm_source("twitter")
    .utm_medium("cpc")
    .utm_campaign("spring")
    .utm_content("cta")
    .utm_term("term")
    .lcp(1200.0)
    .inp(80.0)
    .cls(0.02)
    .fcp(700.0)
    .ttfb(90.0)
    .build()
}
