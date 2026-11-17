//! Deterministic parser for the answer output grammar in `contracts/prompts/README.md`.
//!
//! ```text
//! 1. <cue, 2-6 words>     ← block "cue" (3-7 lines)
//! ---                     ← switches to block "detail"
//! 1: <supporting point>   ← expand for cue 1
//! FACT: <fact>            ← anywhere; kept only with SOURCE
//! SOURCE: <doc name>
//! NOT_IN_DOCS             ← alone
//! ```
//!
//! [`CueStream`] classifies streamed text into `cue` / `detail` deltas with minimal buffering (only a
//! line that could still become `---` is held back). [`parse_answer`] produces the final card.

use ssc_core::protocol::{AnswerEnd, AnswerEndReason, Block, Cue};
use uuid::Uuid;

pub const MAX_CUES: usize = 7;
pub const MIN_CUES: usize = 3;
pub const MAX_CUE_WORDS: usize = 6;

/// Streaming classifier. Feed raw model text; get `(block, text)` deltas ready for `answer.delta`.
#[derive(Debug, Clone)]
pub struct CueStream {
    block: Block,
    line: String,
    pending: String,
    full: String,
}

impl Default for CueStream {
    fn default() -> Self {
        Self::new(Block::Cue)
    }
}

impl CueStream {
    /// `start` is `Cue` for a normal answer; `Detail` for a stream that only carries detail
    /// (the strong tier, or "expand all").
    pub fn new(start: Block) -> Self {
        Self { block: start, line: String::new(), pending: String::new(), full: String::new() }
    }

    pub fn block(&self) -> Block {
        self.block
    }

    /// All text pushed so far (including the `---` separator), for [`parse_answer`].
    pub fn text(&self) -> &str {
        &self.full
    }

    pub fn push(&mut self, text: &str) -> Vec<(Block, String)> {
        self.full.push_str(text);
        let mut out = Vec::new();
        for c in text.chars() {
            if c == '\n' {
                if self.line.trim() == "---" {
                    self.block = Block::Detail;
                    self.pending.clear();
                } else {
                    self.pending.push('\n');
                    let p = std::mem::take(&mut self.pending);
                    emit(&mut out, self.block, p);
                }
                self.line.clear();
            } else {
                self.line.push(c);
                self.pending.push(c);
            }
        }
        if !self.pending.is_empty() && !could_be_separator(&self.line) {
            let p = std::mem::take(&mut self.pending);
            emit(&mut out, self.block, p);
        }
        out
    }

    /// End of stream: flush whatever is held.
    pub fn finish(&mut self) -> Vec<(Block, String)> {
        let mut out = Vec::new();
        if self.line.trim() == "---" {
            self.block = Block::Detail;
        } else if !self.pending.is_empty() {
            let p = std::mem::take(&mut self.pending);
            emit(&mut out, self.block, p);
        }
        self.pending.clear();
        self.line.clear();
        out
    }

    /// Cue lines seen so far (for building the strong-tier detail prompt).
    pub fn cues(&self) -> Vec<String> {
        parse_answer(&self.full).cues.into_iter().map(|c| c.text).collect()
    }
}

fn could_be_separator(line: &str) -> bool {
    let t = line.trim();
    t.len() <= 3 && t.chars().all(|c| c == '-')
}

fn emit(out: &mut Vec<(Block, String)>, block: Block, text: String) {
    if text.is_empty() {
        return;
    }
    match out.last_mut() {
        Some((b, t)) if *b == block => t.push_str(&text),
        _ => out.push((block, text)),
    }
}

/// The parsed answer card (the content of `answer.end`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedAnswer {
    pub cues: Vec<Cue>,
    pub fact: Option<String>,
    pub source_doc: Option<String>,
    pub not_in_docs: bool,
}

impl ParsedAnswer {
    /// The 3–7 cue rule. Fact answers and "Not in your docs" may have zero cues.
    pub fn meets_cue_rule(&self) -> bool {
        let n = self.cues.len();
        (MIN_CUES..=MAX_CUES).contains(&n) || (n == 0 && (self.fact.is_some() || self.not_in_docs))
    }

