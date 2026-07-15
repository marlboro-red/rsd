//! rsd-daemon: wires FSEvents → coalescer → applier over a catalog (P1.6).
//!
//! Overflow anywhere in the pipeline degrades to a scoped/root rescan and is
//! *counted* — the convergence harness asserts zero full rescans on the happy
//! path and at least one on the overflow path.

use rsd_catalog::Catalog;
use rsd_fsevents::{WatchConfig, Watcher};
use rsd_ingest::{apply_work, coalesce, CoalescerConfig, IngestEvent, ScanStats, WorkItem};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct PipelineConfig {
    pub coalescer: CoalescerConfig,
    /// Watcher→pipeline channel bound (small values force overflow handling).
    pub event_capacity: usize,
    /// Coalescer→applier channel bound.
    pub work_capacity: usize,
    pub fsevents_latency: Duration,
    /// How long a last-entry-removed object keeps its identity for rename
    /// pairing before the sweeper reclaims it.
    pub orphan_grace: Duration,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            coalescer: CoalescerConfig::default(),
            event_capacity: 8_192,
            work_capacity: 4_096,
            fsevents_latency: Duration::from_millis(100),
            orphan_grace: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Default)]
pub struct PipelineCounters {
    pub work_items: AtomicU64,
    pub full_rescans: AtomicU64,
    pub orphans_swept: AtomicU64,
}

/// A running ingest pipeline over one watched root.
pub struct Pipeline {
    watcher: Option<Watcher>,
    feeder: Option<JoinHandle<()>>,
    pump: Option<JoinHandle<()>>,
    applier: Option<JoinHandle<()>>,
    pub counters: Arc<PipelineCounters>,
    pub stats: Arc<Mutex<ScanStats>>,
}

impl Pipeline {
    /// Start watching `root` (must be canonicalized) over `catalog`. The caller
    /// is expected to have bootstrapped the catalog first.
    pub fn start(
        catalog: Arc<Catalog>,
        root: &Path,
        cfg: PipelineConfig,
    ) -> std::io::Result<Pipeline> {
        let (watcher, event_rx) = Watcher::start(
            &[root],
            WatchConfig {
                since: None,
                latency: cfg.fsevents_latency,
                capacity: cfg.event_capacity,
            },
        )?;

        let counters = Arc::new(PipelineCounters::default());
        let stats = Arc::new(Mutex::new(ScanStats::default()));

        let (ingest_tx, ingest_rx) = mpsc::channel::<IngestEvent>();
        let (work_tx, work_rx) = mpsc::sync_channel::<WorkItem>(cfg.work_capacity);

        // Feeder: FsEvent → IngestEvent. Ends when the watcher stops.
        let feeder = std::thread::Builder::new()
            .name("rsd-feeder".into())
            .spawn(move || {
                while let Ok(ev) = event_rx.recv() {
                    let recursive = ev.flags.must_scan_subdirs() || ev.flags.dropped();
                    if ingest_tx
                        .send(IngestEvent {
                            path: ev.path,
                            rescan_recursive: recursive,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            })?;

        // Coalescer pump. Ends when the feeder drops its sender.
        let pump = {
            let co_cfg = cfg.coalescer;
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            std::thread::Builder::new()
                .name("rsd-coalescer".into())
                .spawn(move || coalesce::run_pump(ingest_rx, work_tx, co_cfg, stop))?
        };

        // Applier: resolve work by lstat, sweep orphans at idle. Ends when the
        // pump drops the work sender.
        let applier = {
            let catalog = catalog.clone();
            let counters = counters.clone();
            let stats = stats.clone();
            let grace = cfg.orphan_grace;
            std::thread::Builder::new()
                .name("rsd-applier".into())
                .spawn(move || run_applier(&catalog, work_rx, &counters, &stats, grace))?
        };

        Ok(Pipeline {
            watcher: Some(watcher),
            feeder: Some(feeder),
            pump: Some(pump),
            applier: Some(applier),
            counters,
            stats,
        })
    }

    /// True if the watcher's bounded channel shed events since the last
    /// recovery (the flag is cleared by `recover_overflow_if_any`).
    pub fn overflowed(&self) -> bool {
        self.watcher.as_ref().is_some_and(|w| w.overflowed())
    }

    /// If the watcher shed events, reconcile the root recursively and count a
    /// full rescan. Call periodically from the owner's supervision loop.
    pub fn recover_overflow_if_any(&self, catalog: &Catalog, root: &Path) {
        let Some(w) = self.watcher.as_ref() else {
            return;
        };
        if !w.overflowed() {
            return;
        }
        w.clear_overflow();
        self.counters.full_rescans.fetch_add(1, Ordering::Relaxed);
        match rsd_ingest::rescan(catalog, root, true) {
            Ok(s) => self.stats.lock().unwrap().absorb(s),
            Err(e) => tracing::warn!("overflow recovery rescan failed: {e}"),
        }
    }

    /// Stop the watcher and drain the pipeline to completion: each stage's
    /// channel disconnect cascades shutdown to the next.
    pub fn stop(mut self) {
        if let Some(w) = self.watcher.take() {
            let _ = w.stop();
        }
        for t in [self.feeder.take(), self.pump.take(), self.applier.take()]
            .into_iter()
            .flatten()
        {
            let _ = t.join();
        }
    }
}

fn run_applier(
    catalog: &Catalog,
    work_rx: mpsc::Receiver<WorkItem>,
    counters: &PipelineCounters,
    stats: &Mutex<ScanStats>,
    grace: Duration,
) {
    loop {
        match work_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(item) => {
                counters.work_items.fetch_add(1, Ordering::Relaxed);
                match apply_work(catalog, &item) {
                    Ok(s) => stats.lock().unwrap().absorb(s),
                    Err(e) => tracing::warn!("apply_work({:?}) failed: {e}", item.path),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle: reclaim orphaned identities past their grace window.
                match catalog.sweep_orphans(grace) {
                    Ok(n) if n > 0 => {
                        counters
                            .orphans_swept
                            .fetch_add(n as u64, Ordering::Relaxed);
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("orphan sweep failed: {e}"),
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Bootstrap + start: the standard bring-up sequence.
pub fn bring_up(
    catalog: Arc<Catalog>,
    root: &Path,
    cfg: PipelineConfig,
) -> std::io::Result<(Pipeline, ScanStats)> {
    let stats = rsd_ingest::bootstrap(&catalog, root)
        .map_err(|e| std::io::Error::other(format!("bootstrap failed: {e}")))?;
    let p = Pipeline::start(catalog, root, cfg)?;
    Ok((p, stats))
}
