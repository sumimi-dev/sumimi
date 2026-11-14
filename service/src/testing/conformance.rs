//! The conformance suite: the same checks, against any backend.
//!
//! Run it against the open-source DuckDB backend and against the pro backends, and
//! they all have to produce the same numbers — the numbers the dataset says they
//! should produce.

use anyhow::{bail, Result};
use crate::models::Hit;
use crate::storage::{Dimension, Storage};

use super::dataset::{query_for, Dataset, WindowExpectation};
use super::deep::DeepRead;

/// What went wrong, if something did.
#[derive(Debug, Default)]
pub struct Failures {
    /// The failures, one per line.
    pub problems: Vec<String>,
}

impl Failures {
    /// Record a failure.
    fn fail(&mut self, message: String) {
        self.problems.push(message);
    }

    /// Whether anything failed.
    pub fn is_empty(&self) -> bool {
        self.problems.is_empty()
    }

    /// Turn the failures into an error.
    pub fn into_result(self) -> Result<()> {
        if self.problems.is_empty() {
            return Ok(());
        }
        bail!(
            "{} check(s) failed:\n  - {}",
            self.problems.len(),
            self.problems.join("\n  - ")
        )
    }
}

/// Run the whole suite against one backend.
///
/// The dataset is written first, then every report is checked against the numbers
/// the dataset computed for itself.
pub fn run(storage: &dyn Storage, dataset: &Dataset) -> Result<()> {
    let mut failures = Failures::default();

    write_rows(storage, dataset, &mut failures);
    if !failures.is_empty() {
        return failures.into_result();
    }

    for expected in &dataset.expected.windows {
        check_window(storage, expected, &mut failures);
    }

    failures.into_result()
}

/// Run the field-level check: every stored row, every field.
pub fn run_deep(deep: &dyn DeepRead, dataset: &Dataset) -> Result<()> {
    let mut failures = Failures::default();

    for website in &dataset.websites {
        let rows = deep.rows(&website.id)?;
        let written: Vec<&Hit> = dataset
            .hits
            .iter()
            .filter(|hit| hit.website_id == website.id)
            .collect();

        if rows.len() != written.len() {
            failures.fail(format!(
                "{}: {} rows stored, {} written",
                website.name,
                rows.len(),
                written.len()
            ));
            continue;
        }

        // The rows come back in insertion order.
        for (index, row) in rows.iter().enumerate() {
            let differences = super::deep::diff(written[index], &row.hit);
            if !differences.is_empty() {
                failures.fail(format!(
                    "{}: row {index} ({}): {}",
                    website.name,
                    row.id,
                    differences.join("; ")
                ));
                if failures.problems.len() > 20 {
                    failures.fail("… more differences were not listed".to_string());
                    break;
                }
            }
        }
    }

    failures.into_result()
}

/// Write the dataset's rows into the backend.
///
/// The websites are created by the runner — the backend assigns their ids, and the
/// dataset is built around those ids.
pub fn write_rows(storage: &dyn Storage, dataset: &Dataset, failures: &mut Failures) {
    // The hits, in batches.
    for chunk in dataset.hits.chunks(1000) {
        if let Err(err) = storage.write_batch(chunk) {
            failures.fail(format!("writing {} hits: {err}", chunk.len()));
            return;
        }
    }

    // The revenue.
    if let Err(err) = storage.write_revenue(&dataset.revenue) {
        failures.fail(format!("writing the revenue: {err}"));
    }
}

