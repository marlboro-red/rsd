//! rsd-catalog: the FsObject/Entry catalog (DESIGN.md §5, §6.3).
//!
//! Two-entity model: an `FsObject` is a content-bearing filesystem node identified
//! by `(dev, ino)` plus birthtime generation-evidence; an entry is a path pointing
//! at an object, many-to-one (hard links). Paths are attributes, not identity.
//!
//! Phase-1 scope: identity, stat attributes, path/fileid indexes, orphan grace for
//! rename identity preservation. Content hashes, kMDItem attrs, and history arrive
//! with later phases.

use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};

pub use redb::Durability;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

const OBJECTS: TableDefinition<u64, &[u8]> = TableDefinition::new("objects");
const BY_PATH: TableDefinition<&str, u64> = TableDefinition::new("by_path");
const BY_FILEID: TableDefinition<&[u8], u64> = TableDefinition::new("by_fileid");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

const NEXT_OID: &str = "next_oid";

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    // Boxed: redb::Error is ~160 bytes and would bloat every Result.
    #[error("redb: {0}")]
    Db(Box<redb::Error>),
    #[error("encode: {0}")]
    Encode(#[from] postcard::Error),
    #[error("invariant violated: {0}")]
    Invariant(String),
}

macro_rules! from_redb {
    ($($t:ty),*) => {$(
        impl From<$t> for CatalogError {
            fn from(e: $t) -> Self {
                Self::Db(Box::new(e.into()))
            }
        }
    )*};
}
from_redb!(
    redb::Error,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::DatabaseError
);

pub type Result<T> = std::result::Result<T, CatalogError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectKind {
    File,
    Dir,
    Symlink,
}

/// Filesystem identity: device + inode. Birthtime disambiguates inode reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

impl FileId {
    fn key(&self) -> [u8; 16] {
        let mut k = [0u8; 16];
        k[..8].copy_from_slice(&self.dev.to_be_bytes());
        k[8..].copy_from_slice(&self.ino.to_be_bytes());
        k
    }
}

/// Result of an lstat, the only evidence `apply_stat` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatInfo {
    pub kind: ObjectKind,
    pub file_id: FileId,
    pub size: u64,
    pub mtime_ns: i64,
    pub birthtime_ns: i64,
    pub nlink: u64,
}

