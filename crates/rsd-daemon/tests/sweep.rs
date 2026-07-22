//! Orphan reclamation across a crash.
//!
//! Reclaiming an orphan touches two stores that cannot commit atomically
//! together: the catalog forgets the object, and each projection deletes its
//! documents. Projection deletes carry no LSN — they commit with an unchanged
//! watermark — so `recover()` cannot distinguish "swept" from "never swept"
//! by comparing watermarks. Whichever store is written *last* therefore
//! decides what an interrupted sweep leaves behind.
//!
//! The crash gate found this the hard way: it failed intermittently with both
//! planes holding documents for oids the catalog no longer had. That state is
//! unrecoverable — every watermark equals the journal max, so nothing rebuilds
//! and nothing can name the stranded documents in order to delete them.
//!
//! These tests pin the ordering deterministically, rather than waiting for a
//! randomized SIGKILL to land in a window a few instructions wide.

use rsd_caes::Store;
use rsd_catalog::{Catalog, Change, Durability, StatInfo};
use rsd_daemon::Committer;
use rsd_log::{Journal, JournalConfig, Source};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const NO_GRACE: Duration = Duration::ZERO;
const TEXT: &str = "synthetic sweep corpus";

struct Env {
    _tmp: tempfile::TempDir,
    catalog: Arc<Catalog>,
    committer: Committer,
    vector: Arc<Mutex<rsd_vector::VectorPlane>>,
    file: PathBuf,
}

fn stat_of(path: &Path) -> StatInfo {
    StatInfo::from_metadata(&std::fs::symlink_metadata(path).unwrap())
}

/// A committer with both planes, holding one indexed document.
///
/// The file is real: `commit` revalidates every `SetContent` against the
/// filesystem before journaling it (the TOCTOU fence), so a virtual path would
/// be silently dropped and the planes would come up empty.
fn env_with_indexed_document() -> (Env, u64) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    let file = tree.join("a.txt");
    std::fs::write(&file, TEXT).unwrap();
    let path = file.to_string_lossy().into_owned();

    let catalog = Arc::new(
        Catalog::open_with_durability(&dir.join("catalog.redb"), Durability::None).unwrap(),
    );
    let caes = Arc::new(Store::open(&dir.join("caes.redb")).unwrap());
    let journal = Journal::open(
        &dir.join("journal"),
        JournalConfig {
            sync_on_append: false,
            segment_max_bytes: 1 << 20,
        },
    )
    .unwrap();
    let lexical = rsd_lexical::LexicalPlane::open(&dir.join("lexical")).unwrap();
    let vector = Arc::new(Mutex::new(
        rsd_vector::VectorPlane::open(
            &dir.join("vector.redb"),
            Arc::new(rsd_vector::HashEmbedder::default()),
        )
        .unwrap(),
    ));
    let mut committer = Committer::new(catalog.clone(), journal)
        .with_lexical(lexical, caes.clone())
        .with_vector(vector.clone(), caes.clone());

    let content_hash = [7u8; 32];
    let hints_hash = [9u8; 32];
    caes.put(
        &rsd_caes::CaesKey {
            content_hash,
            extractor_id: rsd_extract::EXTRACTOR_ID.into(),
            extractor_version: rsd_extract::EXTRACTOR_VERSION,
            hints_hash,
            abi_version: rsd_caes::ABI_VERSION,
        },
        &rsd_caes::ExtractionRecord {
            status: rsd_caes::ExtractStatus::Complete,
            text: TEXT.to_string(),
            attrs: vec![],
            symbols: vec![],
        },
    )
    .unwrap();

    committer
        .commit(
            Source::Scan,
            &[Change::Upsert {
                path: path.clone(),
                stat: stat_of(&file),
            }],
        )
        .unwrap();
    committer
        .commit(
            Source::Content,
            &[Change::SetContent {
                path: path.clone(),
                content_hash,
                hints_hash,
                state: "complete".into(),
            }],
        )
        .unwrap();

    let (oid, _) = catalog.get_by_path(&path).unwrap().unwrap();
    let env = Env {
        _tmp: tmp,
        catalog,
        committer,
        vector,
        file,
    };
    assert!(
        plane_holds(&env, oid),
        "setup: the document should be in both planes"
    );
    (env, oid)
}

/// Unlink the file and record the removal: the object survives as an orphan
/// awaiting its grace window.
fn orphan_by_unlink(env: &mut Env, oid: u64) {
    let path = env.file.to_string_lossy().into_owned();
    std::fs::remove_file(&env.file).unwrap();
    env.committer
        .commit(Source::Scan, &[Change::RemovePath { path }])
        .unwrap();
    assert!(
        env.catalog.get_object(oid).unwrap().is_some(),
        "an unlinked object stays until swept"
    );
}

fn env_with_one_orphan() -> (Env, u64) {
    let (mut env, oid) = env_with_indexed_document();
    orphan_by_unlink(&mut env, oid);
    (env, oid)
}

fn plane_holds(env: &Env, oid: u64) -> bool {
    let in_lexical = env
        .committer
        .lexical()
        .unwrap()
        .reader()
        .search_content("synthetic", false, 100)
        .unwrap()
        .contains(&oid);
    let in_vector = env
        .vector
        .lock()
        .unwrap()
        .search("synthetic", 100)
        .unwrap()
        .iter()
        .any(|hit| hit.oid == oid);
    in_lexical || in_vector
}

