//! Configuration for the Sumimi service.
//!
//! Everything is overridable by environment variable so a deployment is a systemd unit
//! plus a small `EnvironmentFile`, never a config file in the repository.

use std::path::PathBuf;

use anyhow::Context;

/// Where the dashboard frontend is served from by default.
///
/// jsDelivr serves the public GitHub repository directly, so the frontend can be
/// updated (or pinned to a release tag) without touching the backend.
pub const CDN_APP_URL: &str = "https://cdn.jsdelivr.net/gh/sumimi-dev/sumimi@main/js";

/// Where an instance keeps its files.
///
/// An instance is a **directory**. Everything it owns lives inside it, so backing it
/// up, moving it and deleting it are all one operation on one path:
///
/// ```text
/// sumimi/
///   data.duckdb      the data plane — the events
///   instance.sqlite  the control plane — the accounts, the log, the cache, the chat
///   logs/            the server's log files
///   acme/            the TLS certificate cache
///   jwt-secret       the signing secret
/// ```
#[derive(Debug, Clone)]
pub struct Paths {
    /// The instance directory.
    pub dir: PathBuf,
    /// The data plane: the DuckDB file, or the SQLite file.
    pub data: PathBuf,
    /// The control plane: the accounts, the log, the cache, the snapshots, the chat.
    pub instance: PathBuf,
    /// The server's log files.
    pub logs: PathBuf,
    /// The TLS certificate cache.
    pub acme: PathBuf,
    /// The file the signing secret is kept in.
    pub secret: PathBuf,
}

impl Paths {
    /// Work out where everything lives.
    ///
    /// `dir` is the instance directory. `data` overrides the data file — for the
    /// rare case of keeping the events on a bigger disk than the rest.
    pub fn new(dir: Option<PathBuf>, data: Option<PathBuf>, backend: &str) -> Self {
        let default_name = match backend.to_ascii_lowercase().as_str() {
            "sqlite" => "data.sqlite",
            _ => "data.duckdb",
        };

        let (dir, data) = match (dir, data) {
            (Some(dir), data) => {
                let data = data.unwrap_or_else(|| dir.join(default_name));
                (dir, data)
            }
            (None, Some(data)) => {
                // The old way of asking for a file: the instance lives next to it.
                let dir = data
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                (dir, data)
            }
            (None, None) => {
                let dir = PathBuf::from("sumimi");
                let data = dir.join(default_name);
                (dir, data)
            }
        };

        Self {
            instance: dir.join("instance.sqlite"),
            logs: dir.join("logs"),
            acme: dir.join("acme"),
            secret: dir.join("jwt-secret"),
            dir,
            data,
        }
    }

