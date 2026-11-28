//! Generate synthetic demo data.
//!
//! The data is entirely invented — invented domains, invented visitors, invented
//! traffic shapes. It exists so the dashboard can be evaluated (and demoed) without
//! pointing a real site at it.
//!
//! ```bash
//! sumimi seed --db ./sumimi.duckdb --days 30
//! sumimi seed --backend sqlite --db ./sumimi.sqlite --days 30
//! sumimi seed --seed 4242 --as-of 2026-11-28T06:00:00Z
//! ```
//!
//! The data is written through the storage backend, so the same seed works on
//! DuckDB, on SQLite and on PostgreSQL — and the numbers are the same on all
//! three.
//!
//! **The seed and the clock are the two inputs.** The same seed and the same
//! clock give the same traffic, so two runs can be compared and a measurement
//! on one machine can be repeated on another. The clock is the moment the
//! history is as of: the days are calendar days of it, and the newest day
//! stops at it — **there is no data in the future**.

use anyhow::Result;
use chrono::Datelike;
use sumimi_service::models::{Hit, HitKind};
use sumimi_service::storage::Storage;
use uuid::Uuid;

/// The seed the generator uses when nobody says otherwise.
///
/// **A fixed one, so the data is the data**: the same seed on every machine,
/// the same traffic. The clock is the other half of it — see [`SeedOptions`].
pub const DEFAULT_SEED: u64 = 0xA5A5_1234_ABCD_0001;

/// How far a visit's own rows can reach past its start.
///
/// The last page view is a minute later and the custom event up to ten, so
/// the starts stop this much before the clock — then no row of the data is in
/// the future.
const MAX_SPILL_SECONDS: i64 = 600;

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
    /// The seed of the generator.
    ///
    /// **The same seed gives the same traffic** — the same visitors, the same
    /// pages, the same times of day ⇒ ⇒ so two runs can be compared, and a
    /// measurement on one machine can be repeated on another.
    pub seed: u64,
    /// The moment the history is as of.
    ///
    /// **The clock is part of the data**, so it is an input and not a thing
    /// the generator reaches for: the same seed **and** the same as-of give
    /// the same rows — the same visitors, the same pages, the same moments.
    ///
    /// The days are **calendar days** of the as-of ⇒ ⇒ so the shape does not
    /// move with the hour the seed runs at ⇒ ⇒ and the newest day is cut at
    /// the as-of ⇒ ⇒ **there is no data in the future** ⇒ ⇒ (which is what a
    /// demo wants, and what a measurement needs: the data has to fit in the
    /// range the report asks for).
    pub as_of: chrono::NaiveDateTime,
    /// Whether the site's old rows are gone before the new ones go in.
    ///
    /// **Off by default**: erasing is the one thing a seed can lose. With it
    /// on, seeding the same site twice with the same seed and the same as-of
    /// leaves the same data — the site's own id is reused, so the visitor ids
    /// are the same too.
    pub replace: bool,
}

impl SeedOptions {
    /// The options the command line uses when nobody says otherwise.
    ///
    /// **The clock is the moment the seed runs** — the history is always
    /// recent, which is what a demo wants. Pin it when the data has to be the
    /// same as another run's.
    pub fn new(website: &str, domain: &str, days: i64, visitors_per_day: i64) -> Self {
        Self {
            website: website.to_string(),
            domain: domain.to_string(),
            days,
            visitors_per_day,
            seed: DEFAULT_SEED,
            as_of: chrono::Utc::now().naive_utc(),
            replace: false,
        }
    }
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

