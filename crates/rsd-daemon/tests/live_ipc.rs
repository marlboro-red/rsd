//! P5 end-to-end: live views over IPC, the leak suite, the incremental ==
//! from-scratch property, and notify latency.

use rsd_caes::Store;
use rsd_catalog::{Catalog, Durability};
use rsd_daemon::ipc::{start_ipc, AuthzStore, IpcCtx};
use rsd_daemon::{bring_up, ContentIndexer, ContentSource, PipelineConfig};
use rsd_extract::{extract_bytes, Budgets, ExtractHints};
use rsd_ingest::CoalescerConfig;
use rsd_ipc::{recv, send, Request, Response, Scope};
use rsd_lexical::{LexicalPlane, LexicalReader};
use rsd_live::{LiveEngine, LiveEvent};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Src(Arc<AtomicU64>);
impl ContentSource for Src {
    fn extract_file(
        &mut self,
        file: &std::fs::File,
        _path: &std::path::Path,
        hints: &ExtractHints,
        budgets: &Budgets,
    ) -> Result<rsd_caes::ExtractionRecord, String> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut file = file.try_clone().map_err(|error| error.to_string())?;
        std::io::Seek::rewind(&mut file).map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes).map_err(|error| error.to_string())?;
        Ok(extract_bytes(hints, budgets, &bytes))
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    cat: Arc<Catalog>,
    live: Arc<Mutex<LiveEngine>>,
    pipeline: rsd_daemon::Pipeline,
    sock: PathBuf,
}

fn fast_cfg() -> PipelineConfig {
    PipelineConfig {
        coalescer: CoalescerConfig {
            quiet: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
            max_pending: 65_536,
        },
        fsevents_latency: Duration::from_millis(50),
        journal_sync: false,
        ..Default::default()
    }
}

fn setup(authz: AuthzStore) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("tree");
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::write(root.join("a/alpha.log"), "alpha secret-a").unwrap();
    std::fs::write(root.join("b/beta.log"), "beta secret-b").unwrap();

    let cat =
        Arc::new(Catalog::open_with_durability(&base.join("cat.redb"), Durability::None).unwrap());
    let caes = Arc::new(Store::open(&base.join("caes.redb")).unwrap());
    let indexer = ContentIndexer::new(Box::new(Src(Arc::new(AtomicU64::new(0)))), caes.clone());
    let plane = LexicalPlane::open(&base.join("lexical")).unwrap();
    let live = Arc::new(Mutex::new(LiveEngine::new(Some(caes.clone()))));
    let (pipeline, _) = bring_up(
        cat.clone(),
        &base.join("journal"),
        &root,
        Some(indexer),
        Some((plane, caes)),
        None,
        Some(live.clone()),
        fast_cfg(),
    )
    .unwrap();
    let sock = base.join("rsd.sock");
    start_ipc(
        &sock,
        IpcCtx {
            catalog: cat.clone(),
            lexical_dir: base.join("lexical"),
            vector: None,
            live: live.clone(),
            authz: Arc::new(authz),
        },
    )
    .unwrap();
    Env {
        _tmp: tmp,
        base,
        root,
        cat,
        live,
        pipeline,
        sock,
    }
}

fn connect(env: &Env, principal: &str) -> UnixStream {
    let mut s = UnixStream::connect(&env.sock).unwrap();
    send(
        &mut s,
        &Request::Hello {
            principal: principal.into(),
        },
    )
    .unwrap();
    let Response::Hello { .. } = recv::<Response>(&mut s).unwrap() else {
        panic!("no hello");
    };
    s
}

fn expect_event(s: &mut UnixStream, deadline: Duration) -> Response {
    s.set_read_timeout(Some(deadline)).unwrap();
    recv::<Response>(s).expect("event within deadline")
}

