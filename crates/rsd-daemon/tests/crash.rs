//! P2.4: the crash-injection gate — permanent in CI.
//!
//! ≥500 SIGKILLs at randomized points during commit storms, across parallel
//! runs. After each run completes (through however many kill-resume cycles):
//!   1. the catalog equals the never-crashed reference state;
//!   2. catalog invariants hold;
//!   3. a FRESH catalog rebuilt purely from journal replay equals it too
//!      (mutual reconstructability of the catalog plane);
//!   4. journal LSNs are gapless from 1.

use rand::prelude::*;
use rand_chacha::ChaCha8Rng;
use rsd_catalog::{Catalog, Change, Durability};
use rsd_daemon::commit::{synth, Committer};
use rsd_log::{Journal, JournalConfig};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const OPS: u64 = 1_200;
const TARGET_KILLS: u64 = 500;
const THREADS: u64 = 4;

fn run_one(dir: &Path, rng: &mut ChaCha8Rng) -> u64 {
    let child_bin = env!("CARGO_BIN_EXE_crash-child");
    let mut kills = 0u64;
    let mut cycles = 0u32;
    loop {
        cycles += 1;
        assert!(
            cycles < 300,
            "child made no progress after {cycles} kill-resume cycles"
        );
        let mut child = Command::new(child_bin)
            .arg(dir)
            .arg(OPS.to_string())
            .spawn()
            .expect("spawn crash-child");
        // Escalating delay: early cycles kill during startup/first commits,
        // later cycles reach deeper into the stream, and eventually the child
        // is allowed to finish.
        let delay = rng.gen_range(0..12) + (cycles as u64) * 4;
        std::thread::sleep(Duration::from_millis(delay));
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success(), "child exited with {status}");
                return kills;
            }
            None => {
                child.kill().expect("kill");
                let _ = child.wait();
                kills += 1;
            }
        }
    }
}

fn verify(dir: &Path) {
    // Open the survivor stores; recover any journal tail past the watermark.
    let catalog = std::sync::Arc::new(
        Catalog::open_with_durability(&dir.join("cat.redb"), Durability::Eventual).unwrap(),
    );
    let journal = Journal::open(
        &dir.join("journal"),
        JournalConfig {
            sync_on_append: false,
            segment_max_bytes: 32 * 1024,
        },
    )
    .unwrap();
    let mut committer = Committer::new(catalog.clone(), journal);
    committer.recover().unwrap();

    // 1. Equal to the never-crashed reference.
    let want = synth::expected(OPS);
    let got = catalog.listing().unwrap();
    assert_eq!(got, want, "diverged from reference after crash-recovery");

    // 2. Invariants.
    catalog.check_invariants().unwrap();

    // 3. Mutual reconstructability: a fresh catalog built purely by replaying
    // the journal must reach the same state.
    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh =
        Catalog::open_with_durability(&fresh_dir.path().join("fresh.redb"), Durability::None)
            .unwrap();
    let journal = Journal::open(
        &dir.join("journal"),
        JournalConfig {
            sync_on_append: false,
            segment_max_bytes: 32 * 1024,
        },
    )
    .unwrap();
    let mut records: Vec<(u64, Change)> = Vec::new();
    journal
        .replay(1, |r| records.push((r.lsn, r.change)))
        .unwrap();
    for chunk in records.chunks(1024) {
        let first = chunk[0].0;
        let changes: Vec<Change> = chunk.iter().map(|(_, c)| c.clone()).collect();
        fresh.apply_changes(first, &changes).unwrap();
    }
    assert_eq!(
        fresh.listing().unwrap(),
        want,
        "journal replay did not reconstruct the catalog plane"
    );

    // 4. Gapless LSNs from 1 (torn tails were truncated, never skipped).
    let lsns: Vec<u64> = records.iter().map(|(l, _)| *l).collect();
    let expect: Vec<u64> = (1..=lsns.len() as u64).collect();
    assert_eq!(lsns, expect, "journal has LSN gaps or reordering");
}

#[test]
fn crash_injection_gate_500_kills() {
    let total_kills = AtomicU64::new(0);
    let total_runs = AtomicU64::new(0);

    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let total_kills = &total_kills;
            let total_runs = &total_runs;
            scope.spawn(move || {
                let mut rng = ChaCha8Rng::seed_from_u64(0xC0FFEE + t);
                while total_kills.load(Ordering::Relaxed) < TARGET_KILLS {
                    let dir = tempfile::tempdir().unwrap();
                    let kills = run_one(dir.path(), &mut rng);
                    verify(dir.path());
                    total_kills.fetch_add(kills, Ordering::Relaxed);
                    total_runs.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    let kills = total_kills.load(Ordering::Relaxed);
    let runs = total_runs.load(Ordering::Relaxed);
    eprintln!("crash gate: {kills} kills across {runs} runs, all recovered exactly");
    assert!(kills >= TARGET_KILLS);
}