    /// Create the directories the instance needs.
    pub fn prepare(&self) -> anyhow::Result<()> {
        for path in [&self.dir, &self.logs, &self.acme] {
            std::fs::create_dir_all(path).map_err(|e| {
                anyhow::anyhow!("creating the directory {}: {e}", path.display())
            })?;
        }
        Ok(())
    }
}

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where the instance keeps its files.
    pub paths: Paths,
    /// Address to bind the HTTP server to.
    pub host: String,
    /// Port to bind the HTTP server to.
    pub port: u16,
    /// Secret used to sign JWTs. Generated and persisted on first run when absent.
    pub jwt_secret: String,
    /// Admin username created on first run.
    pub admin_user: String,
    /// Admin password created on first run (`None` = generate + print once).
    pub admin_password: Option<String>,
    /// DuckDB `memory_limit`. The ceiling for the buffer manager — *not* an
    /// allocation. Defaults to a quarter of the machine's RAM, capped at
    /// [`MEMORY_LIMIT_MAX`] (256 MB).
    pub duckdb_memory_limit: String,
    /// DuckDB `allocator_flush_threshold`: how much freed memory the allocator
    /// keeps before returning it to the OS. Kept low so a small box stays small.
    pub duckdb_flush_threshold: String,
    /// DuckDB `max_temp_directory_size`: how much disk a query may spill.
    pub duckdb_temp_limit: String,
    /// DuckDB `threads`. `0` = derive it from the memory limit.
    ///
    /// This is not a performance knob only: a parallel `COUNT(DISTINCT …)` builds one
    /// hash table **per thread**, so the memory a query needs scales with the thread
    /// count. With 10 threads and a 128 MB limit, a five-million-row summary query
    /// runs out of memory — with 2 threads it takes 154 ms.
    pub duckdb_threads: usize,
    /// Maximum number of events buffered in memory before the oldest are dropped.
    pub max_queue: usize,
    /// The largest request body the collector will read, in bytes.
    ///
    /// The collection path is the only place where the traffic decides how much
    /// memory is used, so it is the only place that needs a limit on a single
    /// request. Axum's own default (2 MB) is a request that can be forty times
    /// the size of a page view.
    pub max_body_bytes: usize,
    /// How many collection requests may be in flight — being read or parsed — at
    /// once. `0` = no limit.
    ///
    /// This bounds the collector's live working set: the allocator's arenas grow
    /// to the high-water mark of what is being parsed at the same time, and they
    /// do not shrink afterwards.
    pub max_inflight_requests: u64,
    /// How many bytes may be in flight at once. `0` = no limit.
    ///
    /// **Bytes, not requests**: a request is not a memory unit. This is the
    /// budget the collector spends out of.
    pub max_inflight_bytes: u64,
    /// How many collection requests per second are admitted. `0` = no limit.
    ///
    /// The allocator's high-water mark grows with the *rate* of allocation, not
    /// with the total, so a rate limit is what bounds the churn.
    pub max_admit_rps: u64,
    /// Maximum number of events written in one batch.
    ///
    /// **The batch is what the write path costs in memory** — a batch is one
    /// transaction, and its rows sit in memory while it is written. The flush
    /// interval says how *often* we write; this says how *much*. With a cap,
    /// the write path's memory does not depend on how fast the traffic is.
    pub max_batch: usize,
    /// Seconds between writer flushes.
    pub flush_interval_secs: u64,
    /// How often the uptime of the websites is checked, in seconds.
    ///
    /// `0` turns the checker off. The default is every five minutes: often
    /// enough to see an outage, rare enough not to be a nuisance.
    pub uptime_interval_secs: u64,
    /// How long a website has to answer before it counts as down, in seconds.
    pub uptime_timeout_secs: u64,
    /// How old a cached report may be, in seconds.
    ///
    /// The cache is thrown away whenever the data changes, and the writer
    /// flushes every couple of seconds — so on a busy site the cache never hit,
    /// and the dashboard ran the same query over and over. With a window of
    /// thirty seconds an answer is reused for half a minute: the numbers on the
    /// screen are at most that old, and the query runs once instead of a
    /// hundred times.
    ///
    /// `0` means "no window": an answer is only served while the data has not
    /// changed at all.
    pub cache_stale_secs: i64,
    /// Default retention window in days (`0` = keep forever).
    pub retention_days: i32,
    /// Header used to derive the country code (e.g. `CF-IPCountry`).
    pub country_header: Option<String>,
    /// Keep the query string of collected URLs (`false` by default: query strings
    /// routinely carry session tokens and other personal data).
    pub keep_query: bool,
    /// Store the page title with every session.
    ///
    /// Off by default: at 10M views/day the title is roughly half of the stored bytes
    /// and almost nobody looks at it. Turn it on for small sites if you want the
    /// session list to show page titles.
    pub store_titles: bool,
    /// Store the whole referring URL instead of just its host.
    ///
    /// Off by default: the referrer breakdown only needs the host (that is what
    /// Umami shows too), and the full URL is the single largest field we store.
    pub store_full_referrer: bool,
    /// Directory holding the frontend files served at `/app/*` (and the tracker source).
    pub js_dir: PathBuf,
    /// Base URL of the frontend JavaScript. Point this at a CDN to serve the dashboard
    /// frontend from anywhere; the backend then only ships the tiny HTML shell.
    pub app_url: Option<String>,
    /// Directory served as the documentation website at `/docs`.
    pub docs_dir: PathBuf,
    /// Domain for automatic TLS (ACME). `None` = plain HTTP.
    pub ssl_domain: Option<String>,
    /// Contact address registered with the ACME provider.
    pub ssl_email: Option<String>,
    /// Directory where the ACME account and certificates are cached.
    pub ssl_cache_dir: PathBuf,
    /// Port used for HTTPS (443 by default).
    pub ssl_port: u16,
}

