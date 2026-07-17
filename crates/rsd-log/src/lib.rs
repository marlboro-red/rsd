//! rsd-log: the journal — the authority on *accepted indexing transitions*
//! (DESIGN.md §1, §6.1) — plus the fenced source-cursor store (P2.2).
//!
//! Format: append-only segments named `<first_lsn>.rlog`, each starting with an
//! 8-byte header, followed by records: `[0xA5][len:u32 LE][blake3_16(payload)]
//! [payload]`. A sealed segment gains a `<first_lsn>.seal` manifest (record
//! range + touched paths) for scoped repair per the failure matrix (§6.8).
//!
//! Performance: appends are group-committed — a whole batch is encoded into one
//! buffer and written with a single syscall; fsync happens once per batch and
//! only when `sync_on_append` is set (the journal is the system's durability
//! point; projections run without fsync). Replay streams segment-by-segment in
//! caller-sized chunks — no whole-journal materialization.

use rsd_catalog::Change;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

const SEGMENT_HEADER: &[u8; 8] = b"RSDLOG01";
const RECORD_MARKER: u8 = 0xA5;
const HASH_LEN: usize = 16;
/// marker + len + hash
const RECORD_OVERHEAD: usize = 1 + 4 + HASH_LEN;
/// Sanity bound: no single record payload may exceed this (corruption guard).
const MAX_PAYLOAD: u32 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode: {0}")]
    Encode(#[from] postcard::Error),
    #[error("corrupt record in {segment:?} at offset {offset}: {reason}")]
    Corrupt {
        segment: PathBuf,
        offset: u64,
        reason: String,
    },
    #[error("journal invariant violated: {0}")]
    Invariant(String),
}

pub type Result<T> = std::result::Result<T, LogError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    FsEvents,
    Scan,
    AntiEntropy,
    Repair,
    Synthetic,
    /// Content-indexing outcomes (SetContent records).
    Content,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    pub lsn: u64,
    pub wall_time_ns: u64,
    pub source: Source,
    pub change: Change,
}

/// Written beside a segment at seal time; enables scoped repair (which docs a
/// lost segment ranged over) without scanning the segment itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentManifest {
    pub first_lsn: u64,
    pub last_lsn: u64,
    pub records: u64,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct JournalConfig {
    pub segment_max_bytes: u64,
    /// fsync each append batch. The daemon's durability point; tests and
    /// kill-9 harnesses disable it (SIGKILL does not lose page-cache writes).
    pub sync_on_append: bool,
}

impl Default for JournalConfig {
    fn default() -> Self {
        JournalConfig {
            segment_max_bytes: 64 * 1024 * 1024,
            sync_on_append: true,
        }
    }
}

pub struct Journal {
    dir: PathBuf,
    cfg: JournalConfig,
    active: File,
    active_path: PathBuf,
    active_first_lsn: u64,
    active_len: u64,
    active_records: u64,
    active_paths: BTreeSet<String>,
    next_lsn: u64,
    /// Reused encode buffer (group commit fast path).
    scratch: Vec<u8>,
}

fn segment_name(first_lsn: u64) -> String {
    format!("{first_lsn:020}.rlog")
}

fn sync_parent(path: &Path) -> std::io::Result<()> {
    File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
}

