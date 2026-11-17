//! Deterministic synthetic audio with known speech boundaries, for tests and benches.
//!
//! "Speech" is a voiced, speech-like signal: a harmonic series on a gliding 110–180 Hz
//! fundamental, shaped by two moving formants and a ~4 Hz syllable envelope. Silence is
//! low-level pseudo-random hiss (fixed seed). No randomness from the OS, so every run is
//! bit-identical.

use ssc_core::types::SAMPLE_RATE_HZ;
use std::f64::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    /// Speech-like audio for `ms`.
    Speech { ms: u32 },
    /// Hiss at about -60 dBFS for `ms`.
    Silence { ms: u32 },
}

/// Audio plus ground-truth speech intervals (ms from the start of the clip).
#[derive(Debug, Clone)]
pub struct Labelled {
    pub samples: Vec<i16>,
    pub speech: Vec<(u64, u64)>,
}

pub fn render(segments: &[Segment]) -> Labelled {
    let sr = SAMPLE_RATE_HZ as f64;
    let mut samples = Vec::new();
    let mut speech = Vec::new();
    let mut rng: u32 = 0x1234_5678;
    let mut phase = 0.0f64;
    let mut t_ms = 0u64;
    for seg in segments {
        match *seg {
            Segment::Silence { ms } => {
                let n = (sr * ms as f64 / 1000.0) as usize;
                for _ in 0..n {
                    // xorshift32
                    rng ^= rng << 13;
                    rng ^= rng >> 17;
                    rng ^= rng << 5;
                    let u = (rng as f64 / u32::MAX as f64) * 2.0 - 1.0;
                    samples.push((u * 0.001 * i16::MAX as f64) as i16);
                }
                t_ms += ms as u64;
            }
            Segment::Speech { ms } => {
                let n = (sr * ms as f64 / 1000.0) as usize;
                for i in 0..n {
                    let t = i as f64 / sr;
                    let f0 = 145.0 + 35.0 * (2.0 * PI * 1.3 * t).sin();
                    phase += 2.0 * PI * f0 / sr;
                    let f1 = 600.0 + 250.0 * (2.0 * PI * 3.1 * t).sin();
                    let f2 = 1_700.0 + 500.0 * (2.0 * PI * 2.3 * t + 1.0).sin();
                    let mut v = 0.0;
                    let mut k = 1.0;
                    while k * f0 < 4_000.0 {
                        let f = k * f0;
                        let g = 1.0 / k + 1.5 * formant(f, f1, 120.0) + 0.8 * formant(f, f2, 180.0);
                        v += g * (k * phase).sin();
                        k += 1.0;
                    }
                    // Syllable envelope 0.35..1, plus 10 ms fades at the segment edges.
                    let syl = 0.675 + 0.325 * (2.0 * PI * 4.0 * t).sin();
                    let edge = (i.min(n - 1 - i) as f64 / (0.01 * sr)).min(1.0);
                    samples.push((0.12 * v * syl * edge * i16::MAX as f64).clamp(-32767.0, 32767.0) as i16);
                }
                speech.push((t_ms, t_ms + ms as u64));
                t_ms += ms as u64;
            }
        }
    }
    Labelled { samples, speech }
}

fn formant(f: f64, centre: f64, bw: f64) -> f64 {
    let d = (f - centre) / bw;
    (-d * d).exp()
}

/// Encode as a 16-bit mono WAV in memory (for fixtures handed to code that wants a file
/// format). Never written to disk by library code.
pub fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE_HZ.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE_HZ * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_labelled() {
        let segs = [Segment::Silence { ms: 500 }, Segment::Speech { ms: 800 }, Segment::Silence { ms: 400 }];
        let a = render(&segs);
        let b = render(&segs);
        assert_eq!(a.samples, b.samples);
        assert_eq!(a.samples.len(), 16 * 1700);
        assert_eq!(a.speech, vec![(500, 1300)]);
        let wav = wav_bytes(&a.samples[..10]);
        assert_eq!(wav.len(), 44 + 20);
        assert_eq!(&wav[..4], b"RIFF");
    }
}
