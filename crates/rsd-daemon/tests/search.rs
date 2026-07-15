//! P4 end-to-end: index a live tree, answer RQL queries, stay fresh across
//! renames, and prove the failure-matrix row "lexical segment lost → rebuild
//! from CAES with ZERO filesystem reads / re-extractions".

use rsd_caes::Store;
use rsd_catalog::{Catalog, Durability};
use rsd_daemon::{bring_up, ContentIndexer, ContentSource, PipelineConfig};
use rsd_extract::{extract_bytes, Budgets, ExtractHints};
use rsd_ingest::CoalescerConfig;
use rsd_lexical::{LexicalPlane, LexicalReader};
use rsd_log::{Journal, JournalConfig};
use rsd_query::{parse, Plan, QueryEngine};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct CountingSource {
    calls: Arc<AtomicU64>,
}

impl ContentSource for CountingSource {
    fn extract_file(
        &mut self,
        path: &Path,
        hints: &ExtractHints,
        budgets: &Budgets,
    ) -> Result<rsd_caes::ExtractionRecord, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        Ok(extract_bytes(hints, budgets, &bytes))
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    cat: Arc<Catalog>,
    caes: Arc<Store>,
    calls: Arc<AtomicU64>,
    pipeline: rsd_daemon::Pipeline,
}

fn fast_cfg() -> PipelineConfig {
    PipelineConfig {
        coalescer: CoalescerConfig {
            quiet: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
            max_pending: 65_536,
        },
        fsevents_latency: Duration::from_millis(50),
        journal_sync: false,
        ..Default::default()
    }
}

fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("tree");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("invoice.txt"),
        "Please find the quarterly invoice attached, net-60 payment terms.",
    )
    .unwrap();
    std::fs::write(
        root.join("notes.md"),
        "meeting notes about the flux capacitor prototype",
    )
    .unwrap();
    std::fs::write(
        root.join("engine.rs"),
        "pub fn ignite_thrusters() {}\npub struct WarpCore { dilithium: u64 }\n",
    )
    .unwrap();

    let cat =
        Arc::new(Catalog::open_with_durability(&base.join("cat.redb"), Durability::None).unwrap());
    let caes = Arc::new(Store::open(&base.join("caes.redb")).unwrap());
    let calls = Arc::new(AtomicU64::new(0));
    let indexer = ContentIndexer::new(
        Box::new(CountingSource {
            calls: calls.clone(),
        }),
        caes.clone(),
    );
    let plane = LexicalPlane::open(&base.join("lexical")).unwrap();
    let (pipeline, _) = bring_up(
        cat.clone(),
        &base.join("journal"),
        &root,
        Some(indexer),
        Some((plane, caes.clone())),
        None,
        None,
        fast_cfg(),
    )
    .unwrap();
    Env {
        _tmp: tmp,
        base,
        root,
        cat,
        caes,
        calls,
        pipeline,
    }
}

/// Open a read-side engine over the same state dir (fresh plane handle).
fn query(env: &Env, rql: &str, scope: Option<&str>) -> Vec<String> {
    let plane = LexicalReader::open(&env.base.join("lexical")).unwrap();
    let engine = QueryEngine {
        catalog: &env.cat,
        lexical: Some(&plane),
        vector: None,
        limit: 1000,
    };
    let expr = parse(rql).unwrap();
    engine
        .run(&expr, scope)
        .unwrap()
        .into_iter()
        .map(|h| h.path)
        .collect()
}

fn wait_until(mut f: impl FnMut() -> bool, deadline: Duration, what: &str) {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for: {what}");
}

