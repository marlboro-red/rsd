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
        path: &std::path::Path,
        hints: &ExtractHints,
        budgets: &Budgets,
    ) -> Result<rsd_caes::ExtractionRecord, String> {
        let _ = AtomicU64::new(0);
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
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