impl StatInfo {
    /// Build from `std::fs::symlink_metadata` output.
    pub fn from_metadata(md: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        let ft = md.file_type();
        let kind = if ft.is_symlink() {
            ObjectKind::Symlink
        } else if ft.is_dir() {
            ObjectKind::Dir
        } else {
            ObjectKind::File
        };
        let birthtime_ns = md
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        StatInfo {
            kind,
            file_id: FileId {
                dev: md.dev(),
                ino: md.ino(),
            },
            size: md.size(),
            mtime_ns: md
                .mtime()
                .saturating_mul(1_000_000_000)
                .saturating_add(md.mtime_nsec()),
            birthtime_ns,
            nlink: md.nlink(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectRecord {
    pub kind: ObjectKind,
    pub file_id: FileId,
    pub birthtime_ns: i64,
    pub size: u64,
    pub mtime_ns: i64,
    pub nlink: u64,
    /// All live entry paths referencing this object (hard links => several).
    pub entry_paths: Vec<String>,
    /// Set when the last entry was removed; the object lingers for a grace
    /// period so a rename (remove-then-probe ordering) keeps its identity.
    pub orphaned_at_ns: Option<u64>,
}

/// What `apply_stat` did — used by scanners/tests for op accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    CreatedObject(u64),
    UpdatedObject(u64),
    /// Existing object gained/kept this path after the path previously pointed
    /// at a different object (or the object was resurrected from orphan state).
    RepointedPath(u64),
}

impl Applied {
    pub fn oid(&self) -> u64 {
        match *self {
            Applied::CreatedObject(o) | Applied::UpdatedObject(o) | Applied::RepointedPath(o) => o,
        }
    }
}

/// An absolute, self-contained catalog transition — the journal payload
/// (DESIGN.md §6.1). Deliberately *not* relative or subtree-shaped: replaying a
/// `Change` must produce the same effect regardless of catalog shape at replay
/// time, so subtree removals are expanded to per-path records at resolve time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Change {
    Upsert { path: String, stat: StatInfo },
    RemovePath { path: String },
}

impl Change {
    pub fn path(&self) -> &str {
        match self {
            Change::Upsert { path, .. } | Change::RemovePath { path } => path,
        }
    }
}

const APPLIED_LSN: &str = "applied_lsn";

pub struct Catalog {
    db: Database,
    durability: redb::Durability,
}

fn apply_change_in(t: &mut Tables<'_>, ch: &Change, now_ns: u64) -> Result<()> {
    match ch {
        Change::Upsert { path, stat } => {
            apply_stat_in(t, path, stat, now_ns)?;
        }
        Change::RemovePath { path } => {
            remove_path_in(t, path, now_ns)?;
        }
    }
    Ok(())
}

struct Tables<'txn> {
    objects: redb::Table<'txn, u64, &'static [u8]>,
    by_path: redb::Table<'txn, &'static str, u64>,
    by_fileid: redb::Table<'txn, &'static [u8], u64>,
    meta: redb::Table<'txn, &'static str, u64>,
}

fn get_object(t: &Tables<'_>, oid: u64) -> Result<Option<ObjectRecord>> {
    match t.objects.get(oid)? {
        Some(g) => Ok(Some(postcard::from_bytes(g.value())?)),
        None => Ok(None),
    }
}

fn put_object(t: &mut Tables<'_>, oid: u64, rec: &ObjectRecord) -> Result<()> {
    let buf = postcard::to_allocvec(rec)?;
    t.objects.insert(oid, buf.as_slice())?;
    Ok(())
}

fn alloc_oid(t: &mut Tables<'_>) -> Result<u64> {
    let next = t.meta.get(NEXT_OID)?.map(|g| g.value()).unwrap_or(1);
    t.meta.insert(NEXT_OID, next + 1)?;
    Ok(next)
}

/// Remove `path` from the object's entry list; orphan (not delete) on last entry.
fn detach_path(t: &mut Tables<'_>, oid: u64, path: &str, now_ns: u64) -> Result<()> {
    let Some(mut rec) = get_object(t, oid)? else {
        return Ok(());
    };
    rec.entry_paths.retain(|p| p != path);
    if rec.entry_paths.is_empty() {
        rec.orphaned_at_ns = Some(now_ns);
    }
    put_object(t, oid, &rec)
}

/// Hard-delete an object and every index reference to it.
fn delete_object(t: &mut Tables<'_>, oid: u64) -> Result<()> {
    if let Some(rec) = get_object(t, oid)? {
        for p in &rec.entry_paths {
            t.by_path.remove(p.as_str())?;
        }
        t.by_fileid.remove(rec.file_id.key().as_slice())?;
    }
    t.objects.remove(oid)?;
    Ok(())
}

fn apply_stat_in(t: &mut Tables<'_>, path: &str, st: &StatInfo, now_ns: u64) -> Result<Applied> {
    let key = st.file_id.key();
    let existing_oid = t.by_fileid.get(key.as_slice())?.map(|g| g.value());
    let path_oid = t.by_path.get(path)?.map(|g| g.value());

    // Inode-reuse check: same (dev,ino) but different birthtime is a new object.
    let existing_oid = match existing_oid {
        Some(oid) => match get_object(t, oid)? {
            Some(rec) if rec.birthtime_ns == st.birthtime_ns && rec.kind == st.kind => Some(oid),
            Some(_) => {
                delete_object(t, oid)?;
                None
            }
            None => None,
        },
        None => None,
    };

    match existing_oid {
        Some(oid) => {
            let mut rec = get_object(t, oid)?.expect("checked above");
            let was_orphan = rec.orphaned_at_ns.is_some();
            rec.size = st.size;
            rec.mtime_ns = st.mtime_ns;
            rec.nlink = st.nlink;
            rec.orphaned_at_ns = None;
            let mut repointed = was_orphan;
            if !rec.entry_paths.iter().any(|p| p == path) {
                rec.entry_paths.push(path.to_string());
                repointed = true;
            }
            put_object(t, oid, &rec)?;
            // If the path previously named a different object, detach it there.
            if let Some(old) = path_oid {
                if old != oid {
                    detach_path(t, old, path, now_ns)?;
                    repointed = true;
                }
            }
            t.by_path.insert(path, oid)?;
            if repointed {
                Ok(Applied::RepointedPath(oid))
            } else {
                Ok(Applied::UpdatedObject(oid))
            }
        }
        None => {
            if let Some(old) = path_oid {
                detach_path(t, old, path, now_ns)?;
            }
            let oid = alloc_oid(t)?;
            let rec = ObjectRecord {
                kind: st.kind,
                file_id: st.file_id,
                birthtime_ns: st.birthtime_ns,
                size: st.size,
                mtime_ns: st.mtime_ns,
                nlink: st.nlink,
                entry_paths: vec![path.to_string()],
                orphaned_at_ns: None,
            };
            put_object(t, oid, &rec)?;
            t.by_fileid.insert(key.as_slice(), oid)?;
            t.by_path.insert(path, oid)?;
            Ok(Applied::CreatedObject(oid))
        }
    }
}

fn remove_path_in(t: &mut Tables<'_>, path: &str, now_ns: u64) -> Result<bool> {
    let Some(oid) = t.by_path.get(path)?.map(|g| g.value()) else {
        return Ok(false);
    };
    t.by_path.remove(path)?;
    detach_path(t, oid, path, now_ns)?;
    Ok(true)
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// A (path, kind, ino, size) view used by convergence oracles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntrySummary {
    pub kind: ObjectKind,
    pub ino: u64,
    pub size: u64,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_durability(path, redb::Durability::Immediate)
    }

