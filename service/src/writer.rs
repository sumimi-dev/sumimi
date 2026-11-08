//! The single-writer pipeline.
//!
//! HTTP handlers never touch the database. They hand a [`Hit`] to the [`Writer`], which
//! buffers it in memory and hands batches to one dedicated thread that owns the DuckDB
//! connection. Batches are flushed every few seconds inside a single transaction.
//!
//! Two properties matter:
//!   * **One writer.** DuckDB has a single-writer model anyway; owning the connection in
//!     one thread removes all locking from the request path.
//!   * **Bounded memory.** If the buffer overflows (a traffic spike, a slow disk), the
//!     *oldest* hits are dropped and counted. Analytics may lose a few samples; the server
//!     must not die — and it says so in the log, because a lost hit is a lost hit.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::models::Hit;
use crate::storage::Storage;

/// Shared, bounded buffer between the request handlers and the writer thread.
#[derive(Debug)]
struct Buffer {
    queue: Mutex<VecDeque<Hit>>,
    signal: Condvar,
    dropped: AtomicU64,
    /// Set when the writer should stop.
    stopped: AtomicBool,
}

impl Buffer {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            signal: Condvar::new(),
            dropped: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        }
    }

    /// Ask the writer to stop (it flushes what it has and returns).
    fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.signal.notify_all();
    }

    /// Has the writer been asked to stop?
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Push a hit, if there is room for it.
    ///
    /// Returns whether the hit was **taken**. When the queue is full it is
    /// **not**: the events already in the queue were acknowledged to their
    /// clients, and dropping one of those would lose data somebody believes
    /// they sent. So the server refuses the new one instead — and the caller
    /// answers `503`, so the client knows and can send it again.
    ///
    /// That is the whole invariant: **what we acknowledged, we have.**
    fn push(&self, hit: Hit, capacity: usize) -> bool {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() >= capacity {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        queue.push_back(hit);
        self.signal.notify_one();
        true
    }

    /// Take everything currently buffered.
    fn drain(&self) -> Vec<Hit> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.drain(..).collect()
    }

    /// Take **at most** `max` rows from the front — the rest stay in the
    /// queue, in order, for the next batch.
    fn take_front(&self, max: usize) -> Vec<Hit> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let take = max.min(queue.len());
        queue.drain(..take).collect()
    }
}

/// The writer handle handed to the HTTP layer.
#[derive(Clone)]
pub struct Writer {
    buffer: Arc<Buffer>,
    capacity: usize,
    /// The version of the data.
    ///
    /// It changes whenever a batch is written, and the report cache carries it:
    /// a cached answer from an older version is a miss. That is what keeps the
    /// dashboard from showing numbers from before the last write, while two
    /// people looking at the same dashboard only run the query once.
    version: Arc<AtomicU64>,
    /// How many batches have been written.
    flushes: Arc<AtomicU64>,
    /// How long the last write took, in milliseconds.
    last_flush_ms: Arc<AtomicU64>,
    /// The last few write durations, in milliseconds.
    ///
    /// A ring of the most recent ones — enough to compute a median and a tail
    /// without keeping every measurement forever.
    flush_history: Arc<Mutex<Vec<u64>>>,
    /// The most rows in one batch that was written.
    max_batch_rows: Arc<AtomicU64>,
    /// The most rows in one batch.
    ///
    /// **The batch is what the write path costs in memory.** The flush interval
    /// says how *often* we write; the cap says how *much* — and the memory of a
    /// batch is its rows. At 170 000 views a second, two seconds of traffic is
    /// 340 000 rows, which is 170 MB of `Hit` structs in one transaction — the
    /// measured peak RSS of the soak test was 1.5 GB for exactly this reason.
    ///
    /// With the cap, the write path's memory does not depend on the rate: a
    /// spike is written in more, smaller batches.
    max_batch: usize,
}

/// The most rows in one batch, unless the configuration says otherwise.
///
/// Ten thousand rows is about 5 MB of `Hit` structs and 50 ms of writing —
/// small enough that the dashboard's queries do not wait behind it (the writer
/// holds DuckDB's single write connection), and big enough that the write
/// throughput is not hurt.
pub const DEFAULT_MAX_BATCH: usize = 10_000;

impl Writer {
    /// Spawn the writer thread and return the handle.
    pub fn spawn(
        storage: Arc<dyn Storage>,
        flush_interval: Duration,
        capacity: usize,
    ) -> (Writer, tokio::task::JoinHandle<()>) {
        Self::spawn_with(storage, flush_interval, capacity, DEFAULT_MAX_BATCH)
    }

