//! Locating (and fetching once) the local whisper model. The downloader itself lives in
//! `ssc_memory::download` (pinned revision, SHA-256 check, resume, atomic rename).
//!
//! Default: `ggml-large-v3-turbo.bin` (1.6 GB), the most accurate whisper.cpp model that still
//! runs near real time on Apple Silicon. Neither the spec nor the Build Sheet names a size; this
//! follows "best available model, no silent downgrades".

use ssc_core::Result;
use ssc_memory::download::{self, ModelFile, ModelSpec, ProgressFn};
use std::path::PathBuf;

/// File name of the default model inside its directory.
pub const WHISPER_FILE: &str = "ggml-large-v3-turbo.bin";

/// `ggerganov/whisper.cpp` at a pinned commit. The SHA-256 is the file's Git LFS object id at
/// that commit, which by definition is the SHA-256 of the content; `ensure_whisper` checks it.
pub const WHISPER_LARGE_V3_TURBO: ModelSpec = ModelSpec {
    repo: "ggerganov/whisper.cpp",
    revision: "5359861c739e955e79d9a303bcbc70fb988958b1",
    dir: "whisper-large-v3-turbo@5359861c",
    files: &[ModelFile {
        path: WHISPER_FILE,
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
        size: 1_624_555_275,
    }],
};

/// Downloads (if needed) and verifies the whisper model into `~/.ssc/models/...`; returns the
/// model file path for `WhisperConfig::new` (feature `local-stt`).
pub async fn ensure_whisper(progress: ProgressFn<'_>) -> Result<PathBuf> {
    let dir =
        download::ensure(download::HF_BASE_URL, &download::models_dir(), &WHISPER_LARGE_V3_TURBO, progress).await?;
    Ok(dir.join(WHISPER_FILE))
}

/// The model file if it is already in place, without touching the network.
pub fn whisper_path_if_present() -> Option<PathBuf> {
    let root = download::models_dir();
    download::is_present(&root, &WHISPER_LARGE_V3_TURBO)
        .then(|| download::spec_dir(&root, &WHISPER_LARGE_V3_TURBO).join(WHISPER_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_is_pinned() {
        let s = &WHISPER_LARGE_V3_TURBO;
        assert_eq!(s.revision.len(), 40, "revision must be a full commit hash");
        assert!(s.dir.ends_with(&s.revision[..8]));
        for f in s.files {
            assert_eq!(f.sha256.len(), 64);
            assert!(f.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        }
    }
}
