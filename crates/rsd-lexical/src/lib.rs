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
use tantivy::collector::{Count, TopDocs};
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

/// The tokenizer the `content` field is indexed with. Named here so the
/// single-doc matcher can resolve the *same registered analyzer* instead of
/// rebuilding an equivalent chain by hand (DESIGN.md §9).
pub const CONTENT_TOKENIZER: &str = "default";

/// Strip the RQL wildcard markers a caller may have left on each word. Shared
/// with `rsd_live`'s single-doc matcher, which must normalize identically.
pub fn strip_wildcards(terms: &str) -> String {
    terms
        .split_whitespace()
        .map(|term| term.trim_matches('*'))
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build a throwaway index holding one document's content, using the real
/// schema and the real analyzer.
///
/// Exists for the §9 membership property: comparing the single-doc matcher
/// against the on-disk plane requires a plane containing exactly that
/// document, and building one through the commit pipeline would test the
/// pipeline rather than the tokenizers.
pub fn single_doc_index(dir: &Path, oid: u64, text: &str) -> Result<LexicalReader> {
    let reader = LexicalReader::open(dir)?;
    let mut writer = reader.index.writer(15_000_000)?;
    let mut doc = TantivyDocument::default();
    doc.add_u64(reader.f_oid, oid);
    doc.add_text(reader.f_content, text);
    writer.add_document(doc)?;
    writer.commit()?;
    reader.reader.reload()?;
    Ok(reader)
}

impl LexicalReader {
    /// The name of the tokenizer a field is *indexed* with, straight from the
    /// schema. Asking the schema rather than assuming is the whole point: the
    /// query side previously assumed whitespace splitting for every field,
    /// which silently disagreed with `content`'s analyzer on any term
    /// containing punctuation.
    fn tokenizer_name(&self, field: Field) -> Option<String> {
        match self.index.schema().get_field_entry(field).field_type() {
            tantivy::schema::FieldType::Str(options) => options
                .get_indexing_options()
                .map(|indexing| indexing.tokenizer().to_string()),
            _ => None,
        }
    }

    /// The exact index terms a query string produces for a field.
    ///
    /// Analyzed fields (`content`) run through the *registered* analyzer — the
    /// same instance the writer used — so a query term can never be a string
    /// the indexer would have split. Raw fields (`symbols`) are whole-value
    /// terms, lowercased to match how the writer stores them.
    fn query_terms(&self, field: Field, terms: &str) -> Vec<String> {
        let normalized = strip_wildcards(terms);
        if normalized.is_empty() {
            return Vec::new();
        }
        match self.tokenizer_name(field).as_deref() {
            // `raw` is tantivy's untokenized analyzer (what STRING selects).
            None | Some("raw") => normalized
                .split_whitespace()
                .map(|term| term.to_lowercase())
                .collect(),
            Some(name) => {
                let Some(mut analyzer) = self.index.tokenizers().get(name) else {
                    // A field indexed with an analyzer we cannot resolve must
                    // not silently fall back to whitespace splitting — that is
                    // the bug this function exists to prevent.
                    return Vec::new();
                };
                let mut stream = analyzer.token_stream(&normalized);
                let mut out = Vec::new();
                while let Some(token) = stream.next() {
                    out.push(token.text.clone());
                }
                out
            }
        }
    }

    /// Membership query over a field. Non-phrase queries are an AND over the
    /// analyzed terms — boolean membership, matching `rsd_live::DocMatcher`
    /// exactly (DESIGN.md §9). `phrase` additionally requires adjacency.
    fn field_query(&self, field: Field, terms: &str, phrase: bool) -> Option<Box<dyn Query>> {
        let toks = self.query_terms(field, terms);
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

    /// Exact number of content documents matching the query. Unlike ranked
    /// search, this has no top-k result cap.
    pub fn count_content(&self, terms: &str, phrase: bool) -> Result<u64> {
        self.count_field(self.f_content, terms, phrase)
    }

    pub fn count_content_scoped(&self, terms: &str, phrase: bool, scopes: &[&Path]) -> Result<u64> {
        self.count_field_scoped(self.f_content, terms, phrase, scopes)
    }

    /// Exact single-document membership used by mixed-predicate catalog
    /// scans. Keeping this as an index query avoids materializing a capped set
    /// of matching object ids.
    pub fn matches_content(&self, oid: u64, terms: &str, phrase: bool) -> Result<bool> {
        self.matches_field(self.f_content, oid, terms, phrase)
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

    pub fn matches_symbols(&self, oid: u64, terms: &str) -> Result<bool> {
        self.matches_field(self.f_symbols, oid, terms, false)
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

    fn count_field(&self, field: Field, terms: &str, phrase: bool) -> Result<u64> {
        let Some(query) = self.field_query(field, terms, phrase) else {
            return Ok(0);
        };
        self.count_query(query.as_ref())
    }

    fn count_field_scoped(
        &self,
        field: Field,
        terms: &str,
        phrase: bool,
        scopes: &[&Path],
    ) -> Result<u64> {
        if scopes.is_empty() {
            return Ok(0);
        }
        let Some(content) = self.field_query(field, terms, phrase) else {
            return Ok(0);
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
        self.count_query(&query)
    }

    fn matches_field(&self, field: Field, oid: u64, terms: &str, phrase: bool) -> Result<bool> {
        let Some(content) = self.field_query(field, terms, phrase) else {
            return Ok(false);
        };
        let query = BooleanQuery::new(vec![
            (Occur::Must, content),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(self.f_oid, oid),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
        ]);
        Ok(self.count_query(&query)? != 0)
    }

    fn count_query(&self, query: &dyn Query) -> Result<u64> {
        let searcher = self.reader.searcher();
        Ok(searcher.search(query, &Count)? as u64)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `CONTENT_TOKENIZER` is what the matcher resolves; the schema says what
    /// the writer actually used. If `TEXT` ever stops meaning "default", the
    /// two sides would silently diverge again — so assert they agree.
    #[test]
    fn content_tokenizer_constant_matches_the_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let reader = LexicalReader::open(tmp.path()).unwrap();
        assert_eq!(
            reader.tokenizer_name(reader.f_content).as_deref(),
            Some(CONTENT_TOKENIZER)
        );
        assert_eq!(
            reader.tokenizer_name(reader.f_symbols).as_deref(),
            Some("raw")
        );
        assert!(reader.index.tokenizers().get(CONTENT_TOKENIZER).is_some());
    }

    /// The bug: query terms were whitespace-split while `content` is indexed
    /// with an analyzer that splits on punctuation, so any punctuated term was
    /// searched for as a whole and never matched.
    #[test]
    fn punctuated_query_terms_match_analyzed_content() {
        let tmp = tempfile::tempdir().unwrap();
        let reader = LexicalReader::open(tmp.path()).unwrap();
        let mut writer = reader.index.writer(15_000_000).unwrap();
        let mut doc = TantivyDocument::default();
        doc.add_u64(reader.f_oid, 1);
        doc.add_text(reader.f_content, "quarterly foo-bar report o'brien end");
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
        reader.reader.reload().unwrap();

        for query in ["foo-bar", "o'brien", "FOO-BAR", "quarterly", "foo"] {
            assert!(
                !reader.search_content(query, false, 10).unwrap().is_empty(),
                "query {query:?} should match the indexed content"
            );
        }
        assert!(reader
            .search_content("absent-term", false, 10)
            .unwrap()
            .is_empty());
        // Raw fields keep whole-value semantics: an identifier is not split.
        assert_eq!(
            reader.query_terms(reader.f_symbols, "ignite_thrusters"),
            vec!["ignite_thrusters".to_string()]
        );
        assert_eq!(
            reader.query_terms(reader.f_content, "ignite_thrusters"),
            vec!["ignite".to_string(), "thrusters".to_string()]
        );
    }

    #[test]
    fn exact_count_is_not_truncated_by_ranked_search_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let reader = LexicalReader::open(tmp.path()).unwrap();
        let mut writer = reader.index.writer(64 * 1024 * 1024).unwrap();
        for oid in 1..=10_025u64 {
            let mut doc = TantivyDocument::default();
            doc.add_u64(reader.f_oid, oid);
            doc.add_text(reader.f_content, "shared marker");
            doc.add_text(reader.f_path_scope, "/authorized");
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();
        reader.reader.reload().unwrap();

        assert_eq!(
            reader
                .search_content("shared", false, 10_000)
                .unwrap()
                .len(),
            10_000
        );
        assert_eq!(reader.count_content("shared", false).unwrap(), 10_025);
        assert_eq!(
            reader
                .count_content_scoped("shared", false, &[Path::new("/authorized")])
                .unwrap(),
            10_025
        );
    }
}
