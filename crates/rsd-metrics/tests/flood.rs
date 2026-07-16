//! §18.7 flood test: the metric plane's memory stays constant under an event
//! storm. Cardinality is bounded by construction (fixed fields, no dynamic
//! label map) — this asserts the snapshot's shape never grows with load, and
//! that recording a million samples doesn't blow up.

use rsd_metrics::{metrics, snapshot_json};

fn label_count(v: &serde_json::Value) -> usize {
    fn walk(v: &serde_json::Value) -> usize {
        match v {
            serde_json::Value::Object(m) => m.values().map(walk).sum::<usize>() + m.len(),
            _ => 0,
        }
    }
    walk(v)
}

#[test]
fn cardinality_is_flat_under_a_million_samples() {
    let before = label_count(&snapshot_json());
    let m = metrics();
    for i in 0..1_000_000u64 {
        m.files_indexed.inc();
        m.index_latency_ms.record((i % 5000) as f64 * 0.1);
        m.record_extraction_failure(if i % 7 == 0 { "corrupt" } else { "unsupported" });
        m.coalescer_depth.set((i % 100) as i64);
    }
    let after = label_count(&snapshot_json());
    assert!(
        after <= before + 6,
        "cardinality grew with load: {before} -> {after}"
    );
    assert!(m.files_indexed.get() >= 1_000_000);
    let p99 = m.index_latency_ms.percentile(0.99);
    assert!(p99.is_finite() && p99 > 0.0);
}
