//! rsd-daemon: wires FSEvents → coalescer → committer over a journal + catalog
//! (P1.6 + P2.4).
//!
//! The applier is the system's single writer: it resolves work items against
//! the filesystem (pure reads), then commits the resulting absolute changes
//! through the journal-before-apply state machine. Overflow anywhere degrades
//! to a *counted* scoped rescan and self-heals on the applier thread.

pub mod commit;
mod connection_limit;
pub mod dispatch;
pub mod http;
pub mod ipc;
pub mod ocr;
pub mod transcribe;
pub mod wasm_source;

pub use commit::{CommitError, Committer};
pub use dispatch::{ContentCounters, ContentIndexer, ContentSource, PooledExtractor};

use rsd_catalog::Catalog;
use rsd_fsevents::{WatchConfig, Watcher};
use rsd_ingest::{
    coalesce, resolve_work, CoalescerConfig, IngestEvent, ScanStats, WorkItem, WorkKind,
};
use rsd_log::{CursorStore, Journal, JournalConfig, Source};
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
    /// Bootstrap mode: `false` = blocking full reconciliation before the
    /// pipeline starts (tests, small scopes); `true` = the budgeted trickle
    /// (DESIGN.md §7.2): a paced background walker feeds per-directory scans
    /// through the same work queue as live events — the daemon answers
    /// queries from second one, live changes preempt bulk, and the pace
    /// drops on battery power.
    pub trickle_bootstrap: bool,
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
            trickle_bootstrap: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct PipelineCounters {
    pub work_items: AtomicU64,
    pub bootstrap_dirs: AtomicU64,
    pub bootstrap_done: AtomicU64,
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
    pub applier_down: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
}

