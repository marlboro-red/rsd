//! DESIGN.md §9 / §16 spike 4: the single-doc matcher and the on-disk plane
//! must agree on **tokenization and boolean membership**, bit for bit.
//!
//! Scoring parity is explicitly not claimed — BM25 depends on corpus
//! statistics — so this asserts membership only: for a document and a query,
//! `LexicalReader::search_content` finds it exactly when
//! `DocMatcher::matches_text` says it matches.
//!
//! This is a cross-crate property because the two implementations are the
//! thing under test. They previously agreed only on unpunctuated ASCII: the
//! plane searched for whitespace-split terms while indexing analyzer-split
//! ones, so `foo-bar` matched in a live view and found nothing on disk.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rsd_live::DocMatcher;

/// Vocabulary chosen to cover the cases where the two tokenizers can disagree:
/// internal punctuation, case, Unicode, and terms near the 40-byte length cap
/// that `RemoveLongFilter` drops.
const WORDS: &[&str] = &[
    "quarterly",
    "invoice",
    "foo-bar",
    "o'brien",
    "state-of-the-art",
    "CI/CD",
    "C++",
    "don't",
    "İstanbul",
    "naïve",
    "ignite_thrusters",
    "report.pdf",
    "UPPER",
    "MiXeD",
    "a",
    "ünïcödé-wörd",
    "supercalifragilisticexpialidocious-and-then-some-more-length",
    "42",
    "v1.2.3",
    "e2e",
];

fn corpus(rng: &mut ChaCha8Rng, len: usize) -> String {
    (0..len)
        .map(|_| WORDS[rng.gen_range(0..WORDS.len())])
        .collect::<Vec<_>>()
        .join(" ")
}

/// Index one document and report whether each query finds it.
fn plane_matches(text: &str, queries: &[String]) -> Vec<bool> {
    let tmp = tempfile::tempdir().unwrap();
    let reader = rsd_lexical::single_doc_index(tmp.path(), 1, text).unwrap();
    queries
        .iter()
        .map(|q| !reader.search_content(q, false, 10).unwrap().is_empty())
        .collect()
}

#[test]
fn plane_and_single_doc_matcher_agree_on_membership() {
    let matcher = DocMatcher::new();
    let mut rng = ChaCha8Rng::seed_from_u64(0x5EED);
    let mut checked = 0usize;

    for case in 0..60 {
        let len = rng.gen_range(1..24);
        let text = corpus(&mut rng, len);

        // Queries drawn from the document (must match) and from outside it
        // (must not) — a rule that only ever said "no" would pass otherwise.
        let mut queries: Vec<String> = Vec::new();
        let words: Vec<&str> = text.split_whitespace().collect();
        for _ in 0..3 {
            queries.push(words[rng.gen_range(0..words.len())].to_string());
        }
        // Multi-word conjunctions, including wildcard-marked terms.
        if words.len() >= 2 {
            let a = words[rng.gen_range(0..words.len())];
            let b = words[rng.gen_range(0..words.len())];
            queries.push(format!("{a} {b}"));
            queries.push(format!("*{a}*"));
        }
        queries.push(format!("absent{case}"));
        queries.push(WORDS[rng.gen_range(0..WORDS.len())].to_string());

        for (query, plane) in queries.iter().zip(plane_matches(&text, &queries)) {
            let live = matcher.matches_text(&text, query);
            assert_eq!(
                plane, live,
                "membership disagreement\n  text:  {text:?}\n  query: {query:?}\n  \
                 plane={plane} matcher={live}"
            );
            checked += 1;
        }
    }
    assert!(checked > 300, "property exercised too few pairs: {checked}");
}

/// The specific shapes that regressed, kept as named cases so a failure names
/// the cause instead of a random seed.
#[test]
fn known_divergence_shapes_agree() {
    let matcher = DocMatcher::new();
    let text = "quarterly foo-bar report o'brien İstanbul ignite_thrusters end";
    for query in [
        "foo-bar",
        "o'brien",
        "FOO-BAR",
        "İSTANBUL",
        "ignite_thrusters",
        "quarterly report",
        "*foo-bar*",
        "missing",
        "foo-bar missing",
    ] {
        let plane = plane_matches(text, &[query.to_string()])[0];
        let live = matcher.matches_text(text, query);
        assert_eq!(plane, live, "disagreement on {query:?}");
    }
}
