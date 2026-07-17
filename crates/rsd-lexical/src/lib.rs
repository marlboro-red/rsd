//! rsd-lexical: the tantivy full-text plane (P4.1, DESIGN.md §6.4).
//!
//! Projection discipline: documents are keyed by catalog oid. Stored results
//! still resolve paths through the catalog, so a rename can never serve a stale
//! path. The index additionally carries non-stored component-ancestor terms
//! used only to constrain authorized candidate generation. Rename/unlink
//! commits refresh those terms from the catalog's current hard-link set.

use rsd_caes::{CaesKey, Store, ABI_VERSION};
use rsd_catalog::{Catalog, Change};
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
    f_path_scope: Field,
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
    // Each current path contributes itself and every component ancestor. A
    // grant is therefore an exact TermQuery, not a vulnerable string prefix.
    b.add_text_field("path_scope", STRING);
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
            f_path_scope: s.get_field("path_scope").expect("schema"),
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
        remove_oids: &[u64],
        refresh_oids: &[u64],
        catalog: &Catalog,
        caes: &Store,
    ) -> Result<()> {
        let last = first_lsn + changes.len() as u64 - 1;
        for oid in remove_oids {
            self.writer
                .delete_term(Term::from_field_u64(self.reader.f_oid, *oid));
        }
        let mut refresh: std::collections::HashSet<u64> = refresh_oids.iter().copied().collect();
        for (i, ch) in changes.iter().enumerate() {
            let lsn = first_lsn + i as u64;
            if lsn <= self.applied_lsn {
                continue;
            }
            if let Some((oid, _)) = catalog.get_by_path(ch.path())? {
                refresh.insert(oid);
            }
        }
        for oid in refresh {
            self.refresh_document(oid, catalog, caes)?;
        }
        if last > self.applied_lsn {
            let mut prepared = self.writer.prepare_commit()?;
            prepared.set_payload(&last.to_string());
            prepared.commit()?;
            self.reader.reader.reload()?;
            self.applied_lsn = self.applied_lsn.max(last);
        }
        Ok(())
    }

    fn refresh_document(&mut self, oid: u64, catalog: &Catalog, caes: &Store) -> Result<()> {
        self.writer
            .delete_term(Term::from_field_u64(self.reader.f_oid, oid));
        let Some(object) = catalog.get_object(oid)? else {
            return Ok(());
        };
        let (Some(content_hash), Some(hints_hash)) = (object.content_hash, object.caes_hints_hash)
        else {
            return Ok(());
        };
        let Some(record) = caes.get(&CaesKey {
            content_hash,
            extractor_id: rsd_extract::EXTRACTOR_ID.into(),
            extractor_version: rsd_extract::EXTRACTOR_VERSION,
            hints_hash,
            abi_version: ABI_VERSION,
        })?
        else {
            return Ok(());
        };
        let mut doc = TantivyDocument::default();
        doc.add_u64(self.reader.f_oid, oid);
        doc.add_text(self.reader.f_content, &record.text);
        for symbol in &record.symbols {
            doc.add_text(self.reader.f_symbols, symbol.name.to_lowercase());
        }
        add_path_scopes(&mut doc, self.reader.f_path_scope, &object.entry_paths);
        self.writer.add_document(doc)?;
        Ok(())
    }

    /// Clear the disposable projection and reset its durable watermark.
    pub fn reset(&mut self) -> Result<()> {
        self.writer.delete_all_documents()?;
        let mut prepared = self.writer.prepare_commit()?;
        prepared.set_payload("0");
        prepared.commit()?;
        self.reader.reader.reload()?;
        self.applied_lsn = 0;
        Ok(())
    }

    pub fn remove_oids(&mut self, oids: &[u64]) -> Result<()> {
        if oids.is_empty() {
            return Ok(());
        }
        for oid in oids {
            self.writer
                .delete_term(Term::from_field_u64(self.reader.f_oid, *oid));
        }
        let mut prepared = self.writer.prepare_commit()?;
        prepared.set_payload(&self.applied_lsn.to_string());
        prepared.commit()?;
        self.reader.reader.reload()?;
        Ok(())
    }

    /// Replace the whole disposable projection with the catalog's current
    /// content identities, sourcing every extraction from CAES.
    pub fn rebuild_current(
        &mut self,
        target_lsn: u64,
        catalog: &Catalog,
        caes: &Store,
    ) -> Result<()> {
        self.writer.delete_all_documents()?;
        for binding in catalog.content_bindings()? {
            let key = CaesKey {
                content_hash: binding.content_hash,
                extractor_id: rsd_extract::EXTRACTOR_ID.into(),
                extractor_version: rsd_extract::EXTRACTOR_VERSION,
                hints_hash: binding.hints_hash,
                abi_version: ABI_VERSION,
            };
            let Some(record) = caes.get(&key)? else {
                continue;
            };
            let mut doc = TantivyDocument::default();
            doc.add_u64(self.reader.f_oid, binding.oid);
            doc.add_text(self.reader.f_content, &record.text);
            for symbol in &record.symbols {
                doc.add_text(self.reader.f_symbols, symbol.name.to_lowercase());
            }
            add_path_scopes(&mut doc, self.reader.f_path_scope, &binding.paths);
            self.writer.add_document(doc)?;
        }
        let mut prepared = self.writer.prepare_commit()?;
        prepared.set_payload(&target_lsn.to_string());
        prepared.commit()?;
        self.reader.reader.reload()?;
        self.applied_lsn = target_lsn;
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

fn add_path_scopes(doc: &mut TantivyDocument, field: Field, paths: &[String]) {
    let mut scopes = std::collections::HashSet::new();
    for path in paths {
        for ancestor in Path::new(path).ancestors() {
            if !ancestor.as_os_str().is_empty() {
                scopes.insert(ancestor.to_string_lossy().into_owned());
            }
        }
    }
    for scope in scopes {
        doc.add_text(field, scope);
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

    pub fn search_content_scoped(
        &self,
        terms: &str,
        phrase: bool,
        scopes: &[&Path],
        limit: usize,
    ) -> Result<Vec<u64>> {
        self.search_field_scoped(self.f_content, terms, phrase, scopes, limit)
    }

    /// Symbol search → matching oids.
    pub fn search_symbols(&self, terms: &str, limit: usize) -> Result<Vec<u64>> {
        self.search_field(self.f_symbols, terms, false, limit)
    }

    pub fn search_symbols_scoped(
        &self,
        terms: &str,
        scopes: &[&Path],
        limit: usize,
    ) -> Result<Vec<u64>> {
        self.search_field_scoped(self.f_symbols, terms, false, scopes, limit)
    }

    fn search_field_scoped(
        &self,
        field: Field,
        terms: &str,
        phrase: bool,
        scopes: &[&Path],
        limit: usize,
    ) -> Result<Vec<u64>> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let Some(content) = self.field_query(field, terms, phrase) else {
            return Ok(Vec::new());
        };
        let mut unique = std::collections::HashSet::new();
        let scope_queries: Vec<(Occur, Box<dyn Query>)> = scopes
            .iter()
            .filter_map(|scope| {
                let text = scope.to_string_lossy().into_owned();
                unique.insert(text.clone()).then(|| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.f_path_scope, &text),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    )
                })
            })
            .collect();
        let query = BooleanQuery::new(vec![
            (Occur::Must, content),
            (
                Occur::Must,
                Box::new(BooleanQuery::new(scope_queries)) as Box<dyn Query>,
            ),
        ]);
        self.search_query(&query, limit)
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
        self.search_query(query.as_ref(), limit)
    }

    fn search_query(&self, query: &dyn Query, limit: usize) -> Result<Vec<u64>> {
        let searcher = self.reader.searcher();
        let top = searcher.search(query, &TopDocs::with_limit(limit.max(1)))?;
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
    plane.rebuild_current(journal.max_lsn(), catalog, caes)?;
    Ok(plane)
}
