//! Line-level screen diff (Build Sheet §6 W1-SCREEN).
//!
//! Lines are the trimmed, non-empty lines of every node's text, in tree (reading) order.
//! A line of `next` counts as unchanged if an equal line exists in `prev` (multiset match, so
//! duplicates are counted). This ignores pure reordering, which on screen is usually layout
//! churn rather than new content, and runs in O(n).
//!
//! Ordering ("newest content first"): added lines form contiguous runs (hunks) in `next`.
//! New content in chats, transcripts and documents usually appears lower in the window, so
//! hunks are emitted bottom-most first; lines inside a hunk keep reading order. When the
//! [`SCREEN_DIFF_MAX_BYTES`] cap is hit, the oldest (top-most) content is dropped: a hunk that
//! does not fit keeps its last whole lines. Lines are never cut, except the newest line when it
//! alone exceeds the cap: then its start is kept.

use ssc_core::types::{SCREEN_DIFF_MAX_BYTES, ScreenDiff, ScreenSnapshot};
use std::collections::HashMap;

/// The diffable lines of a snapshot: every node's text split on newlines, trimmed, empties dropped.
pub fn snapshot_lines(snapshot: &ScreenSnapshot) -> Vec<&str> {
    snapshot.nodes.iter().flat_map(|n| n.text.lines()).map(str::trim).filter(|l| !l.is_empty()).collect()
}

/// What changed from `prev` to `next`. `prev = None` means everything in `next` is new.
pub fn diff(prev: Option<&ScreenSnapshot>, next: &ScreenSnapshot) -> ScreenDiff {
    let prev_lines = prev.map(snapshot_lines).unwrap_or_default();
    diff_lines(&prev_lines, &snapshot_lines(next), SCREEN_DIFF_MAX_BYTES)
}

/// Core of [`diff`], over plain lines and with an explicit byte cap (for tests).
/// The cap applies to [`ScreenDiff::text`], i.e. lines plus their `\n` separators.
pub fn diff_lines(prev: &[&str], next: &[&str], max_bytes: usize) -> ScreenDiff {
    let mut available: HashMap<&str, usize> = HashMap::with_capacity(prev.len());
    for l in prev {
        *available.entry(l).or_default() += 1;
    }

    // Contiguous runs of added lines, in reading order.
    let mut hunks: Vec<Vec<&str>> = Vec::new();
    let mut in_hunk = false;
    for l in next {
        match available.get_mut(l) {
            Some(n) if *n > 0 => {
                *n -= 1;
                in_hunk = false;
            }
            _ => {
                if !in_hunk {
                    hunks.push(Vec::new());
                    in_hunk = true;
                }
                hunks.last_mut().expect("pushed above").push(l);
            }
        }
    }
    let removed_lines = available.values().sum();

    let mut added_lines = Vec::new();
    let mut used = 0usize; // bytes of added_lines joined with '\n'
    let mut truncated = false;
    'hunks: for hunk in hunks.iter().rev() {
        let mut fitted: Vec<String> = Vec::new();
        // Walk the hunk bottom-up so its newest lines are kept when it doesn't fit.
        for line in hunk.iter().rev() {
            let sep = usize::from(used > 0);
            let room = max_bytes.saturating_sub(used + sep);
            if line.len() <= room {
                used += sep + line.len();
                fitted.push((*line).to_string());
            } else {
                truncated = true;
                if used == 0 && room > 0 {
                    let cut = floor_char_boundary(line, room);
                    if cut > 0 {
                        fitted.push(line[..cut].to_string());
                    }
                }
                fitted.reverse();
                added_lines.extend(fitted);
                break 'hunks;
            }
        }
        fitted.reverse();
        added_lines.extend(fitted);
    }

    ScreenDiff { added_lines, removed_lines, truncated }
}

