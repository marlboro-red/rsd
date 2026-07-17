//! P6 e2e: the semantic plane through the full daemon — semantic(), hybrid
//! RRF, and freshness on live changes.

use rsd_caes::Store;
use rsd_catalog::{Catalog, Durability};
use rsd_daemon::{bring_up, ContentIndexer, ContentSource, PipelineConfig};
use rsd_extract::{extract_bytes, Budgets, ExtractHints};
use rsd_ingest::CoalescerConfig;
use rsd_lexical::{LexicalPlane, LexicalReader};
use rsd_query::{parse, QueryEngine};
use rsd_vector::{HashEmbedder, VectorPlane};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Src;
impl ContentSource for Src {
    fn extract_file(
        &mut self,
        file: &std::fs::File,
        _path: &std::path::Path,
        hints: &ExtractHints,
        budgets: &Budgets,
    ) -> Result<rsd_caes::ExtractionRecord, String> {
        let _ = AtomicU64::new(0);
        let mut file = file.try_clone().map_err(|error| error.to_string())?;
        std::io::Seek::rewind(&mut file).map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes).map_err(|error| error.to_string())?;
        Ok(extract_bytes(hints, budgets, &bytes))
    }
}

#[test]
fn semantic_and_hybrid_queries_answer_through_the_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("tree");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("billing.txt"),
        "quarterly invoice with net sixty payment terms and billing schedule",
    )
    .unwrap();
    std::fs::write(
        root.join("engine.txt"),
        "warp core dilithium chamber engineering maintenance schedule",
    )
    .unwrap();
    std::fs::write(root.join("recipe.txt"), "sourdough bread flour water salt").unwrap();

    let cat =
        Arc::new(Catalog::open_with_durability(&base.join("cat.redb"), Durability::None).unwrap());
    let caes = Arc::new(Store::open(&base.join("caes.redb")).unwrap());
    let plane = LexicalPlane::open(&base.join("lexical")).unwrap();
    let vplane = Arc::new(Mutex::new(
        VectorPlane::open(&base.join("vector.redb"), Arc::new(HashEmbedder::default())).unwrap(),
    ));
    let indexer = ContentIndexer::new(Box::new(Src), caes.clone());
    let (pipeline, _) = bring_up(
        cat.clone(),
        &base.join("journal"),
        &root,
        Some(indexer),
        Some((plane, caes.clone())),
        Some((vplane.clone(), caes)),
        None,
        PipelineConfig {
            coalescer: CoalescerConfig {
                quiet: Duration::from_millis(100),
                max_delay: Duration::from_secs(1),
                max_pending: 65_536,
            },
            fsevents_latency: Duration::from_millis(50),
            journal_sync: false,
            ..Default::default()
        },
    )
    .unwrap();

    let reader = LexicalReader::open(&base.join("lexical")).unwrap();
    let vguard = vplane.lock().unwrap();
    let engine = QueryEngine {
        catalog: &cat,
        lexical: Some(&reader),
        vector: Some(&vguard),
        limit: 10,
    };

    // Pure semantic: paraphrase-ish query ranks the billing doc first.
    let hits = engine
        .run(
            &parse(r#"semantic("invoice payment quarterly")"#).unwrap(),
            None,
        )
        .unwrap();
    assert!(!hits.is_empty());
    assert!(hits[0].path.ends_with("billing.txt"), "{hits:?}");

    // Hybrid RRF: a term both planes agree on wins; agreement beats either alone.
    let hits = engine.hybrid("schedule maintenance dilithium", 5).unwrap();
    assert!(hits[0].path.ends_with("engine.txt"), "{hits:?}");

    // Scoped hybrid retrieval constrains both lexical and semantic candidates
    // before fusion; an out-of-scope high rank cannot consume the budget.
    let billing_scope = root.join("billing.txt");
    let hits = engine
        .hybrid_tagged_authorized("schedule maintenance", 5, &[billing_scope])
        .unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].0.path.ends_with("billing.txt"), "{hits:?}");

    // Mixed predicate: semantic AND attribute.
    let hits = engine
        .run(
            &parse(r#"semantic("payment invoice") && kMDItemFSName == "*.txt""#).unwrap(),
            None,
        )
        .unwrap();
    assert!(hits.iter().any(|h| h.path.ends_with("billing.txt")));
    drop(vguard);
    pipeline.stop();
}

#[test]
fn semantic_alert_fires_on_similar_content_only() {
    use rsd_daemon::ipc::{start_ipc, AuthzStore, IpcCtx};
    use rsd_ipc::{recv, send, Request, Response};
    use rsd_live::LiveEngine;

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("tree");
    std::fs::create_dir(&root).unwrap();

    let cat =
        Arc::new(Catalog::open_with_durability(&base.join("cat.redb"), Durability::None).unwrap());
    let caes = Arc::new(Store::open(&base.join("caes.redb")).unwrap());
    let plane = LexicalPlane::open(&base.join("lexical")).unwrap();
    let live = Arc::new(Mutex::new({
        let mut e = LiveEngine::new(Some(caes.clone()));
        e.set_embedder(Arc::new(HashEmbedder::default()));
        e
    }));
    let indexer = ContentIndexer::new(Box::new(Src), caes.clone());
    let (pipeline, _) = bring_up(
        cat.clone(),
        &base.join("journal"),
        &root,
        Some(indexer),
        Some((plane, caes)),
        None,
        Some(live.clone()),
        PipelineConfig {
            coalescer: CoalescerConfig {
                quiet: Duration::from_millis(100),
                max_delay: Duration::from_secs(1),
                max_pending: 65_536,
            },
            fsevents_latency: Duration::from_millis(50),
            journal_sync: false,
            ..Default::default()
        },
    )
    .unwrap();
    let sock = base.join("rsd.sock");
    let mut authz = AuthzStore::default();
    authz.grant_unrestricted("t");
    start_ipc(
        &sock,
        IpcCtx {
            catalog: cat,
            lexical_dir: base.join("lexical"),
            vector: None,
            live,
            authz: Arc::new(authz),
        },
    )
    .unwrap();

    let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    send(
        &mut s,
        &Request::Hello {
            principal: "t".into(),
        },
    )
    .unwrap();
    let _: Response = recv(&mut s).unwrap();
    send(
        &mut s,
        &Request::SubscribeAlert {
            query: "invoice payment terms billing".into(),
            threshold: 0.3,
        },
    )
    .unwrap();
    let Response::Subscribed(_) = recv::<Response>(&mut s).unwrap() else {
        panic!("no sub ack")
    };

    // Unrelated content first: must NOT fire.
    std::fs::write(
        root.join("recipe.txt"),
        "sourdough bread flour water salt yeast",
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    // Similar content: must fire.
    std::fs::write(
        root.join("q3.txt"),
        "the quarterly invoice includes payment terms and a billing schedule",
    )
    .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    match recv::<Response>(&mut s).unwrap() {
        Response::Event {
            enter: true, path, ..
        } => {
            assert!(
                path.ends_with("q3.txt"),
                "alert fired for wrong file: {path}"
            );
        }
        other => panic!("expected alert Enter, got {other:?}"),
    }
    pipeline.stop();
}