    /// Open with reduced durability (no fsync per commit). Used by tests and by
    /// callers whose durability is carried by the journal (phase 2+).
    pub fn open_with_durability(path: &Path, durability: redb::Durability) -> Result<Self> {
        let db = Database::create(path)?;
        // Ensure tables exist so read paths never hit TableDoesNotExist.
        let txn = db.begin_write()?;
        {
            txn.open_table(OBJECTS)?;
            txn.open_table(BY_PATH)?;
            txn.open_table(BY_FILEID)?;
            txn.open_table(META)?;
        }
        txn.commit()?;
        Ok(Catalog { db, durability })
    }

    fn with_write<R>(&self, f: impl FnOnce(&mut Tables<'_>) -> Result<R>) -> Result<R> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(self.durability);
        let r = {
            let mut t = Tables {
                objects: txn.open_table(OBJECTS)?,
                by_path: txn.open_table(BY_PATH)?,
                by_fileid: txn.open_table(BY_FILEID)?,
                meta: txn.open_table(META)?,
            };
            f(&mut t)?
        };
        txn.commit()?;
        Ok(r)
    }

    /// Upsert a path→object binding from lstat evidence.
    pub fn apply_stat(&self, path: &str, st: &StatInfo) -> Result<Applied> {
        self.with_write(|t| apply_stat_in(t, path, st, now_ns()))
    }

    /// Batch upsert in a single transaction (scanner fast path).
    pub fn apply_stats(&self, items: &[(String, StatInfo)]) -> Result<Vec<Applied>> {
        self.with_write(|t| {
            let now = now_ns();
            items
                .iter()
                .map(|(p, st)| apply_stat_in(t, p, st, now))
                .collect()
        })
    }

    /// Remove one path binding. Returns false if the path was not cataloged.
    pub fn remove_path(&self, path: &str) -> Result<bool> {
        self.with_write(|t| remove_path_in(t, path, now_ns()))
    }

    /// Remove a path and every cataloged path strictly beneath it.
    pub fn remove_subtree(&self, path: &str) -> Result<usize> {
        let victims = self.subtree_paths(path)?;
        self.with_write(|t| {
            let now = now_ns();
            let mut n = 0;
            for p in &victims {
                if remove_path_in(t, p, now)? {
                    n += 1;
                }
            }
            Ok(n)
        })
    }

    /// The catalog's projection watermark: the highest journal LSN whose effect
    /// is durably reflected here. 0 means "nothing applied".
    pub fn applied_lsn(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        let meta = txn.open_table(META)?;
        Ok(meta.get(APPLIED_LSN)?.map(|g| g.value()).unwrap_or(0))
    }

    /// Apply journaled changes with exactly-once watermark discipline: the batch
    /// lands in ONE transaction together with the watermark advance, so a crash
    /// either applies all of `changes` or none — and replaying an already-applied
    /// batch (lsn <= watermark) is a no-op. `first_lsn` is the LSN of
    /// `changes[0]`; the batch must be LSN-contiguous (journal appends are).
    ///
    /// Returns the number of changes actually applied (skipped ones were already
    /// covered by the watermark).
    pub fn apply_changes(&self, first_lsn: u64, changes: &[Change]) -> Result<u64> {
        if changes.is_empty() {
            return Ok(0);
        }
        self.with_write(|t| {
            let applied = t.meta.get(APPLIED_LSN)?.map(|g| g.value()).unwrap_or(0);
            let now = now_ns();
            let mut n = 0u64;
            for (i, ch) in changes.iter().enumerate() {
                let lsn = first_lsn + i as u64;
                if lsn <= applied {
                    continue;
                }
                apply_change_in(t, ch, now)?;
                n += 1;
            }
            let last = first_lsn + changes.len() as u64 - 1;
            if last > applied {
                t.meta.insert(APPLIED_LSN, last)?;
            }
            Ok(n)
        })
    }

    /// Apply changes without watermark bookkeeping (phase-1 direct path and
    /// tooling). One transaction for the whole batch.
    pub fn apply_changes_direct(&self, changes: &[Change]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        self.with_write(|t| {
            let now = now_ns();
            for ch in changes {
                apply_change_in(t, ch, now)?;
            }
            Ok(())
        })
    }

    /// The path itself (if present) plus all cataloged paths beneath it.
    pub fn subtree_paths(&self, path: &str) -> Result<Vec<String>> {
        let txn = self.db.begin_read()?;
        let by_path = txn.open_table(BY_PATH)?;
        let mut out = Vec::new();
        if by_path.get(path)?.is_some() {
            out.push(path.to_string());
        }
        let prefix = format!("{}/", path.trim_end_matches('/'));
        for item in by_path.range(prefix.as_str()..)? {
            let (k, _) = item?;
            let p = k.value();
            if !p.starts_with(prefix.as_str()) {
                break;
            }
            out.push(p.to_string());
        }
        Ok(out)
    }

    /// Immediate cataloged children of a directory path.
    pub fn children(&self, dir: &str) -> Result<Vec<String>> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        let mut out = Vec::new();
        for p in self.subtree_paths(dir)? {
            if p == dir {
                continue;
            }
            let rest = &p[prefix.len()..];
            if !rest.contains('/') {
                out.push(p);
            }
        }
        Ok(out)
    }

