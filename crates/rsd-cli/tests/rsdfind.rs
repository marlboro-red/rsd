use rsd_catalog::{Catalog, Change, FileId, ObjectKind, StatInfo};

#[test]
fn count_is_exact_above_the_ranked_hit_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path();
    let catalog = Catalog::open(&state.join("catalog.redb")).unwrap();
    let changes: Vec<Change> = (1..=10_025u64)
        .map(|ino| Change::Upsert {
            path: format!("/virtual/{ino}.txt"),
            stat: StatInfo {
                kind: ObjectKind::File,
                file_id: FileId { dev: 1, ino },
                size: 1,
                mtime_ns: 1,
                birthtime_ns: ino as i64,
                nlink: 1,
            },
        })
        .collect();
    catalog.apply_changes_direct(&changes).unwrap();
    drop(catalog);

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rsdfind"))
        .args([
            "--state",
            state.to_str().unwrap(),
            "-count",
            "kMDItemFSSize > 0",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "10025");
}
