//! Real local embedder: bge-small-en-v1.5 (384-dim) through `fastembed` (ONNX Runtime).
//! Behind the `fastembed` cargo feature.
//!
//! fastembed is built without its `hf-hub` feature, so it cannot download anything: the model
//! files come from [`crate::models::ensure_embedder`] (pinned revision, SHA-256 checked) and are
//! loaded here from that directory.

use ssc_core::traits::Embedder;
use ssc_core::{Result, SscError};
use std::path::Path;
use std::sync::Mutex;

pub struct FastEmbedder {
    model: Mutex<fastembed::TextEmbedding>,
}

impl FastEmbedder {
    /// Loads the model from `dir`, the directory returned by
    /// [`ensure_embedder`](crate::models::ensure_embedder). Blocking (reads ~130 MB, builds the
    /// ONNX session); call it off the async runtime.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let read = |name: &str| {
            std::fs::read(dir.join(name))
                .map_err(|e| SscError::Unavailable(format!("embedding model file {name} in {}: {e}", dir.display())))
        };
        let tokenizer = fastembed::TokenizerFiles {
            tokenizer_file: read("tokenizer.json")?,
            config_file: read("config.json")?,
            special_tokens_map_file: read("special_tokens_map.json")?,
            tokenizer_config_file: read("tokenizer_config.json")?,
        };
        // Same post-processing fastembed applies to its built-in `BGESmallENV15`: CLS pooling,
        // then L2 normalisation (always applied by fastembed).
        let model = fastembed::UserDefinedEmbeddingModel::new(read("onnx/model.onnx")?, tokenizer)
            .with_pooling(fastembed::Pooling::Cls);
        let model = fastembed::TextEmbedding::try_new_from_user_defined(model, Default::default())
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
        model.embed(texts, None).map_err(|e| SscError::Unavailable(format!("embedding failed: {e}")))
    }
}
