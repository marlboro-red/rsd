//! P6.1 proof: the learned embedder captures MEANING, not vocabulary — the
//! thing the hash embedder cannot do by construction. Runs when the model is
//! present (scripts/fetch-model.sh); skips cleanly otherwise.

use rsd_ml::MiniLmEmbedder;
use rsd_vector::Embedder;

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
fn paraphrases_beat_unrelated_text_with_zero_shared_words() {
    let dir = MiniLmEmbedder::default_dir();
    if !dir.join("model.safetensors").exists() {
        eprintln!("model absent; run scripts/fetch-model.sh — skipping");
        return;
    }
    let e = MiniLmEmbedder::load(&dir).expect("load");
    assert_eq!(e.dim(), 384);

    // Zero word overlap between query and target.
    let query = e.embed("automobile repair costs");
    let target = e.embed("the mechanic charged a fortune to fix my car");
    let distractor = e.embed("the recipe calls for flour, water and salt");
    let (hit, miss) = (dot(&query, &target), dot(&query, &distractor));
    assert!(
        hit > miss + 0.15,
        "semantic gap too small: hit={hit:.3} miss={miss:.3}"
    );

    // Classic sanity: king/queen closer than king/toaster.
    let king = e.embed("the king ruled the kingdom");
    let queen = e.embed("the queen governed the realm");
    let toaster = e.embed("this toaster has four slots");
    assert!(dot(&king, &queen) > dot(&king, &toaster) + 0.2);
}

#[test]
fn embedding_latency_is_practical() {
    let dir = MiniLmEmbedder::default_dir();
    if !dir.join("model.safetensors").exists() {
        return;
    }
    let e = MiniLmEmbedder::load(&dir).expect("load");
    e.embed("warm up the graph");
    let t = std::time::Instant::now();
    for i in 0..20 {
        e.embed(&format!(
            "some chunk of document text number {i} with a dozen words in it"
        ));
    }
    let per = t.elapsed() / 20;
    eprintln!("MiniLM CPU embed latency: {per:?}/chunk");
    assert!(
        per < std::time::Duration::from_millis(250),
        "{per:?} too slow"
    );
}
