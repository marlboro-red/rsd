//! Localhost JSON API — the seam native UIs (RSD.app) consume.
//!
//! Bound to 127.0.0.1 only; same-machine trust boundary as the UDS surface
//! (macOS has no peer credentials on TCP, so this surface carries first-party
//! grants only — scoped principals stay on UDS/XPC). No external assets,
//! nothing leaves the machine.
//!
//!   GET /api/search?q=<query>&mode=hybrid|lexical|semantic|rql&limit=N
//!   GET /api/status

use crate::connection_limit::ConnectionLimit;
use crate::ipc::IpcCtx;
use rsd_caes::{CaesKey, ABI_VERSION};
use rsd_ipc::Scope;
use rsd_lexical::LexicalReader;
use rsd_query::{parse, QueryEngine};
use serde_json::json;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;

const MAX_CONNECTIONS: usize = 128;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 32 hex chars from the OS RNG — the loopback secret.
pub fn gen_token() -> std::io::Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf)
        .map_err(|error| std::io::Error::other(format!("OS entropy unavailable: {error}")))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn token_matches(expected: &str, presented: &str) -> bool {
    let Ok(expected): Result<&[u8; 32], _> = expected.as_bytes().try_into() else {
        return false;
    };
    let mut candidate = [0u8; 32];
    let copied = presented.len().min(candidate.len());
    candidate[..copied].copy_from_slice(&presented.as_bytes()[..copied]);
    constant_time_eq::constant_time_eq_32(expected, &candidate) & (presented.len() == 32)
}

