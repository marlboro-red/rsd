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
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// How many extraction failures (crashes/hangs) a piece of content gets
/// before it is quarantined.
const QUARANTINE_AFTER: u32 = 3;
const RETRY_COUNT_ATTR: &str = "rsd.retryable_failure_count";
const QUARANTINE_REASON_ATTR: &str = "rsd.quarantine_reason";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessorKey {
    pub extractor_id: String,
    pub extractor_version: u32,
    pub hints_tag: String,
}

#[derive(Debug, Clone, Copy)]
enum RouteKind {
    Wasm,
    Ocr,
    Media,
    Native,
}

/// The extraction backend. Production: `PooledExtractor` (sealed worker
/// processes). Tests: in-process counting sources.
pub trait ContentSource: Send {
    fn extract_file(
        &mut self,
        file: &std::fs::File,
        path: &Path,
        hints: &ExtractHints,
        budgets: &Budgets,
    ) -> Result<ExtractionRecord, String>;

    /// Does this source claim `name`? Default: no (the base text/PDF source is
    /// the fallback, tried last).
    fn handles(&self, _name: &str) -> bool {
        false
    }

    /// Complete CAES processor identity for this routing decision.
    fn processor_key(&self, _name: &str) -> ProcessorKey {
        ProcessorKey {
            extractor_id: EXTRACTOR_ID.into(),
            extractor_version: EXTRACTOR_VERSION,
            hints_tag: String::new(),
        }
    }
}

/// Sealed worker pool as a content source.
pub struct PooledExtractor(pub rsd_worker::WorkerPool);

impl ContentSource for PooledExtractor {
    fn extract_file(
        &mut self,
        file: &std::fs::File,
        _path: &Path,
        hints: &ExtractHints,
        budgets: &Budgets,
    ) -> Result<ExtractionRecord, String> {
        self.0
            .extract_fd(
                file.try_clone().map_err(|error| error.to_string())?,
                hints.clone(),
                *budgets,
            )
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
    ocr: Option<Box<dyn ContentSource>>,
    wasm: Option<Box<dyn ContentSource>>,
    media: Option<Box<dyn ContentSource>>,
    caes: Arc<Store>,
    budgets: Budgets,
    pub counters: Arc<ContentCounters>,
}

/// Streaming whole-file blake3. Extraction may be budget-truncated, but the
/// CAES content identity never is.
fn hash_file(file: &mut std::fs::File) -> std::io::Result<([u8; 32], u64)> {
    let full_size = file.metadata()?.len();
    file.rewind()?;
    let mut hasher = blake3::Hasher::new();
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    reader.seek(std::io::SeekFrom::Start(0))?;
    Ok((*hasher.finalize().as_bytes(), full_size))
}

fn same_generation(left: &StatInfo, right: &StatInfo) -> bool {
    left.kind == right.kind
        && left.file_id == right.file_id
        && left.birthtime_ns == right.birthtime_ns
        && left.size == right.size
        && left.mtime_ns == right.mtime_ns
}

impl ContentIndexer {
    pub fn new(source: Box<dyn ContentSource>, caes: Arc<Store>) -> ContentIndexer {
        ContentIndexer {
            source,
            ocr: None,
            wasm: None,
            media: None,
            caes,
            budgets: Budgets::default(),
            counters: Arc::new(ContentCounters::default()),
        }
    }

    /// Attach the OCR source; image files route to it instead of the text
    /// worker.
    pub fn with_ocr(mut self, ocr: Box<dyn ContentSource>) -> ContentIndexer {
        self.ocr = Some(ocr);
        self
    }

    /// Attach the WASM plugin host; files whose extension a plugin declared
    /// route to it (highest priority — an explicit plugin wins).
    pub fn with_wasm(mut self, wasm: Box<dyn ContentSource>) -> ContentIndexer {
        self.wasm = Some(wasm);
        self
    }

    /// Attach the A/V transcription source (opt-in; see transcribe.rs).
    pub fn with_media(mut self, media: Box<dyn ContentSource>) -> ContentIndexer {
        self.media = Some(media);
        self
    }

