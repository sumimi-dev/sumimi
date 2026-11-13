//! The assistant: ask a question, get an answer.
//!
//! The answer is **computed, not written**. The question is turned into a
//! report — the same report the dashboard asks for — the report is run through
//! the same code the API runs, and the answer is written from the numbers that
//! came back.
//!
//! That has three consequences worth having:
//!
//! * **the assistant cannot disagree with the dashboard.** There is one
//!   implementation of "the top pages" and one implementation of "page views",
//!   and the assistant uses both;
//! * **it works with no key, no network and no data leaving the machine.** The
//!   privacy promise does not get a hole in it for a chat window;
//! * **it cannot make up a number.** Every figure in the answer is a figure the
//!   database returned.
//!
//! What it is not: a language model. It reads a question, and it answers with
//! the numbers it found. A language model can be put in front of it (the
//! [`Facts`] are plain JSON, and a model can be asked to phrase them) — but the
//! model would only ever be rearranging what the database said.
//!
//! The questions it understands are the ones a site owner actually asks:
//!
//! | question | what it runs |
//! | --- | --- |
//! | "how many visitors last 30 days" | the summary |
//! | "top pages this week" | the pages breakdown |
//! | "where do my visitors come from" | the referrers breakdown |
//! | "why did traffic drop" | the summary of both periods, and the movers |

use chrono::{Datelike, NaiveDateTime};
use serde::Serialize;

use crate::error::ApiError;
use crate::models::{MetricsSummary, ReportKind};
use crate::storage::{Dimension, ReportQuery, Storage, TimeRange};

/// The language a question is asked in.
///
/// The assistant reads the question in one language or the other, and answers
/// in the same one. Every word list it knows comes in a pair — one per
/// language — and the language picks the pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Lang {
    /// English.
    En,
    /// Chinese.
    Zh,
}

impl Lang {
    /// The language a question is asked in.
    pub fn of(question: &str) -> Self {
        if question
            .chars()
            .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
        {
            Lang::Zh
        } else {
            Lang::En
        }
    }
}

/// The metric a question is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Metric {
    /// Total page views.
    Pageviews,
    /// Unique visitors.
    Visitors,
    /// Number of sessions.
    Sessions,
    /// Sessions with a single page view.
    Bounces,
    /// Average session length.
    Duration,
}

impl Metric {
    /// The value of this metric in a summary.
    pub fn value(self, summary: &MetricsSummary) -> i64 {
        match self {
            Metric::Pageviews => summary.pageviews.value,
            Metric::Visitors => summary.visitors.value,
            Metric::Sessions => summary.views.value,
            Metric::Bounces => summary.bounces.value,
            Metric::Duration => summary.visit_duration.value,
        }
    }

    /// The period-over-period change of this metric.
    pub fn change(self, summary: &MetricsSummary) -> f64 {
        match self {
            Metric::Pageviews => summary.pageviews.change,
            Metric::Visitors => summary.visitors.change,
            Metric::Sessions => summary.views.change,
            Metric::Bounces => summary.bounces.change,
            Metric::Duration => summary.visit_duration.change,
        }
    }

    /// The name of the metric, in the given language.
    pub fn name(self, lang: Lang) -> &'static str {
        match (self, lang) {
            (Metric::Pageviews, Lang::En) => "page views",
            (Metric::Visitors, Lang::En) => "visitors",
            (Metric::Sessions, Lang::En) => "sessions",
            (Metric::Bounces, Lang::En) => "bounces",
            (Metric::Duration, Lang::En) => "average visit",
            (Metric::Pageviews, Lang::Zh) => "浏览量",
            (Metric::Visitors, Lang::Zh) => "访客数",
            (Metric::Sessions, Lang::Zh) => "会话数",
            (Metric::Bounces, Lang::Zh) => "跳出数",
            (Metric::Duration, Lang::Zh) => "平均停留时长",
        }
    }

    /// The name of the metric, in the given language, for **one** of them.
    ///
    /// "1 page views" is the kind of thing that makes a person stop trusting
    /// the rest of the sentence. Chinese has no plural, so the two are the
    /// same there.
    pub fn name_one(self, lang: Lang) -> &'static str {
        match (self, lang) {
            (Metric::Pageviews, Lang::En) => "page view",
            (Metric::Visitors, Lang::En) => "visitor",
            (Metric::Sessions, Lang::En) => "session",
            (Metric::Bounces, Lang::En) => "bounce",
            (Metric::Duration, Lang::En) => "average visit",
            (Metric::Pageviews, Lang::Zh) => "浏览量",
            (Metric::Visitors, Lang::Zh) => "访客数",
            (Metric::Sessions, Lang::Zh) => "会话数",
            (Metric::Bounces, Lang::Zh) => "跳出数",
            (Metric::Duration, Lang::Zh) => "平均停留时长",
        }
    }
}

