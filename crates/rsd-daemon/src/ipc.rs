//! The IPC surface (P5.3): UDS server, principal grants, enforcement before
//! any output — results, counts, and live deltas are computed over the
//! authorized subset only (DESIGN.md §11).
//!
//! Tier model v1: connections must be same-uid (getpeereid). Authority then
//! comes from one of two places, both explicit:
//!
//!  - `Hello.token` matching the loopback secret grants first-party
//!    `Unrestricted` scope. This is the same authority the HTTP surface grants
//!    any reader of `<state>/http.token` (0600), so it adds no new trust — it
//!    lets first-party CLI clients reach the daemon instead of opening the
//!    store directly, which the single-writer store makes impossible anyway.
//!  - `Hello.principal` selects a configured named grant. The identity is
//!    caller-asserted, so no such grants ship; XPC audit-token code identity
//!    remains required before third-party access can be safely configured.
//!
//! An unlisted principal presenting no token gets `Scope::default()` — deny.
//! `Hello.restrict_to` can only narrow the result, never widen it.

use crate::connection_limit::ConnectionLimit;
use rsd_catalog::Catalog;
use rsd_ipc::{recv, send, Hit, Request, Response, Scope, Snippet, PROTOCOL_VERSION};
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
/// Upper bound on a single response, mirroring the HTTP surface's clamp so a
/// client cannot ask the daemon to materialize an unbounded result vector.
const MAX_RESULTS: usize = 1_000;

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
    /// Extracted-text store, for grounded snippets. `None` disables the
    /// `Snippets` request rather than degrading it to ungrounded output.
    pub caes: Option<Arc<rsd_caes::Store>>,
    /// The loopback secret a first-party client presents in `Hello`. `None`
    /// disables token auth entirely, leaving only named grants.
    pub first_party_token: Option<String>,
}

impl IpcCtx {
    /// Resolve a `Hello` to the scope every later request is served under.
    ///
    /// Returns the granted scope narrowed by any self-restriction. Kept
    /// separate from the connection loop so the authorization decision is
    /// directly testable — the leak suite drives this, not just the socket.
    pub fn resolve_scope(
        &self,
        principal: &str,
        token: Option<&str>,
        restrict_to: Option<&[String]>,
    ) -> Scope {
        let granted = match (self.first_party_token.as_deref(), token) {
            (Some(expected), Some(presented))
                if crate::http::token_matches(expected, presented) =>
            {
                Scope::Unrestricted
            }
            _ => self.authz.scope(principal),
        };
        match restrict_to {
            Some(paths) => granted.intersect(&Scope::paths(paths.to_vec())),
            None => granted,
        }
    }
}

/// `sockaddr_un.sun_path` is 104 bytes on macOS including the NUL. Bind fails
/// with a bare `InvalidInput` when the path is longer, which reads as a bug in
/// the caller's arguments rather than what it is — so check it up front and say
/// which path is too long and by how much.
pub fn check_sock_path(sock_path: &Path) -> std::io::Result<()> {
    const SUN_PATH_MAX: usize = 103; // 104 bytes of storage, less the NUL.
    let len = sock_path.as_os_str().as_encoded_bytes().len();
    if len > SUN_PATH_MAX {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "socket path is {len} bytes, {} over the {SUN_PATH_MAX}-byte limit: {}\n\
                 use a shorter --state directory",
                len - SUN_PATH_MAX,
                sock_path.display()
            ),
        ));
    }
    Ok(())
}

pub fn start_ipc(sock_path: &Path, ctx: IpcCtx) -> std::io::Result<std::thread::JoinHandle<()>> {
    check_sock_path(sock_path)?;
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
    let Request::Hello {
        principal,
        token,
        restrict_to,
    } = hello
    else {
        send(&mut stream, &Response::Err("Hello required first".into()))
            .map_err(std::io::Error::other)?;
        return Ok(());
    };
    let authz_scope = ctx.resolve_scope(&principal, token.as_deref(), restrict_to.as_deref());
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
                let resp = if count_only {
                    run_count(ctx, lexical.as_ref(), &authz_scope, &rql, scope.as_deref())
                        .map(Response::Count)
                } else {
                    run_query(ctx, lexical.as_ref(), &authz_scope, &rql, scope.as_deref())
                        .map(Response::Hits)
                }
                .unwrap_or_else(Response::Err);
                send(&mut stream, &resp).map_err(std::io::Error::other)?;
            }
            Request::Hybrid {
                query,
                scope,
                limit,
            } => {
                let resp = run_hybrid(
                    ctx,
                    lexical.as_ref(),
                    &authz_scope,
                    &query,
                    scope.as_deref(),
                    limit,
                )
                .map(Response::Hits)
                .unwrap_or_else(Response::Err);
                send(&mut stream, &resp).map_err(std::io::Error::other)?;
            }
            Request::Snippets {
                query,
                scope,
                limit,
            } => {
                let resp = run_snippets(
                    ctx,
                    lexical.as_ref(),
                    &authz_scope,
                    &query,
                    scope.as_deref(),
                    limit,
                )
                .map(Response::Snippets)
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
    let hits = match authz_scope {
        Scope::Unrestricted => engine.run(&expr, scope),
        Scope::Paths(prefixes) => {
            let grants: Vec<PathBuf> = prefixes
                .iter()
                .map(|prefix| prefix.as_path().to_path_buf())
                .collect();
            engine.run_authorized(&expr, scope, &grants)
        }
    }
    .map_err(|e| e.to_string())?;
    Ok(hits
        .into_iter()
        .map(|h| Hit {
            oid: h.oid,
            path: h.path,
        })
        .collect())
}

