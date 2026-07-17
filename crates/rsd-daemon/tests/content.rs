//! P3.3 success criteria, counter-proven end to end:
//!  - a rename storm on 1k indexed files causes ZERO extractions;
//!  - one content change causes EXACTLY one extraction;
//!  - a byte-identical copy is a CAES hit (zero extractions);
//!  - repeated extraction failure quarantines the content with a queryable
//!    state.

use rsd_caes::Store;
use rsd_catalog::{Catalog, Durability};
use rsd_daemon::{bring_up, ContentIndexer, ContentSource, PipelineConfig};
use rsd_extract::{extract_bytes, Budgets, ExtractHints};
use rsd_ingest::CoalescerConfig;
use rsd_testkit::converged;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// In-process source: real extraction logic, counted, with a poison trigger.
struct CountingSource {
    calls: Arc<AtomicU64>,
}

impl ContentSource for CountingSource {
    fn extract_file(
        &mut self,
        file: &std::fs::File,
        _path: &Path,
        hints: &ExtractHints,
        budgets: &Budgets,
    ) -> Result<rsd_caes::ExtractionRecord, String> {
        let mut file = file.try_clone().map_err(|error| error.to_string())?;
        std::io::Seek::rewind(&mut file).map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes).map_err(|error| error.to_string())?;
        if bytes.starts_with(b"POISON") {
            return Err("simulated extractor crash".into());
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(extract_bytes(hints, budgets, &bytes))
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    cat: Arc<Catalog>,
    calls: Arc<AtomicU64>,
    counters: Arc<rsd_daemon::ContentCounters>,
    pipeline: rsd_daemon::Pipeline,
}

fn fast_cfg() -> PipelineConfig {
    PipelineConfig {
        coalescer: CoalescerConfig {
            quiet: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
            max_pending: 65_536,
        },
        fsevents_latency: Duration::from_millis(50),
        journal_sync: false,
        ..Default::default()
    }
}

fn setup(files: usize) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("tree");
    std::fs::create_dir(&root).unwrap();
    for i in 0..files {
        std::fs::write(
            root.join(format!("f{i}.txt")),
            format!("unique content {i}"),
        )
        .unwrap();
    }
    let cat =
        Arc::new(Catalog::open_with_durability(&base.join("cat.redb"), Durability::None).unwrap());
    let caes = Arc::new(Store::open(&base.join("caes.redb")).unwrap());
    let calls = Arc::new(AtomicU64::new(0));
    let indexer = ContentIndexer::new(
        Box::new(CountingSource {
            calls: calls.clone(),
        }),
        caes,
    );
    let counters = indexer.counters.clone();
    let (pipeline, _) = bring_up(
        cat.clone(),
        &base.join("journal"),
        &root,
        Some(indexer),
        None,
        None,
        None,
        fast_cfg(),
    )
    .unwrap();
    Env {
        _tmp: tmp,
        root,
        cat,
        calls,
        counters,
        pipeline,
    }
}

fn wait_until(mut f: impl FnMut() -> bool, deadline: Duration, what: &str) {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for: {what}");
}

fn wait_converged(env: &Env) {
    wait_until(
        || converged(&env.cat, &env.root).is_ok(),
        Duration::from_secs(30),
        "fs convergence",
    );
}

#[test]
fn rename_storm_on_1k_files_causes_zero_extractions() {
    let env = setup(1_000);
    let after_bootstrap = env.calls.load(Ordering::Relaxed);
    assert_eq!(
        after_bootstrap, 1_000,
        "bootstrap indexes every unique file"
    );

    // Rename every file.
    for i in 0..1_000 {
        std::fs::rename(
            env.root.join(format!("f{i}.txt")),
            env.root.join(format!("renamed-{i}.txt")),
        )
        .unwrap();
    }
    wait_converged(&env);
    // Let any stray content work settle, then assert.
    wait_until(
        || env.counters.skipped_unchanged.load(Ordering::Relaxed) > 0,
        Duration::from_secs(10),
        "skip counter movement",
    );
    assert_eq!(
        env.calls.load(Ordering::Relaxed),
        after_bootstrap,
        "renames must never re-extract"
    );

    // And the index state is queryable on the renamed entry.
    let (_, rec) = env
        .cat
        .get_by_path(&env.root.join("renamed-7.txt").to_string_lossy())
        .unwrap()
        .expect("renamed file cataloged");
    assert_eq!(rec.index_state.as_deref(), Some("complete"));
    assert!(rec.content_hash.is_some());
    env.pipeline.stop();
}

