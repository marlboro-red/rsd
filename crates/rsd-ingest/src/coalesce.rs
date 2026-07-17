//! The coalescer (P1.4): a pure state machine (injectable clock via `Instant`
//! parameters — tests drive it synthetically) plus a thread pump.
//!
//! Structural backpressure per DESIGN.md P4: the pending map is bounded; when
//! full, per-path work collapses into the parent directory's rescan marker, so
//! memory is O(directories) under any event storm.

use crate::scan::{WorkItem, WorkKind};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Source-agnostic ingest event (FSEvents, sentinel, overflow markers all map
/// into this).
#[derive(Debug, Clone)]
pub struct IngestEvent {
    pub path: PathBuf,
    /// Source demanded a recursive rescan (e.g. kFSEventStreamEventFlagMustScanSubDirs).
    pub rescan_recursive: bool,
    /// Durable-source position represented by this event, when applicable.
    pub source_cursor: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct CoalescerConfig {
    /// Emit after this long with no further events on the path.
    pub quiet: Duration,
    /// Emit no later than this after the first event on the path.
    pub max_delay: Duration,
    /// Pending-map bound; beyond it, work collapses to parent-dir rescans.
    pub max_pending: usize,
}

impl Default for CoalescerConfig {
    fn default() -> Self {
        CoalescerConfig {
            quiet: Duration::from_millis(500),
            max_delay: Duration::from_secs(5),
            max_pending: 65_536,
        }
    }
}

#[derive(Debug)]
struct Pending {
    first: Instant,
    last: Instant,
    kind: WorkKind,
}

pub struct Coalescer {
    cfg: CoalescerConfig,
    pending: HashMap<PathBuf, Pending>,
    /// Collapses performed (observability + tests).
    pub collapses: u64,
    /// Highest cursor observed since the last emitted fence. It is attached
    /// only when the pending set empties, proving every earlier event is
    /// represented by a FIFO work item before that fence.
    fence_cursor: Option<u64>,
}

impl Coalescer {
    pub fn new(cfg: CoalescerConfig) -> Self {
        Coalescer {
            cfg,
            pending: HashMap::new(),
            collapses: 0,
            fence_cursor: None,
        }
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    fn merge(&mut self, path: PathBuf, kind: WorkKind, now: Instant) {
        match self.pending.get_mut(&path) {
            Some(p) => {
                p.last = now;
                p.kind = p.kind.max(kind);
            }
            None => {
                self.pending.insert(
                    path,
                    Pending {
                        first: now,
                        last: now,
                        kind,
                    },
                );
            }
        }
    }

    pub fn observe(&mut self, ev: IngestEvent, now: Instant) {
        self.fence_cursor = self.fence_cursor.max(ev.source_cursor);
        let kind = if ev.rescan_recursive {
            WorkKind::RescanRecursive
        } else {
            WorkKind::Probe
        };
        let full = self.pending.len() >= self.cfg.max_pending;
        if full && !self.pending.contains_key(&ev.path) {
            // Structural collapse: fold into the parent directory's rescan.
            let parent = ev
                .path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| ev.path.clone());
            let collapsed_kind = kind.max(WorkKind::RescanShallow);
            self.collapses += 1;
            self.merge(parent, collapsed_kind, now);
        } else {
            self.merge(ev.path, kind, now);
        }
    }

    /// Drain every entry whose quiet window elapsed or whose max delay hit.
    pub fn due(&mut self, now: Instant) -> Vec<WorkItem> {
        let cfg = self.cfg;
        let ready: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(_, p)| {
                now.duration_since(p.last) >= cfg.quiet
                    || now.duration_since(p.first) >= cfg.max_delay
            })
            .map(|(k, _)| k.clone())
            .collect();
        let mut items: Vec<WorkItem> = ready
            .into_iter()
            .map(|path| {
                let p = self.pending.remove(&path).expect("selected above");
                WorkItem {
                    path,
                    kind: p.kind,
                    source_cursor: None,
                }
            })
            .collect();
        self.attach_fence_if_quiescent(&mut items);
        items
    }

    /// Drain everything regardless of timers (shutdown path).
    pub fn drain_all(&mut self) -> Vec<WorkItem> {
        let mut items: Vec<WorkItem> = self
            .pending
            .drain()
            .map(|(path, p)| WorkItem {
                path,
                kind: p.kind,
                source_cursor: None,
            })
            .collect();
        self.attach_fence_if_quiescent(&mut items);
        items
    }

    fn attach_fence_if_quiescent(&mut self, items: &mut [WorkItem]) {
        if self.pending.is_empty() {
            if let Some(last) = items.last_mut() {
                last.source_cursor = self.fence_cursor.take();
            }
        }
    }
}

/// Thread pump: consumes `IngestEvent`s, emits `WorkItem`s with the coalescer's
/// timing rules. Blocking send on the work channel propagates backpressure into
/// the pending map, which collapses instead of growing (P4).
///
/// Terminates when the event sender disconnects (drains pending first) or when
/// `stop` is set.
pub fn run_pump(
    rx: mpsc::Receiver<IngestEvent>,
    tx: mpsc::SyncSender<WorkItem>,
    cfg: CoalescerConfig,
    stop: Arc<AtomicBool>,
) {
    let mut co = Coalescer::new(cfg);
    let tick = Duration::from_millis(20);
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(tick) {
            Ok(ev) => {
                let now = Instant::now();
                co.observe(ev, now);
                // Absorb any burst that's already queued before checking timers.
                while let Ok(ev) = rx.try_recv() {
                    co.observe(ev, Instant::now());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                for item in co.drain_all() {
                    if tx.send(item).is_err() {
                        return;
                    }
                }
                return;
            }
        }
        for item in co.due(Instant::now()) {
            if tx.send(item).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn cfg() -> CoalescerConfig {
        CoalescerConfig {
            quiet: Duration::from_millis(500),
            max_delay: Duration::from_secs(5),
            max_pending: 4,
        }
    }

    fn ev(p: &str) -> IngestEvent {
        IngestEvent {
            path: PathBuf::from(p),
            rescan_recursive: false,
            source_cursor: None,
        }
    }

    #[test]
    fn burst_on_one_path_emits_exactly_one_item() {
        let mut co = Coalescer::new(cfg());
        let t0 = Instant::now();
        for i in 0..50 {
            co.observe(ev("/r/a"), t0 + Duration::from_millis(i));
        }
        assert!(co.due(t0 + Duration::from_millis(400)).is_empty());
        let items = co.due(t0 + Duration::from_millis(551));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, Path::new("/r/a"));
        assert_eq!(items[0].kind, WorkKind::Probe);
        assert!(co.is_empty());
    }

    #[test]
    fn continuous_events_emit_at_max_delay_cap() {
        let mut co = Coalescer::new(cfg());
        let t0 = Instant::now();
        // An event every 300ms forever: quiet window never elapses.
        let mut emitted = None;
        for i in 0..30 {
            let now = t0 + Duration::from_millis(300 * i);
            co.observe(ev("/r/hot"), now);
            let items = co.due(now);
            if !items.is_empty() {
                emitted = Some((i, items));
                break;
            }
        }
        let (i, items) = emitted.expect("max_delay cap must fire");
        assert_eq!(items.len(), 1);
        // Cap is 5s; events at 300ms intervals => fires on the tick at >= 5s.
        assert!(300 * i >= 5_000, "fired too early: {}ms", 300 * i);
        assert!(300 * i <= 5_400, "fired too late: {}ms", 300 * i);
    }

    #[test]
    fn source_cursor_fences_only_after_all_older_pending_work() {
        let mut co = Coalescer::new(cfg());
        let t0 = Instant::now();
        co.observe(
            IngestEvent {
                path: PathBuf::from("/r/older"),
                rescan_recursive: false,
                source_cursor: Some(10),
            },
            t0,
        );
        co.observe(
            IngestEvent {
                path: PathBuf::from("/r/newer"),
                rescan_recursive: false,
                source_cursor: Some(11),
            },
            t0 + Duration::from_millis(400),
        );

        let older = co.due(t0 + Duration::from_millis(550));
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].path, Path::new("/r/older"));
        assert_eq!(older[0].source_cursor, None);

        let newer = co.due(t0 + Duration::from_millis(951));
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].path, Path::new("/r/newer"));
        assert_eq!(newer[0].source_cursor, Some(11));
    }

    #[test]
    fn recursive_flag_escalates_and_merge_takes_max() {
        let mut co = Coalescer::new(cfg());
        let t0 = Instant::now();
        co.observe(ev("/r/a"), t0);
        co.observe(
            IngestEvent {
                path: PathBuf::from("/r/a"),
                rescan_recursive: true,
                source_cursor: None,
            },
            t0,
        );
        co.observe(ev("/r/a"), t0); // must not downgrade
        let items = co.due(t0 + Duration::from_secs(1));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, WorkKind::RescanRecursive);
    }

    #[test]
    fn overflow_collapses_to_parent_and_bounds_memory() {
        let mut co = Coalescer::new(cfg()); // max_pending = 4
        let t0 = Instant::now();
        for i in 0..100 {
            co.observe(ev(&format!("/r/dir/f{i}")), t0);
        }
        // 4 distinct entries + at most 1 collapsed parent marker.
        assert!(co.len() <= 5, "pending map grew to {}", co.len());
        assert!(co.collapses >= 95);
        let items = co.due(t0 + Duration::from_secs(1));
        assert!(
            items
                .iter()
                .any(|w| w.path == Path::new("/r/dir") && w.kind >= WorkKind::RescanShallow),
            "expected a collapsed parent rescan, got {items:?}"
        );
        assert!(co.is_empty());
    }
}
