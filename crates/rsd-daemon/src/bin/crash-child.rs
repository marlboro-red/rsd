//! Crash-injection child (P2.2 + P2.4): processes a deterministic synthetic
//! event stream through the full fenced commit path —
//!   resolve → journal append → catalog apply → cursor advance —
//! and gets SIGKILLed at random points by the harness. Small journal segments
//! force sealing to happen under fire too.

use rsd_catalog::{Change, Durability};
use rsd_daemon::commit::{synth, Committer};
use rsd_log::{CursorStore, Journal, JournalConfig, Source};
use std::path::Path;

const BATCH: u64 = 16;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: crash-child <dir> <ops>");
        std::process::exit(2);
    }
    let dir = Path::new(&args[0]);
    let ops: u64 = args[1].parse().expect("ops must be a number");
    if let Err(e) = run(dir, ops) {
        eprintln!("crash-child failed: {e}");
        std::process::exit(1);
    }
}

fn run(dir: &Path, ops: u64) -> Result<(), Box<dyn std::error::Error>> {
    // Kill-9 durability model: SIGKILL never loses page-cache writes, so both
    // stores run without fsync — the gate tests ORDERING and ATOMICITY.
    // A catalog torn beyond redb's recovery is a projection-rebuild event
    // (failure matrix §6.8), not a failure: recover() below replays it.
    let (catalog, rebuilt) =
        rsd_daemon::open_catalog_resilient(&dir.join("cat.redb"), Durability::Eventual)?;
    if rebuilt {
        eprintln!("crash-child: catalog projection rebuilt from journal");
    }
    let journal = Journal::open(
        &dir.join("journal"),
        JournalConfig {
            sync_on_append: false,
            segment_max_bytes: 32 * 1024, // force sealing under crashes
        },
    )?;
    let mut committer = Committer::new(catalog, journal);
    committer.recover()?;

    let cursor = CursorStore::new(&dir.join("cursor"));
    // Fenced resume: everything before the cursor is durably journaled;
    // everything after gets re-delivered (duplicates are watermark/idempotent
    // no-ops downstream).
    let mut i = cursor.get()?.unwrap_or(0);
    while i < ops {
        let end = (i + BATCH).min(ops);
        let changes: Vec<Change> = (i..end).map(synth::change).collect();
        committer.commit(Source::Synthetic, &changes)?;
        // The fence: cursor advances only after the batch is journaled+applied.
        cursor.set(end)?;
        i = end;
    }
    Ok(())
}
