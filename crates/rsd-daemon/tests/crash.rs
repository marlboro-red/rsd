//! P2.4: the crash-injection gate — permanent in CI.
//!
//! ≥500 SIGKILLs at randomized points during commit storms, across parallel
//! runs. After each run completes (through however many kill-resume cycles):
//!   1. the catalog equals the never-crashed reference state;
//!   2. catalog invariants hold;
//!   3. fresh catalog, lexical, and vector planes rebuilt from journal + CAES
//!      equal the survivors (mutual reconstructability of every plane);
//!   4. deleted/content-invalidated objects are absent from both projections;
//!   5. journal LSNs are gapless from 1.

use rand::prelude::*;
use rand_chacha::ChaCha8Rng;
use rsd_catalog::{Catalog, Change, Durability};
use rsd_daemon::commit::{synth, Committer};
use rsd_log::{Journal, JournalConfig};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OPS: u64 = 320;
const TARGET_KILLS: u64 = 500;
const THREADS: u64 = 4;

fn run_one(dir: &Path, rng: &mut ChaCha8Rng, total_kills: &AtomicU64, target_kills: u64) -> u64 {
    let caes = rsd_caes::Store::open(&dir.join("caes.redb")).unwrap();
    for i in 0..OPS {
        if let Some((key, record)) = synth::caes_entry(i) {
            caes.put(&key, &record).unwrap();
        }
    }
    drop(caes);

    let child_bin = env!("CARGO_BIN_EXE_crash-child");
    let mut kills = 0u64;
    let mut cycles = 0u32;
    loop {
        cycles += 1;
        assert!(
            cycles < 100,
            "child made no progress after {cycles} kill-resume cycles"
        );
        let mut child = Command::new(child_bin)
            .arg(dir)
            .arg(OPS.to_string())
            .spawn()
            .expect("spawn crash-child");
        if total_kills.load(Ordering::Acquire) >= target_kills {
            let status = child.wait().expect("wait for final recovery run");
            assert!(status.success(), "child exited with {status}");
            return kills;
        }
        // Escalating delay: early cycles kill during startup/first commits,
        // later cycles reach deeper into the stream, and eventually the child
        // is allowed to finish.
        let delay = rng.gen_range(0..20) + (cycles as u64) * 20;
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
                total_kills.fetch_add(1, Ordering::AcqRel);
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
            segment_max_bytes: 8 * 1024,
        },
    )
    .unwrap();
    let caes = Arc::new(rsd_caes::Store::open(&dir.join("caes.redb")).unwrap());
    let lexical = rsd_lexical::LexicalPlane::open(&dir.join("lexical")).unwrap();
    let vector = Arc::new(Mutex::new(
        rsd_vector::VectorPlane::open(
            &dir.join("vector.redb"),
            Arc::new(rsd_vector::HashEmbedder::default()),
        )
        .unwrap(),
    ));
    let mut committer = Committer::new(catalog.clone(), journal)
        .with_lexical(lexical, caes.clone())
        .with_vector(vector.clone(), caes.clone());
    committer.recover().unwrap();

    // 1. Equal to the never-crashed reference.
    let want = synth::expected(OPS);
    let got = catalog.listing().unwrap();
    assert_eq!(got, want, "diverged from reference after crash-recovery");

    // 2. Invariants.
    catalog.check_invariants().unwrap();

    let expected_oids: std::collections::BTreeSet<u64> = want
        .keys()
        .filter_map(|path| catalog.get_by_path(path).unwrap())
        .filter_map(|(oid, record)| record.content_hash.map(|_| oid))
        .collect();
    let survivor_lexical: std::collections::BTreeSet<u64> = committer
        .lexical()
        .unwrap()
        .search_content("synthetic", false, 10_000)
        .unwrap()
        .into_iter()
        .collect();
    let survivor_vector: std::collections::BTreeSet<u64> = vector
        .lock()
        .unwrap()
        .search("synthetic", 10_000)
        .unwrap()
        .into_iter()
        .map(|hit| hit.oid)
        .collect();
    assert_eq!(
        survivor_lexical, expected_oids,
        "stale/missing lexical docs"
    );
    assert_eq!(survivor_vector, expected_oids, "stale/missing vector docs");

    // 3. Mutual reconstructability: fresh projections built purely from the
    // journal and CAES must reach the same state without filesystem reads.
    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh = Arc::new(
        Catalog::open_with_durability(&fresh_dir.path().join("fresh.redb"), Durability::None)
            .unwrap(),
    );
    let journal = Journal::open(
        &dir.join("journal"),
        JournalConfig {
            sync_on_append: false,
            segment_max_bytes: 8 * 1024,
        },
    )
    .unwrap();
    let fresh_lexical = rsd_lexical::LexicalPlane::open(&fresh_dir.path().join("lexical")).unwrap();
    let fresh_vector = Arc::new(Mutex::new(
        rsd_vector::VectorPlane::open(
            &fresh_dir.path().join("vector.redb"),
            Arc::new(rsd_vector::HashEmbedder::default()),
        )
        .unwrap(),
    ));
    let mut fresh_committer = Committer::new(fresh.clone(), journal)
        .with_lexical(fresh_lexical, caes.clone())
        .with_vector(fresh_vector.clone(), caes);
    fresh_committer.recover().unwrap();

    assert_eq!(fresh.listing().unwrap(), want, "fresh catalog diverged");
    let fresh_lexical: std::collections::BTreeSet<u64> = fresh_committer
        .lexical()
        .unwrap()
        .search_content("synthetic", false, 10_000)
        .unwrap()
        .into_iter()
        .collect();
    let fresh_vector: std::collections::BTreeSet<u64> = fresh_vector
        .lock()
        .unwrap()
        .search("synthetic", 10_000)
        .unwrap()
        .into_iter()
        .map(|hit| hit.oid)
        .collect();
    assert_eq!(fresh_lexical, survivor_lexical, "lexical rebuild diverged");
    assert_eq!(fresh_vector, survivor_vector, "vector rebuild diverged");

    // 5. Gapless LSNs from 1 (torn tails were truncated, never skipped).
    let journal = Journal::open(
        &dir.join("journal"),
        JournalConfig {
            sync_on_append: false,
            segment_max_bytes: 8 * 1024,
        },
    )
    .unwrap();
    let mut records: Vec<(u64, Change)> = Vec::new();
    journal
        .replay(1, |r| records.push((r.lsn, r.change)))
        .unwrap();
    let lsns: Vec<u64> = records.iter().map(|(l, _)| *l).collect();
    let expect: Vec<u64> = (1..=lsns.len() as u64).collect();
    assert_eq!(lsns, expect, "journal has LSN gaps or reordering");
}

#[test]
fn crash_injection_gate_500_kills() {
    let target_kills = std::env::var("RSD_CRASH_TARGET_KILLS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(TARGET_KILLS);
    let threads = std::env::var("RSD_CRASH_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(THREADS);
    let total_kills = AtomicU64::new(0);
    let total_runs = AtomicU64::new(0);

    std::thread::scope(|scope| {
        for t in 0..threads {
            let total_kills = &total_kills;
            let total_runs = &total_runs;
            scope.spawn(move || {
                let mut rng = ChaCha8Rng::seed_from_u64(0xC0FFEE + t);
                while total_kills.load(Ordering::Relaxed) < target_kills {
                    let dir = tempfile::tempdir().unwrap();
                    run_one(dir.path(), &mut rng, total_kills, target_kills);
                    verify(dir.path());
                    total_runs.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    let kills = total_kills.load(Ordering::Relaxed);
    let runs = total_runs.load(Ordering::Relaxed);
    eprintln!("crash gate: {kills} kills across {runs} runs, all recovered exactly");
    assert!(kills >= target_kills);
}