#[test]
fn subscribe_streams_enters_and_leaves_over_ipc() {
    let mut authz = AuthzStore::default();
    authz.grant_unrestricted("test");
    let env = setup(authz);
    // Bootstrap is synchronous, but under parallel test load the watcher may
    // still be settling; fence on the seed files being cataloged.
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.cat.entry_count().unwrap() < 5 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut s = connect(&env, "test");
    send(
        &mut s,
        &Request::Subscribe {
            rql: r#"kMDItemFSName == "*.log""#.into(),
        },
    )
    .unwrap();
    let Response::Subscribed(initial) = recv::<Response>(&mut s).unwrap() else {
        panic!("expected Subscribed");
    };
    assert_eq!(initial.len(), 2, "{initial:?}");

    // Create → Enter.
    std::fs::write(env.root.join("a/gamma.log"), "gamma").unwrap();
    match expect_event(&mut s, Duration::from_secs(15)) {
        Response::Event {
            enter: true, path, ..
        } => assert!(path.ends_with("gamma.log")),
        other => panic!("expected Enter, got {other:?}"),
    }
    // Rename out of the predicate → Leave.
    std::fs::rename(env.root.join("a/gamma.log"), env.root.join("a/gamma.txt")).unwrap();
    match expect_event(&mut s, Duration::from_secs(15)) {
        Response::Event {
            enter: false, path, ..
        } => {
            // Rename yields two deltas (old-path removal, new-path upsert) in
            // nondeterministic order; the Leave may carry either binding.
            assert!(
                path.ends_with("gamma.log") || path.ends_with("gamma.txt"),
                "unexpected leave path {path}"
            );
        }
        other => panic!("expected Leave, got {other:?}"),
    }
    env.pipeline.stop();
}

#[test]
fn leak_suite_scoped_principal_sees_nothing_outside_grants() {
    let env = setup(AuthzStore::default());
    // Grants need the concrete root path, so bind a second listener whose
    // authz store scopes the "restricted" principal to <root>/a/ only.
    let mut a = AuthzStore::default();
    a.grant("restricted", vec![format!("{}/a", env.root.display())]);
    a.grant("revoked", vec![]);
    let sock2 = env.base.join("rsd2.sock");
    start_ipc(
        &sock2,
        IpcCtx {
            catalog: env.cat.clone(),
            lexical_dir: env.base.join("lexical"),
            vector: None,
            live: env.live.clone(),
            authz: Arc::new(a),
        },
    )
    .unwrap();
    let mut s = UnixStream::connect(&sock2).unwrap();
    send(
        &mut s,
        &Request::Hello {
            principal: "restricted".into(),
        },
    )
    .unwrap();
    let _: Response = recv(&mut s).unwrap();

    // Unknown principals are deny-all, not implicit first-party clients.
    let mut unknown = UnixStream::connect(&sock2).unwrap();
    send(
        &mut unknown,
        &Request::Hello {
            principal: "unknown".into(),
        },
    )
    .unwrap();
    let _: Response = recv(&mut unknown).unwrap();
    send(
        &mut unknown,
        &Request::Query {
            rql: r#"kMDItemFSName == "*.log""#.into(),
            scope: None,
            count_only: true,
        },
    )
    .unwrap();
    assert!(matches!(recv(&mut unknown).unwrap(), Response::Count(0)));

    // An explicit empty grant is also deny-all on the live path.
    let mut revoked = UnixStream::connect(&sock2).unwrap();
    send(
        &mut revoked,
        &Request::Hello {
            principal: "revoked".into(),
        },
    )
    .unwrap();
    let _: Response = recv(&mut revoked).unwrap();
    send(
        &mut revoked,
        &Request::Subscribe {
            rql: r#"kMDItemFSName == "*.log""#.into(),
        },
    )
    .unwrap();
    let Response::Subscribed(revoked_initial) = recv(&mut revoked).unwrap() else {
        panic!("expected revoked subscription acknowledgment")
    };
    assert!(revoked_initial.is_empty());

    // Counts computed over the authorized subset only.
    send(
        &mut s,
        &Request::Query {
            rql: r#"kMDItemFSName == "*.log""#.into(),
            scope: None,
            count_only: true,
        },
    )
    .unwrap();
    match recv::<Response>(&mut s).unwrap() {
        Response::Count(n) => assert_eq!(n, 1, "count leaked out-of-scope entries"),
        other => panic!("{other:?}"),
    }
    // Results contain no /b paths, even querying content that exists there.
    send(
        &mut s,
        &Request::Query {
            rql: "secret".into(),
            scope: None,
            count_only: false,
        },
    )
    .unwrap();
    match recv::<Response>(&mut s).unwrap() {
        Response::Hits(hits) => {
            assert!(hits.iter().all(|h| h.path.contains("/a/")), "{hits:?}");
        }
        other => panic!("{other:?}"),
    }
    // Live: out-of-scope creations produce NO events; in-scope ones do.
    send(
        &mut s,
        &Request::Subscribe {
            rql: r#"kMDItemFSName == "*.log""#.into(),
        },
    )
    .unwrap();
    let Response::Subscribed(initial) = recv::<Response>(&mut s).unwrap() else {
        panic!()
    };
    assert_eq!(initial.len(), 1);
    std::fs::write(env.root.join("b/covert.log"), "x").unwrap();
    std::fs::write(env.root.join("a/overt.log"), "y").unwrap();
    match expect_event(&mut s, Duration::from_secs(15)) {
        Response::Event {
            enter: true, path, ..
        } => {
            assert!(path.ends_with("overt.log"), "leaked event for {path}");
        }
        other => panic!("{other:?}"),
    }
    revoked
        .set_read_timeout(Some(Duration::from_millis(750)))
        .unwrap();
    assert!(
        recv::<Response>(&mut revoked).is_err(),
        "deny-all principal received a live event"
    );
    env.pipeline.stop();
}

