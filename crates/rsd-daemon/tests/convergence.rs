//! P1.6: the end-to-end convergence harness — the permanent CI gate.
//!
//! Criteria:
//!  - live mutation storm converges with ZERO full rescans (events only);
//!  - watcher-channel overflow degrades to a counted root rescan and converges;
//!  - rename storms preserve object identity via by_fileid + orphan grace.

use rsd_catalog::{Catalog, Durability};
use rsd_daemon::{bring_up, Pipeline, PipelineConfig};
use rsd_ingest::CoalescerConfig;
use rsd_testkit::{assert_converged, converged, gen_tree, Mutator};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn fast_cfg() -> PipelineConfig {
    PipelineConfig {
        coalescer: CoalescerConfig {
            quiet: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
            max_pending: 65_536,
        },
        fsevents_latency: Duration::from_millis(50),
        ..Default::default()
    }
}

fn setup(files: usize, seed: u64) -> (tempfile::TempDir, PathBuf, Arc<Catalog>) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("tree");
    std::fs::create_dir(&root).unwrap();
    gen_tree(&root, files, seed).unwrap();
    let cat = Arc::new(
        Catalog::open_with_durability(&tmp.path().join("cat.redb"), Durability::None).unwrap(),
    );
    (tmp, root, cat)
}

/// Poll until the catalog converges to the filesystem or the deadline passes.
fn wait_converged(cat: &Catalog, root: &Path, deadline: Duration) {
    let start = Instant::now();
    let mut last_err = String::new();
    while start.elapsed() < deadline {
        match converged(cat, root) {
            Ok(()) => return,
            Err(e) => last_err = e,
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("did not converge within {deadline:?}; last diff:\n{last_err}");
}

#[test]
fn live_storm_converges_with_zero_full_rescans() {
    let (_tmp, root, cat) = setup(800, 21);
    let (pipeline, _) = bring_up(cat.clone(), &root, fast_cfg()).unwrap();
    assert_converged(&cat, &root);

    let mut m = Mutator::new(&root, 22).unwrap();
    m.run(500).unwrap();

    wait_converged(&cat, &root, Duration::from_secs(30));
    cat.check_invariants().unwrap();

    assert!(
        !pipeline.overflowed(),
        "watcher overflowed; capacity too small for this storm"
    );
    assert_eq!(
        pipeline.counters.full_rescans.load(Ordering::Relaxed),
        0,
        "converged only via full rescan — event path is broken"
    );
    assert!(pipeline.counters.work_items.load(Ordering::Relaxed) > 0);
    pipeline.stop();
}

#[test]
fn overflow_degrades_to_counted_root_rescan_and_converges() {
    let (_tmp, root, cat) = setup(50, 31);
    let cfg = PipelineConfig {
        event_capacity: 8, // force the callback to shed events
        ..fast_cfg()
    };
    let (pipeline, _) = bring_up(cat.clone(), &root, cfg).unwrap();

    for i in 0..400 {
        std::fs::write(root.join(format!("flood{i}.txt")), "x").unwrap();
    }

    // Wait for the shed flag, then run the supervision recovery exactly as the
    // daemon main loop would.
    let start = Instant::now();
    while !pipeline.overflowed() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(pipeline.overflowed(), "expected overflow with capacity 8");
    // Give the coalescer a beat to flush whatever it did receive, then recover.
    std::thread::sleep(Duration::from_millis(500));
    pipeline.recover_overflow_if_any(&cat, &root);

    wait_converged(&cat, &root, Duration::from_secs(30));
    cat.check_invariants().unwrap();
    assert!(
        pipeline.counters.full_rescans.load(Ordering::Relaxed) >= 1,
        "overflow must be recovered by a counted rescan"
    );
    pipeline.stop();
}

#[test]
fn rename_storm_preserves_object_identity() {
    let (_tmp, root, cat) = setup(50, 41);
    let (pipeline, _) = bring_up(cat.clone(), &root, fast_cfg()).unwrap();
    assert_converged(&cat, &root);

    // Track one file through a storm of renames.
    let subject = root.join("subject.bin");
    std::fs::write(&subject, "identity").unwrap();
    wait_converged(&cat, &root, Duration::from_secs(10));
    let (oid0, _) = cat
        .get_by_path(&subject.to_string_lossy())
        .unwrap()
        .expect("subject cataloged");

    let mut cur = subject.clone();
    for i in 0..6 {
        let next = root.join(format!("subject-r{i}.bin"));
        std::fs::rename(&cur, &next).unwrap();
        cur = next;
        std::thread::sleep(Duration::from_millis(120));
    }

    wait_converged(&cat, &root, Duration::from_secs(20));
    cat.check_invariants().unwrap();
    let (oid1, rec) = cat
        .get_by_path(&cur.to_string_lossy())
        .unwrap()
        .expect("renamed subject cataloged");
    assert_eq!(
        oid0, oid1,
        "object identity lost across rename storm (by_fileid + orphan grace)"
    );
    assert_eq!(rec.entry_paths, vec![cur.to_string_lossy().into_owned()]);
    pipeline.stop();
}

#[test]
fn dir_move_in_and_out_converges() {
    // A directory tree moved INTO the watched root produces one event for the
    // dir; children must be discovered by the unknown-dir recursive escalation.
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("tree");
    let outside = base.join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    gen_tree(&outside, 120, 51).unwrap();

    let cat =
        Arc::new(Catalog::open_with_durability(&base.join("cat.redb"), Durability::None).unwrap());
    let (pipeline, _) = bring_up(cat.clone(), &root, fast_cfg()).unwrap();

    // Move in.
    let moved_in = root.join("imported");
    std::fs::rename(&outside, &moved_in).unwrap();
    wait_converged(&cat, &root, Duration::from_secs(30));
    cat.check_invariants().unwrap();
    assert_eq!(
        pipeline.counters.full_rescans.load(Ordering::Relaxed),
        0,
        "dir move-in must converge without a full rescan"
    );

    // Move out again: the whole subtree must vanish from the catalog.
    std::fs::rename(&moved_in, base.join("evicted")).unwrap();
    wait_converged(&cat, &root, Duration::from_secs(30));
    cat.check_invariants().unwrap();
    pipeline.stop();
}

/// Helper used by tests only; keeps the Pipeline type exercised via the public
/// API surface (bring_up + counters + stop).
#[allow(dead_code)]
fn _types(p: Pipeline) {
    p.stop();
}
