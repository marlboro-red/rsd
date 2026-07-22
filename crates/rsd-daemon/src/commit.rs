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
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("journal: {0}")]
    Journal(#[from] rsd_log::LogError),
    #[error("catalog: {0}")]
    Catalog(#[from] rsd_catalog::CatalogError),
    #[error("lexical projection: {0}")]
    Lexical(#[from] rsd_lexical::LexicalError),
    #[error("vector projection: {0}")]
    Vector(#[from] rsd_vector::VectorError),
    #[error("commit invariant violated: {0}")]
    Invariant(String),
}

pub type Result<T> = std::result::Result<T, CommitError>;

type OnCommit = Box<dyn FnMut(&[rsd_catalog::Delta]) + Send>;

fn replay_in_batches(
    journal: &Journal,
    start_lsn: u64,
    mut apply: impl FnMut(u64, &[Change]) -> Result<()>,
) -> Result<u64> {
    let mut batch: Vec<LogRecord> = Vec::with_capacity(1024);
    let mut apply_error: Option<CommitError> = None;
    let mut replayed = 0u64;
    journal.replay(start_lsn, |record| {
        replayed += 1;
        if apply_error.is_some() {
            return;
        }
        batch.push(record);
        if batch.len() == 1024 {
            let first = batch[0].lsn;
            let changes: Vec<Change> = batch.drain(..).map(|r| r.change).collect();
            if let Err(error) = apply(first, &changes) {
                apply_error = Some(error);
            }
        }
    })?;
    if let Some(error) = apply_error {
        return Err(error);
    }
    if !batch.is_empty() {
        let first = batch[0].lsn;
        let changes: Vec<Change> = batch.drain(..).map(|r| r.change).collect();
        apply(first, &changes)?;
    }
    Ok(replayed)
}

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
        // SetContent is produced after hashing and extraction, so the file may
        // have changed since its catalog Upsert. Revalidate the filesystem
        // generation immediately before journaling; raced results are
        // discarded and the watcher/bootstrap will retry the newer stat.
        let content_changes;
        let changes = if source == Source::Content {
            content_changes = changes
                .iter()
                .filter(|change| match change {
                    Change::SetContent { path, .. } => self.content_target_is_current(path),
                    _ => true,
                })
                .cloned()
                .collect::<Vec<_>>();
            content_changes.as_slice()
        } else {
            changes
        };
        if changes.is_empty() {
            return Ok(None);
        }
        let t_commit = std::time::Instant::now();
        let (first, last) = self.journal.append(source, changes)?;
        let deltas = self.catalog.apply_changes(first, changes)?;
        let remove_oids = self.projection_removals(&deltas)?;
        let refresh_oids = projection_updates(&deltas);
        if let Some((plane, caes)) = self.lexical.as_mut() {
            let apply = if plane.applied_lsn().saturating_add(1) < first {
                plane.rebuild_current(last, &self.catalog, caes)
            } else {
                plane.apply(
                    first,
                    changes,
                    &remove_oids,
                    &refresh_oids,
                    &self.catalog,
                    caes,
                )
            };
            if let Err(error) = apply {
                tracing::error!("lexical apply failed; rebuilding projection: {error}");
                plane.rebuild_current(self.journal.max_lsn(), &self.catalog, caes)?;
            }
        }
        if let Some((plane, caes)) = self.vector.as_ref() {
            let mut plane = plane.lock().unwrap_or_else(|e| e.into_inner());
            let apply = if plane.applied_lsn().saturating_add(1) < first {
                plane.rebuild_current(last, &self.catalog, caes)
            } else {
                plane.apply(first, changes, &remove_oids, &self.catalog, caes)
            };
            if let Err(error) = apply {
                tracing::error!("vector apply failed; rebuilding projection: {error}");
                plane.rebuild_current(self.journal.max_lsn(), &self.catalog, caes)?;
            }
        }
        if let Some(hook) = self.on_commit.as_mut() {
            hook(&deltas);
        }
        rsd_metrics::metrics()
            .commit_ms
            .record(t_commit.elapsed().as_secs_f64() * 1000.0);
        Ok(Some((first, last)))
    }

    fn content_target_is_current(&self, path: &str) -> bool {
        let catalog_record = match self.catalog.get_by_path(path) {
            Ok(Some((_, record))) => record,
            Ok(None) => return false,
            Err(error) => {
                tracing::warn!("content revalidation catalog lookup failed for {path:?}: {error}");
                return false;
            }
        };
        let current = match std::fs::symlink_metadata(path) {
            Ok(metadata) => rsd_catalog::StatInfo::from_metadata(&metadata),
            Err(error) => {
                tracing::debug!("content revalidation raced for {path:?}: {error}");
                return false;
            }
        };
        let matches = current.kind == catalog_record.kind
            && current.file_id == catalog_record.file_id
            && current.birthtime_ns == catalog_record.birthtime_ns
            && current.size == catalog_record.size
            && current.mtime_ns == catalog_record.mtime_ns;
        if !matches {
            tracing::debug!("discarding raced SetContent for {path:?}");
        }
        matches
    }

    fn projection_removals(&self, deltas: &[rsd_catalog::Delta]) -> Result<Vec<u64>> {
        let mut removals = HashSet::new();
        for delta in deltas {
            if let Some((oid, record)) = &delta.new {
                if record.content_hash.is_none() {
                    removals.insert(*oid);
                }
            }
            if let Some((old_oid, _)) = &delta.old {
                let still_same_object = delta
                    .new
                    .as_ref()
                    .is_some_and(|(new_oid, _)| new_oid == old_oid);
                if !still_same_object {
                    // Keep pathless objects projected during orphan grace so a
                    // rename can rebind without a content-indexing gap. Sweep
                    // removes genuinely dead oids from every plane later.
                    if self.catalog.get_object(*old_oid)?.is_none() {
                        removals.insert(*old_oid);
                    }
                }
            }
        }
        Ok(removals.into_iter().collect())
    }

    /// Reclaim orphaned identities: clear their documents out of every
    /// projection, then let the catalog forget them.
    ///
    /// The order is the whole point. Projection deletes commit with an
    /// unchanged watermark — they carry no LSN of their own — so `recover()`
    /// cannot tell "swept" from "never swept" by watermark comparison. If the
    /// catalog forgot the object first, a crash before the projection commit
    /// would strand documents under an oid nothing can name again, and
    /// recovery would see every watermark equal to the journal max and rebuild
    /// nothing. Sweeping projections first inverts that: a crash leaves the
    /// catalog still naming the orphan, so the next sweep retries, and the
    /// deletes are idempotent. Races become retries (DESIGN.md §6.8).
    ///
    /// An orphan has no entry paths, so its documents are already unreachable
    /// by query — removing them before the catalog record costs no visibility.
    pub fn sweep_orphans(&mut self, grace: std::time::Duration) -> Result<usize> {
        let victims = self.catalog.orphan_oids(grace)?;
        if victims.is_empty() {
            return Ok(0);
        }
        self.remove_from_planes(&victims)?;
        Ok(self.catalog.remove_orphans(&victims, grace)?.len())
    }

    /// Evict oids from every disposable projection.
    ///
    /// Idempotent: deleting an oid no plane holds is a no-op, which is what
    /// makes a retried sweep safe. Public so a test can stop between the two
    /// halves of a sweep — the crash point this ordering exists to survive.
    pub fn remove_from_planes(&mut self, victims: &[u64]) -> Result<()> {
        if let Some((plane, _)) = self.lexical.as_mut() {
            plane.remove_oids(victims)?;
        }
        if let Some((plane, _)) = self.vector.as_ref() {
            plane
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove_many(victims)?;
        }
        Ok(())
    }

    /// Bring every attached projection up to the journal. Catalog replay is
    /// streamed in bounded windows. A lagging disposable content plane is
    /// rebuilt from journal + CAES so historical deletions cannot survive.
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
        let replayed = if applied < max {
            replay_in_batches(&self.journal, applied + 1, |first, changes| {
                self.catalog.apply_changes(first, changes)?;
                Ok(())
            })?
        } else {
            0
        };
        if replayed > 0 {
            rsd_metrics::metrics().journal_replays.add(replayed);
        }

        if let Some((plane, caes)) = self.lexical.as_mut() {
            if plane.applied_lsn() != max {
                tracing::warn!(
                    "lexical watermark {} behind journal {max}; rebuilding from journal + CAES",
                    plane.applied_lsn()
                );
                plane.rebuild_current(max, &self.catalog, caes)?;
            }
        }
        if let Some((plane, caes)) = self.vector.as_ref() {
            let mut plane = plane.lock().unwrap_or_else(|e| e.into_inner());
            if plane.applied_lsn() != max {
                tracing::warn!(
                    "vector watermark {} behind journal {max}; rebuilding from journal + CAES",
                    plane.applied_lsn()
                );
                plane.rebuild_current(max, &self.catalog, caes)?;
            }
        }
        Ok(replayed)
    }

    pub fn journal_max_lsn(&self) -> u64 {
        self.journal.max_lsn()
    }
}