#[test]
fn one_content_change_causes_exactly_one_extraction() {
    let env = setup(50);
    assert_eq!(env.calls.load(Ordering::Relaxed), 50);

    std::fs::write(env.root.join("f7.txt"), "entirely new content").unwrap();
    wait_until(
        || env.calls.load(Ordering::Relaxed) == 51,
        Duration::from_secs(10),
        "exactly one new extraction",
    );
    // Quiesce and confirm no runaway.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(env.calls.load(Ordering::Relaxed), 51);
    env.pipeline.stop();
}

#[test]
fn byte_identical_copy_is_a_caes_hit_not_an_extraction() {
    let env = setup(20);
    assert_eq!(env.calls.load(Ordering::Relaxed), 20);
    let hits0 = env.counters.caes_hits.load(Ordering::Relaxed);

    std::fs::copy(env.root.join("f3.txt"), env.root.join("copy-of-f3.txt")).unwrap();
    let copy = env.root.join("copy-of-f3.txt").to_string_lossy().into_owned();
    wait_until(
        || {
            env.cat
                .get_by_path(&copy)
                .ok()
                .flatten()
                .is_some_and(|(_, rec)| rec.index_state.as_deref() == Some("complete"))
        },
        Duration::from_secs(10),
        "committed content state for the CAES-hit copy",
    );
    assert!(env.counters.caes_hits.load(Ordering::Relaxed) > hits0);
    assert_eq!(
        env.calls.load(Ordering::Relaxed),
        20,
        "copy must be a pure store hit"
    );
    env.pipeline.stop();
}

#[test]
fn repeated_failure_quarantines_with_queryable_state() {
    let env = setup(3);
    let poison = env.root.join("hostile.txt");

    // Failure 1 (create), then two byte-identical rewrites (same content hash,
    // new mtime => new event) to trigger retries 2 and 3.
    std::fs::write(&poison, "POISON payload").unwrap();
    for _ in 0..2 {
        std::thread::sleep(Duration::from_millis(500));
        std::fs::write(&poison, "POISON payload").unwrap();
    }

    wait_until(
        || env.counters.quarantined.load(Ordering::Relaxed) >= 1,
        Duration::from_secs(20),
        "quarantine after 3 failures",
    );
    wait_until(
        || {
            env.cat
                .get_by_path(&poison.to_string_lossy())
                .unwrap()
                .is_some_and(|(_, r)| r.index_state.as_deref() == Some("quarantined"))
        },
        Duration::from_secs(10),
        "queryable quarantined state",
    );
    assert!(env.counters.failures.load(Ordering::Relaxed) >= 3);
    env.pipeline.stop();
}

#[test]
fn extraction_failure_budget_survives_daemon_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("tree");
    std::fs::create_dir(&root).unwrap();
    let poison = root.join("hostile.txt");
    std::fs::write(&poison, "POISON persistent payload").unwrap();
    let cat =
        Arc::new(Catalog::open_with_durability(&base.join("cat.redb"), Durability::None).unwrap());

    for pass in 1..=3 {
        let caes = Arc::new(Store::open(&base.join("caes.redb")).unwrap());
        let indexer = ContentIndexer::new(
            Box::new(CountingSource {
                calls: Arc::new(AtomicU64::new(0)),
            }),
            caes,
        );
        let counters = indexer.counters.clone();
        let (pipeline, _) = bring_up(
            cat.clone(),
            &base.join("journal"),
            &root,
            Some(indexer),
            None,
            None,
            None,
            fast_cfg(),
        )
        .unwrap();

        assert_eq!(
            counters.failures.load(Ordering::Relaxed),
            1,
            "each daemon lifetime performs only its one budgeted retry"
        );
        assert_eq!(
            counters.quarantined.load(Ordering::Relaxed),
            u64::from(pass == 3)
        );
        pipeline.stop();
    }

    let (_, record) = cat
        .get_by_path(&poison.to_string_lossy())
        .unwrap()
        .expect("hostile file remains cataloged");
    assert_eq!(record.index_state.as_deref(), Some("quarantined"));
}