#[test]
fn incremental_members_equal_fresh_query_after_storm() {
    let env = setup(AuthzStore::default());
    let expr = rsd_query::parse(r#"kMDItemFSSize > 10"#).unwrap();
    let (view_id, _rx) = env.live.lock().unwrap().subscribe(
        expr.clone(),
        Scope::Unrestricted,
        initial_oids(&env, &expr),
        4096,
    );

    // Storm: creates, rewrites across the threshold, deletes, renames.
    for i in 0..40 {
        std::fs::write(
            env.root.join(format!("a/s{i}.dat")),
            "x".repeat(5 + (i % 3) * 10),
        )
        .unwrap();
    }
    for i in 0..10 {
        std::fs::remove_file(env.root.join(format!("a/s{i}.dat"))).unwrap();
    }
    for i in 10..20 {
        std::fs::rename(
            env.root.join(format!("a/s{i}.dat")),
            env.root.join(format!("b/m{i}.dat")),
        )
        .unwrap();
    }
    // Quiesce, then compare incremental members against a from-scratch run.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let fresh: std::collections::HashSet<u64> = initial_oids(&env, &expr).into_iter().collect();
        let live = env.live.lock().unwrap();
        let inc = live.view_members(view_id).unwrap();
        if *inc == fresh {
            break;
        }
        if Instant::now() > deadline {
            panic!(
                "incremental != from-scratch: inc={} fresh={}",
                inc.len(),
                fresh.len()
            );
        }
    }
    env.pipeline.stop();
}

fn initial_oids(env: &Env, expr: &rsd_query::Expr) -> Vec<u64> {
    let reader = LexicalReader::open(&env.base.join("lexical")).unwrap();
    let engine = rsd_query::QueryEngine {
        catalog: &env.cat,
        lexical: Some(&reader),
        vector: None,
        limit: 100_000,
    };
    engine
        .run(expr, None)
        .unwrap()
        .into_iter()
        .map(|h| h.oid)
        .collect()
}

#[test]
fn notify_latency_p99_under_10ms() {
    // Engine-level: commit-to-receive latency of the delta fan-out itself
    // (the pipeline in front adds coalescer time by design, budgeted apart).
    let mut eng = LiveEngine::new(None);
    let expr = rsd_query::parse(r#"kMDItemFSSize > 0"#).unwrap();
    let (_, rx) = eng.subscribe(expr, Scope::Unrestricted, [], 8192);
    let mut lat = Vec::with_capacity(500);
    for i in 0..500u64 {
        let d = rsd_catalog::Delta {
            path: format!("/r/f{i}"),
            old: None,
            new: Some((
                i + 1,
                rsd_catalog::ObjectRecord {
                    kind: rsd_catalog::ObjectKind::File,
                    file_id: rsd_catalog::FileId { dev: 1, ino: i + 1 },
                    birthtime_ns: 1,
                    size: 10,
                    mtime_ns: 1,
                    nlink: 1,
                    entry_paths: vec![],
                    orphaned_at_ns: None,
                    content_hash: None,
                    index_state: None,
                    caes_hints_hash: None,
                },
            )),
        };
        let t = Instant::now();
        eng.on_commit(&[d]);
        let ev = rx.recv_timeout(Duration::from_millis(100)).unwrap();
        lat.push(t.elapsed());
        assert!(matches!(ev, LiveEvent::Enter { .. }));
    }
    lat.sort_unstable();
    let p99 = lat[lat.len() * 99 / 100];
    eprintln!("notify latency p99 = {p99:?}");
    assert!(p99 < Duration::from_millis(10), "p99 {p99:?} over budget");
}
