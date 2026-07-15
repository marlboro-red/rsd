//! The committer (P2.4): the single writer that owns the per-LSN projection
//! state machine — journal-before-apply, watermark advance atomic with the
//! batch, idempotent replay (DESIGN.md §7.3–7.4).
//!
//! Performance: one group-committed journal write (single syscall, optional
//! single fsync) + one catalog transaction per batch. The catalog runs without
//! fsync — redb's shadow paging keeps it crash-atomic, and the journal is the
//! durability point.

use rsd_catalog::{Catalog, Change};
use rsd_log::{Journal, LogRecord, Source};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("journal: {0}")]
    Journal(#[from] rsd_log::LogError),
    #[error("catalog: {0}")]
    Catalog(#[from] rsd_catalog::CatalogError),
    #[error("commit invariant violated: {0}")]
    Invariant(String),
}

pub type Result<T> = std::result::Result<T, CommitError>;

type OnCommit = Box<dyn FnMut(&[rsd_catalog::Delta]) + Send>;

pub struct Committer {
    catalog: Arc<Catalog>,
    journal: Journal,
    lexical: Option<(rsd_lexical::LexicalPlane, Arc<rsd_caes::Store>)>,
    vector: Option<(
        Arc<std::sync::Mutex<rsd_vector::VectorPlane>>,
        Arc<rsd_caes::Store>,
    )>,
    on_commit: Option<OnCommit>,
}

impl Committer {
    pub fn new(catalog: Arc<Catalog>, journal: Journal) -> Committer {
        Committer {
            catalog,
            journal,
            lexical: None,
            vector: None,
            on_commit: None,
        }
    }

    /// Attach the semantic plane projection (P6.2).
    pub fn with_vector(
        mut self,
        plane: Arc<std::sync::Mutex<rsd_vector::VectorPlane>>,
        caes: Arc<rsd_caes::Store>,
    ) -> Committer {
        self.vector = Some((plane, caes));
        self
    }

    /// Hook receiving every committed batch's deltas (live-view engine).
    pub fn set_on_commit(&mut self, f: OnCommit) {
        self.on_commit = Some(f);
    }

    /// Attach the lexical plane projection; applied after the catalog on every
    /// commit, recovered from its own watermark.
    pub fn with_lexical(
        mut self,
        plane: rsd_lexical::LexicalPlane,
        caes: Arc<rsd_caes::Store>,
    ) -> Committer {
        self.lexical = Some((plane, caes));
        self
    }

    pub fn lexical(&self) -> Option<&rsd_lexical::LexicalPlane> {
        self.lexical.as_ref().map(|(p, _)| p)
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Journal the batch (durable per journal config), then apply it to the
    /// catalog under the watermark. Crash anywhere: recovery replays the
    /// journal delta; double-apply is a watermark no-op.
    pub fn commit(&mut self, source: Source, changes: &[Change]) -> Result<Option<(u64, u64)>> {
        if changes.is_empty() {
            return Ok(None);
        }
        let (first, last) = self.journal.append(source, changes)?;
        let deltas = self.catalog.apply_changes(first, changes)?;
        if let Some((plane, caes)) = self.lexical.as_mut() {
            if let Err(e) = plane.apply(first, changes, &self.catalog, caes) {
                tracing::error!("lexical apply failed (plane lags, rebuildable): {e}");
            }
        }
        if let Some((plane, caes)) = self.vector.as_ref() {
            if let Err(e) = plane
                .lock()
                .unwrap()
                .apply(first, changes, &self.catalog, caes)
            {
                tracing::error!("vector apply failed (plane lags, rebuildable): {e}");
            }
        }
        if let Some(hook) = self.on_commit.as_mut() {
            hook(&deltas);
        }
        Ok(Some((first, last)))
    }

    /// Bring the catalog projection up to the journal: replay every record past
    /// the catalog's watermark, in chunks. Returns the number replayed.
    pub fn recover(&mut self) -> Result<u64> {
        let applied = self.catalog.applied_lsn()?;
        let max = self.journal.max_lsn();
        if applied > max {
            // Catalog claims state the journal never issued: journal loss or
            // foreign catalog — failure-matrix repair, never silent.
            return Err(CommitError::Invariant(format!(
                "catalog watermark {applied} exceeds journal max lsn {max}"
            )));
        }
        if applied == max {
            return Ok(0);
        }
        let mut pending: Vec<LogRecord> = Vec::new();
        self.journal.replay(applied + 1, |rec| pending.push(rec))?;
        let replayed = pending.len() as u64;
        for chunk in pending.chunks(1024) {
            let first = chunk[0].lsn;
            let changes: Vec<Change> = chunk.iter().map(|r| r.change.clone()).collect();
            self.catalog.apply_changes(first, &changes)?;
        }
        Ok(replayed)
    }

    pub fn journal_max_lsn(&self) -> u64 {
        self.journal.max_lsn()
    }
}

/// Deterministic synthetic workload shared by the crash-injection harness and
/// its child binary. Ops are absolute (full-stat upserts / removals) so any
/// re-delivered suffix replays to the same state.
#[doc(hidden)]
pub mod synth {
    use rsd_catalog::{Change, EntrySummary, FileId, ObjectKind, StatInfo};
    use std::collections::BTreeMap;

    pub const PATHS: u64 = 64;

    fn path_index(i: u64) -> u64 {
        (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 33) % PATHS
    }

    fn is_remove(i: u64) -> bool {
        (i.wrapping_mul(0xC2B2_AE3D_27D4_EB4F) >> 41).is_multiple_of(5)
    }

    pub fn change(i: u64) -> Change {
        let j = path_index(i);
        let path = format!("/v/p{j:02}");
        if is_remove(i) {
            Change::RemovePath { path }
        } else {
            Change::Upsert {
                path,
                stat: StatInfo {
                    kind: ObjectKind::File,
                    file_id: FileId { dev: 1, ino: j + 1 },
                    size: i,
                    mtime_ns: i as i64,
                    birthtime_ns: ((j + 1) * 1_000) as i64,
                    nlink: 1,
                },
            }
        }
    }

    /// Final state after ops [0, ops) — the never-crashed reference.
    pub fn expected(ops: u64) -> BTreeMap<String, EntrySummary> {
        let mut state: BTreeMap<String, EntrySummary> = BTreeMap::new();
        for i in 0..ops {
            match change(i) {
                Change::Upsert { path, stat } => {
                    state.insert(
                        path,
                        EntrySummary {
                            kind: stat.kind,
                            ino: stat.file_id.ino,
                            size: stat.size,
                        },
                    );
                }
                Change::RemovePath { path } => {
                    state.remove(&path);
                }
                Change::SetContent { .. } => unreachable!("synth emits no SetContent"),
            }
        }
        state
    }
}