/// Largest index `<= max` that is a char boundary of `s`.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    (0..=max).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::mock::MockScreen;

    fn snap(lines: &[&str]) -> ScreenSnapshot {
        MockScreen::snapshot("Zoom", lines)
    }

    #[test]
    fn no_change_is_empty() {
        let a = snap(&["Pricing", "Q3 revenue 4.2M", "Next steps"]);
        let d = diff(Some(&a), &a.clone());
        assert!(d.is_empty());
        assert!(!d.truncated);
        assert_eq!(d.changed_lines(), 0);
    }

    #[test]
    fn whitespace_and_blank_lines_are_ignored() {
        let a = snap(&["Pricing", "", "Total"]);
        let b = snap(&["  Pricing  ", "   ", "Total\n"]);
        assert!(diff(Some(&a), &b).is_empty());
    }

    #[test]
    fn first_snapshot_is_all_added() {
        let d = diff(None, &snap(&["a", "b"]));
        assert_eq!(d.added_lines, vec!["a", "b"]);
        assert_eq!(d.removed_lines, 0);
    }

    #[test]
    fn append_reports_only_new_lines() {
        let a = snap(&["Alice: hi", "Bob: hello"]);
        let b = snap(&["Alice: hi", "Bob: hello", "Alice: what's the price?", "Alice: and SLA?"]);
        let d = diff(Some(&a), &b);
        assert_eq!(d.added_lines, vec!["Alice: what's the price?", "Alice: and SLA?"]);
        assert_eq!(d.removed_lines, 0);
        assert!(!d.truncated);
        assert_eq!(d.text(), "Alice: what's the price?\nAlice: and SLA?");
    }

    #[test]
    fn replace_counts_removed_and_added() {
        let a = snap(&["Title", "Plan A: 10 seats", "Footer"]);
        let b = snap(&["Title", "Plan B: 25 seats", "Footer"]);
        let d = diff(Some(&a), &b);
        assert_eq!(d.added_lines, vec!["Plan B: 25 seats"]);
        assert_eq!(d.removed_lines, 1);
        assert_eq!(d.changed_lines(), 2);
    }

    #[test]
    fn removal_only() {
        let d = diff(Some(&snap(&["a", "b", "c"])), &snap(&["a"]));
        assert!(d.added_lines.is_empty());
        assert_eq!(d.removed_lines, 2);
        assert!(!d.is_empty());
    }

    #[test]
    fn duplicates_are_counted() {
        let d = diff_lines(&["ok"], &["ok", "ok"], 2048);
        assert_eq!(d.added_lines, vec!["ok"]);
        let d = diff_lines(&["ok", "ok"], &["ok"], 2048);
        assert_eq!(d.removed_lines, 1);
    }

    #[test]
    fn reorder_is_not_a_change() {
        assert!(diff_lines(&["a", "b"], &["b", "a"], 2048).is_empty());
    }

    #[test]
    fn newest_hunk_first_reading_order_within() {
        // Two separate insertions: one near the top, one at the bottom (newest).
        let prev = ["h", "x", "y"];
        let next = ["h", "top1", "top2", "x", "y", "bot1", "bot2"];
        let d = diff_lines(&prev, &next, 2048);
        assert_eq!(d.added_lines, vec!["bot1", "bot2", "top1", "top2"]);
    }

    #[test]
    fn cap_keeps_newest_and_respects_bytes() {
        let lines: Vec<String> = (0..500).map(|i| format!("line number {i:04}")).collect();
        let next: Vec<&str> = lines.iter().map(String::as_str).collect();
        let d = diff_lines(&[], &next, SCREEN_DIFF_MAX_BYTES);
        assert!(d.truncated);
        assert!(d.text().len() <= SCREEN_DIFF_MAX_BYTES, "{} bytes", d.text().len());
        // The newest (bottom) line survives; the oldest is dropped.
        assert_eq!(d.added_lines.last().unwrap(), "line number 0499");
        assert!(!d.added_lines.iter().any(|l| l == "line number 0000"));
        // The kept lines are the contiguous tail, in reading order.
        let n = d.added_lines.len();
        assert_eq!(d.added_lines, next[500 - n..].to_vec());
        // And it is tight: one more line would not have fit.
        assert!(d.text().len() + 1 + "line number 0000".len() > SCREEN_DIFF_MAX_BYTES);
    }

    #[test]
    fn cap_via_public_diff() {
        let lines: Vec<String> = (0..1000).map(|i| format!("row {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let d = diff(None, &snap(&refs));
        assert!(d.truncated);
        assert!(d.text().len() <= SCREEN_DIFF_MAX_BYTES);
        assert_eq!(d.added_lines.last().unwrap(), "row 999");
    }

    #[test]
    fn cap_drops_older_hunks() {
        let prev = ["keep"];
        let next = ["aaaaaaaaaa", "keep", "bbbbbbbbbb"];
        let d = diff_lines(&prev, &next, 15);
        assert_eq!(d.added_lines, vec!["bbbbbbbbbb"]);
        assert!(d.truncated);
    }

    #[test]
    fn single_long_line_is_cut_on_char_boundary() {
        let long = "é".repeat(2000); // 4000 bytes
        let d = diff_lines(&[], &[long.as_str()], SCREEN_DIFF_MAX_BYTES);
        assert!(d.truncated);
        assert_eq!(d.added_lines.len(), 1);
        assert!(d.text().len() <= SCREEN_DIFF_MAX_BYTES);
        assert!(d.text().len() >= SCREEN_DIFF_MAX_BYTES - 1);
    }

    #[test]
    fn exact_fit_is_not_truncated() {
        let d = diff_lines(&[], &["abc", "de"], 6);
        assert_eq!(d.added_lines, vec!["abc", "de"]);
        assert!(!d.truncated);
    }
}
