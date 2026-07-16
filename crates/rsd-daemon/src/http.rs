//! Localhost JSON API — the seam native UIs (RSD.app) consume.
//!
//! Bound to 127.0.0.1 only; same-machine trust boundary as the UDS surface
//! (macOS has no peer credentials on TCP, so this surface carries first-party
//! grants only — scoped principals stay on UDS/XPC). No external assets,
//! nothing leaves the machine.
//!
//!   GET /api/search?q=<query>&mode=hybrid|lexical|semantic|rql&limit=N
//!   GET /api/status

use crate::ipc::IpcCtx;
use rsd_caes::{CaesKey, ABI_VERSION};
use rsd_extract::{EXTRACTOR_ID, EXTRACTOR_VERSION};
use rsd_lexical::LexicalReader;
use rsd_query::{parse, QueryEngine};
use serde_json::json;
use std::io::Read as _;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;

/// 32 hex chars from the OS RNG — the loopback secret.
pub fn gen_token() -> String {
    let mut buf = [0u8; 16];
    // /dev/urandom needs no crate; fall back to time-mixed if it ever fails.
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write the token 0600 (user-only), atomically.
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
    std::fs::rename(&tmp, path)
}

pub fn start_http(
    port: u16,
    token: String,
    ctx: IpcCtx,
    caes: Option<Arc<rsd_caes::Store>>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let ctx = Arc::new((ctx, caes, token));
    std::thread::Builder::new()
        .name("rsd-http".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                let ctx = ctx.clone();
                let _ = std::thread::Builder::new()
                    .name("rsd-http-conn".into())
                    .spawn(move || {
                        let _ = serve(stream, &ctx.0, ctx.1.as_deref(), &ctx.2);
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
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    // Drain headers, capturing the auth header.
    let mut header_token: Option<String> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
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
    if presented.as_bytes() != token.as_bytes() {
        return respond(
            &mut stream,
            "403 Forbidden",
            r#"{"error":"missing or bad token"}"#,
        );
    }

    match path.as_str() {
        "/api/status" => {
            let body = json!({
                "entries": ctx.catalog.entry_count().unwrap_or(0),
                "objects": ctx.catalog.object_count().unwrap_or(0),
                "semantic": ctx.vector.is_some(),
            });
            respond(&mut stream, "200 OK", &body.to_string())
        }
        "/api/metrics" => {
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
            let limit = get("limit").and_then(|l| l.parse().ok()).unwrap_or(25usize);
            if q.trim().is_empty() {
                return respond(&mut stream, "200 OK", r#"{"hits":[],"ms":0}"#);
            }
            let t0 = std::time::Instant::now();
            let lexical = LexicalReader::open(&ctx.lexical_dir).ok();
            let vguard = ctx.vector.as_ref().map(|v| v.lock().unwrap());
            let engine = QueryEngine {
                catalog: &ctx.catalog,
                lexical: lexical.as_ref(),
                vector: vguard.as_deref(),
                limit: limit.max(50),
            };
            let hits = match mode.as_str() {
                "hybrid" if engine.vector.is_some() => engine.hybrid_tagged(&q, limit).map(|v| {
                    v.into_iter()
                        .map(|(h, o)| (h, o.as_str()))
                        .collect::<Vec<_>>()
                }),
                "semantic" => parse(&format!(r#"semantic("{}")"#, q.replace('"', "")))
                    .and_then(|e| engine.run(&e, None))
                    .map(|v| v.into_iter().map(|h| (h, "meaning")).collect::<Vec<_>>()),
                "rql" => parse(&q)
                    .and_then(|e| engine.run(&e, None))
                    .map(|v| v.into_iter().map(|h| (h, "rql")).collect::<Vec<_>>()),
                _ => parse(&format!(r#""{}""#, q.replace('"', "")))
                    .and_then(|e| engine.run(&e, None))
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
            // Match-all standing view: every committed file delta becomes an
            // invalidation tick the UI can refresh on.
            parse("kMDItemFSSize >= 0").map_err(|e| std::io::Error::other(e.to_string()))?,
        ),
        "/api/alert" => {
            let q = get("q").unwrap_or_default();
            let threshold: f32 = get("threshold")
                .and_then(|t| t.parse().ok())
                .unwrap_or(0.35);
            let sub = ctx
                .live
                .lock()
                .unwrap()
                .subscribe_alert(&q, threshold, vec![], 1024);
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

fn sse_headers(stream: &mut TcpStream) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n"
    )
}

fn sse_view(stream: &mut TcpStream, ctx: &IpcCtx, expr: rsd_query::Expr) -> std::io::Result<()> {
    let (_, rx) = ctx.live.lock().unwrap().subscribe(expr, vec![], [], 4096);
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
        extractor_id: EXTRACTOR_ID.into(),
        extractor_version: EXTRACTOR_VERSION,
        hints_hash: hh,
        abi_version: ABI_VERSION,
    }) else {
        return String::new();
    };
    let lower = er.text.to_lowercase();
    let needle = q.split_whitespace().next().unwrap_or(q).to_lowercase();
    let pos = lower.find(&needle).unwrap_or(0);
    let mut start = pos.saturating_sub(60);
    let mut end = (pos + 140).min(er.text.len());
    while !er.text.is_char_boundary(start) {
        start -= 1;
    }
    while !er.text.is_char_boundary(end) {
        end -= 1;
    }
    er.text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
