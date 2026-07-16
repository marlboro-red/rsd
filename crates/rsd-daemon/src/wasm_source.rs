//! The WASM plugin content source (P7.2): files whose extension a loaded
//! plugin declared route here. Wraps `rsd_wasm::PluginHost`; each extraction
//! runs in a fresh fuel-metered, memory-capped, import-free instance.

use crate::dispatch::ContentSource;
use rsd_caes::{ExtractStatus, ExtractionRecord};
use rsd_extract::{Budgets, ExtractHints};
use rsd_wasm::{PluginHost, WasmStatus};
use std::path::Path;

pub struct WasmExtractor {
    host: PluginHost,
}

impl WasmExtractor {
    pub fn new(host: PluginHost) -> WasmExtractor {
        WasmExtractor { host }
    }

    pub fn plugin_count(&self) -> usize {
        self.host.plugin_count()
    }

    fn ext(name: &str) -> String {
        Path::new(name)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }
}

impl ContentSource for WasmExtractor {
    fn extract_file(
        &mut self,
        path: &Path,
        hints: &ExtractHints,
        budgets: &Budgets,
    ) -> Result<ExtractionRecord, String> {
        let ext = Self::ext(&hints.name);
        let mut bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > budgets.max_input_bytes {
            bytes.truncate(budgets.max_input_bytes as usize);
        }
        let ex = self
            .host
            .extract(&ext, &bytes)
            .ok_or_else(|| format!("no plugin for .{ext}"))?
            .map_err(|e| e.to_string())?;
        let status = match ex.status {
            WasmStatus::Complete => ExtractStatus::Complete,
            WasmStatus::Partial => ExtractStatus::Partial,
            WasmStatus::Unsupported => ExtractStatus::Unsupported,
        };
        Ok(ExtractionRecord {
            status,
            text: ex.text,
            attrs: vec![("rsd.plugin".into(), ext)],
            symbols: vec![],
        })
    }

    fn handles(&self, name: &str) -> bool {
        self.host.handles(&Self::ext(name))
    }

    fn processor_tag(&self) -> &str {
        "wasm"
    }
}
