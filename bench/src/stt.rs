//! `bench stt`: Deepgram streaming vs local Whisper on the same clips.
//!
//! Per clip: end-of-speech → final transcript latency (wall time from sending the audio chunk that
//! contains `speech_end` to receiving the `ends_utterance` final), WER of the joined finals against
//! the reference text, and this process's CPU while transcribing (the engine runs in-process).
//!
//! Fixture mode (gate): the real `DeepgramStt` adapter against a loopback fake replaying
//! `crates/ssc-stt/tests/fixtures/deepgram_session.jsonl`, and `ssc-core`'s `MockStt` standing in
//! for Whisper, both on a synthetic clip. Live mode paces audio in real time.

use crate::cpu::cpu_time_ms;
use crate::report::Report;
use crate::stats::{Summary, mean_f64};
use crate::wer;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use ssc_core::protocol::Speaker;
use ssc_core::traits::{Clock, SttEngine, SystemClock};
use ssc_core::types::{AudioChunk, SAMPLE_RATE_HZ};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const CHUNK: usize = 320; // 20 ms at 16 kHz
const CHUNK_MS: u64 = 20;

/// One clip in `bench/data/stt/manifest.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clip {
    pub id: String,
    /// WAV path relative to `bench/` (16 kHz mono PCM16). `null` = an empty slot.
    pub wav: Option<String>,
    /// Exact words spoken; `null` = unknown (latency only, no WER).
    pub reference: Option<String>,
    /// e.g. "en-US (TTS)", "Nigerian English", "Nigerian Pidgin".
    pub accent: String,
    pub source: String,
    pub license: String,
    #[serde(default)]
    pub notes: String,
}

pub fn manifest_path() -> PathBuf {
    crate::bench_path("data/stt/manifest.jsonl")
}

pub fn load_manifest(path: &Path) -> Result<Vec<Clip>> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    s.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("{}:{}", path.display(), i + 1)))
        .collect()
}

pub fn read_wav(path: &Path) -> Result<Vec<i16>> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("open {}", path.display()))?;
    let spec = r.spec();
    anyhow::ensure!(
        spec.sample_rate == SAMPLE_RATE_HZ && spec.channels == 1 && spec.bits_per_sample == 16,
        "{}: need 16 kHz mono 16-bit (got {} Hz, {} ch, {} bit)",
        path.display(),
        spec.sample_rate,
        spec.channels,
        spec.bits_per_sample
    );
    Ok(r.samples::<i16>().collect::<Result<_, _>>()?)
}

/// Pads a clip with silence so the endpointer can fire: 300 ms before, 1500 ms after.
pub fn padded(samples: &[i16]) -> Vec<i16> {
    let ms = |n: usize| n * SAMPLE_RATE_HZ as usize / 1000;
    let mut v = vec![0i16; ms(300)];
    v.extend_from_slice(samples);
    v.extend(std::iter::repeat_n(0i16, ms(1500)));
    v
}

#[derive(Debug, Clone)]
pub struct ClipRun {
    pub clip: String,
    pub engine: String,
    /// Latency of each end-of-utterance final, ms.
    pub latencies: Vec<u64>,
    pub transcript: String,
    pub wer: Option<f64>,
    /// Process CPU while transcribing, % of one core.
    pub cpu_pct: f64,
    pub error: Option<String>,
}