fn list_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(stem) = name.strip_suffix(".rlog") {
            if let Ok(first) = stem.parse::<u64>() {
                out.push((first, entry.path()));
            }
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// Outcome of scanning one segment file.
struct SegmentScan {
    /// Byte offset up to which records decode cleanly.
    valid_end: u64,
    max_lsn: Option<u64>,
    records: u64,
    /// Where and why decoding stopped short of EOF, if it did.
    corrupt: Option<(u64, String)>,
    /// True only when the decoder ran out of bytes in the final frame. This is
    /// the crash-torn-tail case that may be truncated safely.
    torn_tail: bool,
}

/// Stream a segment, calling `sink` for each valid record; stops at the first
/// invalid byte. NEVER panics on arbitrary bytes (fuzz-tested).
fn scan_segment(path: &Path, mut sink: impl FnMut(LogRecord)) -> Result<SegmentScan> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut r = BufReader::with_capacity(1 << 20, file);

    let mut header = [0u8; 8];
    match r.read_exact(&mut header) {
        Ok(()) if &header == SEGMENT_HEADER => {}
        Ok(()) => {
            return Ok(SegmentScan {
                valid_end: 0,
                max_lsn: None,
                records: 0,
                corrupt: Some((0, "bad segment header".into())),
                torn_tail: false,
            });
        }
        Err(_) => {
            return Ok(SegmentScan {
                valid_end: 0,
                max_lsn: None,
                records: 0,
                corrupt: Some((0, "truncated segment header".into())),
                torn_tail: true,
            });
        }
    }

    let mut off = 8u64;
    let mut max_lsn = None;
    let mut records = 0u64;
    let mut payload = Vec::new();
    loop {
        if off >= file_len {
            return Ok(SegmentScan {
                valid_end: off,
                max_lsn,
                records,
                corrupt: None,
                torn_tail: false,
            });
        }
        let stop = |reason: &str, off: u64, max_lsn, records, torn_tail| SegmentScan {
            valid_end: off,
            max_lsn,
            records,
            corrupt: Some((off, reason.to_string())),
            torn_tail,
        };
        let mut fixed = [0u8; RECORD_OVERHEAD];
        if r.read_exact(&mut fixed).is_err() {
            return Ok(stop("truncated record frame", off, max_lsn, records, true));
        }
        if fixed[0] != RECORD_MARKER {
            return Ok(stop("bad record marker", off, max_lsn, records, false));
        }
        let len = u32::from_le_bytes([fixed[1], fixed[2], fixed[3], fixed[4]]);
        if len == 0 || len > MAX_PAYLOAD {
            let frame_reaches_eof = off + RECORD_OVERHEAD as u64 >= file_len;
            return Ok(stop(
                "implausible record length",
                off,
                max_lsn,
                records,
                frame_reaches_eof,
            ));
        }
        payload.clear();
        payload.resize(len as usize, 0);
        if r.read_exact(&mut payload).is_err() {
            return Ok(stop("truncated payload", off, max_lsn, records, true));
        }
        let hash = blake3::hash(&payload);
        if hash.as_bytes()[..HASH_LEN] != fixed[5..] {
            return Ok(stop("checksum mismatch", off, max_lsn, records, false));
        }
        let Ok(rec) = postcard::from_bytes::<LogRecord>(&payload) else {
            return Ok(stop("undecodable payload", off, max_lsn, records, false));
        };
        max_lsn = Some(rec.lsn);
        records += 1;
        sink(rec);
        off += (RECORD_OVERHEAD + len as usize) as u64;
    }
}

impl Journal {
    pub fn open(dir: &Path, cfg: JournalConfig) -> Result<Journal> {
        std::fs::create_dir_all(dir)?;
        let segments = list_segments(dir)?;

        let mut next_lsn = 1u64;
        let mut resume: Option<(u64, PathBuf, SegmentScan, BTreeSet<String>)> = None;

        for (i, (first, path)) in segments.iter().enumerate() {
            let mut paths = BTreeSet::new();
            let scan = scan_segment(path, |rec| {
                paths.insert(rec.change.path().to_string());
            })?;
            if let Some(m) = scan.max_lsn {
                next_lsn = next_lsn.max(m + 1);
            }
            let is_last = i == segments.len() - 1;
            if let Some((offset, reason)) = &scan.corrupt {
                if !is_last || !scan.torn_tail {
                    return Err(LogError::Corrupt {
                        segment: path.clone(),
                        offset: *offset,
                        reason: reason.clone(),
                    });
                }
            }
            if is_last {
                // Only an EOF-truncated final frame is a crash-torn tail. A
                // checksum/marker failure in the active segment is bit rot and
                // must not discard the valid records after it.
                if scan.torn_tail {
                    let f = OpenOptions::new().write(true).open(path)?;
                    if scan.valid_end < 8 {
                        // Crash during segment creation: the header itself is
                        // torn or missing. Rewrite it — resuming behind a bad
                        // header would strand every future record.
                        f.set_len(0)?;
                        let mut f = f;
                        f.write_all(SEGMENT_HEADER)?;
                        f.sync_data()?;
                    } else {
                        f.set_len(scan.valid_end)?;
                        f.sync_data()?;
                    }
                }
                let sealed = path.with_extension("seal").exists();
                if !sealed && scan.valid_end.max(8) < cfg.segment_max_bytes {
                    resume = Some((*first, path.clone(), scan, paths));
                }
            }
        }

        let (active_path, active_first_lsn, active_len, active_records, active_paths) = match resume
        {
            Some((first, path, scan, paths)) => {
                // Header-rewritten segments restart empty at length 8.
                let (len, records, paths) = if scan.valid_end < 8 {
                    (8, 0, BTreeSet::new())
                } else {
                    (scan.valid_end, scan.records, paths)
                };
                (path, first, len, records, paths)
            }
            None => {
                let first = next_lsn;
                let path = dir.join(segment_name(first));
                let mut f = File::create(&path)?;
                f.write_all(SEGMENT_HEADER)?;
                f.sync_data()?;
                drop(f);
                sync_parent(&path)?;
                (path, first, 8, 0, BTreeSet::new())
            }
        };

        let active = OpenOptions::new().append(true).open(&active_path)?;
        Ok(Journal {
            dir: dir.to_path_buf(),
            cfg,
            active,
            active_path,
            active_first_lsn,
            active_len,
            active_records,
            active_paths,
            next_lsn,
            scratch: Vec::with_capacity(1 << 16),
        })
    }