    pub fn into_answer_end(self, answer_id: Uuid, reason: AnswerEndReason) -> AnswerEnd {
        AnswerEnd {
            answer_id,
            reason,
            cues: self.cues,
            fact: self.fact,
            source_doc: self.source_doc,
            not_in_docs: self.not_in_docs,
        }
    }
}

/// `"12. text"` (sep '.') or `"12: text"` (sep ':') → (12, "text").
fn numbered(line: &str, sep: char) -> Option<(usize, String)> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = line[digits..].strip_prefix(sep)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    // Markdown emphasis is noise on a glanceable card.
    let text: String = rest.chars().filter(|c| *c != '*' && *c != '`').collect();
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some((line[..digits].parse().ok()?, text.to_string()))
}

fn strip_prefix_ci<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let head = line.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| line[prefix.len()..].trim())
}

fn truncate_words(s: &str, n: usize) -> String {
    s.split_whitespace().take(n).collect::<Vec<_>>().join(" ")
}

/// Final parse of a complete answer.
pub fn parse_answer(text: &str) -> ParsedAnswer {
    let mut cues: Vec<Cue> = Vec::new();
    let mut fact = None;
    let mut source = None;
    let mut not_in_docs = false;
    let mut detail = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line == "---" {
            detail = true;
            continue;
        }
        if line.trim_end_matches('.').eq_ignore_ascii_case("NOT_IN_DOCS") {
            not_in_docs = true;
            continue;
        }
        if let Some(f) = strip_prefix_ci(line, "FACT:") {
            if fact.is_none() && !f.is_empty() {
                fact = Some(f.to_string());
            }
            continue;
        }
        if let Some(s) = strip_prefix_ci(line, "SOURCE:") {
            if source.is_none() && !s.is_empty() {
                source = Some(s.to_string());
            }
            continue;
        }
        if !detail {
            if let Some((_, t)) = numbered(line, '.')
                && cues.len() < MAX_CUES
            {
                cues.push(Cue { text: truncate_words(&t, MAX_CUE_WORDS), expand: None });
            }
        } else if let Some((n, t)) = numbered(line, ':')
            && let Some(cue) = n.checked_sub(1).and_then(|i| cues.get_mut(i))
            && cue.expand.is_none()
        {
            cue.expand = Some(t);
        }
        // Anything else is dropped.
    }

    if not_in_docs {
        // "It doesn't guess": nothing else is shown.
        return ParsedAnswer { cues: vec![], fact: None, source_doc: None, not_in_docs: true };
    }
    // A fact is only shown with its source document.
    let (fact, source_doc) = match (fact, source) {
        (Some(f), Some(s)) => (Some(f), Some(s)),
        _ => (None, None),
    };
    ParsedAnswer { cues, fact, source_doc, not_in_docs: false }
}

