//! Maps a provider's stream-relative time (seconds of audio sent) back to capture time.

use ssc_core::types::SAMPLE_RATE_HZ;
use std::collections::VecDeque;

const MAX_ENTRIES: usize = 4096;

/// Piecewise-linear map from "samples sent on this stream" to unix ms. A new piece starts
/// whenever a chunk's `t0_ms` doesn't follow on from the previous one (a capture gap).
#[derive(Debug, Default)]
pub struct Timeline {
    /// (first stream sample, unix ms of that sample)
    pieces: VecDeque<(u64, u64)>,
    total: u64,
}

impl Timeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a chunk of `n` samples captured at `t0_ms`, in send order.
    pub fn push(&mut self, t0_ms: u64, n: usize) {
        let continuous = self.pieces.back().is_some_and(|&(s, t)| self.ms_at(s, t, self.total).abs_diff(t0_ms) <= 1);
        if !continuous {
            self.pieces.push_back((self.total, t0_ms));
            if self.pieces.len() > MAX_ENTRIES {
                self.pieces.pop_front();
            }
        }
        self.total += n as u64;
    }

    /// Samples sent so far.
    pub fn total_samples(&self) -> u64 {
        self.total
    }

    /// Stream offset in seconds → unix ms. Offsets before the first chunk clamp to it.
    pub fn to_unix_ms(&self, secs: f64) -> u64 {
        let sample = (secs.max(0.0) * SAMPLE_RATE_HZ as f64).round() as u64;
        let idx = self.pieces.partition_point(|&(s, _)| s <= sample);
        match idx.checked_sub(1).and_then(|i| self.pieces.get(i)).or(self.pieces.front()) {
            Some(&(s, t)) => self.ms_at(s, t, sample.max(s)),
            None => 0,
        }
    }

    fn ms_at(&self, start: u64, t0: u64, sample: u64) -> u64 {
        t0 + (sample - start) * 1000 / SAMPLE_RATE_HZ as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuous_and_gapped() {
        let mut tl = Timeline::new();
        tl.push(10_000, 320); // 10_000..10_020
        tl.push(10_020, 320); // continuous
        tl.push(20_000, 320); // gap: new piece at sample 640
        assert_eq!(tl.to_unix_ms(0.0), 10_000);
        assert_eq!(tl.to_unix_ms(0.03), 10_030);
        assert_eq!(tl.to_unix_ms(0.04), 20_000);
        assert_eq!(tl.to_unix_ms(0.05), 20_010);
        assert_eq!(tl.total_samples(), 960);
        assert_eq!(Timeline::new().to_unix_ms(1.0), 0);
    }
}
