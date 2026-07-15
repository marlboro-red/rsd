//! Scan-based reconciliation (P1.3) and work resolution.
//!
//! Phase-2 split (DESIGN.md §7.3): `resolve_work` READS filesystem + catalog and
//! produces absolute `Change` records; it never writes. The committer journals
//! those records, then applies them to the catalog as a projection. The phase-1
//! direct path (`apply_work`, `rescan`, `bootstrap`) is the same resolution
//! followed by an unjournaled apply — one logic path, two durability modes.
//!
//! Subtree removals are expanded to per-path records at resolve time so every
//! journal record is absolute and replay is shape-independent (§6.1).

use crate::Result;
use rsd_catalog::{Catalog, Change, ObjectKind, StatInfo};
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

    fn count_changes(&mut self, changes: &[Change]) {
        for ch in changes {
            match ch {
                Change::Upsert { .. } => self.upserts += 1,
                Change::RemovePath { .. } => self.removals += 1,
            }
        }
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

/// Emit removals for a cataloged path — expanded per-path if it's a directory.
fn resolve_removal(cat: &Catalog, path: &str, out: &mut Vec<Change>) -> Result<()> {
    match cat.get_by_path(path)? {
        Some((_, rec)) if rec.kind == ObjectKind::Dir => {
            for p in cat.subtree_paths(path)? {
                out.push(Change::RemovePath { path: p });
            }
        }
        Some(_) => out.push(Change::RemovePath {
            path: path.to_string(),
        }),
        None => {}
    }
    Ok(())
}

/// Reconcile one directory level into `out`: upsert every fs child, remove
/// every cataloged child no longer present, optionally recurse.
fn resolve_dir(
    cat: &Catalog,
    dir: &Path,
    recursive: bool,
    out: &mut Vec<Change>,
    stats: &mut ScanStats,
) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if is_not_found(&e) => {
            return resolve_removal(cat, &path_str(dir), out);
        }
        Err(e) => return Err(e.into()),
    };
    stats.dirs_read += 1;

    let mut live: Vec<String> = Vec::new();
    let mut child_dirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        stats.lstats += 1;
        let md = match fs::symlink_metadata(&path) {
            Ok(md) => md,
            // Raced away between readdir and lstat: absent from `live`, so the
            // diff below removes it if cataloged.
            Err(e) if is_not_found(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        let st = StatInfo::from_metadata(&md);
        if st.kind == ObjectKind::Dir {
            child_dirs.push(path.clone());
        }
        let pstr = path_str(&path);
        live.push(pstr.clone());
        out.push(Change::Upsert {
            path: pstr,
            stat: st,
        });
    }

    // Cataloged children that no longer exist on disk.
    let live_set: std::collections::HashSet<&str> = live.iter().map(String::as_str).collect();
    for cataloged in cat.children(&path_str(dir))? {
        if !live_set.contains(cataloged.as_str()) {
            resolve_removal(cat, &cataloged, out)?;
        }
    }

    if recursive {
        for d in child_dirs {
            resolve_dir(cat, &d, true, out, stats)?;
        }
    }
    Ok(())
}

fn resolve_rescan(
    cat: &Catalog,
    path: &Path,
    recursive: bool,
    out: &mut Vec<Change>,
    stats: &mut ScanStats,
) -> Result<()> {
    stats.lstats += 1;
    match fs::symlink_metadata(path) {
        Err(e) if is_not_found(&e) => resolve_removal(cat, &path_str(path), out),
        Err(e) => Err(e.into()),
        Ok(md) => {
            let st = StatInfo::from_metadata(&md);
            out.push(Change::Upsert {
                path: path_str(path),
                stat: st,
            });
            if st.kind == ObjectKind::Dir {
                resolve_dir(cat, path, recursive, out, stats)?;
            }
            Ok(())
        }
    }
}

/// Resolve one work item into absolute changes. Pure read path: lstat decides
/// everything; the item is only a hint about *where* to look.
pub fn resolve_work(cat: &Catalog, item: &WorkItem) -> Result<(Vec<Change>, ScanStats)> {
    let mut out = Vec::new();
    let mut stats = ScanStats::default();
    match item.kind {
        WorkKind::RescanShallow => resolve_rescan(cat, &item.path, false, &mut out, &mut stats)?,
        WorkKind::RescanRecursive => resolve_rescan(cat, &item.path, true, &mut out, &mut stats)?,
        WorkKind::Probe => {
            let pstr = path_str(&item.path);
            stats.lstats += 1;
            match fs::symlink_metadata(&item.path) {
                Err(e) if is_not_found(&e) => resolve_removal(cat, &pstr, &mut out)?,
                Err(e) => return Err(e.into()),
                Ok(md) => {
                    let st = StatInfo::from_metadata(&md);
                    if st.kind != ObjectKind::Dir {
                        out.push(Change::Upsert {
                            path: pstr,
                            stat: st,
                        });
                    } else {
                        // Directories escalate:
                        //  - unknown dir (created or moved in): its children got
                        //    no events of their own => recursive rescan;
                        //  - known dir: shallow rescan to absorb coalesced child
                        //    churn cheaply.
                        let known = matches!(
                            cat.get_by_path(&pstr)?,
                            Some((_, rec)) if rec.kind == ObjectKind::Dir
                                && rec.file_id == st.file_id
                        );
                        resolve_rescan(cat, &item.path, !known, &mut out, &mut stats)?;
                    }
                }
            }
        }
    }
    stats.count_changes(&out);
    Ok((out, stats))
}

/// Resolve + apply directly (unjournaled): phase-1 path and tooling.
pub fn apply_work(cat: &Catalog, item: &WorkItem) -> Result<ScanStats> {
    let (changes, stats) = resolve_work(cat, item)?;
    cat.apply_changes_direct(&changes)?;
    Ok(stats)
}

/// Reconcile `path` against the filesystem, applying directly.
pub fn rescan(cat: &Catalog, path: &Path, recursive: bool) -> Result<ScanStats> {
    let kind = if recursive {
        WorkKind::RescanRecursive
    } else {
        WorkKind::RescanShallow
    };
    apply_work(
        cat,
        &WorkItem {
            path: path.to_path_buf(),
            kind,
        },
    )
}

/// Full recursive reconciliation of a root — bootstrap and last-resort repair.
pub fn bootstrap(cat: &Catalog, root: &Path) -> Result<ScanStats> {
    rescan(cat, root, true)
}
