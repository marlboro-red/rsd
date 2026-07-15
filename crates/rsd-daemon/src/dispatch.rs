//! The content dispatcher (P3.3): routes changed files through
//! CAES-check → extraction → journaled `SetContent`, with crash quarantine.
//!
//! Performance discipline: the expensive thing (parsing) is behind two cheap
//! gates — (1) unchanged (size, mtime) on an already-indexed object skips
//! everything, including the read; (2) a blake3 hash + CAES lookup turns
//! copies and re-observations into store hits. Extractors run only on
//! genuinely novel content.

use crate::commit::Committer;
use rsd_caes::{CaesError, CaesKey, ExtractStatus, ExtractionRecord, Store, ABI_VERSION};
use rsd_catalog::{Change, ObjectKind, StatInfo};
use rsd_extract::{Budgets, ExtractHints, EXTRACTOR_ID, EXTRACTOR_VERSION};
use rsd_log::Source;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// How many extraction failures (crashes/hangs) a piece of content gets
/// before it is quarantined.
const QUARANTINE_AFTER: u32 = 3;

/// The extraction backend. Production: `PooledExtractor` (sealed worker
/// processes). Tests: in-process counting sources.
pub trait ContentSource: Send {
    fn extract_file(
        &mut self,
        path: &Path,
        hints: &ExtractHints,
        budgets: &Budgets,
    ) -> Result<ExtractionRecord, String>;
}

/// Sealed worker pool as a content source.
pub struct PooledExtractor(pub rsd_worker::WorkerPool);

impl ContentSource for PooledExtractor {
    fn extract_file(
        &mut self,
        path: &Path,
        hints: &ExtractHints,
        budgets: &Budgets,
    ) -> Result<ExtractionRecord, String> {
        self.0
            .extract(path, hints.clone(), *budgets)
            .map_err(|e| e.to_string())
    }
}

#[derive(Debug, Default)]
pub struct ContentCounters {
    pub extractions: AtomicU64,
    pub caes_hits: AtomicU64,
    pub skipped_unchanged: AtomicU64,
    pub quarantined: AtomicU64,
    pub failures: AtomicU64,
}

pub struct ContentIndexer {
    source: Box<dyn ContentSource>,
    caes: Arc<Store>,
    budgets: Budgets,
    failures: HashMap<[u8; 32], u32>,
    pub counters: Arc<ContentCounters>,
}

/// Streaming blake3 up to `cap` bytes. Returns (hash, full_size, truncated).
fn hash_file(path: &Path, cap: u64) -> std::io::Result<([u8; 32], u64, bool)> {
    let file = std::fs::File::open(path)?;
    let full_size = file.metadata()?.len();
    let mut hasher = blake3::Hasher::new();
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file.take(cap));
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok((*hasher.finalize().as_bytes(), full_size, full_size > cap))
}

impl ContentIndexer {
    pub fn new(source: Box<dyn ContentSource>, caes: Arc<Store>) -> ContentIndexer {
        ContentIndexer {
            source,
            caes,
            budgets: Budgets::default(),
            failures: HashMap::new(),
            counters: Arc::new(ContentCounters::default()),
        }
    }

    /// Content-index the file upserts of a just-committed batch, journaling
    /// one `SetContent` batch for whatever resolved.
    pub fn process(&mut self, committer: &mut Committer, upserts: &[(String, StatInfo)]) {
        let mut out: Vec<Change> = Vec::new();
        for (path, stat) in upserts {
            if stat.kind != ObjectKind::File {
                continue;
            }
            match self.process_one(committer, path, stat) {
                Ok(Some(ch)) => out.push(ch),
                Ok(None) => {}
                Err(e) => tracing::warn!("content indexing {path:?} failed: {e}"),
            }
        }
        if !out.is_empty() {
            if let Err(e) = committer.commit(Source::Content, &out) {
                tracing::error!("SetContent commit failed: {e}");
            }
        }
    }

    fn process_one(
        &mut self,
        committer: &Committer,
        path: &str,
        stat: &StatInfo,
    ) -> Result<Option<Change>, String> {
        // Gate 1: unchanged stats on an already-indexed object => free skip.
        if let Ok(Some((_, rec))) = committer.catalog().get_by_fileid(stat.file_id) {
            if rec.size == stat.size && rec.mtime_ns == stat.mtime_ns && rec.index_state.is_some() {
                self.counters
                    .skipped_unchanged
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(None);
            }
        }

        // Gate 2: content identity => CAES.
        let p = Path::new(path);
        let (content_hash, full_size, truncated) =
            hash_file(p, self.budgets.max_input_bytes).map_err(|e| e.to_string())?;
        let hints = ExtractHints {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            full_size,
        };
        let key = CaesKey {
            content_hash,
            extractor_id: EXTRACTOR_ID.into(),
            extractor_version: EXTRACTOR_VERSION,
            hints_hash: hints.hints_hash(truncated),
            abi_version: ABI_VERSION,
        };

        let status = match self.caes.get(&key) {
            Ok(Some(rec)) => {
                self.counters.caes_hits.fetch_add(1, Ordering::Relaxed);
                rec.status
            }
            Ok(None) | Err(CaesError::Corrupt { .. }) => {
                if matches!(self.caes.get(&key), Err(CaesError::Corrupt { .. })) {
                    let _ = self.caes.evict(&key);
                }
                match self.source.extract_file(p, &hints, &self.budgets) {
                    Ok(rec) => {
                        self.caes.put(&key, &rec).map_err(|e| e.to_string())?;
                        self.counters.extractions.fetch_add(1, Ordering::Relaxed);
                        self.failures.remove(&content_hash);
                        rec.status
                    }
                    Err(reason) => {
                        self.counters.failures.fetch_add(1, Ordering::Relaxed);
                        let n = self.failures.entry(content_hash).or_insert(0);
                        *n += 1;
                        if *n < QUARANTINE_AFTER {
                            // Leave index_state unset: the next event or scan
                            // retriggers a retry.
                            return Ok(None);
                        }
                        // Quarantine: recorded in CAES so identical content is
                        // never blindly retried, reason queryable.
                        let qrec = ExtractionRecord {
                            status: ExtractStatus::Quarantined,
                            text: String::new(),
                            attrs: vec![("rsd.quarantine_reason".into(), reason)],
                            symbols: vec![],
                        };
                        self.caes.put(&key, &qrec).map_err(|e| e.to_string())?;
                        self.counters.quarantined.fetch_add(1, Ordering::Relaxed);
                        self.failures.remove(&content_hash);
                        ExtractStatus::Quarantined
                    }
                }
            }
            Err(e) => return Err(e.to_string()),
        };

        Ok(Some(Change::SetContent {
            path: path.to_string(),
            content_hash,
            state: status.as_str().to_string(),
        }))
    }
}
