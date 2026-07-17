//! The OCR content source (P7.1): images route here instead of the text/PDF
//! sealed worker. Shells to the `rsd-ocr` Vision helper (a separate process —
//! its own boundary), which reads the image and returns recognized text. The
//! result is a normal `ExtractionRecord`, so CAES/lexical/vector/live pick it
//! up exactly as they do PDF text.

use crate::dispatch::{ContentSource, ProcessorKey};
use rsd_caes::{ExtractStatus, ExtractionRecord};
use rsd_extract::{Budgets, ExtractHints};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct OcrExtractor {
    helper: PathBuf,
}

impl OcrExtractor {
    /// Point at a specific helper binary (tests, custom installs).
    pub fn at(helper: PathBuf) -> OcrExtractor {
        OcrExtractor { helper }
    }

    /// Locate the `rsd-ocr` helper next to the current executable (bundled in
    /// RSD.app / dist), or on PATH. Returns None if unavailable — OCR is then
    /// simply disabled and images stay unindexed-by-policy.
    pub fn discover() -> Option<OcrExtractor> {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let cand = dir.join("rsd-ocr");
                if cand.exists() {
                    return Some(OcrExtractor { helper: cand });
                }
            }
        }
        // Fall back to PATH.
        if Command::new("rsd-ocr").arg("--help").output().is_ok() {
            return Some(OcrExtractor {
                helper: PathBuf::from("rsd-ocr"),
            });
        }
        None
    }
}

impl ContentSource for OcrExtractor {
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
            .output()
            .map_err(|e| format!("rsd-ocr spawn: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "rsd-ocr exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let mut attrs = vec![("rsd.ocr".into(), "vision".into())];
        let status = if text.is_empty() {
            // A valid image with no legible text is a labeled state, not a
            // failure — it just has no content to index.
            attrs.push(("rsd.ocr_result".into(), "no-text".into()));
            ExtractStatus::Complete
        } else {
            ExtractStatus::Complete
        };
        Ok(ExtractionRecord {
            status,
            text,
            attrs,
            symbols: vec![],
        })
    }

    fn handles(&self, name: &str) -> bool {
        rsd_extract::is_image(name)
    }

    fn processor_key(&self, _name: &str) -> ProcessorKey {
        ProcessorKey {
            extractor_id: "rsd.ocr.vision".into(),
            extractor_version: 1,
            hints_tag: "recognition-level=accurate;languages=automatic".into(),
        }
    }
}