impl Pipeline {
    /// Start watching `root` (must be canonicalized) over a recovered,
    /// bootstrapped committer. Use `bring_up` for the standard sequence.
    pub fn start(
        committer: Committer,
        content: Option<ContentIndexer>,
        root: &Path,
        resume_cursor: u64,
        cursor_store: CursorStore,
        cfg: PipelineConfig,
    ) -> std::io::Result<Pipeline> {
        let (watcher, event_rx) = Watcher::start(
            &[root],
            WatchConfig {
                since: Some(resume_cursor),
                latency: cfg.fsevents_latency,
                capacity: cfg.event_capacity,
            },
        )?;
        let overflow = watcher.overflow_handle();

        let counters = Arc::new(PipelineCounters::default());
        let stats = Arc::new(Mutex::new(ScanStats::default()));
        let stopping = Arc::new(AtomicBool::new(false));
        let applier_down = Arc::new(AtomicBool::new(false));
        rsd_metrics::metrics().applier_down.set(0);

        let (ingest_tx, ingest_rx) = mpsc::channel::<IngestEvent>();
        let (work_tx, work_rx) = mpsc::sync_channel::<WorkItem>(cfg.work_capacity);

        // Feeder: FsEvent → IngestEvent. Ends when the watcher stops.
        let feeder = std::thread::Builder::new()
            .name("rsd-feeder".into())
            .spawn(move || {
                while let Ok(ev) = event_rx.recv() {
                    if rsd_ingest::excluded(&ev.path) {
                        continue;
                    }
                    // A moved/created directory must remain recursive even if
                    // an earlier root event already inserted the directory in
                    // the catalog. Event delivery order must not downgrade
                    // subtree discovery to a shallow known-dir probe.
                    let recursive = ev.flags.must_scan_subdirs()
                        || ev.flags.dropped()
                        || (ev.flags.is_dir() && (ev.flags.created() || ev.flags.renamed()));
                    if ingest_tx
                        .send(IngestEvent {
                            path: ev.path,
                            rescan_recursive: recursive,
                            source_cursor: Some(ev.event_id),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            })?;

        // Trickle bootstrap (when configured): a paced walker enqueues one
        // shallow scan per directory through the same bounded work channel as
        // live events — backpressure and interleaving come for free.
        if cfg.trickle_bootstrap {
            let tx = work_tx.clone();
            let root = root.to_path_buf();
            let counters_w = counters.clone();
            let stopping_w = stopping.clone();
            std::thread::Builder::new()
                .name("rsd-bootstrap".into())
                .spawn(move || trickle_walk(&root, tx, &counters_w, &stopping_w))?;
        }

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
            let applier_down_thread = applier_down.clone();
            std::thread::Builder::new()
                .name("rsd-applier".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_applier(
                            committer,
                            content,
                            work_rx,
                            &counters,
                            &stats,
                            grace,
                            overflow,
                            &root,
                            cursor_store,
                        )
                    }));
                    if result.is_err() {
                        applier_down_thread.store(true, Ordering::Release);
                        rsd_metrics::metrics().applier_down.set(1);
                        tracing::error!("health.applier_down=1: applier thread panicked");
                    }
                })?
        };

        Ok(Pipeline {
            watcher: Some(watcher),
            feeder: Some(feeder),
            pump: Some(pump),
            applier: Some(applier),
            counters,
            stats,
            applier_down,
            stopping,
        })
    }

    /// Stop the watcher and drain the pipeline to completion: each stage's
    /// channel disconnect cascades shutdown to the next.
    pub fn stop(mut self) {
        self.stopping.store(true, Ordering::Relaxed);
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
    cursor_store: CursorStore,
) {
    let resolve_and_commit = |committer: &mut Committer,
                              content: &mut Option<ContentIndexer>,
                              item: &WorkItem,
                              source: Source| {
        match resolve_work(committer.catalog(), item) {
            Ok((changes, s)) => {
                stats
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .absorb(s);
                match committer.commit(source, &changes) {
                    Ok(Some(_)) => {
                        counters.commits.fetch_add(1, Ordering::Relaxed);
                        rsd_metrics::metrics().commits.inc();
                        if let Some(indexer) = content.as_mut() {
                            let upserts = file_upserts(&changes);
                            if !upserts.is_empty() && !indexer.process(committer, &upserts) {
                                return false;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("commit({:?}) failed: {e}", item.path);
                        return false;
                    }
                }
                true
            }
            Err(e) => {
                tracing::warn!("resolve({:?}) failed: {e}", item.path);
                false
            }
        }
    };

    loop {
        // Overflow self-heal first: the callback shed events, so nothing under
        // the root can be trusted as observed — reconcile it (counted).
        if overflow.swap(false, Ordering::Relaxed) {
            counters.full_rescans.fetch_add(1, Ordering::Relaxed);
            rsd_metrics::metrics().full_rescans.inc();
            resolve_and_commit(
                &mut committer,
                &mut content,
                &WorkItem {
                    path: root.to_path_buf(),
                    kind: WorkKind::RescanRecursive,
                    source_cursor: None,
                },
                Source::Scan,
            );
        }
        match work_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(item) => {
                counters.work_items.fetch_add(1, Ordering::Relaxed);
                // The whole item journey is synchronous on this thread: resolve
                // (readdir/lstat) + commit(Upsert) + extract (worker round-trip)
                // + commit(SetContent). Time inline — no cross-thread keyed
                // table needed until the async embedder lands. Only PROBE items
                // are single live-edited files; RescanShallow/Recursive are
                // bulk (bootstrap/overflow) and would pollute the freshness
                // histogram with directory-sized samples (§18.6: bulk coarse,
                // interactive fine).
                let fine = item.kind == WorkKind::Probe;
                let t0 = std::time::Instant::now();
                let applied =
                    resolve_and_commit(&mut committer, &mut content, &item, Source::FsEvents);
                // Recheck overflow at the durability edge: the callback may
                // have shed an event while this item was applying. Advancing
                // past it before the next-loop root rescan would lose that
                // event on a crash.
                if applied && !overflow.load(Ordering::Acquire) {
                    if let Some(source_cursor) = item.source_cursor {
                        if let Err(error) = cursor_store.set(source_cursor) {
                            tracing::error!("source cursor advance failed: {error}");
                        }
                    }
                }
                if fine {
                    rsd_metrics::metrics()
                        .index_latency_ms
                        .record(t0.elapsed().as_secs_f64() * 1000.0);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle: reclaim orphaned identities past their grace window.
                // Unjournaled by design: orphan GC is derived state, replay
                // regenerates and re-sweeps it.
                match committer.sweep_orphans(grace) {
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
    // A torn store can make redb PANIC on open (observed on CI runners) as
    // well as error — both are the same failure-matrix event: the catalog is
    // a projection; drop it and let recovery replay it from the journal.
    let attempt = |path: &Path| -> Result<Catalog, String> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Catalog::open_with_durability(path, durability)
        }))
        .map_err(|_| "redb panicked opening the store".to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
    };
    match attempt(path) {
        Ok(c) => Ok((Arc::new(c), false)),
        Err(first_err) => {
            tracing::warn!(
                "catalog at {path:?} unopenable ({first_err}); rebuilding projection from journal"
            );
            std::fs::remove_file(path)?;
            let c = attempt(path)
                .map_err(|e| std::io::Error::other(format!("catalog recreate: {e}")))?;
            Ok((Arc::new(c), true))
        }
    }
}

/// Open the lexical projection, recreating it when Tantivy cannot recover it.
/// The caller's normal journal+CAES recovery immediately repopulates it.
pub fn open_lexical_resilient(path: &Path) -> std::io::Result<(rsd_lexical::LexicalPlane, bool)> {
    let attempt = || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rsd_lexical::LexicalPlane::open(path)
        }))
        .map_err(|_| "tantivy panicked opening the projection".to_string())
        .and_then(|result| result.map_err(|error| error.to_string()))
    };
    match attempt() {
        Ok(plane) => Ok((plane, false)),
        Err(first_error) => {
            tracing::warn!("lexical projection at {path:?} unopenable ({first_error}); rebuilding");
            if path.exists() {
                std::fs::remove_dir_all(path)?;
            }
            attempt()
                .map(|plane| (plane, true))
                .map_err(|error| std::io::Error::other(format!("lexical recreate: {error}")))
        }
    }
}

