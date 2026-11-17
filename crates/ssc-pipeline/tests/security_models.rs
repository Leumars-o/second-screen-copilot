//! W2-HARDEN: every model the agent downloads is pinned (`docs/security/threat-model.md` §7).
//! A pin is a full commit hash plus a SHA-256 and size per file, fetched over HTTPS. The
//! downloader's own tests (`crates/ssc-memory/tests/download.rs`) cover hash mismatch, oversize
//! bodies and resume; this guards the pins themselves so nobody re-pins to a branch name or drops
//! a hash.

use ssc_memory::download::{HF_BASE_URL, ModelSpec};

fn specs() -> Vec<(&'static str, ModelSpec)> {
    vec![
        ("bge-small-en-v1.5", ssc_memory::models::BGE_SMALL_EN_V15),
        ("whisper small.en", ssc_stt::models::WHISPER_SMALL_EN),
        ("whisper large-v3-turbo", ssc_stt::models::WHISPER_LARGE_V3_TURBO),
    ]
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[test]
fn every_model_is_pinned_by_commit_hash_and_size() {
    assert!(HF_BASE_URL.starts_with("https://"), "models come over HTTPS only");
    for (name, spec) in specs() {
        assert!(is_hex(spec.revision, 40), "{name}: revision must be a full commit hash, got {:?}", spec.revision);
        assert!(!spec.files.is_empty(), "{name}: no files");
        assert!(spec.dir.contains(&spec.revision[..8]), "{name}: dir must carry the short revision");
        for f in spec.files {
            assert!(is_hex(f.sha256, 64), "{name}/{}: sha256 must be 64 lower-case hex", f.path);
            assert!(f.size > 0, "{name}/{}: size pinned", f.path);
            assert!(
                !f.path.contains("..") && !f.path.starts_with('/'),
                "{name}/{}: path escapes the model dir",
                f.path
            );
        }
        let mut hashes: Vec<&str> = spec.files.iter().map(|f| f.sha256).collect();
        hashes.sort();
        hashes.dedup();
        assert_eq!(hashes.len(), spec.files.len(), "{name}: duplicate hashes look like a copy-paste pin");
    }
}