/// What a question is about.
#[derive(Debug, Clone, PartialEq)]
pub struct Intent {
    /// The window the question is about.
    pub range: TimeRange,
    /// The metric the question asks about.
    pub metric: Metric,
    /// The dimension the question breaks the traffic down by.
    pub dimension: Option<Dimension>,
    /// Whether the question compares with the period before.
    pub compare: bool,
    /// Whether the question asks *why* — which needs the movers, not just the
    /// numbers.
    pub why: bool,
    /// The language the question is in — and the answer is in.
    pub lang: Lang,
}

impl Default for Intent {
    fn default() -> Self {
        Self {
            range: TimeRange::last_days(7),
            metric: Metric::Pageviews,
            dimension: None,
            compare: false,
            why: false,
            lang: Lang::En,
        }
    }
}

/// One line of a breakdown.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// The dimension value.
    pub name: String,
    /// How much of it there was.
    pub value: i64,
}

/// A change between two periods.
#[derive(Debug, Clone, Serialize)]
pub struct Mover {
    /// The dimension value.
    pub name: String,
    /// The change in the count.
    pub change: i64,
}

/// What the assistant found out.
///
/// This is the whole answer, in numbers — it is what the wording is made of,
/// and it is what a language model would be given if one were put in front.
#[derive(Debug, Clone, Serialize)]
pub struct Facts {
    /// The question, as it was asked.
    pub question: String,
    /// The window the answer is about.
    pub start: NaiveDateTime,
    /// The end of the window.
    pub end: NaiveDateTime,
    /// The metric the question was about.
    pub metric: Metric,
    /// The summary of the window.
    pub summary: Option<MetricsSummary>,
    /// The breakdown, if the question asked for one.
    pub breakdown: Option<Vec<Row>>,
    /// The dimension the breakdown is over.
    pub dimension: Option<String>,
    /// The biggest movers between this period and the one before it.
    pub movers: Option<Vec<Mover>>,
}

/// The answer: the words, and the numbers they are made of.
#[derive(Debug, Clone, Serialize)]
pub struct Answer {
    /// The answer, in words.
    pub text: String,
    /// The numbers behind it.
    pub facts: Facts,
    /// The report that was run, so the dashboard can show it too.
    pub report: ReportKind,
}

/// Ask a question about a website.
///
/// The question is parsed, the matching report is run, and the answer is
/// written from what came back.
pub fn ask(
    storage: &dyn Storage,
    website: &str,
    question: &str,
    now: NaiveDateTime,
) -> Result<Answer, ApiError> {
    let intent = parse(question, now);
    let query = ReportQuery::new(website, intent.range);

    // The summary is always worth having: it is the answer to most questions,
    // and it is the context of the others. It carries the change against the
    // period before, so the comparison is the same one the dashboard shows.
    let summary = storage.summary_with_change(&query)?;

    let breakdown = match intent.dimension {
        Some(dimension) => Some(
            storage
                .breakdown(&query, dimension)?
                .into_iter()
                .map(|row| Row {
                    name: row.name,
                    value: row.value,
                })
                .collect(),
        ),
        None => None,
    };

    // "Why" needs the period before as well, and the movers between them.
    let movers = if intent.why || intent.compare {
        let dimension = intent.dimension.unwrap_or(Dimension::Url);
        let previous = ReportQuery::new(website, intent.range.previous());
        let current = storage.breakdown(&query, dimension)?;
        let before = storage.breakdown(&previous, dimension)?;
        Some(movers(&current, &before))
    } else {
        None
    };

    let facts = Facts {
        question: question.to_string(),
        start: intent.range.start,
        end: intent.range.end,
        metric: intent.metric,
        summary: Some(summary),
        breakdown,
        dimension: intent.dimension.map(|dimension| dimension_name(dimension).to_string()),
        movers,
    };

    let report = intent.dimension.map(report_for).unwrap_or(ReportKind::Summary);

    Ok(Answer {
        text: compose(&facts, &intent),
        facts,
        report,
    })
}

