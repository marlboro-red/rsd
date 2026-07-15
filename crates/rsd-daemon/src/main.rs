//! Smoke binary: `rsd-daemon watch <root> [--state <dir>]` — recover the
//! journal, bootstrap, keep the catalog live-converged against FSEvents, print
//! a stats line.
//!
//! State (catalog + journal) lives OUTSIDE the watched root by default —
//! state inside the root would generate events about its own writes and feed
//! back into the pipeline forever.

use rsd_catalog::{Catalog, Durability};
use rsd_daemon::{bring_up, PipelineConfig};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn usage() -> ! {
    eprintln!("usage: rsd-daemon watch <root> [--state <dir>]");
    std::process::exit(2);
}

fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root = None;
    let mut state = None;
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("watch") => {}
        _ => usage(),
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "--state" => {
                state = Some(std::path::PathBuf::from(
                    it.next().unwrap_or_else(|| usage()),
                ))
            }
            _ if root.is_none() => root = Some(std::path::PathBuf::from(a)),
            _ => usage(),
        }
    }
    let root = root.unwrap_or_else(|| usage()).canonicalize()?;
    let state = state.unwrap_or_else(|| {
        // Sibling of the root: `<parent>/.rsd-state-<rootname>` — never inside.
        let name = root.file_name().map(|s| s.to_string_lossy().into_owned());
        root.parent().unwrap_or(&root).join(format!(
            ".rsd-state-{}",
            name.unwrap_or_else(|| "root".into())
        ))
    });
    if state.starts_with(&root) {
        eprintln!("error: state dir {state:?} must live outside the watched root");
        std::process::exit(2);
    }
    std::fs::create_dir_all(&state)?;

    let catalog = Arc::new(
        Catalog::open_with_durability(&state.join("catalog.redb"), Durability::Eventual)
            .map_err(|e| std::io::Error::other(e.to_string()))?,
    );

    // Content indexing: sealed worker pool, if the worker binary is present.
    let (content, lexical) = match rsd_worker::WorkerPool::new(rsd_worker::PoolConfig::default()) {
        Ok(pool) => {
            let caes = Arc::new(
                rsd_caes::Store::open(&state.join("caes.redb"))
                    .map_err(|e| std::io::Error::other(e.to_string()))?,
            );
            let plane = rsd_lexical::LexicalPlane::open(&state.join("lexical"))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            (
                Some(rsd_daemon::ContentIndexer::new(
                    Box::new(rsd_daemon::PooledExtractor(pool)),
                    caes.clone(),
                )),
                Some((plane, caes)),
            )
        }
        Err(e) => {
            eprintln!("content indexing disabled (worker pool unavailable: {e})");
            (None, None)
        }
    };

    let caes_for_live = lexical.as_ref().map(|(_, c)| c.clone());
    let live = Arc::new(std::sync::Mutex::new(rsd_live::LiveEngine::new(
        caes_for_live,
    )));

    eprintln!("bootstrapping {}...", root.display());
    let t0 = std::time::Instant::now();
    let (pipeline, boot) = bring_up(
        catalog.clone(),
        &state.join("journal"),
        &root,
        content,
        lexical,
        Some(live.clone()),
        PipelineConfig::default(),
    )?;
    let _ipc = rsd_daemon::ipc::start_ipc(
        &state.join("rsd.sock"),
        rsd_daemon::ipc::IpcCtx {
            catalog: catalog.clone(),
            lexical_dir: state.join("lexical"),
            live,
            authz: Arc::new(rsd_daemon::ipc::AuthzStore::default()),
        },
    )?;
    eprintln!("ipc listening at {}", state.join("rsd.sock").display());
    eprintln!(
        "bootstrap done in {:?}: {} dirs, {} entries; watching (ctrl-c to exit)",
        t0.elapsed(),
        boot.dirs_read,
        boot.upserts
    );

    loop {
        std::thread::sleep(Duration::from_secs(5));
        let s = *pipeline.stats.lock().unwrap();
        eprintln!(
            "entries={} objects={} work_items={} commits={} full_rescans={} lstats={} removals={}",
            catalog.entry_count().unwrap_or(0),
            catalog.object_count().unwrap_or(0),
            pipeline.counters.work_items.load(Ordering::Relaxed),
            pipeline.counters.commits.load(Ordering::Relaxed),
            pipeline.counters.full_rescans.load(Ordering::Relaxed),
            s.lstats,
            s.removals,
        );
    }
}
