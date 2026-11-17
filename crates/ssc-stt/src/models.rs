//! Locating (and fetching once) the local whisper model. The downloader itself lives in
//! `ssc_memory::download` (pinned revision, SHA-256 check, resume, atomic rename).
//!
//! Default: `ggml-small.en.bin` (488 MB), user decision 2026-11-17. Whisper is the offline /
//! local-only path (cloud mode uses Deepgram), so end-of-speech latency and leaving GPU/memory
//! headroom for the local answer model on a 16 GB M1 Pro matter more than the last points of
//! accuracy. large-v3-turbo stays pinned below for comparison in `bench/stt`.

use ssc_core::Result;
use ssc_memory::download::{self, ModelFile, ModelSpec, ProgressFn};
use std::path::PathBuf;

/// File name of the default model inside its directory.
pub const WHISPER_FILE: &str = "ggml-small.en.bin";

/// The default local model.
pub const WHISPER_DEFAULT: ModelSpec = WHISPER_SMALL_EN;

/// `ggerganov/whisper.cpp` at a pinned commit; SHA-256 = the file's Git LFS object id.
pub const WHISPER_SMALL_EN: ModelSpec = ModelSpec {
    repo: "ggerganov/whisper.cpp",
    revision: "5359861c739e955e79d9a303bcbc70fb988958b1",
    dir: "whisper-small.en@5359861c",
    files: &[ModelFile {
        path: WHISPER_FILE,
        sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
        size: 487_614_201,
    }],
};

/// `ggerganov/whisper.cpp` at a pinned commit. The SHA-256 is the file's Git LFS object id at
/// that commit, which by definition is the SHA-256 of the content; `ensure_whisper` checks it.
pub const WHISPER_LARGE_V3_TURBO: ModelSpec = ModelSpec {
    repo: "ggerganov/whisper.cpp",
    revision: "5359861c739e955e79d9a303bcbc70fb988958b1",
    dir: "whisper-large-v3-turbo@5359861c",
    files: &[ModelFile {
        path: "ggml-large-v3-turbo.bin",
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
        size: 1_624_555_275,
    }],
};

/// Downloads (if needed) and verifies the whisper model into `~/.ssc/models/...`; returns the
/// model file path for `WhisperConfig::new` (feature `local-stt`).
pub async fn ensure_whisper(progress: ProgressFn<'_>) -> Result<PathBuf> {
    let dir = download::ensure(download::HF_BASE_URL, &download::models_dir(), &WHISPER_DEFAULT, progress).await?;
    Ok(dir.join(WHISPER_FILE))
}

/// The model file if it is already in place, without touching the network.
pub fn whisper_path_if_present() -> Option<PathBuf> {
    let root = download::models_dir();
    download::is_present(&root, &WHISPER_DEFAULT)
        .then(|| download::spec_dir(&root, &WHISPER_DEFAULT).join(WHISPER_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_is_pinned() {
        assert_eq!(WHISPER_DEFAULT.files[0].path, WHISPER_FILE);
        for s in [&WHISPER_SMALL_EN, &WHISPER_LARGE_V3_TURBO] {
            pinned(s);
        }
    }

    fn pinned(s: &ModelSpec) {
        assert_eq!(s.revision.len(), 40, "revision must be a full commit hash");
        assert!(s.dir.ends_with(&s.revision[..8]));
        for f in s.files {
            assert_eq!(f.sha256.len(), 64);
            assert!(f.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        }
    }
}