/// Write the token 0600 (user-only), durably and atomically.
pub fn write_token(path: &std::path::Path, token: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension("token.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(token.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(path.parent().unwrap_or_else(|| std::path::Path::new(".")))?.sync_all()
}

pub fn start_http(
    port: u16,
    token: String,
    scope: Scope,
    ctx: IpcCtx,
    caes: Option<Arc<rsd_caes::Store>>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let ctx = Arc::new((ctx, caes, token, scope));
    let connections = ConnectionLimit::new(MAX_CONNECTIONS);
    std::thread::Builder::new()
        .name("rsd-http".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                let Some(permit) = connections.try_acquire() else {
                    let _ = respond(
                        &mut stream,
                        "503 Service Unavailable",
                        r#"{"error":"connection limit reached"}"#,
                    );
                    continue;
                };
                let ctx = ctx.clone();
                let _ = std::thread::Builder::new()
                    .name("rsd-http-conn".into())
                    .spawn(move || {
                        let _permit = permit;
                        let _ = serve(stream, &ctx.0, ctx.1.as_deref(), &ctx.2, &ctx.3);
                    });
            }
        })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_params(path: &str) -> (String, Vec<(String, String)>) {
    match path.split_once('?') {
        None => (path.to_string(), vec![]),
        Some((p, q)) => (
            p.to_string(),
            q.split('&')
                .filter_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    Some((k.to_string(), percent_decode(v)))
                })
                .collect(),
        ),
    }
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn serve(
    mut stream: TcpStream,
    ctx: &IpcCtx,
    caes: Option<&rsd_caes::Store>,
    token: &str,
    scope: &Scope,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(HEADER_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let head = read_header(&mut reader)?;
    let mut lines = head.lines();
    let line = lines.next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    // Drain headers, capturing the auth header.
    let mut header_token: Option<String> = None;
    for h in lines {
        if let Some(v) = h
            .strip_prefix("X-RSD-Token:")
            .or_else(|| h.strip_prefix("x-rsd-token:"))
        {
            header_token = Some(v.trim().to_string());
        }
    }
    if method != "GET" {
        return respond(&mut stream, "405 Method Not Allowed", "{}");
    }
    let (path, params) = query_params(target);
    let get = |k: &str| {
        params
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, v)| v.clone())
    };

    // Loopback-secret gate (closes the browser-reachable-localhost hole): the
    // token lives in a 0600 file only the user can read; a web page can send a
    // request but cannot read the secret, so it cannot authenticate.
    let presented = get("token").or(header_token).unwrap_or_default();
    if !token_matches(token, &presented) {
        return respond(
            &mut stream,
            "403 Forbidden",
            r#"{"error":"missing or bad token"}"#,
        );
    }

    match path.as_str() {
        "/api/status" => {
            let (entries, objects) = scoped_catalog_counts(&ctx.catalog, scope).unwrap_or((0, 0));
            let body = json!({
                "entries": entries,
                "objects": objects,
                "semantic": ctx.vector.is_some(),
            });
            respond(&mut stream, "200 OK", &body.to_string())
        }
        "/api/metrics" => {
            if !matches!(scope, Scope::Unrestricted) {
                return respond(
                    &mut stream,
                    "403 Forbidden",
                    r#"{"error":"global metrics require unrestricted scope"}"#,
                );
            }
            // The metric plane snapshot (§18.5.2). Cardinality-bounded by
            // construction; behind the same loopback-token gate as everything.
            rsd_metrics::metrics()
                .catalog_entries
                .set(ctx.catalog.entry_count().unwrap_or(0) as i64);
            respond(
                &mut stream,
                "200 OK",
                &rsd_metrics::snapshot_json().to_string(),
            )
        }
        "/api/search" => {
            let q = get("q").unwrap_or_default();
            let mode = get("mode").unwrap_or_else(|| "hybrid".into());
            let limit = get("limit")
                .and_then(|value| value.parse().ok())
                .unwrap_or(25usize)
                .clamp(1, 1_000);
            if q.trim().is_empty() {
                return respond(&mut stream, "200 OK", r#"{"hits":[],"ms":0}"#);
            }
            let t0 = std::time::Instant::now();
            let lexical = LexicalReader::open(&ctx.lexical_dir).ok();
            let vguard = ctx
                .vector
                .as_ref()
                .map(|vector| vector.lock().unwrap_or_else(|error| error.into_inner()));
            let engine = QueryEngine {
                catalog: &ctx.catalog,
                lexical: lexical.as_ref(),
                vector: vguard.as_deref(),
                limit: limit.max(50),
            };
            let hits = match mode.as_str() {
                "hybrid" if engine.vector.is_some() => {
                    run_http_hybrid(&engine, &q, limit, scope).map(|v| {
                        v.into_iter()
                            .map(|(h, o)| (h, o.as_str()))
                            .collect::<Vec<_>>()
                    })
                }
                "semantic" => parse(&format!(r#"semantic("{}")"#, q.replace('"', "")))
                    .and_then(|e| run_http_query(&engine, &e, scope))
                    .map(|v| v.into_iter().map(|h| (h, "meaning")).collect::<Vec<_>>()),
                "rql" => parse(&q)
                    .and_then(|e| run_http_query(&engine, &e, scope))
                    .map(|v| v.into_iter().map(|h| (h, "rql")).collect::<Vec<_>>()),
                _ => parse(&format!(r#""{}""#, q.replace('"', "")))
                    .and_then(|e| run_http_query(&engine, &e, scope))
                    .map(|v| v.into_iter().map(|h| (h, "exact")).collect::<Vec<_>>()),
            };
            match hits {
                Ok(hits) => {
                    let items: Vec<serde_json::Value> = hits
                        .iter()
                        .take(limit)
                        .map(|(h, origin)| {
                            json!({
                                "path": h.path,
                                "snippet": snippet(ctx, caes, h.oid, &q),
                                "match": origin,
                            })
                        })
                        .collect();
                    let body = json!({
                        "hits": items,
                        "ms": t0.elapsed().as_secs_f64() * 1000.0,
                        "mode": mode,
                    });
                    respond(&mut stream, "200 OK", &body.to_string())
                }
                Err(e) => respond(
                    &mut stream,
                    "400 Bad Request",
                    &json!({ "error": e.to_string() }).to_string(),
                ),
            }
        }
        "/api/events" => sse_view(
            &mut stream,
            ctx,
            scope.clone(),
            // Match-all standing view: every committed file delta becomes an
            // invalidation tick the UI can refresh on.
            parse("kMDItemFSSize >= 0").map_err(|e| std::io::Error::other(e.to_string()))?,
        ),
        "/api/alert" => {
            let q = get("q").unwrap_or_default();
            let threshold: f32 = get("threshold")
                .and_then(|t| t.parse().ok())
                .unwrap_or(0.35);
            let sub =
                ctx.live
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .subscribe_alert(&q, threshold, scope.clone(), 1024);
            match sub {
                Some((_, rx)) => sse_forward(&mut stream, rx),
                None => respond(
                    &mut stream,
                    "400 Bad Request",
                    r#"{"error":"semantic disabled"}"#,
                ),
            }
        }
        _ => respond(&mut stream, "404 Not Found", "{}"),
    }
}

fn scope_grants(scope: &Scope) -> Option<Vec<PathBuf>> {
    match scope {
        Scope::Unrestricted => None,
        Scope::Paths(prefixes) => Some(
            prefixes
                .iter()
                .map(|prefix| prefix.as_path().to_path_buf())
                .collect(),
        ),
    }
}

fn scoped_catalog_counts(
    catalog: &rsd_catalog::Catalog,
    scope: &Scope,
) -> rsd_catalog::Result<(u64, u64)> {
    if matches!(scope, Scope::Unrestricted) {
        return Ok((catalog.entry_count()?, catalog.object_count()?));
    }
    let Some(grants) = scope_grants(scope) else {
        unreachable!("unrestricted scope returned above")
    };
    let mut paths = HashSet::new();
    for grant in grants {
        for path in catalog.subtree_paths(&grant.to_string_lossy())? {
            paths.insert(path);
        }
    }
    let mut objects = HashSet::new();
    for path in &paths {
        if let Some((oid, _)) = catalog.get_by_path(path)? {
            objects.insert(oid);
        }
    }
    Ok((paths.len() as u64, objects.len() as u64))
}

fn run_http_query(
    engine: &QueryEngine<'_>,
    expr: &rsd_query::Expr,
    scope: &Scope,
) -> rsd_query::Result<Vec<rsd_query::Hit>> {
    match scope_grants(scope) {
        None => engine.run(expr, None),
        Some(grants) => engine.run_authorized(expr, None, &grants),
    }
}

fn run_http_hybrid(
    engine: &QueryEngine<'_>,
    text: &str,
    limit: usize,
    scope: &Scope,
) -> rsd_query::Result<Vec<(rsd_query::Hit, rsd_vector::MatchOrigin)>> {
    match scope_grants(scope) {
        None => engine.hybrid_tagged(text, limit),
        Some(grants) => engine.hybrid_tagged_authorized(text, limit, &grants),
    }
}

/// Read an HTTP request head without allowing a pre-auth peer to grow a
/// `String` without bound. `fill_buf` itself has fixed capacity; this function
/// copies at most `MAX_HEADER_BYTES` before rejecting the request.
fn read_header(reader: &mut impl BufRead) -> std::io::Result<String> {
    let mut bytes = Vec::with_capacity(1024);
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let available_len = available.len();
        let remaining = MAX_HEADER_BYTES.saturating_sub(bytes.len());
        if remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP request headers exceed limit",
            ));
        }
        let take = available_len.min(remaining);
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n")
            || bytes.windows(2).any(|window| window == b"\n\n")
        {
            return String::from_utf8(bytes)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad header"));
        }
        if take < available_len || bytes.len() == MAX_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP request headers exceed limit",
            ));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "incomplete HTTP request headers",
    ))
}