/// Merge strong-tier detail lines (`N: text`) into the fast model's cues.
pub fn merge_detail(mut base: ParsedAnswer, detail_text: &str) -> ParsedAnswer {
    for raw in detail_text.lines() {
        if let Some((n, t)) = numbered(raw.trim(), ':')
            && let Some(cue) = n.checked_sub(1).and_then(|i| base.cues.get_mut(i))
        {
            cue.expand = Some(t);
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_all(chunks: &[&str]) -> (Vec<(Block, String)>, CueStream) {
        let mut s = CueStream::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(s.push(c));
        }
        out.extend(s.finish());
        // coalesce for easy comparison
        let mut merged: Vec<(Block, String)> = Vec::new();
        for (b, t) in out {
            emit(&mut merged, b, t);
        }
        (merged, s)
    }

    #[test]
    fn classifies_lines_into_blocks() {
        let (out, s) = stream_all(&["1. Requ", "irements\n2. Design\n-", "--\n1: Interview", " users\n"]);
        assert_eq!(
            out,
            vec![
                (Block::Cue, "1. Requirements\n2. Design\n".to_string()),
                (Block::Detail, "1: Interview users\n".to_string())
            ]
        );
        assert_eq!(s.block(), Block::Detail);
        assert_eq!(s.cues(), vec!["Requirements", "Design"]);
    }

    #[test]
    fn partial_text_is_emitted_before_newline() {
        let mut s = CueStream::default();
        assert_eq!(s.push("1. Requ"), vec![(Block::Cue, "1. Requ".to_string())]);
        // A line that might be the separator is held...
        assert_eq!(s.push("irements\n-"), vec![(Block::Cue, "irements\n".to_string())]);
        // ...and released once it can't be.
        assert_eq!(s.push("x"), vec![(Block::Cue, "-x".to_string())]);
    }

    #[test]
    fn dash_bullets_are_not_separators() {
        let (out, _) = stream_all(&["--- \n", "----\n"]);
        assert_eq!(out, vec![(Block::Detail, "----\n".to_string())]);
    }

    #[test]
    fn final_parse_full_grammar() {
        let text = "Sure! Here you go:\n1. Requirements\n2. **Design** phase\n3. Build it fast and well please now\n---\n1: Interview users first\n3: Ship weekly\n9: dropped\n2. dropped after separator\nFACT: ₦25,000 per seat\nSOURCE: pricing.pdf\n";
        let p = parse_answer(text);
        assert_eq!(
            p.cues,
            vec![
                Cue { text: "Requirements".into(), expand: Some("Interview users first".into()) },
                Cue { text: "Design phase".into(), expand: None },
                Cue { text: "Build it fast and well please".into(), expand: Some("Ship weekly".into()) },
            ]
        );
        assert_eq!(p.fact.as_deref(), Some("₦25,000 per seat"));
        assert_eq!(p.source_doc.as_deref(), Some("pricing.pdf"));
        assert!(!p.not_in_docs);
        assert!(p.meets_cue_rule());
    }

    #[test]
    fn caps_at_seven_cues_and_counts_rule() {
        let text: String = (1..=9).map(|i| format!("{i}. cue {i}\n")).collect();
        let p = parse_answer(&text);
        assert_eq!(p.cues.len(), 7);
        assert!(p.meets_cue_rule());
        let two = parse_answer("1. a\n2. b\n");
        assert!(!two.meets_cue_rule());
        assert!(!parse_answer("hello").meets_cue_rule());
    }

    #[test]
    fn fact_needs_source() {
        let p = parse_answer("FACT: 99 users max\n");
        assert_eq!(p.fact, None);
        assert!(!p.meets_cue_rule());
        let p = parse_answer("fact: 99 users max\nSOURCE: limits.md\n");
        assert_eq!(p.fact.as_deref(), Some("99 users max"));
        assert!(p.meets_cue_rule(), "fact answers may have zero cues");
    }

    #[test]
    fn not_in_docs_clears_everything() {
        let p = parse_answer("NOT_IN_DOCS\n");
        assert!(p.not_in_docs && p.cues.is_empty() && p.meets_cue_rule());
        let p = parse_answer("1. guess\n2. guess\n3. guess\nFACT: 5\nSOURCE: x\nNOT_IN_DOCS.\n");
        assert_eq!(p, ParsedAnswer { not_in_docs: true, ..Default::default() });
    }

    #[test]
    fn malformed_numbering_dropped() {
        let p = parse_answer("1.no space\n1234. too many digits\n. nothing\n1. \n1. ok\n");
        assert_eq!(p.cues.len(), 1);
        assert_eq!(p.cues[0].text, "ok");
    }

    #[test]
    fn merge_strong_detail() {
        let base = parse_answer("1. A\n2. B\n3. C\n---\n1: fast detail\n");
        let m = merge_detail(base, "---\n1: strong one\n3: strong three\n");
        assert_eq!(m.cues[0].expand.as_deref(), Some("strong one"));
        assert_eq!(m.cues[1].expand, None);
        assert_eq!(m.cues[2].expand.as_deref(), Some("strong three"));
    }

    #[test]
    fn answer_end_validates_against_protocol_schema() {
        let p = parse_answer("1. A b\n2. C\n3. D\n---\n2: x\n");
        let end = p.into_answer_end(Uuid::nil(), AnswerEndReason::Done);
        let env = ssc_core::protocol::Envelope::new(1, 1, None, ssc_core::protocol::Message::AnswerEnd(end));
        ssc_core::protocol::Envelope::from_value(&env.to_value()).expect("schema-valid answer.end");
    }
}