impl Config {
    /// Build the configuration from CLI flags + environment.
    pub fn from_env_and_flags(
        paths: Paths,
        host: Option<String>,
        port: Option<u16>,
        jwt_secret: Option<String>,
        retention_days: Option<i32>,
        country_header: Option<String>,
        docs_dir: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let host = host
            .or_else(|| env_any("SUMIMI_HOST"))
            .unwrap_or_else(|| "0.0.0.0".to_string());

        let port = port
            .or_else(|| env_any("SUMIMI_PORT").and_then(|v| v.parse().ok()))
            .unwrap_or(3000);

        let jwt_secret = jwt_secret
            .or_else(|| env_any("SUMIMI_JWT_SECRET"))
            .unwrap_or_default();

        let admin_user = env_any("SUMIMI_ADMIN_USER").unwrap_or_else(|| "admin".to_string());
        let admin_password = env_any("SUMIMI_ADMIN_PASSWORD");

        // DuckDB's own default is 80% of RAM — on a 512 MB VPS that is 410 MB,
        // which would leave nothing for the OS. We take a quarter of RAM, capped
        // at 256 MB: the analytics queries stream and spill to disk long before
        // that, so the limit costs nothing and buys a lot of headroom.
        //
        // The two budgets (see [`MEMORY_BUDGET_MIN`] and
        // [`MEMORY_BUDGET_RECOMMENDED`]) are the two the tests run against.
        let duckdb_memory_limit = env_any("SUMIMI_DUCKDB_MEMORY_LIMIT")
            .unwrap_or_else(default_memory_limit);

        // DuckDB's default flush threshold is 128 MB: it would keep that much
        // freed memory before giving it back. On a small box, hand it back sooner.
        let duckdb_flush_threshold = env_any("SUMIMI_DUCKDB_FLUSH_THRESHOLD")
            .unwrap_or_else(|| "32MB".to_string());

        // A runaway query spills to disk; it must not fill the VPS.
        let duckdb_temp_limit = env_any("SUMIMI_DUCKDB_TEMP_LIMIT")
            .unwrap_or_else(|| "2GB".to_string());

        // 0 = derive from the memory limit.
        let duckdb_threads = env_any("SUMIMI_DUCKDB_THREADS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let max_queue = env_any("SUMIMI_MAX_QUEUE")
            .and_then(|v| v.parse().ok())
            .unwrap_or(100_000);

        // The admission gate (see `limits`). The rate is **not** off by
        // default: a box that never says no is a box that runs out of memory.
        // When it is not set, the recommended rate for this box is used — see
        // [`recommended_rate`].
        let max_body_bytes = env_any("SUMIMI_MAX_BODY_BYTES")
            .and_then(|v| v.parse().ok())
            .unwrap_or(crate::limits::DEFAULT_MAX_BODY_BYTES);
        let max_inflight_requests = env_any("SUMIMI_MAX_INFLIGHT_REQUESTS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let max_inflight_bytes = env_any("SUMIMI_MAX_INFLIGHT_BYTES")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // `0` is a deliberate choice — it turns the limit off.
        let max_admit_rps = env_any("SUMIMI_MAX_ADMIT_RPS")
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| recommended_rate(&duckdb_memory_limit));

        // The batch cap: ten thousand rows is about 5 MB of hit structs and
        // 50 ms of writing — small enough that the dashboard's queries do not
        // wait behind it (the writer holds DuckDB's single write connection),
        // and big enough that the write throughput is not hurt.
        let max_batch = env_any("SUMIMI_MAX_BATCH")
            .and_then(|v| v.parse().ok())
            .unwrap_or(crate::writer::DEFAULT_MAX_BATCH)
            .max(1);

        let flush_interval_secs = env_any("SUMIMI_FLUSH_INTERVAL_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(2);

        // The uptime checker: every five minutes, with ten seconds to answer.
        let uptime_interval_secs = env_any("SUMIMI_UPTIME_INTERVAL_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        let uptime_timeout_secs = env_any("SUMIMI_UPTIME_TIMEOUT_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);

        // How old a cached report may be. Thirty seconds is invisible on a
        // dashboard and turns a hundred queries into one.
        let cache_stale_secs = env_any("SUMIMI_CACHE_STALE_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(30)
            .max(0);

        let retention_days = retention_days
            .or_else(|| env_any("SUMIMI_RETENTION_DAYS").and_then(|v| v.parse().ok()))
            .unwrap_or(0);

        let country_header = country_header.or_else(|| env_any("SUMIMI_COUNTRY_HEADER"));

        // These three keep everything by default — that is what Umami stores, and
        // it is the baseline we promised. Turning one off is a per-site policy
        // decision, so it is environment-only.
        let store_titles = env_flag("SUMIMI_STORE_TITLES", true);
        let store_full_referrer = env_flag("SUMIMI_STORE_FULL_REFERRER", true);

        let keep_query = env_flag("SUMIMI_KEEP_QUERY", true);

        // The frontend and the documentation site live next to the executable
        // when they are shipped with it — that is where the release archive puts
        // them, and where a `cargo install`ed binary finds them. The working
        // directory comes first, so running from a checkout still works.
        let js_dir = env_path_any("SUMIMI_JS_DIR").unwrap_or_else(|| next_to_executable("js"));

        // The dashboard frontend is published on a CDN (jsDelivr serves this public
        // repository directly), so the backend never ships it. Point this at `/app`
        // to serve it from the backend instead — useful for air-gapped installs.
        let app_url = env_any("SUMIMI_APP_URL")
            .filter(|s| !s.is_empty())
            .or_else(|| Some(CDN_APP_URL.to_string()));

        let docs_dir = docs_dir
            .or_else(|| env_path_any("SUMIMI_DOCS_DIR"))
            .unwrap_or_else(|| next_to_executable("docs"));

        let ssl_domain = std::env::var("SUMIMI_SSL_DOMAIN").ok().filter(|s| !s.is_empty());
        let ssl_email = std::env::var("SUMIMI_SSL_EMAIL").ok().filter(|s| !s.is_empty());
        // The ACME cache lives inside the instance directory by default.
        let ssl_cache_dir = env_path_any("SUMIMI_SSL_CACHE_DIR").unwrap_or_else(|| paths.acme.clone());
        let ssl_port = std::env::var("SUMIMI_SSL_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(443);

        Ok(Config {
            paths,
            host,
            port,
            jwt_secret,
            admin_user,
            admin_password,
            duckdb_memory_limit,
            duckdb_flush_threshold,
            duckdb_temp_limit,
            duckdb_threads,
            max_queue,
            max_batch,
            max_body_bytes,
            max_inflight_requests,
            max_inflight_bytes,
            max_admit_rps,
            flush_interval_secs,
            cache_stale_secs,
            uptime_interval_secs,
            uptime_timeout_secs,
            retention_days,
            country_header,
            keep_query,
            store_titles,
            store_full_referrer,
            js_dir,
            app_url,
            docs_dir,
            ssl_domain,
            ssl_email,
            ssl_cache_dir,
            ssl_port,
        })
    }

    /// The collection path's limits — what the admission gate enforces.
    ///
    /// **One request must always fit in the budget.** A request whose length is
    /// not declared reserves the whole per-request limit, so a body limit larger
    /// than the in-flight budget would refuse everything — including the first
    /// request. The body limit is therefore lowered to the budget.
    pub fn limits(&self) -> crate::limits::Limits {
        let max_body_bytes = if self.max_inflight_bytes > 0 {
            self.max_body_bytes.min(self.max_inflight_bytes as usize)
        } else {
            self.max_body_bytes
        };
        crate::limits::Limits {
            max_body_bytes,
            max_inflight_requests: self.max_inflight_requests,
            max_inflight_bytes: self.max_inflight_bytes,
            max_admit_rps: self.max_admit_rps,
        }
    }
}

/// The two memory budgets the product is specified against.
///
/// Every test runs against **both** of them. They are not "the engine's
/// allowance" — the number that matters is the **whole process's RSS**, because
/// that is what a small VPS runs out of. The engine's number is a part of it.
pub mod budget {
    /// The **minimum** requirement: a 512 MB machine (a 0.5G VPS), half of it.
    ///
    /// A quarter of 512 MB is 128 MB, so the minimum machine gets a 128 MB
    /// engine budget — and the process must stay under 256 MB.
    pub const MIN_ENGINE: &str = "128MB";
    /// The **minimum** requirement's whole-process ceiling: 256 MB.
    pub const MIN_RSS: u64 = 256 * 1024 * 1024;
    /// The **recommended** setup: a 1 GB machine (a normal VPS), half of it.
    ///
    /// A quarter of 1 GB is 256 MB, so the recommended machine gets a 256 MB
    /// engine budget — and the process must stay under 512 MB.
    pub const RECOMMENDED_ENGINE: &str = "256MB";
    /// The **recommended** setup's whole-process ceiling: 512 MB.
    pub const RECOMMENDED_RSS: u64 = 512 * 1024 * 1024;
}

/// The default DuckDB memory ceiling: a quarter of RAM, capped at 256 MB.
///
/// 512 MB of RAM → 128 MB. 1 GB → 256 MB. More → still 256 MB: the engine
/// streams and spills long before that, and the cap is what keeps a big
/// machine's engine from eating a small box's page cache.
pub fn default_memory_limit() -> String {
    let ram = total_ram_bytes().unwrap_or(512 * 1024 * 1024);
    let limit = (ram / 4).min(MEMORY_LIMIT_MAX);
    format!("{}MB", limit / (1024 * 1024))
}

/// The recommended admission rate, derived from the memory the box has.
///
/// **The rule: 20 000 requests a second for every 128 MB of the engine's
/// budget** — so the minimum tier (a 128 MB engine, a 256 MB process) admits
/// **20 000/s**, and the recommended tier (a 256 MB engine, a 512 MB process)
/// admits **40 000/s**.
///
/// It comes from the measurement, not from a hunch: the peak resident set is
/// about **68 MB + 7.8 MB per 1 000 requests a second** on a 2 vCPU box
/// (`MEMORY.md` §6), so the rate that fills the box is where the rule puts it —
/// and the box is *full but not over* at the limit. Above it the server
/// refuses, which is the honest answer: the traffic is arriving faster than
/// the disk can take it.
///
/// **The margin, and where it goes** ✓ — that 7.8 MB per 1 000 requests is
/// **the allocator's retention of the churn** ✓, not the data ✓ (the payload
/// is 134 bytes ✓): the same load with `MALLOC_MMAP_THRESHOLD_=65536` peaks
/// **210 MB instead of 274 MB** ✓ ⇒ **the churn comes back to the kernel** ✓.
/// Two consequences ✓:
/// - over a **network** the traffic arrives in batches ✓ ⇒ the peak is ~20%
///   higher than over loopback ✓ ⇒ at 20 000/s the minimum tier reached
///   **261 MB on a 512 MB box** ✗ ⇒ **the promise was broken by 5 MB** ✗
///   ([#17](https://github.com/sumimi-dev/sumimi/issues/17) ✓);
/// - the *fix* is not a lower rate but **fewer/cheaper allocations per
///   request** ✓ ⇒ [#18](https://github.com/sumimi-dev/sumimi/issues/18) ✓ ✓
///   (where the 7.8 KB per request actually goes ✓).
///
/// The user can always set `SUMIMI_MAX_ADMIT_RPS` to something else — this is
/// only what happens when they do not.
pub fn recommended_rate(memory_limit: &str) -> u64 {
    let budget = parse_size(memory_limit).unwrap_or(128 * 1024 * 1024);
    let megabytes = budget / (1024 * 1024);
    // 20 000 a second per 128 MB — the division keeps the smaller budgets
    // proportional instead of rounding them away.
    (megabytes.saturating_mul(20_000) / 128).max(1_000)
}

/// The hard ceiling on the engine's memory limit: 256 MB.
pub const MEMORY_LIMIT_MAX: u64 = 256 * 1024 * 1024;

/// How many threads DuckDB should use for a given memory limit.
///
/// A parallel hash aggregate builds one hash table per thread, so the memory a query
/// needs grows with the thread count. We give DuckDB one thread per 64 MB of budget,
/// never more than the machine has cores.
pub fn threads_for(memory_limit: &str, cores: usize) -> usize {
    let budget = parse_size(memory_limit).unwrap_or(128 * 1024 * 1024);
    let by_budget = (budget / (64 * 1024 * 1024)).max(1) as usize;
    by_budget.min(cores.max(1))
}

/// The thread count the two budgets imply.
///
/// 128 MB → 2 threads, 256 MB → 4 threads. The numbers are the design: a
/// parallel `COUNT(DISTINCT …)` builds one hash table per thread, so the
/// memory a query needs scales with the thread count.
pub fn threads_for_budget(memory_limit: &str) -> usize {
    threads_for(memory_limit, std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recommended rate follows the memory: twice the memory, twice the
    /// rate. The minimum tier (a 128 MB engine) admits 20 000 a second, the
    /// recommended tier (256 MB) admits 40 000 ✓.
    #[test]
    fn the_recommended_rate_follows_the_memory() {
        assert_eq!(recommended_rate("128MB"), 20_000);
        assert_eq!(recommended_rate("256MB"), 40_000);
        assert_eq!(recommended_rate("64MB"), 10_000);
        // Nonsense in, the default out — never a rate of zero.
        assert_eq!(recommended_rate("not a size"), 20_000);
        assert_eq!(recommended_rate("1MB"), 1_000);
    }
}

/// Parse a size like `128MB` into bytes.
pub fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim().to_ascii_uppercase();
    let (number, unit) = match value.find(|c: char| c.is_alphabetic()) {
        Some(index) => (&value[..index], &value[index..]),
        None => (value.as_str(), "B"),
    };
    let number: f64 = number.trim().parse().ok()?;
    let multiplier: f64 = match unit {
        "B" => 1.0,
        "KB" | "K" => 1024.0,
        "MB" | "M" => 1024.0 * 1024.0,
        "GB" | "G" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * multiplier) as u64)
}

/// The memory this process may use, if we can find it out.
///
/// **Not always the machine's memory.** A container may use less than the
/// machine has — `docker run --memory=512m` on a 7.7 GB box ⇒ 512 MB ✓ ⇒ and
/// the budget has to be sized by **that** ✓ ⇒ otherwise a 512 MB container
/// sizes its engine at 256 MB ✓, admits 40 000 requests a second ✓, and is
/// killed by the OOM reaper ✗ ✗.
///
/// So: the smaller of the machine's memory and the cgroup's limit ✓ ✓.
pub fn total_ram_bytes() -> Option<u64> {
    // Linux: /proc/meminfo.
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kb: u64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if kb > 0 {
                    let machine = kb * 1024;
                    return Some(
                        crate::machine::cgroup_memory_limit().map_or(machine, |limit| machine.min(limit)),
                    );
                }
            }
        }
    }