fn sse_headers(stream: &mut TcpStream) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n"
    )
}

fn sse_view(
    stream: &mut TcpStream,
    ctx: &IpcCtx,
    scope: Scope,
    expr: rsd_query::Expr,
) -> std::io::Result<()> {
    let (_, rx) = ctx
        .live
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .subscribe(expr, scope, [], 4096);
    sse_forward(stream, rx)
}

/// Forward live events as SSE until the client hangs up (write fails).
fn sse_forward(
    stream: &mut TcpStream,
    rx: std::sync::mpsc::Receiver<rsd_live::LiveEvent>,
) -> std::io::Result<()> {
    sse_headers(stream)?;
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(rsd_live::LiveEvent::Enter { path, .. }) => {
                write!(stream, "data: {}\n\n", json!({"event":"enter","path":path}))?;
            }
            Ok(rsd_live::LiveEvent::Leave { path, .. }) => {
                write!(stream, "data: {}\n\n", json!({"event":"leave","path":path}))?;
            }
            Ok(rsd_live::LiveEvent::Resync) => {
                write!(stream, "data: {}\n\n", json!({"event":"resync"}))?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                write!(stream, ": ping\n\n")?; // heartbeat; detects dead peers
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        stream.flush()?;
    }
}

/// Grounded excerpt around the first query-term hit (mirrors rsd-mcp).
fn snippet(ctx: &IpcCtx, caes: Option<&rsd_caes::Store>, oid: u64, q: &str) -> String {
    let Some(caes) = caes else {
        return String::new();
    };
    let Ok(Some(rec)) = ctx.catalog.get_object(oid) else {
        return String::new();
    };
    let (Some(ch), Some(hh)) = (rec.content_hash, rec.caes_hints_hash) else {
        return String::new();
    };
    let Ok(Some(er)) = caes.get(&CaesKey {
        content_hash: ch,
        extractor_id: rsd_extract::EXTRACTOR_ID.into(),
        extractor_version: rsd_extract::EXTRACTOR_VERSION,
        hints_hash: hh,
        abi_version: ABI_VERSION,
    }) else {
        return String::new();
    };
    snippet_window(&er.text, q)
}

