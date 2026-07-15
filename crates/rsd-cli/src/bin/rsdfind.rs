//! rsdfind (P4.3): mdfind-flag-compatible one-shot query CLI.
//!
//!   rsdfind --state <dir> [-onlyin dir] [-count] [-name pat] [-0] [--explain] [query]
//!
//! Phase-4 note: operates directly on a daemon state dir (read-only). The IPC
//! client path (live daemon, no state-dir locking caveats) is Phase 5; until
//! then, run against a quiesced state dir.

use rsd_catalog::{Catalog, Durability};
use rsd_lexical::LexicalReader;
use rsd_query::{parse, QueryEngine};
use std::io::Write;

fn usage() -> ! {
    eprintln!(
        "usage: rsdfind --state <dir> [-onlyin <dir>] [-count] [-name <pattern>] [-0] [--explain] [query]"
    );
    std::process::exit(2);
}

/// -live: subscribe over the daemon's IPC socket and stream deltas.
fn run_live(state: &std::path::Path, rql: &str) -> ! {
    use rsd_ipc::{recv, send, Request, Response};
    let sock = state.join("rsd.sock");
    let mut stream = std::os::unix::net::UnixStream::connect(&sock).unwrap_or_else(|e| {
        eprintln!("rsdfind: cannot reach daemon at {sock:?}: {e}");
        std::process::exit(1);
    });
    send(
        &mut stream,
        &Request::Hello {
            principal: "rsdfind".into(),
        },
    )
    .unwrap();
    let _: Response = recv(&mut stream).unwrap();
    send(&mut stream, &Request::Subscribe { rql: rql.into() }).unwrap();
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
            Ok(Response::Err(e)) => {
                eprintln!("rsdfind: {e}");
                std::process::exit(1);
            }
            Ok(_) => {}
            Err(_) => std::process::exit(0),
        }
    }
}

/// -live --semantic: a standing semantic alert (threshold classification).
fn run_alert(state: &std::path::Path, query: &str, threshold: f32) -> ! {
    use rsd_ipc::{recv, send, Request, Response};
    let mut stream = std::os::unix::net::UnixStream::connect(state.join("rsd.sock"))
        .unwrap_or_else(|e| {
            eprintln!("rsdfind: cannot reach daemon: {e}");
            std::process::exit(1);
        });
    send(
        &mut stream,
        &Request::Hello {
            principal: "rsdfind".into(),
        },
    )
    .unwrap();
    let _: Response = recv(&mut stream).unwrap();
    send(
        &mut stream,
        &Request::SubscribeAlert {
            query: query.into(),
            threshold,
        },
    )
    .unwrap();
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
            Ok(_) => {}
            Err(_) => std::process::exit(0),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut state = None;
    let mut onlyin = None;
    let mut count = false;
    let mut name_pat: Option<String> = None;
    let mut nul = false;
    let mut explain = false;
    let mut live = false;
    let mut semantic = false;
    let mut hybrid = false;
    let mut threshold = 0.35f32;
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
            "--hybrid" => hybrid = true,
            "--explain" => explain = true,
            _ => query_parts.push(a),
        }
    }
    let Some(state) = state else { usage() };
    let state = std::path::PathBuf::from(state);

    // Build the RQL source: -name maps to a kMDItemFSName glob; bare args are
    // a content search; both AND together (mdfind semantics).
    let mut clauses: Vec<String> = Vec::new();
    if let Some(pat) = &name_pat {
        let pat = if pat.contains('*') {
            pat.clone()
        } else {
            format!("*{pat}*")
        };
        clauses.push(format!(r#"kMDItemFSName == "{pat}"c"#));
    }
    let text = query_parts.join(" ");
    if !text.is_empty() {
        if semantic {
            clauses.push(format!(r#"semantic("{text}")"#));
        } else {
            clauses.push(text);
        }
    }
    if clauses.is_empty() {
        usage();
    }
    let src = clauses.join(" && ");

    if live {
        if semantic {
            run_alert(&state, &query_parts.join(" "), threshold);
        }
        run_live(&state, &src);
    }

    let expr = match parse(&src) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("rsdfind: {e}");
            std::process::exit(1);
        }
    };

    let catalog = Catalog::open_with_durability(&state.join("catalog.redb"), Durability::Eventual)
        .unwrap_or_else(|e| {
            eprintln!("rsdfind: cannot open catalog: {e}");
            std::process::exit(1);
        });
    let lexical = LexicalReader::open(&state.join("lexical")).ok();
    let vector = rsd_vector::VectorPlane::open(
        &state.join("vector.redb"),
        std::sync::Arc::new(rsd_vector::HashEmbedder::default()),
    )
    .ok();

    let engine = QueryEngine {
        catalog: &catalog,
        lexical: lexical.as_ref(),
        vector: vector.as_ref(),
        limit: 10_000,
    };
    if hybrid {
        let text = query_parts.join(" ");
        match engine.hybrid(&text, 100) {
            Ok(hits) => {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                if count {
                    let _ = writeln!(out, "{}", hits.len());
                } else {
                    for h in hits {
                        let _ = writeln!(out, "{}", h.path);
                    }
                }
                return;
            }
            Err(e) => {
                eprintln!("rsdfind: {e}");
                std::process::exit(1);
            }
        }
    }
    if explain {
        eprintln!("plan: {:?}", engine.plan(&expr));
    }
    let hits = match engine.run(&expr, onlyin.as_deref()) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("rsdfind: {e}");
            std::process::exit(1);
        }
    };

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if count {
        let _ = writeln!(out, "{}", hits.len());
        return;
    }
    for h in hits {
        if nul {
            let _ = write!(out, "{}\0", h.path);
        } else {
            let _ = writeln!(out, "{}", h.path);
        }
    }
}