    /// The writer, with the batch cap.
    pub fn spawn_with(
        storage: Arc<dyn Storage>,
        flush_interval: Duration,
        capacity: usize,
        max_batch: usize,
    ) -> (Writer, tokio::task::JoinHandle<()>) {
        let writer = Writer::new(capacity, max_batch);
        let buffer = writer.buffer.clone();
        let writer_for_thread = writer.clone();

        let handle = tokio::task::spawn_blocking(move || {
            writer_loop(storage, buffer, flush_interval, capacity, writer_for_thread);
        });

        (writer, handle)
    }

    /// Spawn the writer on its own OS thread.
    ///
    /// The same pipeline as [`Writer::spawn`], but not tied to a Tokio runtime —
    /// for the tools that drive the write path outside a server (the memory
    /// budget, the soak test). The handle can be **joined**, so the caller
    /// knows the last flush has landed.
    pub fn spawn_thread(
        storage: Arc<dyn Storage>,
        flush_interval: Duration,
        capacity: usize,
    ) -> (Writer, std::thread::JoinHandle<()>) {
        Self::spawn_thread_with(storage, flush_interval, capacity, DEFAULT_MAX_BATCH)
    }

    /// The writer on its own thread, with the batch cap.
    pub fn spawn_thread_with(
        storage: Arc<dyn Storage>,
        flush_interval: Duration,
        capacity: usize,
        max_batch: usize,
    ) -> (Writer, std::thread::JoinHandle<()>) {
        let writer = Writer::new(capacity, max_batch);
        let buffer = writer.buffer.clone();
        let writer_for_thread = writer.clone();

        let handle = std::thread::spawn(move || {
            writer_loop(storage, buffer, flush_interval, capacity, writer_for_thread);
        });

        (writer, handle)
    }

