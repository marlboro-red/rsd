//! rsd-daemon: wires FSEvents → coalescer → committer over a journal + catalog
//! (P1.6 + P2.4).
//!
//! The applier is the system's single writer: it resolves work items against
//! the filesystem (pure reads), then commits the resulting absolute changes
//! through the journal-before-apply state machine. Overflow anywhere degrades
//! to a *counted* scoped rescan and self-heals on the applier thread.

pub mod commit;
pub mod dispatch;

pub use commit::{CommitError, Committer};
pub use dispatch::{ContentCounters, ContentIndexer, ContentSource, PooledExtractor};

use rsd_catalog::Catalog;
use rsd_fsevents::{WatchConfig, Watcher};
use rsd_ingest::{
    coalesce, resolve_work, CoalescerConfig, IngestEvent, ScanStats, WorkItem, WorkKind,
};
use rsd_log::{Journal, JournalConfig, Source};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// fsync journal appends (the durability point). Tests disable for speed;
    /// kill-9 safety does not depend on it (page cache survives SIGKILL).
    pub journal_sync: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            coalescer: CoalescerConfig::default(),
            event_capacity: 8_192,
            work_capacity: 4_096,
            fsevents_latency: Duration::from_millis(100),
            orphan_grace: Duration::from_secs(10),
            journal_sync: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct PipelineCounters {
    pub work_items: AtomicU64,
    pub full_rescans: AtomicU64,
    pub orphans_swept: AtomicU64,
    pub commits: AtomicU64,
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
    /// Start watching `root` (must be canonicalized) over a recovered,
    /// bootstrapped committer. Use `bring_up` for the standard sequence.
    pub fn start(
        committer: Committer,
        content: Option<ContentIndexer>,
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
        let overflow = watcher.overflow_handle();

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
            let stop = Arc::new(AtomicBool::new(false));
            std::thread::Builder::new()
                .name("rsd-coalescer".into())
                .spawn(move || coalesce::run_pump(ingest_rx, work_tx, co_cfg, stop))?
        };

        // Applier: the single committing writer. Ends when the pump drops the
        // work sender.
        let applier = {
            let counters = counters.clone();
            let stats = stats.clone();
            let grace = cfg.orphan_grace;
            let root = root.to_path_buf();
            std::thread::Builder::new()
                .name("rsd-applier".into())
                .spawn(move || {
                    run_applier(
                        committer, content, work_rx, &counters, &stats, grace, overflow, &root,
                    )
                })?
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

#[allow(clippy::too_many_arguments)]
fn run_applier(
    mut committer: Committer,
    mut content: Option<ContentIndexer>,
    work_rx: mpsc::Receiver<WorkItem>,
    counters: &PipelineCounters,
    stats: &Mutex<ScanStats>,
    grace: Duration,
    overflow: Arc<AtomicBool>,
    root: &Path,
) {
    let resolve_and_commit = |committer: &mut Committer,
                              content: &mut Option<ContentIndexer>,
                              item: &WorkItem,
                              source: Source| {
        match resolve_work(committer.catalog(), item) {
            Ok((changes, s)) => {
                stats.lock().unwrap().absorb(s);
                match committer.commit(source, &changes) {
                    Ok(Some(_)) => {
                        counters.commits.fetch_add(1, Ordering::Relaxed);
                        if let Some(indexer) = content.as_mut() {
                            let upserts = file_upserts(&changes);
                            if !upserts.is_empty() {
                                indexer.process(committer, &upserts);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::error!("commit({:?}) failed: {e}", item.path),
                }
            }
            Err(e) => tracing::warn!("resolve({:?}) failed: {e}", item.path),
        }
    };

    loop {
        // Overflow self-heal first: the callback shed events, so nothing under
        // the root can be trusted as observed — reconcile it (counted).
        if overflow.swap(false, Ordering::Relaxed) {
            counters.full_rescans.fetch_add(1, Ordering::Relaxed);
            resolve_and_commit(
                &mut committer,
                &mut content,
                &WorkItem {
                    path: root.to_path_buf(),
                    kind: WorkKind::RescanRecursive,
                },
                Source::Scan,
            );
        }
        match work_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(item) => {
                counters.work_items.fetch_add(1, Ordering::Relaxed);
                resolve_and_commit(&mut committer, &mut content, &item, Source::FsEvents);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle: reclaim orphaned identities past their grace window.
                // Unjournaled by design: orphan GC is derived state, replay
                // regenerates and re-sweeps it.
                match committer.catalog().sweep_orphans(grace) {
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

/// Open the catalog projection, treating an unopenable store as a
/// failure-matrix event, not an error: the catalog is a CACHE of the journal
/// (DESIGN.md §1, §6.8) — delete it and let recovery replay it back into
/// existence. Returns `(catalog, rebuilt)`.
pub fn open_catalog_resilient(
    path: &Path,
    durability: rsd_catalog::Durability,
) -> std::io::Result<(Arc<Catalog>, bool)> {
    match Catalog::open_with_durability(path, durability) {
        Ok(c) => Ok((Arc::new(c), false)),
        Err(first_err) => {
            tracing::warn!(
                "catalog at {path:?} unopenable ({first_err}); rebuilding projection from journal"
            );
            std::fs::remove_file(path)?;
            let c = Catalog::open_with_durability(path, durability)
                .map_err(|e| std::io::Error::other(format!("catalog recreate: {e}")))?;
            Ok((Arc::new(c), true))
        }
    }
}

/// File upserts from a committed batch — the content indexer's input.
fn file_upserts(changes: &[rsd_catalog::Change]) -> Vec<(String, rsd_catalog::StatInfo)> {
    changes
        .iter()
        .filter_map(|c| match c {
            rsd_catalog::Change::Upsert { path, stat }
                if stat.kind == rsd_catalog::ObjectKind::File =>
            {
                Some((path.clone(), *stat))
            }
            _ => None,
        })
        .collect()
}

/// The standard bring-up sequence: open journal → recover projection →
/// journaled bootstrap reconciliation (content-indexed) → live pipeline.
pub fn bring_up(
    catalog: Arc<Catalog>,
    journal_dir: &Path,
    root: &Path,
    mut content: Option<ContentIndexer>,
    cfg: PipelineConfig,
) -> std::io::Result<(Pipeline, ScanStats)> {
    let journal = Journal::open(
        journal_dir,
        JournalConfig {
            sync_on_append: cfg.journal_sync,
            ..Default::default()
        },
    )
    .map_err(|e| std::io::Error::other(format!("journal open: {e}")))?;
    let mut committer = Committer::new(catalog.clone(), journal);
    let replayed = committer
        .recover()
        .map_err(|e| std::io::Error::other(format!("recovery: {e}")))?;
    if replayed > 0 {
        tracing::info!("recovered {replayed} journal records into the catalog");
    }

    // Journaled bootstrap: resolve the whole root, commit in group batches.
    let (changes, stats) = resolve_work(
        &catalog,
        &WorkItem {
            path: root.to_path_buf(),
            kind: WorkKind::RescanRecursive,
        },
    )
    .map_err(|e| std::io::Error::other(format!("bootstrap resolve: {e}")))?;
    for chunk in changes.chunks(1024) {
        committer
            .commit(Source::Scan, chunk)
            .map_err(|e| std::io::Error::other(format!("bootstrap commit: {e}")))?;
        if let Some(indexer) = content.as_mut() {
            let upserts = file_upserts(chunk);
            if !upserts.is_empty() {
                indexer.process(&mut committer, &upserts);
            }
        }
    }

    let p = Pipeline::start(committer, content, root, cfg)?;
    Ok((p, stats))
}
