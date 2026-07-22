//! rsd-mcp scope enforcement and result bounding, against a running daemon.
//!
//! The catalog is populated directly and then held open by this test for the
//! duration — the same single-writer lock the daemon holds in production. A
//! client that opened the store itself could not pass this.

use rsd_catalog::{Catalog, Change, Durability, FileId, ObjectKind, StatInfo};
use rsd_daemon::ipc::{start_ipc, AuthzStore, IpcCtx};
use rsd_live::LiveEngine;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

#[test]
fn mcp_scope_is_enforced_and_requested_limits_are_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path();
    let catalog = Arc::new(
        Catalog::open_with_durability(&state.join("catalog.redb"), Durability::None).unwrap(),
    );
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

    let token = rsd_daemon::http::gen_token().unwrap();
    rsd_daemon::http::write_token(&state.join("http.token"), &token).unwrap();
    start_ipc(
        &state.join("rsd.sock"),
        IpcCtx {
            catalog: catalog.clone(),
            lexical_dir: state.join("lexical"),
            vector: None,
            live: Arc::new(Mutex::new(LiveEngine::new(None))),
            authz: Arc::new(AuthzStore::default()),
            caes: None,
            first_party_token: Some(token),
        },
    )
    .unwrap();

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
    assert_eq!(lines.len(), 1_000, "requested limit was not bounded");
    assert!(lines.iter().all(|path| path.starts_with("/allowed/")));
    assert!(!text.contains("/private/"), "scope leak");

    // The lock the client never needed.
    assert!(catalog.entry_count().unwrap() > 1_000);
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
