//! The dataset, measured: the shape of the traffic the generator produces.
//!
//! The generator is the **truth** every engine is measured against, so its
//! shape decides what the numbers mean. This probe prints the properties that
//! the storage design leans on — above all the **order of the visit ids**:
//! the ids are minted when the request arrives, so the id's order is the
//! order the visits happen in, and the sort key may lean on that.
//!
//! ```bash
//! cargo run --release --example dataset_shape -p sumimi-service -- 200
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::NaiveDateTime;
use sumimi_service::event_id;
use sumimi_service::testing::dataset;
use uuid::Uuid;

/// The visit, as the generator made it.
struct Visit {
    id: Uuid,
    start: Option<NaiveDateTime>,
    site: String,
    rows: usize,
    first: NaiveDateTime,
    last: NaiveDateTime,
}

fn main() -> anyhow::Result<()> {
    let visitors: i64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    let ids: Vec<String> = (0..3).map(|n| format!("site-{n}")).collect();
    let data = dataset::build(visitors, &ids);

    // The visits, in the order the ids were handed out.
    let mut visits: BTreeMap<Uuid, Visit> = BTreeMap::new();
    let mut order: Vec<Uuid> = Vec::new();
    for hit in &data.hits {
        let visit = visits.entry(hit.visit_id).or_insert_with(|| {
            order.push(hit.visit_id);
            Visit {
                id: hit.visit_id,
                start: hit.session_start,
                site: hit.website_id.clone(),
                rows: 0,
                first: hit.created_at,
                last: hit.created_at,
            }
        });
        visit.rows += 1;
        visit.first = visit.first.min(hit.created_at);
        visit.last = visit.last.max(hit.created_at);
    }
    let minted: Vec<&Visit> = order
        .iter()
        .filter_map(|id| {
            let visit = visits.get(id)?;
            if event_id::layout_of(&visit.id) == event_id::VISIT_VERSION {
                Some(visit)
            } else {
                None
            }
        })
        .collect();

    println!("the dataset: {} rows, {} visits", data.hits.len(), visits.len());

    // 1. The ids follow the clock.
    let mut descents = 0;
    for pair in minted.windows(2) {
        if pair[0].start > pair[1].start {
            descents += 1;
        }
    }
    println!("\n1. the ids follow the clock");
    println!("   visits minted by the counter: {}", minted.len());
    println!(
        "   a visit that starts earlier than the one before it: {descents} (the old generator: 271,068)"
    );

    // 2. One start per visit.
    let mut starts: HashMap<Uuid, HashSet<Option<NaiveDateTime>>> = HashMap::new();
    for hit in &data.hits {
        starts
            .entry(hit.visit_id)
            .or_default()
            .insert(hit.session_start);
    }
    let many = starts.values().filter(|set| set.len() > 1).count();
    println!("\n2. one start per visit");
    println!("   visits with more than one start: {many}");

    // 3. No id is shared by two sites.
    let mut site_of: HashMap<Uuid, &str> = HashMap::new();
    let mut shared = 0;
    for visit in visits.values() {
        match site_of.insert(visit.id, visit.site.as_str()) {
            Some(previous) if previous != visit.site => shared += 1,
            _ => {}
        }
    }
    println!("\n3. no id is shared by two sites");
    println!("   ids on two sites: {shared}");

    // 4. How long a visit lasts, and how many rows it has.
    let mut durations: Vec<i64> = minted
        .iter()
        .map(|visit| (visit.last - visit.first).num_seconds())
        .collect();
    durations.sort_unstable();
    // **The rows are sorted before the median is taken** — the visits are in
    // the order their ids were minted, and the middle *element* of that order
    // is one visit, not the median one.
    let mut rows: Vec<usize> = minted.iter().map(|visit| visit.rows).collect();
    rows.sort_unstable();
    println!("\n4. the shape of a visit");
    println!(
        "   the rows of a visit: min {}, median {}, max {}",
        rows.iter().min().unwrap_or(&0),
        rows.get(rows.len() / 2).copied().unwrap_or(0),
        rows.iter().max().unwrap_or(&0)
    );
    println!(
        "   how long a visit lasts: median {}s, p90 {}s, max {}s (the idle timeout is {}s)",
        durations.get(durations.len() / 2).copied().unwrap_or(0),
        durations.get(durations.len() * 9 / 10).copied().unwrap_or(0),
        durations.last().copied().unwrap_or(0),
        sumimi_service::session::IDLE_TIMEOUT_SECONDS
    );

    // 5. How many visits share a start.
    let mut per_start: HashMap<Option<NaiveDateTime>, usize> = HashMap::new();
    for visit in &minted {
        *per_start.entry(visit.start).or_default() += 1;
    }
    let sharing: usize = per_start.values().filter(|count| **count > 1).sum();
    println!("\n5. the starts");
    println!(
        "   distinct starts: {} for {} visits",
        per_start.len(),
        minted.len()
    );
    println!("   visits that share a start with another: {sharing}");

    // 6. The rows go in as they arrive: by time.
    let mut descents = 0;
    for pair in data.hits.windows(2) {
        if pair[0].created_at > pair[1].created_at {
            descents += 1;
        }
    }
    println!("\n6. the rows are written in the order they happened");
    println!("   rows out of order: {descents}");

    Ok(())
}