/// Check every report of one window.
fn check_window(storage: &dyn Storage, expected: &WindowExpectation, failures: &mut Failures) {
    let query = query_for(&expected.window);
    let label = format!("[{}]", expected.window.name);

    // The summary.
    match storage.summary(&query) {
        Ok(summary) => {
            if summary.pageviews != expected.pageviews {
                failures.fail(format!(
                    "{label} pageviews: {} != {}",
                    summary.pageviews, expected.pageviews
                ));
            }
            if summary.visitors != expected.visitors {
                failures.fail(format!(
                    "{label} visitors: {} != {}",
                    summary.visitors, expected.visitors
                ));
            }
            if summary.views != expected.views {
                failures.fail(format!(
                    "{label} views: {} != {}",
                    summary.views, expected.views
                ));
            }
            if summary.bounces != expected.bounces {
                failures.fail(format!(
                    "{label} bounces: {} != {}",
                    summary.bounces, expected.bounces
                ));
            }
            if (summary.visit_duration - expected.visit_duration).abs() > 0.001 {
                failures.fail(format!(
                    "{label} visit duration: {} != {}",
                    summary.visit_duration, expected.visit_duration
                ));
            }
        }
        Err(err) => failures.fail(format!("{label} summary: {err}")),
    }

    // The breakdowns.
    for (name, dimension) in [
        ("url", Dimension::Url),
        ("referrer", Dimension::Referrer),
        ("browser", Dimension::Browser),
        ("os", Dimension::Os),
        ("device", Dimension::Device),
        ("country", Dimension::Country),
        ("language", Dimension::Language),
        ("screen", Dimension::Screen),
    ] {
        let want = expected.breakdowns.get(name).cloned().unwrap_or_default();
        match storage.breakdown(&query, dimension) {
            Ok(rows) => {
                let got: Vec<(String, i64)> =
                    rows.into_iter().map(|row| (row.name, row.value)).collect();
                if got != want {
                    failures.fail(format!(
                        "{label} the {name} breakdown differs: got {got:?}, want {want:?}"
                    ));
                }
            }
            Err(err) => failures.fail(format!("{label} the {name} breakdown: {err}")),
        }
    }

    // The time series.
    match storage.timeseries(&query) {
        Ok(rows) => {
            let got: Vec<(String, i64)> = rows
                .into_iter()
                .map(|row| (row.name.clone(), row.value))
                .collect();
            if got != expected.timeseries {
                failures.fail(format!(
                    "{label} the time series differs: {} buckets, want {}",
                    got.len(),
                    expected.timeseries.len()
                ));
                let first = got
                    .iter()
                    .zip(expected.timeseries.iter())
                    .find(|(a, b)| a != b);
                if let Some((a, b)) = first {
                    failures.fail(format!("{label}   first difference: {a:?} != {b:?}"));
                }
            }
        }
        Err(err) => failures.fail(format!("{label} the time series: {err}")),
    }

    // The sessions. The order of the ties is not part of the contract, so the two
    // sides are sorted before they are compared.
    match storage.sessions(&query) {
        Ok(rows) => {
            let mut got: Vec<(String, i64, i64)> = rows
                .into_iter()
                .map(|row| (row.id, row.views, row.duration))
                .collect();
            got.sort();
            let mut want = expected.sessions.clone();
            want.sort();
            if got != want {
                failures.fail(format!(
                    "{label} the sessions differ: {} rows, want {}",
                    got.len(),
                    want.len()
                ));
                let first = got.iter().zip(want.iter()).find(|(a, b)| a != b);
                if let Some((a, b)) = first {
                    failures.fail(format!("{label}   first difference: {a:?} != {b:?}"));
                }
            }
        }
        Err(err) => failures.fail(format!("{label} the sessions: {err}")),
    }

    // The event log. Same: the ties are not part of the contract.
    match storage.event_log(&query) {
        Ok(rows) => {
            let mut got: Vec<(String, Option<String>, Option<String>, i64)> = rows
                .into_iter()
                .map(|row| {
                    (
                        row.session_id.unwrap_or_default(),
                        row.url,
                        row.name,
                        row.created_at.and_utc().timestamp_millis(),
                    )
                })
                .collect();
            got.sort();
            let mut want: Vec<(String, Option<String>, Option<String>, i64)> = expected
                .event_log
                .iter()
                .map(|(session, url, name, at)| {
                    (
                        session.clone(),
                        url.clone(),
                        name.clone(),
                        at.and_utc().timestamp_millis(),
                    )
                })
                .collect();
            want.sort();
            if got != want {
                failures.fail(format!(
                    "{label} the event log differs: {} rows, want {}",
                    got.len(),
                    want.len()
                ));
                let first = got.iter().zip(want.iter()).find(|(a, b)| a != b);
                if let Some((a, b)) = first {
                    failures.fail(format!("{label}   first difference: {a:?} != {b:?}"));
                }
            }
        }
        Err(err) => failures.fail(format!("{label} the event log: {err}")),
    }

    // The revenue.
    match storage.revenue(&query) {
        Ok(rows) => {
            let mut got: Vec<(String, String, i64)> = rows
                .into_iter()
                .map(|row| {
                    (
                        row.visit_id
                            .map(crate::session::format_visit_id)
                            .unwrap_or_default(),
                        row.currency,
                        (row.revenue * 1000.0).round() as i64,
                    )
                })
                .collect();
            got.sort();
            let mut want: Vec<(String, String, i64)> = expected
                .revenue
                .iter()
                .map(|(session, currency, amount)| {
                    (session.clone(), currency.clone(), (amount * 1000.0).round() as i64)
                })
                .collect();
            want.sort();
            if got != want {
                failures.fail(format!(
                    "{label} the revenue differs: {} rows, want {}",
                    got.len(),
                    want.len()
                ));
                let first = got.iter().zip(want.iter()).find(|(a, b)| a != b);
                if let Some((a, b)) = first {
                    failures.fail(format!("{label}   first difference: {a:?} != {b:?}"));
                }
            }
        }
        Err(err) => failures.fail(format!("{label} the revenue: {err}")),
    }
}
