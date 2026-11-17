//! Summary statistics. Percentiles use `ssc_core::timing::percentile` (nearest rank) so the bench
//! and the agent's debug panel agree on what "p95" means.

use ssc_core::timing::percentile;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub p50: u64,
    pub p95: u64,
    pub min: u64,
    pub max: u64,
    pub mean: f64,
}

impl Summary {
    /// None for an empty sample.
    pub fn of(values: &[u64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        Some(Self {
            n: values.len(),
            p50: percentile(values, 50.0)?,
            p95: percentile(values, 95.0)?,
            min: *values.iter().min()?,
            max: *values.iter().max()?,
            mean: values.iter().sum::<u64>() as f64 / values.len() as f64,
        })
    }
}

/// Mean of f64 values; None when empty.
pub fn mean_f64(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

/// Nearest-rank percentile for f64 values (e.g. CPU %).
pub fn percentile_f64(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let rank = ((p.clamp(0.0, 100.0) / 100.0) * v.len() as f64).ceil() as usize;
    Some(v[rank.saturating_sub(1).min(v.len() - 1)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary() {
        let s = Summary::of(&[300, 100, 200, 400]).unwrap();
        assert_eq!((s.n, s.p50, s.p95, s.min, s.max), (4, 200, 400, 100, 400));
        assert!((s.mean - 250.0).abs() < 1e-9);
        assert_eq!(Summary::of(&[]), None);
    }

    #[test]
    fn f64_percentiles() {
        let v: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(percentile_f64(&v, 50.0), Some(10.0));
        assert_eq!(percentile_f64(&v, 95.0), Some(19.0));
        assert_eq!(mean_f64(&[1.0, 2.0]), Some(1.5));
        assert_eq!(percentile_f64(&[], 50.0), None);
    }
}
