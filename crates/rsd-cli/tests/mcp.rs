use rsd_catalog::{Catalog, Change, FileId, ObjectKind, StatInfo};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn mcp_scope_is_enforced_and_requested_limits_are_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path();
    let catalog = Catalog::open(&state.join("catalog.redb")).unwrap();
    let mut changes: Vec<Change> = (1..=1_005u64)
        .map(|ino| Change::Upsert {
            path: format!("/allowed/{ino}.txt"),
            stat: stat(ino),
        })
        .collect();
    changes.push(Change::Upsert {
        path: "/private/secret.txt".into(),
        stat: stat(2_000),
    });
    catalog.apply_changes_direct(&changes).unwrap();
    drop(catalog);

    let mut child = Command::new(env!("CARGO_BIN_EXE_rsd-mcp"))
        .args(["--state", state.to_str().unwrap(), "--scope", "/allowed"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.as_mut().unwrap(),
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"rsd_search","arguments":{{"kind":"rql","query":"kMDItemFSSize > 0","limit":999999}}}}}}"#
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "{:?}", output);
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1_000);
    assert!(lines.iter().all(|path| path.starts_with("/allowed/")));
    assert!(!text.contains("/private/"));
}

fn stat(ino: u64) -> StatInfo {
    StatInfo {
        kind: ObjectKind::File,
        file_id: FileId { dev: 1, ino },
        size: 1,
        mtime_ns: 1,
        birthtime_ns: ino as i64,
        nlink: 1,
    }
}
