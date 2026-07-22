//! Smoke binary: `rsd-daemon watch <root> [--state <dir>]` — recover the
//! journal, bootstrap, keep the catalog live-converged against FSEvents, print
//! a stats line.
//!
//! State (catalog + journal) lives OUTSIDE the watched root by default —
//! state inside the root would generate events about its own writes and feed
//! back into the pipeline forever.

use rsd_catalog::Durability;
use rsd_daemon::{bring_up, PipelineConfig};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn usage() -> ! {
    eprintln!("usage: rsd-daemon watch <root> [--state <dir>]");
    std::process::exit(2);
}

fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root = None;
    let mut state = None;
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("watch") => {}
        _ => usage(),
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "--state" => {
                state = Some(std::path::PathBuf::from(
                    it.next().unwrap_or_else(|| usage()),
                ))
            }
            _ if root.is_none() => root = Some(std::path::PathBuf::from(a)),
            _ => usage(),
        }
    }
    let root = root.unwrap_or_else(|| usage()).canonicalize()?;
    let state = state.unwrap_or_else(|| {
        // Sibling of the root: `<parent>/.rsd-state-<rootname>` — never inside.
        let name = root.file_name().map(|s| s.to_string_lossy().into_owned());
        root.parent().unwrap_or(&root).join(format!(
            ".rsd-state-{}",
            name.unwrap_or_else(|| "root".into())
        ))
    });
    if state.starts_with(&root) && !rsd_ingest::excluded(&state) {
        eprintln!(
            "error: state dir {state:?} must live outside the watched root (or under an excluded dir like ~/Library)"
        );
        std::process::exit(2);
    }
    std::fs::create_dir_all(&state)?;

    // Before anything expensive: a state dir too deep to hold a bound socket
    // path would otherwise fail only after a full bootstrap walk.
    rsd_daemon::ipc::check_sock_path(&state.join("rsd.sock"))?;

    let (catalog, catalog_rebuilt) =
        rsd_daemon::open_catalog_resilient(&state.join("catalog.redb"), Durability::Eventual)?;
    if catalog_rebuilt {
        eprintln!("catalog: rebuilding projection from journal");
    }

    // Content indexing: sealed worker pool, if the worker binary is present.
    let (content, lexical) = match rsd_worker::WorkerPool::new(rsd_worker::PoolConfig::default()) {
        Ok(pool) => {
            let caes = Arc::new(
                rsd_caes::Store::open(&state.join("caes.redb"))
                    .map_err(|e| std::io::Error::other(e.to_string()))?,
            );
            let (plane, lexical_rebuilt) =
                rsd_daemon::open_lexical_resilient(&state.join("lexical"))?;
            if lexical_rebuilt {
                eprintln!("lexical: rebuilding projection from journal + CAES");
            }
            let mut indexer = rsd_daemon::ContentIndexer::new(
                Box::new(rsd_daemon::PooledExtractor(pool)),
                caes.clone(),
            );
            match rsd_daemon::ocr::OcrExtractor::discover() {
                Some(ocr) => {
                    eprintln!("ocr: Vision text recognition enabled");
                    indexer = indexer.with_ocr(Box::new(ocr));
                }
                None => eprintln!("ocr: disabled (rsd-ocr helper not found)"),
            }
            match rsd_daemon::transcribe::TranscribeExtractor::discover() {
                Some(t) => {
                    eprintln!("media: A/V transcription enabled (whisper)");
                    indexer = indexer.with_media(Box::new(t));
                }
                None => eprintln!("media: transcription off (set RSD_TRANSCRIBE=1 + fetch model)"),
            }
            // WASM extractor plugins from <state>/plugins/*.wasm.
            match rsd_wasm::PluginHost::new() {
                Ok(mut host) => match host.load_dir(&state.join("plugins")) {
                    Ok(n) if n > 0 => {
                        eprintln!("wasm: {n} extractor plugin(s) loaded");
                        indexer = indexer
                            .with_wasm(Box::new(rsd_daemon::wasm_source::WasmExtractor::new(host)));
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("wasm: plugin load failed: {e}"),
                },
                Err(e) => eprintln!("wasm: host unavailable: {e}"),
            }
            (Some(indexer), Some((plane, caes)))
        }
        Err(e) => {
            eprintln!("content indexing disabled (worker pool unavailable: {e})");
            (None, None)
        }
    };

    // Semantic plane (P6): learned embedder (MiniLM via candle) when the
    // model is present, hash-projection fallback otherwise. Same trait, same
    // plane; the VectorPlane skips stale-embedder vectors automatically.
    // Embedder chain: ANE sidecar (evictable, Neural-Engine) > in-process
    // MiniLM (CPU) > hash-projection. Each is the same Embedder trait; the
    // vector plane re-embeds if the chosen embedder differs from stored tags.
    let embedder: Arc<dyn rsd_vector::Embedder> =
        if let Some(sidecar) = rsd_ml::SidecarEmbedder::discover() {
            eprintln!("semantic: ANE sidecar embedder ({})", rsd_ml::SIDECAR_ID);
            Arc::new(sidecar)
        } else {
            match rsd_ml::MiniLmEmbedder::load(&rsd_ml::MiniLmEmbedder::default_dir()) {
                Ok(m) => {
                    eprintln!("semantic: learned embedder ({}, CPU)", rsd_ml::MODEL_ID);
                    Arc::new(m)
                }
                Err(e) => {
                    eprintln!("semantic: hash-projection fallback ({e})");
                    Arc::new(rsd_vector::HashEmbedder::default())
                }
            }
        };
    let vector = lexical
        .as_ref()
        .map(|(_, caes)| -> std::io::Result<_> {
            let (plane, vector_rebuilt) =
                rsd_daemon::open_vector_resilient(&state.join("vector.redb"), embedder.clone())?;
            if vector_rebuilt {
                eprintln!("vector: rebuilding projection from journal + CAES");
            }
            Ok((Arc::new(std::sync::Mutex::new(plane)), caes.clone()))
        })
        .transpose()?;
    let vector_handle = vector.as_ref().map(|(p, _)| p.clone());
    let vector_handle_http = vector_handle.clone();
    let lexical_caes = lexical.as_ref().map(|(_, c)| c.clone());
    let caes_for_live = lexical.as_ref().map(|(_, c)| c.clone());
    let live = Arc::new(std::sync::Mutex::new({
        let mut eng = rsd_live::LiveEngine::new(caes_for_live);
        eng.set_embedder(embedder.clone());
        eng
    }));
    let live_http = live.clone();

    eprintln!("bootstrapping {}...", root.display());
    let t0 = std::time::Instant::now();
    let (pipeline, boot) = bring_up(
        catalog.clone(),
        &state.join("journal"),
        &root,
        content,
        lexical,
        vector,
        Some(live.clone()),
        PipelineConfig {
            trickle_bootstrap: true,
            ..Default::default()
        },
    )?;
    // Loopback secret: a random token in a 0600 file. The HTTP surface needs it
    // so no web page can read the index over 127.0.0.1; the IPC surface accepts
    // it as first-party authority so `rsdfind` and `rsd-mcp` can query the
    // running daemon instead of opening the single-writer store directly.
    // Regenerated each start.
    let token = rsd_daemon::http::gen_token()?;
    let token_path = state.join("http.token");
    rsd_daemon::http::write_token(&token_path, &token)?;

    let _ipc = rsd_daemon::ipc::start_ipc(
        &state.join("rsd.sock"),
        rsd_daemon::ipc::IpcCtx {
            catalog: catalog.clone(),
            lexical_dir: state.join("lexical"),
            vector: vector_handle,
            live,
            authz: Arc::new(rsd_daemon::ipc::AuthzStore::default()),
            caes: lexical_caes.clone(),
            first_party_token: Some(token.clone()),
        },
    )?;
    eprintln!("ipc listening at {}", state.join("rsd.sock").display());

    let caes_for_http = lexical_caes.clone();
    let _http = rsd_daemon::http::start_http(
        5871,
        token.clone(),
        rsd_ipc::Scope::Unrestricted,
        rsd_daemon::ipc::IpcCtx {
            catalog: catalog.clone(),
            lexical_dir: state.join("lexical"),
            vector: vector_handle_http,
            live: live_http,
            authz: Arc::new(rsd_daemon::ipc::AuthzStore::default()),
            caes: lexical_caes.clone(),
            first_party_token: Some(token.clone()),
        },
        caes_for_http,
    )?;
    eprintln!(
        "http api at http://127.0.0.1:5871 (token at {})",
        token_path.display()
    );
    let _ = (t0, boot);
    eprintln!("pipeline live; trickle bootstrap running in the background (ctrl-c to exit)");

    loop {
        std::thread::sleep(Duration::from_secs(5));
        let s = *pipeline
            .stats
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        eprintln!(
            "entries={} objects={} work_items={} commits={} full_rescans={} lstats={} removals={} bootstrap_dirs={} applier_down={}{}",
            catalog.entry_count().unwrap_or(0),
            catalog.object_count().unwrap_or(0),
            pipeline.counters.work_items.load(Ordering::Relaxed),
            pipeline.counters.commits.load(Ordering::Relaxed),
            pipeline.counters.full_rescans.load(Ordering::Relaxed),
            s.lstats,
            s.removals,
            pipeline.counters.bootstrap_dirs.load(Ordering::Relaxed),
            pipeline.applier_down.load(Ordering::Acquire),
            if pipeline.counters.bootstrap_done.load(Ordering::Relaxed) == 1 {
                " (done)"
            } else {
                ""
            },
        );
    }
}
