//! P1.6: the end-to-end convergence harness — the permanent CI gate.
//! Phase 2: the pipeline now journals every batch (journal-before-apply), so
//! this harness also proves the fenced commit path converges live.
//!
//! Criteria:
//!  - live mutation storm converges with ZERO full rescans (events only);
//!  - watcher-channel overflow degrades to a counted root rescan, self-healed
//!    on the applier thread, and converges;
//!  - rename storms preserve object identity via by_fileid + orphan grace;
//!  - a directory tree moved into the root is discovered recursively.

use rsd_catalog::{Catalog, Durability};
use rsd_daemon::{bring_up, PipelineConfig};
use rsd_ingest::CoalescerConfig;
use rsd_log::CursorStore;
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
        journal_sync: false,
        ..Default::default()
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    journal_dir: PathBuf,
    catalog_path: PathBuf,
    cat: Arc<Catalog>,
}

fn setup(files: usize, seed: u64) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("tree");
    std::fs::create_dir(&root).unwrap();
    if files > 0 {
        gen_tree(&root, files, seed).unwrap();
    }
    let catalog_path = base.join("cat.redb");
    let cat = Arc::new(Catalog::open_with_durability(&catalog_path, Durability::None).unwrap());
    Env {
        _tmp: tmp,
        journal_dir: base.join("journal"),
        catalog_path,
        root,
        cat,
    }
}

#[test]
fn restart_reconciles_mutations_that_happened_while_stopped() {
    let env = setup(80, 61);
    let (pipeline, _) = bring_up(
        env.cat.clone(),
        &env.journal_dir,
        &env.root,
        None,
        None,
        None,
        None,
        fast_cfg(),
    )
    .unwrap();
    assert_converged(&env.cat, &env.root);
    pipeline.stop();

    std::fs::write(env.root.join("created-while-down.txt"), "new").unwrap();
    let existing = env
        .cat
        .listing()
        .unwrap()
        .keys()
        .find(|path| Path::new(path).is_file())
        .cloned()
        .expect("seed file");
    std::fs::remove_file(existing).unwrap();
    drop(env.cat);

    // Reopen the actual persistent catalog and pipeline. Blocking bootstrap
    // must close the watcher downtime gap before bring_up returns.
    let catalog =
        Arc::new(Catalog::open_with_durability(&env.catalog_path, Durability::None).unwrap());
    let (pipeline, _) = bring_up(
        catalog.clone(),
        &env.journal_dir,
        &env.root,
        None,
        None,
        None,
        None,
        fast_cfg(),
    )
    .unwrap();
    assert_converged(&catalog, &env.root);
    catalog.check_invariants().unwrap();
    pipeline.stop();
}

#[test]
fn live_events_advance_the_durable_fsevents_cursor() {
    let env = setup(0, 62);
    let (pipeline, _) = bring_up(
        env.cat.clone(),
        &env.journal_dir,
        &env.root,
        None,
        None,
        None,
        None,
        fast_cfg(),
    )
    .unwrap();
    std::fs::write(env.root.join("cursor-proof.txt"), "durable event").unwrap();
    wait_converged(&env.cat, &env.root, Duration::from_secs(15));
    pipeline.stop();

    let cursor = CursorStore::new(&env.journal_dir.join("fsevents.cursor"));
    assert!(
        cursor.get().unwrap().is_some(),
        "drained live work must persist its FSEvents fence"
    );
}

