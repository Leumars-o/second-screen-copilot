//! Speculative drafting: start the answer on the partial transcript, and when the final transcript
//! arrives keep the draft unless the meaning changed — measured deterministically as the
//! normalised edit distance between the two texts (> 0.2 → cancel and restart on the final text).

use ssc_core::traits::Cancel;
use std::future::Future;
use tokio::task::JoinHandle;
use tokio_util::sync::DropGuard;

pub const DEFAULT_THRESHOLD: f64 = 0.2;

/// Lowercase, punctuation removed, whitespace collapsed: "What's the price?" ≈ "whats the price".
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        let w: String = w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect();
        if w.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&w);
    }
    out
}

/// Levenshtein distance over chars.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Edit distance of the normalised texts divided by the longer length, in 0..=1.
pub fn normalized_edit_distance(a: &str, b: &str) -> f64 {
    let (a, b) = (normalize(a), normalize(b));
    let len = a.chars().count().max(b.chars().count());
    if len == 0 {
        return 0.0;
    }
    levenshtein(&a, &b) as f64 / len as f64
}

pub fn meaning_changed(partial: &str, final_text: &str, threshold: f64) -> bool {
    normalized_edit_distance(partial, final_text) > threshold
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finalized {
    Kept,
    Restarted,
}

/// A running draft. Dropping it cancels the draft.
pub struct Speculation<T> {
    basis: String,
    cancel: Cancel,
    handle: JoinHandle<T>,
    _guard: DropGuard,
}

impl<T: Send + 'static> Speculation<T> {
    /// Launch `launch(text, cancel)` on a partial (or final) transcript.
    pub fn start<F, Fut>(text: impl Into<String>, launch: &F) -> Self
    where
        F: Fn(String, Cancel) -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let basis = text.into();
        let cancel = Cancel::new();
        let handle = tokio::spawn(launch(basis.clone(), cancel.clone()));
        Self { basis, _guard: cancel.clone().drop_guard(), cancel, handle }
    }

    /// The transcript this draft was started from.
    pub fn basis(&self) -> &str {
        &self.basis
    }

    /// The final transcript arrived: keep this draft, or cancel it and start again on `final_text`.
    pub fn finalize<F, Fut>(self, final_text: &str, threshold: f64, launch: &F) -> (Self, Finalized)
    where
        F: Fn(String, Cancel) -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        if meaning_changed(&self.basis, final_text, threshold) {
            self.cancel.cancel();
            self.handle.abort();
            (Self::start(final_text, launch), Finalized::Restarted)
        } else {
            (self, Finalized::Kept)
        }
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Wait for the draft's result. `None` if it was aborted or panicked.
    pub async fn join(self) -> Option<T> {
        let Self { handle, _guard, .. } = self;
        let r = handle.await.ok();
        _guard.disarm();
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_basics() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(normalized_edit_distance("", ""), 0.0);
        assert_eq!(normalized_edit_distance("What's the price?", "whats the PRICE"), 0.0);
        assert_eq!(normalized_edit_distance("abc", ""), 1.0);
    }

    #[test]
    fn meaning_change_threshold() {
        // Small ASR fix-ups keep the draft.
        assert!(!meaning_changed("how much does the pro plan cost", "how much does the pro plan cost?", 0.2));
        assert!(!meaning_changed("how much is the pro plan", "how much is the pro plan per seat", 0.4));
        assert!(!meaning_changed("can you walk me through the setup", "could you walk me through the setup", 0.2));
        // The question changed.
        assert!(meaning_changed("how much is the pro plan", "how much is the pro plan per seat", 0.2));
        assert!(meaning_changed("what does it cost", "when can we schedule a demo", 0.2));
    }
}
