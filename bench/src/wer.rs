//! Word error rate: word-level Levenshtein distance / reference length, after normalising case and
//! punctuation (apostrophes kept: "don't" ≠ "dont").

pub fn normalise(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(String::from)
        .collect()
}

/// (substitutions + deletions + insertions, reference words).
pub fn edits(reference: &str, hypothesis: &str) -> (usize, usize) {
    let r = normalise(reference);
    let h = normalise(hypothesis);
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    for (i, rw) in r.iter().enumerate() {
        let mut cur = vec![i + 1; h.len() + 1];
        for (j, hw) in h.iter().enumerate() {
            let sub = prev[j] + usize::from(rw != hw);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    (prev[h.len()], r.len())
}

/// WER in 0..; None when the reference is empty.
pub fn wer(reference: &str, hypothesis: &str) -> Option<f64> {
    let (e, n) = edits(reference, hypothesis);
    (n > 0).then(|| e as f64 / n as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wer_cases() {
        assert_eq!(wer("What is your pricing?", "what is your pricing"), Some(0.0));
        assert_eq!(wer("what is your pricing", "what is the pricing"), Some(0.25));
        assert_eq!(wer("what is your pricing", "what is pricing"), Some(0.25));
        assert_eq!(wer("what is your pricing", "so what is your pricing now"), Some(0.5));
        assert_eq!(wer("a b", ""), Some(1.0));
        assert_eq!(wer("", "anything"), None);
        assert_eq!(edits("Don't stop", "dont stop"), (1, 2));
    }
}