/// The biggest changes between two periods.
fn movers(
    current: &[crate::models::BreakdownRow],
    before: &[crate::models::BreakdownRow],
) -> Vec<Mover> {
    let before_values: std::collections::HashMap<&str, i64> = before
        .iter()
        .map(|row| (row.name.as_str(), row.value))
        .collect();

    let mut out: Vec<Mover> = current
        .iter()
        .map(|row| Mover {
            name: row.name.clone(),
            change: row.value - before_values.get(row.name.as_str()).copied().unwrap_or(0),
        })
        .collect();

    // The biggest change first, whatever its direction.
    out.sort_by(|a, b| {
        b.change
            .abs()
            .cmp(&a.change.abs())
            .then_with(|| a.name.cmp(&b.name))
    });
    out.truncate(5);
    out
}

/// The report that answers a question about a dimension.
fn report_for(dimension: Dimension) -> ReportKind {
    match dimension {
        Dimension::Url => ReportKind::Pages,
        Dimension::Referrer => ReportKind::Referrers,
        Dimension::Browser => ReportKind::Browsers,
        Dimension::Os => ReportKind::Os,
        Dimension::Device => ReportKind::Devices,
        Dimension::Country => ReportKind::Countries,
        Dimension::Language => ReportKind::Languages,
        Dimension::Screen => ReportKind::Screens,
        Dimension::Channel => ReportKind::Channels,
        Dimension::Region => ReportKind::Regions,
        Dimension::City => ReportKind::Cities,
        Dimension::Host => ReportKind::Hosts,
        Dimension::Tag => ReportKind::Tags,
    }
}

/// The name of a dimension, in English.
fn dimension_name(dimension: Dimension) -> &'static str {
    match dimension {
        Dimension::Url => "pages",
        Dimension::Referrer => "referrers",
        Dimension::Browser => "browsers",
        Dimension::Os => "operating systems",
        Dimension::Device => "devices",
        Dimension::Country => "countries",
        Dimension::Language => "languages",
        Dimension::Screen => "screens",
        Dimension::Channel => "channels",
        Dimension::Region => "regions",
        Dimension::City => "cities",
        Dimension::Host => "hosts",
        Dimension::Tag => "tags",
    }
}

// --------------------------------------------------------------------------------------
// The words the assistant knows
// --------------------------------------------------------------------------------------

/// The dimension each phrase asks for, in English.
///
/// The first table with a phrase in the question wins.
const EN_DIMENSIONS: &[(&[&str], Dimension)] = &[
    (
        &[
            "referrer",
            "referer",
            "traffic source",
            "where do",
            "where from",
            "source",
        ],
        Dimension::Referrer,
    ),
    (&["browser"], Dimension::Browser),
    (&["operating system", " os ", "os?"], Dimension::Os),
    (&["device"], Dimension::Device),
    (&["country", "countries"], Dimension::Country),
    (&["region", "state", "province"], Dimension::Region),
    (&["city", "cities"], Dimension::City),
    (&["channel"], Dimension::Channel),
    (&["hostname", "host name", "hosts"], Dimension::Host),
    (&["language"], Dimension::Language),
    (&["screen", "resolution"], Dimension::Screen),
    (&["event"], Dimension::Url),
    (&["page", "url", "path", "blog"], Dimension::Url),
];

/// The dimension each phrase asks for, in Chinese.
const ZH_DIMENSIONS: &[(&[&str], Dimension)] = &[
    (&["来源", "引荐", "referer", "referrer"], Dimension::Referrer),
    (&["浏览器"], Dimension::Browser),
    (&["系统", "操作系统"], Dimension::Os),
    (&["设备"], Dimension::Device),
    (&["国家"], Dimension::Country),
    (&["地区", "省份", "州"], Dimension::Region),
    (&["城市"], Dimension::City),
    (&["渠道", "来源渠道"], Dimension::Channel),
    (&["域名", "主机"], Dimension::Host),
    (&["语言"], Dimension::Language),
    (&["屏幕", "分辨率"], Dimension::Screen),
    (&["事件"], Dimension::Url),
    (&["页面", "网页", "路径"], Dimension::Url),
];