    /// A UUID out of the generator — sixteen bytes, two draws.
    ///
    /// **Not `Uuid::new_v4()`**: the random one would make every run a
    /// different run, and a seed that is not reproducible is not a seed.
    fn uuid(&mut self) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&self.next().to_be_bytes());
        bytes[8..].copy_from_slice(&self.next().to_be_bytes());
        Uuid::from_bytes(bytes)
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
/// Returns the website id, the number of page views, the number of custom
/// events and the number of rows that were erased first.
pub fn seed(storage: &dyn Storage, options: &SeedOptions) -> Result<(String, u64, u64, u64)> {
    // The seed must not be the mixing constant itself: `Rng::new` xors the two
    // together, and xorshift on zero stays zero forever — every session came
    // out identical.
    let mut rng = Rng::new(options.seed);
    let as_of = options.as_of;

    // The website, created when it is not there yet.
    let website_id = match storage.websites()?.into_iter().find(|website| {
        website.name == options.website || website.domain.as_deref() == Some(&options.domain)
    }) {
        Some(website) => website.id,
        None => storage
            .create_website(&options.website, Some(&options.domain), 0, None)?
            .id,
    };

    // The site's old rows: gone, if the caller asked for it.
    //
    // **The rows, not the site.** The site stays registered — it is the site
    // the operator asked for — and only its rows go, so the same site can be
    // seeded again and land on the same ids.
    let mut erased = 0u64;
    if options.replace {
        let (rows, revenue) = storage.delete_before(
            as_of + chrono::Duration::days(36_500),
            Some(&website_id),
        )?;
        erased = rows + revenue;
    }

    let mut hits: Vec<Hit> = Vec::new();
    let mut views = 0u64;
    let mut events = 0u64;
    // The visit ids are a counter — the server's own — and the ids are handed
    // out **in the order the visits happen**, not in the order the generator
    // walks the days. ⇒ ⇒ So every visit's rows are collected first, and the
    // ids are minted afterwards, in the order the visits began.
    let mut sessions: Vec<(chrono::NaiveDateTime, usize, usize)> = Vec::new();

    for day in (0..options.days).rev() {
        // **The days are calendar days** — the day the as-of falls on, and the
        // days before it. The buckets do not slide with the hour the seed
        // runs at, so the shape of the history is the same whenever it runs.
        let date = as_of.date() - chrono::Duration::days(day);
        let day_start = date.and_hms_opt(0, 0, 0).expect("midnight is a time");
        // The newest day stops at the clock ⇒ ⇒ ⇒ no data in the future ⇒ ⇒ ⇒
        // and the data fits in the range a report asks for.
        let day_end = std::cmp::min(day_start + chrono::Duration::days(1), as_of);
        // The margin: **a visit's own rows reach past its start** — the last
        // page view is a minute later, the custom event up to ten. So the
        // starts stop that much before the clock ⇒ ⇒ ⇒ and then no row of the
        // data is in the future either.
        let span = (day_end - day_start).num_seconds() - MAX_SPILL_SECONDS;
        if span <= 0 {
            // The as-of is at midnight: this day has not started yet.
            continue;
        }
        let weekday = day_start.weekday().num_days_from_monday() as i64;

        // Weekends are quieter, and traffic grows a little every week.
        let weekend = if weekday >= 5 { 0.62 } else { 1.0 };
        let growth = 1.0 + (options.days - day) as f64 * 0.004;
        let visitors = (options.visitors_per_day as f64 * weekend * growth) as i64;

        // A pool of visitors for the day: some of them come back, so the
        // "visitors" count is lower than the "sessions" count, like a real site.
        //
        // **The pool comes out of the seed** ⇒ ⇒ the same seed gives the same
        // visitors — a run of the seed is a run of the same traffic, not a
        // different traffic that looks the same.
        let pool: Vec<Uuid> = (0..visitors).map(|_| rng.uuid()).collect();

        for _ in 0..visitors {
            let visitor_id = pool[rng.below(pool.len())];
            let (ip, ip_prefix, user_agent) =
                sumimi_service::testing::visitor_identity(&visitor_id);
            let started = day_start + chrono::Duration::seconds(rng.between(0, span));
            // The session id is worked out from the visitor and the moment the
            // session began — the same shape the server writes.
            let visitor_id = sumimi_service::ua::visitor_id(
                &sumimi_service::ua::visitor_address(&ip),
                &user_agent,
                &website_id,
            );
            // The visit id is the counter the server mints — but it is minted
            // later, when every visit is known (see below).
            let first_hit = hits.len();

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
                    visit_id: Uuid::nil(),
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
                    properties: Vec::new(),
                    ver: None,
                });
                views += 1;
            }

            // Every third session fires a custom event.
            if rng.chance(33) {
                let name = rng.weighted(EVENTS);
                hits.push(Hit {
                    website_id: website_id.clone(),
                    visit_id: Uuid::nil(),
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
                    properties: vec![sumimi_service::models::Property {
                        key: "plan".to_string(),
                        value: sumimi_service::models::PropertyValue::Text("pro".to_string()),
                    }],
                    ver: None,
                });
                events += 1;
            }

            // The visit's rows: from the first one to here.
            sessions.push((started, first_hit, hits.len()));
        }
    }

    // **The visit ids**, in the order the visits happen: the counter grows
    // with the clock, the way it grows on a live server (D-066).
    sessions.sort_by_key(|(started, _, _)| *started);
    let mut visit_counter = 0u64;
    for (_, first, end) in &sessions {
        visit_counter += 1;
        let visit_id = sumimi_service::event_id::compose_version(
            sumimi_service::event_id::VISIT_VERSION,
            1,
            visit_counter,
        );
        for hit in &mut hits[*first..*end] {
            hit.visit_id = visit_id;
        }
    }

    // The rows go in as they arrive: by time. The row id is the insertion
    // order, and the reports use it to break the ties.
    hits.sort_by_key(|hit| hit.created_at);

    // **The data is checked before it is written.** A run that would write
    // rows that break the rules writes nothing — the seed does not get to
    // leave a half-right history behind.
    self_check(&hits, as_of)?;

    // Write in batches, the way the server does.
    for chunk in hits.chunks(1000) {
        storage.write_batch(chunk)?;
    }

    Ok((website_id, views, events, erased))
}

