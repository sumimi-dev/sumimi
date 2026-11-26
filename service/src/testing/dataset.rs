//! A production-like dataset, and the numbers a correct implementation must produce.
//!
//! The point of this module is that **the expected numbers are computed here, in
//! plain Rust, and never by running SQL**. A test that only compares two backends
//! against each other proves they agree — not that they are right. Both could be
//! wrong in the same way and the test would be green. Computing the truth
//! independently, here, is what makes the suite worth running.
//!
//! The dataset is meant to look like a real site's traffic, not like a fixture:
//! several websites with different shapes, sessions of a realistic length, visitors
//! that come back on later days, a daily traffic curve with quiet weekends, and
//! **every one of the 42 columns** populated with a plausible value — including the
//! awkward cases (a `NULL` and an empty string are not the same thing, a title with
//! emoji in it, a URL longer than most people write, a session that runs past
//! midnight).

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use crate::models::{Hit, HitKind, Revenue};
use crate::storage::{session_scan_limit, Dimension, ReportQuery, TimeRange};
use uuid::Uuid;

/// The first day of the dataset.
/// The first day of the dataset. (Not a `const`: `from_ymd_opt` only became
/// `const` in Rust 1.83, and this crate supports 1.80.)
pub fn start_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()
}

/// How many days of traffic the dataset covers.
pub const DAYS: i64 = 45;

/// The three sites: `(name, domain)`.
///
/// The runner creates these websites through the backend — the backend assigns the
/// ids — and then builds the dataset around the ids it got back.
pub const SITES: [(&str, &str); 3] = [
    ("The Blog", "blog.example.com"),
    ("The Docs", "docs.example.com"),
    ("The Landing Page", "example.com"),
];

/// A window the suite checks.
#[derive(Debug, Clone)]
pub struct Window {
    /// What the window is for.
    pub name: &'static str,
    /// The website it is about.
    pub website_id: String,
    /// The window itself.
    pub range: TimeRange,
    /// How many rows the breakdowns may return.
    pub limit: i64,
}

/// The numbers a correct implementation must produce for one window.
#[derive(Debug)]
pub struct WindowExpectation {
    /// The window.
    pub window: Window,
    /// `COUNT(*)`.
    pub pageviews: i64,
    /// `COUNT(DISTINCT visitor_id)`.
    pub visitors: i64,
    /// `COUNT(DISTINCT session_id)`.
    pub views: i64,
    /// Sessions with a single page view.
    pub bounces: i64,
    /// The average session length, in seconds.
    pub visit_duration: f64,
    /// The breakdowns, per dimension: `(name, value)`, in the order the API returns
    /// them (`value DESC, name ASC`).
    pub breakdowns: BTreeMap<&'static str, Vec<(String, i64)>>,
    /// The time series: `(bucket, value)`.
    pub timeseries: Vec<(String, i64)>,
    /// The sessions: `(session id, views, duration in seconds)`.
    pub sessions: Vec<(String, i64, i64)>,
    /// The event log rows, newest first: `(visit id, visitor id, url, name, created at)`.
    pub event_log: Vec<EventLogRow>,
    /// The revenue records in the window.
    pub revenue: Vec<(String, String, f64)>,
}

/// Everything a correct implementation must produce.
#[derive(Debug)]
pub struct Expectations {
    /// One entry per window.
    pub windows: Vec<WindowExpectation>,
    /// The total number of hits written.
    pub hits: usize,
    /// The total number of revenue records written.
    pub revenue: usize,
    /// The number of distinct visitors in the whole dataset.
    pub visitors: usize,
    /// The number of distinct sessions in the whole dataset.
    pub sessions: usize,
}

/// One row of the event log: the two ids, the page, the event name and when.
pub type EventLogRow = (String, String, Option<String>, Option<String>, NaiveDateTime);

/// The dataset itself.
#[derive(Debug)]
pub struct Dataset {
    /// The websites.
    pub websites: Vec<crate::models::Website>,
    /// Every hit, in the order it was written.
    pub hits: Vec<Hit>,
    /// Every revenue record.
    pub revenue: Vec<Revenue>,
    /// The windows the suite checks.
    pub windows: Vec<Window>,
    /// The numbers a correct implementation must produce.
    pub expected: Expectations,
}

/// Is this hit a custom event (or an identify call)?
///
/// Neither is a page view — the pageview metrics leave them out.
fn is_custom(hit: &Hit) -> bool {
    matches!(hit.kind, HitKind::Event | HitKind::Identify)
}

