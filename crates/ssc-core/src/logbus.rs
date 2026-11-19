//! CR-23 log hub. Every agent log line goes to stderr (as before) and into a ring buffer with a
//! broadcast feed, so the phone's Logs tab can show the backlog and new lines.
//!
//! Only operational messages belong here: never prompts, transcripts or screen text (the debug
//! answer dump prints to stderr directly and bypasses this hub).

use crate::protocol::{LogEntry, LogLevel};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use tokio::sync::broadcast;

/// Lines kept for the phone's backlog.
pub const BACKLOG: usize = 200;
const MAX_TEXT_CHARS: usize = 500;
const MAX_SOURCE_CHARS: usize = 32;

struct Hub {
    ring: Mutex<VecDeque<LogEntry>>,
    tx: broadcast::Sender<LogEntry>,
}

fn hub() -> &'static Hub {
    static H: OnceLock<Hub> = OnceLock::new();
    H.get_or_init(|| Hub { ring: Mutex::new(VecDeque::with_capacity(BACKLOG)), tx: broadcast::channel(256).0 })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max - 1).chain(['…']).collect() }
}

/// Log one line: stderr as `[source] text`, plus the ring buffer and the live feed.
pub fn emit(level: LogLevel, source: &str, text: impl AsRef<str>) {
    let text = text.as_ref();
    eprintln!("[{source}] {text}");
    let text = clip(text, MAX_TEXT_CHARS);
    if text.is_empty() {
        return;
    }
    let source = clip(if source.is_empty() { "agent" } else { source }, MAX_SOURCE_CHARS);
    let e = LogEntry { t: now_ms(), level, source, text };
    let h = hub();
    {
        let mut r = h.ring.lock().unwrap_or_else(|p| p.into_inner());
        if r.len() == BACKLOG {
            r.pop_front();
        }
        r.push_back(e.clone());
    }
    let _ = h.tx.send(e); // no subscribers is fine
}

pub fn info(source: &str, text: impl AsRef<str>) {
    emit(LogLevel::Info, source, text);
}
pub fn warn(source: &str, text: impl AsRef<str>) {
    emit(LogLevel::Warn, source, text);
}
pub fn error(source: &str, text: impl AsRef<str>) {
    emit(LogLevel::Error, source, text);
}

/// Level for a free-text message from code that logs without one (deterministic keywords).
pub fn infer_level(text: &str) -> LogLevel {
    let t = text.to_ascii_lowercase();
    if ["failed", "could not", "panicked", "error", "refused", "denied"].iter().any(|k| t.contains(k)) {
        LogLevel::Error
    } else if ["retry", "retrying", "fallback", "falling back", "no image", "timed out", "timeout", "missing", "skipp"]
        .iter()
        .any(|k| t.contains(k))
    {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

/// The last [`BACKLOG`] lines, oldest first.
pub fn backlog() -> Vec<LogEntry> {
    hub().ring.lock().unwrap_or_else(|p| p.into_inner()).iter().cloned().collect()
}

/// New lines from now on.
pub fn subscribe() -> broadcast::Receiver<LogEntry> {
    hub().tx.subscribe()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_last_lines_clipped_and_feeds_subscribers() {
        let mut rx = subscribe();
        for i in 0..(BACKLOG + 5) {
            info("t", format!("line {i}"));
        }
        let long = "x".repeat(900);
        error("a-very-long-source-name-that-goes-past-thirty-two-chars", &long);
        let b = backlog();
        assert!(b.len() <= BACKLOG);
        let last = b.last().unwrap();
        assert_eq!(last.level, LogLevel::Error);
        assert_eq!(last.text.chars().count(), MAX_TEXT_CHARS);
        assert!(last.text.ends_with('…'));
        assert_eq!(last.source.chars().count(), MAX_SOURCE_CHARS);
        assert!(!b.iter().any(|e| e.text == "line 0"), "oldest dropped");
        assert_eq!(rx.try_recv().unwrap().text, "line 0", "subscribers get every line");
    }

    #[test]
    fn levels_from_keywords() {
        assert_eq!(infer_level("screen hotkey: could not read the focused window"), LogLevel::Error);
        assert_eq!(infer_level("answer abc failed: rate limited"), LogLevel::Error);
        assert_eq!(infer_level("screen hotkey: no image (permission); answering from the window text"), LogLevel::Warn);
        assert_eq!(infer_level("lan_tls: A record update failed; retrying"), LogLevel::Error);
        assert_eq!(infer_level("session started"), LogLevel::Info);
    }
}
