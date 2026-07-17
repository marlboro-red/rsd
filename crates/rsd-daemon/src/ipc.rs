//! The IPC surface (P5.3): UDS server, principal grants, enforcement before
//! any output — results, counts, and live deltas are computed over the
//! authorized subset only (DESIGN.md §11).
//!
//! Tier model v1: connections must be same-uid (getpeereid). The caller-stated
//! `Hello` principal selects an explicit grant; unlisted principals are denied.
//! XPC audit-token code identity remains required before third-party access can
//! be safely configured.

use crate::connection_limit::ConnectionLimit;
use rsd_catalog::Catalog;
use rsd_ipc::{recv, send, Hit, Request, Response, Scope, PROTOCOL_VERSION};
use rsd_lexical::LexicalReader;
use rsd_live::{LiveEngine, LiveEvent};
use rsd_query::{parse, QueryEngine, GRAMMAR_VERSION};
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_CONNECTIONS: usize = 128;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Principal → explicit scope. Principals not present have no access.
#[derive(Debug, Default)]
pub struct AuthzStore {
    grants: HashMap<String, Scope>,
}

impl AuthzStore {
    pub fn grant(&mut self, principal: &str, prefixes: Vec<String>) {
        self.grant_scope(principal, Scope::paths(prefixes));
    }

    pub fn grant_unrestricted(&mut self, principal: &str) {
        self.grant_scope(principal, Scope::Unrestricted);
    }

    pub fn grant_scope(&mut self, principal: &str, scope: Scope) {
        self.grants.insert(principal.to_string(), scope);
    }

    pub fn scope(&self, principal: &str) -> Scope {
        self.grants.get(principal).cloned().unwrap_or_default()
    }
}

pub struct IpcCtx {
    pub catalog: Arc<Catalog>,
    pub lexical_dir: PathBuf,
    pub vector: Option<Arc<Mutex<rsd_vector::VectorPlane>>>,
    pub live: Arc<Mutex<LiveEngine>>,
    pub authz: Arc<AuthzStore>,
}

pub fn start_ipc(sock_path: &Path, ctx: IpcCtx) -> std::io::Result<std::thread::JoinHandle<()>> {
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path)?;
    let ctx = Arc::new(ctx);
    let connections = ConnectionLimit::new(MAX_CONNECTIONS);
    std::thread::Builder::new()
        .name("rsd-ipc".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                let Some(permit) = connections.try_acquire() else {
                    let _ = send(
                        &mut stream,
                        &Response::Err("connection limit reached".into()),
                    );
                    continue;
                };
                let ctx = ctx.clone();
                let _ = std::thread::Builder::new()
                    .name("rsd-ipc-conn".into())
                    .spawn(move || {
                        let _permit = permit;
                        if let Err(e) = serve_conn(stream, &ctx) {
                            tracing::debug!("ipc conn ended: {e}");
                        }
                    });
            }
        })
}

fn serve_conn(mut stream: UnixStream, ctx: &IpcCtx) -> std::io::Result<()> {
    // Same-uid gate: the v1 trust boundary.
    let (peer_uid, _) = nix::unistd::getpeereid(&stream)
        .map_err(|e| std::io::Error::other(format!("getpeereid: {e}")))?;
    if peer_uid != nix::unistd::getuid() {
        return Err(std::io::Error::other("cross-uid connection refused"));
    }

    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;

    let hello: Request = recv(&mut stream).map_err(std::io::Error::other)?;
    let Request::Hello { principal } = hello else {
        send(&mut stream, &Response::Err("Hello required first".into()))
            .map_err(std::io::Error::other)?;
        return Ok(());
    };
    let authz_scope = ctx.authz.scope(&principal);
    tracing::info!("ipc: principal {principal:?} connected (scope: {authz_scope:?})");
    send(
        &mut stream,
        &Response::Hello {
            protocol: PROTOCOL_VERSION,
            grammar: GRAMMAR_VERSION,
        },
    )
    .map_err(std::io::Error::other)?;
    stream.set_read_timeout(None)?;

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
                let resp = run_query(ctx, lexical.as_ref(), &authz_scope, &rql, scope.as_deref())
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
            Request::SubscribeAlert { query, threshold } => {
                let sub = ctx
                    .live
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .subscribe_alert(&query, threshold, authz_scope.clone(), 1024);
                let Some((_, rx)) = sub else {
                    send(
                        &mut stream,
                        &Response::Err("daemon has no embedder (semantic disabled)".into()),
                    )
                    .map_err(std::io::Error::other)?;
                    continue;
                };
                send(&mut stream, &Response::Subscribed(vec![])).map_err(std::io::Error::other)?;
                forward_events(&mut stream, rx)?;
                return Ok(());
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
                    let mut live = ctx.live.lock().unwrap_or_else(|error| error.into_inner());
                    let initial = run_query(ctx, lexical.as_ref(), &authz_scope, &rql, None)
                        .unwrap_or_default();
                    let (_, rx) = live.subscribe(
                        expr,
                        authz_scope.clone(),
                        initial.iter().map(|h| h.oid),
                        1024,
                    );
                    (initial, rx)
                };
                send(&mut stream, &Response::Subscribed(initial)).map_err(std::io::Error::other)?;
                forward_events(&mut stream, rx)?;
                return Ok(());
            }
        }
    }
}

/// Dedicated event stream: forward live events until either side hangs up.
fn forward_events(
    stream: &mut UnixStream,
    rx: std::sync::mpsc::Receiver<LiveEvent>,
) -> std::io::Result<()> {
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(LiveEvent::Enter { oid, path }) => {
                send(
                    stream,
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
                    stream,
                    &Response::Event {
                        enter: false,
                        oid,
                        path,
                    },
                )
                .map_err(std::io::Error::other)?;
            }
            Ok(LiveEvent::Resync) => {
                send(stream, &Response::Resync).map_err(std::io::Error::other)?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn run_query(
    ctx: &IpcCtx,
    lexical: Option<&LexicalReader>,
    authz_scope: &Scope,
    rql: &str,
    scope: Option<&str>,
) -> Result<Vec<Hit>, String> {
    let expr = parse(rql).map_err(|e| e.to_string())?;
    let vguard = ctx
        .vector
        .as_ref()
        .map(|vector| vector.lock().unwrap_or_else(|error| error.into_inner()));
    let engine = QueryEngine {
        catalog: &ctx.catalog,
        lexical,
        vector: vguard.as_deref(),
        limit: 10_000,
    };
    let hits = engine.run(&expr, scope).map_err(|e| e.to_string())?;
    // Enforcement before ANY output: counts and results alike are computed
    // over the authorized subset only.
    Ok(hits
        .into_iter()
        .filter(|h| authz_scope.allows(&h.path))
        .map(|h| Hit {
            oid: h.oid,
            path: h.path,
        })
        .collect())
}