fn snippet_window(text: &str, query: &str) -> String {
    let needle = query.split_whitespace().next().unwrap_or(query);
    let pos = text.find(needle).or_else(|| {
        text.char_indices().find_map(|(start, _)| {
            let end = start.checked_add(needle.len())?;
            (end <= text.len()
                && text.is_char_boundary(end)
                && text[start..end].eq_ignore_ascii_case(needle))
            .then_some(start)
        })
    });
    let pos = pos.unwrap_or(0);
    let mut start = pos.saturating_sub(60);
    let mut end = pos.saturating_add(140).min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_random_fixed_width_hex() {
        let first = gen_token().unwrap();
        let second = gen_token().unwrap();
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn token_comparison_requires_an_exact_match() {
        let token = "0123456789abcdef0123456789abcdef";
        assert!(token_matches(token, token));
        assert!(!token_matches(token, "0123456789abcdef0123456789abcdee"));
        assert!(!token_matches(token, "0123456789abcdef"));
        assert!(!token_matches(token, ""));
    }

    #[test]
    fn http_query_execution_honors_its_explicit_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = rsd_catalog::Catalog::open(&tmp.path().join("catalog.redb")).unwrap();
        let stat = |ino| rsd_catalog::StatInfo {
            kind: rsd_catalog::ObjectKind::File,
            file_id: rsd_catalog::FileId { dev: 1, ino },
            size: 1,
            mtime_ns: 1,
            birthtime_ns: ino as i64,
            nlink: 1,
        };
        catalog
            .apply_changes_direct(&[
                rsd_catalog::Change::Upsert {
                    path: "/allowed/a.txt".into(),
                    stat: stat(1),
                },
                rsd_catalog::Change::Upsert {
                    path: "/private/b.txt".into(),
                    stat: stat(2),
                },
            ])
            .unwrap();
        let engine = QueryEngine {
            catalog: &catalog,
            lexical: None,
            vector: None,
            limit: 10,
        };
        let expr = parse("kMDItemFSSize > 0").unwrap();
        let scope = Scope::paths(["/allowed"]);
        let hits = run_http_query(&engine, &expr, &scope).unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/allowed/a.txt");
        assert_eq!(scoped_catalog_counts(&catalog, &scope).unwrap(), (1, 1));
        assert_eq!(
            scoped_catalog_counts(&catalog, &Scope::Unrestricted).unwrap(),
            (2, 2)
        );
    }

    #[test]
    fn request_headers_are_size_bounded() {
        let valid = b"GET /api/status HTTP/1.1\r\nX-RSD-Token: token\r\n\r\n";
        assert_eq!(
            read_header(&mut std::io::Cursor::new(valid)).unwrap(),
            String::from_utf8(valid.to_vec()).unwrap()
        );

        let oversized = vec![b'x'; MAX_HEADER_BYTES + 1];
        let error = read_header(&mut std::io::Cursor::new(oversized)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn snippet_offsets_stay_on_original_unicode_boundaries() {
        let text = "İstanbul planning notes and meeting summary";
        assert_eq!(
            snippet_window(text, "istanbul"),
            "İstanbul planning notes and meeting summary"
        );
    }
}