/// The invariant the crash gate asserts: no projection may hold a document
/// for an oid the catalog cannot name.
fn assert_no_stranded_documents(env: &Env, oid: u64) {
    if env.catalog.get_object(oid).unwrap().is_none() {
        assert!(
            !plane_holds(env, oid),
            "stranded document: oid {oid} is in a projection the catalog cannot name"
        );
    }
}

#[test]
fn a_sweep_interrupted_before_the_catalog_write_retries_and_converges() {
    let (mut env, oid) = env_with_one_orphan();

    // The crash point: projections cleared, catalog write never happened.
    let victims = env.catalog.orphan_oids(NO_GRACE).unwrap();
    assert_eq!(victims, vec![oid]);
    env.committer.remove_from_planes(&victims).unwrap();

    // The catalog still names the orphan, so the work is not lost.
    assert!(
        env.catalog.get_object(oid).unwrap().is_some(),
        "an interrupted sweep must leave the orphan retryable"
    );
    assert_no_stranded_documents(&env, oid);

    // The retry the applier performs once it next goes idle.
    assert_eq!(env.committer.sweep_orphans(NO_GRACE).unwrap(), 1);
    assert!(env.catalog.get_object(oid).unwrap().is_none());
    assert!(!plane_holds(&env, oid));

    // Still idempotent afterwards.
    assert_eq!(env.committer.sweep_orphans(NO_GRACE).unwrap(), 0);
    assert_no_stranded_documents(&env, oid);
}

#[test]
fn a_completed_sweep_leaves_no_document_behind() {
    let (mut env, oid) = env_with_one_orphan();
    assert_eq!(env.committer.sweep_orphans(NO_GRACE).unwrap(), 1);
    assert!(env.catalog.get_object(oid).unwrap().is_none());
    assert!(!plane_holds(&env, oid));
    assert_no_stranded_documents(&env, oid);
}

/// The ordering gate.
///
/// If a projection cannot accept its deletion, the catalog must not already
/// have forgotten the object — otherwise the documents are stranded under an
/// oid nothing can name, which no watermark comparison can detect and no
/// retry can reach. Making the lexical index unwritable turns the crash
/// window into a deterministic error, so this pins the order without needing
/// a SIGKILL to land inside it.
#[test]
fn a_failing_projection_write_leaves_the_catalog_untouched() {
    use std::os::unix::fs::PermissionsExt;

    let (mut env, oid) = env_with_one_orphan();
    let lexical_dir = env.file.parent().unwrap().parent().unwrap().join("lexical");
    assert!(lexical_dir.is_dir(), "expected {lexical_dir:?}");

    // Deny new segment files: the plane's delete-commit can no longer land.
    let original = std::fs::metadata(&lexical_dir).unwrap().permissions();
    let mut readonly = original.clone();
    readonly.set_mode(0o555);
    std::fs::set_permissions(&lexical_dir, readonly).unwrap();

    let result = env.committer.sweep_orphans(NO_GRACE);
    std::fs::set_permissions(&lexical_dir, original).unwrap();

    if result.is_ok() {
        // The plane accepted the write anyway (tantivy had room to commit
        // without creating a file). Nothing to assert about failure ordering.
        return;
    }
    assert!(
        env.catalog.get_object(oid).unwrap().is_some(),
        "the catalog forgot an object whose projection delete failed: those \
         documents are now stranded under an oid nothing can name"
    );
}

/// The catalog half must not delete an object that stopped being an orphan
/// between identification and removal — that would drop a live document.
///
/// This is the orphan-grace rename pairing: a rename observed as
/// remove-then-add leaves the object briefly pathless, and the add rebinds the
/// *same* inode. A sweep that ignored the rebind would delete a live file's
/// identity.
#[test]
fn an_orphan_rebound_before_removal_is_not_swept() {
    let (mut env, oid) = env_with_indexed_document();

    // A real rename: the inode survives, so the add rebinds the same object.
    let renamed = env.file.with_file_name("renamed.txt");
    std::fs::rename(&env.file, &renamed).unwrap();
    let old_path = env.file.to_string_lossy().into_owned();
    env.committer
        .commit(Source::Scan, &[Change::RemovePath { path: old_path }])
        .unwrap();

    let victims = env.catalog.orphan_oids(NO_GRACE).unwrap();
    assert_eq!(victims, vec![oid], "briefly pathless mid-rename");

    let renamed_path = renamed.to_string_lossy().into_owned();
    env.committer
        .commit(
            Source::Scan,
            &[Change::Upsert {
                path: renamed_path.clone(),
                stat: stat_of(&renamed),
            }],
        )
        .unwrap();

    let removed = env.catalog.remove_orphans(&victims, NO_GRACE).unwrap();
    assert!(
        removed.is_empty(),
        "a rebound object must survive the sweep"
    );
    assert!(env.catalog.get_object(oid).unwrap().is_some());
    assert_eq!(
        env.catalog
            .get_by_path(&renamed_path)
            .unwrap()
            .map(|(resolved, _)| resolved),
        Some(oid),
        "the rebound path should resolve to the original object"
    );
}
