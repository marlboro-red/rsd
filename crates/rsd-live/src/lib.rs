//! rsd-live: standing queries as incrementally-maintained views (P5.1) and
//! the single-doc matcher (P5.2).
//!
//! Contract (DESIGN.md §9, exact class): attribute predicates, boolean text
//! membership, and their combinations are maintained point-incrementally from
//! the committed delta stream — enter/leave events, old-state evidence, no
//! re-query. A slow subscriber gets `Resync` instead of unbounded buffering.
//!
//! The single-doc matcher claims exactly what the design allows it to claim:
//! bit-identical TOKENIZATION and boolean MEMBERSHIP with the on-disk index
//! (same tantivy analyzer chain); scoring parity is explicitly not claimed.

use rsd_caes::{CaesKey, Store, ABI_VERSION};
use rsd_catalog::{Delta, ObjectKind, ObjectRecord};
use rsd_ipc::Scope;
use rsd_query::{eval_live, Expr};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::sync::Arc;
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer};

/// P5.2: tokenizes with tantivy's OWN default analyzer chain (SimpleTokenizer
/// → RemoveLong(40) → LowerCaser), so membership answers are bit-identical to
/// what the on-disk index would return for term queries.
pub struct DocMatcher {
    analyzer: std::sync::Mutex<TextAnalyzer>,
}

impl Default for DocMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl DocMatcher {
    pub fn new() -> DocMatcher {
        DocMatcher {
            analyzer: std::sync::Mutex::new(
                TextAnalyzer::builder(SimpleTokenizer::default())
                    .filter(RemoveLongFilter::limit(40))
                    .filter(LowerCaser)
                    .build(),
            ),
        }
    }

    pub fn tokens(&self, text: &str) -> Vec<String> {
        let mut an = self
            .analyzer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut stream = an.token_stream(text);
        let mut out = Vec::new();
        while let Some(tok) = stream.next() {
            out.push(tok.text.clone());
        }
        out
    }

    /// All query tokens present in the doc (AND membership, `*` stripped —
    /// identical to the index-side query mapping in rsd-lexical).
    pub fn matches_text(&self, doc_text: &str, query_terms: &str) -> bool {
        let doc: HashSet<String> = self.tokens(doc_text).into_iter().collect();
        let normalized_query = query_terms
            .split_whitespace()
            .map(|term| term.trim_matches('*'))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let q = self.tokens(&normalized_query);
        !q.is_empty() && q.iter().all(|t| doc.contains(t))
    }

    /// Symbols are raw-token, lowercased, exact (mirrors the STRING field).
    pub fn matches_symbols(&self, symbols: &[rsd_caes::SymbolRec], query: &str) -> bool {
        let q = query.trim_matches('*').to_lowercase();
        symbols.iter().any(|s| s.name.to_lowercase() == q)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    Enter {
        oid: u64,
        path: String,
    },
    Leave {
        oid: u64,
        path: String,
    },
    /// Subscriber fell behind; it must re-run the query and rejoin.
    Resync,
}

enum ViewKind {
    Expr(Expr),
    /// Semantic alert (P6.4): threshold classification by design — "is this
    /// new thing similar enough?" — never top-k (DESIGN.md §9).
    Alert {
        qvec: Vec<f32>,
        threshold: f32,
    },
}

struct View {
    kind: ViewKind,
    /// Scope-prefix authorization baked in at subscribe time (P5.3): deltas
    /// outside the granted prefixes are invisible — no events, no counts.
    scope: Scope,
    tx: mpsc::SyncSender<LiveEvent>,
    members: HashSet<u64>,
    needs_resync: bool,
}

pub struct LiveEngine {
    caes: Option<Arc<Store>>,
    embedder: Option<Arc<dyn rsd_vector::Embedder>>,
    matcher: DocMatcher,
    views: HashMap<u64, View>,
    next_id: u64,
    pub deltas_processed: u64,
}

impl LiveEngine {
    pub fn new(caes: Option<Arc<Store>>) -> LiveEngine {
        LiveEngine {
            caes,
            embedder: None,
            matcher: DocMatcher::new(),
            views: HashMap::new(),
            next_id: 1,
            deltas_processed: 0,
        }
    }

    /// Enable semantic alerts (requires an embedder for query vectors).
    pub fn set_embedder(&mut self, e: Arc<dyn rsd_vector::Embedder>) {
        self.embedder = Some(e);
    }