    pub fn get_by_path(&self, path: &str) -> Result<Option<(u64, ObjectRecord)>> {
        let txn = self.db.begin_read()?;
        let by_path = txn.open_table(BY_PATH)?;
        let objects = txn.open_table(OBJECTS)?;
        let Some(oid) = by_path.get(path)?.map(|g| g.value()) else {
            return Ok(None);
        };
        let Some(g) = objects.get(oid)? else {
            return Ok(None);
        };
        Ok(Some((oid, postcard::from_bytes(g.value())?)))
    }

    /// Every live (path → summary) binding. Orphaned objects have no entries and
    /// are naturally excluded.
    pub fn listing(&self) -> Result<BTreeMap<String, EntrySummary>> {
        let txn = self.db.begin_read()?;
        let by_path = txn.open_table(BY_PATH)?;
        let objects = txn.open_table(OBJECTS)?;
        let mut out = BTreeMap::new();
        for item in by_path.iter()? {
            let (k, v) = item?;
            let Some(g) = objects.get(v.value())? else {
                continue;
            };
            let rec: ObjectRecord = postcard::from_bytes(g.value())?;
            out.insert(
                k.value().to_string(),
                EntrySummary {
                    kind: rec.kind,
                    ino: rec.file_id.ino,
                    size: rec.size,
                },
            );
        }
        Ok(out)
    }

    /// Delete objects that have been orphaned longer than `grace`.
    pub fn sweep_orphans(&self, grace: std::time::Duration) -> Result<usize> {
        let cutoff = now_ns().saturating_sub(grace.as_nanos() as u64);
        self.with_write(|t| {
            let mut victims = Vec::new();
            for item in t.objects.iter()? {
                let (k, v) = item?;
                let rec: ObjectRecord = postcard::from_bytes(v.value())?;
                if matches!(rec.orphaned_at_ns, Some(ts) if ts <= cutoff) {
                    victims.push(k.value());
                }
            }
            for oid in &victims {
                delete_object(t, *oid)?;
            }
            Ok(victims.len())
        })
    }

