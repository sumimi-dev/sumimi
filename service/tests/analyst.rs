//! The assistant: what it understands, and what it answers.
//!
//! The answers are checked against numbers that were put into the database by
//! hand — not against the numbers the assistant computed for itself.

use sumimi_service::analyst::{self, Lang, Metric};
use sumimi_service::models::Hit;
use sumimi_service::pool::Db;
use sumimi_service::storage::{Dimension, Storage};
use sumimi_service::testing::HitBuilder;

/// A unique temporary database path.
fn temp_db(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-analyst-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("sumimi.duckdb")
}

/// A fixed "now", so the windows are the same on every run.
fn now() -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp(1_767_225_600, 0)
        .map(|d| d.naive_utc())
        .unwrap_or_default()
}

/// A hit, with the fields the tests care about.
fn hit(website: &str, session: &str, visitor: &str, url: &str, days_ago: i64) -> Hit {
    HitBuilder::new(website, session, visitor)
        .at(now() - chrono::Duration::days(days_ago))
        .url(url)
        .referrer("news.ycombinator.com")
        .browser("Chrome")
        .os("macOS")
        .device("desktop")
        .country("DE")
        .language("en-US")
        .screen("1920x1080")
        .build()
}

/// A database with a website and a known amount of traffic.
fn database(name: &str) -> (Db, String) {
    let path = temp_db(name);
    let db = Db::open(&path, "256MB").unwrap();
    let website = db
        .create_website("Example", Some("example.com"), 0, None)
        .unwrap()
        .id;

    // Two sessions in the last week: one with two page views, one with one.
    let hits = vec![
        hit(&website, "s1", "v1", "/one", 1),
        hit(&website, "s1", "v1", "/two", 1),
        hit(&website, "s2", "v2", "/one", 2),
        // And one in the week before, so there is something to compare with.
        hit(&website, "s3", "v3", "/one", 10),
    ];
    db.write_batch(&hits).unwrap();

    (db, website)
}

// --------------------------------------------------------------------------------------
// Reading the question
// --------------------------------------------------------------------------------------

/// The window comes from the question.
#[test]
fn the_window_is_read_from_the_question() {
    let now = now();

    let intent = analyst::parse("how many visitors last 30 days", now);
    assert_eq!(
        (intent.range.end - intent.range.start).num_days(),
        30,
        "the question asked for 30 days"
    );

    let intent = analyst::parse("how many visitors past 7 days", now);
    assert_eq!((intent.range.end - intent.range.start).num_days(), 7);

    // No window in the question: the last seven days.
    let intent = analyst::parse("how many visitors", now);
    assert_eq!((intent.range.end - intent.range.start).num_days(), 7);

    // The window ends now.
    let intent = analyst::parse("visitors last 30 days", now);
    assert_eq!(intent.range.end, now);
}

/// "This week" is this week — from Monday until now.
///
/// It used to be the week **before** this one: the window ended at the start
/// of the week instead of at now, so a question about this week answered with
/// the numbers of the week before it.
#[test]
fn this_week_is_this_week() {
    // The fixed "now" is a Thursday.
    let now = now();
    let monday = chrono::NaiveDate::from_ymd_opt(2025, 12, 29)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();

    let intent = analyst::parse("how many page views this week", now);
    assert_eq!(intent.range.start, monday);
    assert_eq!(intent.range.end, now);

    // The answer holds the page views of this week — the three of them, not
    // the one from ten days ago.
    let (db, website) = database("this-week");
    let answer = analyst::ask(&db, &website, "how many page views this week", now).unwrap();
    assert_eq!(
        answer.facts.summary.as_ref().unwrap().pageviews.value,
        3,
        "{}",
        answer.text
    );
}

/// "Last week" is the week before this one.
#[test]
fn last_week_is_the_week_before() {
    let now = now();
    let monday = chrono::NaiveDate::from_ymd_opt(2025, 12, 29)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let last_monday = chrono::NaiveDate::from_ymd_opt(2025, 12, 22)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();

    let intent = analyst::parse("how many page views last week", now);
    assert_eq!(intent.range.start, last_monday);
    assert_eq!(intent.range.end, monday);
}

