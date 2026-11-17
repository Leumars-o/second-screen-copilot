//! Acceptance: raw audio is never written to disk.
//!
//! Two checks:
//! 1. Runtime: run audio through the capture-side assembler, the VAD and the Deepgram adapter
//!    (against the local fake) and assert no file appeared in the temp dir, the working dir or
//!    either crate dir. (Own test binary, so no other test writes files concurrently.)
//! 2. Static: library sources of ssc-audio and ssc-stt contain no file-writing API at all, so
//!    no code path (including real-device capture, which tests can't drive) can write audio.

mod common;

use common::{Script, load_fixture};
use ssc_audio::chunker::ChunkAssembler;
use ssc_audio::synth::{Segment, render};
use ssc_core::protocol::Speaker;
use ssc_core::secrets::Secret;
use ssc_core::traits::SttEngine;
use ssc_stt::{DeepgramConfig, DeepgramStt};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn snapshot(dirs: &[PathBuf]) -> BTreeSet<PathBuf> {
    fn walk(p: &Path, depth: usize, out: &mut BTreeSet<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(p) else { return };
        for e in rd.flatten() {
            let path = e.path();
            // Skip build output; cargo may touch it while the test runs.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            out.insert(path.clone());
            if depth > 0 && path.is_dir() {
                walk(&path, depth - 1, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    for d in dirs {
        walk(d, 3, &mut out);
    }
    out
}

#[tokio::test]
async fn pipeline_creates_no_files() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dirs =
        vec![std::env::temp_dir(), std::env::current_dir().unwrap(), manifest.clone(), manifest.join("../ssc-audio")];
    // Other processes may write to the shared temp dir; only our audio-ish files would matter,
    // so compare everything but report only new paths.
    let before = snapshot(&dirs);

    let fake =
        common::start(Script::Fixture { lines: load_fixture("deepgram_session.jsonl"), answer_finalize: true }).await;
    let cfg = DeepgramConfig { url: fake.url.clone(), ..DeepgramConfig::default() };
    let stt = DeepgramStt::new(cfg, Secret::new("k"));
    let (atx, arx) = mpsc::channel(4096);
    let (stx, mut srx) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    stt.start(arx, stx, cancel.clone()).unwrap();

    // 48 kHz stereo "device" audio → assembler → engine, as the real capture path does.
    let clip = render(&[Segment::Silence { ms: 500 }, Segment::Speech { ms: 1_500 }, Segment::Silence { ms: 1_000 }]);
    let mut interleaved = Vec::new();
    for &s in &clip.samples {
        let v = s as f32 / 32_767.0;
        for _ in 0..3 {
            interleaved.extend([v, v]); // crude 3x upsample, stereo
        }
    }
    let mut asm = ChunkAssembler::new(Speaker::Them, 48_000, 2, 20);
    for (i, buf) in interleaved.chunks(960 * 2).enumerate() {
        for c in asm.push_interleaved(buf, 1_000_000 + i as u64 * 20) {
            atx.send(c).await.unwrap();
        }
    }
    drop(atx);
    let mut n = 0;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_secs(5), srx.recv()).await {
        n += 1;
    }
    cancel.cancel();
    assert!(n > 0, "pipeline produced no segments");

    let after = snapshot(&dirs);
    let new: Vec<&PathBuf> = after.difference(&before).collect();
    let suspicious: Vec<&&PathBuf> = new
        .iter()
        .filter(|p| {
            let s = p.to_string_lossy().to_lowercase();
            p.starts_with(&manifest)
                || p.starts_with(manifest.join("../ssc-audio"))
                || [".wav", ".pcm", ".raw", ".aiff", ".flac", ".ogg", ".mp3", "ssc"].iter().any(|ext| s.contains(ext))
        })
        .collect();
    assert!(suspicious.is_empty(), "files created during the audio pipeline: {suspicious:?}");
}

#[test]
fn library_sources_have_no_file_writes() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let forbidden = [
        "File::create",
        "OpenOptions",
        "fs::write",
        "fs::copy",
        "create_dir",
        "WavWriter",
        "tempfile",
        "BufWriter<File",
        "std::fs",
    ];
    let mut checked = 0;
    for crate_dir in [manifest.clone(), manifest.join("../ssc-audio")] {
        let mut stack = vec![crate_dir.join("src")];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let src = std::fs::read_to_string(&p).unwrap();
                    // Only library code: drop `#[cfg(test)] mod tests { ... }` (always last in our files).
                    let lib = src.split("#[cfg(test)]\nmod tests").next().unwrap();
                    for f in forbidden {
                        assert!(!lib.contains(f), "{} uses `{f}`: raw audio must never touch disk", p.display());
                    }
                    checked += 1;
                }
            }
        }
    }
    assert!(checked >= 10, "only {checked} source files scanned");
}