    pub fn entry_count(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        Ok(txn.open_table(BY_PATH)?.len()?)
    }

    pub fn object_count(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        Ok(txn.open_table(OBJECTS)?.len()?)
    }

    /// Verify the mirror invariants between objects, by_path, and by_fileid.
    /// Test/scrubber use; O(catalog).
    pub fn check_invariants(&self) -> Result<()> {
        let txn = self.db.begin_read()?;
        let objects = txn.open_table(OBJECTS)?;
        let by_path = txn.open_table(BY_PATH)?;
        let by_fileid = txn.open_table(BY_FILEID)?;

        let mut entry_total = 0u64;
        for item in objects.iter()? {
            let (k, v) = item?;
            let oid = k.value();
            let rec: ObjectRecord = postcard::from_bytes(v.value())?;
            if rec.entry_paths.is_empty() && rec.orphaned_at_ns.is_none() {
                return Err(CatalogError::Invariant(format!(
                    "object {oid} has no entries but is not orphaned"
                )));
            }
            if !rec.entry_paths.is_empty() && rec.orphaned_at_ns.is_some() {
                return Err(CatalogError::Invariant(format!(
                    "object {oid} has entries but is marked orphaned"
                )));
            }
            for p in &rec.entry_paths {
                entry_total += 1;
                match by_path.get(p.as_str())? {
                    Some(g) if g.value() == oid => {}
                    other => {
                        return Err(CatalogError::Invariant(format!(
                            "object {oid} lists path {p:?} but by_path maps it to {:?}",
                            other.map(|g| g.value())
                        )));
                    }
                }
            }
            match by_fileid.get(rec.file_id.key().as_slice())? {
                Some(g) if g.value() == oid => {}
                other => {
                    return Err(CatalogError::Invariant(format!(
                        "object {oid} fileid {:?} maps to {:?} in by_fileid",
                        rec.file_id,
                        other.map(|g| g.value())
                    )));
                }
            }
        }
        if by_path.len()? != entry_total {
            return Err(CatalogError::Invariant(format!(
                "by_path has {} rows but objects list {} entry paths",
                by_path.len()?,
                entry_total
            )));
        }
        if by_fileid.len()? != objects.len()? {
            return Err(CatalogError::Invariant(format!(
                "by_fileid has {} rows but there are {} objects",
                by_fileid.len()?,
                objects.len()?
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(kind: ObjectKind, ino: u64, size: u64, birth: i64) -> StatInfo {
        StatInfo {
            kind,
            file_id: FileId { dev: 1, ino },
            size,
            mtime_ns: 42,
            birthtime_ns: birth,
            nlink: 1,
        }
    }

    fn open_temp() -> (tempfile::TempDir, Catalog) {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open(&dir.path().join("cat.redb")).unwrap();
        (dir, cat)
    }

    #[test]
    fn create_update_rename_remove() {
        let (_d, cat) = open_temp();
        let a = cat
            .apply_stat("/r/a.txt", &st(ObjectKind::File, 10, 5, 100))
            .unwrap();
        assert!(matches!(a, Applied::CreatedObject(_)));
        let oid = a.oid();

        // Update in place.
        let a = cat
            .apply_stat("/r/a.txt", &st(ObjectKind::File, 10, 9, 100))
            .unwrap();
        assert_eq!(a, Applied::UpdatedObject(oid));
        assert_eq!(cat.get_by_path("/r/a.txt").unwrap().unwrap().1.size, 9);

        // Rename: new path stat first (probe order), then old path removal.
        let a = cat
            .apply_stat("/r/b.txt", &st(ObjectKind::File, 10, 9, 100))
            .unwrap();
        assert_eq!(a, Applied::RepointedPath(oid));
        assert!(cat.remove_path("/r/a.txt").unwrap());
        assert!(cat.get_by_path("/r/a.txt").unwrap().is_none());
        assert_eq!(cat.get_by_path("/r/b.txt").unwrap().unwrap().0, oid);
        cat.check_invariants().unwrap();

        // Remove last entry: object orphaned, invisible in listing, then swept.
        assert!(cat.remove_path("/r/b.txt").unwrap());
        assert!(cat.listing().unwrap().is_empty());
        assert_eq!(cat.object_count().unwrap(), 1);
        cat.check_invariants().unwrap();
        assert_eq!(cat.sweep_orphans(std::time::Duration::ZERO).unwrap(), 1);
        assert_eq!(cat.object_count().unwrap(), 0);
        cat.check_invariants().unwrap();
    }

    #[test]
    fn rename_preserves_identity_across_remove_first_ordering() {
        let (_d, cat) = open_temp();
        let oid = cat
            .apply_stat("/r/a", &st(ObjectKind::File, 7, 1, 5))
            .unwrap()
            .oid();
        // Applier saw the removal event first: object becomes orphaned...
        assert!(cat.remove_path("/r/a").unwrap());
        // ...then the new path is probed. Same fileid+birthtime: identity kept.
        let a = cat
            .apply_stat("/r/b", &st(ObjectKind::File, 7, 1, 5))
            .unwrap();
        assert_eq!(a, Applied::RepointedPath(oid));
        cat.check_invariants().unwrap();
    }

    #[test]
    fn hard_links_share_object_and_unlink_keeps_sibling() {
        let (_d, cat) = open_temp();
        let mut s = st(ObjectKind::File, 20, 3, 9);
        s.nlink = 2;
        let oid = cat.apply_stat("/r/one", &s).unwrap().oid();
        assert_eq!(
            cat.apply_stat("/r/two", &s).unwrap(),
            Applied::RepointedPath(oid)
        );
        assert_eq!(cat.entry_count().unwrap(), 2);
        assert_eq!(cat.object_count().unwrap(), 1);

        assert!(cat.remove_path("/r/one").unwrap());
        let (o2, rec) = cat.get_by_path("/r/two").unwrap().unwrap();
        assert_eq!(o2, oid);
        assert_eq!(rec.entry_paths, vec!["/r/two".to_string()]);
        cat.check_invariants().unwrap();
    }

    #[test]
    fn inode_reuse_with_different_birthtime_is_a_new_object() {
        let (_d, cat) = open_temp();
        let oid1 = cat
            .apply_stat("/r/x", &st(ObjectKind::File, 30, 1, 111))
            .unwrap()
            .oid();
        // Same (dev,ino), different birthtime: prior object must be replaced.
        let oid2 = cat
            .apply_stat("/r/x", &st(ObjectKind::File, 30, 2, 222))
            .unwrap()
            .oid();
        assert_ne!(oid1, oid2);
        assert_eq!(cat.object_count().unwrap(), 1);
        cat.check_invariants().unwrap();
    }

    #[test]
    fn subtree_and_children() {
        let (_d, cat) = open_temp();
        for (i, p) in [
            "/r", "/r/d", "/r/d/f1", "/r/d/f2", "/r/d/e", "/r/d/e/g", "/r/z",
        ]
        .iter()
        .enumerate()
        {
            let kind = if p.ends_with(['1', '2']) || *p == "/r/d/e/g" {
                ObjectKind::File
            } else {
                ObjectKind::Dir
            };
            cat.apply_stat(p, &st(kind, 100 + i as u64, 0, 1)).unwrap();
        }
        assert_eq!(
            cat.children("/r/d").unwrap(),
            vec![
                "/r/d/e".to_string(),
                "/r/d/f1".to_string(),
                "/r/d/f2".to_string()
            ]
        );
        assert_eq!(cat.remove_subtree("/r/d").unwrap(), 5);
        assert_eq!(
            cat.listing().unwrap().keys().cloned().collect::<Vec<_>>(),
            vec!["/r".to_string(), "/r/z".to_string()]
        );
        cat.check_invariants().unwrap();
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("cat.redb");
        {
            let cat = Catalog::open(&db).unwrap();
            cat.apply_stat("/r/a", &st(ObjectKind::File, 40, 8, 3))
                .unwrap();
        }
        let cat = Catalog::open(&db).unwrap();
        let (_, rec) = cat.get_by_path("/r/a").unwrap().unwrap();
        assert_eq!(rec.size, 8);
        cat.check_invariants().unwrap();
    }
}
