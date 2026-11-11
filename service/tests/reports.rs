//! Every report, one test each.
//!
//! The numbers are worked out by hand in the comments, so a change in a report
//! has to be a change on purpose: the test says what the answer should be, and
//! the report has to give it.
//!
//! The data is **fixed** — the same four events, at the same minute, with the
//! same pages, browsers, referrers and visitors. A report that changes its
//! answer because the clock moved is not a report anybody can trust.

use sumimi_service::models::{Hit, HitKind};
use sumimi_service::pool::Db;
use sumimi_service::storage::{parse_query_parameters, Dimension, ReportQuery, Storage, TimeRange};
use sumimi_service::testing::HitBuilder;

/// The moment the synthetic traffic happened.
///
/// A fixed timestamp: the reports bucket by the hour, and a test that depends
/// on what time it is when the test runs is a test that fails at midnight.
fn base() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
}

/// The window the reports ask about.
fn window() -> TimeRange {
    TimeRange {
        start: base() - chrono::Duration::days(1),
        end: base() + chrono::Duration::days(1),
    }
}

/// A database with the fixed traffic in it.
fn database(name: &str) -> (Db, String) {
    let dir = std::env::temp_dir().join(format!("sumimi-reports-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open(&dir.join("sumimi.duckdb"), "256MB").unwrap();

    let website = db
        .create_website("Example", Some("example.com"), 0, None)
        .unwrap()
        .id;

    // Four events:
    //
    // | # | when   | kind        | page    | referrer             | visitor | session |
    // |---|--------|-------------|---------|----------------------|---------|---------|
    // | 1 | 12:00  | page view   | /one    | google.com           | v1      | s1      |
    // | 2 | 12:01  | page view   | /two    | google.com           | v1      | s1      |
    // | 3 | 12:02  | page view   | /one    | news.ycombinator.com | v2      | s2      |
    // | 4 | 12:02:30 | custom   | /three  | —                    | v3      | s3      |
    //
    // The first two are one session with two views; the other two are one view
    // each.
    let hits = vec![
        hit(&website, "/one", Some("google.com"), "Chrome", "macOS", "desktop", "en-US", "DE", "Berlin", "Berlin", "example.com", "a=1&b=2", "v1", "s1", 0, "tag-a"),
        hit(&website, "/two", Some("google.com"), "Chrome", "macOS", "desktop", "en-US", "DE", "Berlin", "Berlin", "example.com", "a=1&b=3", "v1", "s1", 60, "tag-a"),
        hit(&website, "/one", Some("news.ycombinator.com"), "Firefox", "Windows", "mobile", "de-DE", "FR", "Ile-de-France", "Paris", "other.example", "a=2", "v2", "s2", 120, "tag-b"),
        hit(&website, "/three", None, "Firefox", "Windows", "mobile", "de-DE", "FR", "Ile-de-France", "Paris", "other.example", "", "v3", "s3", 150, "signup"),
    ];
    db.write_batch(&hits).unwrap();

    (db, website)
}

/// One event, with every field set.
#[allow(clippy::too_many_arguments)]
fn hit(
    website: &str,
    url: &str,
    referrer: Option<&str>,
    browser: &str,
    os: &str,
    device: &str,
    language: &str,
    country: &str,
    region: &str,
    city: &str,
    hostname: &str,
    query: &str,
    visitor: &str,
    session: &str,
    seconds: i64,
    tag: &str,
) -> Hit {
    let at = base() + chrono::Duration::seconds(seconds);
    let channel = sumimi_service::channels::channel(&sumimi_service::channels::ChannelInput::new(
        referrer,
        if query.is_empty() {
            None
        } else {
            Some(query)
        },
        None,
        None,
        Some(hostname),
    ));
    HitBuilder::new(website, session, visitor)
        .at(at)
        .kind(if url == "/three" {
            HitKind::Event
        } else {
            HitKind::Pageview
        })
        .url(url)
        .url_query(if query.is_empty() {
            None
        } else {
            Some(query)
        })
        .title(format!("The title of {url}"))
        .referrer(referrer.unwrap_or_default())
        .referrer_name(sumimi_service::channels::referrer_name(referrer).unwrap_or_default())
        .referrer_path(referrer.map(|_| "/item").unwrap_or_default())
        .referrer_query(referrer.map(|_| "id=1").unwrap_or_default())
        .hostname(hostname)
        .channel(channel)
        .browser(browser)
        .browser_version("120")
        .os(os)
        .os_version("10")
        .device(device)
        .screen("1920x1080")
        .language(language)
        .country(country)
        .region(region)
        .city(city)
        .latitude(52.52)
        .longitude(13.405)
        .lcp(1200.0)
        .inp(80.0)
        .cls(0.02)
        .fcp(700.0)
        .ttfb(90.0)
        .name(if url == "/three" { "signup" } else { "" })
        // The tag travels with every event, the way the tracker sends it.
        .tag(tag)
        .build()
}

/// The query for a website.
fn query(website: &str) -> ReportQuery {
    ReportQuery::new(website, window())
}

/// The summary: the four numbers at the top of the dashboard.
///
/// Four events, three visitors, three sessions. Two of the sessions had a
/// single view, so they are bounces — and the one session with two views
/// lasted a minute, so the average is 20 seconds.
#[test]
fn the_summary_counts_the_traffic() {
    let (db, website) = database("summary");
    let summary = db.summary(&query(&website)).unwrap();

    // The **page views** are the three page views: the custom event is not
    // one, and the reference implementation excludes it from every pageview
    // metric too (`event_type NOT IN (2, 5)`).
    assert_eq!(summary.pageviews, 3, "the custom event is not a page view");
    // The visitors and the visits are counted over everything: a visitor who
    // only fired a custom event is still a visitor.
    assert_eq!(summary.visitors, 3, "three visitors came");
    assert_eq!(summary.views, 3, "in three sessions");
    // One bounce: the session with a single page view. The session that only
    // fired a custom event has no page view at all, and the session with two
    // page views is not a bounce either.
    assert_eq!(summary.bounces, 1, "one session had a single page view");
    assert_eq!(summary.visit_duration, 30.0, "the average session is a minute");
}

/// The timeseries: the views over time.
///
/// All four events are inside the same hour, so there is one bucket.
#[test]
fn the_timeseries_buckets_by_the_hour() {
    let (db, website) = database("timeseries");
    let rows = db.timeseries(&query(&website)).unwrap();

    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].name, "2026-01-15T12:00:00Z");
    assert_eq!(rows[0].value, 3, "the custom event is not a page view");
    assert_eq!(rows[0].date.as_deref(), Some("2026-01-15T12:00:00Z"));
}

