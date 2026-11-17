//! Downmix + streaming band-limited resampling to 16 kHz mono, and f32 → PCM16.
//!
//! The resampler is a windowed-sinc interpolator with a precomputed polyphase table. When
//! downsampling, the sinc cutoff is lowered to the output Nyquist so content above 8 kHz is
//! filtered out instead of aliasing into the speech band. State is kept between calls, so
//! feeding audio in arbitrary block sizes gives the same output as one big block.

use std::f64::consts::PI;

/// Zero crossings of the sinc on each side, at the (possibly lowered) cutoff.
const ZERO_CROSSINGS: usize = 16;
/// Fractional positions resolved by the table. Phase error ≤ 1/(2·PHASES) of a sample.
const PHASES: usize = 512;

/// Average interleaved frames into one channel. `channels == 0` is treated as mono.
pub fn downmix_interleaved(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels <= 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let inv = 1.0 / channels as f32;
    out.extend(interleaved.chunks_exact(channels).map(|f| f.iter().sum::<f32>() * inv));
}

/// Average planar (non-interleaved) channels into one. Channels shorter than the first are
/// treated as silent past their end.
pub fn downmix_planar(planes: &[&[f32]], out: &mut Vec<f32>) {
    let Some(first) = planes.first() else { return };
    if planes.len() == 1 {
        out.extend_from_slice(first);
        return;
    }
    let inv = 1.0 / planes.len() as f32;
    out.extend((0..first.len()).map(|i| planes.iter().map(|p| p.get(i).copied().unwrap_or(0.0)).sum::<f32>() * inv));
}

/// Clamp to [-1, 1] and scale to i16.
pub fn f32_to_i16(x: f32) -> i16 {
    let x = if x.is_nan() { 0.0 } else { x.clamp(-1.0, 1.0) };
    (x * i16::MAX as f32).round() as i16
}

pub fn i16_to_f32(x: i16) -> f32 {
    x as f32 / i16::MAX as f32
}

/// Streaming mono resampler from `in_rate` to `out_rate`.
pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    /// Input samples advanced per output sample.
    step: f64,
    half: usize,
    /// `PHASES + 1` rows of `2 * half` taps; row `p` is for fractional offset `p / PHASES`.
    table: Vec<f32>,
    /// Input history; `hist[0]` is input sample index `hist_start`.
    hist: Vec<f32>,
    /// Position of the next output sample, in input samples relative to `hist[0]`.
    pos: f64,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        assert!(in_rate > 0 && out_rate > 0, "sample rates must be positive");
        let step = in_rate as f64 / out_rate as f64;
        let cutoff = (out_rate as f64 / in_rate as f64).min(1.0);
        let half = if in_rate == out_rate { 0 } else { (ZERO_CROSSINGS as f64 / cutoff).ceil() as usize };
        let taps = 2 * half;
        let mut table = Vec::with_capacity((PHASES + 1) * taps);
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            // Tap j sits at input offset k = j - half + 1 from floor(pos); distance x = frac - k.
            let row: Vec<f64> = (0..taps)
                .map(|j| {
                    let x = frac - (j as f64 - half as f64 + 1.0);
                    kernel(x, cutoff, half as f64)
                })
                .collect();
            // Normalise each phase to unity DC gain so a constant input stays constant.
            let sum: f64 = row.iter().sum();
            table.extend(row.iter().map(|v| (v / sum) as f32));
        }
        // `half` zeros of history so the first output is centred on the first input sample.
        Self { in_rate, out_rate, step, half, table, hist: vec![0.0; half], pos: half as f64 }
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    /// Append resampled output for `input` to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.half == 0 {
            out.extend_from_slice(input);
            return;
        }
        self.hist.extend_from_slice(input);
        let taps = 2 * self.half;
        // Need hist[floor(pos) + half] to exist.
        while (self.pos.floor() as usize) + self.half < self.hist.len() {
            let base = self.pos.floor() as usize;
            let frac = self.pos - base as f64;
            let phase = (frac * PHASES as f64).round() as usize;
            let row = &self.table[phase * taps..(phase + 1) * taps];
            let start = base + 1 - self.half;
            let window = &self.hist[start..start + taps];
            out.push(window.iter().zip(row).map(|(a, b)| a * b).sum());
            self.pos += self.step;
        }
        // Drop history no future output can reach.
        let keep_from = (self.pos.floor() as usize).saturating_sub(self.half - 1).min(self.hist.len());
        if keep_from > 0 {
            self.hist.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
    }
}

