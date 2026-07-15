//! P1.1 property test: 1,000 randomized op sequences (24k ops, one continuous
//! catalog) maintain the mirror invariants, and the catalog listing always
//! matches an in-memory reference model at every sequence boundary.

use rand::prelude::*;
use rand_chacha::ChaCha8Rng;
use rsd_catalog::{Applied, Catalog, EntrySummary, FileId, ObjectKind, StatInfo};
use std::collections::BTreeMap;

struct Model {
    /// path -> ino
    paths: BTreeMap<String, u64>,
    /// ino -> canonical stat used for links/renames
    stats: BTreeMap<u64, StatInfo>,
    next_ino: u64,
}

impl Model {
    fn new() -> Self {
        Model {
            paths: BTreeMap::new(),
            stats: BTreeMap::new(),
            next_ino: 1,
        }
    }

    fn link_count(&self, ino: u64) -> usize {
        self.paths.values().filter(|i| **i == ino).count()
    }

    fn expected_listing(&self) -> BTreeMap<String, EntrySummary> {
        self.paths
            .iter()
            .map(|(p, ino)| {
                let st = &self.stats[ino];
                (
                    p.clone(),
                    EntrySummary {
                        kind: st.kind,
                        ino: st.file_id.ino,
                        size: st.size,
                    },
                )
            })
            .collect()
    }
}

fn fresh_stat(m: &mut Model, kind: ObjectKind) -> StatInfo {
    let ino = m.next_ino;
    m.next_ino += 1;
    let st = StatInfo {
        kind,
        file_id: FileId { dev: 1, ino },
        size: ino % 977,
        mtime_ns: ino as i64,
        birthtime_ns: (ino as i64) * 1_000,
        nlink: 1,
    };
    m.stats.insert(ino, st);
    st
}

fn random_path(rng: &mut ChaCha8Rng) -> String {
    format!("/r/d{}/f{}", rng.gen_range(0..8), rng.gen_range(0..64))
}

#[test]
fn randomized_op_sequences_maintain_invariants() {
    const SEQUENCES: usize = 1_000;
    const OPS_PER_SEQ: usize = 24;

    let dir = tempfile::tempdir().unwrap();
    let cat =
        Catalog::open_with_durability(&dir.path().join("cat.redb"), redb::Durability::None)
            .unwrap();
    let mut m = Model::new();

    for seq in 0..SEQUENCES {
        let mut rng = ChaCha8Rng::seed_from_u64(seq as u64);

        for _ in 0..OPS_PER_SEQ {
            match rng.gen_range(0..100) {
                // Create/overwrite a path with a brand-new object.
                0..=39 => {
                    let p = random_path(&mut rng);
                    let st = fresh_stat(&mut m, ObjectKind::File);
                    cat.apply_stat(&p, &st).unwrap();
                    m.paths.insert(p, st.file_id.ino);
                }
                // Hard link: existing object gains a second path.
                40..=54 => {
                    if let Some((_, &ino)) = m.paths.iter().choose(&mut rng) {
                        let p2 = random_path(&mut rng);
                        let st = m.stats[&ino];
                        cat.apply_stat(&p2, &st).unwrap();
                        m.paths.insert(p2, ino);
                    }
                }
                // Rename: probe new path, then remove old (event ordering A).
                55..=69 => {
                    if let Some((p, &ino)) = m.paths.iter().choose(&mut rng) {
                        let p = p.clone();
                        let p2 = random_path(&mut rng);
                        if p2 != p {
                            let st = m.stats[&ino];
                            cat.apply_stat(&p2, &st).unwrap();
                            cat.remove_path(&p).unwrap();
                            m.paths.remove(&p);
                            m.paths.insert(p2, ino);
                        }
                    }
                }
                // Rename, reversed event ordering B: remove old, probe new.
                // Identity must survive orphan grace.
                70..=79 => {
                    if let Some((p, &ino)) = m.paths.iter().choose(&mut rng) {
                        let p = p.clone();
                        let p2 = random_path(&mut rng);
                        if p2 != p {
                            let st = m.stats[&ino];
                            let sole_link = m.link_count(ino) == 1;
                            let p2_already_same = m.paths.get(&p2) == Some(&ino);
                            cat.remove_path(&p).unwrap();
                            let a = cat.apply_stat(&p2, &st).unwrap();
                            m.paths.remove(&p);
                            m.paths.insert(p2, ino);
                            if sole_link && !p2_already_same {
                                assert!(
                                    matches!(a, Applied::RepointedPath(_)),
                                    "seq {seq}: orphan-grace rename lost identity: {a:?}"
                                );
                            }
                        }
                    }
                }
                // Remove a path.
                _ => {
                    if let Some((p, _)) = m.paths.iter().choose(&mut rng) {
                        let p = p.clone();
                        cat.remove_path(&p).unwrap();
                        m.paths.remove(&p);
                    }
                }
            }
        }

        cat.check_invariants()
            .unwrap_or_else(|e| panic!("seq {seq}: {e}"));
        let got = cat.listing().unwrap();
        let want = m.expected_listing();
        assert_eq!(got, want, "listing mismatch in seq {seq}");
    }
}
