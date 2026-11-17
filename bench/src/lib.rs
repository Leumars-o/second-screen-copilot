//! `ssc-bench`: the W2-BENCH harnesses (Build Sheet §7).
//!
//! Every harness has a fixture mode (no network, run by the gate) and a `--live` mode (feature
//! `live`; real providers, keys from the OS keychain). Each writes a markdown report.
//!
//! - [`latency`]: per-stage and end-to-end p50/p95 from `ssc-core::timing` JSONL.
//! - [`ttft`]: time to first token per answer model (Groq, Gemini Flash-Lite, local Ollama).
//! - [`stt`]: Deepgram vs local Whisper: end-of-speech → final latency, WER, CPU.
//! - [`decision`]: Jev vs Tev1 4B accuracy per question + latency on the labelled set.
//! - [`cpu`]: agent CPU while idle and while capturing.
//! - [`answers`]: deterministic cue-rule checks + tester usefulness ratings.

pub mod answers;
pub mod cpu;
pub mod decision;
pub mod fake;
pub mod latency;
#[cfg(feature = "live")]
pub mod live;
pub mod report;
pub mod stats;
pub mod stt;
pub mod ttft;
pub mod wer;

use std::path::{Path, PathBuf};

/// `bench/` (this crate's directory).
pub fn bench_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// `bench/<rel>`.
pub fn bench_path(rel: &str) -> PathBuf {
    bench_dir().join(rel)
}

/// Default report location: `bench/reports/<name>.md` (git-ignored).
pub fn default_report(name: &str) -> PathBuf {
    bench_path(&format!("reports/{name}.md"))
}