#[test]
fn scoped_journal_corruption_repairs_current_paths() {
    let env = setup(12, 63);
    let mut cfg = fast_cfg();
    cfg.journal_sync = true;
    let (pipeline, _) = bring_up(
        env.cat.clone(),
        &env.journal_dir,
        &env.root,
        None,
        None,
        None,
        None,
        cfg,
    )
    .unwrap();
    assert_converged(&env.cat, &env.root);
    pipeline.stop();

    let removed = env
        .cat
        .listing()
        .unwrap()
        .keys()
        .find(|path| Path::new(path).is_file())
        .cloned()
        .unwrap();
    std::fs::remove_file(&removed).unwrap();

    let segment = std::fs::read_dir(&env.journal_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "rlog"))
        .unwrap();
    let mut bytes = std::fs::read(&segment).unwrap();
    bytes[8 + 1 + 4 + 16 + 2] ^= 0x80;
    std::fs::write(&segment, bytes).unwrap();

    let mut cfg = fast_cfg();
    cfg.journal_sync = true;
    let (pipeline, _) = bring_up(
        env.cat.clone(),
        &env.journal_dir,
        &env.root,
        None,
        None,
        None,
        None,
        cfg,
    )
    .unwrap();
    assert_converged(&env.cat, &env.root);
    assert!(env.cat.get_by_path(&removed).unwrap().is_none());
    assert!(std::fs::read_dir(&env.journal_dir)
        .unwrap()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-")));
    pipeline.stop();
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
    let env = setup(800, 21);
    let (pipeline, _) = bring_up(
        env.cat.clone(),
        &env.journal_dir,
        &env.root,
        None,
        None,
        None,
        None,
        fast_cfg(),
    )
    .unwrap();
    assert_converged(&env.cat, &env.root);

    let mut m = Mutator::new(&env.root, 22).unwrap();
    m.run(500).unwrap();

    wait_converged(&env.cat, &env.root, Duration::from_secs(30));
    env.cat.check_invariants().unwrap();

    assert_eq!(
        pipeline.counters.full_rescans.load(Ordering::Relaxed),
        0,
        "converged only via full rescan — event path is broken"
    );
    assert!(pipeline.counters.work_items.load(Ordering::Relaxed) > 0);
    assert!(pipeline.counters.commits.load(Ordering::Relaxed) > 0);
    pipeline.stop();
}

#[test]
fn overflow_self_heals_with_counted_root_rescan() {
    let env = setup(50, 31);
    let cfg = PipelineConfig {
        event_capacity: 8, // force the callback to shed events
        ..fast_cfg()
    };
    let (pipeline, _) = bring_up(
        env.cat.clone(),
        &env.journal_dir,
        &env.root,
        None,
        None,
        None,
        None,
        cfg,
    )
    .unwrap();

    for i in 0..400 {
        std::fs::write(env.root.join(format!("flood{i}.txt")), "x").unwrap();
    }

    // The applier notices the shed flag and reconciles on its own thread.
    let start = Instant::now();
    while pipeline.counters.full_rescans.load(Ordering::Relaxed) == 0
        && start.elapsed() < Duration::from_secs(15)
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        pipeline.counters.full_rescans.load(Ordering::Relaxed) >= 1,
        "overflow must be recovered by a counted rescan"
    );
    wait_converged(&env.cat, &env.root, Duration::from_secs(30));
    env.cat.check_invariants().unwrap();
    pipeline.stop();
}

#[test]
fn rename_storm_preserves_object_identity() {
    let env = setup(50, 41);
    let (pipeline, _) = bring_up(
        env.cat.clone(),
        &env.journal_dir,
        &env.root,
        None,
        None,
        None,
        None,
        fast_cfg(),
    )
    .unwrap();
    assert_converged(&env.cat, &env.root);

    // Track one file through a storm of renames.
    let subject = env.root.join("subject.bin");
    std::fs::write(&subject, "identity").unwrap();
    wait_converged(&env.cat, &env.root, Duration::from_secs(10));
    let (oid0, _) = env
        .cat
        .get_by_path(&subject.to_string_lossy())
        .unwrap()
        .expect("subject cataloged");

    let mut cur = subject.clone();
    for i in 0..6 {
        let next = env.root.join(format!("subject-r{i}.bin"));
        std::fs::rename(&cur, &next).unwrap();
        cur = next;
        std::thread::sleep(Duration::from_millis(120));
    }

    wait_converged(&env.cat, &env.root, Duration::from_secs(20));
    env.cat.check_invariants().unwrap();
    let (oid1, rec) = env
        .cat
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
    let env = setup(0, 51);
    let base = env.root.parent().unwrap().to_path_buf();
    let outside = base.join("outside");
    std::fs::create_dir(&outside).unwrap();
    gen_tree(&outside, 120, 51).unwrap();

    let (pipeline, _) = bring_up(
        env.cat.clone(),
        &env.journal_dir,
        &env.root,
        None,
        None,
        None,
        None,
        fast_cfg(),
    )
    .unwrap();

    // Move in.
    let moved_in = env.root.join("imported");
    std::fs::rename(&outside, &moved_in).unwrap();
    wait_converged(&env.cat, &env.root, Duration::from_secs(30));
    env.cat.check_invariants().unwrap();
    assert_eq!(
        pipeline.counters.full_rescans.load(Ordering::Relaxed),
        0,
        "dir move-in must converge without a full rescan"
    );

    // Move out again: the whole subtree must vanish from the catalog.
    std::fs::rename(&moved_in, base.join("evicted")).unwrap();
    wait_converged(&env.cat, &env.root, Duration::from_secs(30));
    env.cat.check_invariants().unwrap();
    pipeline.stop();
}