    /// Decide routing once: explicit plugin > OCR > media > native.
    fn route(&self, name: &str) -> (RouteKind, ProcessorKey) {
        if self.wasm.as_ref().is_some_and(|w| w.handles(name)) {
            return (
                RouteKind::Wasm,
                self.wasm.as_ref().unwrap().processor_key(name),
            );
        }
        if self.ocr.as_ref().is_some_and(|o| o.handles(name)) {
            return (
                RouteKind::Ocr,
                self.ocr.as_ref().unwrap().processor_key(name),
            );
        }
        if self.media.as_ref().is_some_and(|m| m.handles(name)) {
            return (
                RouteKind::Media,
                self.media.as_ref().unwrap().processor_key(name),
            );
        }
        (RouteKind::Native, self.source.processor_key(name))
    }

    fn source_mut(&mut self, route: RouteKind) -> &mut dyn ContentSource {
        match route {
            RouteKind::Wasm => self.wasm.as_mut().unwrap().as_mut(),
            RouteKind::Ocr => self.ocr.as_mut().unwrap().as_mut(),
            RouteKind::Media => self.media.as_mut().unwrap().as_mut(),
            RouteKind::Native => self.source.as_mut(),
        }
    }

    fn store_with_projection_alias(
        &self,
        key: &CaesKey,
        record: &ExtractionRecord,
    ) -> Result<(), String> {
        self.caes
            .put(key, record)
            .map_err(|error| error.to_string())?;
        self.store_projection_alias(key, record)
    }