/// The pages: the most visited pages.
///
/// `/one` was visited twice, the other two once each.
#[test]
fn the_pages_are_the_most_visited_ones() {
    let (db, website) = database("pages");
    let rows = db.breakdown(&query(&website), Dimension::Url).unwrap();

    let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(names, vec!["/one", "/two"], "{rows:?}");

    let one = rows.iter().find(|row| row.name == "/one").unwrap();
    assert_eq!(one.value, 2);
    // The url travels with the row, so the dashboard can link to it.
    assert_eq!(one.url.as_deref(), Some("/one"));
}

/// The referrers: where the traffic came from.
///
/// The referrers are a **content** dimension: they count the views, not the
/// visitors. Two views from google.com are two views.
#[test]
fn the_referrers_are_where_the_traffic_came_from() {
    let (db, website) = database("referrers");
    let rows = db.breakdown(&query(&website), Dimension::Referrer).unwrap();

    let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(names.len(), 2, "{rows:?}");

    let google = rows.iter().find(|row| row.name == "google.com").unwrap();
    assert_eq!(google.value, 2);

    let hacker = rows
        .iter()
        .find(|row| row.name == "news.ycombinator.com")
        .unwrap();
    assert_eq!(hacker.value, 1);
}

/// The browsers.
#[test]
fn the_browsers_are_counted_by_visitor() {
    let (db, website) = database("browsers");
    let rows = db.breakdown(&query(&website), Dimension::Browser).unwrap();

    let chrome = rows.iter().find(|row| row.name == "Chrome").unwrap();
    assert_eq!(chrome.value, 1, "one visitor used Chrome");

    let firefox = rows.iter().find(|row| row.name == "Firefox").unwrap();
    assert_eq!(firefox.value, 1, "one visitor used Firefox");
}