fn projection_updates(deltas: &[rsd_catalog::Delta]) -> Vec<u64> {
    let mut updates = HashSet::new();
    for delta in deltas {
        if let Some((oid, _)) = &delta.old {
            updates.insert(*oid);
        }
        if let Some((oid, _)) = &delta.new {
            updates.insert(*oid);
        }
    }
    updates.into_iter().collect()
}

/// Deterministic synthetic workload shared by the crash-injection harness and
/// its child binary. Ops are absolute (full-stat upserts / removals) so any
/// re-delivered suffix replays to the same state.
#[doc(hidden)]
pub mod synth {
    use rsd_caes::{CaesKey, ExtractStatus, ExtractionRecord, ABI_VERSION};
    use rsd_catalog::{Change, EntrySummary, FileId, ObjectKind, StatInfo};
    use rsd_extract::{EXTRACTOR_ID, EXTRACTOR_VERSION};
    use std::collections::BTreeMap;

    pub const PATHS: u64 = 64;

    fn path_index(i: u64) -> u64 {
        (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 33) % PATHS
    }

    fn is_remove(i: u64) -> bool {
        (i.wrapping_mul(0xC2B2_AE3D_27D4_EB4F) >> 41).is_multiple_of(5)
    }

    fn is_content(i: u64) -> bool {
        (i.wrapping_mul(0x1656_67B1_9E37_79F9) >> 39).is_multiple_of(3)
    }