#[test]
fn text_symbol_attr_and_mixed_queries_answer() {
    let env = setup();
    // Bootstrap indexed everything synchronously in bring_up.
    let hits = query(&env, r#""invoice""#, None);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].ends_with("invoice.txt"));

    // Symbol search finds the function definition.
    let hits = query(&env, r#"kRSDSymbols == "ignite_thrusters""#, None);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].ends_with("engine.rs"));

    // Attribute predicate.
    let hits = query(&env, r#"kMDItemFSName == "*.md""#, None);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].ends_with("notes.md"));

    // Mixed: text AND name.
    let hits = query(
        &env,
        r#"kMDItemTextContent == "payment" && kMDItemFSName == "*.txt""#,
        None,
    );
    assert_eq!(hits.len(), 1);
    assert!(hits[0].ends_with("invoice.txt"));

    // Negation excludes.
    let hits = query(
        &env,
        r#"kMDItemFSSize > 0 && !(kMDItemFSName == "*.rs")"#,
        None,
    );
    assert!(hits.iter().all(|p| !p.ends_with(".rs")), "{hits:?}");

    // Plan surface: pure text is the lexical fast path.
    let plane = LexicalReader::open(&env.base.join("lexical")).unwrap();
    let engine = QueryEngine {
        catalog: &env.cat,
        lexical: Some(&plane),
        vector: None,
        limit: 10,
    };
    assert_eq!(
        engine.plan(&parse(r#""foo""#).unwrap()),
        Plan::LexicalDirect
    );
    env.pipeline.stop();
}

#[test]
fn renamed_file_answers_with_current_path_immediately() {
    let env = setup();
    let from = env.root.join("invoice.txt");
    let to = env.root.join("q3-invoice-final.txt");
    std::fs::rename(&from, &to).unwrap();

    // No re-extraction happens on rename, yet results must show the NEW path
    // (paths resolve through the catalog at query time — never stored stale).
    wait_until(
        || {
            let hits = query(&env, r#""invoice""#, None);
            hits.len() == 1 && hits[0].ends_with("q3-invoice-final.txt")
        },
        Duration::from_secs(15),
        "rename-fresh search result",
    );
    env.pipeline.stop();
}

#[test]
fn live_created_file_becomes_searchable() {
    let env = setup();
    std::fs::write(env.root.join("fresh.py"), "def quantum_entangle(): pass\n").unwrap();
    wait_until(
        || {
            let hits = query(&env, r#"kRSDSymbols == "quantum_entangle""#, None);
            hits.len() == 1
        },
        Duration::from_secs(15),
        "live file searchable",
    );
    env.pipeline.stop();
}

#[test]
fn lexical_plane_rebuilds_from_caes_with_zero_extractions() {
    let env = setup();
    let calls_before = env.calls.load(Ordering::Relaxed);
    assert!(calls_before >= 3);
    env.pipeline.stop(); // release the writer lock on the plane

    // Catastrophe: the whole lexical plane is destroyed.
    let lex_dir = env.base.join("lexical");
    std::fs::remove_dir_all(&lex_dir).unwrap();

    // Failure-matrix repair: rebuild purely from journal + CAES.
    let journal = Journal::open(
        &env.base.join("journal"),
        JournalConfig {
            sync_on_append: false,
            ..Default::default()
        },
    )
    .unwrap();
    let plane = rsd_lexical::rebuild(&lex_dir, &journal, &env.cat, &env.caes).unwrap();
    assert!(plane.doc_count().unwrap() >= 3);
    drop(plane);

    // Search works again...
    let hits = {
        let plane = LexicalReader::open(&lex_dir).unwrap();
        let engine = QueryEngine {
            catalog: &env.cat,
            lexical: Some(&plane),
            vector: None,
            limit: 100,
        };
        engine.run(&parse(r#""dilithium""#).unwrap(), None).unwrap()
    };
    assert_eq!(hits.len(), 1);
    assert!(hits[0].path.ends_with("engine.rs"));

    // ...and NOTHING was re-extracted: content came exclusively from CAES.
    assert_eq!(
        env.calls.load(Ordering::Relaxed),
        calls_before,
        "rebuild must not touch extractors or files"
    );
}