/// A tiny deterministic PRNG, so a failing run can be reproduced exactly.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn between(&mut self, low: i64, high: i64) -> i64 {
        if high <= low {
            return low;
        }
        low + (self.next() % ((high - low + 1) as u64)) as i64
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// The pages of a site.
struct Site {
    id: String,
    name: String,
    domain: String,
    /// `(path, title, weight)`.
    pages: Vec<(&'static str, &'static str, u64)>,
    /// Visitors per day, on a normal weekday.
    visitors_per_day: i64,
    /// The referrers, with their weights. An empty string means "direct".
    referrers: Vec<(&'static str, u64)>,
    /// `(browser, browser version, os, os version, device, screen, language, country, region, city)`.
    profiles: Vec<Profile>,
    /// The campaigns, with their weights.
    campaigns: Vec<(&'static str, u64)>,
}

/// One visitor's device profile.
#[derive(Clone)]
struct Profile {
    browser: &'static str,
    browser_version: &'static str,
    os: &'static str,
    os_version: &'static str,
    device: &'static str,
    screen: &'static str,
    language: &'static str,
    country: &'static str,
    region: &'static str,
    city: &'static str,
    /// Where the city is, in degrees.
    latitude: f64,
    longitude: f64,
}

/// Build the dataset.
///
/// `visitors_per_day` scales every site down; the default is small enough for a
/// test to run in seconds and large enough that every dimension has many values.
pub fn build(visitors_per_day: i64, website_ids: &[String]) -> Dataset {
    assert_eq!(
        website_ids.len(),
        3,
        "the dataset has three sites; the caller creates them and passes their ids"
    );
    let mut rng = Rng::new(0x5EED_1234_ABCD_0025);

    let sites = vec![
        Site {
            id: website_ids[0].clone(),
            name: SITES[0].0.to_string(),
            domain: SITES[0].1.to_string(),
            pages: vec![
                ("/", "The Blog — home", 100),
                ("/posts/why-analytics-is-hard", "Why analytics is hard", 40),
                ("/posts/duckdb-vs-postgres", "DuckDB vs Postgres", 30),
                ("/posts/self-hosting-on-a-budget", "Self-hosting on a budget", 25),
                ("/posts/rust-for-backend-people", "Rust for backend people", 20),
                ("/tags/rust", "Tag: rust", 8),
                ("/tags/duckdb", "Tag: duckdb", 6),
                ("/feed.xml", "RSS feed", 5),
                ("/about", "About", 4),
                ("/archive/2026", "Archive 2026", 3),
            ],
            visitors_per_day,
            referrers: vec![
                ("", 220),
                ("news.ycombinator.com", 90),
                ("www.google.com", 120),
                ("duckduckgo.com", 40),
                ("github.com", 30),
                ("x.com", 15),
                ("reddit.com", 12),
            ],
            campaigns: vec![("newsletter", 40), ("twitter", 25), ("google", 20), ("hn", 15)],
            profiles: profiles(),
        },
        Site {
            id: website_ids[1].clone(),
            name: SITES[1].0.to_string(),
            domain: SITES[1].1.to_string(),
            pages: vec![
                ("/", "Documentation", 60),
                ("/docs/install", "Install", 40),
                ("/docs/configure", "Configure", 30),
                ("/docs/api", "API reference", 25),
                ("/docs/faq", "FAQ", 12),
                ("/changelog", "Changelog", 10),
            ],
            visitors_per_day: (visitors_per_day / 3).max(1),
            referrers: vec![
                ("", 120),
                ("www.google.com", 80),
                ("github.com", 60),
                ("news.ycombinator.com", 25),
            ],
            campaigns: vec![("google", 50), ("twitter", 30), ("newsletter", 20)],
            profiles: profiles(),
        },
        Site {
            id: website_ids[2].clone(),
            name: SITES[2].0.to_string(),
            domain: SITES[2].1.to_string(),
            pages: vec![
                ("/", "Example — the product", 100),
                ("/pricing", "Pricing", 30),
                ("/signup", "Sign up", 20),
                ("/demo", "Watch the demo", 15),
            ],
            visitors_per_day: (visitors_per_day / 2).max(1),
            referrers: vec![
                ("", 100),
                ("www.google.com", 70),
                ("x.com", 40),
                ("linkedin.com", 25),
            ],
            campaigns: vec![("google", 60), ("linkedin", 40)],
            profiles: profiles(),
        },
    ];

    let websites: Vec<crate::models::Website> = sites
        .iter()
        .map(|site| crate::models::Website {
            id: site.id.clone(),
            name: site.name.clone(),
            domain: Some(site.domain.clone()),
            created_at: start_date().and_hms_opt(0, 0, 0).unwrap(),
            updated_at: start_date().and_hms_opt(0, 0, 0).unwrap(),
            retention_days: 0,
            country_header: None,
        })
        .collect();

    let mut hits: Vec<Hit> = Vec::new();
    let mut revenue: Vec<Revenue> = Vec::new();
    // Visitors that came back on a later day.
    let mut returning_visitors: Vec<Uuid> = Vec::new();
    let mut visitor_counter = 0u64;
    // The visit ids are a counter — the server's own.
    let mut visit_counter = 0u64;

    for site in &sites {
        for day in 0..DAYS {
            let date = start_date() + Duration::days(day);
            let weekday = date.weekday().num_days_from_monday() as i64;
            let weekend = if weekday >= 5 { 0.55 } else { 1.0 };
            let growth = 1.0 + (day as f64) * 0.006;
            let visitors =
                ((site.visitors_per_day as f64) * weekend * growth) as i64;

            for _ in 0..visitors {
                // A visitor is either new or one that came back.
                let visitor_id = if !returning_visitors.is_empty() && rng.chance(22) {
                    let index = rng.below(returning_visitors.len());
                    returning_visitors.remove(index)
                } else {
                    visitor_counter += 1;
                    uuid_of(&format!("visitor-{visitor_counter:08x}"))
                };
                // Some visitors come back later.
                if rng.chance(18) {
                    returning_visitors.push(visitor_id);
                }

                // The visitor is the pair (the address prefix, the user agent);
                // the session id is eight bytes and carries the segment's start.
                let (ip, ip_prefix, user_agent) =
                    crate::testing::visitor_identity(&visitor_id);

                let profile = rng.pick(&site.profiles).clone();
                let referrer = weighted(&mut rng, &site.referrers);
                let campaign = if rng.chance(30) {
                    Some(weighted(&mut rng, &site.campaigns).to_string())
                } else {
                    None
                };

                // The session starts somewhere in the day.
                let start = date.and_hms_opt(0, 0, 0).unwrap()
                    + Duration::seconds(rng.between(0, 86_399));
                // The visitor: a hash of the address, the agent and the site.
                let visitor_id = crate::ua::visitor_id(
                    &crate::ua::visitor_address(&ip),
                    &user_agent,
                    &site.id,
                );
                visit_counter += 1;
                let session_id = crate::event_id::compose_version(
                    crate::event_id::VISIT_VERSION,
                    1,
                    visit_counter,
                );

                // How many pages the session looks at.
                let views = match rng.below(100) {
                    0..=34 => 1,
                    35..=59 => 2,
                    60..=76 => 3,
                    77..=88 => 4,
                    89..=95 => 5,
                    96..=98 => 6,
                    _ => 7,
                };

                let landing = weighted_page(&mut rng, &site.pages);
                let mut url = landing.0.to_string();
                let mut title = landing.1.to_string();

                for view in 0..views {
                    // The pages after the first are chosen from the site too.
                    if view > 0 {
                        let page = weighted_page(&mut rng, &site.pages);
                        url = page.0.to_string();
                        title = page.1.to_string();
                    }

                    // The marketing parameters, on the landing page of a campaign
                    // session only — like a real site, where the tags survive the
                    // session.
                    let (url_query, utm_source, utm_medium, utm_campaign, utm_content, utm_term) =
                        if view == 0 && campaign.is_some() {
                            let campaign = campaign.as_deref().unwrap();
                            (
                                Some(format!(
                                    "utm_source={campaign}&utm_medium=cpc&utm_campaign=spring-2026&utm_content=cta&utm_term=analytics"
                                )),
                                Some(campaign.to_string()),
                                Some("cpc".to_string()),
                                Some("spring-2026".to_string()),
                                Some("cta".to_string()),
                                Some("analytics".to_string()),
                            )
                        } else {
                            (None, None, None, None, None, None)
                        };

                    // The ad click id, on some campaign sessions.
                    let (gclid, fbclid, msclkid, ttclid, lifatid, twclid) = if view == 0
                        && campaign.is_some()
                        && rng.chance(45)
                    {
                        match rng.below(6) {
                            0 => (
                                Some(format!("gclid-{:08x}", rng.next() as u32)),
                                None,
                                None,
                                None,
                                None,
                                None,
                            ),
                            1 => (
                                None,
                                Some(format!("fbclid-{:08x}", rng.next() as u32)),
                                None,
                                None,
                                None,
                                None,
                            ),
                            2 => (
                                None,
                                None,
                                Some(format!("msclkid-{:08x}", rng.next() as u32)),
                                None,
                                None,
                                None,
                            ),
                            3 => (
                                None,
                                None,
                                None,
                                Some(format!("ttclid-{:08x}", rng.next() as u32)),
                                None,
                                None,
                            ),
                            4 => (
                                None,
                                None,
                                None,
                                None,
                                Some(format!("lifatid-{:08x}", rng.next() as u32)),
                                None,
                            ),
                            _ => (
                                None,
                                None,
                                None,
                                None,
                                None,
                                Some(format!("twclid-{:08x}", rng.next() as u32)),
                            ),
                        }
                    } else {
                        (None, None, None, None, None, None)
                    };

                    // The web vitals. A tenth of the page views have none at all —
                    // the visitor left before the page finished painting.
                    let (lcp, inp, cls, fcp, ttfb) = if rng.chance(10) {
                        (None, None, None, None, None)
                    } else {
                        (
                            Some(600.0 + rng.between(0, 2400) as f64),
                            Some(20.0 + rng.between(0, 300) as f64),
                            Some(rng.between(0, 120) as f64 / 1000.0),
                            Some(300.0 + rng.between(0, 1500) as f64),
                            Some(40.0 + rng.between(0, 400) as f64),
                        )
                    };

                    // A session that runs past midnight: the timestamp is on the
                    // next day.
                    let at = start + Duration::seconds(view * rng.between(8, 90));

                    hits.push(Hit {
                        website_id: site.id.clone(),
                        visit_id: session_id,
                        visitor_id,
                        session_start: Some(start),
                        referrer_name: if referrer.is_empty() {
                            None
                        } else {
                            crate::channels::referrer_name(Some(referrer))
                        },
                        latitude: Some(profile.latitude),
                        longitude: Some(profile.longitude),
                        ip: ip.clone(),
                        ip_prefix: ip_prefix.clone(),
                        user_agent: user_agent.clone(),
                        retention_days: 0,
                        kind: HitKind::Pageview,
                        created_at: at,
                        url: Some(url.clone()),
                        url_query: url_query.clone(),
                        title: Some(title.clone()),
                        referrer: if referrer.is_empty() {
                            None
                        } else {
                            Some(referrer.to_string())
                        },
                        channel: Some(
                            crate::channels::channel(
                                &crate::channels::ChannelInput::new(
                                    Some(referrer),
                                    url_query.as_deref(),
                                    utm_medium.as_deref(),
                                    utm_source.as_deref(),
                                    Some(&site.domain),
                                ),
                            )
                            .to_string(),
                        ),
                        referrer_path: if referrer.is_empty() {
                            None
                        } else {
                            Some(format!("/item?id={}", rng.below(100_000)))
                        },
                        referrer_query: if referrer.is_empty() {
                            None
                        } else {
                            Some(format!("?id={}&utm_source=hn", rng.below(100_000)))
                        },
                        hostname: Some(site.domain.clone()),
                        browser: Some(profile.browser.to_string()),
                        browser_version: Some(profile.browser_version.to_string()),
                        os: Some(profile.os.to_string()),
                        os_version: Some(profile.os_version.to_string()),
                        device: Some(profile.device.to_string()),
                        screen: Some(profile.screen.to_string()),
                        language: Some(profile.language.to_string()),
                        country: Some(profile.country.to_string()),
                        region: Some(profile.region.to_string()),
                        city: Some(profile.city.to_string()),
                        utm_source,
                        utm_medium,
                        utm_campaign,
                        utm_content,
                        utm_term,
                        gclid,
                        fbclid,
                        msclkid,
                        ttclid,
                        lifatid,
                        twclid,
                        lcp,
                        inp,
                        cls,
                        fcp,
                        ttfb,
                        name: None,
                        tag: None,
                        properties: Vec::new(),
                        ver: None,
    user_id: None,
    });
                }

                // A third of the sessions fire a custom event.
                if rng.chance(33) {
                    let name = *rng.pick(&["signup", "download", "cta_click", "newsletter"]);
                    hits.push(Hit {
                        website_id: site.id.clone(),
                        visit_id: session_id,
                        visitor_id,
                        session_start: Some(start),
                        referrer_name: if referrer.is_empty() {
                            None
                        } else {
                            crate::channels::referrer_name(Some(referrer))
                        },
                        latitude: Some(profile.latitude),
                        longitude: Some(profile.longitude),
                        ip: ip.clone(),
                        ip_prefix: ip_prefix.clone(),
                        user_agent: user_agent.clone(),
                        retention_days: 0,
                        kind: HitKind::Event,
                        created_at: start + Duration::seconds(rng.between(30, 600)),
                        url: Some(url.clone()),
                        url_query: None,
                        title: Some(title.clone()),
                        referrer: None,
                        referrer_path: None,
                        referrer_query: None,
                        channel: Some("direct".to_string()),
                        hostname: Some(site.domain.clone()),
                        browser: Some(profile.browser.to_string()),
                        browser_version: Some(profile.browser_version.to_string()),
                        os: Some(profile.os.to_string()),
                        os_version: Some(profile.os_version.to_string()),
                        device: Some(profile.device.to_string()),
                        screen: Some(profile.screen.to_string()),
                        language: Some(profile.language.to_string()),
                        country: Some(profile.country.to_string()),
                        region: Some(profile.region.to_string()),
                        city: Some(profile.city.to_string()),
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
                        properties: vec![
                            crate::models::Property {
                                key: "plan".to_string(),
                                value: crate::models::PropertyValue::Text(
                                    (*rng.pick(&["free", "pro"])).to_string(),
                                ),
                            },
                            // A **number** beside a text one: the two kinds
                            // go into different columns, and a number has to
                            // come back as a number and not as text.
                            crate::models::Property {
                                key: "seats".to_string(),
                                value: crate::models::PropertyValue::Number(
                                    *rng.pick(&[1.0, 3.0, 10.0]),
                                ),
                            },
                        ],
                        ver: None,
    user_id: None,
    });
                }

                // Some sessions earn money.
                if rng.chance(12) {
                    revenue.push(Revenue {
                        website_id: site.id.clone(),
                        visit_id: Some(session_id),
                        event_id: None,
                        event_name: Some("carbon".to_string()),
                        currency: "USD".to_string(),
                        revenue: (rng.between(1, 400) as f64) / 1000.0,
                        created_at: start + Duration::seconds(rng.between(60, 1800)),
                    });
                }
            }
        }
    }

    // The edge cases a real site produces, on top of the generated traffic.
    let edge = edge_cases(&sites[0]);
    hits.extend(edge.0);
    revenue.extend(edge.1);

    let windows = windows(&sites);
    let expected = compute_expectations(&hits, &revenue, &windows);

    Dataset {
        websites,
        hits,
        revenue,
        windows,
        expected,
    }
}

/// The windows the suite checks.
fn windows(sites: &[Site]) -> Vec<Window> {
    let blog = sites[0].id.clone();
    let docs = sites[1].id.clone();
    let landing = sites[2].id.clone();
    let at = |day: i64, hour: u32| {
        (start_date() + Duration::days(day)).and_hms_opt(hour, 0, 0).unwrap()
    };
    vec![
        // A full month of the busiest site.
        Window {
            name: "the whole month",
            website_id: blog.clone(),
            range: TimeRange {
                start: at(0, 0),
                end: at(30, 0),
            },
            limit: 50,
        },
        // A single day.
        Window {
            name: "one day",
            website_id: blog.clone(),
            range: TimeRange {
                start: at(10, 0),
                end: at(11, 0),
            },
            limit: 50,
        },
        // A short window — the time series buckets by hour here.
        Window {
            name: "two hours",
            website_id: blog.clone(),
            range: TimeRange {
                start: at(5, 9),
                end: at(5, 11),
            },
            limit: 50,
        },
        // A window with no traffic at all.
        Window {
            name: "an empty window",
            website_id: blog.clone(),
            range: TimeRange {
                start: at(-10, 0),
                end: at(-9, 0),
            },
            limit: 50,
        },
        // The whole dataset of the second site.
        Window {
            name: "the docs site",
            website_id: docs,
            range: TimeRange {
                start: at(0, 0),
                end: at(DAYS, 0),
            },
            limit: 50,
        },
        // A third site, to prove the filter by website works.
        Window {
            name: "the landing page",
            website_id: landing,
            range: TimeRange {
                start: at(0, 0),
                end: at(DAYS, 0),
            },
            limit: 50,
        },
    ]
}

/// The awkward cases, written by hand.
///
/// A real site produces these; a random generator never does.
fn edge_cases(
    site: &Site,
) -> (
    Vec<Hit>,
    Vec<Revenue>,
) {
    let base = site.id.clone();
    let visitor = uuid_of("visitor-edge-case-0001");
    let (ip, ip_prefix, user_agent) = crate::testing::visitor_identity(&visitor);
    let mut hits = Vec::new();

    // A title with an emoji and non-latin characters.
    hits.push(Hit {
        website_id: base.clone(),
        visit_id: crate::session::client_session_id("session-edge-unicode"),
        visitor_id: visitor,
        session_start: None,
        referrer_name: None,
        latitude: None,
        longitude: None,
        ip: ip.clone(),
        ip_prefix: ip_prefix.clone(),
        user_agent: user_agent.clone(),
        retention_days: 0,
        kind: HitKind::Pageview,
        created_at: start_date().and_hms_opt(3, 30, 0).unwrap(),
        url: Some("/posts/中文-标题".to_string()),
        url_query: None,
        title: Some("中文标题 with an emoji 🐱 and an em dash —".to_string()),
        referrer: Some("news.ycombinator.com".to_string()),
        channel: Some("social".to_string()),
        referrer_path: Some("/item?id=1".to_string()),
        referrer_query: Some("?id=1".to_string()),
        hostname: Some(site.domain.clone()),
        browser: Some("Chrome".to_string()),
        browser_version: Some("120".to_string()),
        os: Some("macOS".to_string()),
        os_version: Some("10.15".to_string()),
        device: Some("desktop".to_string()),
        screen: Some("1920x1080".to_string()),
        language: Some("zh-CN".to_string()),
        country: Some("CN".to_string()),
        region: Some("Beijing".to_string()),
        city: Some("Beijing".to_string()),
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
        name: None,
        tag: None,
        properties: Vec::new(),
        ver: None,
    user_id: None,
    });

    // A very long URL and a very long title.
    hits.push(Hit {
        website_id: base.clone(),
        visit_id: crate::session::client_session_id("session-edge-long"),
        visitor_id: visitor,
        session_start: None,
        referrer_name: None,
        latitude: None,
        longitude: None,
        ip: ip.clone(),
        ip_prefix: ip_prefix.clone(),
        user_agent: user_agent.clone(),
        retention_days: 0,
        kind: HitKind::Pageview,
        created_at: start_date().and_hms_opt(4, 0, 0).unwrap(),
        url: Some(format!("/posts/{}", "a-very-long-slug-".repeat(40))),
        url_query: Some(format!("q={}", "x".repeat(500))),
        title: Some(format!("A very long title {}", "that goes on ".repeat(30))),
        referrer: None,
        referrer_path: None,
        referrer_query: None,
        channel: Some("direct".to_string()),
        hostname: Some(site.domain.clone()),
        browser: Some("Safari".to_string()),
        browser_version: Some("17".to_string()),
        os: Some("iOS".to_string()),
        os_version: Some("17".to_string()),
        device: Some("mobile".to_string()),
        screen: Some("390x844".to_string()),
        language: Some("en-GB".to_string()),
        country: Some("GB".to_string()),
        region: Some("England".to_string()),
        city: Some("London".to_string()),
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
        lcp: Some(1.0),
        inp: Some(1.0),
        cls: Some(0.0),
        fcp: Some(1.0),
        ttfb: Some(1.0),
        name: None,
        tag: None,
        properties: Vec::new(),
        ver: None,
    user_id: None,
    });

    // A session that runs past midnight: two hits, 23:59:30 and 00:00:30.
    for (step, url) in [(0i64, "/posts/midnight-before"), (1, "/posts/midnight-after")] {
        hits.push(Hit {
            website_id: base.clone(),
            visit_id: crate::session::client_session_id("session-edge-midnight"),
            visitor_id: visitor,
            session_start: None,
            referrer_name: None,
            latitude: None,
            longitude: None,
            ip: ip.clone(),
        ip_prefix: ip_prefix.clone(),
        user_agent: user_agent.clone(),
            retention_days: 0,
            kind: HitKind::Pageview,
            created_at: start_date().and_hms_opt(23, 59, 30).unwrap() + Duration::seconds(step * 60),
            url: Some(url.to_string()),
            url_query: None,
            title: Some("Midnight".to_string()),
            referrer: Some(String::new()),
            channel: Some("direct".to_string()),
            referrer_path: None,
            referrer_query: None,
            hostname: Some(site.domain.clone()),
            browser: Some("Firefox".to_string()),
            browser_version: Some("130".to_string()),
            os: Some("Windows".to_string()),
            os_version: Some("11".to_string()),
            device: Some("desktop".to_string()),
            screen: Some("1366x768".to_string()),
            language: Some("de-DE".to_string()),
            country: Some("DE".to_string()),
            region: Some("Berlin".to_string()),
            city: Some("Berlin".to_string()),
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
            lcp: Some(500.0),
            inp: Some(50.0),
            cls: Some(0.01),
            fcp: Some(400.0),
            ttfb: Some(50.0),
            name: None,
            tag: None,
            properties: Vec::new(),
        ver: None,
    user_id: None,
    });
    }

    // A session with exactly one view: it counts as a bounce.
    hits.push(Hit {
        website_id: base.clone(),
        visit_id: crate::session::client_session_id("session-edge-bounce"),
        visitor_id: visitor,
        session_start: None,
        referrer_name: None,
        latitude: None,
        longitude: None,
        ip: ip.clone(),
        ip_prefix: ip_prefix.clone(),
        user_agent: user_agent.clone(),
        retention_days: 0,
        kind: HitKind::Pageview,
        created_at: start_date().and_hms_opt(6, 0, 0).unwrap(),
        url: Some("/".to_string()),
        url_query: None,
        title: Some("Home".to_string()),
        referrer: None,
        referrer_path: None,
        referrer_query: None,
        channel: Some("direct".to_string()),
        hostname: Some(site.domain.clone()),
        browser: Some("Chrome".to_string()),
        browser_version: Some("120".to_string()),
        os: Some("Windows".to_string()),
        os_version: Some("11".to_string()),
        device: Some("desktop".to_string()),
        screen: Some("1920x1080".to_string()),
        language: Some("en-US".to_string()),
        country: Some("US".to_string()),
        region: Some("California".to_string()),
        city: Some("San Francisco".to_string()),
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
        name: None,
        tag: None,
        properties: Vec::new(),
        ver: None,
    user_id: None,
    });

    // A revenue record with a currency other than USD.
    let revenue = vec![Revenue {
        website_id: base.clone(),
        visit_id: Some(crate::session::client_session_id("session-edge-bounce")),
        event_id: None,
        event_name: Some("carbon".to_string()),
        currency: "EUR".to_string(),
        revenue: 0.42,
        created_at: start_date().and_hms_opt(6, 5, 0).unwrap(),
    }];

    (hits, revenue)
}

/// The visitor profiles.
fn profiles() -> Vec<Profile> {
    vec![
        Profile {
            browser: "Chrome",
            browser_version: "120",
            os: "Windows",
            os_version: "11",
            device: "desktop",
            screen: "1920x1080",
            language: "en-US",
            country: "US",
            region: "California",
            city: "San Francisco",
            latitude: 37.7749,
            longitude: -122.4194,
        },
        Profile {
            browser: "Safari",
            browser_version: "17",
            os: "macOS",
            os_version: "10.15",
            device: "desktop",
            screen: "2560x1440",
            language: "en-US",
            country: "US",
            region: "New York",
            city: "New York",
            latitude: 40.7128,
            longitude: -74.006,
        },
        Profile {
            browser: "Safari",
            browser_version: "17",
            os: "iOS",
            os_version: "17",
            device: "mobile",
            screen: "390x844",
            language: "en-US",
            country: "US",
            region: "Texas",
            city: "Austin",
            latitude: 30.2672,
            longitude: -97.7431,
        },
        Profile {
            browser: "Chrome",
            browser_version: "120",
            os: "Android",
            os_version: "14",
            device: "mobile",
            screen: "412x915",
            language: "en-GB",
            country: "GB",
            region: "England",
            city: "London",
            latitude: 51.5074,
            longitude: -0.1278,
        },
        Profile {
            browser: "Firefox",
            browser_version: "130",
            os: "Windows",
            os_version: "10",
            device: "desktop",
            screen: "1366x768",
            language: "de-DE",
            country: "DE",
            region: "Berlin",
            city: "Berlin",
            latitude: 52.52,
            longitude: 13.405,
        },
        Profile {
            browser: "Edge",
            browser_version: "120",
            os: "Windows",
            os_version: "11",
            device: "desktop",
            screen: "1536x864",
            language: "fr-FR",
            country: "FR",
            region: "Ile-de-France",
            city: "Paris",
            latitude: 48.8566,
            longitude: 2.3522,
        },
        Profile {
            browser: "Chrome",
            browser_version: "119",
            os: "Linux",
            os_version: "6",
            device: "desktop",
            screen: "1920x1200",
            language: "ja-JP",
            country: "JP",
            region: "Tokyo",
            city: "Tokyo",
            latitude: 35.6762,
            longitude: 139.6503,
        },
        Profile {
            browser: "Safari",
            browser_version: "16",
            os: "iOS",
            os_version: "16",
            device: "tablet",
            screen: "768x1024",
            language: "zh-CN",
            country: "CN",
            region: "Zhejiang",
            city: "Hangzhou",
            latitude: 30.2741,
            longitude: 120.1551,
        },
    ]
}

/// A UUID for a text id.
///
/// The dataset's ids are text; the schema stores them as 16-byte UUIDs. The
/// mapping has to be the same as the one the server uses, so the dataset asks
/// the service crate for it.
fn uuid_of(text: &str) -> Uuid {
    crate::ua::uuid_from_text(text)
}

/// A weighted pick.
fn weighted<'a>(rng: &mut Rng, items: &'a [(&'static str, u64)]) -> &'a str {
    let total: u64 = items.iter().map(|(_, w)| w).sum();
    let mut roll = rng.next() % total;
    for (value, weight) in items {
        if roll < *weight {
            return value;
        }
        roll -= weight;
    }
    items[0].0
}

/// A weighted pick among the pages of a site.
fn weighted_page<'a>(
    rng: &mut Rng,
    pages: &'a [(&'static str, &'static str, u64)],
) -> &'a (&'static str, &'static str, u64) {
    let total: u64 = pages.iter().map(|(_, _, w)| w).sum();
    let mut roll = rng.next() % total;
    for page in pages {
        if roll < page.2 {
            return page;
        }
        roll -= page.2;
    }
    &pages[0]
}

/// Compute the numbers a correct implementation must produce.
///
/// This is a **second implementation** of the metric definitions, in plain Rust.
/// It never runs SQL — that is the whole point.
fn compute_expectations(
    hits: &[Hit],
    revenue: &[Revenue],
    windows: &[Window],
) -> Expectations {
    let mut out = Vec::new();

    for window in windows {
        let in_window: Vec<&Hit> = hits
            .iter()
            .filter(|hit| {
                hit.website_id == window.website_id
                    && hit.created_at >= window.range.start
                    && hit.created_at < window.range.end
            })
            .collect();

        // The **page views**: a custom event is not a page view, and neither
        // is an identify call — the same rule the backends and the reference
        // implementation use.
        let pageviews: Vec<&Hit> = in_window
            .iter()
            .copied()
            .filter(|hit| !is_custom(hit))
            .collect();

        // A visitor is counted by its **id** — the hash of the address, the
        // agent and the site. It is what the backends count, and it is what
        // the 口径 says: the visitor is the id, not the pair.
        //
        // **The rule is the reference implementation's** (since v12): the
        // visitors and the visits are counted over the groups that have **at
        // least one page view** ⇒ ⇒ a visitor whose only event was a custom
        // event is not a visitor, and the visit it was in is not a visit.
        // ⇒ ⇒ So they are counted over the page views — the same rows the
        // backends count them over.
        let mut visitors: HashSet<uuid::Uuid> = HashSet::new();
        let mut sessions: HashSet<uuid::Uuid> = HashSet::new();
        for hit in &pageviews {
            visitors.insert(hit.visitor_id);
            sessions.insert(hit.visit_id);
        }

        // The session aggregates: the page views of each session, the length of
        // each session, and whether it fired a custom event.
        //
        // A bounce is a session with a single **page view** that did not fire
        // a custom event — the same rule the reference implementation uses.
        let mut per_session: BTreeMap<uuid::Uuid, (i64, NaiveDateTime, NaiveDateTime, bool)> =
            BTreeMap::new();
        for hit in &in_window {
            let entry = per_session
                .entry(hit.visit_id)
                .or_insert((0, hit.created_at, hit.created_at, false));
            if is_custom(hit) {
                entry.3 = true;
                continue;
            }
            entry.0 += 1;
            if hit.created_at < entry.1 {
                entry.1 = hit.created_at;
            }
            if hit.created_at > entry.2 {
                entry.2 = hit.created_at;
            }
        }
        let bounces = per_session
            .values()
            .filter(|(n, _, _, custom)| *n == 1 && !*custom)
            .count() as i64;
        // The sessions with at least one page view: the others have no length.
        let lengths: Vec<f64> = per_session
            .values()
            .filter(|(n, _, _, _)| *n > 0)
            .map(|(_, first, last, _)| (*last - *first).num_seconds() as f64)
            .collect();
        let duration: f64 = if lengths.is_empty() {
            0.0
        } else {
            lengths.iter().sum::<f64>() / lengths.len() as f64
        };

        // The breakdowns.
        let mut breakdowns = BTreeMap::new();
        for (label, dimension) in [
            ("url", Dimension::Url),
            ("referrer", Dimension::Referrer),
            ("browser", Dimension::Browser),
            ("os", Dimension::Os),
            ("device", Dimension::Device),
            ("country", Dimension::Country),
            ("language", Dimension::Language),
            ("screen", Dimension::Screen),
            ("channel", Dimension::Channel),
            ("region", Dimension::Region),
            ("city", Dimension::City),
            ("host", Dimension::Host),
            ("tag", Dimension::Tag),
        ] {
            let mut counts: HashMap<String, i64> = HashMap::new();
            let mut visitors_by: HashMap<String, HashSet<uuid::Uuid>> = HashMap::new();
            for hit in &pageviews {
                let name = match dimension {
                    Dimension::Url => hit.url.clone(),
                    Dimension::Referrer => hit.referrer.clone(),
                    Dimension::Browser => hit.browser.clone(),
                    Dimension::Os => hit.os.clone(),
                    Dimension::Device => hit.device.clone(),
                    Dimension::Country => hit.country.clone(),
                    Dimension::Language => hit.language.clone(),
                    Dimension::Screen => hit.screen.clone(),
                    Dimension::Channel => hit.channel.clone(),
                    Dimension::Region => hit.region.clone(),
                    Dimension::City => hit.city.clone(),
                    Dimension::Host => hit.hostname.clone(),
                    Dimension::Tag => hit.tag.clone(),
                }
                .unwrap_or_else(|| "unknown".to_string());
                match dimension.aggregate() {
                    "views" => {
                        *counts.entry(name).or_insert(0) += 1;
                    }
                    _ => {
                        visitors_by.entry(name).or_default().insert(hit.visitor_id);
                    }
                }
            }
            let mut rows: Vec<(String, i64)> = if dimension.aggregate() == "views" {
                counts.into_iter().collect()
            } else {
                visitors_by
                    .into_iter()
                    .map(|(name, set)| (name, set.len() as i64))
                    .collect()
            };
            // `ORDER BY value DESC, name ASC`, then the limit.
            rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            rows.truncate(window.limit.clamp(1, 1000) as usize);
            breakdowns.insert(label, rows);
        }

        // The time series: by hour for a short window, by day for a long one.
        let by_hour = (window.range.end - window.range.start).num_seconds() <= 2 * 24 * 3600;
        let mut buckets: BTreeMap<String, i64> = BTreeMap::new();
        for hit in &pageviews {
            let bucket = if by_hour {
                hit.created_at.format("%Y-%m-%dT%H:00:00Z").to_string()
            } else {
                hit.created_at.format("%Y-%m-%dT00:00:00Z").to_string()
            };
            *buckets.entry(bucket).or_insert(0) += 1;
        }
        let timeseries = buckets.into_iter().collect();

        // The session log — and this is the part that is **not** a summary of the
        // whole window: the log reads the most recent events and groups those,
        // so its cost does not grow with the size of the table.
        //
        // The rule has to be the same on both sides, so it is written out here
        // rather than borrowed from the implementation: the most recent events
        // (newest first, the row id breaks the tie), then grouped by session.
        let mut recent: Vec<(usize, &Hit)> = in_window.iter().enumerate().map(|(i, h)| (i, *h)).collect();
        recent.sort_by(|a, b| {
            b.1.created_at
                .cmp(&a.1.created_at)
                .then_with(|| b.0.cmp(&a.0))
        });
        recent.truncate(session_scan_limit(window.limit) as usize);

        let mut recent_sessions: BTreeMap<uuid::Uuid, (i64, NaiveDateTime, NaiveDateTime)> =
            BTreeMap::new();
        for (_, hit) in &recent {
            let entry = recent_sessions
                .entry(hit.visit_id)
                .or_insert((0, hit.created_at, hit.created_at));
            entry.0 += 1;
            if hit.created_at < entry.1 {
                entry.1 = hit.created_at;
            }
            if hit.created_at > entry.2 {
                entry.2 = hit.created_at;
            }
        }

        // The sessions, newest first — the same order and the same limit the API
        // uses, so the two sides pick the same rows.
        let mut session_rows: Vec<(String, i64, i64, NaiveDateTime)> = recent_sessions
            .iter()
            .map(|(id, (views, first, last))| {
                (
                    crate::session::format_visit_id(*id),
                    *views,
                    (*last - *first).num_seconds(),
                    *first,
                )
            })
            .collect();
        session_rows.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
        session_rows.truncate(window.limit.clamp(1, 1000) as usize);
        let session_rows: Vec<(String, i64, i64)> = session_rows
            .into_iter()
            .map(|(id, views, duration, _)| (id, views, duration))
            .collect();

        // The event log, newest first. The row id is the insertion order, so it
        // breaks the ties exactly like the API does.
        let mut event_log: Vec<(EventLogRow, usize)> =
            in_window
                .iter()
                .enumerate()
                .map(|(index, hit)| {
                    (
                        (
                            crate::session::format_visit_id(hit.visit_id),
                            // The visitor's id in the text form the log shows
                            // it: the hex of its bytes — the first eight of
                            // the hash (see `crate::ua`).
                            crate::ua::format_visitor_id(&crate::ua::visitor_bytes(
                                &hit.visitor_id,
                            )),
                            hit.url.clone(),
                            hit.name.clone(),
                            hit.created_at,
                        ),
                        index,
                    )
                })
                .collect();
        event_log.sort_by(|a, b| b.0 .4.cmp(&a.0 .4).then_with(|| b.1.cmp(&a.1)));
        event_log.truncate(window.limit.clamp(1, 1000) as usize);
        let event_log: Vec<EventLogRow> = event_log.into_iter().map(|(row, _)| row).collect();

        // The revenue.
        let mut revenue_rows: Vec<(String, String, f64, NaiveDateTime)> = revenue
            .iter()
            .filter(|record| {
                record.website_id == window.website_id
                    && record.created_at >= window.range.start
                    && record.created_at < window.range.end
            })
            .map(|record| {
                (
                    record
                        .visit_id
                        .map(crate::session::format_visit_id)
                        .unwrap_or_default(),
                    record.currency.clone(),
                    record.revenue,
                    record.created_at,
                )
            })
            .collect();
        revenue_rows.sort_by_key(|row| std::cmp::Reverse(row.3));
        revenue_rows.truncate(window.limit.clamp(1, 1000) as usize);
        let revenue_rows: Vec<(String, String, f64)> = revenue_rows
            .into_iter()
            .map(|(session, currency, amount, _)| (session, currency, amount))
            .collect();

        out.push(WindowExpectation {
            window: window.clone(),
            pageviews: pageviews.len() as i64,
            visitors: visitors.len() as i64,
            views: sessions.len() as i64,
            bounces,
            visit_duration: duration,
            breakdowns,
            timeseries,
            sessions: session_rows,
            event_log,
            revenue: revenue_rows,
        });
    }

    let all_visitors: HashSet<uuid::Uuid> = hits.iter().map(|h| h.visitor_id).collect();
    let all_sessions: HashSet<uuid::Uuid> = hits.iter().map(|h| h.visit_id).collect();

    Expectations {
        windows: out,
        hits: hits.len(),
        revenue: revenue.len(),
        visitors: all_visitors.len(),
        sessions: all_sessions.len(),
    }
}

/// The report query for a window.
pub fn query_for(window: &Window) -> ReportQuery {
    ReportQuery {
        website_id: window.website_id.clone(),
        range: window.range,
        dimensions: Default::default(),
        limit: window.limit,
        offset: 0,
        include_markers: Vec::new(),
    }
}
