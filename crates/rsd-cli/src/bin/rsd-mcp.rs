//! rsd-mcp (P7.3): the agent surface — an MCP stdio server exposing the index
//! to local AI agents. JSON-RPC 2.0 over stdin/stdout.
//!
//! Tools: rsd_search (lexical | semantic | hybrid | rql), rsd_snippets
//! (grounded excerpts with byte offsets — agents cite, not guess).
//!
//!   rsd-mcp --state <dir>

use rsd_caes::{CaesKey, ABI_VERSION};
use rsd_catalog::{Catalog, Durability};
use rsd_extract::{EXTRACTOR_ID, EXTRACTOR_VERSION};
use rsd_lexical::LexicalReader;
use rsd_query::{parse, QueryEngine};
use rsd_vector::VectorPlane;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

struct State {
    catalog: Catalog,
    lexical: Option<LexicalReader>,
    vector: Option<VectorPlane>,
    caes: Option<rsd_caes::Store>,
}

fn rsd_ml_or_hash() -> std::sync::Arc<dyn rsd_vector::Embedder> {
    match rsd_ml::MiniLmEmbedder::load(&rsd_ml::MiniLmEmbedder::default_dir()) {
        Ok(m) => std::sync::Arc::new(m),
        Err(_) => std::sync::Arc::new(rsd_vector::HashEmbedder::default()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let state_dir = match args.as_slice() {
        [flag, dir] if flag == "--state" => std::path::PathBuf::from(dir),
        _ => {
            eprintln!("usage: rsd-mcp --state <dir>");
            std::process::exit(2);
        }
    };
    let st = State {
        catalog: Catalog::open_with_durability(
            &state_dir.join("catalog.redb"),
            Durability::Eventual,
        )
        .expect("catalog"),
        lexical: LexicalReader::open(&state_dir.join("lexical")).ok(),
        vector: VectorPlane::open(&state_dir.join("vector.redb"), rsd_ml_or_hash()).ok(),
        caes: rsd_caes::Store::open(&state_dir.join("caes.redb")).ok(),
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let resp = match method {
            "initialize" => Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "rsd", "version": "0.1.0"}
            })),
            "notifications/initialized" => None,
            "tools/list" => Some(json!({"tools": [
                {"name": "rsd_search",
                 "description": "Search the local file index. kind: lexical (exact words), semantic (by meaning), hybrid (fused, best default), rql (raw RQL predicate).",
                 "inputSchema": {"type": "object", "properties": {
                     "query": {"type": "string"},
                     "kind": {"type": "string", "enum": ["lexical", "semantic", "hybrid", "rql"]},
                     "limit": {"type": "integer"}},
                  "required": ["query"]}},
                {"name": "rsd_snippets",
                 "description": "Search and return grounded text excerpts (with byte offsets) from the matching files, for citation.",
                 "inputSchema": {"type": "object", "properties": {
                     "query": {"type": "string"},
                     "limit": {"type": "integer"}},
                  "required": ["query"]}}
            ]})),
            "tools/call" => {
                let p = req.get("params").cloned().unwrap_or_default();
                let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let a = p.get("arguments").cloned().unwrap_or_default();
                let query = a
                    .get("query")
                    .and_then(|q| q.as_str())
                    .unwrap_or("")
                    .to_string();
                let limit = a.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
                let kind = a.get("kind").and_then(|k| k.as_str()).unwrap_or("hybrid");
                let out = match name {
                    "rsd_search" => run_search(&st, &query, kind, limit),
                    "rsd_snippets" => run_snippets(&st, &query, limit),
                    _ => Err(format!("unknown tool {name}")),
                };
                Some(match out {
                    Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
                    Err(e) => {
                        json!({"content": [{"type": "text", "text": format!("error: {e}")}], "isError": true})
                    }
                })
            }
            _ => id.as_ref().map(|_| json!({})),
        };
        if let (Some(id), Some(result)) = (id, resp) {
            let msg = json!({"jsonrpc": "2.0", "id": id, "result": result});
            let mut out = stdout.lock();
            let _ = writeln!(out, "{msg}");
            let _ = out.flush();
        }
    }
}

fn engine<'a>(st: &'a State) -> QueryEngine<'a> {
    QueryEngine {
        catalog: &st.catalog,
        lexical: st.lexical.as_ref(),
        vector: st.vector.as_ref(),
        limit: 1000,
    }
}

fn run_search(st: &State, query: &str, kind: &str, limit: usize) -> Result<String, String> {
    let hits = match kind {
        "hybrid" => engine(st).hybrid(query, limit).map_err(|e| e.to_string())?,
        "semantic" => {
            let expr = parse(&format!(r#"semantic("{query}")"#)).map_err(|e| e.to_string())?;
            engine(st).run(&expr, None).map_err(|e| e.to_string())?
        }
        "rql" => {
            let expr = parse(query).map_err(|e| e.to_string())?;
            engine(st).run(&expr, None).map_err(|e| e.to_string())?
        }
        _ => {
            let expr = parse(&format!(r#""{query}""#)).map_err(|e| e.to_string())?;
            engine(st).run(&expr, None).map_err(|e| e.to_string())?
        }
    };
    let lines: Vec<String> = hits.iter().take(limit).map(|h| h.path.clone()).collect();
    Ok(if lines.is_empty() {
        "no results".into()
    } else {
        lines.join("\n")
    })
}

fn run_snippets(st: &State, query: &str, limit: usize) -> Result<String, String> {
    let hits = engine(st).hybrid(query, limit).map_err(|e| e.to_string())?;
    let caes = st.caes.as_ref().ok_or("no CAES store")?;
    let mut out = String::new();
    for h in hits.iter().take(limit) {
        let Ok(Some(rec)) = st.catalog.get_object(h.oid) else {
            continue;
        };
        let (Some(ch), Some(hh)) = (rec.content_hash, rec.caes_hints_hash) else {
            continue;
        };
        let Ok(Some(er)) = caes.get(&CaesKey {
            content_hash: ch,
            extractor_id: EXTRACTOR_ID.into(),
            extractor_version: EXTRACTOR_VERSION,
            hints_hash: hh,
            abi_version: ABI_VERSION,
        }) else {
            continue;
        };
        // Excerpt around the first query-term occurrence (grounded citation).
        let lower = er.text.to_lowercase();
        let needle = query
            .split_whitespace()
            .next()
            .unwrap_or(query)
            .to_lowercase();
        let pos = lower.find(&needle).unwrap_or(0);
        let start = er.text[..pos]
            .char_indices()
            .rev()
            .nth(80)
            .map(|(i, _)| i)
            .unwrap_or(0);
        let end = (pos + 160).min(er.text.len());
        let mut end_c = end;
        while !er.text.is_char_boundary(end_c) {
            end_c -= 1;
        }
        let mut start_c = start;
        while !er.text.is_char_boundary(start_c) {
            start_c += 1;
        }
        out.push_str(&format!(
            "{} [bytes {start_c}..{end_c}]\n  {}\n",
            h.path,
            er.text[start_c..end_c].replace('\n', " ")
        ));
    }
    Ok(if out.is_empty() {
        "no results".into()
    } else {
        out
    })
}
