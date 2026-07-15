//! Performance smoke (run explicitly, release mode):
//!   cargo test --release -p rsd-daemon --test perf -- --ignored --nocapture
//!
//! Prints commit-path throughput in the two durability modes. Numbers feed the
//! benchmark matrix (DESIGN.md §15); this is a smoke, not the matrix itself.

use rsd_catalog::{Change, Durability};
use rsd_daemon::commit::{synth, Committer};
use rsd_log::{Journal, JournalConfig, Source};
use std::sync::Arc;
use std::time::Instant;

fn run(n: u64, batch: usize, sync: bool) -> (f64, f64) {
    let dir = tempfile::tempdir().unwrap();
    let catalog = Arc::new(
        rsd_catalog::Catalog::open_with_durability(
            &dir.path().join("cat.redb"),
            Durability::Eventual,
        )
        .unwrap(),
    );
    let journal = Journal::open(
        &dir.path().join("journal"),
        JournalConfig {
            sync_on_append: sync,
            ..Default::default()
        },
    )
    .unwrap();
    let mut committer = Committer::new(catalog, journal);

    let changes: Vec<Change> = (0..n).map(synth::change).collect();
    let t0 = Instant::now();
    let mut batches = 0u64;
    for chunk in changes.chunks(batch) {
        committer.commit(Source::Synthetic, chunk).unwrap();
        batches += 1;
    }
    let dt = t0.elapsed().as_secs_f64();
    (n as f64 / dt, batches as f64 / dt)
}

#[test]
#[ignore]
fn commit_throughput() {
    // Warm.
    run(5_000, 512, false);

    let (cps, bps) = run(200_000, 512, false);
    println!("no-fsync   (journal is durability point deferred): {cps:>12.0} changes/s  {bps:>8.0} batches/s");

    let (cps, bps) = run(20_000, 512, true);
    println!("fsync/batch (production journal durability):       {cps:>12.0} changes/s  {bps:>8.0} batches/s");

    let (cps, bps) = run(20_000, 16, true);
    println!("fsync/batch, small batches (worst realistic case): {cps:>12.0} changes/s  {bps:>8.0} batches/s");
}