    /// Register a semantic alert: fires Enter when new/changed content clears
    /// the similarity threshold for `query`.
    pub fn subscribe_alert(
        &mut self,
        query: &str,
        threshold: f32,
        scope: Scope,
        buffer: usize,
    ) -> Option<(u64, mpsc::Receiver<LiveEvent>)> {
        let qvec = self.embedder.as_ref()?.embed(query);
        let (tx, rx) = mpsc::sync_channel(buffer.max(16));
        let id = self.next_id;
        self.next_id += 1;
        self.views.insert(
            id,
            View {
                kind: ViewKind::Alert { qvec, threshold },
                scope,
                tx,
                members: HashSet::new(),
                needs_resync: false,
            },
        );
        Some((id, rx))
    }

    /// Register a view. `initial_members` come from a one-shot query fenced by
    /// the caller before subscribing.
    pub fn subscribe(
        &mut self,
        expr: Expr,
        scope: Scope,
        initial_members: impl IntoIterator<Item = u64>,
        buffer: usize,
    ) -> (u64, mpsc::Receiver<LiveEvent>) {
        let (tx, rx) = mpsc::sync_channel(buffer.max(16));
        let id = self.next_id;
        self.next_id += 1;
        self.views.insert(
            id,
            View {
                kind: ViewKind::Expr(expr),
                scope,
                tx,
                members: initial_members.into_iter().collect(),
                needs_resync: false,
            },
        );
        (id, rx)
    }

    pub fn unsubscribe(&mut self, id: u64) {
        self.views.remove(&id);
    }

    pub fn view_members(&self, id: u64) -> Option<&HashSet<u64>> {
        self.views.get(&id).map(|v| &v.members)
    }

    fn text_of(
        caes: &Option<Arc<Store>>,
        rec: &ObjectRecord,
    ) -> Option<rsd_caes::ExtractionRecord> {
        let caes = caes.as_ref()?;
        let (Some(ch), Some(hh)) = (rec.content_hash, rec.caes_hints_hash) else {
            return None;
        };
        caes.get(&CaesKey {
            content_hash: ch,
            extractor_id: rsd_extract::EXTRACTOR_ID.into(),
            extractor_version: rsd_extract::EXTRACTOR_VERSION,
            hints_hash: hh,
            abi_version: ABI_VERSION,
        })
        .ok()
        .flatten()
    }

