//! The client binaries against a *running* daemon.
//!
//! This is the case the shipped CLI and agent surface are actually used in, and
//! the one nothing covered: the catalog is a single-writer store, so a client
//! that opens it directly fails for as long as the daemon is up. Every test
//! here holds the daemon's catalog open for the whole run — if a client ever
//! reverts to opening the store, these fail with a lock error rather than
//! silently working in CI and breaking in the field.

use rsd_caes::Store;
use rsd_catalog::{Catalog, Durability};
use rsd_daemon::ipc::{start_ipc, AuthzStore, IpcCtx};
use rsd_daemon::{bring_up, ContentIndexer, ContentSource, PipelineConfig};
use rsd_extract::{extract_bytes, Budgets, ExtractHints};
use rsd_ingest::CoalescerConfig;
use rsd_lexical::LexicalPlane;
use rsd_live::LiveEngine;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Src;
impl ContentSource for Src {
    fn extract_file(
        &mut self,
        file: &std::fs::File,
        _path: &Path,
        hints: &ExtractHints,
        budgets: &Budgets,
    ) -> Result<rsd_caes::ExtractionRecord, String> {
        let mut file = file.try_clone().map_err(|e| e.to_string())?;
        std::io::Seek::rewind(&mut file).map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes).map_err(|e| e.to_string())?;
        Ok(extract_bytes(hints, budgets, &bytes))
    }
}

struct Daemon {
    _tmp: tempfile::TempDir,
    state: PathBuf,
    root: PathBuf,
    /// Held open for the lifetime of the test: this is the exclusive lock a
    /// client must not need.
    _catalog: Arc<Catalog>,
    _pipeline: rsd_daemon::Pipeline,
}

/// Stand up a daemon over a small corpus, using the real on-disk layout the
/// clients look for (`rsd.sock`, `http.token`, `catalog.redb`, …).
fn daemon() -> Daemon {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().canonicalize().unwrap();
    let root = state.join("tree");
    std::fs::create_dir_all(root.join("public")).unwrap();
    std::fs::create_dir_all(root.join("private")).unwrap();
    std::fs::write(
        root.join("public/invoice.txt"),
        "Quarterly invoice for Acme Corp. Total due 4200 dollars.",
    )
    .unwrap();
    std::fs::write(
        root.join("private/secrets.txt"),
        "Quarterly invoice ledger, confidential internal copy.",
    )
    .unwrap();

    let catalog = Arc::new(
        Catalog::open_with_durability(&state.join("catalog.redb"), Durability::None).unwrap(),
    );
    let caes = Arc::new(Store::open(&state.join("caes.redb")).unwrap());
    let indexer = ContentIndexer::new(Box::new(Src), caes.clone());
    let plane = LexicalPlane::open(&state.join("lexical")).unwrap();
    // HashEmbedder, not the learned model: hybrid needs *a* vector plane, and
    // this one is deterministic and needs no downloaded weights.
    let vector = Arc::new(Mutex::new(
        rsd_vector::VectorPlane::open(
            &state.join("vector.redb"),
            Arc::new(rsd_vector::HashEmbedder::default()),
        )
        .unwrap(),
    ));
    let live = Arc::new(Mutex::new(LiveEngine::new(Some(caes.clone()))));
    let (pipeline, _) = bring_up(
        catalog.clone(),
        &state.join("journal"),
        &root,
        Some(indexer),
        Some((plane, caes.clone())),
        Some((vector.clone(), caes.clone())),
        Some(live.clone()),
        PipelineConfig {
            coalescer: CoalescerConfig {
                quiet: Duration::from_millis(100),
                max_delay: Duration::from_secs(1),
                max_pending: 65_536,
            },
            fsevents_latency: Duration::from_millis(50),
            journal_sync: false,
            ..Default::default()
        },
    )
    .unwrap();

    let token = rsd_daemon::http::gen_token().unwrap();
    rsd_daemon::http::write_token(&state.join("http.token"), &token).unwrap();
    start_ipc(
        &state.join("rsd.sock"),
        IpcCtx {
            catalog: catalog.clone(),
            lexical_dir: state.join("lexical"),
            vector: Some(vector),
            live,
            authz: Arc::new(AuthzStore::default()),
            caes: Some(caes),
            first_party_token: Some(token),
        },
    )
    .unwrap();

    wait_for_content(&catalog, 2);
    Daemon {
        _tmp: tmp,
        state,
        root,
        _catalog: catalog,
        _pipeline: pipeline,
    }
}