/// The metric each phrase asks for, in English.
const EN_METRICS: &[(&[&str], Metric)] = &[
    (&["bounce"], Metric::Bounces),
    (&["duration", "time on site", "how long"], Metric::Duration),
    (&["visitor"], Metric::Visitors),
    (&["session", "visit ", "visits"], Metric::Sessions),
];

/// The metric each phrase asks for, in Chinese.
const ZH_METRICS: &[(&[&str], Metric)] = &[
    (&["跳出"], Metric::Bounces),
    (&["停留", "时长"], Metric::Duration),
    (&["访客"], Metric::Visitors),
    (&["会话"], Metric::Sessions),
    (&["浏览", "访问量", "流量"], Metric::Pageviews),
];

/// The window each phrase asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// Today.
    Today,
    /// Yesterday.
    Yesterday,
    /// From Monday until now.
    ThisWeek,
    /// The seven days before this week.
    LastWeek,
    /// From the first of the month until now.
    ThisMonth,
    /// The calendar month before this one.
    LastMonth,
}

/// The window each phrase asks for, in English.
const EN_WINDOWS: &[(&[&str], Window)] = &[
    (&["today"], Window::Today),
    (&["yesterday"], Window::Yesterday),
    (&["this week"], Window::ThisWeek),
    (&["last week"], Window::LastWeek),
    (&["this month"], Window::ThisMonth),
    (&["last month"], Window::LastMonth),
];

/// The window each phrase asks for, in Chinese.
///
/// **The same windows the English ones ask for** — 「本月」 is the days from the
/// first of the month until now, 「上个月」 is the calendar month before it.
/// (They used to ask for the thirty days before this month, in both cases; that
/// was a bug, and the two languages did not agree.)
const ZH_WINDOWS: &[(&[&str], Window)] = &[
    (&["今天"], Window::Today),
    (&["昨天"], Window::Yesterday),
    (&["上周"], Window::LastWeek),
    (&["本月", "这个月"], Window::ThisMonth),
    (&["上个月", "上月"], Window::LastMonth),
];

/// The words that ask for a number of days, in English.
const EN_DAY_WORDS: &[&str] = &["last", "past", "previous"];

/// The words that ask for a number of days, in Chinese.
const ZH_DAY_WORDS: &[&str] = &["最近", "过去", "近"];

/// The words that say the question compares with the period before, in English.
const EN_COMPARE_WORDS: &[&str] = &[
    " vs ",
    "versus",
    "compare",
    "compared",
    "than before",
    "change",
    "trend",
    "drop",
    "dropped",
    "rise",
    "grew",
];

/// The words that say the question compares with the period before, in Chinese.
const ZH_COMPARE_WORDS: &[&str] = &[
    "变化", "对比", "相比", "比上", "环比", "同比", "下降", "上涨", "增长", "跌", "趋势",
];

/// The words that ask *why*, in English.
const EN_WHY_WORDS: &[&str] = &["why", "reason", "cause"];

/// The words that ask *why*, in Chinese.
const ZH_WHY_WORDS: &[&str] = &["为什么", "原因", "怎么回事", "咋回事"];

/// The words the comparison is cut at, in English.
const EN_COMPARISON_MARKERS: &[&str] = &[
    " vs ",
    " versus ",
    " compared to ",
    " compare to ",
    " against ",
];

/// The words the comparison is cut at, in Chinese.
const ZH_COMPARISON_MARKERS: &[&str] = &["相比", "对比", "比上", "较上", "环比", "同比"];

/// The words that start the other period's clause, in Chinese.
///
/// "和上周相比" — the clause starts at the 和, not at the 相比.
const ZH_CONJUNCTIONS: &[&str] = &["和", "与", "跟", "同"];

impl Lang {
    /// The dimension each phrase asks for.
    fn dimensions(self) -> &'static [(&'static [&'static str], Dimension)] {
        match self {
            Lang::En => EN_DIMENSIONS,
            Lang::Zh => ZH_DIMENSIONS,
        }
    }

