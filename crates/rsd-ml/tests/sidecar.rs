//! P6.1 ANE sidecar: the rsd-embed helper produces usable sentence embeddings
//! over the pipe. Runs when RSD_EMBED_BIN points at the built helper (ci.sh
//! sets it); skips cleanly otherwise.

use rsd_ml::SidecarEmbedder;
use rsd_vector::Embedder;

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
fn sidecar_embeds_and_paraphrases_beat_unrelated() {
    if std::env::var_os("RSD_EMBED_BIN").is_none() {
        assert!(
            std::env::var_os("RSD_CI_HELPERS_REQUIRED").is_none(),
            "CI requires RSD_EMBED_BIN; helper build/export was skipped"
        );
        eprintln!("RSD_EMBED_BIN unset — skipping sidecar test outside helper CI");
        return;
    }
    let emb = SidecarEmbedder::discover().expect("configured sidecar failed to start");
    assert!(emb.dim() > 0);
    let q = emb.embed("automobile repair costs");
    let hit = emb.embed("the mechanic charged a fortune to fix my car");
    let miss = emb.embed("sourdough bread needs flour and water");
    assert_eq!(q.len(), emb.dim());
    assert!(q.iter().all(|x| x.is_finite()));
    assert!(
        dot(&q, &hit) > dot(&q, &miss),
        "paraphrase {:.3} not > unrelated {:.3}",
        dot(&q, &hit),
        dot(&q, &miss)
    );
}
