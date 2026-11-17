//! Timing JSONL with daily rotation (L7).
//!
//! `ssc_core::timing::Timing::with_jsonl` (frozen) appends to one file chosen at start-up, so an
//! agent running past midnight kept writing into yesterday's file. Instead the agent's `Timing`
//! is ring-only and this task copies new events into `<dir>/<YYYY-MM-DD>.jsonl`, choosing the
//! file by each event's own UTC day (`daily_file`), so the split is exact at midnight.
//!
//! New events are found by position in the ring: the last event written is located again (from
//! the end) and everything after it is appended. If it fell out of the ring (more than the ring
//! size between two flushes), the whole ring is written; with a 4096-event ring and a 1 s flush
//! that needs > 4096 events/s, far above the ~10 per answer the pipeline records.

use ssc_core::timing::{Timing, TimingEvent, daily_file};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const FLUSH_EVERY: Duration = Duration::from_secs(1);

/// Copies new timing events to daily JSONL files until `cancel`, then flushes once more.
pub fn spawn(timing: Arc<Timing>, dir: PathBuf, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut w = DailyWriter::new(dir);
        let mut tick = tokio::time::interval(FLUSH_EVERY);
        loop {
            let stop = tokio::select! {
                _ = cancel.cancelled() => true,
                _ = tick.tick() => false,
            };
            let events = timing.events();
            let mut w2 = std::mem::replace(&mut w, DailyWriter::new(PathBuf::new()));
            w = tokio::task::spawn_blocking(move || {
                w2.flush(&events);
                w2
            })
            .await
            .unwrap_or_else(|_| DailyWriter::new(PathBuf::new()));
            if stop {
                break;
            }
        }
    })
}

/// Writes ring snapshots into per-day files, remembering what it already wrote.
pub struct DailyWriter {
    dir: PathBuf,
    last: Option<TimingEvent>,
}

impl DailyWriter {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, last: None }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append the events of `ring` (oldest first) not written yet. Errors are logged, never fatal.
    pub fn flush(&mut self, ring: &[TimingEvent]) {
        if self.dir.as_os_str().is_empty() {
            return;
        }
        let start = match &self.last {
            Some(l) => ring.iter().rposition(|e| e == l).map_or(0, |i| i + 1),
            None => 0,
        };
        let new = &ring[start..];
        if new.is_empty() {
            return;
        }
        if let Err(e) = self.append(new) {
            eprintln!("[ssc-pipeline] timing JSONL write failed: {e}");
        }
        self.last = new.last().cloned();
    }

    fn append(&self, events: &[TimingEvent]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let mut i = 0;
        while i < events.len() {
            let path = daily_file(&self.dir, events[i].t_ms);
            let mut f = std::io::BufWriter::new(std::fs::OpenOptions::new().create(true).append(true).open(&path)?);
            while i < events.len() && daily_file(&self.dir, events[i].t_ms) == path {
                serde_json::to_writer(&mut f, &events[i]).map_err(std::io::Error::other)?;
                f.write_all(b"\n")?;
                i += 1;
            }
            f.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::timing::Stage;
    use uuid::Uuid;

    const DAY: u64 = 86_400_000;
    // 2026-11-17 23:59:59.000 UTC
    const LATE: u64 = 1_794_959_999_000;

    fn lines(p: &Path) -> Vec<TimingEvent> {
        std::fs::read_to_string(p).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn rotates_at_midnight_and_writes_each_event_once() {
        let dir = std::env::temp_dir().join(format!("ssc-timing-{}", Uuid::new_v4()));
        let t = Timing::new(16);
        let id = Uuid::new_v4();
        let mut w = DailyWriter::new(dir.clone());
        t.record(id, Stage::SpeechEnd, LATE);
        t.record(id, Stage::TriggerFired, LATE + 300);
        w.flush(&t.events());
        w.flush(&t.events()); // nothing new
        t.record(id, Stage::FirstToken, LATE + 1_200); // past midnight
        t.record(id, Stage::FrameSent, LATE + 1_250);
        w.flush(&t.events());
        let day1 = daily_file(&dir, LATE);
        let day2 = daily_file(&dir, LATE + DAY);
        assert!(day1.ends_with("2026-11-17.jsonl"), "{day1:?}");
        assert!(day2.ends_with("2026-11-18.jsonl"), "{day2:?}");
        assert_eq!(lines(&day1).iter().map(|e| e.stage).collect::<Vec<_>>(), [Stage::SpeechEnd, Stage::TriggerFired]);
        assert_eq!(lines(&day2).iter().map(|e| e.stage).collect::<Vec<_>>(), [Stage::FirstToken, Stage::FrameSent]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ring_overflow_between_flushes_writes_what_is_left() {
        let dir = std::env::temp_dir().join(format!("ssc-timing-{}", Uuid::new_v4()));
        let t = Timing::new(4);
        let mut w = DailyWriter::new(dir.clone());
        let id = Uuid::new_v4();
        t.record(id, Stage::SpeechEnd, LATE - 10_000);
        w.flush(&t.events());
        for i in 0..6 {
            t.record(id, Stage::FirstToken, LATE - 9_000 + i);
        }
        w.flush(&t.events());
        assert_eq!(lines(&daily_file(&dir, LATE)).len(), 1 + 4);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
