//! The IPC surface (P5.3): UDS server, principal grants, enforcement before
//! any output — results, counts, and live deltas are computed over the
//! authorized subset only (DESIGN.md §11).
//!
//! Tier model v1: connections must be same-uid (getpeereid). The `Hello`
//! principal selects grants: unlisted principals are first-party (full index);
//! listed principals see only their path prefixes. XPC audit-token code
//! identity (untrusted third-party tier) is the documented next step; until it
//! lands, cross-uid and unknown-binary access simply doesn't exist.

use rsd_catalog::Catalog;
use rsd_ipc::{recv, send, Hit, Request, Response, PROTOCOL_VERSION};
use rsd_lexical::LexicalReader;
use rsd_live::{LiveEngine, LiveEvent};
use rsd_query::{parse, QueryEngine, GRAMMAR_VERSION};
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Principal → granted path prefixes. Principals not present are first-party
/// (full access). An empty grant list means NO access.
#[derive(Debug, Default)]
pub struct AuthzStore {
    grants: HashMap<String, Vec<String>>,
}

impl AuthzStore {
    pub fn grant(&mut self, principal: &str, prefixes: Vec<String>) {
        self.grants.insert(principal.to_string(), prefixes);
    }

    /// None = unrestricted (first-party). Some(prefixes) = scoped.
    pub fn scopes(&self, principal: &str) -> Option<Vec<String>> {
        self.grants.get(principal).cloned()
    }
}

pub struct IpcCtx {
    pub catalog: Arc<Catalog>,
    pub lexical_dir: PathBuf,
    pub live: Arc<Mutex<LiveEngine>>,
    pub authz: Arc<AuthzStore>,
}

pub fn start_ipc(sock_path: &Path, ctx: IpcCtx) -> std::io::Result<std::thread::JoinHandle<()>> {
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path)?;
    let ctx = Arc::new(ctx);
    std::thread::Builder::new()
        .name("rsd-ipc".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                let ctx = ctx.clone();
                let _ = std::thread::Builder::new()
                    .name("rsd-ipc-conn".into())
                    .spawn(move || {
                        if let Err(e) = serve_conn(stream, &ctx) {
                            tracing::debug!("ipc conn ended: {e}");
                        }
                    });
            }
        })
}

fn authorized(grants: &Option<Vec<String>>, path: &str) -> bool {
    match grants {
        None => true,
        Some(list) => list.iter().any(|g| path.starts_with(g.as_str())),
    }
}

fn serve_conn(mut stream: UnixStream, ctx: &IpcCtx) -> std::io::Result<()> {
    // Same-uid gate: the v1 trust boundary.
    let (peer_uid, _) = nix::unistd::getpeereid(&stream)
        .map_err(|e| std::io::Error::other(format!("getpeereid: {e}")))?;
    if peer_uid != nix::unistd::getuid() {
        return Err(std::io::Error::other("cross-uid connection refused"));
    }

    let hello: Request = recv(&mut stream).map_err(std::io::Error::other)?;
    let Request::Hello { principal } = hello else {
        send(&mut stream, &Response::Err("Hello required first".into()))
            .map_err(std::io::Error::other)?;
        return Ok(());
    };
    let grants = ctx.authz.scopes(&principal);
    tracing::info!("ipc: principal {principal:?} connected (grants: {grants:?})");
    send(
        &mut stream,
        &Response::Hello {
            protocol: PROTOCOL_VERSION,
            grammar: GRAMMAR_VERSION,
        },
    )
    .map_err(std::io::Error::other)?;

    let lexical = LexicalReader::open(&ctx.lexical_dir).ok();

    loop {
        let req: Request = match recv(&mut stream) {
            Ok(r) => r,
            Err(_) => return Ok(()), // client hung up
        };
        match req {
            Request::Hello { .. } => {
                send(&mut stream, &Response::Err("already identified".into()))
                    .map_err(std::io::Error::other)?;
            }
            Request::Query {
                rql,
                scope,
                count_only,
            } => {
                let resp = run_query(ctx, lexical.as_ref(), &grants, &rql, scope.as_deref())
                    .map(|hits| {
                        if count_only {
                            Response::Count(hits.len() as u64)
                        } else {
                            Response::Hits(hits)
                        }
                    })
                    .unwrap_or_else(Response::Err);
                send(&mut stream, &resp).map_err(std::io::Error::other)?;
            }
            Request::Subscribe { rql } => {
                let expr = match parse(&rql) {
                    Ok(e) => e,
                    Err(e) => {
                        send(&mut stream, &Response::Err(e.to_string()))
                            .map_err(std::io::Error::other)?;
                        continue;
                    }
                };
                // Fence: register + one-shot under the engine lock, so no
                // commit can slip between the initial set and the stream.
                let (initial, rx) = {
                    let mut live = ctx.live.lock().unwrap();
                    let initial =
                        run_query(ctx, lexical.as_ref(), &grants, &rql, None).unwrap_or_default();
                    let (_, rx) = live.subscribe(
                        expr,
                        grants.clone().unwrap_or_default(),
                        initial.iter().map(|h| h.oid),
                        1024,
                    );
                    (initial, rx)
                };
                send(&mut stream, &Response::Subscribed(initial)).map_err(std::io::Error::other)?;
                // Dedicated stream from here on.
                loop {
                    match rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(LiveEvent::Enter { oid, path }) => {
                            send(
                                &mut stream,
                                &Response::Event {
                                    enter: true,
                                    oid,
                                    path,
                                },
                            )
                            .map_err(std::io::Error::other)?;
                        }
                        Ok(LiveEvent::Leave { oid, path }) => {
                            send(
                                &mut stream,
                                &Response::Event {
                                    enter: false,
                                    oid,
                                    path,
                                },
                            )
                            .map_err(std::io::Error::other)?;
                        }
                        Ok(LiveEvent::Resync) => {
                            send(&mut stream, &Response::Resync).map_err(std::io::Error::other)?;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                }
            }
        }
    }
}

fn run_query(
    ctx: &IpcCtx,
    lexical: Option<&LexicalReader>,
    grants: &Option<Vec<String>>,
    rql: &str,
    scope: Option<&str>,
) -> Result<Vec<Hit>, String> {
    let expr = parse(rql).map_err(|e| e.to_string())?;
    let engine = QueryEngine {
        catalog: &ctx.catalog,
        lexical,
        limit: 10_000,
    };
    let hits = engine.run(&expr, scope).map_err(|e| e.to_string())?;
    // Enforcement before ANY output: counts and results alike are computed
    // over the authorized subset only.
    Ok(hits
        .into_iter()
        .filter(|h| authorized(grants, &h.path))
        .map(|h| Hit {
            oid: h.oid,
            path: h.path,
        })
        .collect())
}