/// Open the vector projection, recreating its single-file redb store when it
/// is unopenable. Journal+CAES recovery restores it before serving queries.
pub fn open_vector_resilient(
    path: &Path,
    embedder: Arc<dyn rsd_vector::Embedder>,
) -> std::io::Result<(rsd_vector::VectorPlane, bool)> {
    let attempt = || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rsd_vector::VectorPlane::open(path, embedder.clone())
        }))
        .map_err(|_| "redb panicked opening the vector projection".to_string())
        .and_then(|result| result.map_err(|error| error.to_string()))
    };
    match attempt() {
        Ok(plane) => Ok((plane, false)),
        Err(first_error) => {
            tracing::warn!("vector projection at {path:?} unopenable ({first_error}); rebuilding");
            if path.exists() {
                std::fs::remove_file(path)?;
            }
            attempt()
                .map(|plane| (plane, true))
                .map_err(|error| std::io::Error::other(format!("vector recreate: {error}")))
        }
    }
}

/// Power-aware pacing for bulk work: the walker sleeps between directories,
/// longer on battery. Interactive work never waits — it rides the coalescer
/// path and interleaves whenever the applier is between bulk items.
fn bulk_pause_ms() -> u64 {
    // `pmset -g batt` is cheap and needs no entitlements; checked by the
    // caller only every N directories.
    let on_battery = std::process::Command::new("/usr/bin/pmset")
        .args(["-g", "batt"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Battery Power"))
        .unwrap_or(false);
    if on_battery {
        60
    } else {
        8
    }
}

fn trickle_walk(
    root: &Path,
    tx: mpsc::SyncSender<WorkItem>,
    counters: &PipelineCounters,
    stopping: &AtomicBool,
) {
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut pause = bulk_pause_ms();
    let mut since_check = 0u32;
    while let Some(dir) = queue.pop_front() {
        if stopping.load(Ordering::Relaxed) {
            return;
        }
        if rsd_ingest::excluded(&dir) {
            continue;
        }
        // Discover children first (cheap readdir, no stats), then hand the
        // heavy per-file work to the applier as one shallow scan.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    queue.push_back(e.path());
                }
            }
        }
        counters.bootstrap_dirs.fetch_add(1, Ordering::Relaxed);
        rsd_metrics::metrics()
            .bootstrap_dirs
            .set(counters.bootstrap_dirs.load(Ordering::Relaxed) as i64);
        if tx
            .send(WorkItem {
                path: dir,
                kind: WorkKind::RescanShallow,
                source_cursor: None,
            })
            .is_err()
        {
            return; // pipeline shut down
        }
        since_check += 1;
        if since_check >= 64 {
            since_check = 0;
            pause = bulk_pause_ms();
        }
        std::thread::sleep(Duration::from_millis(pause));
    }
    counters.bootstrap_done.store(1, Ordering::Relaxed);
    rsd_metrics::metrics().bootstrap_done.set(1);
    tracing::info!(
        "trickle bootstrap complete: {} directories",
        counters.bootstrap_dirs.load(Ordering::Relaxed)
    );
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
#[allow(clippy::too_many_arguments)] // bring-up wires every plane once; a builder adds ceremony without safety
pub fn bring_up(
    catalog: Arc<Catalog>,
    journal_dir: &Path,
    root: &Path,
    mut content: Option<ContentIndexer>,
    lexical: Option<(rsd_lexical::LexicalPlane, Arc<rsd_caes::Store>)>,
    vector: Option<(
        Arc<std::sync::Mutex<rsd_vector::VectorPlane>>,
        Arc<rsd_caes::Store>,
    )>,
    live: Option<Arc<std::sync::Mutex<rsd_live::LiveEngine>>>,
    cfg: PipelineConfig,
) -> std::io::Result<(Pipeline, ScanStats)> {
    let cursor_store = CursorStore::new(&journal_dir.join("fsevents.cursor"));
    let resume_cursor = cursor_store
        .get()
        .map_err(|error| std::io::Error::other(format!("cursor read: {error}")))?
        .unwrap_or_else(rsd_fsevents::current_event_id);
    let journal_cfg = JournalConfig {
        sync_on_append: cfg.journal_sync,
        ..Default::default()
    };
    let (mut journal, repair_scope) = Journal::open_with_scoped_repair(journal_dir, journal_cfg)
        .map_err(|e| std::io::Error::other(format!("journal open: {e}")))?;
    let mut repair_upserts = Vec::new();
    if let Some(repair) = repair_scope {
        tracing::error!(
            "journal segment quarantined at {:?}; repairing {} affected paths",
            repair.quarantined_segment,
            repair.paths.len()
        );
        let mut changes = Vec::with_capacity(repair.paths.len());
        for path in repair.paths {
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    let stat = rsd_catalog::StatInfo::from_metadata(&metadata);
                    if stat.kind == rsd_catalog::ObjectKind::File {
                        repair_upserts.push((path.clone(), stat));
                    }
                    changes.push(rsd_catalog::Change::Upsert { path, stat });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    changes.push(rsd_catalog::Change::RemovePath { path });
                }
                Err(error) => return Err(error),
            }
        }
        if !changes.is_empty() {
            journal.append(Source::Repair, &changes).map_err(|error| {
                std::io::Error::other(format!("journal repair append: {error}"))
            })?;
        }
    }
    let mut committer = Committer::new(catalog.clone(), journal);
    if let Some((plane, caes)) = lexical {
        committer = committer.with_lexical(plane, caes);
    }
    if let Some((plane, caes)) = vector {
        committer = committer.with_vector(plane, caes);
    }
    if let Some(live) = live {
        committer.set_on_commit(Box::new(move |deltas| {
            live.lock()
                .unwrap_or_else(|error| error.into_inner())
                .on_commit(deltas);
        }));
    }
    let replayed = committer
        .recover()
        .map_err(|e| std::io::Error::other(format!("recovery: {e}")))?;
    if replayed > 0 {
        tracing::info!("recovered {replayed} journal records into the catalog");
    }
    if !repair_upserts.is_empty() {
        if let Some(indexer) = content.as_mut() {
            if !indexer.process(&mut committer, &repair_upserts) {
                return Err(std::io::Error::other(
                    "content repair failed for a quarantined journal segment",
                ));
            }
        }
    }

    if cfg.trickle_bootstrap {
        // The walker inside Pipeline::start owns bootstrap; queries answer
        // immediately against whatever is already indexed.
        let p = Pipeline::start(committer, content, root, resume_cursor, cursor_store, cfg)?;
        return Ok((p, ScanStats::default()));
    }

    // Journaled bootstrap: resolve the whole root, commit in group batches.
    let (changes, stats) = resolve_work(
        &catalog,
        &WorkItem {
            path: root.to_path_buf(),
            kind: WorkKind::RescanRecursive,
            source_cursor: None,
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
                let _ = indexer.process(&mut committer, &upserts);
            }
        }
    }

    let p = Pipeline::start(committer, content, root, resume_cursor, cursor_store, cfg)?;
    Ok((p, stats))
}