fn wait_for_content(catalog: &Catalog, want: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let indexed = catalog
            .listing()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|(path, _)| path.ends_with(".txt"))
                    .count()
            })
            .unwrap_or(0);
        if indexed >= want {
            // Content lands a commit after the catalog entry; give it a beat.
            std::thread::sleep(Duration::from_millis(400));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("corpus never finished indexing");
}

fn rsdfind(state: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rsdfind"))
        .args(["--state", state.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap()
}

/// Drive rsd-mcp over stdio and return the text of one tool call.
fn mcp_tool(state: &Path, scope: &[&str], tool: &str, arguments: serde_json::Value) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rsd-mcp"));
    cmd.args(["--state", state.to_str().unwrap()]);
    if scope.is_empty() {
        cmd.arg("--unrestricted");
    } else {
        for path in scope {
            cmd.args(["--scope", path]);
        }
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": tool, "arguments": arguments},
    });
    writeln!(stdin, "{request}").unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    let stdout = child.stdout.take().unwrap();
    let line = BufReader::new(stdout)
        .lines()
        .next()
        .unwrap_or_else(|| {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read as _;
                let _ = e.read_to_string(&mut err);
            }
            panic!("rsd-mcp produced no output; stderr: {err}");
        })
        .unwrap();
    let _ = child.wait();
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(
        value["result"]["isError"].as_bool() != Some(true),
        "tool call failed: {value}"
    );
    value["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn rsdfind_queries_a_running_daemon() {
    let d = daemon();
    let out = rsdfind(&d.state, &[r#""invoice""#]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.status.success(),
        "rsdfind failed against a live daemon: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("invoice.txt"),
        "expected the indexed file, got {stdout:?}"
    );
    // It must have gone over IPC — the fallback would have hit the lock.
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        !stderr.contains("reading the state dir directly"),
        "fell back to the direct path while the daemon was up: {stderr}"
    );
}

