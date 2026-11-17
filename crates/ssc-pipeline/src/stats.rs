//! Debug-panel numbers: p50/p95 per stage from the timing ring buffer.

use serde::Serialize;
use ssc_core::timing::{Stage, TimingEvent, percentile, spans};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StageStat {
    /// `from → to`, e.g. `speech_end → first_token`.
    pub from: Stage,
    pub to: Stage,
    pub count: usize,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
}

/// Consecutive stage pairs, then the end-to-end spans the MVP targets are defined on.
pub fn stage_stats(events: &[TimingEvent]) -> Vec<StageStat> {
    let mut pairs: Vec<(Stage, Stage)> = Stage::ALL.windows(2).map(|w| (w[0], w[1])).collect();
    pairs.push((Stage::SpeechEnd, Stage::FirstToken));
    pairs.push((Stage::SpeechEnd, Stage::PhoneRendered));
    pairs.push((Stage::TriggerFired, Stage::FirstToken));
    pairs
        .into_iter()
        .map(|(from, to)| {
            let v = spans(events, from, to);
            StageStat { from, to, count: v.len(), p50_ms: percentile(&v, 50.0), p95_ms: percentile(&v, 95.0) }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn computes_pairs_and_end_to_end() {
        let a = Uuid::new_v4();
        let ev = |stage, t_ms| TimingEvent { answer_id: a, stage, t_ms };
        let events = vec![ev(Stage::SpeechEnd, 100), ev(Stage::FirstToken, 900), ev(Stage::PhoneRendered, 1000)];
        let s = stage_stats(&events);
        let e2e = s.iter().find(|x| x.from == Stage::SpeechEnd && x.to == Stage::PhoneRendered).unwrap();
        assert_eq!((e2e.count, e2e.p50_ms, e2e.p95_ms), (1, Some(900), Some(900)));
        let ft = s.iter().find(|x| x.from == Stage::FrameSent && x.to == Stage::PhoneRendered).unwrap();
        assert_eq!(ft.count, 0);
        assert_eq!(s.len(), 8 + 3);
    }
}