    /// Highest LSN ever assigned (0 if empty).
    pub fn max_lsn(&self) -> u64 {
        self.next_lsn - 1
    }

    /// Group-commit a batch: encode every record into one buffer, write it with
    /// one syscall, fsync once (if configured). Returns `(first, last)` LSNs.
    pub fn append(&mut self, source: Source, changes: &[Change]) -> Result<(u64, u64)> {
        if changes.is_empty() {
            return Err(LogError::Invariant("empty append".into()));
        }
        let wall_time_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let first = self.next_lsn;
        self.scratch.clear();
        for ch in changes {
            let rec = LogRecord {
                lsn: self.next_lsn,
                wall_time_ns,
                source,
                change: ch.clone(),
            };
            self.next_lsn += 1;
            let payload = postcard::to_allocvec(&rec)?;
            let hash = blake3::hash(&payload);
            self.scratch.push(RECORD_MARKER);
            self.scratch
                .extend_from_slice(&(payload.len() as u32).to_le_bytes());
            self.scratch.extend_from_slice(&hash.as_bytes()[..HASH_LEN]);
            self.scratch.extend_from_slice(&payload);
            self.active_paths.insert(ch.path().to_string());
        }
        self.active.write_all(&self.scratch)?;
        if self.cfg.sync_on_append {
            self.active.sync_data()?;
        }
        self.active_len += self.scratch.len() as u64;
        self.active_records += changes.len() as u64;
        let last = self.next_lsn - 1;

        if self.active_len >= self.cfg.segment_max_bytes {
            self.seal_and_rotate()?;
        }
        Ok((first, last))
    }

    pub fn sync(&mut self) -> Result<()> {
        self.active.sync_data()?;
        Ok(())
    }

    fn seal_and_rotate(&mut self) -> Result<()> {
        self.active.sync_data()?;
        let manifest = SegmentManifest {
            first_lsn: self.active_first_lsn,
            last_lsn: self.next_lsn - 1,
            records: self.active_records,
            paths: std::mem::take(&mut self.active_paths).into_iter().collect(),
        };
        let seal_path = self.active_path.with_extension("seal");
        let tmp = seal_path.with_extension("seal.tmp");
        let mut seal = File::create(&tmp)?;
        seal.write_all(&postcard::to_allocvec(&manifest)?)?;
        seal.sync_all()?;
        drop(seal);
        std::fs::rename(&tmp, &seal_path)?;
        sync_parent(&seal_path)?;

        let first = self.next_lsn;
        let path = self.dir.join(segment_name(first));
        let mut f = File::create(&path)?;
        f.write_all(SEGMENT_HEADER)?;
        f.sync_data()?;
        drop(f);
        sync_parent(&path)?;
        self.active = OpenOptions::new().append(true).open(&path)?;
        self.active_path = path;
        self.active_first_lsn = first;
        self.active_len = 8;
        self.active_records = 0;
        Ok(())
    }

    /// Stream records with `lsn >= from` to `sink` in journal order.
    ///
    /// Corruption inside a non-active segment is a hard error (bit rot in a
    /// sealed range — failure-matrix repair, not silent skip). The active
    /// segment was tail-truncated at open, so a clean end is expected there.
    pub fn replay(&self, from: u64, mut sink: impl FnMut(LogRecord)) -> Result<u64> {
        let mut delivered = 0u64;
        for (i, (_, path)) in list_segments(&self.dir)?.iter().enumerate() {
            let is_active = *path == self.active_path;
            let scan = scan_segment(path, |rec| {
                if rec.lsn >= from {
                    delivered += 1;
                    sink(rec);
                }
            })?;
            if let Some((offset, reason)) = scan.corrupt {
                if !is_active || !scan.torn_tail {
                    return Err(LogError::Corrupt {
                        segment: path.clone(),
                        offset,
                        reason,
                    });
                }
                // Active segment: only an EOF-torn frame reaches here;
                // open() already truncated it.
                let _ = i;
            }
        }
        Ok(delivered)
    }

