//! Real local embedder: bge-small-en-v1.5 (384-dim) through `fastembed` (ONNX Runtime).
//! Behind the `fastembed` cargo feature; the model is downloaded to `cache_dir` on first use.

use ssc_core::traits::Embedder;
use ssc_core::{Result, SscError};
use std::path::PathBuf;
use std::sync::Mutex;

pub struct FastEmbedder {
    model: Mutex<fastembed::TextEmbedding>,
}

impl FastEmbedder {
    /// `cache_dir`: where the ONNX model lives (e.g. `~/.ssc/models`). `None` = fastembed's default.
    pub fn new(cache_dir: Option<PathBuf>) -> Result<Self> {
        let mut opts =
            fastembed::InitOptions::new(fastembed::EmbeddingModel::BGESmallENV15).with_show_download_progress(false);
        if let Some(dir) = cache_dir {
            opts = opts.with_cache_dir(dir);
        }
        let model = fastembed::TextEmbedding::try_new(opts)
            .map_err(|e| SscError::Unavailable(format!("embedding model: {e}")))?;
        Ok(Self { model: Mutex::new(model) })
    }
}

impl Embedder for FastEmbedder {
    fn dim(&self) -> usize {
        384
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut model = self.model.lock().unwrap_or_else(|p| p.into_inner());
        model.embed(texts.to_vec(), None).map_err(|e| SscError::Unavailable(format!("embedding failed: {e}")))
    }
}
