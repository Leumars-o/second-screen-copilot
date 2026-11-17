//! Turns native capture buffers (any rate, any channel count, f32) into fixed-size
//! 16 kHz mono PCM16 [`AudioChunk`]s with capture timestamps.
//!
//! Timestamps follow the sample count, anchored to the wall clock of the first buffer. If a
//! buffer's reported capture time drifts more than [`REANCHOR_MS`] from the sample-count
//! prediction (device hiccup, dropped buffers), the timeline re-anchors to the wall clock.

use crate::resample::{Resampler, downmix_interleaved, f32_to_i16};
use ssc_core::protocol::Speaker;
use ssc_core::types::{AudioChunk, SAMPLE_RATE_HZ};

pub const REANCHOR_MS: u64 = 100;
/// Default chunk length. Short chunks keep the VAD/STT path low-latency.
pub const DEFAULT_CHUNK_MS: u32 = 20;

pub struct ChunkAssembler {
    speaker: Speaker,
    channels: usize,
    chunk_samples: usize,
    resampler: Resampler,
    mono: Vec<f32>,
    out: Vec<f32>,
    /// Wall time (unix ms) of output sample `anchor_n`.
    anchor_ms: Option<u64>,
    anchor_n: u64,
    /// Output samples produced so far (emitted + pending in `out`).
    produced: u64,
}

impl ChunkAssembler {
    pub fn new(speaker: Speaker, in_rate: u32, channels: usize, chunk_ms: u32) -> Self {
        let chunk_samples = (SAMPLE_RATE_HZ as usize * chunk_ms.max(1) as usize / 1000).max(1);
        Self {
            speaker,
            channels: channels.max(1),
            chunk_samples,
            resampler: Resampler::new(in_rate, SAMPLE_RATE_HZ),
            mono: Vec::new(),
            out: Vec::new(),
            anchor_ms: None,
            anchor_n: 0,
            produced: 0,
        }
    }

    pub fn speaker(&self) -> Speaker {
        self.speaker
    }

    fn time_of(&self, n: u64) -> u64 {
        self.anchor_ms.unwrap_or(0) + (n - self.anchor_n) * 1000 / SAMPLE_RATE_HZ as u64
    }

    /// Feed one interleaved capture buffer whose first frame was captured at `captured_at_ms`.
    /// Returns every complete chunk.
    pub fn push_interleaved(&mut self, interleaved: &[f32], captured_at_ms: u64) -> Vec<AudioChunk> {
        self.mono.clear();
        downmix_interleaved(interleaved, self.channels, &mut self.mono);
        let mono = std::mem::take(&mut self.mono);
        let chunks = self.push_mono(&mono, captured_at_ms);
        self.mono = mono;
        chunks
    }

    /// Feed one mono buffer at the input rate.
    pub fn push_mono(&mut self, mono: &[f32], captured_at_ms: u64) -> Vec<AudioChunk> {
        match self.anchor_ms {
            None => {
                self.anchor_ms = Some(captured_at_ms);
                self.anchor_n = self.produced;
            }
            Some(_) => {
                let predicted = self.time_of(self.produced);
                if predicted.abs_diff(captured_at_ms) > REANCHOR_MS {
                    self.anchor_ms = Some(captured_at_ms);
                    self.anchor_n = self.produced;
                }
            }
        }
        let before = self.out.len();
        self.resampler.process(mono, &mut self.out);
        self.produced += (self.out.len() - before) as u64;

        let mut chunks = Vec::new();
        let mut first_n = self.produced - self.out.len() as u64;
        let mut drained = 0;
        while self.out.len() - drained >= self.chunk_samples {
            let samples = self.out[drained..drained + self.chunk_samples].iter().map(|&x| f32_to_i16(x)).collect();
            chunks.push(AudioChunk { speaker: self.speaker, t0_ms: self.time_of(first_n), samples });
            drained += self.chunk_samples;
            first_n += self.chunk_samples as u64;
        }
        self.out.drain(..drained);
        chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_size_chunks_with_monotonic_times() {
        let mut a = ChunkAssembler::new(Speaker::Them, 48_000, 2, 20);
        let mut all = Vec::new();
        // 10 ms stereo buffers at 48 kHz, delivered on time.
        for i in 0..100u64 {
            let buf = vec![0.1f32; 480 * 2];
            all.extend(a.push_interleaved(&buf, 1_000_000 + i * 10));
        }
        assert!(all.len() >= 48, "{} chunks", all.len());
        for (i, c) in all.iter().enumerate() {
            assert_eq!(c.samples.len(), 320);
            assert_eq!(c.speaker, Speaker::Them);
            assert_eq!(c.t0_ms, 1_000_000 + i as u64 * 20);
        }
    }

    #[test]
    fn reanchors_after_a_gap() {
        let mut a = ChunkAssembler::new(Speaker::Me, 16_000, 1, 20);
        let c1 = a.push_mono(&[0.0; 320], 5_000);
        assert_eq!(c1[0].t0_ms, 5_000);
        let c2 = a.push_mono(&[0.0; 320], 5_020);
        assert_eq!(c2[0].t0_ms, 5_020);
        // 2 s gap: the device stalled. Next chunk follows the wall clock.
        let c3 = a.push_mono(&[0.0; 320], 7_040);
        assert_eq!(c3[0].t0_ms, 7_040);
        // Small jitter (< REANCHOR_MS) does not move the timeline.
        let c4 = a.push_mono(&[0.0; 320], 7_090);
        assert_eq!(c4[0].t0_ms, 7_060);
    }

    #[test]
    fn partial_chunks_carry_over() {
        let mut a = ChunkAssembler::new(Speaker::Me, 16_000, 1, 20);
        assert!(a.push_mono(&[0.5; 200], 0).is_empty());
        let c = a.push_mono(&[0.5; 200], 12);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].t0_ms, 0);
        assert!(c[0].samples.iter().all(|&s| s == f32_to_i16(0.5)));
    }
}