    fn store_projection_alias(
        &self,
        key: &CaesKey,
        record: &ExtractionRecord,
    ) -> Result<(), String> {
        if key.extractor_id != EXTRACTOR_ID || key.extractor_version != EXTRACTOR_VERSION {
            self.caes
                .put(
                    &CaesKey {
                        content_hash: key.content_hash,
                        extractor_id: EXTRACTOR_ID.into(),
                        extractor_version: EXTRACTOR_VERSION,
                        hints_hash: key.hints_hash,
                        abi_version: key.abi_version,
                    },
                    record,
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Content-index the file upserts of a just-committed batch, journaling
    /// one `SetContent` batch for whatever resolved.
    pub fn process(&mut self, committer: &mut Committer, upserts: &[(String, StatInfo)]) -> bool {
        let mut out: Vec<Change> = Vec::new();
        let mut succeeded = true;
        for (path, stat) in upserts {
            if stat.kind != ObjectKind::File {
                continue;
            }
            match self.process_one(committer, path, stat) {
                Ok(Some(ch)) => out.push(ch),
                Ok(None) => {}
                Err(e) => {
                    succeeded = false;
                    tracing::warn!("content indexing {path:?} failed: {e}");
                }
            }
        }
        if !out.is_empty() {
            if let Err(e) = committer.commit(Source::Content, &out) {
                succeeded = false;
                tracing::error!("SetContent commit failed: {e}");
            }
        }
        succeeded
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
        let mut file = std::fs::File::open(p).map_err(|error| error.to_string())?;
        let pinned = StatInfo::from_metadata(&file.metadata().map_err(|error| error.to_string())?);
        if !same_generation(&pinned, stat) {
            tracing::debug!("content open raced catalog stat for {path:?}; rescheduling");
            return Ok(None);
        }
        let (content_hash, full_size) = hash_file(&mut file).map_err(|e| e.to_string())?;
        let truncated = full_size > self.budgets.max_input_bytes;
        let hints = ExtractHints {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            full_size,
        };
        let (route, processor) = self.route(&hints.name);
        let hints_hash = hints.hints_hash_with(truncated, &processor.hints_tag);
        let key = CaesKey {
            content_hash,
            extractor_id: processor.extractor_id,
            extractor_version: processor.extractor_version,
            hints_hash,
            abi_version: ABI_VERSION,
        };

        let prior_failures = match self.caes.get(&key) {
            Ok(Some(rec)) => {
                if let Some(count) = retry_failure_count(&rec) {
                    count
                } else {
                    self.counters.caes_hits.fetch_add(1, Ordering::Relaxed);
                    rsd_metrics::metrics().caes_hits.inc();
                    let after = StatInfo::from_metadata(
                        &file.metadata().map_err(|error| error.to_string())?,
                    );
                    if !same_generation(&pinned, &after) {
                        tracing::debug!(
                            "content changed during hashing for {path:?}; rescheduling"
                        );
                        return Ok(None);
                    }
                    self.store_projection_alias(&key, &rec)?;
                    return Ok(Some(Change::SetContent {
                        path: path.to_string(),
                        content_hash,
                        hints_hash,
                        state: rec.status.as_str().to_string(),
                    }));
                }
            }
            Ok(None) => 0,
            Err(CaesError::Corrupt { .. }) => {
                self.caes.evict(&key).map_err(|error| error.to_string())?;
                0
            }
            Err(e) => return Err(e.to_string()),
        };

        rsd_metrics::metrics().caes_misses.inc();
        let t_ex = std::time::Instant::now();
        let budgets = self.budgets;
        let src = self.source_mut(route);
        let extracted = src.extract_file(&file, p, &hints, &budgets);
        rsd_metrics::metrics()
            .extract_ms
            .record(t_ex.elapsed().as_secs_f64() * 1000.0);
        let after = StatInfo::from_metadata(&file.metadata().map_err(|error| error.to_string())?);
        if !same_generation(&pinned, &after) {
            tracing::debug!("content changed during extraction for {path:?}; rescheduling");
            return Ok(None);
        }
        let status = match extracted {
            Ok(rec) => {
                rsd_metrics::metrics().files_indexed.inc();
                if rec.status.as_str() != "complete" && rec.status.as_str() != "partial" {
                    rsd_metrics::metrics().record_extraction_failure(rec.status.as_str());
                }
                self.store_with_projection_alias(&key, &rec)?;
                self.counters.extractions.fetch_add(1, Ordering::Relaxed);
                rec.status
            }
            Err(reason) => {
                self.counters.failures.fetch_add(1, Ordering::Relaxed);
                let failures = prior_failures.saturating_add(1);
                if failures < QUARANTINE_AFTER {
                    // Persist the retry budget in CAES. A daemon restart now
                    // resumes at the same count instead of granting hostile
                    // content three fresh worker crashes on every boot.
                    let retry = ExtractionRecord {
                        status: ExtractStatus::Corrupt,
                        text: String::new(),
                        attrs: vec![
                            (RETRY_COUNT_ATTR.into(), failures.to_string()),
                            (QUARANTINE_REASON_ATTR.into(), reason),
                        ],
                        symbols: vec![],
                    };
                    self.store_with_projection_alias(&key, &retry)?;
                    // Leave index_state unset: the next event or scan retries.
                    return Ok(None);
                }
                let qrec = ExtractionRecord {
                    status: ExtractStatus::Quarantined,
                    text: String::new(),
                    attrs: vec![(QUARANTINE_REASON_ATTR.into(), reason)],
                    symbols: vec![],
                };
                self.store_with_projection_alias(&key, &qrec)?;
                self.counters.quarantined.fetch_add(1, Ordering::Relaxed);
                rsd_metrics::metrics().quarantines.inc();
                rsd_metrics::metrics().record_extraction_failure("quarantined");
                ExtractStatus::Quarantined
            }
        };

        Ok(Some(Change::SetContent {
            path: path.to_string(),
            content_hash,
            hints_hash,
            state: status.as_str().to_string(),
        }))
    }
}

fn retry_failure_count(record: &ExtractionRecord) -> Option<u32> {
    (record.status == ExtractStatus::Corrupt)
        .then(|| {
            record
                .attrs
                .iter()
                .find(|(key, _)| key == RETRY_COUNT_ATTR)
                .and_then(|(_, value)| value.parse().ok())
        })
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsd_catalog::{Catalog, Durability};
    use rsd_log::{Journal, JournalConfig};
    use std::io::{Seek, SeekFrom, Write};

    struct TaggedSource;

    struct ReplacePathDuringExtraction;

    impl ContentSource for ReplacePathDuringExtraction {
        fn extract_file(
            &mut self,
            file: &std::fs::File,
            path: &Path,
            _hints: &ExtractHints,
            _budgets: &Budgets,
        ) -> Result<ExtractionRecord, String> {
            std::fs::rename(path, path.with_extension("old")).map_err(|error| error.to_string())?;
            std::fs::write(path, b"replacement bytes").map_err(|error| error.to_string())?;

            let mut pinned = file.try_clone().map_err(|error| error.to_string())?;
            pinned.rewind().map_err(|error| error.to_string())?;
            let mut bytes = Vec::new();
            pinned
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            Ok(ExtractionRecord {
                status: ExtractStatus::Complete,
                text: String::from_utf8(bytes).unwrap(),
                attrs: vec![],
                symbols: vec![],
            })
        }
    }

    impl ContentSource for TaggedSource {
        fn extract_file(
            &mut self,
            _file: &std::fs::File,
            _path: &Path,
            _hints: &ExtractHints,
            _budgets: &Budgets,
        ) -> Result<ExtractionRecord, String> {
            unreachable!()
        }

        fn handles(&self, name: &str) -> bool {
            name.ends_with(".png")
        }

        fn processor_key(&self, _name: &str) -> ProcessorKey {
            ProcessorKey {
                extractor_id: "test.ocr".into(),
                extractor_version: 7,
                hints_tag: "language=fr".into(),
            }
        }
    }

    #[test]
    fn content_hash_includes_bytes_after_extraction_budget() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.bin");
        let second = dir.path().join("second.bin");
        let cap = Budgets::default().max_input_bytes;
        for (path, suffix) in [(&first, b'A'), (&second, b'B')] {
            let mut file = std::fs::File::create(path).unwrap();
            file.set_len(cap).unwrap();
            file.seek(SeekFrom::Start(cap)).unwrap();
            file.write_all(&[suffix]).unwrap();
        }
        let (first_hash, _) = hash_file(&mut std::fs::File::open(&first).unwrap()).unwrap();
        let (second_hash, _) = hash_file(&mut std::fs::File::open(&second).unwrap()).unwrap();
        assert_ne!(first_hash, second_hash);
    }

    #[test]
    fn routing_returns_the_processor_key_for_the_selected_source() {
        let dir = tempfile::tempdir().unwrap();
        let caes = Arc::new(Store::open(&dir.path().join("caes.redb")).unwrap());
        let indexer =
            ContentIndexer::new(Box::new(TaggedSource), caes).with_ocr(Box::new(TaggedSource));
        let (route, key) = indexer.route("scan.png");
        assert!(matches!(route, RouteKind::Ocr));
        assert_eq!(key.extractor_id, "test.ocr");
        assert_eq!(key.extractor_version, 7);
        assert_eq!(key.hints_tag, "language=fr");
    }

    #[test]
    fn path_replacement_cannot_store_new_bytes_under_old_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raced.txt");
        std::fs::write(&path, b"original bytes").unwrap();
        let stat = StatInfo::from_metadata(&std::fs::symlink_metadata(&path).unwrap());
        let path_string = path.to_string_lossy().into_owned();

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
        let mut committer = Committer::new(catalog, journal);
        committer
            .commit(
                Source::Scan,
                &[Change::Upsert {
                    path: path_string.clone(),
                    stat,
                }],
            )
            .unwrap();

        let caes = Arc::new(Store::open(&dir.path().join("caes.redb")).unwrap());
        let mut indexer = ContentIndexer::new(Box::new(ReplacePathDuringExtraction), caes.clone());
        let change = indexer
            .process_one(&committer, &path_string, &stat)
            .unwrap()
            .expect("pinned old generation produces a candidate");
        let Change::SetContent {
            content_hash,
            hints_hash,
            ..
        } = &change
        else {
            panic!("expected SetContent");
        };
        assert_eq!(*content_hash, *blake3::hash(b"original bytes").as_bytes());
        let record = caes
            .get(&CaesKey {
                content_hash: *content_hash,
                extractor_id: EXTRACTOR_ID.into(),
                extractor_version: EXTRACTOR_VERSION,
                hints_hash: *hints_hash,
                abi_version: ABI_VERSION,
            })
            .unwrap()
            .unwrap();
        assert_eq!(record.text, "original bytes");

        assert_eq!(committer.commit(Source::Content, &[change]).unwrap(), None);
        assert_eq!(committer.journal_max_lsn(), 1);
    }
}
