//! rsdfind: mdfind-flag-compatible query CLI.
//!
//!   rsdfind --state <dir> [-onlyin dir] [-count] [-name pat] [-0] [--offline] [query]
//!
//! Transport is chosen automatically. If a daemon is listening on
//! `<state>/rsd.sock`, queries go to it; the catalog is a single-writer store,
//! so a client *cannot* open it while the daemon holds it, and IPC is the only
//! way one-shot search works on a live index. With no daemon listening, the
//! state dir is opened directly, which is how a stopped index is inspected.
//!
//! `--offline` forces the direct path and fails rather than falling back, for
//! scripts that need to know which one they got.

use rsd_ipc::{recv, send, Request, Response};
use std::io::Write;

fn usage() -> ! {
    eprintln!(
        "usage: rsdfind --state <dir> [-onlyin <dir>] [-count] [-name <pattern>] [-0]\n\
         \x20              [--semantic | --hybrid] [-live] [--threshold <f>] [--explain]\n\
         \x20              [--offline] [query]"
    );
    std::process::exit(2);
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("rsdfind: {msg}");
    std::process::exit(1);
}

/// The loopback secret proving first-party authority to the daemon. Absent when
/// the daemon never started or the file is unreadable; the daemon then applies
/// its deny-by-default named-grant table.
fn first_party_token(state: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(state.join("http.token"))
        .ok()
        .map(|t| t.trim().to_string())
}

/// Connect and complete the Hello handshake. `None` means no daemon is
/// listening — the caller decides whether that is fatal or a fallback.
fn try_connect(state: &std::path::Path) -> Option<std::os::unix::net::UnixStream> {
    let mut stream = std::os::unix::net::UnixStream::connect(state.join("rsd.sock")).ok()?;
    send(
        &mut stream,
        &Request::Hello {
            principal: "rsdfind".into(),
            token: first_party_token(state),
            restrict_to: None,
        },
    )
    .ok()?;
    // A handshake failure against a daemon that IS listening is a real error,
    // not a reason to silently fall back to a store it has locked anyway.
    match recv::<Response>(&mut stream) {
        Ok(Response::Hello { .. }) => Some(stream),
        Ok(Response::Err(e)) => die(e),
        Ok(_) => die("unexpected response to Hello"),
        Err(e) => die(e),
    }
}

fn connect_or_die(state: &std::path::Path) -> std::os::unix::net::UnixStream {
    try_connect(state).unwrap_or_else(|| {
        die(format!(
            "no daemon listening at {}\n\
             start one with `rsd-daemon watch <root> --state {}`",
            state.join("rsd.sock").display(),
            state.display()
        ))
    })
}

fn print_paths(paths: impl IntoIterator<Item = String>, nul: bool) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for path in paths {
        let _ = if nul {
            write!(out, "{path}\0")
        } else {
            writeln!(out, "{path}")
        };
    }
}

/// -live: subscribe over the daemon's IPC socket and stream deltas.
fn run_live(state: &std::path::Path, rql: &str) -> ! {
    let mut stream = connect_or_die(state);
    send(&mut stream, &Request::Subscribe { rql: rql.into() }).unwrap_or_else(|e| die(e));
    let stdout = std::io::stdout();
    loop {
        match recv::<Response>(&mut stream) {
            Ok(Response::Subscribed(hits)) => {
                let mut out = stdout.lock();
                for h in hits {
                    let _ = writeln!(out, "{}", h.path);
                }
                let _ = out.flush();
            }
            Ok(Response::Event { enter, path, .. }) => {
                let mut out = stdout.lock();
                let _ = writeln!(out, "{} {path}", if enter { "+" } else { "-" });
                let _ = out.flush();
            }
            Ok(Response::Resync) => {
                let mut out = stdout.lock();
                let _ = writeln!(out, "! resync");
                let _ = out.flush();
            }
            Ok(Response::Err(e)) => die(e),
            Ok(_) => {}
            Err(_) => std::process::exit(0),
        }
    }
}

/// -live --semantic: a standing semantic alert (threshold classification).
fn run_alert(state: &std::path::Path, query: &str, threshold: f32) -> ! {
    let mut stream = connect_or_die(state);
    send(
        &mut stream,
        &Request::SubscribeAlert {
            query: query.into(),
            threshold,
        },
    )
    .unwrap_or_else(|e| die(e));
    let stdout = std::io::stdout();
    loop {
        match recv::<Response>(&mut stream) {
            Ok(Response::Event {
                enter: true, path, ..
            }) => {
                let mut out = stdout.lock();
                let _ = writeln!(out, "! {path}");
                let _ = out.flush();
            }
            Ok(Response::Err(e)) => die(e),
            Ok(_) => {}
            Err(_) => std::process::exit(0),
        }
    }
}

/// One-shot search against the running daemon.
fn run_daemon_query(
    mut stream: std::os::unix::net::UnixStream,
    request: Request,
    count: bool,
    nul: bool,
) -> ! {
    send(&mut stream, &request).unwrap_or_else(|e| die(e));
    match recv::<Response>(&mut stream) {
        Ok(Response::Hits(hits)) => {
            if count {
                println!("{}", hits.len());
            } else {
                print_paths(hits.into_iter().map(|h| h.path), nul);
            }
        }
        Ok(Response::Count(total)) => println!("{total}"),
        Ok(Response::Err(e)) => die(e),
        Ok(_) => die("unexpected response"),
        Err(e) => die(e),
    }
    std::process::exit(0)
}

fn rsd_ml_or_hash() -> std::sync::Arc<dyn rsd_vector::Embedder> {
    match rsd_ml::MiniLmEmbedder::load(&rsd_ml::MiniLmEmbedder::default_dir()) {
        Ok(m) => std::sync::Arc::new(m),
        Err(_) => std::sync::Arc::new(rsd_vector::HashEmbedder::default()),
    }
}

