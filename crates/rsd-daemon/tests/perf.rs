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

#[test]
#[ignore]
fn lexical_query_latency_100k_docs() {
    use rsd_catalog::{Catalog, Change, Durability, FileId, ObjectKind, StatInfo};

    let dir = tempfile::tempdir().unwrap();
    let catalog = Arc::new(
        Catalog::open_with_durability(&dir.path().join("cat.redb"), Durability::None).unwrap(),
    );
    let caes = Arc::new(rsd_caes::Store::open(&dir.path().join("caes.redb")).unwrap());
    let mut plane = rsd_lexical::LexicalPlane::open(&dir.path().join("lexical")).unwrap();

    // 100k docs: common words + per-doc rare terms, realistic-ish zipf-y text.
    const N: u64 = 100_000;
    let words = [
        "invoice",
        "meeting",
        "prototype",
        "engine",
        "quarterly",
        "payment",
        "report",
        "design",
        "kernel",
        "journal",
        "catalog",
        "search",
        "index",
        "vector",
        "sealed",
        "worker",
    ];
    let mut lsn = 1u64;
    let t_build = Instant::now();
    for chunk_start in (0..N).step_by(1024) {
        let mut changes = Vec::new();
        for i in chunk_start..(chunk_start + 1024).min(N) {
            let path = format!("/corpus/doc{i}.txt");
            let text: String = (0..24)
                .map(|k| words[((i * 31 + k * 7) % 16) as usize])
                .collect::<Vec<_>>()
                .join(" ")
                + &format!(" rareterm{i}");
            let hash = *blake3::hash(text.as_bytes()).as_bytes();
            let hints = rsd_extract::ExtractHints {
                name: format!("doc{i}.txt"),
                full_size: text.len() as u64,
            };
            let hh = hints.hints_hash(false);
            caes.put(
                &rsd_caes::CaesKey {
                    content_hash: hash,
                    extractor_id: rsd_extract::EXTRACTOR_ID.into(),
                    extractor_version: rsd_extract::EXTRACTOR_VERSION,
                    hints_hash: hh,
                    abi_version: rsd_caes::ABI_VERSION,
                },
                &rsd_caes::ExtractionRecord {
                    status: rsd_caes::ExtractStatus::Complete,
                    text,
                    attrs: vec![],
                    symbols: vec![],
                },
            )
            .unwrap();
            changes.push(Change::Upsert {
                path: path.clone(),
                stat: StatInfo {
                    kind: ObjectKind::File,
                    file_id: FileId { dev: 9, ino: i + 1 },
                    size: 100,
                    mtime_ns: i as i64,
                    birthtime_ns: 1,
                    nlink: 1,
                },
            });
            changes.push(Change::SetContent {
                path,
                content_hash: hash,
                hints_hash: hh,
                state: "complete".into(),
            });
        }
        let first = lsn;
        lsn += changes.len() as u64;
        catalog.apply_changes(first, &changes).unwrap();
        plane.apply(first, &changes, &[], &catalog, &caes).unwrap();
    }
    println!(
        "built 100k-doc corpus in {:?} ({} docs)",
        t_build.elapsed(),
        plane.doc_count().unwrap()
    );
    drop(plane);

    let reader = rsd_lexical::LexicalReader::open(&dir.path().join("lexical")).unwrap();
    // Warm.
    for w in words {
        reader.search_content(w, false, 100).unwrap();
    }
    let mut lat: Vec<u128> = Vec::with_capacity(2_000);
    for q in 0..2_000u64 {
        let query = if q % 2 == 0 {
            words[(q % 16) as usize].to_string()
        } else {
            format!("rareterm{}", q * 37 % N)
        };
        let t = Instant::now();
        let hits = reader.search_content(&query, false, 100).unwrap();
        lat.push(t.elapsed().as_micros());
        assert!(!hits.is_empty());
    }
    lat.sort_unstable();
    let p50 = lat[lat.len() / 2];
    let p99 = lat[lat.len() * 99 / 100];
    println!("lexical query latency over 100k docs: p50={p50}us p99={p99}us");
    assert!(p50 < 1_000, "p50 target 1ms, got {p50}us");
    assert!(p99 < 10_000, "p99 target 10ms, got {p99}us");
}
