//! rsd-metrics: the metric plane (DESIGN.md §18.3) — the daemon's account of
//! its own quantities over time.
//!
//! Cardinality-safe *by construction*: every metric is a named struct field or
//! a small enum-indexed array. There is no dynamic `String -> metric` map, so
//! it is structurally impossible to add a per-path or per-query label — the
//! O(files) registry growth that would reintroduce the exact unbounded-memory
//! failure P4 exists to prevent simply cannot be expressed here. Bounded
//! domains (status, plane) are fixed arrays; paths and queries live in the
//! event plane, never here.
//!
//! Hot path is a few relaxed atomic adds — histograms are fixed-bucket
//! (constant memory, branchless-ish bucket increment), percentiles interpolate
//! on read. Everything is a global read from `metrics()`.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;

pub struct Counter(AtomicU64);
impl Counter {
    const fn new() -> Self {
        Counter(AtomicU64::new(0))
    }
    #[inline]
    pub fn inc(&self) {
        self.0.fetch_add(1, Relaxed);
    }
    #[inline]
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Relaxed)
    }
}

/// Instantaneous level; can rise and fall.
pub struct Gauge(AtomicI64);
impl Gauge {
    const fn new() -> Self {
        Gauge(AtomicI64::new(0))
    }
    #[inline]
    pub fn set(&self, v: i64) {
        self.0.store(v, Relaxed);
    }
    pub fn get(&self) -> i64 {
        self.0.load(Relaxed)
    }
}

/// Log-spaced millisecond boundaries: 0.1ms → 60s. A recording is one linear
/// scan (18 comparisons) + two atomic adds; memory is constant.
const BOUNDS_MS: &[f64] = &[
    0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0,
    10000.0, 30000.0, 60000.0,
];

pub struct Histogram {
    // One extra bucket for the +inf overflow.
    buckets: [AtomicU64; 19],
    /// Sum in microseconds (integer, so no float atomics).
    sum_us: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    const fn new() -> Self {
        // Cannot array-init AtomicU64 from a const fn generically; spell it out.
        Histogram {
            buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            sum_us: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn record(&self, ms: f64) {
        let idx = BOUNDS_MS
            .iter()
            .position(|&b| ms <= b)
            .unwrap_or(BOUNDS_MS.len());
        self.buckets[idx].fetch_add(1, Relaxed);
        self.sum_us.fetch_add((ms * 1000.0) as u64, Relaxed);
        self.count.fetch_add(1, Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Relaxed)
    }

    pub fn mean_ms(&self) -> f64 {
        let c = self.count.load(Relaxed);
        if c == 0 {
            0.0
        } else {
            (self.sum_us.load(Relaxed) as f64 / 1000.0) / c as f64
        }
    }

    /// Percentile in ms via linear interpolation across the crossing bucket.
    pub fn percentile(&self, p: f64) -> f64 {
        let total = self.count.load(Relaxed);
        if total == 0 {
            return 0.0;
        }
        let target = (p * total as f64).ceil() as u64;
        let mut cum = 0u64;
        for (i, b) in self.buckets.iter().enumerate() {
            cum += b.load(Relaxed);
            if cum >= target {
                let lo = if i == 0 { 0.0 } else { BOUNDS_MS[i - 1] };
                let hi = *BOUNDS_MS.get(i).unwrap_or(BOUNDS_MS.last().unwrap());
                return (lo + hi) / 2.0;
            }
        }
        *BOUNDS_MS.last().unwrap()
    }
}

/// Typed extraction-failure buckets (§10.1 statuses). Fixed domain → fixed
/// array → cardinality can never grow.
#[derive(Clone, Copy)]
pub enum FailKind {
    Encrypted = 0,
    Corrupt = 1,
    Unsupported = 2,
    BudgetExceeded = 3,
    Quarantined = 4,
    Other = 5,
}
const N_FAIL: usize = 6;

impl FailKind {
    pub fn from_status(s: &str) -> FailKind {
        match s {
            "encrypted" | "password-required" => FailKind::Encrypted,
            "corrupt" => FailKind::Corrupt,
            "unsupported" => FailKind::Unsupported,
            "budget-exceeded" => FailKind::BudgetExceeded,
            "quarantined" => FailKind::Quarantined,
            _ => FailKind::Other,
        }
    }
    fn label(i: usize) -> &'static str {
        [
            "encrypted",
            "corrupt",
            "unsupported",
            "budget-exceeded",
            "quarantined",
            "other",
        ][i]
    }
}

pub struct Metrics {
    // Throughput (counters)
    pub files_indexed: Counter,
    pub caes_hits: Counter,
    pub caes_misses: Counter,
    pub commits: Counter,
    // Health (counters)
    pub full_rescans: Counter,
    pub worker_crashes: Counter,
    pub quarantines: Counter,
    pub journal_replays: Counter,
    extraction_failures: [Counter; N_FAIL],
    // Freshness / stage timing (histograms, ms)
    pub index_latency_ms: Histogram,
    pub extract_ms: Histogram,
    pub commit_ms: Histogram,
    // Backlog / resource (gauges)
    pub coalescer_depth: Gauge,
    pub catalog_entries: Gauge,
    pub bootstrap_dirs: Gauge,
    /// 0 = bootstrapping, 1 = complete.
    pub bootstrap_done: Gauge,
    /// 1 once the single-writer applier has panicked.
    pub applier_down: Gauge,
}

impl Metrics {
    const fn new() -> Self {
        Metrics {
            files_indexed: Counter::new(),
            caes_hits: Counter::new(),
            caes_misses: Counter::new(),
            commits: Counter::new(),
            full_rescans: Counter::new(),
            worker_crashes: Counter::new(),
            quarantines: Counter::new(),
            journal_replays: Counter::new(),
            extraction_failures: [
                Counter::new(),
                Counter::new(),
                Counter::new(),
                Counter::new(),
                Counter::new(),
                Counter::new(),
            ],
            index_latency_ms: Histogram::new(),
            extract_ms: Histogram::new(),
            commit_ms: Histogram::new(),
            coalescer_depth: Gauge::new(),
            catalog_entries: Gauge::new(),
            bootstrap_dirs: Gauge::new(),
            bootstrap_done: Gauge::new(),
            applier_down: Gauge::new(),
        }
    }