    /// The metric each phrase asks for.
    fn metrics(self) -> &'static [(&'static [&'static str], Metric)] {
        match self {
            Lang::En => EN_METRICS,
            Lang::Zh => ZH_METRICS,
        }
    }

    /// The window each phrase asks for.
    fn windows(self) -> &'static [(&'static [&'static str], Window)] {
        match self {
            Lang::En => EN_WINDOWS,
            Lang::Zh => ZH_WINDOWS,
        }
    }

    /// The words that ask for a number of days.
    fn day_words(self) -> &'static [&'static str] {
        match self {
            Lang::En => EN_DAY_WORDS,
            Lang::Zh => ZH_DAY_WORDS,
        }
    }

    /// The words that say the question compares with the period before.
    fn compare_words(self) -> &'static [&'static str] {
        match self {
            Lang::En => EN_COMPARE_WORDS,
            Lang::Zh => ZH_COMPARE_WORDS,
        }
    }

    /// The words that ask *why*.
    fn why_words(self) -> &'static [&'static str] {
        match self {
            Lang::En => EN_WHY_WORDS,
            Lang::Zh => ZH_WHY_WORDS,
        }
    }

    /// The words the comparison is cut at.
    fn comparison_markers(self) -> &'static [&'static str] {
        match self {
            Lang::En => EN_COMPARISON_MARKERS,
            Lang::Zh => ZH_COMPARISON_MARKERS,
        }
    }

    /// The words that start the other period's clause.
    fn conjunctions(self) -> &'static [&'static str] {
        match self {
            Lang::En => &[],
            Lang::Zh => ZH_CONJUNCTIONS,
        }
    }
}

/// Does the question hold one of the words?
fn holds(text: &str, words: &[&str]) -> bool {
    words.iter().any(|word| text.contains(word))
}

// --------------------------------------------------------------------------------------
// Reading the question
// --------------------------------------------------------------------------------------

/// Read a question.
///
/// The rules are deliberately simple: a window, a metric, a dimension and
/// whether the question compares with the period before. Everything else is
/// ignored — a question the assistant does not understand still gets an answer
/// about the last seven days.
pub fn parse(question: &str, now: NaiveDateTime) -> Intent {
    let text = question.to_lowercase();
    let lang = Lang::of(question);

    let mut intent = Intent {
        lang,
        ..Intent::default()
    };

    intent.compare = compare(&text, lang);
    intent.why = why(&text, lang);

    // "how many visitors vs last week" asks about **this** week and compares it
    // with last week — so the words after the comparison belong to the other
    // period, and the window is read from what comes before.
    let head = strip_comparison(&text, lang);

    intent.range = window(&head, lang, now);
    intent.metric = metric(&head, lang);
    intent.dimension = dimension(&head, lang);

    intent
}

/// Cut the question at the comparison, so the window is read from the part that
/// talks about the period being asked about.
///
/// "how many visitors vs last week" → "how many visitors"
/// "最近 30 天的访客数和上周相比" → "最近 30 天的访客数"
fn strip_comparison(text: &str, lang: Lang) -> String {
    for marker in lang.comparison_markers() {
        let Some(position) = text.find(marker) else {
            continue;
        };
        let mut head = &text[..position];
        // "和上周相比" — the clause starts at the 和, not at the 相比.
        for conjunction in lang.conjunctions() {
            if let Some(at) = head.rfind(conjunction) {
                if head.len() - at <= 12 {
                    head = &head[..at];
                }
            }
        }
        return head.trim().to_string();
    }

    text.to_string()
}