#[test]
fn rsdfind_counts_over_ipc() {
    let d = daemon();
    let out = rsdfind(&d.state, &["-count", r#""invoice""#]);
    assert!(out.status.success(), "{:?}", out);
    let count: u64 = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(count, 2, "both corpus files mention invoice");
}

#[test]
fn rsdfind_hybrid_takes_natural_language() {
    let d = daemon();
    // The pre-IPC CLI parsed this as RQL and died with "trailing input".
    let out = rsdfind(&d.state, &["--hybrid", "quarterly invoice"]);
    assert!(
        out.status.success(),
        "hybrid rejected a natural-language query: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8(out.stdout).unwrap().contains(".txt"));
}

#[test]
fn rsdfind_falls_back_to_the_store_when_no_daemon_is_running() {
    // No daemon: the direct path still serves a stopped index.
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path();
    let catalog = Catalog::open(&state.join("catalog.redb")).unwrap();
    catalog
        .apply_changes_direct(&[rsd_catalog::Change::Upsert {
            path: "/virtual/one.txt".into(),
            stat: rsd_catalog::StatInfo {
                kind: rsd_catalog::ObjectKind::File,
                file_id: rsd_catalog::FileId { dev: 1, ino: 1 },
                size: 1,
                mtime_ns: 1,
                birthtime_ns: 1,
                nlink: 1,
            },
        }])
        .unwrap();
    drop(catalog);

    let out = rsdfind(state, &["-count", "kMDItemFSSize > 0"]);
    assert!(out.status.success(), "{:?}", out);
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "1");
    assert!(String::from_utf8(out.stderr)
        .unwrap()
        .contains("reading the state dir directly"));
}

#[test]
fn mcp_serves_an_agent_while_the_daemon_holds_the_store() {
    let d = daemon();
    let text = mcp_tool(
        &d.state,
        &[],
        "rsd_search",
        serde_json::json!({"query": "invoice", "kind": "hybrid"}),
    );
    assert!(
        text.contains("invoice.txt"),
        "expected a hit from the live daemon, got {text:?}"
    );
}

#[test]
fn mcp_snippets_are_grounded_in_extracted_text() {
    let d = daemon();
    let text = mcp_tool(
        &d.state,
        &[],
        "rsd_snippets",
        serde_json::json!({"query": "invoice"}),
    );
    assert!(text.contains("bytes "), "no byte range in {text:?}");
    assert!(
        text.to_lowercase().contains("invoice"),
        "excerpt does not contain the term: {text:?}"
    );
}

#[test]
fn mcp_scope_is_enforced_by_the_daemon_not_the_client() {
    let d = daemon();
    let public = d.root.join("public");
    let text = mcp_tool(
        &d.state,
        &[public.to_str().unwrap()],
        "rsd_search",
        serde_json::json!({"query": "invoice", "kind": "hybrid"}),
    );
    assert!(
        text.contains("invoice.txt"),
        "granted root should be visible: {text:?}"
    );
    assert!(
        !text.contains("secrets.txt"),
        "scope leak: ungranted file reached the agent: {text:?}"
    );

    // Same restriction on the RQL path, which passes the agent's string through.
    let text = mcp_tool(
        &d.state,
        &[public.to_str().unwrap()],
        "rsd_search",
        serde_json::json!({"query": r#""invoice""#, "kind": "rql"}),
    );
    assert!(
        !text.contains("secrets.txt"),
        "scope leak on the rql path: {text:?}"
    );
}

#[test]
fn an_unauthenticated_client_gets_nothing() {
    let d = daemon();
    // A client that presents no token and names a principal with no grant is
    // deny-by-default, even though it is same-uid and the socket is reachable.
    use rsd_ipc::{recv, send, Request, Response};
    let mut stream = std::os::unix::net::UnixStream::connect(d.state.join("rsd.sock")).unwrap();
    send(
        &mut stream,
        &Request::Hello {
            principal: "stranger".into(),
            token: None,
            restrict_to: None,
        },
    )
    .unwrap();
    let Response::Hello { .. } = recv::<Response>(&mut stream).unwrap() else {
        panic!("no hello");
    };
    send(
        &mut stream,
        &Request::Hybrid {
            query: "invoice".into(),
            scope: None,
            limit: 10,
        },
    )
    .unwrap();
    match recv::<Response>(&mut stream).unwrap() {
        Response::Hits(hits) => assert!(hits.is_empty(), "unauthenticated leak: {hits:?}"),
        Response::Err(_) => {}
        other => panic!("unexpected {other:?}"),
    }

    // And a wrong token is not a shortcut to first-party authority.
    let mut stream = std::os::unix::net::UnixStream::connect(d.state.join("rsd.sock")).unwrap();
    send(
        &mut stream,
        &Request::Hello {
            principal: "stranger".into(),
            token: Some("f".repeat(32)),
            restrict_to: None,
        },
    )
    .unwrap();
    let _ = recv::<Response>(&mut stream).unwrap();
    send(
        &mut stream,
        &Request::Hybrid {
            query: "invoice".into(),
            scope: None,
            limit: 10,
        },
    )
    .unwrap();
    match recv::<Response>(&mut stream).unwrap() {
        Response::Hits(hits) => assert!(hits.is_empty(), "forged-token leak: {hits:?}"),
        Response::Err(_) => {}
        other => panic!("unexpected {other:?}"),
    }
}
