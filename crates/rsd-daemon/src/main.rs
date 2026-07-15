//! P1.6 smoke binary: `rsd-daemon watch <root> [--db <path>]` — bootstrap a
//! catalog, keep it live-converged against FSEvents, print a stats line.

use rsd_catalog::{Catalog, Durability};
use rsd_daemon::{bring_up, PipelineConfig};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn usage() -> ! {
    eprintln!("usage: rsd-daemon watch <root> [--db <path>]");
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
    let mut db = None;
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("watch") => {}
        _ => usage(),
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "--db" => {
                db = Some(std::path::PathBuf::from(
                    it.next().unwrap_or_else(|| usage()),
                ))
            }
            _ if root.is_none() => root = Some(std::path::PathBuf::from(a)),
            _ => usage(),
        }
    }
    let root = root.unwrap_or_else(|| usage()).canonicalize()?;
    let db = db.unwrap_or_else(|| root.join(".rsd-catalog.redb"));

    let catalog = Arc::new(
        Catalog::open_with_durability(&db, Durability::Eventual)
            .map_err(|e| std::io::Error::other(e.to_string()))?,
    );

    eprintln!("bootstrapping {}...", root.display());
    let t0 = std::time::Instant::now();
    let (pipeline, boot) = bring_up(catalog.clone(), &root, PipelineConfig::default())?;
    eprintln!(
        "bootstrap done in {:?}: {} dirs, {} entries; watching (ctrl-c to exit)",
        t0.elapsed(),
        boot.dirs_read,
        boot.upserts
    );

    loop {
        std::thread::sleep(Duration::from_secs(5));
        pipeline.recover_overflow_if_any(&catalog, &root);
        let s = *pipeline.stats.lock().unwrap();
        eprintln!(
            "entries={} objects={} work_items={} full_rescans={} lstats={} removals={}",
            catalog.entry_count().unwrap_or(0),
            catalog.object_count().unwrap_or(0),
            pipeline.counters.work_items.load(Ordering::Relaxed),
            pipeline.counters.full_rescans.load(Ordering::Relaxed),
            s.lstats,
            s.removals,
        );
    }
}
