//! Text forms of the structured layers, so they round-trip through `MemoryItem { title, body }`.
//!
//! The phone edits `body` as plain text. About Me and session bodies use fixed `Label:` headers;
//! parsing is lenient (unknown text goes to the first field) so a hand-edited body never errors.

use ssc_core::types::AboutMe;
use uuid::Uuid;

/// Fixed id of the single About Me item.
pub const ABOUT_ME_ID: Uuid = Uuid::from_u128(0x5c5c_0000_0000_4000_8000_0000_0000_0001);
/// Namespace for profile ids: profiles are keyed by slug in SQLite and by UUIDv5(slug) on the wire.
pub const PROFILE_NS: Uuid = Uuid::from_u128(0x5c5c_0000_0000_4000_8000_0000_0000_0002);

pub fn profile_uuid(slug: &str) -> Uuid {
    Uuid::new_v5(&PROFILE_NS, slug.as_bytes())
}

/// Lowercase ASCII slug: "Sales: Product X" → "sales-product-x".
pub fn slugify(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "profile".into() } else { s }
}

// ---------- About Me ----------

const ABOUT_FIELDS: [&str; 5] = ["Role", "Company", "Product", "Tone", "Preferred answers"];

pub fn about_me_body(a: &AboutMe) -> String {
    let vals = [&a.role, &a.company, &a.product, &a.tone, &a.preferred_answers];
    ABOUT_FIELDS.iter().zip(vals).map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n")
}

/// Splits `body` into `labels` sections. Returns one string per label (trimmed).
fn sections(body: &str, labels: &[&str]) -> Vec<String> {
    let mut out = vec![String::new(); labels.len()];
    let mut cur = 0usize;
    for line in body.lines() {
        let hit = labels.iter().position(|l| {
            line.len() > l.len()
                && line.is_char_boundary(l.len())
                && line[..l.len()].eq_ignore_ascii_case(l)
                && line[l.len()..].starts_with(':')
        });
        let rest = match hit {
            Some(i) => {
                cur = i;
                line[labels[i].len() + 1..].trim_start()
            }
            None => line,
        };
        if hit.is_some() && rest.is_empty() {
            continue;
        }
        if !out[cur].is_empty() {
            out[cur].push('\n');
        }
        out[cur].push_str(rest);
    }
    out.iter().map(|s| s.trim().to_string()).collect()
}

pub fn parse_about_me(title: &str, body: &str) -> AboutMe {
    let s = sections(body, &ABOUT_FIELDS);
    AboutMe {
        name: title.trim().to_string(),
        role: s[0].clone(),
        company: s[1].clone(),
        product: s[2].clone(),
        tone: s[3].clone(),
        preferred_answers: s[4].clone(),
    }
}

// ---------- Session history ----------

/// What is stored for a finished session (`session_items`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionOutcome {
    pub summary: String,
    pub action_items: Vec<String>,
    /// (question, answer)
    pub key_qa: Vec<(String, String)>,
    pub followup_email: String,
}

const SESSION_LABELS: [&str; 4] = ["Summary", "Action items", "Key Q&A", "Follow-up email"];

pub fn session_body(o: &SessionOutcome) -> String {
    let mut parts = Vec::new();
    if !o.summary.is_empty() {
        parts.push(format!("Summary:\n{}", o.summary));
    }
    if !o.action_items.is_empty() {
        let items: Vec<String> = o.action_items.iter().map(|a| format!("- {a}")).collect();
        parts.push(format!("Action items:\n{}", items.join("\n")));
    }
    if !o.key_qa.is_empty() {
        let qa: Vec<String> = o.key_qa.iter().map(|(q, a)| format!("Q: {q}\nA: {a}")).collect();
        parts.push(format!("Key Q&A:\n{}", qa.join("\n")));
    }
    if !o.followup_email.is_empty() {
        parts.push(format!("Follow-up email:\n{}", o.followup_email));
    }
    parts.join("\n\n")
}

pub fn parse_session(body: &str) -> SessionOutcome {
    let s = sections(body, &SESSION_LABELS);
    let action_items = s[1]
        .lines()
        .map(|l| l.trim().trim_start_matches(['-', '*']).trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let mut key_qa: Vec<(String, String)> = Vec::new();
    let mut in_answer = false;
    for line in s[2].lines() {
        if let Some(q) = line.strip_prefix("Q:") {
            key_qa.push((q.trim().to_string(), String::new()));
            in_answer = false;
        } else if let Some(a) = line.strip_prefix("A:") {
            if key_qa.is_empty() {
                key_qa.push((String::new(), String::new()));
            }
            key_qa.last_mut().unwrap().1 = a.trim().to_string();
            in_answer = true;
        } else if let Some(last) = key_qa.last_mut() {
            let t = if in_answer { &mut last.1 } else { &mut last.0 };
            t.push('\n');
            t.push_str(line);
        }
    }
    SessionOutcome { summary: s[0].clone(), action_items, key_qa, followup_email: s[3].clone() }
}

/// "YYYY-MM-DD HH:MM" (UTC) from unix ms. Civil-from-days algorithm (H. Hinnant).
pub fn format_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let (h, m) = ((secs % 86_400) / 3600, (secs % 3600) / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slugify("Sales: Product X"), "sales-product-x");
        assert_eq!(slugify("  --  "), "profile");
        assert_eq!(slugify("Investor call!"), "investor-call");
        assert_ne!(profile_uuid("a"), profile_uuid("b"));
        assert_eq!(profile_uuid("a"), profile_uuid("a"));
    }

    #[test]
    fn about_me_round_trips() {
        let a = AboutMe {
            name: "Ada".into(),
            role: "AE".into(),
            company: "Acme".into(),
            product: "Rockets\nand boosters".into(),
            tone: "direct".into(),
            preferred_answers: String::new(),
        };
        assert_eq!(parse_about_me("Ada", &about_me_body(&a)), a);
        // Free text with no labels lands in the first field rather than being lost.
        assert_eq!(parse_about_me("x", "just some text").role, "just some text");
        // Case-insensitive labels.
        assert_eq!(parse_about_me("x", "tone: calm").tone, "calm");
    }

    #[test]
    fn session_round_trips() {
        let o = SessionOutcome {
            summary: "Discussed SSO.\nPositive.".into(),
            action_items: vec!["Ada sends pricing by Fri".into(), "Bob books demo".into()],
            key_qa: vec![("Do you support SAML?".into(), "Yes, on Enterprise".into())],
            followup_email: "Subject: Thanks\n\nHi Bob".into(),
        };
        assert_eq!(parse_session(&session_body(&o)), o);
        assert_eq!(parse_session(""), SessionOutcome::default());
    }

    #[test]
    fn utc_format() {
        assert_eq!(format_utc(0), "1970-01-01 00:00");
        assert_eq!(format_utc(1_731_650_000_123), "2024-11-15 05:53");
        assert_eq!(format_utc(951_782_400_000), "2000-02-29 00:00");
    }
}
