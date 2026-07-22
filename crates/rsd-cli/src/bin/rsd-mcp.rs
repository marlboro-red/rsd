//! rsd-mcp (P7.3): the agent surface — an MCP stdio server exposing the index
//! to local AI agents. JSON-RPC 2.0 over stdin/stdout.
//!
//! Tools: rsd_search (lexical | semantic | hybrid | rql), rsd_snippets
//! (grounded excerpts with byte offsets — agents cite, not guess).
//!
//!   rsd-mcp --state <dir> (--scope <path>... | --unrestricted)
//!
//! Every request is served by the running daemon over `<state>/rsd.sock`. The
//! catalog is a single-writer store, so opening it here would fail outright
//! whenever the daemon is up — which is exactly when an agent wants to search.
//!
//! `--scope` is sent to the daemon as `Hello.restrict_to`, so the daemon
//! intersects it with what this client is entitled to and enforces the result
//! during candidate generation. The restriction is server-side; a bug in this
//! process cannot widen it.

use rsd_ipc::{recv, send, Request, Response, Snippet};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;

struct State {
    stream: UnixStream,
}

const MAX_TOOL_RESULTS: usize = 1_000;

#[derive(Debug, PartialEq, Eq)]
struct Config {
    state_dir: std::path::PathBuf,
    grants: Option<Vec<std::path::PathBuf>>,
}

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut state_dir = None;
    let mut scopes = Vec::new();
    let mut unrestricted = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state" => {
                i += 1;
                state_dir = args.get(i).map(std::path::PathBuf::from);
                if state_dir.is_none() {
                    return Err("--state requires a directory".into());
                }
            }
            "--scope" => {
                i += 1;
                let Some(scope) = args.get(i) else {
                    return Err("--scope requires a path".into());
                };
                scopes.push(std::path::PathBuf::from(scope));
            }
            "--unrestricted" => unrestricted = true,
            other => return Err(format!("unknown argument {other}")),
        }
        i += 1;
    }
    let state_dir = state_dir.ok_or("--state is required")?;
    let grants = match (unrestricted, scopes.is_empty()) {
        (true, true) => None,
        (false, false) => Some(scopes),
        (true, false) => return Err("--scope and --unrestricted are mutually exclusive".into()),
        (false, true) => {
            return Err("explicit authority required: pass --scope PATH or --unrestricted".into())
        }
    };
    Ok(Config { state_dir, grants })
}