/// Everything the two transports need to answer one query, resolved from argv.
struct Query {
    /// RQL source, empty when `hybrid` supplies the retrieval instead.
    src: String,
    /// Raw query text, used by --hybrid and --semantic.
    text: String,
    onlyin: Option<String>,
    hybrid: bool,
    count: bool,
    nul: bool,
    explain: bool,
    limit: usize,
}

/// Direct-store path for a stopped index. Takes the store's exclusive lock, so
/// it fails loudly if a daemon is running.
fn run_offline(state: &std::path::Path, q: &Query) -> ! {
    use rsd_catalog::{Catalog, Durability};
    use rsd_lexical::LexicalReader;
    use rsd_query::{parse, QueryEngine};

    let catalog = Catalog::open_with_durability(&state.join("catalog.redb"), Durability::Eventual)
        .unwrap_or_else(|e| {
            die(format!(
                "cannot open catalog: {e}\n\
                 the direct path needs exclusive access; stop the daemon, or drop \
                 --offline to query it over its socket"
            ))
        });
    let lexical = LexicalReader::open(&state.join("lexical")).ok();
    let vector = rsd_vector::VectorPlane::open(&state.join("vector.redb"), rsd_ml_or_hash()).ok();
    let engine = QueryEngine {
        catalog: &catalog,
        lexical: lexical.as_ref(),
        vector: vector.as_ref(),
        limit: q.limit,
    };

    if q.hybrid {
        let hits = engine.hybrid(&q.text, q.limit).unwrap_or_else(|e| die(e));
        if q.count {
            println!("{}", hits.len());
        } else {
            print_paths(hits.into_iter().map(|h| h.path), q.nul);
        }
        std::process::exit(0);
    }

    let expr = parse(&q.src).unwrap_or_else(|e| die(e));
    if q.explain {
        eprintln!("plan: {:?}", engine.plan(&expr));
    }
    let onlyin = q.onlyin.as_deref();
    if q.count {
        println!("{}", engine.count(&expr, onlyin).unwrap_or_else(|e| die(e)));
        std::process::exit(0);
    }
    let hits = engine.run(&expr, onlyin).unwrap_or_else(|e| die(e));
    print_paths(hits.into_iter().map(|h| h.path), q.nul);
    std::process::exit(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut state = None;
    let mut onlyin: Option<String> = None;
    let mut count = false;
    let mut name_pat: Option<String> = None;
    let mut nul = false;
    let mut explain = false;
    let mut live = false;
    let mut semantic = false;
    let mut hybrid = false;
    let mut offline = false;
    let mut threshold = 0.35f32;
    let mut limit = 100usize;
    let mut query_parts: Vec<String> = Vec::new();

    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--state" => state = it.next(),
            "-onlyin" => onlyin = it.next(),
            "-count" => count = true,
            "-name" => name_pat = it.next(),
            "-0" => nul = true,
            "-live" => live = true,
            "--semantic" => semantic = true,
            "--threshold" => {
                threshold = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--limit" => {
                limit = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--hybrid" => hybrid = true,
            "--explain" => explain = true,
            "--offline" => offline = true,
            _ => query_parts.push(a),
        }
    }
    let Some(state) = state else { usage() };
    let state = std::path::PathBuf::from(state);
    let text = query_parts.join(" ");

    // Build the RQL source: -name maps to a kMDItemFSName glob; bare args are
    // a content search; both AND together (mdfind semantics). Hybrid is not
    // expressible as RQL — it is its own retrieval mode — so it skips this.
    let mut clauses: Vec<String> = Vec::new();
    if let Some(pat) = &name_pat {
        let pat = if pat.contains('*') {
            pat.clone()
        } else {
            format!("*{pat}*")
        };
        clauses.push(format!(r#"kMDItemFSName == "{pat}"c"#));
    }
    if !text.is_empty() {
        if semantic {
            clauses.push(format!(r#"semantic("{text}")"#));
        } else {
            // Bare args are RQL (`"phrase"`, `kMDItemFSSize > 0`, …). Natural
            // language belongs to --semantic and --hybrid.
            clauses.push(text.clone());
        }
    }
    // Hybrid needs only the raw text; every other mode needs an RQL clause.
    let usable = if hybrid {
        !text.is_empty()
    } else {
        !clauses.is_empty()
    };
    if !usable {
        usage();
    }
    let src = clauses.join(" && ");

    if live {
        if hybrid {
            die("-live does not support --hybrid (ranked views are not a live class)");
        }
        if semantic {
            run_alert(&state, &text, threshold);
        }
        run_live(&state, &src);
    }

    let q = Query {
        src,
        text,
        onlyin,
        hybrid,
        count,
        nul,
        explain,
        limit,
    };

    // Prefer the daemon; fall back to the store only when nothing is listening.
    let stream = if offline { None } else { try_connect(&state) };
    let Some(stream) = stream else {
        if !offline {
            eprintln!(
                "rsdfind: no daemon at {}; reading the state dir directly",
                state.join("rsd.sock").display()
            );
        }
        run_offline(&state, &q);
    };

    if q.explain {
        eprintln!("plan: served by the daemon; use --offline --explain for a local plan");
    }
    let request = if q.hybrid {
        Request::Hybrid {
            query: q.text,
            scope: q.onlyin,
            limit: q.limit as u32,
        }
    } else {
        Request::Query {
            rql: q.src,
            scope: q.onlyin,
            count_only: q.count,
        }
    };
    run_daemon_query(stream, request, q.count, q.nul);
}
