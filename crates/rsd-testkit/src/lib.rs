//! rsd-testkit: seeded tree generation, filesystem mutation storms, and the
//! convergence oracle (P1.2). Every convergence test in the workspace builds on
//! these primitives.

use rand::prelude::*;
use rand_chacha::ChaCha8Rng;
use rsd_catalog::{Catalog, ObjectKind};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// lstat-level truth for one path, as compared by the oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsNode {
    pub kind: ObjectKind,
    pub ino: u64,
    /// Compared for files only; 0 for dirs/symlinks.
    pub size: u64,
}

fn node_of(md: &fs::Metadata) -> FsNode {
    use std::os::unix::fs::MetadataExt;
    let ft = md.file_type();
    let kind = if ft.is_symlink() {
        ObjectKind::Symlink
    } else if ft.is_dir() {
        ObjectKind::Dir
    } else {
        ObjectKind::File
    };
    FsNode {
        kind,
        ino: md.ino(),
        size: if kind == ObjectKind::File {
            md.size()
        } else {
            0
        },
    }
}

/// Walk `root` (lstat semantics, never following symlinks) into a map of
/// absolute path → node. Excludes `root` itself.
pub fn fs_listing(root: &Path) -> io::Result<BTreeMap<String, FsNode>> {
    let mut out = BTreeMap::new();
    walk(root, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, out: &mut BTreeMap<String, FsNode>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let md = fs::symlink_metadata(&path)?;
        let node = node_of(&md);
        let is_dir = node.kind == ObjectKind::Dir;
        out.insert(path.to_string_lossy().into_owned(), node);
        if is_dir {
            walk(&path, out)?;
        }
    }
    Ok(())
}

/// Generate a deterministic tree of roughly `files` files under `root`,
/// spread over nested directories, with a sprinkle of symlinks.
pub fn gen_tree(root: &Path, files: usize, seed: u64) -> io::Result<usize> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut dirs = vec![root.to_path_buf()];
    let n_dirs = (files / 8).max(1);
    for i in 0..n_dirs {
        let parent = dirs.choose(&mut rng).unwrap().clone();
        let d = parent.join(format!("d{i}"));
        fs::create_dir(&d)?;
        dirs.push(d);
    }
    let mut created: Vec<PathBuf> = Vec::new();
    for i in 0..files {
        let dir = dirs.choose(&mut rng).unwrap();
        let f = dir.join(format!("f{i}.txt"));
        fs::write(&f, format!("content-{i}-{}", rng.gen::<u32>()))?;
        created.push(f);
    }
    // ~2% symlinks to random files.
    let n_links = files / 50;
    for i in 0..n_links {
        if let Some(target) = created.choose(&mut rng) {
            let dir = dirs.choose(&mut rng).unwrap();
            let l = dir.join(format!("l{i}"));
            let _ = std::os::unix::fs::symlink(target, &l);
        }
    }
    Ok(n_dirs + files + n_links)
}

/// Seeded random filesystem mutations under a root. Refreshes its view of the
/// tree from the filesystem after structural (directory) changes, so ops never
/// target stale paths.
pub struct Mutator {
    root: PathBuf,
    rng: ChaCha8Rng,
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
    counter: u64,
    pub ops_applied: u64,
}

impl Mutator {
    pub fn new(root: &Path, seed: u64) -> io::Result<Self> {
        let mut m = Mutator {
            root: root.to_path_buf(),
            rng: ChaCha8Rng::seed_from_u64(seed),
            files: Vec::new(),
            dirs: Vec::new(),
            counter: 0,
            ops_applied: 0,
        };
        m.refresh()?;
        Ok(m)
    }

    fn refresh(&mut self) -> io::Result<()> {
        self.files.clear();
        self.dirs.clear();
        self.dirs.push(self.root.clone());
        for (p, n) in fs_listing(&self.root)? {
            match n.kind {
                ObjectKind::Dir => self.dirs.push(PathBuf::from(p)),
                ObjectKind::File => self.files.push(PathBuf::from(p)),
                ObjectKind::Symlink => {}
            }
        }
        Ok(())
    }

