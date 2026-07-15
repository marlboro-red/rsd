//! P5.2 property test: the single-doc matcher's membership answers are
//! IDENTICAL to the on-disk index's term-query results, for every doc, across
//! randomized corpora and queries. (Scoring parity is explicitly not claimed.)

use rand::prelude::*;
use rand_chacha::ChaCha8Rng;
use rsd_live::DocMatcher;
use std::collections::HashSet;
use tantivy::collector::DocSetCollector;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Schema, Value, FAST, INDEXED, STORED, TEXT};
use tantivy::{doc, Index, TantivyDocument, Term};

#[test]
fn matcher_membership_equals_index_membership() {
    let mut rng = ChaCha8Rng::seed_from_u64(4242);
    let vocab: Vec<String> = (0..40).map(|i| format!("word{i}")).collect();

    let mut schema_b = Schema::builder();
    let f_id = schema_b.add_u64_field("id", INDEXED | FAST | STORED);
    let f_content = schema_b.add_text_field("content", TEXT); // default analyzer
    let schema = schema_b.build();

    for trial in 0..20 {
        let index = Index::create_in_ram(schema.clone());
        let mut writer = index.writer(16 * 1024 * 1024).unwrap();
        let docs: Vec<String> = (0..50)
            .map(|_| {
                let n = rng.gen_range(3..25);
                (0..n)
                    .map(|_| vocab[rng.gen_range(0..vocab.len())].as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        for (i, text) in docs.iter().enumerate() {
            writer
                .add_document(doc!(f_id => i as u64, f_content => text.as_str()))
                .unwrap();
        }
        writer.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();
        let matcher = DocMatcher::new();

        for _ in 0..30 {
            let nq = rng.gen_range(1..4);
            let terms: Vec<&str> = (0..nq)
                .map(|_| vocab[rng.gen_range(0..vocab.len())].as_str())
                .collect();
            let query_str = terms.join(" ");

            // Index-side: AND of term queries (rsd-lexical's mapping).
            let q = BooleanQuery::new(
                terms
                    .iter()
                    .map(|t| {
                        (
                            Occur::Must,
                            Box::new(TermQuery::new(
                                Term::from_field_text(f_content, t),
                                IndexRecordOption::Basic,
                            )) as Box<dyn Query>,
                        )
                    })
                    .collect(),
            );
            let addrs = searcher.search(&q, &DocSetCollector).unwrap();
            let index_hits: HashSet<u64> = addrs
                .into_iter()
                .map(|a| {
                    let d: TantivyDocument = searcher.doc(a).unwrap();
                    d.get_first(f_id).and_then(|v| v.as_u64()).unwrap()
                })
                .collect();

            for (i, text) in docs.iter().enumerate() {
                assert_eq!(
                    matcher.matches_text(text, &query_str),
                    index_hits.contains(&(i as u64)),
                    "trial {trial} divergence: doc {i} query {query_str:?}"
                );
            }
        }
    }
}