    pub fn record_extraction_failure(&self, status: &str) {
        self.extraction_failures[FailKind::from_status(status) as usize].inc();
    }
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

pub fn metrics() -> &'static Metrics {
    METRICS.get_or_init(Metrics::new)
}

/// Full registry snapshot as JSON — the /api/metrics body and the CLI source.
pub fn snapshot_json() -> serde_json::Value {
    let m = metrics();
    let hist = |h: &Histogram| {
        serde_json::json!({
            "count": h.count(),
            "mean_ms": (h.mean_ms() * 100.0).round() / 100.0,
            "p50_ms": h.percentile(0.50),
            "p90_ms": h.percentile(0.90),
            "p99_ms": h.percentile(0.99),
        })
    };
    let mut fails = serde_json::Map::new();
    for i in 0..N_FAIL {
        let v = m.extraction_failures[i].get();
        if v > 0 {
            fails.insert(FailKind::label(i).to_string(), v.into());
        }
    }
    serde_json::json!({
        "throughput": {
            "files_indexed": m.files_indexed.get(),
            "caes_hits": m.caes_hits.get(),
            "caes_misses": m.caes_misses.get(),
            "commits": m.commits.get(),
        },
        "health": {
            "full_rescans": m.full_rescans.get(),
            "worker_crashes": m.worker_crashes.get(),
            "quarantines": m.quarantines.get(),
            "journal_replays": m.journal_replays.get(),
            "extraction_failures": fails,
        },
        "freshness": {
            "index_latency_ms": hist(&m.index_latency_ms),
            "extract_ms": hist(&m.extract_ms),
            "commit_ms": hist(&m.commit_ms),
        },
        "backlog": {
            "coalescer_depth": m.coalescer_depth.get(),
            "catalog_entries": m.catalog_entries.get(),
            "bootstrap_dirs": m.bootstrap_dirs.get(),
            "bootstrap_done": m.bootstrap_done.get() == 1,
            "applier_down": m.applier_down.get() == 1,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_percentiles_are_monotonic_and_bounded() {
        let h = Histogram::new();
        for i in 1..=1000 {
            h.record(i as f64); // 1..1000 ms uniform
        }
        assert_eq!(h.count(), 1000);
        let p50 = h.percentile(0.50);
        let p99 = h.percentile(0.99);
        assert!(p50 < p99, "p50 {p50} !< p99 {p99}");
        assert!(p99 <= 1000.0);
        // Mean of 1..1000 is ~500.
        assert!((h.mean_ms() - 500.0).abs() < 50.0, "mean {}", h.mean_ms());
    }

    #[test]
    fn empty_histogram_is_zero_not_panic() {
        let h = Histogram::new();
        assert_eq!(h.percentile(0.99), 0.0);
        assert_eq!(h.mean_ms(), 0.0);
    }

    #[test]
    fn extraction_failures_bucket_by_status_bounded() {
        let m = Metrics::new();
        for _ in 0..5 {
            m.record_extraction_failure("corrupt");
        }
        m.record_extraction_failure("encrypted");
        m.record_extraction_failure("totally-unknown-status"); // -> other
        assert_eq!(m.extraction_failures[FailKind::Corrupt as usize].get(), 5);
        assert_eq!(m.extraction_failures[FailKind::Encrypted as usize].get(), 1);
        assert_eq!(m.extraction_failures[FailKind::Other as usize].get(), 1);
    }

    #[test]
    fn snapshot_is_valid_json_with_expected_shape() {
        metrics().files_indexed.add(42);
        metrics().index_latency_ms.record(7.5);
        let s = snapshot_json();
        assert!(s["throughput"]["files_indexed"].as_u64().unwrap() >= 42);
        assert!(
            s["freshness"]["index_latency_ms"]["count"]
                .as_u64()
                .unwrap()
                >= 1
        );
        assert_eq!(s["backlog"]["applier_down"], false);
    }
}