    /// Build the handle, without starting anything.
    ///
    /// **Public for the measurement** ✓ — `service/tests/allocations.rs`
    /// measures what a push costs as the queue grows ✓ (that is the mechanism
    /// behind "the memory follows the rate" ✓ — see `MEMORY.md` §6 ✓).
    pub fn new(capacity: usize, max_batch: usize) -> Writer {
        Writer {
            max_batch: max_batch.max(1),
            buffer: Arc::new(Buffer::new()),
            capacity,
            // The clock, so a restart invalidates the cache that is still on disk.
            version: Arc::new(AtomicU64::new(
                chrono::Utc::now().timestamp_millis().max(0) as u64,
            )),
            flushes: Arc::new(AtomicU64::new(0)),
            last_flush_ms: Arc::new(AtomicU64::new(0)),
            max_batch_rows: Arc::new(AtomicU64::new(0)),
            flush_history: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Queue a hit for the next flush.
    ///
    /// Returns whether the hit was **taken**. `false` means the server is over
    /// capacity: the hit was not stored, and the caller has to tell the client
    /// — otherwise the client walks away thinking everything was stored, and
    /// the data is gone.
    pub fn push(&self, hit: Hit) -> bool {
        self.buffer.push(hit, self.capacity)
    }

    /// The most rows in one batch.
    pub fn max_batch(&self) -> usize {
        self.max_batch
    }

    /// Number of hits waiting in memory.
    pub fn queued(&self) -> u64 {
        self.buffer.queue.lock().unwrap_or_else(|e| e.into_inner()).len() as u64
    }

    /// Number of hits dropped because the buffer was full.
    pub fn dropped(&self) -> u64 {
        self.buffer.dropped.load(Ordering::Relaxed)
    }

    /// The version of the data.
    ///
    /// It changes whenever a batch is written.
    pub fn version(&self) -> &AtomicU64 {
        &self.version
    }

    /// How many batches have been written since the writer started.
    pub fn flushes(&self) -> u64 {
        self.flushes.load(Ordering::Relaxed)
    }

    /// The most rows in one batch since the writer started.
    ///
    /// It must never be more than the cap — that is the whole point of the
    /// cap: the write path's memory does not depend on the traffic.
    pub fn max_batch_rows(&self) -> usize {
        self.max_batch_rows.load(Ordering::Relaxed) as usize
    }

    /// How long the last write took, in milliseconds.
    pub fn last_flush_ms(&self) -> u64 {
        self.last_flush_ms.load(Ordering::Relaxed)
    }

    /// A percentile of the write durations, in milliseconds.
    pub fn flush_percentile(&self, percent: usize) -> u64 {
        let history = self.flush_history();
        if history.is_empty() {
            return 0;
        }
        let mut sorted = history;
        sorted.sort_unstable();
        let index = (percent * sorted.len()) / 100;
        sorted[index.min(sorted.len() - 1)]
    }

    /// The last few write durations, in milliseconds (oldest first).
    pub fn flush_history(&self) -> Vec<u64> {
        self.flush_history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Record a write duration.
    fn record_flush(&self, millis: u64) {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        self.last_flush_ms.store(millis, Ordering::Relaxed);
        let mut history = self.flush_history.lock().unwrap_or_else(|e| e.into_inner());
        // A ring of the most recent measurements: enough for a median and a
        // tail, without growing without bound on a long-running server.
        const KEEP: usize = 1024;
        if history.len() == KEEP {
            history.remove(0);
        }
        history.push(millis);
    }

    /// Stop the writer thread.
    ///
    /// The thread flushes whatever is still buffered and returns, so the process (and
    /// the Tokio runtime) can shut down cleanly.
    pub fn shutdown(&self) {
        self.buffer.stop();
    }
}

/// The writer thread: wait for hits, drain, persist.
///
/// The event table is **append only**, so this is a bulk append — the fastest path the
/// engine has (measured 641x faster than an upsert, see issue #17).
///
/// **A failed write does not end the thread.** A hiccup — a locked file, a disk that
/// hiccups, a database that is briefly busy — must not cost the site owner the
/// traffic. The batch that failed is **kept** and written again, with a pause
/// between the attempts. The thread only gives up when it is told to stop, and
/// then it writes what is left before it leaves.
fn writer_loop(
    storage: Arc<dyn Storage>,
    buffer: Arc<Buffer>,
    flush_interval: Duration,
    _capacity: usize,
    writer: Writer,
) {
    let max_batch = writer.max_batch;
    // The batch that could not be written yet. It stays here until it is.
    let mut pending: Vec<Hit> = Vec::new();
    let mut failures = 0u32;
    let mut reported_dropped = 0u64;

    loop {
        if buffer.is_stopped() {
            // Flush what is left — and what could not be written — then leave.
            let mut batch = std::mem::take(&mut pending);
            batch.extend(buffer.drain());
            if !batch.is_empty() {
                let started = std::time::Instant::now();
                if let Err(err) = storage.write_batch(&batch) {
                    tracing::error!(
                        error = %err,
                        count = batch.len(),
                        "the last batch could not be written — it is lost"
                    );
                }
                writer.record_flush(started.elapsed().as_millis() as u64);
            }
            tracing::debug!("sumimi writer stopped");
            break;
        }

        // Block until something arrives or the flush interval elapses.
        {
            let queue = buffer.queue.lock().unwrap_or_else(|e| e.into_inner());
            if queue.is_empty() && pending.is_empty() {
                let _ = buffer
                    .signal
                    .wait_timeout(queue, flush_interval)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }

        // The batch is **capped**: the memory of the write path is the rows in
        // it, and the rows in it must not depend on how fast the traffic is.
        // Whatever does not fit stays in the queue for the next round — and
        // the loop comes straight back for it (the queue is not empty, so
        // there is no wait).
        let mut batch = std::mem::take(&mut pending);
        let room = max_batch.saturating_sub(batch.len());
        batch.extend(buffer.take_front(room));
        if batch.is_empty() {
            continue;
        }

        let count = batch.len();
        writer.max_batch_rows.fetch_max(count as u64, Ordering::Relaxed);
        let started = std::time::Instant::now();
        match storage.write_batch(&batch) {
            Ok(()) => {
                failures = 0;
                // The data changed: every cached report is stale now.
                writer.version().fetch_add(1, Ordering::Relaxed);
                writer.record_flush(started.elapsed().as_millis() as u64);
                tracing::debug!(count, "flushed batch");
            }
            Err(err) => {
                // Keep the batch and try again. The pause grows with the number of
                // failures, so a database that is down is not hammered.
                failures += 1;
                let pause = Duration::from_millis(250u64.saturating_mul(failures as u64).min(8_000));
                tracing::error!(
                    error = %err,
                    count,
                    failures,
                    "failed to write batch — keeping it and trying again"
                );
                pending = batch;
                std::thread::sleep(pause);
            }
        }

        // Say it when hits are dropped: the buffer was full, so the **oldest**
        // hits are gone. That is data the owner cannot get back — it is said
        // once per batch, not once per hit.
        let dropped = writer.dropped();
        if dropped > reported_dropped {
            tracing::warn!(
                dropped,
                "hits were dropped — the queue was full (raise SUMIMI_MAX_QUEUE)"
            );
            reported_dropped = dropped;
        }
    }
}
