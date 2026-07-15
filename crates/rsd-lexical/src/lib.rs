//! rsd-lexical: the tantivy full-text plane (P4.1, DESIGN.md §6.4).
//!
//! Projection discipline: documents are keyed by catalog oid and store NO
//! paths or names — results resolve to paths through the catalog at query
//! time, so a rename can never serve a stale path. The plane is rebuildable
//! from journal + CAES with zero filesystem reads (failure matrix §6.8), and
//! carries its own applied-LSN watermark in tantivy's commit payload.

use rsd_caes::{CaesKey, Store, ABI_VERSION};
use rsd_catalog::{Catalog, Change};
use rsd_extract::{EXTRACTOR_ID, EXTRACTOR_VERSION};
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TantivyDocument, Value, FAST, INDEXED, STORED, STRING, TEXT,
};
use tantivy::{Index, IndexWriter, Term};

#[derive(Debug, thiserror::Error)]
pub enum LexicalError {
    #[error("tantivy: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error("catalog: {0}")]
    Catalog(#[from] rsd_catalog::CatalogError),
    #[error("caes: {0}")]
    Caes(#[from] rsd_caes::CaesError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, LexicalError>;

/// Read-only handle: safe to open while a writer (the daemon) is live —
/// takes no locks. This is what query engines and rsdfind use.
pub struct LexicalReader {
    index: Index,
    /// Cached tantivy reader (reloads on commit): building one per query
    /// costs ~1ms; reusing it makes a query ~100µs.
    reader: tantivy::IndexReader,
    f_oid: Field,
    f_content: Field,
    f_symbols: Field,
}

pub struct LexicalPlane {
    reader: LexicalReader,
    writer: IndexWriter,
    applied_lsn: u64,
}

fn schema() -> Schema {
    let mut b = Schema::builder();
    b.add_u64_field("oid", INDEXED | FAST | STORED);
    b.add_text_field("content", TEXT);
    // Raw (untokenized): identifiers like `ignite_thrusters` must match
    // exactly — the default tokenizer would split them on `_`. One value per
    // symbol, lowercased at both index and query time.
    b.add_text_field("symbols", STRING);
    b.build()
}

impl LexicalReader {
    pub fn open(dir: &Path) -> Result<LexicalReader> {
        std::fs::create_dir_all(dir)?;
        let mmap =
            tantivy::directory::MmapDirectory::open(dir).map_err(tantivy::TantivyError::from)?;
        let index = Index::open_or_create(mmap, schema())?;
        let s = index.schema();
        let reader = index.reader()?;
        Ok(LexicalReader {
            f_oid: s.get_field("oid").expect("schema"),
            f_content: s.get_field("content").expect("schema"),
            f_symbols: s.get_field("symbols").expect("schema"),
            reader,
            index,
        })
    }
}

impl LexicalPlane {
    pub fn open(dir: &Path) -> Result<LexicalPlane> {
        let reader = LexicalReader::open(dir)?;
        let writer = reader.index.writer(64 * 1024 * 1024)?;
        // Watermark rides in the commit payload.
        let applied_lsn = reader
            .index
            .load_metas()?
            .payload
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        Ok(LexicalPlane {
            reader,
            writer,
            applied_lsn,
        })
    }

    pub fn reader(&self) -> &LexicalReader {
        &self.reader
    }

    pub fn applied_lsn(&self) -> u64 {
        self.applied_lsn
    }

    /// Apply a committed batch: for each `SetContent` past the watermark, pull
    /// the extraction record from CAES (never the filesystem) and upsert the
    /// document under its oid. One tantivy commit per batch that changed
    /// anything; watermark advances with the commit payload.
    pub fn apply(
        &mut self,
        first_lsn: u64,
        changes: &[Change],
        catalog: &Catalog,
        caes: &Store,
    ) -> Result<()> {
        let last = first_lsn + changes.len() as u64 - 1;
        let mut dirty = false;
        for (i, ch) in changes.iter().enumerate() {
            let lsn = first_lsn + i as u64;
            if lsn <= self.applied_lsn {
                continue;
            }
            if let Change::SetContent {
                path,
                content_hash,
                hints_hash,
                ..
            } = ch
            {
                let Some((oid, _)) = catalog.get_by_path(path)? else {
                    continue;
                };
                let key = CaesKey {
                    content_hash: *content_hash,
                    extractor_id: EXTRACTOR_ID.into(),
                    extractor_version: EXTRACTOR_VERSION,
                    hints_hash: *hints_hash,
                    abi_version: ABI_VERSION,
                };
                let Some(rec) = caes.get(&key)? else {
                    continue;
                };
                self.writer
                    .delete_term(Term::from_field_u64(self.reader.f_oid, oid));
                let mut doc = TantivyDocument::default();
                doc.add_u64(self.reader.f_oid, oid);
                doc.add_text(self.reader.f_content, &rec.text);
                for sym in &rec.symbols {
                    doc.add_text(self.reader.f_symbols, sym.name.to_lowercase());
                }
                self.writer.add_document(doc)?;
                dirty = true;
            }
        }
        if dirty || last > self.applied_lsn {
            if dirty {
                let mut prepared = self.writer.prepare_commit()?;
                prepared.set_payload(&last.to_string());
                prepared.commit()?;
            }
            self.applied_lsn = self.applied_lsn.max(last);
        }
        Ok(())
    }

    /// Content search → matching oids (rank order). The p50 < 1ms path.
    pub fn search_content(&self, terms: &str, phrase: bool, limit: usize) -> Result<Vec<u64>> {
        self.reader.search_content(terms, phrase, limit)
    }

    /// Symbol search → matching oids.
    pub fn search_symbols(&self, terms: &str, limit: usize) -> Result<Vec<u64>> {
        self.reader.search_symbols(terms, limit)
    }

    pub fn doc_count(&self) -> Result<u64> {
        self.reader.doc_count()
    }
}

impl LexicalReader {
    /// Word/phrase membership query over a field. `terms` are whitespace-split
    /// and lowercased (matching the default tokenizer); multiple terms become
    /// a phrase when `phrase`, else an AND of terms.
    fn field_query(&self, field: Field, terms: &str, phrase: bool) -> Option<Box<dyn Query>> {
        let toks: Vec<String> = terms
            .split_whitespace()
            .map(|t| t.trim_matches('*').to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        match toks.len() {
            0 => None,
            1 => Some(Box::new(TermQuery::new(
                Term::from_field_text(field, &toks[0]),
                IndexRecordOption::Basic,
            ))),
            _ if phrase => Some(Box::new(PhraseQuery::new(
                toks.iter()
                    .map(|t| Term::from_field_text(field, t))
                    .collect(),
            ))),
            _ => Some(Box::new(BooleanQuery::new(
                toks.iter()
                    .map(|t| {
                        (
                            Occur::Must,
                            Box::new(TermQuery::new(
                                Term::from_field_text(field, t),
                                IndexRecordOption::Basic,
                            )) as Box<dyn Query>,
                        )
                    })
                    .collect(),
            ))),
        }
    }

    /// Content search → matching oids (rank order). The p50 < 1ms path.
    pub fn search_content(&self, terms: &str, phrase: bool, limit: usize) -> Result<Vec<u64>> {
        self.search_field(self.f_content, terms, phrase, limit)
    }

    /// Symbol search → matching oids.
    pub fn search_symbols(&self, terms: &str, limit: usize) -> Result<Vec<u64>> {
        self.search_field(self.f_symbols, terms, false, limit)
    }

    fn search_field(
        &self,
        field: Field,
        terms: &str,
        phrase: bool,
        limit: usize,
    ) -> Result<Vec<u64>> {
        let Some(query) = self.field_query(field, terms, phrase) else {
            return Ok(vec![]);
        };
        let searcher = self.reader.searcher();
        let top = searcher.search(&query, &TopDocs::with_limit(limit.max(1)))?;
        let mut out = Vec::with_capacity(top.len());
        for (_score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            if let Some(v) = doc.get_first(self.f_oid).and_then(|v| v.as_u64()) {
                out.push(v);
            }
        }
        Ok(out)
    }

    pub fn doc_count(&self) -> Result<u64> {
        self.reader.reload()?;
        Ok(self.reader.searcher().num_docs())
    }
}

/// Failure-matrix repair (§6.8): rebuild the whole plane from journal + CAES.
/// ZERO filesystem reads — content comes exclusively from CAES records.
pub fn rebuild(
    dir: &Path,
    journal: &rsd_log::Journal,
    catalog: &Catalog,
    caes: &Store,
) -> Result<LexicalPlane> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    let mut plane = LexicalPlane::open(dir)?;
    let mut batch: Vec<(u64, Change)> = Vec::new();
    journal
        .replay(1, |rec| batch.push((rec.lsn, rec.change)))
        .map_err(|e| std::io::Error::other(format!("journal replay: {e}")))?;
    for chunk in batch.chunks(4096) {
        let first = chunk[0].0;
        let changes: Vec<Change> = chunk.iter().map(|(_, c)| c.clone()).collect();
        plane.apply(first, &changes, catalog, caes)?;
    }
    Ok(plane)
}
