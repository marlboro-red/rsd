//! rsd-caes: the content-addressed extraction store (DESIGN.md §6.2) — the
//! authority on retained extraction records, and the rebuild source for the
//! content planes.
//!
//! Keying discipline (§10.2): records hold *content-derived facts only*, keyed
//! by `(content_hash, extractor_id, extractor_version, hints_hash, abi_version)`.
//! Instance-derived facts (path/name/xattr attributes) are never stored here.
//! Identical content under two paths — or ten thousand copies — extracts once.
//!
//! Performance: the store IS the fast path. A CAES hit turns "parse the file"
//! into one hash + one B-tree lookup; `Indexer` checks it before ever calling
//! an extractor, and blake3 makes the hash cost negligible relative to a read.

use redb::{Database, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const RECORDS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("extraction_records");
const HASH_LEN: usize = 16;
/// Bump on any wire-visible change to the host↔extractor contract.
pub const ABI_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum CaesError {
    #[error("redb: {0}")]
    Db(Box<redb::Error>),
    #[error("encode: {0}")]
    Encode(#[from] postcard::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt CAES record for key {key_hex}: {reason}")]
    Corrupt { key_hex: String, reason: String },
}

macro_rules! from_redb {
    ($($t:ty),*) => {$(
        impl From<$t> for CaesError {
            fn from(e: $t) -> Self {
                Self::Db(Box::new(e.into()))
            }
        }
    )*};
}
from_redb!(
    redb::Error,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::DatabaseError
);

pub type Result<T> = std::result::Result<T, CaesError>;

/// The full cache key. Every field participates — an extractor upgrade or a
/// hint change is a different record, never a stale hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaesKey {
    pub content_hash: [u8; 32],
    pub extractor_id: String,
    pub extractor_version: u32,
    /// Canonical hash of extraction-relevant hints (declared type, options).
    pub hints_hash: [u8; 32],
    pub abi_version: u32,
}

impl CaesKey {
    /// Collapse to a fixed storage key (blake3 of the serialized tuple).
    pub fn storage_key(&self) -> [u8; 32] {
        let bytes = postcard::to_allocvec(self).expect("key serialization is infallible");
        *blake3::hash(&bytes).as_bytes()
    }
}

/// Typed status per the extraction contract (§10.1). "Unindexable by policy or
/// physics is a labeled, queryable state."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtractStatus {
    Complete,
    Partial,
    EncryptedContent,
    PasswordRequired,
    CloudPlaceholder,
    ResourceBudgetExceeded,
    Unsupported,
    Corrupt,
    /// Extraction repeatedly crashed/hung on this content; recorded so the
    /// content is never retried blindly and the reason is queryable.
    Quarantined,
}

impl ExtractStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExtractStatus::Complete => "complete",
            ExtractStatus::Partial => "partial",
            ExtractStatus::EncryptedContent => "encrypted",
            ExtractStatus::PasswordRequired => "password-required",
            ExtractStatus::CloudPlaceholder => "cloud-placeholder",
            ExtractStatus::ResourceBudgetExceeded => "budget-exceeded",
            ExtractStatus::Unsupported => "unsupported",
            ExtractStatus::Corrupt => "corrupt",
            ExtractStatus::Quarantined => "quarantined",
        }
    }
}

/// A code symbol (function/type definition) with its 1-based line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRec {
    pub name: String,
    pub kind: String,
    pub line: u32,
}

/// A content-derived extraction record. Chunk boundaries, references, and
/// embeddings attach in later phases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractionRecord {
    pub status: ExtractStatus,
    pub text: String,
    pub attrs: Vec<(String, String)>,
    pub symbols: Vec<SymbolRec>,
}

