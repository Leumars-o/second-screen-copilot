//! Local speech-to-text with whisper.cpp (`whisper-rs`), for local-only mode. Feature `local-stt`.
//!
//! Whisper is not a streaming model, so utterances are cut by the VAD [`Endpointer`]:
//! - audio is buffered in memory from `pre_roll_ms` before speech start;
//! - every `partial_every_ms` of buffered speech, the buffer so far is transcribed and emitted
//!   as a partial segment;
//! - on `speech_end`, the utterance is transcribed and emitted as a final segment with
//!   `ends_utterance = true` and `t1_ms = speech_end`;
//! - an utterance longer than `max_utterance_ms` is emitted as a final without
//!   `ends_utterance` and buffering continues.
//!
//! One `WhisperState` per speaker; the model weights (`WhisperContext`) are shared.

use crate::demux::{WORKER_QUEUE, route_by_speaker};
use ssc_audio::vad::{Endpointer, EndpointerConfig, VadEvent};
use ssc_core::protocol::Speaker;
use ssc_core::traits::SttEngine;
use ssc_core::types::{AudioChunk, SAMPLE_RATE_HZ, TranscriptSegment};
use ssc_core::{Result, SscError};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

#[derive(Debug, Clone, PartialEq)]
pub struct WhisperConfig {
    /// ggml model file, e.g. `ggml-base.en.bin`.
    pub model_path: PathBuf,
    pub language: String,
    pub endpointer: EndpointerConfig,
    /// 0 disables partials.
    pub partial_every_ms: u32,
    pub max_utterance_ms: u32,
    pub pre_roll_ms: u32,
    pub n_threads: i32,
}

impl WhisperConfig {
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            language: "en".into(),
            endpointer: EndpointerConfig::default(),
            partial_every_ms: 1_000,
            max_utterance_ms: 28_000,
            pre_roll_ms: 200,
            n_threads: std::thread::available_parallelism().map(|n| n.get().min(4) as i32).unwrap_or(4),
        }
    }
}

pub struct WhisperStt {
    cfg: WhisperConfig,
    ctx: Arc<WhisperContext>,
}

impl WhisperStt {
    /// Loads the model (blocking; call from `spawn_blocking` if on a runtime thread).
    pub fn new(cfg: WhisperConfig) -> Result<Self> {
        let path = cfg.model_path.to_str().ok_or_else(|| SscError::Invalid("model path is not UTF-8".into()))?;
        let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .map_err(|e| SscError::Unavailable(format!("whisper model {path}: {e}")))?;
        Ok(Self { cfg, ctx: Arc::new(ctx) })
    }
}

impl SttEngine for WhisperStt {
    fn start(
        &self,
        rx: mpsc::Receiver<AudioChunk>,
        tx: mpsc::Sender<TranscriptSegment>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let cfg = self.cfg.clone();
        let ctx = self.ctx.clone();
        let c = cancel.clone();
        route_by_speaker(rx, cancel, move |speaker| {
            let (wtx, wrx) = mpsc::channel(WORKER_QUEUE);
            let state = ctx.create_state().map(|s| Arc::new(Mutex::new(s)));
            let (cfg, tx, c) = (cfg.clone(), tx.clone(), c.clone());
            tokio::spawn(async move {
                // A state that can't be created leaves this speaker without transcripts; its
                // input is still drained so the demux never blocks.
                match state {
                    Ok(state) => run_speaker(speaker, cfg, state, wrx, tx, c).await,
                    Err(_) => drain(wrx, c).await,
                }
            });
            wtx
        });
        Ok(())
    }
}

async fn drain(mut rx: mpsc::Receiver<AudioChunk>, cancel: CancellationToken) {
    while !cancel.is_cancelled() && rx.recv().await.is_some() {}
}

/// In-memory utterance buffer with its capture start time.
struct Utterance {
    samples: Vec<i16>,
    t0_ms: u64,
    last_partial_len: usize,
}

async fn transcribe(state: Arc<Mutex<WhisperState>>, cfg: &WhisperConfig, samples: Vec<i16>) -> Option<String> {
    let lang = cfg.language.clone();
    let threads = cfg.n_threads;
    tokio::task::spawn_blocking(move || {
        let audio: Vec<f32> = samples.iter().map(|&s| s as f32 / 32_768.0).collect();
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(&lang));
        params.set_n_threads(threads);
        params.set_no_context(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
        st.full(params, &audio).ok()?;
        let text: Vec<String> =
            st.as_iter().filter_map(|seg| seg.to_str_lossy().ok().map(|s| s.trim().to_string())).collect();
        Some(text.join(" ").trim().to_string())
    })
    .await
    .ok()
    .flatten()
}

fn ms(samples: usize) -> u64 {
    samples as u64 * 1000 / SAMPLE_RATE_HZ as u64
}

fn samples_in(ms: u64) -> usize {
    (ms * SAMPLE_RATE_HZ as u64 / 1000) as usize
}

