//! Deterministic text rules for "them" speech: is this utterance a question (or pushback) worth
//! answering, is it small talk, and a keyword guess at the intent for offline mode.
//!
//! These rules only decide *whether to look closer*. In cloud mode a question still goes to the
//! decision model; offline they fire directly (spec "Answer models": offline triggering).
//!
//! ## Recognised forms (keep this list in sync with the tables below)
//!
//! - **Question mark** anywhere in the utterance.
//! - **English opening words** ([`QUESTION_OPENERS`]): what, when, where, which, who, whom, whose,
//!   why, how, is, are, am, was, were, do, does, did, can, could, will, would, should, shall,
//!   may, might, have, has, had and their negative contractions (isn't, don't, can't, ...).
//! - **Phrases anywhere** ([`QUESTION_PHRASES`]): "tell me", "walk me through", "explain",
//!   "can you", "could you", "do you", "how much", "how many", "how long", "any idea",
//!   "i'm wondering", "let me know", "is it that", "what about".
//! - **Tag questions at the end** ([`QUESTION_TAGS`]): right, abi, shebi, ehn, isn't it,
//!   no be so, or not.
//! - **Nigerian English / Pidgin** ([`PIDGIN_OPENERS`], [`QUESTION_PHRASES`]):
//!   - `abi` / `shebi` (tag: "right?", "isn't it?"), `shey` / `se` at the start ("is it that...?")
//!   - `wetin` ("what"), `wetin be`, `na wetin`
//!   - `how much be`, `how much e be`, `na how much` (price)
//!   - `which kain` ("what kind"), `which one`
//!   - `you fit` / `una fit` / `e fit` ("can you / can it")
//!   - `e get` ("is there"), `una get` / `you get` ("do you have")
//!   - `you sabi` ("do you know"), `wen` ("when")
//! - **Pushback** ([`OBJECTION_PHRASES`]) counts as needing a response even without a question:
//!   "too expensive", "too costly", "too much", "not convinced", "not sure about", "we already
//!   use", "cheaper", "competitor", "concerned about", "worried about", "e too cost", "e cost
//!   well well", "no be small money".
//! - **Small talk** ([`SMALL_TALK`]): greetings and call logistics are never answered
//!   automatically: "how are you", "how's it going", "how was your weekend", "can you hear me",
//!   "can you see my screen", "good morning", "hope you're well", and Pidgin "how far",
//!   "how body", "how you dey", "how una dey", "you dey hear me", "how market".

use ssc_core::protocol::Intent;

/// What an utterance looks like to the deterministic rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtteranceKind {
    /// A question or a request for information.
    Question,
    /// Pushback that needs a response even though it isn't phrased as a question.
    Objection,
    /// Greetings and call logistics: never answered automatically.
    SmallTalk,
    /// Anything else: no trigger.
    Statement,
}

impl UtteranceKind {
    /// True if this kind should become a question trigger.
    pub fn wants_answer(self) -> bool {
        matches!(self, UtteranceKind::Question | UtteranceKind::Objection)
    }
}

pub const QUESTION_OPENERS: &[&str] = &[
    "what",
    "what's",
    "whats",
    "when",
    "where",
    "which",
    "who",
    "whom",
    "whose",
    "why",
    "how",
    "how's",
    "is",
    "are",
    "am",
    "was",
    "were",
    "do",
    "does",
    "did",
    "can",
    "could",
    "will",
    "would",
    "should",
    "shall",
    "may",
    "might",
    "have",
    "has",
    "had",
    "isn't",
    "aren't",
    "wasn't",
    "don't",
    "doesn't",
    "didn't",
    "can't",
    "couldn't",
    "won't",
    "wouldn't",
    "shouldn't",
    "haven't",
    "hasn't",
];

/// Pidgin / Nigerian English opening words that make a question.
pub const PIDGIN_OPENERS: &[&str] = &["shey", "se", "shebi", "wetin", "wen", "abi"];

