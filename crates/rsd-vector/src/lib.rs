//! rsd-vector: the semantic plane (P6.2/P6.3, DESIGN.md §6.5).
//!
//! Architecture per design: chunks are keyed under the document oid, the plane
//! is a projection of journal + CAES with its own watermark, and the embedder
//! is a trait — the CoreML/ANE sidecar (P6.1) slots in behind `Embedder`
//! without touching the plane. Shipped embedder: a deterministic hashed
//! n-gram projection (feature hashing over words + word bigrams, L2
//! normalized). It is honest about what it is — lexical-overlap semantics,
//! not a learned model — but it is fast (μs/chunk), fully local, and exercises
//! every seam the learned model will use.
//!
//! Retrieval: exact cosine scan over normalized vectors. At T0/T1 corpus
//! scale (≤ ~1M chunks) an exact scan is a few ms and beats HNSW's build
//! complexity; segmented HNSW lands when the benchmark matrix says so.

use redb::{Database, ReadableTable, TableDefinition};
use rsd_caes::{CaesKey, Store, ABI_VERSION};
use rsd_catalog::{Catalog, Change};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

const VECTORS: TableDefinition<u64, &[u8]> = TableDefinition::new("vectors");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const APPLIED_LSN: &str = "applied_lsn";

#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    #[error("redb: {0}")]
    Db(Box<redb::Error>),
    #[error("encode: {0}")]
    Encode(#[from] postcard::Error),
    #[error("catalog: {0}")]
    Catalog(#[from] rsd_catalog::CatalogError),
    #[error("caes: {0}")]
    Caes(#[from] rsd_caes::CaesError),
    #[error("embedding unavailable: {0}")]
    Embedding(String),
}

macro_rules! from_redb {
    ($($t:ty),*) => {$(
        impl From<$t> for VectorError {
            fn from(e: $t) -> Self { Self::Db(Box::new(e.into())) }
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

pub type Result<T> = std::result::Result<T, VectorError>;

// -------------------------------------------------------------- embedding

/// The seam the CoreML/ANE sidecar plugs into (P6.1). Vectors MUST be L2
/// normalized so search is a dot product.
pub trait Embedder: Send + Sync {
    fn id(&self) -> &str;
    fn version(&self) -> u32;
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// Deterministic hashed n-gram projection: words + word-bigrams feature-hashed
/// into `dim` buckets with hash-derived signs, L2 normalized.
pub struct HashEmbedder {
    dim: usize,
}

impl Default for HashEmbedder {
    fn default() -> Self {
        HashEmbedder { dim: 256 }
    }
}

fn fold_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

fn h64(s: &str, seed: u64) -> u64 {
    // FNV-1a with seed mix — deterministic across runs and platforms.
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ seed;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

impl Embedder for HashEmbedder {
    fn id(&self) -> &str {
        "rsd.hash-ngram"
    }
    fn version(&self) -> u32 {
        1
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        let toks = fold_tokens(text);
        let mut bump = |s: &str, w: f32| {
            let h = h64(s, 7);
            let idx = (h as usize) % self.dim;
            let sign = if h & (1 << 63) == 0 { 1.0 } else { -1.0 };
            v[idx] += sign * w;
        };
        for t in &toks {
            bump(t, 1.0);
        }
        for pair in toks.windows(2) {
            bump(&format!("{} {}", pair[0], pair[1]), 0.5);
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

// --------------------------------------------------------------- chunking

/// Structure-aware-lite chunking: paragraph boundaries first, packed to
/// ~1000 chars. Returns (byte_offset, chunk_text).
pub fn chunk(text: &str) -> Vec<(usize, String)> {
    const TARGET: usize = 1000;
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut cur = String::new();
    let mut cur_off = 0usize;
    let mut off = 0usize;
    for para in text.split_inclusive("\n\n") {
        if cur.is_empty() {
            cur_off = off;
        }
        cur.push_str(para);
        off += para.len();
        if cur.len() >= TARGET {
            out.push((cur_off, std::mem::take(&mut cur)));
        }
    }
    if !cur.trim().is_empty() {
        out.push((cur_off, cur));
    }
    out
}

// ------------------------------------------------------------------ plane

#[derive(Serialize, Deserialize)]
struct DocVectors {
    embedder_id: String,
    embedder_version: u32,
    chunks: Vec<(u32 /*offset*/, Vec<f32>)>,
}

pub struct VectorPlane {
    db: Database,
    embedder: Arc<dyn Embedder>,
    applied_lsn: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticHit {
    pub oid: u64,
    pub score: f32,
    pub chunk_offset: u32,
}

impl VectorPlane {
    fn embed_chunks(&self, text: &str) -> Result<Vec<(u32, Vec<f32>)>> {
        chunk(text)
            .into_iter()
            .map(|(offset, text)| {
                let vector = self.embedder.embed(&text);
                if vector.len() != self.embedder.dim()
                    || vector.iter().any(|value| !value.is_finite())
                    || vector.iter().all(|value| *value == 0.0)
                {
                    return Err(VectorError::Embedding(
                        "helper returned an invalid/zero vector".into(),
                    ));
                }
                Ok((offset as u32, vector))
            })
            .collect()
    }

    pub fn open(path: &Path, embedder: Arc<dyn Embedder>) -> Result<VectorPlane> {
        let db = Database::create(path)?;
        let txn = db.begin_write()?;
        {
            txn.open_table(VECTORS)?;
            txn.open_table(META)?;
        }
        txn.commit()?;
        let applied_lsn = {
            let txn = db.begin_read()?;
            let meta = txn.open_table(META)?;
            meta.get(APPLIED_LSN)?.map(|g| g.value()).unwrap_or(0)
        };
        Ok(VectorPlane {
            db,
            embedder,
            applied_lsn,
        })
    }

    pub fn applied_lsn(&self) -> u64 {
        self.applied_lsn
    }

    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }

    /// Third projection: SetContent → chunk → embed → store under oid, with
    /// the plane's own watermark (the "second timeline" of DESIGN.md §7.3 —
    /// here synchronous because the shipped embedder is μs-fast; the sidecar
    /// keeps the same apply shape asynchronously).
    pub fn apply(
        &mut self,
        first_lsn: u64,
        changes: &[Change],
        remove_oids: &[u64],
        catalog: &Catalog,
        caes: &Store,
    ) -> Result<()> {
        let last = first_lsn + changes.len() as u64 - 1;
        let mut txn = self.db.begin_write()?;
        txn.set_durability(redb::Durability::Eventual);
        {
            let mut table = txn.open_table(VECTORS)?;
            let mut meta = txn.open_table(META)?;
            for oid in remove_oids {
                table.remove(*oid)?;
            }
            for (i, ch) in changes.iter().enumerate() {
                if first_lsn + i as u64 <= self.applied_lsn {
                    continue;
                }
                if let Change::Upsert { path, .. } = ch {
                    if let Some((oid, rec)) = catalog.get_by_path(path)? {
                        if rec.content_hash.is_none() {
                            table.remove(oid)?;
                        }
                    }
                }
                let Change::SetContent {
                    path,
                    content_hash,
                    hints_hash,
                    ..
                } = ch
                else {
                    continue;
                };
                let Some((oid, _)) = catalog.get_by_path(path)? else {
                    continue;
                };
                let Some(rec) = caes.get(&CaesKey {
                    content_hash: *content_hash,
                    extractor_id: rsd_extract::EXTRACTOR_ID.into(),
                    extractor_version: rsd_extract::EXTRACTOR_VERSION,
                    hints_hash: *hints_hash,
                    abi_version: ABI_VERSION,
                })?
                else {
                    continue;
                };
                let chunks = self.embed_chunks(&rec.text)?;
                let doc = DocVectors {
                    embedder_id: self.embedder.id().to_string(),
                    embedder_version: self.embedder.version(),
                    chunks,
                };
                table.insert(oid, postcard::to_allocvec(&doc)?.as_slice())?;
            }
            if last > self.applied_lsn {
                meta.insert(APPLIED_LSN, last)?;
            }
        }
        txn.commit()?;
        self.applied_lsn = self.applied_lsn.max(last);
        Ok(())
    }

    /// Clear the disposable projection and reset its durable watermark.
    pub fn reset(&mut self) -> Result<()> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(redb::Durability::Eventual);
        {
            let mut table = txn.open_table(VECTORS)?;
            let mut meta = txn.open_table(META)?;
            let keys: Vec<u64> = table
                .iter()?
                .map(|item| item.map(|(key, _)| key.value()))
                .collect::<std::result::Result<_, _>>()?;
            for oid in keys {
                table.remove(oid)?;
            }
            meta.insert(APPLIED_LSN, 0)?;
        }
        txn.commit()?;
        self.applied_lsn = 0;
        Ok(())
    }

    /// Replace the whole disposable projection with current catalog objects,
    /// sourcing text only from CAES.
    pub fn rebuild_current(
        &mut self,
        target_lsn: u64,
        catalog: &Catalog,
        caes: &Store,
    ) -> Result<()> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(redb::Durability::Eventual);
        {
            let mut table = txn.open_table(VECTORS)?;
            let mut meta = txn.open_table(META)?;
            let keys: Vec<u64> = table
                .iter()?
                .map(|item| item.map(|(key, _)| key.value()))
                .collect::<std::result::Result<_, _>>()?;
            for oid in keys {
                table.remove(oid)?;
            }
            for binding in catalog.content_bindings()? {
                let Some(record) = caes.get(&CaesKey {
                    content_hash: binding.content_hash,
                    extractor_id: rsd_extract::EXTRACTOR_ID.into(),
                    extractor_version: rsd_extract::EXTRACTOR_VERSION,
                    hints_hash: binding.hints_hash,
                    abi_version: ABI_VERSION,
                })?
                else {
                    continue;
                };
                let chunks = self.embed_chunks(&record.text)?;
                let doc = DocVectors {
                    embedder_id: self.embedder.id().to_string(),
                    embedder_version: self.embedder.version(),
                    chunks,
                };
                table.insert(binding.oid, postcard::to_allocvec(&doc)?.as_slice())?;
            }
            meta.insert(APPLIED_LSN, target_lsn)?;
        }
        txn.commit()?;
        self.applied_lsn = target_lsn;
        Ok(())
    }

    pub fn remove(&mut self, oid: u64) -> Result<()> {
        self.remove_many(&[oid])
    }

    pub fn remove_many(&mut self, oids: &[u64]) -> Result<()> {
        if oids.is_empty() {
            return Ok(());
        }
        let mut txn = self.db.begin_write()?;
        txn.set_durability(redb::Durability::Eventual);
        {
            let mut table = txn.open_table(VECTORS)?;
            for oid in oids {
                table.remove(*oid)?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Exact cosine top-k: per-doc best chunk score. Normalized vectors =>
    /// dot product.
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<SemanticHit>> {
        self.search_filtered(query, k, |_| true)
    }

    /// Exact cosine top-k with authorization applied before candidates enter
    /// the ranked result set.
    pub fn search_filtered(
        &self,
        query: &str,
        k: usize,
        allowed: impl Fn(u64) -> bool,
    ) -> Result<Vec<SemanticHit>> {
        let qv = self.embedder.embed(query);
        let txn = self.db.begin_read()?;
        let table = txn.open_table(VECTORS)?;
        let mut hits: Vec<SemanticHit> = Vec::new();
        for item in table.iter()? {
            let (key, val) = item?;
            if !allowed(key.value()) {
                continue;
            }
            let doc: DocVectors = postcard::from_bytes(val.value())?;
            if doc.embedder_id != self.embedder.id()
                || doc.embedder_version != self.embedder.version()
            {
                continue; // mixed projection versions: skip stale (§6.2)
            }
            let mut best: Option<(f32, u32)> = None;
            for (off, v) in &doc.chunks {
                let score: f32 = v.iter().zip(&qv).map(|(a, b)| a * b).sum();
                if best.map(|(b, _)| score > b).unwrap_or(true) {
                    best = Some((score, *off));
                }
            }
            if let Some((score, off)) = best {
                hits.push(SemanticHit {
                    oid: key.value(),
                    score,
                    chunk_offset: off,
                });
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        Ok(hits)
    }

    /// Semantic-alert primitive (P6.4): does this text clear the similarity
    /// threshold for the query? Used by the live engine per delta.
    pub fn text_similarity(&self, query_vec: &[f32], text: &str) -> f32 {
        chunk(text)
            .into_iter()
            .map(|(_, c)| {
                let v = self.embedder.embed(&c);
                v.iter().zip(query_vec).map(|(a, b)| a * b).sum::<f32>()
            })
            .fold(0f32, f32::max)
    }
}

/// Which engine(s) surfaced a fused hit — provenance the UI can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchOrigin {
    Exact,
    Meaning,
    Both,
}

impl MatchOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            MatchOrigin::Exact => "exact",
            MatchOrigin::Meaning => "meaning",
            MatchOrigin::Both => "both",
        }
    }
}

/// RRF with per-hit provenance.
pub fn rrf_tagged(lexical: &[u64], semantic: &[u64], k: usize) -> Vec<(u64, MatchOrigin)> {
    let lex: std::collections::HashSet<u64> = lexical.iter().copied().collect();
    let sem: std::collections::HashSet<u64> = semantic.iter().copied().collect();
    rrf(lexical, semantic, k)
        .into_iter()
        .map(|oid| {
            let origin = match (lex.contains(&oid), sem.contains(&oid)) {
                (true, true) => MatchOrigin::Both,
                (true, false) => MatchOrigin::Exact,
                _ => MatchOrigin::Meaning,
            };
            (oid, origin)
        })
        .collect()
}

/// Reciprocal-rank fusion of two ranked oid lists (P6.3): 1/(60+rank).
pub fn rrf(lexical: &[u64], semantic: &[u64], k: usize) -> Vec<u64> {
    let mut scores: std::collections::HashMap<u64, f64> = std::collections::HashMap::new();
    for (rank, oid) in lexical.iter().enumerate() {
        *scores.entry(*oid).or_default() += 1.0 / (60.0 + rank as f64);
    }
    for (rank, oid) in semantic.iter().enumerate() {
        *scores.entry(*oid).or_default() += 1.0 / (60.0 + rank as f64);
    }
    let mut out: Vec<(u64, f64)> = scores.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out.truncate(k);
    out.into_iter().map(|(oid, _)| oid).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ZeroEmbedder;

    impl Embedder for ZeroEmbedder {
        fn id(&self) -> &str {
            "zero"
        }
        fn version(&self) -> u32 {
            1
        }
        fn dim(&self) -> usize {
            4
        }
        fn embed(&self, _text: &str) -> Vec<f32> {
            vec![0.0; 4]
        }
    }

    #[test]
    fn embedder_is_deterministic_and_normalized() {
        let e = HashEmbedder::default();
        let a = e.embed("the quarterly invoice with payment terms");
        let b = e.embed("the quarterly invoice with payment terms");
        assert_eq!(a, b);
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn similar_text_scores_higher_than_unrelated() {
        let e = HashEmbedder::default();
        let doc = e.embed("invoice payment terms net sixty days quarterly billing");
        let close = e.embed("quarterly invoice payment");
        let far = e.embed("dilithium warp core engineering schematics");
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!(dot(&doc, &close) > dot(&doc, &far) + 0.2);
    }

    #[test]
    fn chunking_packs_paragraphs_with_offsets() {
        let text = format!("{}\n\n{}\n\n{}", "a".repeat(600), "b".repeat(600), "tail");
        let chunks = chunk(&text);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0].0, 0);
        for (off, c) in &chunks {
            assert_eq!(&text[*off..*off + c.len()], c.as_str());
        }
    }

    #[test]
    fn rrf_prefers_agreement() {
        let fused = rrf(&[1, 2, 3], &[3, 4, 5], 10);
        assert_eq!(fused[0], 3, "doc ranked by both lists must win");
    }

    #[test]
    fn zero_embedding_is_not_persisted_or_watermarked() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(&dir.path().join("catalog.redb")).unwrap();
        let caes = Store::open(&dir.path().join("caes.redb")).unwrap();
        let path = dir.path().join("doc.txt").to_string_lossy().into_owned();
        let stat = rsd_catalog::StatInfo {
            kind: rsd_catalog::ObjectKind::File,
            file_id: rsd_catalog::FileId { dev: 1, ino: 1 },
            size: 4,
            mtime_ns: 1,
            birthtime_ns: 1,
            nlink: 1,
        };
        let content_hash = [3u8; 32];
        let hints_hash = [7u8; 32];
        let key = CaesKey {
            content_hash,
            extractor_id: rsd_extract::EXTRACTOR_ID.into(),
            extractor_version: rsd_extract::EXTRACTOR_VERSION,
            hints_hash,
            abi_version: ABI_VERSION,
        };
        caes.put(
            &key,
            &rsd_caes::ExtractionRecord {
                status: rsd_caes::ExtractStatus::Complete,
                text: "some text".into(),
                attrs: vec![],
                symbols: vec![],
            },
        )
        .unwrap();
        let changes = vec![
            Change::Upsert {
                path: path.clone(),
                stat,
            },
            Change::SetContent {
                path,
                content_hash,
                hints_hash,
                state: "complete".into(),
            },
        ];
        catalog.apply_changes(1, &changes).unwrap();
        let mut plane =
            VectorPlane::open(&dir.path().join("vector.redb"), Arc::new(ZeroEmbedder)).unwrap();
        assert!(matches!(
            plane.apply(1, &changes, &[], &catalog, &caes),
            Err(VectorError::Embedding(_))
        ));
        assert_eq!(plane.applied_lsn(), 0);
        assert!(plane.search("text", 10).unwrap().is_empty());
    }
}
