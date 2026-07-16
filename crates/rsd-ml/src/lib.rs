//! rsd-ml (P6.1): the learned embedder — all-MiniLM-L6-v2 via candle, behind
//! the same `Embedder` trait as the hash-projection fallback. 384-dim mean-
//! pooled sentence embeddings, L2 normalized. CPU inference (Metal/ANE are
//! device swaps behind this same seam); model memory lives in this crate so
//! processization (the evictable sidecar) is a transport change, not a
//! redesign.
//!
//! Model files (config.json, tokenizer.json, model.safetensors) load from a
//! directory — `scripts/fetch-model.sh` populates ~/.cache/rsd/models/minilm.

mod sidecar;
pub use sidecar::{SidecarEmbedder, SIDECAR_ID};

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use rsd_vector::Embedder;
use std::path::Path;
use std::sync::Mutex;
use tokenizers::Tokenizer;

#[derive(Debug, thiserror::Error)]
pub enum MlError {
    #[error("model load: {0}")]
    Load(String),
    #[error("inference: {0}")]
    Infer(String),
}

pub struct MiniLmEmbedder {
    // Mutex: candle inference is &self-safe, but the tokenizer pads state;
    // one lock keeps the whole path simple. The pool parallelism story lives
    // in the sidecar (future), not here.
    inner: Mutex<(BertModel, Tokenizer)>,
    device: Device,
}

pub const MODEL_ID: &str = "rsd.minilm-l6-v2";
pub const MODEL_DIM: usize = 384;
const MAX_TOKENS: usize = 256;

impl MiniLmEmbedder {
    /// Default model location: ~/.cache/rsd/models/minilm (or $RSD_MODEL_DIR).
    pub fn default_dir() -> std::path::PathBuf {
        if let Ok(d) = std::env::var("RSD_MODEL_DIR") {
            return d.into();
        }
        dirs_home()
            .join(".cache")
            .join("rsd")
            .join("models")
            .join("minilm")
    }

    pub fn load(dir: &Path) -> Result<MiniLmEmbedder, MlError> {
        let device = Device::Cpu;
        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json"))
                .map_err(|e| MlError::Load(format!("config.json: {e}")))?,
        )
        .map_err(|e| MlError::Load(format!("config parse: {e}")))?;
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[dir.join("model.safetensors")],
                DType::F32,
                &device,
            )
            .map_err(|e| MlError::Load(format!("safetensors: {e}")))?
        };
        let model =
            BertModel::load(vb, &config).map_err(|e| MlError::Load(format!("bert: {e}")))?;
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| MlError::Load(format!("tokenizer: {e}")))?;
        // The shipped tokenizer.json enables fixed-length padding; mean
        // pooling over [PAD] tokens crushes every embedding toward the same
        // vector. We pool over real tokens only.
        tokenizer.with_padding(None);
        let _ = tokenizer.with_truncation(None);
        tracing::info!("loaded {MODEL_ID} from {dir:?}");
        Ok(MiniLmEmbedder {
            inner: Mutex::new((model, tokenizer)),
            device,
        })
    }

    fn embed_inner(&self, text: &str) -> Result<Vec<f32>, MlError> {
        let guard = self.inner.lock().unwrap();
        let (model, tokenizer) = &*guard;
        let enc = tokenizer
            .encode(text, true)
            .map_err(|e| MlError::Infer(e.to_string()))?;
        let mut ids: Vec<u32> = enc.get_ids().to_vec();
        ids.truncate(MAX_TOKENS);
        let n = ids.len().max(1);
        let ids = Tensor::new(ids, &self.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| MlError::Infer(e.to_string()))?;
        let type_ids = ids
            .zeros_like()
            .map_err(|e| MlError::Infer(e.to_string()))?;
        let hidden = model
            .forward(&ids, &type_ids, None)
            .map_err(|e| MlError::Infer(e.to_string()))?; // (1, seq, 384)
                                                          // Mean pooling over the sequence, then L2 normalize.
        let pooled = hidden
            .sum(1)
            .and_then(|t| t / (n as f64))
            .and_then(|t| t.squeeze(0))
            .map_err(|e| MlError::Infer(e.to_string()))?;
        let v: Vec<f32> = pooled
            .to_vec1()
            .map_err(|e| MlError::Infer(e.to_string()))?;
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        Ok(if norm > 0.0 {
            v.into_iter().map(|x| x / norm).collect()
        } else {
            v
        })
    }
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(Into::into)
        .unwrap_or_else(|| "/tmp".into())
}

impl Embedder for MiniLmEmbedder {
    fn id(&self) -> &str {
        MODEL_ID
    }
    fn version(&self) -> u32 {
        1
    }
    fn dim(&self) -> usize {
        MODEL_DIM
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        self.embed_inner(text).unwrap_or_else(|e| {
            tracing::warn!("embedding failed ({e}); zero vector");
            vec![0.0; MODEL_DIM]
        })
    }
}