    /// Feed one committed batch's deltas through every view.
    pub fn on_commit(&mut self, deltas: &[Delta]) {
        self.deltas_processed += deltas.len() as u64;
        let caes = self.caes.clone();
        let matcher = &self.matcher;
        let mut dead: Vec<u64> = Vec::new();

        for (id, view) in self.views.iter_mut() {
            // A pending Resync outranks everything: deliver it as soon as a
            // slot frees; drop events until it lands (the client re-fetches).
            if view.needs_resync {
                match view.tx.try_send(LiveEvent::Resync) {
                    Ok(()) => view.needs_resync = false,
                    Err(mpsc::TrySendError::Full(_)) => continue,
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        dead.push(*id);
                        continue;
                    }
                }
            }
            for d in deltas {
                if !view.scope.allows(&d.path) {
                    continue;
                }
                let new_state = d.new.as_ref().filter(|(_, r)| r.kind != ObjectKind::Dir);
                let new_match = match new_state {
                    Some((_, rec)) => match &view.kind {
                        ViewKind::Expr(expr) => {
                            let cache = Self::text_of(&caes, rec);
                            eval_live(expr, &d.path, rec, &|terms, symbols| match &cache {
                                Some(r) if symbols => matcher.matches_symbols(&r.symbols, terms),
                                Some(r) => matcher.matches_text(&r.text, terms),
                                None => false,
                            })
                        }
                        ViewKind::Alert { qvec, threshold } => {
                            match (Self::text_of(&caes, rec), self.embedder.as_ref()) {
                                (Some(r), Some(emb)) => {
                                    let best = rsd_vector::chunk(&r.text)
                                        .into_iter()
                                        .map(|(_, c)| {
                                            let v = emb.embed(&c);
                                            v.iter().zip(qvec).map(|(a, b)| a * b).sum::<f32>()
                                        })
                                        .fold(0f32, f32::max);
                                    best >= *threshold
                                }
                                _ => false,
                            }
                        }
                    },
                    None => false,
                };
                let oid_new = new_state.map(|(o, _)| *o);
                let oid_old = d.old.as_ref().map(|(o, _)| *o);

                let mut events: Vec<LiveEvent> = Vec::new();
                match (oid_new, new_match) {
                    (Some(oid), true) => {
                        if view.members.insert(oid) {
                            events.push(LiveEvent::Enter {
                                oid,
                                path: d.path.clone(),
                            });
                        }
                    }
                    (Some(oid), false) => {
                        if view.members.remove(&oid) {
                            events.push(LiveEvent::Leave {
                                oid,
                                path: d.path.clone(),
                            });
                        }
                    }
                    (None, _) => {
                        if let Some(oid) = oid_old {
                            if view.members.remove(&oid) {
                                events.push(LiveEvent::Leave {
                                    oid,
                                    path: d.path.clone(),
                                });
                            }
                        }
                    }
                }
                for ev in events {
                    match view.tx.try_send(ev) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(_)) => {
                            // Bounded buffer overflowed: degrade to resync,
                            // never queue unboundedly (DESIGN.md §9).
                            view.needs_resync = true;
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => {
                            dead.push(*id);
                        }
                    }
                }
            }
        }
        for id in dead {
            self.views.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsd_catalog::{FileId, StatInfo};

    fn rec(size: u64) -> ObjectRecord {
        let st = StatInfo {
            kind: ObjectKind::File,
            file_id: FileId { dev: 1, ino: 1 },
            size,
            mtime_ns: 1,
            birthtime_ns: 1,
            nlink: 1,
        };
        ObjectRecord {
            kind: st.kind,
            file_id: st.file_id,
            birthtime_ns: 1,
            size: st.size,
            mtime_ns: 1,
            nlink: 1,
            entry_paths: vec![],
            orphaned_at_ns: None,
            content_hash: None,
            index_state: None,
            caes_hints_hash: None,
        }
    }

    fn delta(path: &str, old: Option<(u64, u64)>, new: Option<(u64, u64)>) -> Delta {
        Delta {
            path: path.into(),
            old: old.map(|(o, s)| (o, rec(s))),
            new: new.map(|(o, s)| (o, rec(s))),
        }
    }

    #[test]
    fn attr_view_enters_leaves_and_dedupes() {
        let mut eng = LiveEngine::new(None);
        let expr = rsd_query::parse(r#"kMDItemFSSize > 100"#).unwrap();
        let (id, rx) = eng.subscribe(expr, Scope::Unrestricted, [], 64);

        eng.on_commit(&[delta("/r/a", None, Some((1, 500)))]);
        assert_eq!(
            rx.try_recv().unwrap(),
            LiveEvent::Enter {
                oid: 1,
                path: "/r/a".into()
            }
        );
        // Same state again: no duplicate event.
        eng.on_commit(&[delta("/r/a", Some((1, 500)), Some((1, 500)))]);
        assert!(rx.try_recv().is_err());
        // Shrinks below threshold: leave.
        eng.on_commit(&[delta("/r/a", Some((1, 500)), Some((1, 50)))]);
        assert_eq!(
            rx.try_recv().unwrap(),
            LiveEvent::Leave {
                oid: 1,
                path: "/r/a".into()
            }
        );
        // Removal of a non-member: silence.
        eng.on_commit(&[delta("/r/a", Some((1, 50)), None)]);
        assert!(rx.try_recv().is_err());
        assert!(eng.view_members(id).unwrap().is_empty());
    }

    #[test]
    fn scope_grants_hide_deltas_entirely() {
        let mut eng = LiveEngine::new(None);
        let expr = rsd_query::parse(r#"kMDItemFSSize > 0"#).unwrap();
        let (_, rx) = eng.subscribe(expr, Scope::paths(["/a"]), [], 64);
        eng.on_commit(&[
            delta("/a/x", None, Some((1, 10))),
            delta("/a-private/sibling", None, Some((3, 10))),
            delta("/b/secret", None, Some((2, 10))),
        ]);
        assert!(matches!(
            rx.try_recv().unwrap(),
            LiveEvent::Enter { oid: 1, .. }
        ));
        assert!(rx.try_recv().is_err(), "out-of-scope delta leaked!");
    }

    #[test]
    fn empty_path_scope_is_deny_all_for_live_deltas() {
        let mut eng = LiveEngine::new(None);
        let expr = rsd_query::parse(r#"kMDItemFSSize > 0"#).unwrap();
        let (_, rx) = eng.subscribe(expr, Scope::default(), [], 64);
        eng.on_commit(&[delta("/secret", None, Some((1, 10)))]);
        assert!(rx.try_recv().is_err(), "deny-all scope leaked a live delta");
    }

    #[test]
    fn slow_subscriber_gets_resync_not_unbounded_buffering() {
        let mut eng = LiveEngine::new(None);
        let expr = rsd_query::parse(r#"kMDItemFSSize > 0"#).unwrap();
        let (_, rx) = eng.subscribe(expr, Scope::Unrestricted, [], 16);
        for i in 0..200u64 {
            eng.on_commit(&[delta(&format!("/r/f{i}"), None, Some((i + 1, 10)))]);
        }
        // Drain the stale backlog (frees slots), then the next commit must
        // deliver the Resync marker before anything else.
        while rx.try_recv().is_ok() {}
        eng.on_commit(&[delta("/r/more", None, Some((999, 10)))]);
        assert_eq!(rx.try_recv().unwrap(), LiveEvent::Resync);
    }
}