/// Feed `samples` to `engine` as "them" audio (paced in real time or as fast as possible) and
/// collect the finals. `base_ms` is the capture time of the first sample.
pub async fn run_clip(
    engine: &dyn SttEngine,
    engine_name: &str,
    clip_id: &str,
    samples: &[i16],
    reference: Option<&str>,
    pace: bool,
) -> ClipRun {
    let clock = SystemClock;
    let base_ms = clock.now_ms();
    let (atx, arx) = mpsc::channel::<AudioChunk>(8192);
    let (stx, mut srx) = mpsc::channel(512);
    let cancel = CancellationToken::new();
    let pid = std::process::id();
    let cpu0 = cpu_time_ms(pid).unwrap_or(0);
    let wall0 = std::time::Instant::now();
    let mut run = ClipRun {
        clip: clip_id.into(),
        engine: engine_name.into(),
        latencies: vec![],
        transcript: String::new(),
        wer: None,
        cpu_pct: 0.0,
        error: None,
    };
    if let Err(e) = engine.start(arx, stx, cancel.clone()) {
        run.error = Some(e.to_string());
        return run;
    }

    let n_chunks = samples.len().div_ceil(CHUNK);
    let start = tokio::time::Instant::now();
    let feed = async {
        let mut sent_at = Vec::with_capacity(n_chunks);
        for (i, c) in samples.chunks(CHUNK).enumerate() {
            if pace {
                tokio::time::sleep_until(start + Duration::from_millis(i as u64 * CHUNK_MS)).await;
            }
            sent_at.push(clock.now_ms());
            let chunk =
                AudioChunk { speaker: Speaker::Them, t0_ms: base_ms + i as u64 * CHUNK_MS, samples: c.to_vec() };
            if atx.send(chunk).await.is_err() {
                break;
            }
        }
        drop(atx);
        sent_at
    };
    let collect = async {
        let mut finals = Vec::new();
        // Stop when the engine closes its output, or after 4 s of silence from it.
        while let Ok(Some(seg)) = tokio::time::timeout(Duration::from_secs(4), srx.recv()).await {
            if seg.is_final {
                finals.push((seg, clock.now_ms()));
            }
        }
        finals
    };
    let (sent_at, finals) = tokio::join!(feed, collect);
    cancel.cancel();

    let wall = wall0.elapsed().as_millis().max(1) as f64;
    run.cpu_pct = 100.0 * cpu_time_ms(pid).unwrap_or(cpu0).saturating_sub(cpu0) as f64 / wall;
    for (seg, recv) in &finals {
        if seg.ends_utterance && !sent_at.is_empty() {
            let idx = (seg.t1_ms.saturating_sub(base_ms) / CHUNK_MS) as usize;
            let sent = sent_at[idx.min(sent_at.len() - 1)];
            run.latencies.push(recv.saturating_sub(sent));
        }
    }
    run.transcript = finals.iter().map(|(s, _)| s.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ");
    run.wer = reference.and_then(|r| wer::wer(r, &run.transcript));
    if finals.is_empty() {
        run.error = Some("no final transcript".into());
    }
    run
}

pub fn render(r: &mut Report, runs: &[ClipRun], skipped: &[(String, String)]) {
    let mut engines: Vec<&str> = runs.iter().map(|x| x.engine.as_str()).collect();
    engines.dedup();
    let summary: Vec<Vec<String>> = engines
        .iter()
        .map(|e| {
            let mine: Vec<&ClipRun> = runs.iter().filter(|x| x.engine == *e).collect();
            let lat: Vec<u64> = mine.iter().flat_map(|x| x.latencies.iter().copied()).collect();
            let s = Summary::of(&lat);
            let wers: Vec<f64> = mine.iter().filter_map(|x| x.wer).collect();
            let cpu: Vec<f64> = mine.iter().map(|x| x.cpu_pct).collect();
            vec![
                e.to_string(),
                s.map(|s| s.p50.to_string()).unwrap_or("n/a".into()),
                s.map(|s| s.p95.to_string()).unwrap_or("n/a".into()),
                mean_f64(&wers).map(|w| format!("{:.1}%", w * 100.0)).unwrap_or("n/a".into()),
                mean_f64(&cpu).map(|c| format!("{c:.1}")).unwrap_or("n/a".into()),
                format!("{}", mine.iter().filter(|x| x.error.is_some()).count()),
            ]
        })
        .collect();
    r.table(&["engine", "speech end → final p50 ms", "p95 ms", "mean WER", "CPU % (1 core)", "errors"], &summary);
    r.h2("Per clip");
    let rows: Vec<Vec<String>> = runs
        .iter()
        .map(|x| {
            vec![
                x.clip.clone(),
                x.engine.clone(),
                format!("{:?}", x.latencies),
                x.wer.map(|w| format!("{:.1}%", w * 100.0)).unwrap_or("n/a".into()),
                format!("{:.1}", x.cpu_pct),
                x.error.clone().unwrap_or_else(|| x.transcript.clone()),
            ]
        })
        .collect();
    r.table(&["clip", "engine", "latencies ms", "WER", "CPU %", "transcript / error"], &rows);
    if !skipped.is_empty() {
        r.h2("Skipped");
        for (what, why) in skipped {
            r.bullet(&format!("{what}: {why}"));
        }
        r.end_list();
    }
}

// ---------- fixture mode ----------

pub const FIXTURE_REFERENCE: &str = "What is your pricing?";

/// The synthetic clip the recorded Deepgram session was made for: speech 500–2000 ms.
pub fn fixture_clip() -> Vec<i16> {
    use ssc_audio::synth::{Segment, render};
    render(&[Segment::Silence { ms: 500 }, Segment::Speech { ms: 1_500 }, Segment::Silence { ms: 1_000 }]).samples
}

pub async fn run_fixture(report_path: &Path) -> Result<(Report, Vec<ClipRun>)> {
    use ssc_core::mock::MockStt;
    use ssc_core::secrets::Secret;
    use ssc_core::types::TranscriptSegment;
    use ssc_stt::{DeepgramConfig, DeepgramStt};

    let fixture = crate::bench_path("../crates/ssc-stt/tests/fixtures/deepgram_session.jsonl");
    let lines: Vec<String> = std::fs::read_to_string(&fixture)
        .with_context(|| fixture.display().to_string())?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();
    let url = crate::fake::deepgram(lines).await?;
    let dg = DeepgramStt::new(
        DeepgramConfig { url, finalize_timeout_ms: 2_000, ..DeepgramConfig::default() },
        Secret::new("fixture-key-not-real"),
    );
    let clip = fixture_clip();
    let mut runs =
        vec![run_clip(&dg, "deepgram (loopback replay)", "synth-pricing", &clip, Some(FIXTURE_REFERENCE), false).await];

    // Whisper stand-in: one end-of-utterance final 120 ms after the audio is in. Its t1 is set
    // when the clip starts, so build the engine per run.
    let base = SystemClock.now_ms();
    let mock = MockStt {
        script: vec![(
            Duration::from_millis(120),
            TranscriptSegment {
                speaker: Speaker::Them,
                text: "what is your pricing".into(),
                is_final: true,
                t0_ms: base + 500,
                t1_ms: base + 2_000,
                ends_utterance: true,
            },
        )],
    };
    runs.push(
        run_clip(&mock, "whisper stand-in (MockStt)", "synth-pricing", &clip, Some(FIXTURE_REFERENCE), false).await,
    );

    let mut r = Report::new("bench/stt — fixture mode");
    r.para(
        "Deepgram: the production adapter against a loopback fake replaying a recorded session. \
         Whisper: `MockStt` stand-in (real whisper needs a model file and `--features local-stt`). \
         Audio is fed unpaced, so latencies are harness overhead, not provider speed.",
    );
    render(&mut r, &runs, &[]);
    r.write(report_path)?;
    Ok((r, runs))
}

// ---------- live mode ----------

#[cfg(feature = "live")]
pub mod live {
    use super::*;
    use ssc_core::secrets::KeychainStore;
    use ssc_stt::{DeepgramConfig, DeepgramStt};

    pub struct LiveOpts {
        pub manifest: PathBuf,
        /// ggml whisper model; defaults to `$SSC_WHISPER_MODEL`.
        pub whisper_model: Option<PathBuf>,
    }

    pub async fn run(report_path: &Path, o: &LiveOpts) -> Result<Report> {
        let clips = load_manifest(&o.manifest)?;
        let mut skipped = Vec::new();
        let mut audio = Vec::new();
        for c in &clips {
            let Some(rel) = &c.wav else {
                skipped.push((format!("clip {}", c.id), format!("empty slot ({}): {}", c.accent, c.notes)));
                continue;
            };
            let p = crate::bench_path(rel);
            match read_wav(&p) {
                Ok(s) => audio.push((c, padded(&s))),
                Err(e) => skipped.push((
                    format!("clip {}", c.id),
                    format!("{e:#} (generate TTS clips with bench/scripts/make_stt_clips.sh)"),
                )),
            }
        }

        let mut engines: Vec<(String, Box<dyn SttEngine>)> = Vec::new();
        match DeepgramStt::from_store(&KeychainStore, DeepgramConfig::default()) {
            Ok(dg) => engines.push(("deepgram nova-3".into(), Box::new(dg))),
            Err(ssc_core::SscError::NotFound) => {
                skipped.push(("deepgram".into(), "no `api.deepgram` key in the keychain".into()))
            }
            Err(e) => skipped.push(("deepgram".into(), e.to_string())),
        }
        match whisper(o.whisper_model.clone()).await {
            Ok((name, w)) => engines.push((name, w)),
            Err(why) => skipped.push(("whisper".into(), why)),
        }

        let mut runs = Vec::new();
        for (name, e) in &engines {
            for (c, samples) in &audio {
                eprintln!("stt: {name} on {}", c.id);
                runs.push(run_clip(e.as_ref(), name, &c.id, samples, c.reference.as_deref(), true).await);
            }
        }
        let mut r = Report::new("bench/stt — live");
        r.para("Audio paced in real time (20 ms chunks). CPU is this process while the engine runs.");
        render(&mut r, &runs, &skipped);
        r.write(report_path)?;
        Ok(r)
    }

    #[cfg(feature = "local-stt")]
    async fn whisper(model: Option<PathBuf>) -> Result<(String, Box<dyn SttEngine>), String> {
        use ssc_stt::{WhisperConfig, WhisperStt};
        let path = model
            .or_else(|| std::env::var_os("SSC_WHISPER_MODEL").map(PathBuf::from))
            .ok_or("set SSC_WHISPER_MODEL or --whisper-model to a ggml model file")?;
        if !path.exists() {
            return Err(format!("{} not found", path.display()));
        }
        let name = format!("whisper {}", path.file_name().and_then(|n| n.to_str()).unwrap_or("?"));
        let w = tokio::task::spawn_blocking(move || WhisperStt::new(WhisperConfig::new(path)))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        Ok((name, Box::new(w)))
    }

    #[cfg(not(feature = "local-stt"))]
    async fn whisper(_model: Option<PathBuf>) -> Result<(String, Box<dyn SttEngine>), String> {
        Err("built without `--features local-stt` (needs cmake)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_has_nigerian_slots() {
        let clips = load_manifest(&manifest_path()).unwrap();
        assert!(clips.iter().any(|c| c.accent.contains("Nigerian English") && c.wav.is_none()));
        assert!(clips.iter().any(|c| c.accent.contains("Pidgin") && c.wav.is_none()));
        assert!(clips.iter().filter(|c| c.wav.is_some()).all(|c| c.reference.is_some()));
    }

    #[test]
    fn padding() {
        let p = padded(&[1, 2, 3]);
        assert_eq!(p.len(), 4800 + 3 + 24000);
        assert_eq!(&p[4800..4803], &[1, 2, 3]);
    }
}