    // macOS: sysctl.
    if cfg!(target_os = "macos") {
        if let Ok(out) = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
        {
            if let Ok(text) = String::from_utf8(out.stdout) {
                if let Ok(bytes) = text.trim().parse::<u64>() {
                    return Some(bytes);
                }
            }
        }
    }

    None
}

/// Read a boolean environment variable.
fn env_flag(key: &str, default: bool) -> bool {
    env_any(key)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

/// Read an environment variable, with the **old** name as a fallback.
///
/// The project was called `sumami` until v0.2.0; the settings it took with it
/// are still in people's `/etc/sumami/sumami.env` files and systemd units, so
/// `SUMAMI_DIR` still works. The new name wins when both are set.
fn env_any(new_key: &str) -> Option<String> {
    if let Ok(value) = std::env::var(new_key) {
        if !value.is_empty() {
            return Some(value);
        }
    }
    let old_key = format!("SUMAMI{}", &new_key["SUMIMI".len()..]);
    std::env::var(old_key).ok().filter(|value| !value.is_empty())
}

/// A path from the environment, with the old name as a fallback.
fn env_path_any(key: &str) -> Option<PathBuf> {
    env_any(key).map(PathBuf::from)
}

/// Resolve the JWT secret, generating and persisting one on first run.
///
/// A random per-install secret is better than a hard-coded default: a leaked default would
/// let anybody mint admin tokens.
pub fn resolve_jwt_secret(configured: &str, secret_path: &std::path::Path) -> anyhow::Result<String> {
    if !configured.is_empty() {
        return Ok(configured.to_string());
    }
    if let Ok(v) = std::env::var("SUMIMI_JWT_SECRET_FILE") {
        let path = PathBuf::from(v);
        if path.exists() {
            let s = std::fs::read_to_string(&path)
                .with_context(|| format!("reading jwt secret file {}", path.display()))?;
            let s = s.trim().to_string();
            if !s.is_empty() {
                return Ok(s);
            }
        }
    }
    // Fall back to a secret file next to the database.
    let path = secret_path.to_path_buf();
    if path.exists() {
        let s = std::fs::read_to_string(&path)
            .with_context(|| format!("reading jwt secret file {}", path.display()))?;
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    let secret = random_secret();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    std::fs::write(&path, &secret)
        .with_context(|| format!("writing jwt secret file {}", path.display()))?;
    restrict_permissions(&path);
    Ok(secret)
}

/// A directory next to the executable, or next to the working directory.
///
/// The release archive ships the frontend and the documentation site next to
/// the binary, so a `cargo install`ed Sumimi finds them wherever it was put.
pub fn next_to_executable(name: &str) -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(directory) = exe.parent() {
            let candidate = directory.join(name);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from(name)
}

/// Best-effort `chmod 0600` — for a **file**.
pub fn restrict_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Best-effort `chmod 0700` — for a **directory**.
///
/// A directory needs the **execute** bit: without it the directory cannot be
/// entered, and the files inside it cannot be opened — not even by the owner.
///
/// (The instance directory was set to `0600`, which is right for a file and
/// wrong for a directory: the server started, ran, and then **could not start
/// again** — the data file was unreachable.)
pub fn restrict_directory(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Generate `n` random bytes from the OS.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let hex = random_secret();
    let bytes: Vec<u8> = hex
        .as_bytes()
        .chunks(2)
        .filter_map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect();
    bytes.into_iter().take(n).collect()
}

/// Generate a URL-safe random secret.
pub fn random_secret() -> String {
    use rand_compat::RngCore;
    let mut bytes = [0u8; 32];
    rand_compat::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Small RNG shim so we do not need an extra dependency for `getrandom`.
pub mod rand_compat {
    /// Thread-local CSPRNG backed by the OS.
    pub struct ThreadRng(std::cell::RefCell<State>);

    struct State {
        s: [u8; 32],
        i: usize,
    }

    impl ThreadRng {
        fn next_byte(&mut self) -> u8 {
            let mut st = self.0.borrow_mut();
            if st.i == 0 {
                st.s = os_random();
                st.i = 0;
            }
            let b = st.s[st.i % 32];
            st.i += 1;
            b
        }
    }

    /// Random bytes from the OS.
    pub trait RngCore {
        /// Fill the buffer with random bytes.
        fn fill_bytes(&mut self, dest: &mut [u8]);
    }

    impl RngCore for ThreadRng {
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for b in dest.iter_mut() {
                *b = self.next_byte();
            }
        }
    }

    /// Thread-local RNG.
    pub fn thread_rng() -> ThreadRng {
        ThreadRng(std::cell::RefCell::new(State { s: [0u8; 32], i: 32 }))
    }

    fn os_random() -> [u8; 32] {
        // /dev/urandom is available on every unix target we support.
        use std::io::Read;
        let mut buf = [0u8; 32];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(&mut buf).is_ok() {
                return buf;
            }
        }
        // Extremely unlikely fallback: mix time + address entropy.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let a = &buf as *const _ as u64;
        for (i, b) in buf.iter_mut().enumerate() {
            let mut x = t ^ (a << (i % 8)) ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            x ^= x >> 33;
            x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
            x ^= x >> 33;
            *b = (x & 0xff) as u8;
        }
        buf
    }
}
