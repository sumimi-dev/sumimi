//! **Could the event id become a concurrency bottleneck?**
//!
//! The id is handed out by one atomic compare-and-swap (the state is the last
//! id itself — see `service/src/event_id.rs`). That is the kind of thing that
//! *can* become a bottleneck: one cache line, hammered by every writer.
//!
//! This measures how fast the hand-out is, and how fast it stays when more
//! than one thread asks for an id at the same time.
//!
//! ```bash
//! cargo run --release --example event_id_rate -p sumimi-service
//! ```

use std::sync::Arc;
use std::time::Instant;

use sumimi_service::event_id::Generator;

fn main() -> anyhow::Result<()> {
    let total: u64 = 20_000_000;

    println!("=== one thread ===");
    let generator = Generator::new(7);
    let start = Instant::now();
    let mut last = 0u64;
    for _ in 0..total {
        last = generator.counter();
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "  {total} ids in {:.3} s   ⇒  {:.0} ids/s   ({:.1} ns per id)   last counter {last}",
        elapsed,
        total as f64 / elapsed,
        elapsed * 1e9 / total as f64,
    );

    println!();
    println!("=== several threads, one generator ===");
    for threads in [2usize, 4, 8, 16] {
        let generator = Arc::new(Generator::new(7));
        let each = total / threads as u64;
        let start = Instant::now();
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let generator = generator.clone();
                std::thread::spawn(move || {
                    let mut last = 0u64;
                    for _ in 0..each {
                        last = generator.counter();
                    }
                    last
                })
            })
            .collect();
        let mut ids = 0u64;
        let mut distinct = std::collections::HashSet::new();
        for handle in handles {
            let last = handle.join().unwrap();
            ids += each;
            distinct.insert(last);
        }
        let elapsed = start.elapsed().as_secs_f64();
        println!(
            "  {threads:>2} threads   ⇒  {:>10.0} ids/s   ({:.1} ns per id)   last counters distinct: {}",
            ids as f64 / elapsed,
            elapsed * 1e9 / ids as f64,
            distinct.len() == threads,
        );
    }

    println!();
    println!("=== what the traffic looks like ===");
    for (what, per_second) in [
        ("a site with 1 000 events/day", 1_000.0 / 86_400.0),
        ("a site with 1 000 000 events/day", 1_000_000.0 / 86_400.0),
        ("a site with 100 000 events/second", 100_000.0),
    ] {
        println!("  {what:<38} {per_second:>12.1} events/s");
    }
    Ok(())
}