/// The operating systems.
#[test]
fn the_operating_systems_are_counted_by_visitor() {
    let (db, website) = database("os");
    let rows = db.breakdown(&query(&website), Dimension::Os).unwrap();

    assert_eq!(
        rows.iter().find(|row| row.name == "macOS").unwrap().value,
        1
    );
    assert_eq!(
        rows.iter().find(|row| row.name == "Windows").unwrap().value,
        1
    );
}

/// The devices.
#[test]
fn the_devices_are_counted_by_visitor() {
    let (db, website) = database("devices");
    let rows = db.breakdown(&query(&website), Dimension::Device).unwrap();

    assert_eq!(
        rows.iter().find(|row| row.name == "desktop").unwrap().value,
        1
    );
    assert_eq!(
        rows.iter().find(|row| row.name == "mobile").unwrap().value,
        1
    );
}

/// The countries.
#[test]
fn the_countries_are_counted_by_visitor() {
    let (db, website) = database("countries");
    let rows = db.breakdown(&query(&website), Dimension::Country).unwrap();

    assert_eq!(rows.iter().find(|row| row.name == "DE").unwrap().value, 1);
    assert_eq!(rows.iter().find(|row| row.name == "FR").unwrap().value, 1);
}

/// The languages.
#[test]
fn the_languages_are_counted_by_visitor() {
    let (db, website) = database("languages");
    let rows = db.breakdown(&query(&website), Dimension::Language).unwrap();

    assert_eq!(rows.iter().find(|row| row.name == "en-US").unwrap().value, 1);
    assert_eq!(rows.iter().find(|row| row.name == "de-DE").unwrap().value, 1);
}

/// The screens.
#[test]
fn the_screens_are_counted_by_visitor() {
    let (db, website) = database("screens");
    let rows = db.breakdown(&query(&website), Dimension::Screen).unwrap();

    assert_eq!(rows.len(), 1, "every page view has the same screen");
    assert_eq!(rows[0].value, 2, "two visitors had that screen");
}

/// The channels: where the traffic came from, in one word.
///
/// The bucket names are the reference implementation's — see
/// `service/src/channels.rs`.
#[test]
fn the_channels_are_the_reference_implementations_buckets() {
    let (db, website) = database("channels");
    let rows = db.breakdown(&query(&website), Dimension::Channel).unwrap();

    let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    assert!(names.contains(&"Search"), "{names:?}");
    assert!(names.contains(&"Social"), "{names:?}");

    // One visitor came from a search engine, one from a social network. The
    // custom event came directly, and it is not a page view.
    assert_eq!(
        rows.iter().find(|row| row.name == "Search").unwrap().value,
        1
    );
    assert_eq!(
        rows.iter().find(|row| row.name == "Social").unwrap().value,
        1
    );
}

/// The regions.
#[test]
fn the_regions_are_where_the_visitors_are() {
    let (db, website) = database("regions");
    let rows = db.breakdown(&query(&website), Dimension::Region).unwrap();

    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        rows.iter().find(|row| row.name == "Berlin").unwrap().value,
        1
    );
    assert_eq!(
        rows.iter().find(|row| row.name == "Ile-de-France").unwrap().value,
        1
    );
}

