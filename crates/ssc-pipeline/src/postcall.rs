//! Post-call text handling: the session text fed to `summary.v1`, and parsing its JSON output.
//! Parsing is deterministic and tolerant: a model that wraps the JSON in prose or code fences
//! still parses; one that returns no JSON at all gives its text as the summary.

use serde_json::Value;
use ssc_core::protocol::{ContactSuggestion, Speaker};
use ssc_core::types::TranscriptSegment;
use ssc_memory::SessionOutcome;
use uuid::Uuid;

/// One answer shown during the session, for the summary's "key Q&A".
#[derive(Debug, Clone, PartialEq)]
pub struct ShownAnswer {
    pub question: String,
    pub cues: Vec<String>,
}

/// Transcript (chronological) then the answers that were shown.
pub fn session_text(transcript: &[TranscriptSegment], answers: &[ShownAnswer]) -> String {
    let mut out = String::new();
    if !transcript.is_empty() {
        out.push_str("Transcript:\n");
        for s in transcript.iter().filter(|s| !s.text.trim().is_empty()) {
            let who = match s.speaker {
                Speaker::Me => "me",
                Speaker::Them => "them",
            };
            out.push_str(&format!("{who}: {}\n", s.text.trim()));
        }
    }
    let shown: Vec<&ShownAnswer> = answers.iter().filter(|a| !a.cues.is_empty()).collect();
    if !shown.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("Answers shown:\n");
        for a in shown {
            let q = if a.question.trim().is_empty() { "(hotkey)" } else { a.question.trim() };
            out.push_str(&format!("Q: {q}\nA: {}\n", a.cues.join("; ")));
        }
    }
    out
}

/// Parsed `summary.v1` output (follow-up email is filled in separately).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedSummary {
    pub outcome: SessionOutcome,
    pub contacts: Vec<ContactSuggestion>,
}

fn json_object(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| serde_json::from_str::<Value>(&text[start..=end]).ok()).flatten().filter(Value::is_object)
}

fn str_of(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn parse_summary(text: &str) -> ParsedSummary {
    let Some(v) = json_object(text) else {
        return ParsedSummary {
            outcome: SessionOutcome { summary: text.trim().to_string(), ..Default::default() },
            contacts: vec![],
        };
    };
    let action_items = v
        .get("action_items")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let key_qa = v
        .get("key_qa")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| Some((str_of(x, "q")?, str_of(x, "a").unwrap_or_default()))).collect())
        .unwrap_or_default();
    let contacts = v
        .get("contact_suggestions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some(ContactSuggestion {
                        id: Uuid::new_v4(),
                        name: str_of(x, "name")?,
                        company: str_of(x, "company"),
                        notes: str_of(x, "notes"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    ParsedSummary {
        outcome: SessionOutcome {
            summary: str_of(&v, "summary").unwrap_or_default(),
            action_items,
            key_qa,
            followup_email: String::new(),
        },
        contacts,
    }
}

/// What the follow-up prompt sees as `{{session}}`: summary plus action items.
pub fn followup_session(o: &SessionOutcome) -> String {
    let mut s = o.summary.clone();
    if !o.action_items.is_empty() {
        s.push_str("\n\nAction items:\n");
        for a in &o.action_items {
            s.push_str(&format!("- {a}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json() {
        let t = "Here you go:\n```json\n{\"summary\": \"Discussed SSO.\", \"action_items\": [\"Sam sends docs by Friday\", \"\"], \"key_qa\": [{\"q\": \"Price?\", \"a\": \"45k\"}], \"contact_suggestions\": [{\"name\": \"Dana\", \"company\": \"Acme\", \"notes\": \"wants SSO\"}, {\"name\": \"\"}]}\n```";
        let p = parse_summary(t);
        assert_eq!(p.outcome.summary, "Discussed SSO.");
        assert_eq!(p.outcome.action_items, vec!["Sam sends docs by Friday"]);
        assert_eq!(p.outcome.key_qa, vec![("Price?".to_string(), "45k".to_string())]);
        assert_eq!(p.contacts.len(), 1);
        assert_eq!(p.contacts[0].company.as_deref(), Some("Acme"));
    }

    #[test]
    fn non_json_becomes_summary_text() {
        let p = parse_summary("  The call went well.  ");
        assert_eq!(p.outcome.summary, "The call went well.");
        assert!(p.contacts.is_empty() && p.outcome.action_items.is_empty());
    }

    #[test]
    fn session_text_lists_transcript_then_answers() {
        let seg = |sp, t: &str| TranscriptSegment {
            speaker: sp,
            text: t.into(),
            is_final: true,
            t0_ms: 0,
            t1_ms: 1,
            ends_utterance: true,
        };
        let s = session_text(
            &[seg(Speaker::Them, "How much is it?"), seg(Speaker::Me, " "), seg(Speaker::Me, "Let me check")],
            &[ShownAnswer { question: "How much is it?".into(), cues: vec!["45k per seat".into(), "Annual".into()] }],
        );
        assert_eq!(
            s,
            "Transcript:\nthem: How much is it?\nme: Let me check\n\nAnswers shown:\nQ: How much is it?\nA: 45k per seat; Annual\n"
        );
    }
}