/// The window the question is about.
fn window(text: &str, lang: Lang, now: NaiveDateTime) -> TimeRange {
    let day = chrono::Duration::days(1);

    let asked = lang
        .windows()
        .iter()
        .find(|(words, _)| holds(text, words))
        .map(|(_, window)| *window);

    match asked {
        Some(Window::Today) => TimeRange {
            start: now - day,
            end: now,
        },
        Some(Window::Yesterday) => TimeRange {
            start: now - day * 2,
            end: now - day,
        },
        Some(Window::ThisWeek) => TimeRange {
            // This week: from Monday until now.
            start: start_of_week(now),
            end: now,
        },
        Some(Window::LastWeek) => {
            // Last week: the seven days before this one.
            let end = start_of_week(now);
            TimeRange {
                start: end - day * 7,
                end,
            }
        }
        Some(Window::ThisMonth) => TimeRange {
            // This month: from the first until now.
            start: start_of_month(now),
            end: now,
        },
        Some(Window::LastMonth) => {
            // Last month: the calendar month before this one.
            let end = start_of_month(now);
            let previous = end.date().pred_opt().unwrap_or(end.date());
            let start = previous
                .with_day(1)
                .unwrap_or(previous)
                .and_hms_opt(0, 0, 0)
                .unwrap_or(end);
            TimeRange { start, end }
        }
        None => {
            // "last 30 days" — the number has to follow the word.
            if let Some(days) = days_after(text, lang.day_words()) {
                return TimeRange {
                    start: now - chrono::Duration::days(days),
                    end: now,
                };
            }
            TimeRange {
                start: now - day * 7,
                end: now,
            }
        }
    }
}

/// How many days the question asks about.
///
/// The number has to follow the keyword: "last 30 days", "past 7 days".
fn days_after(text: &str, keywords: &[&str]) -> Option<i64> {
    for keyword in keywords {
        let mut rest = text;
        while let Some(position) = rest.find(keyword) {
            let after = &rest[position + keyword.len()..];
            let digits: String = after
                .chars()
                .skip_while(|c| c.is_whitespace())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(days) = digits.parse::<i64>() {
                if days > 0 && days <= 3650 {
                    return Some(days);
                }
            }
            rest = &rest[position + keyword.len()..];
        }
    }
    None
}

/// The start of the week the day is in (Monday).
fn start_of_week(now: NaiveDateTime) -> NaiveDateTime {
    let weekday = now.weekday().num_days_from_monday() as i64;
    (now - chrono::Duration::days(weekday)).date().and_hms_opt(0, 0, 0).unwrap_or(now)
}

/// The start of the month the day is in.
fn start_of_month(now: NaiveDateTime) -> NaiveDateTime {
    let first = now.date().with_day(1).unwrap_or(now.date());
    first.and_hms_opt(0, 0, 0).unwrap_or(now)
}

/// The metric the question is about.
fn metric(text: &str, lang: Lang) -> Metric {
    lang.metrics()
        .iter()
        .find(|(words, _)| holds(text, words))
        .map(|(_, metric)| *metric)
        // No word about a metric: the question is about the page views.
        .unwrap_or(Metric::Pageviews)
}

/// The dimension the question breaks the traffic down by.
fn dimension(text: &str, lang: Lang) -> Option<Dimension> {
    // "page views" is a **metric**, not a dimension: "how many page views" is
    // not a question about the pages. The metric phrases are taken out before
    // the dimensions are looked for.
    let text = &text
        .replace("page views", " ")
        .replace("pageviews", " ")
        .replace("page view", " ");

    lang.dimensions()
        .iter()
        .find(|(words, _)| holds(text, words))
        .map(|(_, dimension)| *dimension)
}

/// Does the question compare with the period before?
fn compare(text: &str, lang: Lang) -> bool {
    holds(text, lang.compare_words())
}

/// Does the question ask *why*?
fn why(text: &str, lang: Lang) -> bool {
    holds(text, lang.why_words())
}

// --------------------------------------------------------------------------------------
// Writing the answer
// --------------------------------------------------------------------------------------

/// Write the answer from the facts.
///
/// The wording is the only place where the assistant sounds like a person; every
/// number in it comes from [`Facts`].
pub fn compose(facts: &Facts, intent: &Intent) -> String {
    let mut sentences: Vec<String> = Vec::new();
    sentences.extend(compose_summary(facts, intent));
    sentences.extend(compose_change(facts, intent));
    sentences.extend(compose_breakdown(facts, intent));
    sentences.extend(compose_movers(facts, intent));

    if sentences.is_empty() {
        return match intent.lang {
            Lang::Zh => "这段时间没有数据。".to_string(),
            Lang::En => "There is no data for that period.".to_string(),
        };
    }

    sentences.join(" ")
}