/// Connect to the daemon and complete the Hello handshake, presenting the
/// loopback token as first-party authority and `--scope` as a self-restriction.
fn connect(config: &Config) -> Result<UnixStream, String> {
    let sock = config.state_dir.join("rsd.sock");
    let mut stream = UnixStream::connect(&sock).map_err(|e| {
        format!(
            "cannot reach daemon at {}: {e}\nstart it with `rsd-daemon watch <root> --state {}`",
            sock.display(),
            config.state_dir.display()
        )
    })?;
    let token = std::fs::read_to_string(config.state_dir.join("http.token"))
        .ok()
        .map(|t| t.trim().to_string());
    let restrict_to = config.grants.as_ref().map(|grants| {
        grants
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    });
    send(
        &mut stream,
        &Request::Hello {
            principal: "rsd-mcp".into(),
            token,
            restrict_to,
        },
    )
    .map_err(|e| e.to_string())?;
    match recv::<Response>(&mut stream) {
        Ok(Response::Hello { .. }) => Ok(stream),
        Ok(Response::Err(e)) => Err(e),
        Ok(_) => Err("unexpected response to Hello".into()),
        Err(e) => Err(e.to_string()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match parse_args(&args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("rsd-mcp: {error}");
            eprintln!("usage: rsd-mcp --state <dir> (--scope <path>... | --unrestricted)");
            std::process::exit(2);
        }
    };
    let mut st = match connect(&config) {
        Ok(stream) => State { stream },
        Err(error) => {
            eprintln!("rsd-mcp: {error}");
            std::process::exit(1);
        }
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
                let limit = a
                    .get("limit")
                    .and_then(|l| l.as_u64())
                    .unwrap_or(10)
                    .clamp(1, MAX_TOOL_RESULTS as u64) as usize;
                let kind = a.get("kind").and_then(|k| k.as_str()).unwrap_or("hybrid");
                let out = match name {
                    "rsd_search" => run_search(&mut st, &query, kind, limit),
                    "rsd_snippets" => run_snippets(&mut st, &query, limit),
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

/// One request/response round trip on the daemon connection.
fn call(st: &mut State, request: Request) -> Result<Response, String> {
    send(&mut st.stream, &request).map_err(|e| e.to_string())?;
    match recv::<Response>(&mut st.stream) {
        Ok(Response::Err(e)) => Err(e),
        Ok(response) => Ok(response),
        Err(e) => Err(e.to_string()),
    }
}

fn hits_from(response: Response) -> Result<Vec<rsd_ipc::Hit>, String> {
    match response {
        Response::Hits(hits) => Ok(hits),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

fn snippets_from(response: Response) -> Result<Vec<Snippet>, String> {
    match response {
        Response::Snippets(snippets) => Ok(snippets),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

fn run_search(st: &mut State, query: &str, kind: &str, limit: usize) -> Result<String, String> {
    let limit = limit.clamp(1, MAX_TOOL_RESULTS);
    // The daemon owns retrieval and authorization; this builds the request and
    // renders the answer. `rql` passes the agent's string through verbatim,
    // which is why every other kind quotes the query into a literal.
    let request = match kind {
        "hybrid" => Request::Hybrid {
            query: query.into(),
            scope: None,
            limit: limit as u32,
        },
        "semantic" => Request::Query {
            rql: format!(r#"semantic("{}")"#, query.replace('"', "")),
            scope: None,
            count_only: false,
        },
        "rql" => Request::Query {
            rql: query.into(),
            scope: None,
            count_only: false,
        },
        _ => Request::Query {
            rql: format!(r#""{}""#, query.replace('"', "")),
            scope: None,
            count_only: false,
        },
    };
    let hits = hits_from(call(st, request)?)?;
    let lines: Vec<String> = hits.iter().take(limit).map(|h| h.path.clone()).collect();
    Ok(if lines.is_empty() {
        "no results".into()
    } else {
        lines.join("\n")
    })
}

fn run_snippets(st: &mut State, query: &str, limit: usize) -> Result<String, String> {
    let limit = limit.clamp(1, MAX_TOOL_RESULTS);
    let snippets = snippets_from(call(
        st,
        Request::Snippets {
            query: query.into(),
            scope: None,
            limit: limit as u32,
        },
    )?)?;
    let mut out = String::new();
    for snippet in snippets.iter().take(limit) {
        out.push_str(&format!(
            "{} [bytes {}..{}]\n  {}\n",
            snippet.path, snippet.start, snippet.end, snippet.text
        ));
    }
    Ok(if out.is_empty() {
        "no results".into()
    } else {
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn authority_is_explicit_and_modes_are_mutually_exclusive() {
        assert!(parse_args(&args(&["--state", "/tmp/state"])).is_err());
        assert!(parse_args(&args(&[
            "--state",
            "/tmp/state",
            "--scope",
            "/allowed",
            "--unrestricted",
        ]))
        .is_err());
        assert_eq!(
            parse_args(&args(&[
                "--state",
                "/tmp/state",
                "--scope",
                "/allowed/a",
                "--scope",
                "/allowed/b",
            ]))
            .unwrap()
            .grants
            .unwrap()
            .len(),
            2
        );
        assert_eq!(
            parse_args(&args(&["--state", "/tmp/state", "--unrestricted"]))
                .unwrap()
                .grants,
            None
        );
    }
}
