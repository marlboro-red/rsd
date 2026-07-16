//! P1.3 success criteria: bootstrap convergence, post-storm rescan convergence,
//! and scoped-rescan locality.

use rsd_catalog::{Catalog, Durability};
use rsd_ingest::{bootstrap, rescan};
use rsd_testkit::{assert_converged, gen_tree, Mutator};
use std::path::PathBuf;

fn setup(files: usize, seed: u64) -> (tempfile::TempDir, PathBuf, Catalog, usize) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("tree");
    std::fs::create_dir(&root).unwrap();
    let nodes = gen_tree(&root, files, seed).unwrap();
    let cat =
        Catalog::open_with_durability(&tmp.path().join("cat.redb"), Durability::None).unwrap();
    (tmp, root, cat, nodes)
}

#[test]
fn bootstrap_converges_on_3000_node_tree() {
    let (_tmp, root, cat, nodes) = setup(2_600, 7);
    assert!(nodes >= 2_900, "generator produced only {nodes} nodes");
    let stats = bootstrap(&cat, &root).unwrap();
    assert!(stats.dirs_read > 0 && stats.upserts as usize >= nodes);
    assert_converged(&cat, &root);
    cat.check_invariants().unwrap();
}

#[test]
fn mutation_storm_then_recursive_rescan_converges() {
    let (_tmp, root, cat, _) = setup(2_600, 11);
    bootstrap(&cat, &root).unwrap();
    assert_converged(&cat, &root);

    let mut m = Mutator::new(&root, 13).unwrap();
    m.run(1_000).unwrap();
    assert_eq!(m.ops_applied, 1_000);

    rescan(&cat, &root, true).unwrap();
    assert_converged(&cat, &root);
    cat.check_invariants().unwrap();
}

#[test]
fn scoped_rescan_touches_only_the_requested_subtree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("tree");
    std::fs::create_dir(&root).unwrap();
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    gen_tree(&a, 200, 3).unwrap();
    gen_tree(&b, 200, 4).unwrap();

    let cat =
        Catalog::open_with_durability(&tmp.path().join("cat.redb"), Durability::None).unwrap();
    bootstrap(&cat, &root).unwrap();
    assert_converged(&cat, &root);

    // Mutate only under `a`, and also make catalog's view of `b` stale by
    // mutating `b` — a scoped rescan of `a` must not repair (or even read) `b`.
    let mut ma = Mutator::new(&a, 5).unwrap();
    ma.run(200).unwrap();
    std::fs::write(b.join("stale-marker.txt"), "x").unwrap();

    let before_b: Vec<String> = cat.subtree_paths(&b.to_string_lossy()).unwrap();
    let stats = rescan(&cat, &a, true).unwrap();
    let after_b: Vec<String> = cat.subtree_paths(&b.to_string_lossy()).unwrap();

    // Locality: b's catalog state untouched (still stale), and the dirs read
    // are bounded by a's subtree (+1 is impossible: root wasn't rescanned).
    assert_eq!(before_b, after_b);
    let dirs_under_a = 1 + rsd_testkit::fs_listing(&a)
        .unwrap()
        .values()
        .filter(|n| n.kind == rsd_catalog::ObjectKind::Dir)
        .count() as u64;
    assert!(
        stats.dirs_read <= dirs_under_a,
        "read {} dirs, subtree has {}",
        stats.dirs_read,
        dirs_under_a
    );
    assert_converged(&cat, &a);

    // The stale marker under b is then repaired by b's own scoped rescan.
    rescan(&cat, &b, true).unwrap();
    assert_converged(&cat, &root);
}

#[test]
fn excluded_dirs_never_enter_the_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("tree");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("node_modules/dep")).unwrap();
    std::fs::create_dir_all(root.join(".git/objects")).unwrap();
    std::fs::create_dir_all(root.join("Library/Caches")).unwrap();
    std::fs::write(root.join("src/keep.rs"), "fn keep() {}").unwrap();
    std::fs::write(root.join("node_modules/dep/skip.js"), "x").unwrap();
    std::fs::write(root.join(".git/objects/skip"), "x").unwrap();
    std::fs::write(root.join("Library/Caches/skip.db"), "x").unwrap();

    let cat =
        Catalog::open_with_durability(&tmp.path().join("cat.redb"), Durability::None).unwrap();
    bootstrap(&cat, &root).unwrap();
    let paths: Vec<String> = cat.listing().unwrap().into_keys().collect();
    assert!(paths.iter().any(|p| p.ends_with("keep.rs")));
    assert!(
        !paths
            .iter()
            .any(|p| p.contains("node_modules") || p.contains(".git") || p.contains("Library")),
        "{paths:?}"
    );
}
