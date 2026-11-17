//! Pinned local models for the memory store, fetched by [`crate::download`].

use crate::download::{self, ModelFile, ModelSpec, ProgressFn};
use ssc_core::Result;
use std::path::PathBuf;

/// bge-small-en-v1.5 (384-dim) as ONNX, the same files fastembed's `BGESmallENV15` uses, pinned
/// to one commit. Hashes computed from a one-off download; `onnx/model.onnx` also matches the
/// repo's Git LFS object id.
pub const BGE_SMALL_EN_V15: ModelSpec = ModelSpec {
    repo: "Xenova/bge-small-en-v1.5",
    revision: "ea104dacec62c0de699686887e3f920caeb4f3e3",
    dir: "bge-small-en-v1.5@ea104dac",
    files: &[
        ModelFile {
            path: "config.json",
            sha256: "fa73f90bf92c8cace1fbcb709626306f2bdbc9ea3e5b5f94b440df9b6aa56350",
            size: 683,
        },
        ModelFile {
            path: "special_tokens_map.json",
            sha256: "b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3",
            size: 125,
        },
        ModelFile {
            path: "tokenizer.json",
            sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
            size: 711_396,
        },
        ModelFile {
            path: "tokenizer_config.json",
            sha256: "9261e7d79b44c8195c1cada2b453e55b00aeb81e907a6664974b4d7776172ab3",
            size: 366,
        },
        ModelFile {
            path: "onnx/model.onnx",
            sha256: "828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35",
            size: 133_093_490,
        },
    ],
};

/// Downloads (if needed) and verifies the embedder into `~/.ssc/models/bge-small-en-v1.5@ea104dac`.
/// Returns that directory, ready for `embed::FastEmbedder::from_dir` (feature `fastembed`).
pub async fn ensure_embedder(progress: ProgressFn<'_>) -> Result<PathBuf> {
    download::ensure(download::HF_BASE_URL, &download::models_dir(), &BGE_SMALL_EN_V15, progress).await
}

/// The embedder directory if it is already downloaded and in place, without touching the network.
pub fn embedder_dir_if_present() -> Option<PathBuf> {
    let root = download::models_dir();
    download::is_present(&root, &BGE_SMALL_EN_V15).then(|| download::spec_dir(&root, &BGE_SMALL_EN_V15))
}