/// "This month" is this month, and "last month" is the calendar month before it.
#[test]
fn the_months_are_the_months() {
    let now = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    let january = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let december = chrono::NaiveDate::from_ymd_opt(2025, 12, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();

    let intent = analyst::parse("how many page views this month", now);
    assert_eq!(intent.range.start, january);
    assert_eq!(intent.range.end, now);

    let intent = analyst::parse("how many page views last month", now);
    assert_eq!(intent.range.start, december);
    assert_eq!(intent.range.end, january);
}

/// The metric comes from the question.
#[test]
fn the_metric_is_read_from_the_question() {
    let now = now();

    assert_eq!(analyst::parse("visitors", now).metric, Metric::Visitors);
    assert_eq!(analyst::parse("page views", now).metric, Metric::Pageviews);
    assert_eq!(analyst::parse("sessions", now).metric, Metric::Sessions);
    assert_eq!(analyst::parse("bounce rate", now).metric, Metric::Bounces);
    assert_eq!(
        analyst::parse("average visit duration", now).metric,
        Metric::Duration
    );
}

/// The dimension comes from the question.
#[test]
fn the_dimension_is_read_from_the_question() {
    let now = now();

    assert_eq!(
        analyst::parse("top pages", now).dimension,
        Some(Dimension::Url)
    );
    assert_eq!(
        analyst::parse("where do my visitors come from", now).dimension,
        Some(Dimension::Referrer)
    );
    assert_eq!(
        analyst::parse("which browsers", now).dimension,
        Some(Dimension::Browser)
    );
    assert_eq!(
        analyst::parse("what devices", now).dimension,
        Some(Dimension::Device)
    );
    assert_eq!(
        analyst::parse("which countries", now).dimension,
        Some(Dimension::Country)
    );
}

/// A question that compares with the period before is recognised.
#[test]
fn a_comparison_is_recognised() {
    let now = now();

    assert!(analyst::parse("how many visitors vs last week", now).compare);
    assert!(analyst::parse("why did traffic drop", now).compare);
    assert!(analyst::parse("why did traffic drop", now).why);
    assert!(!analyst::parse("how many visitors", now).compare);
}

/// A question in Chinese is understood, and answered in Chinese.
#[test]
fn a_chinese_question_is_understood() {
    let now = now();

    let intent = analyst::parse("最近 30 天的访客数", now);
    assert_eq!(intent.lang, Lang::Zh);
    assert_eq!(intent.metric, Metric::Visitors);
    assert_eq!((intent.range.end - intent.range.start).num_days(), 30);

    let intent = analyst::parse("访问量最多的页面", now);
    assert_eq!(intent.dimension, Some(Dimension::Url));

    let intent = analyst::parse("为什么流量下降了", now);
    assert!(intent.why);
    assert!(intent.compare);
}

// --------------------------------------------------------------------------------------
// Answering
// --------------------------------------------------------------------------------------

/// The answer holds the numbers that are in the database.
#[test]
fn the_answer_holds_the_numbers_of_the_database() {
    let (db, website) = database("numbers");
    let answer = analyst::ask(&db, &website, "how many page views", now()).unwrap();

    let summary = answer.facts.summary.as_ref().unwrap();
    assert_eq!(summary.pageviews.value, 3, "three page views in the window");
    assert_eq!(summary.visitors.value, 2, "two visitors");
    assert_eq!(summary.views.value, 2, "two sessions");
    assert_eq!(summary.bounces.value, 1, "one session with a single view");

    // The words hold the same numbers.
    assert!(
        answer.text.contains("3"),
        "the answer should hold the page views: {}",
        answer.text
    );
}

/// A question about a breakdown answers with the breakdown.
#[test]
fn a_breakdown_question_answers_with_the_breakdown() {
    let (db, website) = database("breakdown");
    let answer = analyst::ask(&db, &website, "top pages", now()).unwrap();

    let rows = answer.facts.breakdown.as_ref().unwrap();
    assert_eq!(rows.len(), 2, "two pages were visited");
    // The most visited page first.
    assert_eq!(rows[0].name, "/one");
    assert_eq!(rows[0].value, 2);
    assert_eq!(rows[1].name, "/two");
    assert_eq!(rows[1].value, 1);

    assert!(answer.text.contains("/one"), "{}", answer.text);
}

/// A question about a comparison answers with the change.
#[test]
fn a_comparison_question_answers_with_the_change() {
    let (db, website) = database("comparison");
    let answer = analyst::ask(&db, &website, "how many page views vs last week", now()).unwrap();

    // The last week has three page views, the week before had one.
    let summary = answer.facts.summary.as_ref().unwrap();
    assert_eq!(summary.pageviews.value, 3);
    assert!(
        summary.pageviews.change > 0.0,
        "the page views went up: {}",
        summary.pageviews.change
    );
    assert!(answer.text.contains('%'), "{}", answer.text);
}

/// A "why" question names the movers.
#[test]
fn a_why_question_names_the_movers() {
    let (db, website) = database("why");
    let answer = analyst::ask(&db, &website, "why did traffic drop", now()).unwrap();

    let movers = answer.facts.movers.as_ref().unwrap();
    assert!(!movers.is_empty(), "there should be movers");
    // The biggest change first.
    assert!(movers[0].change.abs() >= movers[movers.len() - 1].change.abs());
}

/// The assistant answers in the language of the question.
#[test]
fn the_answer_is_in_the_language_of_the_question() {
    let (db, website) = database("language");

    let english = analyst::ask(&db, &website, "how many page views", now()).unwrap();
    assert!(english.text.contains("page views"), "{}", english.text);

    let chinese = analyst::ask(&db, &website, "浏览量是多少", now()).unwrap();
    assert!(chinese.text.contains("浏览"), "{}", chinese.text);
}

/// The answer holds no number that is not in the database.
#[test]
fn the_answer_holds_no_invented_numbers() {
    let (db, website) = database("invented");
    let answer = analyst::ask(&db, &website, "how many page views", now()).unwrap();

    let summary = answer.facts.summary.as_ref().unwrap();
    let duration = summary.visit_duration.value;

    // The numbers the answer is allowed to hold: the five metrics, the parts of
    // the duration, and the length of the period.
    let allowed = [
        summary.pageviews.value.to_string(),
        summary.visitors.value.to_string(),
        summary.views.value.to_string(),
        summary.bounces.value.to_string(),
        duration.to_string(),
        (duration / 60).to_string(),
        (duration % 60).to_string(),
        ((answer.facts.end - answer.facts.start).num_days()).to_string(),
    ];

    for number in numbers(&answer.text) {
        assert!(
            allowed.contains(&number),
            "the answer holds a number that is not in the facts: {number} — {}",
            answer.text
        );
    }
}

/// The numbers in a piece of text.
fn numbers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if character.is_ascii_digit() {
            current.push(character);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// "page views" is a metric, not a question about the pages.
#[test]
fn the_metric_is_not_mistaken_for_a_dimension() {
    let now = now();

    // "how many page views" asks for a number, not for the breakdown.
    let intent = analyst::parse("how many page views last 7 days", now);
    assert_eq!(intent.metric, Metric::Pageviews);
    assert_eq!(intent.dimension, None, "not a question about the pages");

    // "top pages" is a question about the pages.
    let intent = analyst::parse("top pages", now);
    assert_eq!(intent.dimension, Some(Dimension::Url));

    // The same in Chinese: "浏览量" is the metric, "页面" is the dimension.
    let intent = analyst::parse("最近 30 天的浏览量", now);
    assert_eq!(intent.metric, Metric::Pageviews);
    assert_eq!(intent.dimension, None);
}

/// One is one.
#[test]
fn one_is_not_plural() {
    // A database with exactly one page view.
    let path = temp_db("plural");
    let db = Db::open(&path, "256MB").unwrap();
    let website = db
        .create_website("Example", Some("example.com"), 0, None)
        .unwrap()
        .id;
    db.write_batch(&[hit(&website, "s1", "v1", "/one", 1)])
        .unwrap();

    let answer = analyst::ask(&db, &website, "how many page views", now()).unwrap();
    assert!(
        answer.text.contains("1 page view"),
        "the answer should be singular: {}",
        answer.text
    );
    assert!(
        !answer.text.contains("1 page views"),
        "the answer should be singular: {}",
        answer.text
    );
}

// --------------------------------------------------------------------------------------
// The Chinese questions
// --------------------------------------------------------------------------------------

/// The window is read from the Chinese question, too.
///
/// The Chinese words are not a translation of the English ones — they are a
/// different set of words, and the windows they ask for have to be right.
#[test]
fn the_chinese_window_is_read_from_the_question() {
    let now = now();

    // Today and yesterday.
    let intent = analyst::parse("今天的访客数", now);
    assert_eq!(intent.range.end - intent.range.start, chrono::Duration::days(1));
    assert_eq!(intent.range.end, now);

    let intent = analyst::parse("昨天的访客数", now);
    assert_eq!(intent.range.end - intent.range.start, chrono::Duration::days(1));
    assert_eq!(intent.range.end, now - chrono::Duration::days(1));

    // Last week: the seven days before this one.
    let monday = chrono::NaiveDate::from_ymd_opt(2025, 12, 29)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let intent = analyst::parse("上周的访客数", now);
    assert_eq!(intent.range.start, monday - chrono::Duration::days(7));
    assert_eq!(intent.range.end, monday);

    // The last N days.
    let intent = analyst::parse("最近 30 天的访客数", now);
    assert_eq!(intent.range.end - intent.range.start, chrono::Duration::days(30));
    assert_eq!(intent.range.end, now);

    // No window in the question: the last seven days.
    let intent = analyst::parse("访客数", now);
    assert_eq!(intent.range.end - intent.range.start, chrono::Duration::days(7));
}

/// The Chinese months ask for the same windows the English ones ask for.
///
/// 「本月」 is the days from the first of the month until now; 「上个月」 is the
/// calendar month before it. (Both used to ask for the thirty days before this
/// month, in both cases — that was the bug this test was pinning.)
#[test]
fn the_chinese_months_answer_the_same_windows_as_the_english_ones() {
    let now = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    let january = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let december = chrono::NaiveDate::from_ymd_opt(2025, 12, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();

    for question in ["本月的访客数", "这个月的访客数"] {
        let intent = analyst::parse(question, now);
        assert_eq!(intent.range.start, january, "{question}: from the first");
        assert_eq!(intent.range.end, now, "{question}: until now");
    }

    for question in ["上个月的访客数", "上月的访客数"] {
        let intent = analyst::parse(question, now);
        assert_eq!(intent.range.start, december, "{question}: the month before");
        assert_eq!(intent.range.end, january, "{question}: up to this month");
    }
}

/// The Chinese metric words.
#[test]
fn the_chinese_metric_is_read_from_the_question() {
    let now = now();

    assert_eq!(analyst::parse("跳出数", now).metric, Metric::Bounces);
    assert_eq!(analyst::parse("平均停留时长", now).metric, Metric::Duration);
    assert_eq!(analyst::parse("访客数", now).metric, Metric::Visitors);
    assert_eq!(analyst::parse("会话数", now).metric, Metric::Sessions);
    assert_eq!(analyst::parse("浏览量", now).metric, Metric::Pageviews);
    assert_eq!(analyst::parse("流量", now).metric, Metric::Pageviews);
}

/// The Chinese dimension words.
#[test]
fn the_chinese_dimension_is_read_from_the_question() {
    let now = now();

    assert_eq!(
        analyst::parse("访问来源", now).dimension,
        Some(Dimension::Referrer)
    );
    assert_eq!(
        analyst::parse("哪些浏览器", now).dimension,
        Some(Dimension::Browser)
    );
    assert_eq!(
        analyst::parse("什么设备", now).dimension,
        Some(Dimension::Device)
    );
    assert_eq!(
        analyst::parse("哪些国家", now).dimension,
        Some(Dimension::Country)
    );
    assert_eq!(
        analyst::parse("哪些城市", now).dimension,
        Some(Dimension::City)
    );
    assert_eq!(
        analyst::parse("什么屏幕", now).dimension,
        Some(Dimension::Screen)
    );
    assert_eq!(
        analyst::parse("访问量最多的页面", now).dimension,
        Some(Dimension::Url)
    );
}

/// The comparison is cut off, and the window is read from what is left.
///
/// "最近 30 天的访客数和上周相比" is a question about **these** 30 days — the
/// words after the comparison belong to the other period.
#[test]
fn the_chinese_comparison_is_cut_off() {
    let now = now();

    let intent = analyst::parse("最近 30 天的访客数和上周相比", now);
    assert!(intent.compare, "it compares with the period before");
    assert_eq!(
        intent.range.end - intent.range.start,
        chrono::Duration::days(30),
        "the window comes from the part before the comparison"
    );
    assert_eq!(intent.metric, Metric::Visitors);
}

/// The Chinese comparison words are recognised.
///
/// "较上" is **not** one of them — the question is cut at it, but the answer
/// carries no change. The list of comparison words and the list of markers
/// disagree, and that is a bug to fix on purpose; the test pins what the
/// assistant does today.
#[test]
fn the_chinese_comparison_is_recognised() {
    let now = now();

    for question in ["流量相比上周", "流量对比上周", "流量比上周", "流量环比", "流量同比"] {
        assert!(
            analyst::parse(question, now).compare,
            "{question} compares with the period before"
        );
    }

    assert!(
        !analyst::parse("流量较上周", now).compare,
        "较上 is cut off but not recognised as a comparison"
    );
    assert!(!analyst::parse("访客数", now).compare);
}