pub struct Store {
    db: Database,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let db = Database::create(path)?;
        // Opening an existing authority must be read-only. Starting a schema
        // write transaction on every daemon restart lets SIGKILL during open
        // leave redb recovery work despite there being no CAES mutation.
        let needs_init = {
            let txn = db.begin_read()?;
            match txn.open_table(RECORDS) {
                Ok(_) => false,
                Err(redb::TableError::TableDoesNotExist(_)) => true,
                Err(error) => return Err(error.into()),
            }
        };
        if needs_init {
            let txn = db.begin_write()?;
            {
                txn.open_table(RECORDS)?;
            }
            txn.commit()?;
        }
        Ok(Store { db })
    }

    pub fn get(&self, key: &CaesKey) -> Result<Option<ExtractionRecord>> {
        let sk = key.storage_key();
        let txn = self.db.begin_read()?;
        let table = txn.open_table(RECORDS)?;
        let Some(guard) = table.get(sk.as_slice())? else {
            return Ok(None);
        };
        let bytes = guard.value();
        // Value layout: [payload][blake3_16(payload)] — defense in depth above
        // redb's page checksums; a mismatch is a detected-bit-rot event, never
        // a silent wrong answer.
        if bytes.len() < HASH_LEN {
            return Err(CaesError::Corrupt {
                key_hex: hex(&sk),
                reason: "record shorter than checksum".into(),
            });
        }
        let (payload, hash) = bytes.split_at(bytes.len() - HASH_LEN);
        if blake3::hash(payload).as_bytes()[..HASH_LEN] != *hash {
            return Err(CaesError::Corrupt {
                key_hex: hex(&sk),
                reason: "checksum mismatch".into(),
            });
        }
        Ok(Some(postcard::from_bytes(payload)?))
    }

    pub fn put(&self, key: &CaesKey, rec: &ExtractionRecord) -> Result<()> {
        let sk = key.storage_key();
        let mut payload = postcard::to_allocvec(rec)?;
        let hash = blake3::hash(&payload);
        payload.extend_from_slice(&hash.as_bytes()[..HASH_LEN]);
        let mut txn = self.db.begin_write()?;
        txn.set_durability(redb::Durability::Eventual);
        {
            let mut table = txn.open_table(RECORDS)?;
            table.insert(sk.as_slice(), payload.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Drop a (possibly corrupt) record so the next index pass re-extracts.
    pub fn evict(&self, key: &CaesKey) -> Result<()> {
        let sk = key.storage_key();
        let mut txn = self.db.begin_write()?;
        txn.set_durability(redb::Durability::Eventual);
        {
            let mut table = txn.open_table(RECORDS)?;
            table.remove(sk.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn len(&self) -> Result<u64> {
        use redb::ReadableTableMetadata;
        let txn = self.db.begin_read()?;
        Ok(txn.open_table(RECORDS)?.len()?)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Test hook: write raw bytes under a key (corruption injection).
    #[doc(hidden)]
    pub fn put_raw_for_tests(&self, key: &CaesKey, bytes: &[u8]) -> Result<()> {
        let sk = key.storage_key();
        let mut txn = self.db.begin_write()?;
        txn.set_durability(redb::Durability::Eventual);
        {
            let mut table = txn.open_table(RECORDS)?;
            table.insert(sk.as_slice(), bytes)?;
        }
        txn.commit()?;
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The extraction interface Phase 3 moves out-of-process; the trait boundary
/// stays identical so CAES logic never changes.
pub trait Extractor: Send + Sync {
    fn id(&self) -> &str;
    fn version(&self) -> u32;
    fn extract(&self, bytes: &[u8]) -> ExtractionRecord;
}

/// Get-or-extract front door. Counts calls so dedup is *proven*, not assumed.
pub struct Indexer<'a> {
    store: &'a Store,
    extractor: &'a dyn Extractor,
    pub extract_calls: AtomicU64,
    pub cache_hits: AtomicU64,
}

impl<'a> Indexer<'a> {
    pub fn new(store: &'a Store, extractor: &'a dyn Extractor) -> Indexer<'a> {
        Indexer {
            store,
            extractor,
            extract_calls: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
        }
    }

    fn key_for(&self, content_hash: [u8; 32], hints_hash: [u8; 32]) -> CaesKey {
        CaesKey {
            content_hash,
            extractor_id: self.extractor.id().to_string(),
            extractor_version: self.extractor.version(),
            hints_hash,
            abi_version: ABI_VERSION,
        }
    }

    /// Index raw bytes: CAES hit or extract-and-store.
    pub fn index_bytes(
        &self,
        bytes: &[u8],
        hints_hash: [u8; 32],
    ) -> Result<(CaesKey, ExtractionRecord)> {
        let key = self.key_for(*blake3::hash(bytes).as_bytes(), hints_hash);
        match self.store.get(&key) {
            Ok(Some(rec)) => {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                return Ok((key, rec));
            }
            Ok(None) => {}
            Err(CaesError::Corrupt { .. }) => {
                // Failure-matrix repair path: evict, re-extract.
                self.store.evict(&key)?;
            }
            Err(e) => return Err(e),
        }
        self.extract_calls.fetch_add(1, Ordering::Relaxed);
        let rec = self.extractor.extract(bytes);
        self.store.put(&key, &rec)?;
        Ok((key, rec))
    }

    /// Index a file's content (reads + hashes + get-or-extract).
    pub fn index_file(&self, path: &Path) -> Result<(CaesKey, ExtractionRecord)> {
        let bytes = std::fs::read(path)?;
        self.index_bytes(&bytes, [0u8; 32])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CountingExtractor;
    impl Extractor for CountingExtractor {
        fn id(&self) -> &str {
            "test.plain"
        }
        fn version(&self) -> u32 {
            1
        }
        fn extract(&self, bytes: &[u8]) -> ExtractionRecord {
            ExtractionRecord {
                status: ExtractStatus::Complete,
                text: String::from_utf8_lossy(bytes).into_owned(),
                attrs: vec![("test.len".into(), bytes.len().to_string())],
                symbols: vec![],
            }
        }
    }

    fn open_temp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("caes.redb")).unwrap();
        (dir, store)
    }

    #[test]
    fn round_trip_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("caes.redb");
        let key = CaesKey {
            content_hash: *blake3::hash(b"hello").as_bytes(),
            extractor_id: "test.plain".into(),
            extractor_version: 1,
            hints_hash: [0; 32],
            abi_version: ABI_VERSION,
        };
        let rec = ExtractionRecord {
            status: ExtractStatus::Complete,
            text: "hello".into(),
            attrs: vec![("a".into(), "b".into())],
            symbols: vec![],
        };
        {
            let store = Store::open(&db).unwrap();
            store.put(&key, &rec).unwrap();
            assert_eq!(store.get(&key).unwrap(), Some(rec.clone()));
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(store.get(&key).unwrap(), Some(rec));
    }

    #[test]
    fn key_fields_all_discriminate() {
        let base = CaesKey {
            content_hash: [1; 32],
            extractor_id: "x".into(),
            extractor_version: 1,
            hints_hash: [2; 32],
            abi_version: 1,
        };
        let mut variants = vec![base.clone()];
        variants.push(CaesKey {
            content_hash: [9; 32],
            ..base.clone()
        });
        variants.push(CaesKey {
            extractor_id: "y".into(),
            ..base.clone()
        });
        variants.push(CaesKey {
            extractor_version: 2,
            ..base.clone()
        });
        variants.push(CaesKey {
            hints_hash: [9; 32],
            ..base.clone()
        });
        variants.push(CaesKey {
            abi_version: 2,
            ..base.clone()
        });
        let keys: std::collections::HashSet<[u8; 32]> =
            variants.iter().map(|k| k.storage_key()).collect();
        assert_eq!(
            keys.len(),
            variants.len(),
            "every key field must discriminate"
        );
    }

    #[test]
    fn identical_content_under_two_paths_extracts_once() {
        let (_d, store) = open_temp();
        let files = tempfile::tempdir().unwrap();
        let a = files.path().join("a.txt");
        let b = files.path().join("copy of a.txt");
        std::fs::write(&a, "same bytes, different entries").unwrap();
        std::fs::copy(&a, &b).unwrap();

        let ex = CountingExtractor;
        let idx = Indexer::new(&store, &ex);
        let (ka, ra) = idx.index_file(&a).unwrap();
        let (kb, rb) = idx.index_file(&b).unwrap();

        assert_eq!(ka, kb);
        assert_eq!(ra, rb);
        assert_eq!(
            idx.extract_calls.load(Ordering::Relaxed),
            1,
            "the copy must be a pure cache hit"
        );
        assert_eq!(idx.cache_hits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn corrupt_record_is_detected_and_self_heals() {
        let (_d, store) = open_temp();
        let ex = CountingExtractor;
        let idx = Indexer::new(&store, &ex);

        let (key, _) = idx.index_bytes(b"precious", [0; 32]).unwrap();
        assert_eq!(idx.extract_calls.load(Ordering::Relaxed), 1);

        // Bit-rot the stored record.
        store
            .put_raw_for_tests(&key, b"garbage-no-checksum")
            .unwrap();
        assert!(matches!(store.get(&key), Err(CaesError::Corrupt { .. })));

        // The indexer's repair path: detect → evict → re-extract.
        let (_, rec) = idx.index_bytes(b"precious", [0; 32]).unwrap();
        assert_eq!(rec.text, "precious");
        assert_eq!(idx.extract_calls.load(Ordering::Relaxed), 2);
        assert!(matches!(store.get(&key), Ok(Some(_))));
    }
}
