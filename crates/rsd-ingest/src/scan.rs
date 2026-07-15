//! Scan-based reconciliation (P1.3) and the work applier.
//!
//! `rescan` is the convergence authority: readdir-diff of a scope against the
//! catalog, batched into single catalog transactions per directory. `apply_work`
//! resolves coalescer output by lstat.

use crate::Result;
use rsd_catalog::{Catalog, ObjectKind, StatInfo};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Operation accounting: scoped-rescan tests assert on these (P1.3), and the
/// e2e harness proves "zero full rescans" with them (P1.6).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanStats {
    pub dirs_read: u64,
    pub lstats: u64,
    pub upserts: u64,
    pub removals: u64,
}

impl ScanStats {
    pub fn absorb(&mut self, other: ScanStats) {
        self.dirs_read += other.dirs_read;
        self.lstats += other.lstats;
        self.upserts += other.upserts;
        self.removals += other.removals;
    }
}

/// Escalation ladder for a unit of work; ordering matters (merge takes max).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkKind {
    /// lstat one path and reconcile it (shallow-rescan if it's a known dir).
    Probe,
    /// readdir-diff one directory level.
    RescanShallow,
    /// readdir-diff a whole subtree.
    RescanRecursive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    pub path: PathBuf,
    pub kind: WorkKind,
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn is_not_found(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

/// Remove a path from the catalog, as a subtree if it was a directory.
fn remove_cataloged(cat: &Catalog, path: &str, stats: &mut ScanStats) -> Result<()> {
    match cat.get_by_path(path)? {
        Some((_, rec)) if rec.kind == ObjectKind::Dir => {
            stats.removals += cat.remove_subtree(path)? as u64;
        }
        Some(_) if cat.remove_path(path)? => {
            stats.removals += 1;
        }
        Some(_) => {}
        None => {}
    }
    Ok(())
}

/// Reconcile one directory level: lstat + upsert every fs child, remove every
/// cataloged child no longer present, optionally recurse into child dirs.
fn scan_dir(cat: &Catalog, dir: &Path, recursive: bool, stats: &mut ScanStats) -> Result<()> {
    let mut fs_children: Vec<(String, StatInfo)> = Vec::new();
    let mut child_dirs: Vec<PathBuf> = Vec::new();

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if is_not_found(&e) => {
            remove_cataloged(cat, &path_str(dir), stats)?;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    stats.dirs_read += 1;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        stats.lstats += 1;
        let md = match fs::symlink_metadata(&path) {
            Ok(md) => md,
            // Raced away between readdir and lstat: it simply isn't in
            // fs_children, so the diff below removes it if cataloged.
            Err(e) if is_not_found(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        let st = StatInfo::from_metadata(&md);
        if st.kind == ObjectKind::Dir {
            child_dirs.push(path.clone());
        }
        fs_children.push((path_str(&path), st));
    }

    stats.upserts += fs_children.len() as u64;
    cat.apply_stats(&fs_children)?;

    // Remove cataloged children that no longer exist on disk.
    let live: std::collections::HashSet<&str> =
        fs_children.iter().map(|(p, _)| p.as_str()).collect();
    for cataloged in cat.children(&path_str(dir))? {
        if !live.contains(cataloged.as_str()) {
            remove_cataloged(cat, &cataloged, stats)?;
        }
    }

    if recursive {
        for d in child_dirs {
            scan_dir(cat, &d, true, stats)?;
        }
    }
    Ok(())
}

/// Reconcile `path` against the filesystem: the path itself, plus its children
/// (one level, or the whole subtree when `recursive`).
pub fn rescan(cat: &Catalog, path: &Path, recursive: bool) -> Result<ScanStats> {
    let mut stats = ScanStats::default();
    stats.lstats += 1;
    match fs::symlink_metadata(path) {
        Err(e) if is_not_found(&e) => {
            remove_cataloged(cat, &path_str(path), &mut stats)?;
        }
        Err(e) => return Err(e.into()),
        Ok(md) => {
            let st = StatInfo::from_metadata(&md);
            cat.apply_stat(&path_str(path), &st)?;
            stats.upserts += 1;
            if st.kind == ObjectKind::Dir {
                scan_dir(cat, path, recursive, &mut stats)?;
            }
        }
    }
    Ok(stats)
}

/// Full recursive reconciliation of a root — bootstrap and last-resort repair.
pub fn bootstrap(cat: &Catalog, root: &Path) -> Result<ScanStats> {
    rescan(cat, root, true)
}

/// Resolve one work item. `lstat` decides everything; the item is only a hint
/// about *where* to look.
pub fn apply_work(cat: &Catalog, item: &WorkItem) -> Result<ScanStats> {
    match item.kind {
        WorkKind::RescanShallow => rescan(cat, &item.path, false),
        WorkKind::RescanRecursive => rescan(cat, &item.path, true),
        WorkKind::Probe => {
            let mut stats = ScanStats::default();
            let pstr = path_str(&item.path);
            stats.lstats += 1;
            match fs::symlink_metadata(&item.path) {
                Err(e) if is_not_found(&e) => {
                    remove_cataloged(cat, &pstr, &mut stats)?;
                    Ok(stats)
                }
                Err(e) => Err(e.into()),
                Ok(md) => {
                    let st = StatInfo::from_metadata(&md);
                    if st.kind != ObjectKind::Dir {
                        cat.apply_stat(&pstr, &st)?;
                        stats.upserts += 1;
                        return Ok(stats);
                    }
                    // Directories escalate:
                    //  - unknown dir (created or moved in): its children got no
                    //    events of their own => recursive rescan;
                    //  - known dir: shallow rescan to absorb coalesced child
                    //    churn cheaply.
                    let known = matches!(
                        cat.get_by_path(&pstr)?,
                        Some((_, rec)) if rec.kind == ObjectKind::Dir
                            && rec.file_id == st.file_id
                    );
                    let mut s = rescan(cat, &item.path, !known)?;
                    s.lstats += stats.lstats;
                    Ok(s)
                }
            }
        }
    }
}