    /// Read a sealed segment's manifest, if present.
    pub fn manifest(&self, first_lsn: u64) -> Result<Option<SegmentManifest>> {
        let p = self
            .dir
            .join(segment_name(first_lsn))
            .with_extension("seal");
        match std::fs::read(&p) {
            Ok(bytes) => Ok(Some(postcard::from_bytes(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn segment_count(&self) -> Result<usize> {
        Ok(list_segments(&self.dir)?.len())
    }
}

/// Fenced source cursor (P2.2): "events up to here are durably journaled".
/// Written durably and atomically (fsync tmp + rename + fsync parent). A
/// missing or corrupt cursor reads as `None`, which re-delivers — always the
/// safe direction.
pub struct CursorStore {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct CursorFile {
    value: u64,
    hash: [u8; HASH_LEN],
}

impl CursorStore {
    pub fn new(path: &Path) -> CursorStore {
        CursorStore {
            path: path.to_path_buf(),
        }
    }

    pub fn get(&self) -> Result<Option<u64>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let Ok(cf) = postcard::from_bytes::<CursorFile>(&bytes) else {
            return Ok(None);
        };
        let expect = blake3::hash(&cf.value.to_le_bytes());
        if expect.as_bytes()[..HASH_LEN] != cf.hash {
            return Ok(None);
        }
        Ok(Some(cf.value))
    }

    /// Advance the cursor. Callers MUST have durably journaled all work derived
    /// from events up to `value` first — that ordering is the fence.
    pub fn set(&self, value: u64) -> Result<()> {
        let hash: [u8; HASH_LEN] = blake3::hash(&value.to_le_bytes()).as_bytes()[..HASH_LEN]
            .try_into()
            .expect("length");
        let bytes = postcard::to_allocvec(&CursorFile { value, hash })?;
        let tmp = self.path.with_extension("cursor.tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &self.path)?;
        sync_parent(&self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::prelude::*;
    use rand_chacha::ChaCha8Rng;

    fn ch(i: u64) -> Change {
        Change::RemovePath {
            path: format!("/t/f{i}"),
        }
    }

    fn cfg_small() -> JournalConfig {
        JournalConfig {
            segment_max_bytes: 2_000,
            sync_on_append: false,
        }
    }

    #[test]
    fn round_trip_across_sealed_segments_with_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = Journal::open(dir.path(), cfg_small()).unwrap();
        for batch in 0..20u64 {
            let changes: Vec<Change> = (0..10).map(|k| ch(batch * 10 + k)).collect();
            let (first, last) = j.append(Source::Synthetic, &changes).unwrap();
            assert_eq!(last - first + 1, 10);
        }
        assert!(j.segment_count().unwrap() > 1, "expected sealing");
        assert_eq!(j.max_lsn(), 200);

        let mut got = Vec::new();
        let n = j.replay(1, |r| got.push(r.lsn)).unwrap();
        assert_eq!(n, 200);
        assert_eq!(got, (1..=200).collect::<Vec<_>>());

        // Manifest of the first sealed segment exists and is coherent.
        let m = j.manifest(1).unwrap().expect("manifest for sealed segment");
        assert_eq!(m.first_lsn, 1);
        assert!(m.records > 0 && !m.paths.is_empty());

        // Partial replay honors `from`.
        let mut later = Vec::new();
        j.replay(150, |r| later.push(r.lsn)).unwrap();
        assert_eq!(later, (150..=200).collect::<Vec<_>>());
    }

    #[test]
    fn reopen_resumes_lsn_allocation() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut j = Journal::open(dir.path(), cfg_small()).unwrap();
            j.append(Source::Synthetic, &[ch(1), ch(2)]).unwrap();
        }
        let mut j = Journal::open(dir.path(), cfg_small()).unwrap();
        assert_eq!(j.max_lsn(), 2);
        let (first, last) = j.append(Source::Synthetic, &[ch(3)]).unwrap();
        assert_eq!((first, last), (3, 3));
        let mut lsns = Vec::new();
        j.replay(1, |r| lsns.push(r.lsn)).unwrap();
        assert_eq!(lsns, vec![1, 2, 3]);
    }

    #[test]
    fn torn_tail_is_truncated_and_journal_continues() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path;
        {
            let mut j = Journal::open(
                dir.path(),
                JournalConfig {
                    segment_max_bytes: u64::MAX,
                    sync_on_append: false,
                },
            )
            .unwrap();
            for i in 0..100 {
                j.append(Source::Synthetic, &[ch(i)]).unwrap();
            }
            seg_path = dir.path().join(segment_name(1));
        }
        // Tear the tail mid-record.
        let len = std::fs::metadata(&seg_path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&seg_path).unwrap();
        f.set_len(len - 7).unwrap();
        drop(f);

        let mut j = Journal::open(dir.path(), cfg_small()).unwrap();
        let mut lsns = Vec::new();
        j.replay(1, |r| lsns.push(r.lsn)).unwrap();
        assert_eq!(
            lsns,
            (1..=99).collect::<Vec<_>>(),
            "exactly the torn record lost"
        );
        assert_eq!(j.max_lsn(), 99);
        // The torn LSN is reassigned to the next append (it was never acked).
        let (first, _) = j.append(Source::Synthetic, &[ch(999)]).unwrap();
        assert_eq!(first, 100);
    }

    #[test]
    fn corruption_in_sealed_segment_is_detected_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = Journal::open(dir.path(), cfg_small()).unwrap();
        for batch in 0..20u64 {
            let changes: Vec<Change> = (0..10).map(|k| ch(batch * 10 + k)).collect();
            j.append(Source::Synthetic, &changes).unwrap();
        }
        assert!(j.segment_count().unwrap() > 1);
        // Flip a byte in the middle of the FIRST (sealed) segment.
        let seg = dir.path().join(segment_name(1));
        let mut bytes = std::fs::read(&seg).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&seg, &bytes).unwrap();

        let err = j.replay(1, |_| {}).unwrap_err();
        assert!(matches!(err, LogError::Corrupt { .. }), "got {err:?}");
    }

    #[test]
    fn corruption_in_middle_of_active_segment_is_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let seg = dir.path().join(segment_name(1));
        {
            let mut journal = Journal::open(
                dir.path(),
                JournalConfig {
                    segment_max_bytes: u64::MAX,
                    sync_on_append: false,
                },
            )
            .unwrap();
            for i in 0..20 {
                journal.append(Source::Synthetic, &[ch(i)]).unwrap();
            }
        }
        let original_len = std::fs::metadata(&seg).unwrap().len();
        let mut bytes = std::fs::read(&seg).unwrap();
        bytes[8 + RECORD_OVERHEAD + 2] ^= 0x80;
        std::fs::write(&seg, bytes).unwrap();

        let error = match Journal::open(dir.path(), cfg_small()) {
            Ok(_) => panic!("mid-segment corruption was silently accepted"),
            Err(error) => error,
        };
        assert!(matches!(error, LogError::Corrupt { .. }), "got {error:?}");
        assert_eq!(
            std::fs::metadata(&seg).unwrap().len(),
            original_len,
            "mid-segment corruption must not truncate the valid suffix"
        );
    }

    #[test]
    fn fuzz_arbitrary_bytes_never_panic() {
        let mut rng = ChaCha8Rng::seed_from_u64(99);
        for trial in 0..200 {
            let dir = tempfile::tempdir().unwrap();
            let n: usize = rng.gen_range(0..4096);
            let mut bytes: Vec<u8> = (0..n).map(|_| rng.gen()).collect();
            if trial % 3 == 0 && bytes.len() >= 8 {
                // Valid header, garbage body — the nastier case.
                bytes[..8].copy_from_slice(SEGMENT_HEADER);
            }
            std::fs::write(dir.path().join(segment_name(1)), &bytes).unwrap();
            // Must not panic; may or may not salvage records.
            if let Ok(journal) = Journal::open(dir.path(), cfg_small()) {
                let _ = journal.replay(1, |_| {});
            }
        }
    }

    #[test]
    fn cursor_round_trip_and_corrupt_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let c = CursorStore::new(&dir.path().join("cursor"));
        assert_eq!(c.get().unwrap(), None);
        c.set(42).unwrap();
        assert_eq!(c.get().unwrap(), Some(42));
        c.set(43).unwrap();
        assert_eq!(c.get().unwrap(), Some(43));
        // Corrupt it: must read as None (re-deliver — the safe direction).
        std::fs::write(dir.path().join("cursor"), b"garbage").unwrap();
        assert_eq!(c.get().unwrap(), None);
    }
}