    fn fresh_name(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}{}", self.counter)
    }

    /// Apply `n` random mutations.
    pub fn run(&mut self, n: usize) -> io::Result<()> {
        for _ in 0..n {
            self.step()?;
        }
        Ok(())
    }

    pub fn step(&mut self) -> io::Result<()> {
        let roll = self.rng.gen_range(0..100);
        match roll {
            // Create a file.
            0..=24 => {
                let dir = self.dirs.choose(&mut self.rng).unwrap().clone();
                let name = self.fresh_name("nf");
                let f = dir.join(format!("{name}.txt"));
                fs::write(&f, format!("new-{}", self.counter))?;
                self.files.push(f);
            }
            // Rewrite a file (size change).
            25..=44 => {
                if let Some(f) = self.files.choose(&mut self.rng).cloned() {
                    let pad = "x".repeat(self.rng.gen_range(0..256));
                    fs::write(&f, format!("rewritten-{}-{pad}", self.counter))?;
                    self.counter += 1;
                }
            }
            // Delete a file.
            45..=59 => {
                if !self.files.is_empty() {
                    let i = self.rng.gen_range(0..self.files.len());
                    let f = self.files.swap_remove(i);
                    let _ = fs::remove_file(&f);
                }
            }
            // Rename a file (possibly across directories).
            60..=74 => {
                if !self.files.is_empty() {
                    let i = self.rng.gen_range(0..self.files.len());
                    let from = self.files[i].clone();
                    let dir = self.dirs.choose(&mut self.rng).unwrap().clone();
                    let name = self.fresh_name("rn");
                    let to = dir.join(format!("{name}.txt"));
                    if fs::rename(&from, &to).is_ok() {
                        self.files[i] = to;
                    }
                }
            }
            // Make a directory.
            75..=82 => {
                let parent = self.dirs.choose(&mut self.rng).unwrap().clone();
                let name = self.fresh_name("nd");
                let d = parent.join(name);
                if fs::create_dir(&d).is_ok() {
                    self.dirs.push(d);
                }
            }
            // Hard link an existing file.
            83..=88 => {
                if let Some(target) = self.files.choose(&mut self.rng).cloned() {
                    let dir = self.dirs.choose(&mut self.rng).unwrap().clone();
                    let name = self.fresh_name("hl");
                    let link = dir.join(format!("{name}.txt"));
                    if fs::hard_link(&target, &link).is_ok() {
                        self.files.push(link);
                    }
                }
            }
            // Symlink to an existing file.
            89..=92 => {
                if let Some(target) = self.files.choose(&mut self.rng).cloned() {
                    let dir = self.dirs.choose(&mut self.rng).unwrap().clone();
                    let name = self.fresh_name("sl");
                    let _ = std::os::unix::fs::symlink(&target, dir.join(name));
                }
            }
            // Rename a directory (subtree move) — structural, refresh after.
            93..=96 => {
                if self.dirs.len() > 2 {
                    let i = self.rng.gen_range(1..self.dirs.len());
                    let from = self.dirs[i].clone();
                    let dest_parent = self.dirs.choose(&mut self.rng).unwrap().clone();
                    // Guard against moving a dir into itself or its own subtree.
                    if !dest_parent.starts_with(&from) {
                        let name = self.fresh_name("md");
                        let to = dest_parent.join(name);
                        if fs::rename(&from, &to).is_ok() {
                            self.refresh()?;
                        }
                    }
                }
            }
            // Remove a whole subtree — structural, refresh after.
            _ => {
                if self.dirs.len() > 2 {
                    let i = self.rng.gen_range(1..self.dirs.len());
                    let victim = self.dirs[i].clone();
                    if fs::remove_dir_all(&victim).is_ok() {
                        self.refresh()?;
                    }
                }
            }
        }
        self.ops_applied += 1;
        Ok(())
    }
}

/// The convergence oracle: exact set equality between filesystem truth and the
/// catalog's view of `root` (kind, ino, and size-for-files per path).
pub fn converged(cat: &Catalog, root: &Path) -> Result<(), String> {
    let fs_map = fs_listing(root).map_err(|e| format!("walk failed: {e}"))?;
    let root_str = root.to_string_lossy().into_owned();
    let prefix = format!("{}/", root_str.trim_end_matches('/'));
    let cat_map: BTreeMap<String, FsNode> = cat
        .listing()
        .map_err(|e| format!("catalog listing failed: {e}"))?
        .into_iter()
        .filter(|(p, _)| p.starts_with(&prefix))
        .map(|(p, s)| {
            (
                p,
                FsNode {
                    kind: s.kind,
                    ino: s.ino,
                    size: if s.kind == ObjectKind::File {
                        s.size
                    } else {
                        0
                    },
                },
            )
        })
        .collect();

    if fs_map == cat_map {
        return Ok(());
    }
    // Build a readable diff for failure reports.
    let mut report = String::new();
    for (p, n) in &fs_map {
        match cat_map.get(p) {
            None => report.push_str(&format!("missing from catalog: {p} ({n:?})\n")),
            Some(c) if c != n => {
                report.push_str(&format!("mismatch at {p}: fs={n:?} catalog={c:?}\n"))
            }
            _ => {}
        }
    }
    for p in cat_map.keys() {
        if !fs_map.contains_key(p) {
            report.push_str(&format!("stale in catalog: {p}\n"));
        }
    }
    Err(report)
}

/// Panic with a diff if not converged.
pub fn assert_converged(cat: &Catalog, root: &Path) {
    if let Err(diff) = converged(cat, root) {
        panic!("catalog diverged from filesystem:\n{diff}");
    }
}
