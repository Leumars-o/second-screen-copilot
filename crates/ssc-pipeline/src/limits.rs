//! Clamp outgoing payloads to the limits in `contracts/protocol/messages/*.schema.json`, so model
//! output of any length can never make a frame fail validation (the transport rejects those).

use ssc_core::protocol::{AnswerEnd, ContactSuggestion, Cue, MemoryItem, SessionSummary};

/// Longest prefix of `s` with at most `max` chars (schema `maxLength` counts characters).
pub fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// `clip`, then `None` if empty (for `minLength: 1` optional fields).
fn clip_opt(s: Option<String>, max: usize) -> Option<String> {
    s.map(|v| clip(v.trim(), max)).filter(|v| !v.is_empty())
}

/// Splits `s` into pieces of at most `max` chars (answer.delta text is 1..=4096).
pub fn split_chars(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut n = 0;
    for c in s.chars() {
        if n == max {
            out.push(std::mem::take(&mut cur));
            n = 0;
        }
        cur.push(c);
        n += 1;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn answer_end(mut e: AnswerEnd) -> AnswerEnd {
    e.cues = e
        .cues
        .into_iter()
        .filter_map(|c| {
            let text = clip(c.text.trim(), 80);
            (!text.is_empty()).then(|| Cue { text, expand: clip_opt(c.expand, 400) })
        })
        .take(7)
        .collect();
    e.fact = clip_opt(e.fact, 200);
    e.source_doc = clip_opt(e.source_doc, 256);
    e
}

pub fn session_summary(mut s: SessionSummary) -> SessionSummary {
    s.summary = clip(&s.summary, 8000);
    s.followup_email = clip(&s.followup_email, 8000);
    s.action_items = s.action_items.iter().map(|a| clip(a.trim(), 400)).filter(|a| !a.is_empty()).take(50).collect();
    s.contact_suggestions = s
        .contact_suggestions
        .into_iter()
        .filter_map(|c| {
            let name = clip(c.name.trim(), 200);
            (!name.is_empty()).then(|| ContactSuggestion {
                id: c.id,
                name,
                company: clip_opt(c.company, 200),
                notes: c.notes.map(|n| clip(n.trim(), 2000)),
            })
        })
        .take(20)
        .collect();
    s
}

/// The relay drops frames over 64 KB; leave room for the envelope, AEAD and JSON escaping.
pub const SNAPSHOT_BUDGET_BYTES: usize = 48 * 1024;

/// Memory items clipped to the schema, newest first, stopping before the frame budget.
pub fn memory_items(mut items: Vec<MemoryItem>) -> Vec<MemoryItem> {
    items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let mut out = Vec::new();
    let mut used = 0;
    for mut it in items.into_iter().take(5000) {
        it.title = clip(it.title.trim(), 200);
        if it.title.is_empty() {
            it.title = "(untitled)".into();
        }
        it.body = clip(&it.body, 20_000);
        let size = serde_json::to_vec(&it).map(|v| v.len()).unwrap_or(usize::MAX);
        if used + size > SNAPSHOT_BUDGET_BYTES {
            break;
        }
        used += size;
        out.push(it);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::protocol::{AnswerEndReason, Envelope, Layer, Message};
    use uuid::Uuid;

    fn valid(m: Message) -> bool {
        Envelope::from_slice(&Envelope::new(1, 1, None, m).to_bytes()).is_ok()
    }

    #[test]
    fn clip_counts_chars_not_bytes() {
        assert_eq!(clip("héllo", 2), "hé");
        assert_eq!(clip("ab", 5), "ab");
        assert_eq!(split_chars("abcde", 2), vec!["ab", "cd", "e"]);
        assert!(split_chars("", 2).is_empty());
    }

    #[test]
    fn oversized_answer_end_becomes_valid() {
        let long = "x".repeat(500);
        let e = AnswerEnd {
            answer_id: Uuid::new_v4(),
            reason: AnswerEndReason::Done,
            cues: (0..10).map(|_| Cue { text: long.clone(), expand: Some(long.clone()) }).collect(),
            fact: Some(long.clone()),
            source_doc: Some(" ".into()),
            not_in_docs: false,
        };
        assert!(!valid(Message::AnswerEnd(e.clone())));
        let e = answer_end(e);
        assert_eq!(e.cues.len(), 7);
        assert!(e.source_doc.is_none());
        assert!(valid(Message::AnswerEnd(e)));
    }

    #[test]
    fn oversized_summary_and_snapshot_become_valid() {
        let s = SessionSummary {
            session_id: Uuid::new_v4(),
            summary: "s".repeat(9000),
            action_items: vec!["".into(), "a".repeat(600)],
            followup_email: String::new(),
            contact_suggestions: vec![ContactSuggestion {
                id: Uuid::new_v4(),
                name: "Ada".into(),
                company: Some(String::new()),
                notes: None,
            }],
        };
        let s = session_summary(s);
        assert_eq!(s.action_items.len(), 1);
        assert!(valid(Message::SessionSummary(s)));

        let items: Vec<MemoryItem> = (0..100)
            .map(|i| MemoryItem {
                id: Uuid::new_v4(),
                layer: Layer::Session,
                title: String::new(),
                body: "b".repeat(30_000),
                updated_at: i,
            })
            .collect();
        let out = memory_items(items);
        assert!(!out.is_empty() && out.len() < 100);
        assert_eq!(out[0].updated_at, 99, "newest first");
        let m = Message::MemorySnapshot(ssc_core::protocol::MemorySnapshot { items: out });
        let bytes = Envelope::new(1, 1, None, m.clone()).to_bytes();
        assert!(bytes.len() < 60 * 1024);
        assert!(valid(m));
    }
}
