//! P1.5 live success criteria: real FSEvents deliveries for create / modify /
//! rename / delete under a watched tempdir, and clean stream shutdown.

use rsd_fsevents::{WatchConfig, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Collect events until every `wanted` path has been seen or the deadline hits.
fn collect_until(
    rx: &mpsc::Receiver<rsd_fsevents::FsEvent>,
    wanted: &[&Path],
    deadline: Duration,
) -> HashSet<PathBuf> {
    let start = Instant::now();
    let want: HashSet<PathBuf> = wanted.iter().map(|p| p.to_path_buf()).collect();
    let mut seen = HashSet::new();
    while start.elapsed() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ev) => {
                if want.contains(&ev.path) {
                    seen.insert(ev.path.clone());
                }
                if seen.len() == want.len() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    seen
}

#[test]
fn create_modify_rename_delete_produce_events_and_stream_stops_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    let (watcher, rx) = Watcher::start(
        &[&root],
        WatchConfig {
            latency: Duration::from_millis(50),
            ..Default::default()
        },
    )
    .unwrap();

    // Create.
    let f = root.join("a.txt");
    std::fs::write(&f, "hello").unwrap();
    let seen = collect_until(&rx, &[&f], Duration::from_secs(5));
    assert!(seen.contains(&f), "no create event for {f:?}");

    // Modify.
    std::fs::write(&f, "hello world").unwrap();
    let seen = collect_until(&rx, &[&f], Duration::from_secs(5));
    assert!(seen.contains(&f), "no modify event for {f:?}");

    // Rename: expect events on both old and new paths.
    let g = root.join("b.txt");
    std::fs::rename(&f, &g).unwrap();
    let seen = collect_until(&rx, &[&f, &g], Duration::from_secs(5));
    assert!(
        seen.contains(&f) && seen.contains(&g),
        "rename events incomplete: {seen:?}"
    );

    // Delete.
    std::fs::remove_file(&g).unwrap();
    let seen = collect_until(&rx, &[&g], Duration::from_secs(5));
    assert!(seen.contains(&g), "no delete event for {g:?}");

    assert!(watcher.delivered() > 0);
    assert!(!watcher.overflowed());

    // Clean shutdown: join must complete promptly (no leaked runloop thread).
    let start = Instant::now();
    watcher.stop().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "watcher thread did not stop promptly"
    );
}

#[test]
fn overflow_sets_flag_instead_of_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    // Tiny capacity, and we never drain the receiver until the flood is done.
    let (watcher, rx) = Watcher::start(
        &[&root],
        WatchConfig {
            latency: Duration::from_millis(10),
            capacity: 4,
            ..Default::default()
        },
    )
    .unwrap();

    for i in 0..300 {
        std::fs::write(root.join(format!("f{i}.txt")), "x").unwrap();
    }

    // Wait for deliveries to accumulate past capacity.
    let start = Instant::now();
    while !watcher.overflowed() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        watcher.overflowed(),
        "expected overflow with capacity 4 and 300 files (delivered={})",
        watcher.delivered()
    );
    drop(rx);
    watcher.stop().unwrap();
}
