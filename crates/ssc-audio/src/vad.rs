//! Voice activity detection and end-of-speech (`speech_end`) detection.
//!
//! Frame classifier: [earshot](https://crates.io/crates/earshot), a small pure-Rust neural VAD
//! (16 ms frames, ~8 KiB state, no C/ONNX runtime), plus an energy floor so digital silence and
//! faint hiss never count as speech. Chosen over `webrtc-vad` (C, no Windows cross-build, and a
//! `!Send` handle) and Silero (needs ONNX Runtime).
//!
//! The [`Endpointer`] turns per-frame decisions into utterance boundaries:
//! - speech starts after `start_ms` of consecutive voiced frames (ignores clicks);
//! - speech ends after `silence_ms` of unvoiced frames; the reported `speech_end` time is the
//!   end of the last voiced frame (minus `end_compensation_ms`), i.e. the real end of speech,
//!   not the moment of detection;
//! - utterances with less than `min_speech_ms` of voiced audio are dropped.
//!
//! Detection latency after the real end of speech ≈ `silence_ms` (default 300 ms, the spec's
//! `question_silence_ms`).

use earshot::Detector;
use ssc_core::types::SAMPLE_RATE_HZ;

/// Earshot's fixed frame: 256 samples = 16 ms at 16 kHz.
pub const FRAME_SAMPLES: usize = 256;
pub const FRAME_MS: u32 = 16;

#[derive(Debug, Clone, PartialEq)]
pub struct EndpointerConfig {
    /// Frame voice probability at or above this counts as voiced. Earshot suggests 0.5; its
    /// score decays for 50–150 ms after speech stops (longer in noise), so the default is 0.6
    /// with `end_compensation_ms = 32`, tuned on `tests/vad_fixtures.rs` (clean, -35 dBFS
    /// noise, real TTS speech: all ends within ±100 ms).
    pub threshold: f32,
    /// Consecutive voiced audio needed to start an utterance.
    pub start_ms: u32,
    /// Silence that ends an utterance.
    pub silence_ms: u32,
    /// Utterances with less voiced audio than this are ignored.
    pub min_speech_ms: u32,
    /// Subtracted from the last voiced frame's end to undo classifier hangover.
    pub end_compensation_ms: u32,
    /// Frames quieter than this (dBFS RMS) are unvoiced regardless of the classifier.
    pub min_rms_dbfs: f32,
}

impl Default for EndpointerConfig {
    fn default() -> Self {
        Self {
            threshold: 0.6,
            start_ms: 64,
            silence_ms: 300,
            min_speech_ms: 120,
            end_compensation_ms: 32,
            min_rms_dbfs: -55.0,
        }
    }
}

impl EndpointerConfig {
    /// Default config with the silence window from `Config.timeouts.question_silence_ms`.
    pub fn with_silence_ms(silence_ms: u64) -> Self {
        Self { silence_ms: silence_ms.min(u32::MAX as u64) as u32, ..Self::default() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    SpeechStart {
        t_ms: u64,
    },
    /// `t_ms` is the `speech_end` timestamp (unix ms on the capture clock).
    SpeechEnd {
        start_ms: u64,
        t_ms: u64,
    },
}

pub struct Endpointer {
    vad: Box<Detector>,
    cfg: EndpointerConfig,
    buf: Vec<i16>,
    /// Capture time of `buf[0]` (frames are exactly 16 ms, so ms arithmetic is exact).
    buf_t0: u64,
    in_speech: bool,
    voiced_run_ms: u32,
    run_start_ms: u64,
    speech_start_ms: u64,
    last_voice_end_ms: u64,
    voiced_ms: u32,
    silence_run_ms: u32,
}

impl Endpointer {
    pub fn new(cfg: EndpointerConfig) -> Self {
        Self {
            vad: Detector::default_boxed(),
            cfg,
            buf: Vec::new(),
            buf_t0: 0,
            in_speech: false,
            voiced_run_ms: 0,
            run_start_ms: 0,
            speech_start_ms: 0,
            last_voice_end_ms: 0,
            voiced_ms: 0,
            silence_run_ms: 0,
        }
    }

    pub fn config(&self) -> &EndpointerConfig {
        &self.cfg
    }

    pub fn in_speech(&self) -> bool {
        self.in_speech
    }

    /// Feed 16 kHz mono samples whose first sample was captured at `t0_ms`.
    pub fn push(&mut self, samples: &[i16], t0_ms: u64) -> Vec<VadEvent> {
        if self.buf.is_empty() {
            self.buf_t0 = t0_ms;
        }
        self.buf.extend_from_slice(samples);
        let mut events = Vec::new();
        let mut off = 0;
        while self.buf.len() - off >= FRAME_SAMPLES {
            let frame_t0 = self.buf_t0 + ms(off);
            let voiced = self.classify(off);
            self.step(voiced, frame_t0, frame_t0 + FRAME_MS as u64, &mut events);
            off += FRAME_SAMPLES;
        }
        self.buf.drain(..off);
        self.buf_t0 += ms(off);
        events
    }

    /// End of input (stream closed, or the source went quiet with no packets): close an open
    /// utterance if it is long enough. Classifier state is kept.
    pub fn flush(&mut self) -> Option<VadEvent> {
        self.buf.clear();
        self.voiced_run_ms = 0;
        if !self.in_speech {
            return None;
        }
        self.in_speech = false;
        self.end_event()
    }

    fn classify(&mut self, off: usize) -> bool {
        let frame = &self.buf[off..off + FRAME_SAMPLES];
        let energy: f64 = frame.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / frame.len() as f64;
        let dbfs = 10.0 * (energy.max(1e-9) / (i16::MAX as f64).powi(2)).log10();
        // Always run the detector so its context stays continuous.
        let p = self.vad.predict_i16(frame);
        dbfs >= self.cfg.min_rms_dbfs as f64 && p >= self.cfg.threshold
    }

    fn step(&mut self, voiced: bool, t0: u64, t1: u64, events: &mut Vec<VadEvent>) {
        if !self.in_speech {
            if voiced {
                if self.voiced_run_ms == 0 {
                    self.run_start_ms = t0;
                }
                self.voiced_run_ms += FRAME_MS;
                if self.voiced_run_ms >= self.cfg.start_ms {
                    self.in_speech = true;
                    self.speech_start_ms = self.run_start_ms;
                    self.voiced_ms = self.voiced_run_ms;
                    self.last_voice_end_ms = t1;
                    self.silence_run_ms = 0;
                    events.push(VadEvent::SpeechStart { t_ms: self.speech_start_ms });
                }
            } else {
                self.voiced_run_ms = 0;
            }
            return;
        }
        if voiced {
            self.voiced_ms += FRAME_MS;
            self.last_voice_end_ms = t1;
            self.silence_run_ms = 0;
        } else {
            self.silence_run_ms += FRAME_MS;
            if self.silence_run_ms >= self.cfg.silence_ms {
                self.in_speech = false;
                self.voiced_run_ms = 0;
                if let Some(e) = self.end_event() {
                    events.push(e);
                }
            }
        }
    }

    fn end_event(&self) -> Option<VadEvent> {
        if self.voiced_ms < self.cfg.min_speech_ms {
            return None;
        }
        let t_ms = self.last_voice_end_ms.saturating_sub(self.cfg.end_compensation_ms as u64).max(self.speech_start_ms);
        Some(VadEvent::SpeechEnd { start_ms: self.speech_start_ms, t_ms })
    }
}

fn ms(samples: usize) -> u64 {
    samples as u64 * 1000 / SAMPLE_RATE_HZ as u64
}