    pub fn caes_entry(i: u64) -> Option<(CaesKey, ExtractionRecord)> {
        if is_remove(i) || !is_content(i) {
            return None;
        }
        let j = path_index(i);
        let text = format!("synthetic projection content path-{j:02}");
        let content_hash = *blake3::hash(text.as_bytes()).as_bytes();
        let hints_hash = *blake3::hash(format!("synth-hints-{j:02}").as_bytes()).as_bytes();
        Some((
            CaesKey {
                content_hash,
                extractor_id: EXTRACTOR_ID.into(),
                extractor_version: EXTRACTOR_VERSION,
                hints_hash,
                abi_version: ABI_VERSION,
            },
            ExtractionRecord {
                status: ExtractStatus::Complete,
                text,
                attrs: vec![],
                symbols: vec![],
            },
        ))
    }

    pub fn change(i: u64) -> Change {
        let j = path_index(i);
        let path = format!("/v/p{j:02}");
        if is_remove(i) {
            Change::RemovePath { path }
        } else if let Some((key, _)) = caes_entry(i) {
            Change::SetContent {
                path,
                content_hash: key.content_hash,
                hints_hash: key.hints_hash,
                state: "complete".into(),
            }
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
                Change::SetContent { .. } => {}
            }
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsd_catalog::{Durability, StatInfo};
    use rsd_log::JournalConfig;

    #[test]
    fn raced_set_content_is_not_journaled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raced.txt");
        std::fs::write(&path, "before").unwrap();
        let stat = StatInfo::from_metadata(&std::fs::symlink_metadata(&path).unwrap());
        let catalog = Arc::new(
            Catalog::open_with_durability(&dir.path().join("catalog.redb"), Durability::None)
                .unwrap(),
        );
        let journal = Journal::open(
            &dir.path().join("journal"),
            JournalConfig {
                sync_on_append: false,
                ..Default::default()
            },
        )
        .unwrap();
        let mut committer = Committer::new(catalog.clone(), journal);
        let path = path.to_string_lossy().into_owned();
        committer
            .commit(
                Source::Scan,
                &[Change::Upsert {
                    path: path.clone(),
                    stat,
                }],
            )
            .unwrap();

        std::fs::write(&path, "after with a different size").unwrap();
        let committed = committer
            .commit(
                Source::Content,
                &[Change::SetContent {
                    path: path.clone(),
                    content_hash: [1u8; 32],
                    hints_hash: [2u8; 32],
                    state: "complete".into(),
                }],
            )
            .unwrap();
        assert_eq!(committed, None);
        assert_eq!(committer.journal_max_lsn(), 1);
        assert!(catalog
            .get_by_path(&path)
            .unwrap()
            .is_some_and(|(_, record)| record.index_state.is_none()));
    }
}