/// Authorization for one hybrid request, as a grant list.
///
/// The hybrid retriever has no `-onlyin` parameter of its own (unlike
/// `QueryEngine::run`), so the request's `-onlyin` is folded into the grants by
/// intersection. That keeps the narrowing enforced during candidate generation
/// instead of as a post-filter, and reuses the one intersection implementation.
/// `None` means "no constraint" — reachable only from an unrestricted scope
/// with no `-onlyin`.
fn hybrid_grants(authz_scope: &Scope, onlyin: Option<&str>) -> Option<Vec<PathBuf>> {
    let effective = match onlyin {
        Some(path) => authz_scope.intersect(&Scope::paths([path.to_string()])),
        None => authz_scope.clone(),
    };
    match effective {
        Scope::Unrestricted => None,
        Scope::Paths(prefixes) => Some(
            prefixes
                .iter()
                .map(|prefix| prefix.as_path().to_path_buf())
                .collect(),
        ),
    }
}

fn run_hybrid(
    ctx: &IpcCtx,
    lexical: Option<&LexicalReader>,
    authz_scope: &Scope,
    query: &str,
    onlyin: Option<&str>,
    limit: u32,
) -> Result<Vec<Hit>, String> {
    let limit = (limit as usize).clamp(1, MAX_RESULTS);
    let vguard = ctx
        .vector
        .as_ref()
        .map(|vector| vector.lock().unwrap_or_else(|error| error.into_inner()));
    let engine = QueryEngine {
        catalog: &ctx.catalog,
        lexical,
        vector: vguard.as_deref(),
        limit,
    };
    let hits = match hybrid_grants(authz_scope, onlyin) {
        None => engine.hybrid(query, limit),
        Some(grants) => engine
            .hybrid_tagged_authorized(query, limit, &grants)
            .map(|tagged| tagged.into_iter().map(|(hit, _)| hit).collect()),
    }
    .map_err(|e| e.to_string())?;
    Ok(hits
        .into_iter()
        .map(|h| Hit {
            oid: h.oid,
            path: h.path,
        })
        .collect())
}

fn run_snippets(
    ctx: &IpcCtx,
    lexical: Option<&LexicalReader>,
    authz_scope: &Scope,
    query: &str,
    onlyin: Option<&str>,
    limit: u32,
) -> Result<Vec<Snippet>, String> {
    let caes = ctx
        .caes
        .as_ref()
        .ok_or("daemon has no extracted-text store (snippets unavailable)")?;
    let hits = run_hybrid(ctx, lexical, authz_scope, query, onlyin, limit)?;
    let mut out = Vec::new();
    for hit in hits {
        let Some(text) = crate::snippet::text_for(&ctx.catalog, Some(caes.as_ref()), hit.oid)
        else {
            continue;
        };
        let (start, end) = crate::snippet::span(&text, query);
        out.push(Snippet {
            path: hit.path,
            start: start as u64,
            end: end as u64,
            text: text[start..end]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        });
    }
    Ok(out)
}

fn run_count(
    ctx: &IpcCtx,
    lexical: Option<&LexicalReader>,
    authz_scope: &Scope,
    rql: &str,
    scope: Option<&str>,
) -> Result<u64, String> {
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
    match authz_scope {
        Scope::Unrestricted => engine.count(&expr, scope),
        Scope::Paths(prefixes) => {
            let grants: Vec<PathBuf> = prefixes
                .iter()
                .map(|prefix| prefix.as_path().to_path_buf())
                .collect();
            engine.count_authorized(&expr, scope, &grants)
        }
    }
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlong_socket_paths_are_rejected_with_an_actionable_error() {
        let ok = PathBuf::from(format!("/{}/rsd.sock", "a".repeat(80)));
        assert!(check_sock_path(&ok).is_ok());

        let too_long = PathBuf::from(format!("/{}/rsd.sock", "a".repeat(120)));
        let error = check_sock_path(&too_long).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let message = error.to_string();
        assert!(message.contains("over the"), "{message}");
        assert!(message.contains("--state"), "{message}");
    }

    #[test]
    fn onlyin_narrows_grants_and_cannot_widen_them() {
        // Unrestricted with no -onlyin is the only unconstrained case.
        assert_eq!(hybrid_grants(&Scope::Unrestricted, None), None);
        assert_eq!(
            hybrid_grants(&Scope::Unrestricted, Some("/root/docs")),
            Some(vec![PathBuf::from("/root/docs")])
        );
        // -onlyin outside the grant yields no reachable roots, not the -onlyin.
        assert_eq!(
            hybrid_grants(&Scope::paths(["/root/docs"]), Some("/root/other")),
            Some(vec![])
        );
        // -onlyin inside the grant narrows to the -onlyin.
        assert_eq!(
            hybrid_grants(&Scope::paths(["/root"]), Some("/root/docs")),
            Some(vec![PathBuf::from("/root/docs")])
        );
    }
}