/// Phrases that make a question anywhere in the utterance (matched on word boundaries).
pub const QUESTION_PHRASES: &[&str] = &[
    "tell me",
    "walk me through",
    "explain",
    "can you",
    "could you",
    "do you",
    "how much",
    "how many",
    "how long",
    "any idea",
    "i'm wondering",
    "i am wondering",
    "let me know",
    "is it that",
    "what about",
    // Pidgin / Nigerian English
    "wetin",
    "how much be",
    "how much e be",
    "na how much",
    "which kain",
    "you fit",
    "una fit",
    "e fit",
    "e get",
    "una get",
    "you get",
    "you sabi",
];

/// Tags at the end of an utterance that turn a statement into a question.
pub const QUESTION_TAGS: &[&str] = &["right", "abi", "shebi", "ehn", "isn't it", "no be so", "or not"];

pub const OBJECTION_PHRASES: &[&str] = &[
    "too expensive",
    "too costly",
    "too much",
    "too pricey",
    "not convinced",
    "not sure about",
    "we already use",
    "we already have",
    "cheaper",
    "competitor",
    "concerned about",
    "worried about",
    "doesn't make sense",
    "e too cost",
    "e cost well well",
    "no be small money",
];

/// Small-talk openings. An utterance is small talk if it starts with one of these and is short.
pub const SMALL_TALK: &[&str] = &[
    "how are you",
    "how are you doing",
    "how's it going",
    "hows it going",
    "how is it going",
    "how was your weekend",
    "how was your day",
    "how is your day",
    "how's your day",
    "how is the family",
    "how's the family",
    "how is the weather",
    "how's the weather",
    "can you hear me",
    "can you see my screen",
    "can you see me",
    "are you there",
    "good morning",
    "good afternoon",
    "good evening",
    "hope you're well",
    "hope you are well",
    "nice to meet you",
    // Pidgin
    "how far",
    "how body",
    "how you dey",
    "how una dey",
    "you dey hear me",
    "una dey hear me",
    "how market",
    "how work",
];

/// Extra words allowed after a small-talk phrase before it stops counting as small talk
/// ("how are you doing today" yes; "how are you planning to integrate the API" no).
const SMALL_TALK_SLACK_WORDS: usize = 2;

/// Lowercase words, apostrophes kept, everything else a separator.
pub fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace(['\u{2019}', '\u{2018}'], "'")
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Dedupe key: lowercase words joined by single spaces (punctuation and case ignored).
pub fn normalize(text: &str) -> String {
    words(text).join(" ")
}

fn phrase_words(p: &str) -> Vec<String> {
    words(p)
}

fn contains_phrase(ws: &[String], phrase: &str) -> bool {
    let p = phrase_words(phrase);
    !p.is_empty() && ws.windows(p.len()).any(|w| w == p.as_slice())
}

fn starts_with_phrase(ws: &[String], phrase: &str) -> bool {
    let p = phrase_words(phrase);
    !p.is_empty() && ws.len() >= p.len() && ws[..p.len()] == p[..]
}

fn ends_with_phrase(ws: &[String], phrase: &str) -> bool {
    let p = phrase_words(phrase);
    !p.is_empty() && ws.len() > p.len() && ws[ws.len() - p.len()..] == p[..]
}

/// Classify one "them" utterance.
pub fn classify(text: &str) -> UtteranceKind {
    let ws = words(text);
    if ws.is_empty() {
        return UtteranceKind::Statement;
    }
    if SMALL_TALK
        .iter()
        .any(|p| starts_with_phrase(&ws, p) && ws.len() <= phrase_words(p).len() + SMALL_TALK_SLACK_WORDS)
    {
        return UtteranceKind::SmallTalk;
    }
    if OBJECTION_PHRASES.iter().any(|p| contains_phrase(&ws, p)) {
        return UtteranceKind::Objection;
    }
    let first = ws[0].as_str();
    let question = text.contains('?')
        || QUESTION_OPENERS.contains(&first)
        || PIDGIN_OPENERS.contains(&first)
        || QUESTION_PHRASES.iter().any(|p| contains_phrase(&ws, p))
        || QUESTION_TAGS.iter().any(|t| ends_with_phrase(&ws, t));
    if question { UtteranceKind::Question } else { UtteranceKind::Statement }
}