/// Blackman-windowed sinc with normalised `cutoff` (1.0 = input Nyquist).
fn kernel(x: f64, cutoff: f64, half: f64) -> f64 {
    if x.abs() >= half {
        return 0.0;
    }
    let s = if x == 0.0 { 1.0 } else { (PI * cutoff * x).sin() / (PI * cutoff * x) };
    let n = (x + half) / (2.0 * half); // 0..1 across the window
    let w = 0.42 - 0.5 * (2.0 * PI * n).cos() + 0.08 * (4.0 * PI * n).cos();
    cutoff * s * w
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, secs: f64, amp: f32) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n).map(|i| amp * (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Steady-state slice, skipping filter warm-up/tail.
    fn mid(x: &[f32]) -> &[f32] {
        &x[x.len() / 10..x.len() * 9 / 10]
    }

    fn zero_crossings(x: &[f32]) -> usize {
        x.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
    }

    fn resample_all(in_rate: u32, out_rate: u32, x: &[f32]) -> Vec<f32> {
        let mut r = Resampler::new(in_rate, out_rate);
        let mut out = Vec::new();
        r.process(x, &mut out);
        out
    }

    #[test]
    fn passthrough_at_16k() {
        let x = sine(16_000, 440.0, 0.1, 0.5);
        assert_eq!(resample_all(16_000, 16_000, &x), x);
    }

    #[test]
    fn length_matches_ratio() {
        for in_rate in [8_000u32, 22_050, 24_000, 44_100, 48_000, 96_000] {
            let x = sine(in_rate, 300.0, 1.0, 0.5);
            let y = resample_all(in_rate, 16_000, &x);
            // Output lags by `half` input samples; allow that much missing at the end.
            let expected = 16_000.0;
            let lag = (ZERO_CROSSINGS as f64 * (in_rate as f64 / 16_000.0).max(1.0)) * 16_000.0 / in_rate as f64;
            assert!(
                (y.len() as f64) <= expected + 1.0 && (y.len() as f64) >= expected - lag - 2.0,
                "{in_rate}: got {} samples",
                y.len()
            );
        }
    }

    #[test]
    fn in_band_tone_keeps_amplitude_and_pitch() {
        for in_rate in [44_100u32, 48_000] {
            let x = sine(in_rate, 1_000.0, 1.0, 0.5);
            let y = resample_all(in_rate, 16_000, &x);
            let (rx, ry) = (rms(mid(&x)), rms(mid(&y)));
            assert!((ry / rx - 1.0).abs() < 0.02, "{in_rate}: rms {rx} -> {ry}");
            // 1 kHz → 2000 crossings per second; mid() keeps 80 % of the signal.
            let zc = zero_crossings(mid(&y)) as f64 / (mid(&y).len() as f64 / 16_000.0);
            assert!((zc - 2_000.0).abs() < 10.0, "{in_rate}: {zc} crossings/s");
        }
    }

    #[test]
    fn content_above_8k_is_filtered_not_aliased() {
        // 12 kHz at 48 kHz would alias to 4 kHz without the anti-alias filter.
        let x = sine(48_000, 12_000.0, 1.0, 0.5);
        let y = resample_all(48_000, 16_000, &x);
        let att_db = 20.0 * (rms(mid(&y)) / rms(mid(&x))).log10();
        assert!(att_db < -40.0, "only {att_db:.1} dB attenuation");
    }

    #[test]
    fn upsampling_works() {
        let x = sine(8_000, 500.0, 1.0, 0.5);
        let y = resample_all(8_000, 16_000, &x);
        assert!((rms(mid(&y)) / rms(mid(&x)) - 1.0).abs() < 0.02);
    }

    #[test]
    fn dc_stays_constant() {
        let y = resample_all(44_100, 16_000, &vec![0.25; 44_100]);
        for v in mid(&y) {
            assert!((v - 0.25).abs() < 1e-4, "{v}");
        }
    }

    #[test]
    fn streaming_equals_one_shot() {
        let x = sine(44_100, 700.0, 0.5, 0.7);
        let whole = resample_all(44_100, 16_000, &x);
        let mut r = Resampler::new(44_100, 16_000);
        let mut parts = Vec::new();
        let sizes = [1usize, 7, 441, 13, 1024, 3, 512];
        let mut i = 0;
        let mut k = 0;
        while i < x.len() {
            let n = sizes[k % sizes.len()].min(x.len() - i);
            r.process(&x[i..i + n], &mut parts);
            i += n;
            k += 1;
        }
        assert_eq!(whole.len(), parts.len());
        for (a, b) in whole.iter().zip(&parts) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn downmix_and_convert() {
        let mut m = Vec::new();
        downmix_interleaved(&[1.0, 0.0, 0.5, 0.5, -1.0, 1.0], 2, &mut m);
        assert_eq!(m, vec![0.5, 0.5, 0.0]);
        let mut p = Vec::new();
        downmix_planar(&[&[1.0, 1.0], &[0.0]], &mut p);
        assert_eq!(p, vec![0.5, 0.5]);
        assert_eq!(f32_to_i16(1.5), i16::MAX);
        assert_eq!(f32_to_i16(-2.0), -i16::MAX);
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(f32::NAN), 0);
        assert!((i16_to_f32(f32_to_i16(0.3)) - 0.3).abs() < 1e-4);
    }
}