/// The seed's own checks: the properties the data must have.
///
/// **Five of them**, and they are the ones the server's own rules make:
///
/// 1. **one start per visit** — the rows of a visit share one start, or the
///    sort key's grouping column does not group;
/// 2. **the visit ids follow the clock** — the counter grows with the time,
///    the way it grows on a live server (D-066);
/// 3. **the rows of a visit are in the order they happened** — a row's time
///    does not go backwards inside its visit;
/// 4. **nothing is in the future** — the newest row is at the as-of, or before;
/// 5. **the rows are where their visit says they are** — every row is at or
///    after its visit's start.
fn self_check(hits: &[Hit], as_of: chrono::NaiveDateTime) -> Result<()> {
    use std::collections::HashMap;

    // 1. One start per visit.
    let mut starts: HashMap<uuid::Uuid, Option<chrono::NaiveDateTime>> = HashMap::new();
    for hit in hits {
        match starts.get(&hit.visit_id) {
            Some(start) => {
                if *start != hit.session_start {
                    anyhow::bail!(
                        "the visit {} has two starts: {start:?} and {:?}",
                        hit.visit_id,
                        hit.session_start
                    );
                }
            }
            None => {
                starts.insert(hit.visit_id, hit.session_start);
            }
        }
    }

    // 2. The visit ids follow the clock: sorted by the start, the counters
    //    grow.
    let mut by_start: Vec<(&Option<chrono::NaiveDateTime>, u64)> = starts
        .iter()
        .map(|(visit, start)| {
            (
                start,
                sumimi_service::event_id::counter_of(visit),
            )
        })
        .collect();
    by_start.sort_by_key(|(start, _)| start.unwrap_or(chrono::NaiveDateTime::MIN));
    let mut previous = 0u64;
    for (_, counter) in &by_start {
        if *counter <= previous {
            anyhow::bail!("the visit ids do not follow the clock: {counter} after {previous}");
        }
        previous = *counter;
    }

    // 3, 4 and 5: the rows of a visit, in order, at or after the start, and
    //    not in the future.
    let mut last: HashMap<uuid::Uuid, chrono::NaiveDateTime> = HashMap::new();
    for hit in hits {
        if hit.created_at > as_of {
            anyhow::bail!(
                "a row is in the future: {} at {}",
                hit.created_at,
                hit.url.as_deref().unwrap_or("-")
            );
        }
        let start = hit.session_start.unwrap_or(hit.created_at);
        if hit.created_at < start {
            anyhow::bail!(
                "a row is before its visit's start: {} at {}, the visit started at {}",
                hit.created_at,
                hit.url.as_deref().unwrap_or("-"),
                start
            );
        }
        if let Some(previous) = last.get(&hit.visit_id) {
            if hit.created_at < *previous {
                anyhow::bail!(
                    "the rows of the visit {} go backwards: {} after {}",
                    hit.visit_id,
                    hit.created_at,
                    previous
                );
            }
        }
        last.insert(hit.visit_id, hit.created_at);
    }

    Ok(())
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
