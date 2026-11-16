//! Per-stage timestamps (Build Sheet §4.6). Every stage records `(answer_id, stage, t_ms)` on the
//! PC clock. Events go to a ring buffer (debug panel) and optionally a JSONL file.
//!
//! `phone_rendered` is derived on the PC (no cross-device clock): `frame_sent` of the first
//! delta + RTT/2 (from heartbeat echo) + the phone's reported `delta_ms`.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    SpeechEnd,
    TranscriptFinal,
    TriggerFired,
    DecisionDone,
    ContextReady,
    ModelRequest,
    FirstToken,
    FrameSent,
    PhoneRendered,
}

impl Stage {
    pub const ALL: [Stage; 9] = [
        Stage::SpeechEnd,
        Stage::TranscriptFinal,
        Stage::TriggerFired,
        Stage::DecisionDone,
        Stage::ContextReady,
        Stage::ModelRequest,
        Stage::FirstToken,
        Stage::FrameSent,
        Stage::PhoneRendered,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingEvent {
    pub answer_id: Uuid,
    pub stage: Stage,
    pub t_ms: u64,
}

pub struct Timing {
    cap: usize,
    ring: Mutex<VecDeque<TimingEvent>>,
    writer: Option<Mutex<BufWriter<File>>>,
}

impl Timing {
    pub fn new(cap: usize) -> Self {
        Self { cap: cap.max(1), ring: Mutex::new(VecDeque::new()), writer: None }
    }

    /// Also append every event as one JSON line to `path` (created if missing).
    pub fn with_jsonl(mut self, path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        self.writer = Some(Mutex::new(BufWriter::new(f)));
        Ok(self)
    }

    pub fn record(&self, answer_id: Uuid, stage: Stage, t_ms: u64) {
        let ev = TimingEvent { answer_id, stage, t_ms };
        if let Some(w) = &self.writer {
            let mut w = w.lock().unwrap();
            // Timing must never break the pipeline; a failed write is dropped.
            if serde_json::to_writer(&mut *w, &ev).is_ok() {
                let _ = w.write_all(b"\n").and_then(|_| w.flush());
            }
        }
        let mut ring = self.ring.lock().unwrap();
        if ring.len() == self.cap {
            ring.pop_front();
        }
        ring.push_back(ev);
    }

    pub fn events(&self) -> Vec<TimingEvent> {
        self.ring.lock().unwrap().iter().cloned().collect()
    }

    /// `to - from` in ms for every answer that has both stages (first occurrence of each).
    pub fn spans(&self, from: Stage, to: Stage) -> Vec<u64> {
        spans(&self.events(), from, to)
    }
}

pub fn spans(events: &[TimingEvent], from: Stage, to: Stage) -> Vec<u64> {
    let mut per: HashMap<Uuid, (Option<u64>, Option<u64>)> = HashMap::new();
    for e in events {
        let slot = per.entry(e.answer_id).or_default();
        if e.stage == from && slot.0.is_none() {
            slot.0 = Some(e.t_ms);
        }
        if e.stage == to && slot.1.is_none() {
            slot.1 = Some(e.t_ms);
        }
    }
    let mut out: Vec<u64> = per.values().filter_map(|(a, b)| Some(b.as_ref()?.saturating_sub(*a.as_ref()?))).collect();
    out.sort_unstable();
    out
}

/// Nearest-rank percentile (`p` in 0..=100). None for an empty slice.
pub fn percentile(values: &[u64], p: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let rank = ((p.clamp(0.0, 100.0) / 100.0) * v.len() as f64).ceil() as usize;
    Some(v[rank.saturating_sub(1).min(v.len() - 1)])
}

/// `<dir>/<YYYY-MM-DD>.jsonl` for the UTC day of `now_ms`.
pub fn daily_file(dir: &Path, now_ms: u64) -> PathBuf {
    let (y, m, d) = civil_from_days((now_ms / 86_400_000) as i64);
    dir.join(format!("{y:04}-{m:02}-{d:02}.jsonl"))
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), Some(50));
        assert_eq!(percentile(&v, 95.0), Some(95));
        assert_eq!(percentile(&v, 100.0), Some(100));
        assert_eq!(percentile(&v, 0.0), Some(1));
        assert_eq!(percentile(&[7], 95.0), Some(7));
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[30, 10, 20], 50.0), Some(20));
    }

    #[test]
    fn spans_per_answer_and_ring_cap() {
        let t = Timing::new(3);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        t.record(a, Stage::SpeechEnd, 1000);
        t.record(a, Stage::FirstToken, 1600);
        t.record(b, Stage::SpeechEnd, 2000);
        t.record(b, Stage::FirstToken, 2900); // evicts a/SpeechEnd
        assert_eq!(t.events().len(), 3);
        assert_eq!(t.spans(Stage::SpeechEnd, Stage::FirstToken), vec![900]);
    }

    #[test]
    fn jsonl_written() {
        let dir = std::env::temp_dir().join(format!("ssc-timing-{}", Uuid::new_v4()));
        let path = daily_file(&dir, 1_763_290_000_123);
        assert!(path.ends_with("2025-11-16.jsonl"));
        let t = Timing::new(10).with_jsonl(&path).unwrap();
        t.record(Uuid::nil(), Stage::FrameSent, 5);
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 1);
        let ev: TimingEvent = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(ev.stage, Stage::FrameSent);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_773), (2026, 11, 16));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
