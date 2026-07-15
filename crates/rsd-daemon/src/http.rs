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
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

pub fn start_http(
    port: u16,
    ctx: IpcCtx,
    caes: Option<Arc<rsd_caes::Store>>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let ctx = Arc::new((ctx, caes));
    std::thread::Builder::new()
        .name("rsd-http".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                let ctx = ctx.clone();
                let _ = std::thread::Builder::new()
                    .name("rsd-http-conn".into())
                    .spawn(move || {
                        let _ = serve(stream, &ctx.0, ctx.1.as_deref());
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
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn serve(
    mut stream: TcpStream,
    ctx: &IpcCtx,
    caes: Option<&rsd_caes::Store>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    // Drain headers.
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
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

    match path.as_str() {
        "/api/status" => {
            let body = json!({
                "entries": ctx.catalog.entry_count().unwrap_or(0),
                "objects": ctx.catalog.object_count().unwrap_or(0),
                "semantic": ctx.vector.is_some(),
            });
            respond(&mut stream, "200 OK", &body.to_string())
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
                "hybrid" if engine.vector.is_some() => engine.hybrid(&q, limit),
                "semantic" => parse(&format!(r#"semantic("{}")"#, q.replace('"', "")))
                    .and_then(|e| engine.run(&e, None)),
                "rql" => parse(&q).and_then(|e| engine.run(&e, None)),
                _ => parse(&format!(r#""{}""#, q.replace('"', "")))
                    .and_then(|e| engine.run(&e, None)),
            };
            match hits {
                Ok(hits) => {
                    let items: Vec<serde_json::Value> = hits
                        .iter()
                        .take(limit)
                        .map(|h| {
                            json!({
                                "path": h.path,
                                "snippet": snippet(ctx, caes, h.oid, &q),
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
        _ => respond(&mut stream, "404 Not Found", "{}"),
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