const PRICING: &[&str] = &[
    "price",
    "prices",
    "pricing",
    "cost",
    "costs",
    "how much",
    "discount",
    "fee",
    "fees",
    "charge",
    "budget",
    "naira",
    "dollar",
    "dollars",
    "quote",
    "per seat",
    "license",
    "subscription",
    "pay",
];
const SCHEDULING: &[&str] = &[
    "when",
    "wen",
    "date",
    "deadline",
    "schedule",
    "timeline",
    "what time",
    "next week",
    "calendar",
    "available",
    "availability",
    "how long",
    "go live",
    "start date",
];
const TECHNICAL: &[&str] = &[
    "how does",
    "how do",
    "integrate",
    "integration",
    "api",
    "security",
    "encryption",
    "architecture",
    "explain",
    "walk me through",
    "work",
    "works",
    "support",
    "sso",
    "data",
    "how e dey",
];

/// Keyword guess at the intent, used offline when the decision model is not awaited.
/// Order matters: pushback beats pricing ("why is it so expensive?"), pricing beats scheduling.
pub fn heuristic_intent(text: &str) -> Intent {
    let ws = words(text);
    let any = |list: &[&str]| list.iter().any(|p| contains_phrase(&ws, p));
    match classify(text) {
        UtteranceKind::SmallTalk => Intent::SmallTalk,
        UtteranceKind::Objection => Intent::Objection,
        _ if any(PRICING) => Intent::Pricing,
        _ if any(SCHEDULING) => Intent::Scheduling,
        _ if any(TECHNICAL) => Intent::Technical,
        _ => Intent::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use UtteranceKind::*;

    /// Table-driven fixtures: (utterance, expected kind).
    const FIXTURES: &[(&str, UtteranceKind)] = &[
        // English questions
        ("How much is the enterprise plan?", Question),
        ("what's your uptime guarantee", Question),
        ("Can you walk me through the onboarding", Question),
        ("Tell me about your security model.", Question),
        ("Do you integrate with Salesforce", Question),
        ("So it works offline, right", Question),
        ("I'm wondering about data residency", Question),
        ("Is it that you charge per seat", Question),
        // Nigerian English / Pidgin
        ("How much be this one", Question),
        ("na how much for the premium", Question),
        ("Wetin be the difference between the two plans", Question),
        ("shey una dey support USSD", Question),
        ("e fit work with Paystack", Question),
        ("una get monthly option", Question),
        ("this one go work offline abi", Question),
        ("which kain support una dey give", Question),
        ("e get discount for NGOs", Question),
        // Pushback
        ("That's too expensive for us", Objection),
        ("Honestly we already use HubSpot for this", Objection),
        ("e too cost o", Objection),
        // Small talk
        ("How are you doing today?", SmallTalk),
        ("Can you hear me?", SmallTalk),
        ("How far", SmallTalk),
        ("how you dey my guy", SmallTalk),
        ("Good morning everyone", SmallTalk),
        // Not a question
        ("Okay let me share my screen", Statement),
        ("We spoke with your colleague last week", Statement),
        ("Thanks that makes sense", Statement),
        ("", Statement),
        ("   ", Statement),
    ];

    #[test]
    fn fixture_table() {
        let mut failures = vec![];
        for (text, want) in FIXTURES {
            let got = classify(text);
            if got != *want {
                failures.push(format!("{text:?}: want {want:?}, got {got:?}"));
            }
        }
        assert!(failures.is_empty(), "misclassified:\n{}", failures.join("\n"));
    }

    #[test]
    fn small_talk_slack_is_bounded() {
        // A real question that happens to start like a greeting is not small talk.
        assert_eq!(classify("How are you planning to handle the API rate limits?"), Question);
    }

    #[test]
    fn intents() {
        let cases: &[(&str, Intent)] = &[
            ("How much is the enterprise plan?", Intent::Pricing),
            ("how much be this one", Intent::Pricing),
            ("When can we go live?", Intent::Scheduling),
            ("How does the API integration work?", Intent::Technical),
            ("That's too expensive", Intent::Objection),
            ("How are you?", Intent::SmallTalk),
            ("Who else is on your team?", Intent::None),
        ];
        for (text, want) in cases {
            assert_eq!(heuristic_intent(text), *want, "{text}");
        }
    }

    #[test]
    fn normalize_ignores_case_and_punctuation() {
        assert_eq!(normalize("How much is it?"), normalize("how much  is it"));
        assert_eq!(normalize("What\u{2019}s the price?!"), "what's the price");
    }
}
