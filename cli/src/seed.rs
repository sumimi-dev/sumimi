//! Generate synthetic demo data.
//!
//! The data is entirely invented — invented domains, invented visitors, invented
//! traffic shapes. It exists so the dashboard can be evaluated (and demoed) without
//! pointing a real site at it.
//!
//! ```bash
//! sumimi seed --db ./sumimi.duckdb --days 30
//! sumimi seed --backend sqlite --db ./sumimi.sqlite --days 30
//! ```
//!
//! The data is written through the storage backend, so the same seed works on
//! DuckDB, on SQLite and on PostgreSQL — and the numbers are the same on all
//! three.

use anyhow::Result;
use chrono::Datelike;
use sumimi_service::models::{Hit, HitKind};
use sumimi_service::storage::Storage;
use uuid::Uuid;

/// How much data to generate.
#[derive(Debug, Clone)]
pub struct SeedOptions {
    /// The website to seed (created when missing).
    pub website: String,
    /// The domain of the website.
    pub domain: String,
    /// How many days of history.
    pub days: i64,
    /// Roughly how many visitors per day.
    pub visitors_per_day: i64,
}

/// A tiny deterministic PRNG, so a seed run is reproducible.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x5EED_1234_ABCD_0001)
    }

    /// The next random number.
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A number in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    /// A number in `low..=high`.
    fn between(&mut self, low: i64, high: i64) -> i64 {
        if high <= low {
            return low;
        }
        low + (self.next() % ((high - low + 1) as u64)) as i64
    }

    /// `true` with probability `percent`/100.
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    /// Pick an item from a weighted list.
    fn weighted(&mut self, items: &[(&'static str, u64)]) -> &'static str {
        let total: u64 = items.iter().map(|(_, w)| w).sum();
        let mut roll = self.next() % total;
        for (value, weight) in items {
            if roll < *weight {
                return value;
            }
            roll -= *weight;
        }
        items[0].0
    }
}

const URLS: &[(&str, u64)] = &[
    ("/", 1000),
    ("/blog/analytics-without-cookies", 420),
    ("/blog/duckdb-vs-postgres", 260),
    ("/blog/self-hosting-on-a-budget-vps", 210),
    ("/blog/rust-for-backend-people", 180),
    ("/docs", 150),
    ("/docs/install", 120),
    ("/pricing", 80),
    ("/download", 70),
    ("/changelog", 40),
];

const REFERRERS: &[(&str, u64)] = &[
    ("", 320),
    ("https://www.google.com/", 240),
    ("https://duckduckgo.com/", 120),
    ("https://news.ycombinator.com/", 95),
    ("https://github.com/", 70),
    ("https://x.com/", 45),
];

const BROWSERS: &[(&str, u64)] = &[("Chrome", 540), ("Safari", 220), ("Firefox", 90), ("Edge", 80)];

const SYSTEMS: &[(&str, u64)] = &[("Windows", 330), ("macOS", 300), ("iOS", 170), ("Android", 130)];

const DEVICES: &[(&str, u64)] = &[("desktop", 620), ("mobile", 330), ("tablet", 50)];

const COUNTRIES: &[(&str, u64)] = &[
    ("US", 300),
    ("CN", 150),
    ("DE", 90),
    ("GB", 85),
    ("JP", 60),
    ("IN", 55),
    ("FR", 40),
    ("BR", 35),
];

const LANGUAGES: &[(&str, u64)] = &[
    ("en-US", 440),
    ("zh-CN", 150),
    ("en-GB", 85),
    ("ja-JP", 60),
    ("de-DE", 50),
];

const SCREENS: &[(&str, u64)] = &[
    ("1920x1080", 300),
    ("1536x864", 200),
    ("1366x768", 160),
    ("390x844", 150),
    ("412x915", 60),
    ("768x1024", 40),
];

const SOURCES: &[(&str, u64)] = &[("twitter", 40), ("newsletter", 35), ("google", 20), ("hn", 15)];

const EVENTS: &[(&str, u64)] = &[("signup", 4), ("download", 3), ("cta_click", 2), ("newsletter", 1)];

/// Generate the demo data.
///
/// Returns the website id, the number of page views and the number of custom
/// events.
pub fn seed(storage: &dyn Storage, options: &SeedOptions) -> Result<(String, u64, u64)> {
    // The seed must not be the mixing constant itself: `Rng::new` xors the two
    // together, and xorshift on zero stays zero forever — every session came
    // out identical.
    let mut rng = Rng::new(0xA5A5_1234_ABCD_0001);

    // The website, created when it is not there yet.
    let website_id = match storage.websites()?.into_iter().find(|website| {
        website.name == options.website || website.domain.as_deref() == Some(&options.domain)
    }) {
        Some(website) => website.id,
        None => storage
            .create_website(&options.website, Some(&options.domain), 0, None)?
            .id,
    };

    let now = chrono::Utc::now().naive_utc();
    let mut hits: Vec<Hit> = Vec::new();
    let mut views = 0u64;
    let mut events = 0u64;

    for day in (0..options.days).rev() {
        let day_start = now - chrono::Duration::days(day);
        let weekday = day_start.weekday().num_days_from_monday() as i64;

        // Weekends are quieter, and traffic grows a little every week.
        let weekend = if weekday >= 5 { 0.62 } else { 1.0 };
        let growth = 1.0 + (options.days - day) as f64 * 0.004;
        let visitors = (options.visitors_per_day as f64 * weekend * growth) as i64;

        // A pool of visitors for the day: some of them come back, so the
        // "visitors" count is lower than the "sessions" count, like a real site.
        let pool: Vec<Uuid> = (0..visitors).map(|_| Uuid::new_v4()).collect();

        for _ in 0..visitors {
            let visitor_id = pool[rng.below(pool.len())];
            let (ip, ip_prefix, user_agent) =
                sumimi_service::testing::visitor_identity(&visitor_id);
            let started = day_start + chrono::Duration::seconds(rng.between(0, 86_399));
            // The session id is worked out from the visitor and the moment the
            // session began — the same shape the server writes.
            let visitor_id = sumimi_service::ua::visitor_id(
                &sumimi_service::ua::visitor_address(&ip),
                &user_agent,
                &website_id,
            );
            let session_id = sumimi_service::session::session_id(
                visitor_id,
                started.and_utc().timestamp_micros(),
            );

            // A session is one to four page views.
            let page_views = match rng.below(100) {
                0..=44 => 1,
                45..=69 => 2,
                70..=84 => 3,
                _ => 4,
            };

            let referrer = rng.weighted(REFERRERS);
            let (referrer_host, referrer_path, referrer_query) = split_referrer(referrer);
            let browser = rng.weighted(BROWSERS);
            let os = rng.weighted(SYSTEMS);
            let device = rng.weighted(DEVICES);
            let country = rng.weighted(COUNTRIES);
            let language = rng.weighted(LANGUAGES);
            let screen = rng.weighted(SCREENS);
            let source = rng.weighted(SOURCES);

            let mut last_url = "/".to_string();
            for view in 0..page_views {
                let url = rng.weighted(URLS).to_string();
                last_url = url.clone();
                let at = started + chrono::Duration::seconds(view * 35);

                let url_query = format!(
                    "utm_source={}&utm_medium=cpc&utm_campaign=spring&utm_content=cta-{}&utm_term=analytics",
                    source,
                    rng.below(5)
                );
                let gclid = if rng.chance(9) {
                    Some(format!("gclid-{:08x}", rng.next() as u32))
                } else {
                    None
                };
                let fbclid = if rng.chance(11) {
                    Some(format!("fbclid-{:08x}", rng.next() as u32))
                } else {
                    None
                };

                hits.push(Hit {
                    website_id: website_id.clone(),
                    visit_id: session_id,
                    visitor_id,
                    session_start: Some(started),
                    ip: ip.clone(),
                    ip_prefix: ip_prefix.clone(),
                    user_agent: user_agent.clone(),
                    user_id: None,
                    retention_days: 0,
                    kind: HitKind::Pageview,
                    created_at: at,
                    url: Some(url.clone()),
                    url_query: Some(url_query.clone()),
                    title: Some(title_for(&url)),
                    referrer: referrer_host.clone(),
                    referrer_path: referrer_path.clone(),
                    referrer_query: referrer_query.clone(),
                    referrer_name: sumimi_service::channels::referrer_name(
                        referrer_host.as_deref(),
                    ),
                    channel: Some(
                        sumimi_service::channels::channel(&sumimi_service::channels::ChannelInput::new(
                            referrer_host.as_deref(),
                            Some(url_query.as_str()),
                            Some("cpc"),
                            Some(source),
                            Some("example.com"),
                        ))
                        .to_string(),
                    ),
                    hostname: Some("example.com".to_string()),
                    browser: Some(browser.to_string()),
                    browser_version: Some("120".to_string()),
                    os: Some(os.to_string()),
                    os_version: Some("10".to_string()),
                    device: Some(device.to_string()),
                    screen: Some(screen.to_string()),
                    language: Some(language.to_string()),
                    country: Some(country.to_string()),
                    region: None,
                    city: None,
                    latitude: None,
                    longitude: None,
                    utm_source: Some(source.to_string()),
                    utm_medium: Some("cpc".to_string()),
                    utm_campaign: Some("spring".to_string()),
                    utm_content: Some(format!("cta-{}", rng.below(5))),
                    utm_term: Some("analytics".to_string()),
                    gclid: gclid.clone(),
                    fbclid: fbclid.clone(),
                    msclkid: None,
                    ttclid: None,
                    lifatid: None,
                    twclid: None,
                    lcp: Some(600.0 + rng.between(0, 1800) as f64),
                    inp: Some(30.0 + rng.between(0, 260) as f64),
                    cls: Some(rng.between(0, 120) as f64 / 1000.0),
                    fcp: Some(400.0 + rng.between(0, 900) as f64),
                    ttfb: Some(60.0 + rng.between(0, 220) as f64),
                    name: None,
                    tag: None,
                    data: None,
                });
                views += 1;
            }

            // Every third session fires a custom event.
            if rng.chance(33) {
                let name = rng.weighted(EVENTS);
                hits.push(Hit {
                    website_id: website_id.clone(),
                    visit_id: session_id,
                    visitor_id,
                    session_start: Some(started),
                    ip,
                    ip_prefix,
                    user_agent,
                    user_id: None,
                    retention_days: 0,
                    kind: HitKind::Event,
                    created_at: started + chrono::Duration::seconds(rng.between(30, 600)),
                    url: Some(last_url.clone()),
                    url_query: None,
                    title: None,
                    referrer: None,
                    referrer_path: None,
                    referrer_query: None,
                    referrer_name: None,
                    channel: Some("direct".to_string()),
                    hostname: Some("example.com".to_string()),
                    browser: Some(browser.to_string()),
                    browser_version: Some("120".to_string()),
                    os: Some(os.to_string()),
                    os_version: Some("10".to_string()),
                    device: Some(device.to_string()),
                    screen: Some(screen.to_string()),
                    language: Some(language.to_string()),
                    country: Some(country.to_string()),
                    region: None,
                    city: None,
                    latitude: None,
                    longitude: None,
                    utm_source: None,
                    utm_medium: None,
                    utm_campaign: None,
                    utm_content: None,
                    utm_term: None,
                    gclid: None,
                    fbclid: None,
                    msclkid: None,
                    ttclid: None,
                    lifatid: None,
                    twclid: None,
                    lcp: None,
                    inp: None,
                    cls: None,
                    fcp: None,
                    ttfb: None,
                    name: Some(name.to_string()),
                    tag: Some(format!("cta-{name}")),
                    data: Some("{\"plan\":\"pro\"}".to_string()),
                });
                events += 1;
            }
        }
    }

    // Write in batches, the way the server does.
    for chunk in hits.chunks(1000) {
        storage.write_batch(chunk)?;
    }

    Ok((website_id, views, events))
}

/// The title of a page.
fn title_for(path: &str) -> String {
    match path {
        "/" => "Home".to_string(),
        "/blog/analytics-without-cookies" => "Analytics without cookies".to_string(),
        "/blog/duckdb-vs-postgres" => "DuckDB vs Postgres".to_string(),
        "/blog/self-hosting-on-a-budget-vps" => "Self-hosting on a budget VPS".to_string(),
        "/blog/rust-for-backend-people" => "Rust for backend people".to_string(),
        "/docs" => "Documentation".to_string(),
        "/docs/install" => "Installation".to_string(),
        "/pricing" => "Pricing".to_string(),
        "/download" => "Download".to_string(),
        "/changelog" => "Changelog".to_string(),
        other => format!("Page {other}"),
    }
}

/// Split a referrer into its host, path and query.
fn split_referrer(referrer: &str) -> (Option<String>, Option<String>, Option<String>) {
    if referrer.is_empty() {
        return (None, None, None);
    }
    let without_scheme = referrer
        .strip_prefix("https://")
        .or_else(|| referrer.strip_prefix("http://"))
        .unwrap_or(referrer);
    match without_scheme.split_once('/') {
        Some((host, rest)) => match rest.split_once('?') {
            Some((path, query)) => (
                Some(host.to_string()),
                Some(path.to_string()),
                Some(query.to_string()),
            ),
            None => (Some(host.to_string()), Some(rest.to_string()), None),
        },
        None => (Some(without_scheme.to_string()), None, None),
    }
}