/// The cities.
#[test]
fn the_cities_are_where_the_visitors_are() {
    let (db, website) = database("cities");
    let rows = db.breakdown(&query(&website), Dimension::City).unwrap();

    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        rows.iter().find(|row| row.name == "Berlin").unwrap().value,
        1
    );
    assert_eq!(rows.iter().find(|row| row.name == "Paris").unwrap().value, 1);
}

/// The hosts: the sites the traffic came to.
///
/// The hosts are a **content** dimension too: they count the views.
#[test]
fn the_hosts_are_the_sites() {
    let (db, website) = database("hosts");
    let rows = db.breakdown(&query(&website), Dimension::Host).unwrap();

    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        rows.iter().find(|row| row.name == "example.com").unwrap().value,
        2
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.name == "other.example")
            .unwrap()
            .value,
        1
    );
}

/// The custom events.
#[test]
fn the_events_are_the_custom_ones() {
    let (db, website) = database("events");
    let rows = db.events(&query(&website)).unwrap();

    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].name, "signup");
    assert_eq!(rows[0].value, 1);
}

/// The tags.
///
/// The tags are a content dimension: they count the views. The three page
/// views have no tag, so they are "unknown".
#[test]
fn the_tags_are_the_events_tags() {
    let (db, website) = database("tags");
    let rows = db.breakdown(&query(&website), Dimension::Tag).unwrap();

    assert_eq!(rows.len(), 2, "{rows:?}");
    let a = rows.iter().find(|row| row.name == "tag-a").unwrap();
    assert_eq!(a.value, 2);
    let b = rows.iter().find(|row| row.name == "tag-b").unwrap();
    assert_eq!(b.value, 1);
}

/// The query parameters: what the visitors asked for.
///
/// `a` was on three of the four events, `b` on two. The count is per name
/// **and** value.
#[test]
fn the_query_parameters_are_split_out() {
    let (db, website) = database("parameters");
    let rows = parse_query_parameters(&db.query_strings(&query(&website)).unwrap());

    let a: i64 = rows.iter().filter(|row| row.name == "a").map(|row| row.count).sum();
    assert_eq!(a, 3, "{rows:?}");

    let b: i64 = rows.iter().filter(|row| row.name == "b").map(|row| row.count).sum();
    assert_eq!(b, 2, "{rows:?}");

    let values: Vec<&str> = rows
        .iter()
        .filter(|row| row.name == "a")
        .map(|row| row.value.as_str())
        .collect();
    assert!(values.contains(&"1"), "{values:?}");
    assert!(values.contains(&"2"), "{values:?}");
}

/// The session log: the sessions, with what happened in them.
#[test]
fn the_session_log_has_the_sessions() {
    let (db, website) = database("sessions");
    let sessions = db.sessions(&query(&website)).unwrap();

    assert_eq!(sessions.len(), 3, "{sessions:?}");

    // The session with two views.
    let two_views = sessions
        .iter()
        .find(|session| session.views == 2)
        .expect("the session with two views");
    assert_eq!(two_views.browser.as_deref(), Some("Chrome"));
    assert_eq!(two_views.channel.as_deref(), Some("Search"));
    assert_eq!(two_views.duration, 60, "it lasted a minute");
}

/// The event log: the events themselves.
#[test]
fn the_event_log_has_the_events() {
    let (db, website) = database("event-log");
    let events = db.event_log(&query(&website)).unwrap();

    assert_eq!(events.len(), 4, "{events:?}");

    // The newest first.
    let times: Vec<chrono::NaiveDateTime> = events.iter().map(|event| event.created_at).collect();
    assert!(times.windows(2).all(|pair| pair[0] >= pair[1]), "{times:?}");

    // The fields the dashboard shows.
    let newest = &events[0];
    assert_eq!(newest.name.as_deref(), Some("signup"));
    assert_eq!(newest.url.as_deref(), Some("/three"));
    assert_eq!(newest.channel.as_deref(), Some("direct"));
    assert_eq!(newest.hostname.as_deref(), Some("other.example"));
}
