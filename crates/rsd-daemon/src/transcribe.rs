//! The A/V transcription content source (P7.1 media): audio and video route
//! here, and what was *said* becomes searchable text.
//!
//! Shells to the `rsd-transcribe` helper (whisper.cpp via whisper-rs in a
//! separate process — heavy model isolated from the daemon, Metal-accelerated,
//! no authorization prompts and works headless, which is why we use whisper
//! rather than Apple's Speech framework).
//!
//! **Opt-in by design** (DESIGN.md §8/P5: A/V transcription is per-scope
//! opt-in, lowest priority): disabled unless `RSD_TRANSCRIBE=1`, and requires
//! both the helper and a fetched whisper model. Otherwise media files stay
//! unindexed-by-policy rather than silently burning CPU on a music library.

use crate::dispatch::{ContentSource, ProcessorKey};
use rsd_caes::{ExtractStatus, ExtractionRecord};
use rsd_extract::{Budgets, ExtractHints};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct TranscribeExtractor {
    helper: PathBuf,
    model: PathBuf,
    model_revision: String,
}

impl TranscribeExtractor {
    fn model_revision(model: &Path) -> String {
        let metadata = std::fs::metadata(model).ok();
        let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = metadata
            .and_then(|m| m.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        format!("{}:{size}:{modified}", model.display())
    }

    /// Default model location, matching scripts/fetch-model.sh.
    pub fn default_model() -> PathBuf {
        if let Ok(p) = std::env::var("RSD_WHISPER_MODEL") {
            return p.into();
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".cache/rsd/models/whisper/ggml-base.en.bin")
    }

    /// Opt-in + helper + model must all be present.
    pub fn discover() -> Option<TranscribeExtractor> {
        if std::env::var("RSD_TRANSCRIBE").ok().as_deref() != Some("1") {
            return None;
        }
        let helper = std::env::var_os("RSD_TRANSCRIBE_BIN")
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|e| e.parent().map(|d| d.join("rsd-transcribe")))
                    .filter(|p| p.exists())
            })?;
        let model = Self::default_model();
        if !model.exists() {
            return None;
        }
        let model_revision = Self::model_revision(&model);
        Some(TranscribeExtractor {
            helper,
            model,
            model_revision,
        })
    }

    pub fn at(helper: PathBuf, model: PathBuf) -> TranscribeExtractor {
        let model_revision = Self::model_revision(&model);
        TranscribeExtractor {
            helper,
            model,
            model_revision,
        }
    }
}

impl ContentSource for TranscribeExtractor {
    fn extract_file(
        &mut self,
        file: &std::fs::File,
        _path: &Path,
        _hints: &ExtractHints,
        _budgets: &Budgets,
    ) -> Result<ExtractionRecord, String> {
        let out = Command::new(&self.helper)
            .arg("/dev/stdin")
            .stdin(Stdio::from(
                file.try_clone().map_err(|error| error.to_string())?,
            ))
            .arg("--model")
            .arg(&self.model)
            .output()
            .map_err(|e| format!("rsd-transcribe spawn: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "rsd-transcribe exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .last()
                    .unwrap_or("")
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let mut attrs = vec![("rsd.transcribed".into(), "whisper".into())];
        if text.is_empty() {
            attrs.push(("rsd.transcript".into(), "no-speech".into()));
        }
        Ok(ExtractionRecord {
            status: ExtractStatus::Complete,
            text,
            attrs,
            symbols: vec![],
        })
    }

    fn handles(&self, name: &str) -> bool {
        rsd_extract::is_media(name)
    }

    fn processor_key(&self, _name: &str) -> ProcessorKey {
        ProcessorKey {
            extractor_id: "rsd.transcribe.whisper".into(),
            extractor_version: 1,
            hints_tag: format!("model={}", self.model_revision),
        }
    }
}