/// The headline: the metric that was asked about — and the rest of the summary.
fn compose_summary(facts: &Facts, intent: &Intent) -> Vec<String> {
    let Some(summary) = facts.summary.as_ref() else {
        return Vec::new();
    };
    let lang = intent.lang;
    let metric = facts.metric;
    let value = metric.value(summary);

    let mut out = vec![match lang {
        Lang::Zh => format!(
            "最近这段时间（{}）：{} {}。",
            period(facts, lang),
            number(value),
            metric.name(lang)
        ),
        Lang::En => format!(
            "Over the last {}: {} {}.",
            period(facts, lang),
            number(value),
            // One is one.
            if value == 1 {
                metric.name_one(lang)
            } else {
                metric.name(lang)
            }
        ),
    }];

    // The rest of the summary, when the question was not about one metric.
    if metric == Metric::Pageviews && intent.dimension.is_none() {
        out.push(match lang {
            Lang::Zh => format!(
                "{} 访客，{} 次会话，{} 次跳出，平均停留 {}。",
                number(summary.visitors.value),
                number(summary.views.value),
                number(summary.bounces.value),
                duration(summary.visit_duration.value)
            ),
            Lang::En => format!(
                "{} visitors, {} sessions, {} bounces, {} average visit.",
                number(summary.visitors.value),
                number(summary.views.value),
                number(summary.bounces.value),
                duration(summary.visit_duration.value)
            ),
        });
    }

    out
}

/// The change against the period before.
fn compose_change(facts: &Facts, intent: &Intent) -> Vec<String> {
    if !intent.compare {
        return Vec::new();
    }
    let Some(summary) = facts.summary.as_ref() else {
        return Vec::new();
    };
    let lang = intent.lang;
    let change = facts.metric.change(summary);

    vec![match lang {
        Lang::Zh => format!(
            "和上一个{}相比，{} {} {:+.1}%。",
            period(facts, lang),
            facts.metric.name(lang),
            if change >= 0.0 { "上涨了" } else { "下降了" },
            change.abs()
        ),
        Lang::En => format!(
            "That is {:+.1}% against the previous {}.",
            change,
            period(facts, lang)
        ),
    }]
}

/// The breakdown: the top rows of the dimension the question asked about.
fn compose_breakdown(facts: &Facts, intent: &Intent) -> Vec<String> {
    let Some(rows) = facts.breakdown.as_ref() else {
        return Vec::new();
    };
    if rows.is_empty() {
        return Vec::new();
    }
    let dimension = facts.dimension.as_deref().unwrap_or("pages");
    let list: Vec<String> = rows
        .iter()
        .take(5)
        .map(|row| format!("{} ({})", row.name, number(row.value)))
        .collect();

    vec![match intent.lang {
        Lang::Zh => format!("最多的{}：{}。", dimension, list.join("、")),
        Lang::En => format!("The top {}: {}.", dimension, list.join(", ")),
    }]
}

/// The movers: the answer to "why".
fn compose_movers(facts: &Facts, intent: &Intent) -> Vec<String> {
    let Some(movers) = facts.movers.as_ref() else {
        return Vec::new();
    };
    let changed: Vec<&Mover> = movers.iter().filter(|m| m.change != 0).collect();
    if changed.is_empty() {
        return Vec::new();
    }
    let list: Vec<String> = changed
        .iter()
        .take(3)
        .map(|mover| format!("{} ({:+})", mover.name, number(mover.change)))
        .collect();

    vec![match intent.lang {
        Lang::Zh => format!("变化最大的：{}。", list.join("、")),
        Lang::En => format!("The biggest movers: {}.", list.join(", ")),
    }]
}

/// The period, in words.
fn period(facts: &Facts, lang: Lang) -> String {
    let days = (facts.end - facts.start).num_days().max(1);
    match lang {
        Lang::Zh => format!("{days} 天"),
        Lang::En => format!("{days} days"),
    }
}

/// A number with the thousand separators a person expects.
fn number(value: i64) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(character);
    }
    if negative {
        format!("-{out}")
    } else {
        out
    }
}

/// A duration in seconds, in words.
fn duration(seconds: i64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    let rest = seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {rest}s");
    }
    format!("{}h {}m", minutes / 60, minutes % 60)
}