async fn run_speaker(
    speaker: Speaker,
    cfg: WhisperConfig,
    state: Arc<Mutex<WhisperState>>,
    mut audio: mpsc::Receiver<AudioChunk>,
    tx: mpsc::Sender<TranscriptSegment>,
    cancel: CancellationToken,
) {
    let mut vad = Endpointer::new(cfg.endpointer.clone());
    // Enough recent audio to cover pre-roll plus the VAD's start delay.
    let keep_recent = samples_in(cfg.pre_roll_ms as u64 + cfg.endpointer.start_ms as u64 + 100);
    let partial_every = samples_in(cfg.partial_every_ms as u64);
    let max_len = samples_in(cfg.max_utterance_ms as u64);
    let mut recent: VecDeque<(u64, Vec<i16>)> = VecDeque::new();
    let mut utt: Option<Utterance> = None;
    let idle_after = Duration::from_millis(cfg.endpointer.silence_ms as u64);
    let mut last_audio = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let mut ended: Option<u64> = None;
        let mut closed = false;
        tokio::select! {
            // Biased: queued audio is always read before the silence tick. Otherwise time spent in a
            // slow partial transcription (> silence_ms) looks like an audio gap and splits the utterance.
            biased;
            _ = cancel.cancelled() => return,
            c = audio.recv() => match c {
                Some(chunk) if chunk.speaker == speaker => {
                    last_audio = Instant::now();
                    let events = vad.push(&chunk.samples, chunk.t0_ms);
                    match utt.as_mut() {
                        Some(u) => u.samples.extend_from_slice(&chunk.samples),
                        None => {
                            recent.push_back((chunk.t0_ms, chunk.samples));
                            while recent.len() > 1 && recent.iter().map(|(_, s)| s.len()).sum::<usize>() > keep_recent {
                                recent.pop_front();
                            }
                        }
                    }
                    for ev in events {
                        match ev {
                            VadEvent::SpeechStart { t_ms } if utt.is_none() => {
                                utt = Some(start_utterance(&mut recent, t_ms.saturating_sub(cfg.pre_roll_ms as u64)));
                            }
                            VadEvent::SpeechEnd { t_ms, .. } => ended = Some(t_ms),
                            _ => {}
                        }
                    }
                }
                Some(_) => {}
                None => {
                    closed = true;
                    if let Some(VadEvent::SpeechEnd { t_ms, .. }) = vad.flush() {
                        ended = Some(t_ms);
                    }
                }
            },
            _ = tick.tick() => {
                // Sources may stop delivering packets during silence: treat a gap as silence.
                if vad.in_speech() && last_audio.elapsed() >= idle_after
                    && let Some(VadEvent::SpeechEnd { t_ms, .. }) = vad.flush()
                {
                    ended = Some(t_ms);
                }
            }
        }

        if let Some(t_end) = ended {
            if let Some(u) = utt.take() {
                // Trim trailing silence (keep 150 ms after speech_end).
                let keep = samples_in(t_end.saturating_sub(u.t0_ms) + 150).min(u.samples.len());
                let text = transcribe(state.clone(), &cfg, u.samples[..keep].to_vec()).await.unwrap_or_default();
                let seg = TranscriptSegment {
                    speaker,
                    text,
                    is_final: true,
                    t0_ms: u.t0_ms,
                    t1_ms: t_end,
                    ends_utterance: true,
                };
                if tx.send(seg).await.is_err() {
                    return;
                }
            }
        } else if let Some(u) = utt.as_mut() {
            if u.samples.len() >= max_len {
                let samples = std::mem::take(&mut u.samples);
                let (t0, t1) = (u.t0_ms, u.t0_ms + ms(samples.len()));
                u.t0_ms = t1;
                u.last_partial_len = 0;
                let text = transcribe(state.clone(), &cfg, samples).await.unwrap_or_default();
                let seg =
                    TranscriptSegment { speaker, text, is_final: true, t0_ms: t0, t1_ms: t1, ends_utterance: false };
                if tx.send(seg).await.is_err() {
                    return;
                }
            } else if partial_every > 0 && u.samples.len() >= u.last_partial_len + partial_every {
                u.last_partial_len = u.samples.len();
                let (t0, t1) = (u.t0_ms, u.t0_ms + ms(u.samples.len()));
                if let Some(text) = transcribe(state.clone(), &cfg, u.samples.clone()).await
                    && !text.is_empty()
                {
                    let seg = TranscriptSegment {
                        speaker,
                        text,
                        is_final: false,
                        t0_ms: t0,
                        t1_ms: t1,
                        ends_utterance: false,
                    };
                    if tx.send(seg).await.is_err() {
                        return;
                    }
                }
            }
        }
        if closed {
            return;
        }
    }
}

/// Build the utterance buffer from recent chunks, starting at capture time `from_ms`.
fn start_utterance(recent: &mut VecDeque<(u64, Vec<i16>)>, from_ms: u64) -> Utterance {
    let mut samples = Vec::new();
    let mut t0 = None;
    for (ct0, s) in recent.drain(..) {
        if ct0 + ms(s.len()) <= from_ms {
            continue;
        }
        let skip = samples_in(from_ms.saturating_sub(ct0)).min(s.len());
        t0.get_or_insert(ct0 + ms(skip));
        samples.extend_from_slice(&s[skip..]);
    }
    Utterance { samples, t0_ms: t0.unwrap_or(from_ms), last_partial_len: 0 }
}
