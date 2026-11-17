//! `ssc-context`: assembles the model prompt (Build Sheet §6 W1-MEMORY, spec "Data flow" step 4).
//!
//! Renders `contracts/prompts/*.v1.md` into a [`Prompt`]:
//! - **prefix**: rules + About Me + active profile. Depends only on those two and the budget, so it
//!   is byte-stable for a session (provider prompt caching / Ollama prefix caching).
//! - **dynamic**: top doc chunks, the last 60–90 s of transcript, the screen diff and the question.
//!
//! Every part has its own token budget (cl100k count via `tiktoken-rs`). On overflow the oldest
//! transcript lines and the lowest-ranked chunks are cut first; the newest transcript segment always
//! stays (if it alone exceeds the budget, its end is kept). [`Budget::local`] keeps the dynamic part
//! under ~300 tokens.
//!
//! Call content (transcript, screen, question, session) is wrapped in `<untrusted>` exactly where the
//! prompt files put it, and any `<untrusted>` / `</untrusted>` tag inside that content is neutralised
//! so it cannot close the wrapper early. Placeholders are substituted in a single pass, so text that
//! looks like `{{docs}}` inside a value is never expanded.

use ssc_core::protocol::{AnswerFormat, Speaker};
use ssc_core::traits::MemoryStore;
use ssc_core::types::{AboutMe, Chunk, Profile, Prompt, TranscriptSegment};
use std::sync::OnceLock;

// ---------- token counting ----------

fn bpe() -> &'static tiktoken_rs::CoreBPE {
    tiktoken_rs::cl100k_base_singleton()
}

/// Token count (cl100k_base). Close enough to every provider's tokenizer for budgeting.
pub fn count_tokens(s: &str) -> usize {
    if s.is_empty() { 0 } else { bpe().encode_ordinary(s).len() }
}

/// Decodes a token slice, shrinking it from the cut side until it is valid UTF-8.
fn decode_trimmed(tokens: &[u32], keep_head: bool) -> String {
    let mut t = tokens;
    loop {
        if t.is_empty() {
            return String::new();
        }
        if let Ok(s) = bpe().decode(t.to_vec()) {
            return s;
        }
        t = if keep_head { &t[..t.len() - 1] } else { &t[1..] };
    }
}

/// First `max` tokens of `s`.
pub fn truncate_head(s: &str, max: usize) -> String {
    let toks = bpe().encode_ordinary(s);
    if toks.len() <= max { s.to_string() } else { decode_trimmed(&toks[..max], true) }
}

/// Last `max` tokens of `s`.
pub fn truncate_tail(s: &str, max: usize) -> String {
    let toks = bpe().encode_ordinary(s);
    if toks.len() <= max { s.to_string() } else { decode_trimmed(&toks[toks.len() - max..], false) }
}

// ---------- templates ----------

/// Which prompt file to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptKind {
    Cue,
    Fact,
    Objection,
    Expand,
    Summary,
    Followup,
}

impl PromptKind {
    pub const ALL: [PromptKind; 6] =
        [Self::Cue, Self::Fact, Self::Objection, Self::Expand, Self::Summary, Self::Followup];

    pub fn for_format(f: AnswerFormat) -> Self {
        match f {
            AnswerFormat::Cue => Self::Cue,
            AnswerFormat::Fact => Self::Fact,
            AnswerFormat::Objection => Self::Objection,
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::Cue => include_str!("../../../contracts/prompts/cue.v1.md"),
            Self::Fact => include_str!("../../../contracts/prompts/fact.v1.md"),
            Self::Objection => include_str!("../../../contracts/prompts/objection.v1.md"),
            Self::Expand => include_str!("../../../contracts/prompts/expand.v1.md"),
            Self::Summary => include_str!("../../../contracts/prompts/summary.v1.md"),
            Self::Followup => include_str!("../../../contracts/prompts/followup.v1.md"),
        }
    }

    fn template(self) -> &'static Template {
        static T: OnceLock<Vec<Template>> = OnceLock::new();
        let all = T.get_or_init(|| Self::ALL.iter().map(|k| Template::parse(k.source())).collect());
        &all[Self::ALL.iter().position(|k| *k == self).unwrap()]
    }

    /// Output cap passed to the model. Cue answers stay short (first block ~40 tokens).
    pub fn max_tokens(self) -> u32 {
        match self {
            Self::Cue => 160,
            Self::Fact => 100,
            Self::Objection => 120,
            Self::Expand => 400,
            Self::Summary => 700,
            Self::Followup => 350,
        }
    }

    fn temperature(self) -> f32 {
        match self {
            Self::Summary => 0.1,
            _ => 0.2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    AboutMe,
    ProfileName,
    ProfileInstructions,
    Docs,
    Transcript,
    Screen,
    Question,
    Answer,
    Session,
}

impl Slot {
    fn from_name(n: &str) -> Option<Self> {
        Some(match n {
            "about_me" => Self::AboutMe,
            "profile_name" => Self::ProfileName,
            "profile_instructions" => Self::ProfileInstructions,
            "docs" => Self::Docs,
            "transcript" => Self::Transcript,
            "screen" => Self::Screen,
            "question" => Self::Question,
            "answer" => Self::Answer,
            "session" => Self::Session,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Lit(String),
    /// (slot, inside an `<untrusted>` wrapper in the template)
    Slot(Slot, bool),
}

#[derive(Debug)]
struct Template {
    prefix: Vec<Part>,
    dynamic: Vec<Part>,
}

const MARKER: &str = "<<<DYNAMIC>>>";

impl Template {
    fn parse(src: &str) -> Self {
        let src = src.replace("\r\n", "\n");
        let (prefix, dynamic) = match src.split_once(&format!("\n{MARKER}\n")) {
            // The newline before the marker stays with the prefix so prefix + dynamic reads cleanly.
            Some((p, d)) => (format!("{p}\n"), d.to_string()),
            None => (src.clone(), String::new()),
        };
        Self { prefix: Self::parts(&prefix), dynamic: Self::parts(&dynamic) }
    }

    fn parts(s: &str) -> Vec<Part> {
        let mut out = Vec::new();
        let mut rest = s;
        let mut untrusted = false;
        let push_lit = |lit: &str, out: &mut Vec<Part>, untrusted: &mut bool| {
            if lit.is_empty() {
                return;
            }
            if let (Some(o), c) = (lit.rfind("<untrusted>"), lit.rfind("</untrusted>")) {
                *untrusted = c.is_none_or(|c| o > c);
            } else if lit.contains("</untrusted>") {
                *untrusted = false;
            }
            out.push(Part::Lit(lit.to_string()));
        };
        while let Some(i) = rest.find("{{") {
            let Some(j) = rest[i + 2..].find("}}") else { break };
            let name = &rest[i + 2..i + 2 + j];
            match Slot::from_name(name) {
                Some(slot) => {
                    push_lit(&rest[..i], &mut out, &mut untrusted);
                    out.push(Part::Slot(slot, untrusted));
                }
                // Unknown placeholder: a template bug; keep it visible rather than silently dropping text.
                None => push_lit(&rest[..i + 4 + j], &mut out, &mut untrusted),
            }
            rest = &rest[i + 4 + j..];
        }
        push_lit(rest, &mut out, &mut untrusted);
        out
    }

    fn slots(&self) -> impl Iterator<Item = Slot> + '_ {
        self.prefix.iter().chain(&self.dynamic).filter_map(|p| match p {
            Part::Slot(s, _) => Some(*s),
            Part::Lit(_) => None,
        })
    }
}

/// Neutralises `<untrusted ...>` and `</untrusted>` (any case, any inner spacing) inside call content
/// so it can't end the wrapper and smuggle text out as instructions.
pub fn neutralize_untrusted(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (i, _) in s.match_indices('<') {
        let mut j = i + 1;
        while j < b.len() && (b[j].is_ascii_whitespace() || b[j] == b'/') {
            j += 1;
        }
        if b.len() >= j + 9 && b[j..j + 9].eq_ignore_ascii_case(b"untrusted") {
            out.push_str(&s[last..i]);
            out.push_str("&lt;");
            last = i + 1;
        }
    }
    out.push_str(&s[last..]);
    out
}

fn render(parts: &[Part], vals: &Values) -> String {
    let mut out = String::new();
    for p in parts {
        match p {
            Part::Lit(l) => out.push_str(l),
            Part::Slot(s, untrusted) => {
                let v = vals.get(*s);
                if *untrusted {
                    out.push_str(&neutralize_untrusted(v));
                } else {
                    out.push_str(v);
                }
            }
        }
    }
    out
}

#[derive(Debug, Default)]
struct Values {
    about_me: String,
    profile_name: String,
    profile_instructions: String,
    docs: String,
    transcript: String,
    screen: String,
    question: String,
    answer: String,
    session: String,
}

impl Values {
    fn get(&self, s: Slot) -> &str {
        match s {
            Slot::AboutMe => &self.about_me,
            Slot::ProfileName => &self.profile_name,
            Slot::ProfileInstructions => &self.profile_instructions,
            Slot::Docs => &self.docs,
            Slot::Transcript => &self.transcript,
            Slot::Screen => &self.screen,
            Slot::Question => &self.question,
            Slot::Answer => &self.answer,
            Slot::Session => &self.session,
        }
    }
}

// ---------- budgets and inputs ----------

/// Per-part token budgets. Values are tokens of the rendered part, not counting template text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub about_me: usize,
    /// Profile name + instructions.
    pub profile: usize,
    pub docs: usize,
    pub max_chunks: usize,
    pub transcript: usize,
    /// Only transcript segments ending within this window before `now_ms` are used
    /// (the newest segment is always kept).
    pub transcript_window_ms: u64,
    pub screen: usize,
    pub question: usize,
    pub answer: usize,
    pub session: usize,
    /// Cards for known contacts/accounts mentioned in the call (placed with the documents).
    pub known: usize,
    /// Hard cap on the whole dynamic part, template text included. `None` = no cap.
    pub dynamic_cap: Option<usize>,
}

impl Budget {
    /// Cloud models: room for 5 chunks and 90 s of transcript.
    pub fn cloud() -> Self {
        Self {
            about_me: 300,
            profile: 400,
            docs: 2000,
            max_chunks: 5,
            transcript: 700,
            transcript_window_ms: 90_000,
            screen: 500,
            question: 200,
            answer: 300,
            session: 6000,
            known: 150,
            dynamic_cap: None,
        }
    }

    /// Local model (Gemma-class on the M1): prefix is cached, so only the dynamic part costs time.
    /// Dynamic part stays under 300 tokens.
    pub fn local() -> Self {
        Self {
            about_me: 200,
            profile: 200,
            docs: 130,
            max_chunks: 3,
            transcript: 60,
            transcript_window_ms: 60_000,
            screen: 30,
            question: 40,
            answer: 80,
            session: 150,
            known: 40,
            dynamic_cap: Some(300),
        }
    }

    /// Post-call (summary, follow-up): the session text dominates.
    pub fn post_call() -> Self {
        Self { session: 12_000, docs: 1500, ..Self::cloud() }
    }
}

/// Everything the builder may use. Chunks are ranked best first; transcript is chronological.
#[derive(Debug, Clone, Default)]
pub struct ContextInput {
    pub about_me: Option<AboutMe>,
    pub profile: Option<Profile>,
    pub chunks: Vec<Chunk>,
    pub transcript: Vec<TranscriptSegment>,
    /// Current screen diff text (`ScreenDiff::text()`), newest content first.
    pub screen: String,
    pub question: String,
    /// Existing cues (expand prompt).
    pub answer: String,
    /// Session text (summary / follow-up prompts).
    pub session: String,
    /// Reference time for the transcript window, unix ms.
    pub now_ms: u64,
    /// Saved contacts and accounts; the ones mentioned in the question, screen or recent
    /// transcript get a compact card next to the documents (M7).
    pub known: Vec<Known>,
}

/// The rendered prompt plus what went into it (for `source_doc`, `not_in_docs` and the debug panel).
#[derive(Debug, Clone, PartialEq)]
pub struct Assembled {
    pub prompt: Prompt,
    /// Chunks actually placed in the prompt, best first.
    pub chunks_used: Vec<Chunk>,
    pub chunks_dropped: usize,
    pub transcript_used: usize,
    pub transcript_dropped: usize,
    pub prefix_tokens: usize,
    pub dynamic_tokens: usize,
}

// ---------- part builders ----------

fn about_me_text(a: &AboutMe, budget: usize) -> String {
    let mut lines = Vec::new();
    for (k, v) in [
        ("Name", &a.name),
        ("Role", &a.role),
        ("Company", &a.company),
        ("Product", &a.product),
        ("Tone", &a.tone),
        ("Preferred answers", &a.preferred_answers),
    ] {
        if !v.trim().is_empty() {
            lines.push(format!("{k}: {}", v.trim()));
        }
    }
    truncate_head(&lines.join("\n"), budget)
}

fn speaker(s: Speaker) -> &'static str {
    match s {
        Speaker::Me => "me",
        Speaker::Them => "them",
    }
}

/// Minimum tokens of the newest transcript segment that are always kept.
pub const NEWEST_MIN_TOKENS: usize = 24;

/// Newest-first fill: walk back from the newest segment until the budget is used. Returns
/// (text, used, dropped).
fn transcript_text(segs: &[TranscriptSegment], now_ms: u64, window_ms: u64, budget: usize) -> (String, usize, usize) {
    let segs: Vec<&TranscriptSegment> = segs.iter().filter(|s| !s.text.trim().is_empty()).collect();
    let Some((newest, older)) = segs.split_last() else { return (String::new(), 0, 0) };
    let line = |s: &TranscriptSegment| format!("{}: {}", speaker(s.speaker), s.text.trim());

    let newest_line = line(newest);
    // The newest segment always survives, with at least NEWEST_MIN_TOKENS of it even when the
    // budget (or the local dynamic cap) is smaller: this rule wins over the budget.
    let budget = budget.max(NEWEST_MIN_TOKENS);
    let mut lines = vec![if count_tokens(&newest_line) > budget {
        // When it alone is too big, keep its end (where the question is).
        let label = format!("{}: …", speaker(newest.speaker));
        let room = budget.saturating_sub(count_tokens(&label)).max(1);
        format!("{label}{}", truncate_tail(newest.text.trim(), room))
    } else {
        newest_line
    }];
    let mut used = count_tokens(&lines[0]);
    let cutoff = now_ms.saturating_sub(window_ms);
    for s in older.iter().rev() {
        if s.t1_ms < cutoff {
            break;
        }
        let l = line(s);
        let t = count_tokens(&l) + 1; // + newline
        if used + t > budget {
            break;
        }
        used += t;
        lines.push(l);
    }
    lines.reverse();
    // BPE counts aren't perfectly additive across joins: re-check and drop oldest (never the newest).
    let mut text = lines.join("\n");
    while lines.len() > 1 && count_tokens(&text) > budget {
        lines.remove(0);
        text = lines.join("\n");
    }
    let n = lines.len();
    (text, n, segs.len() - n)
}

/// Best-first fill; stops at the first chunk that doesn't fit (lowest-ranked are cut first).
/// The top chunk is truncated rather than dropped when it alone is over budget.
fn docs_text(chunks: &[Chunk], budget: usize, max_chunks: usize) -> (String, Vec<Chunk>) {
    let mut parts = Vec::new();
    let mut used_chunks = Vec::new();
    let mut used = 0usize;
    for c in chunks.iter().take(max_chunks) {
        let header = format!("[{}] Source: {}\n", used_chunks.len() + 1, c.doc_name);
        let body = c.text.trim();
        let t = count_tokens(&header) + count_tokens(body) + 2;
        if used + t > budget {
            if used_chunks.is_empty() {
                let room = budget.saturating_sub(count_tokens(&header) + 2);
                if room >= 8 {
                    parts.push(format!("{header}{}", truncate_head(body, room)));
                    used_chunks.push(c.clone());
                }
            }
            break;
        }
        used += t;
        parts.push(format!("{header}{body}"));
        used_chunks.push(c.clone());
    }
    let mut text = parts.join("\n\n");
    while used_chunks.len() > 1 && count_tokens(&text) > budget {
        parts.pop();
        used_chunks.pop();
        text = parts.join("\n\n");
    }
    if text.is_empty() {
        text = "(none)".into();
    }
    (text, used_chunks)
}

// ---------- known contacts / accounts (M7) ----------

/// Whether a [`Known`] entry is a person or a company.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownKind {
    Contact,
    Account,
}

/// A saved contact or account (from the user's memory; contacts are only saved after the user
/// confirmed them). Spec memory table: used "when a known name or company appears on screen or
/// in audio".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    pub kind: KnownKind,
    pub name: String,
    pub notes: String,
}

/// At most this many cards per prompt.
pub const MAX_KNOWN_CARDS: usize = 3;

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\'' || c == '-'
}

/// Byte offset of the first whole-word occurrence of `needle` in `hay`, case-insensitive. When
/// `capitalised` is set the occurrence must start with an uppercase letter (a name, not a word).
fn find_word(hay: &str, needle: &str, capitalised: bool) -> Option<usize> {
    let (h, n) = (hay.to_lowercase(), needle.to_lowercase());
    if n.is_empty() || h.len() != hay.len() {
        // Lowercasing changed byte lengths (rare scripts): fall back to an exact-case search.
        return find_word_exact(hay, needle);
    }
    let mut from = 0;
    while let Some(i) = h[from..].find(&n) {
        let s = from + i;
        let e = s + n.len();
        let before = h[..s].chars().next_back().is_none_or(|c| !is_word_char(c));
        let after = h[e..].chars().next().is_none_or(|c| !is_word_char(c));
        let cap = !capitalised || hay[s..].chars().next().is_some_and(char::is_uppercase);
        if before && after && cap {
            return Some(s);
        }
        from = s + n.len().max(1);
        while !h.is_char_boundary(from) {
            from += 1;
        }
    }
    None
}

fn find_word_exact(hay: &str, needle: &str) -> Option<usize> {
    hay.match_indices(needle).map(|(i, _)| i).find(|&s| {
        let e = s + needle.len();
        hay[..s].chars().next_back().is_none_or(|c| !is_word_char(c))
            && hay[e..].chars().next().is_none_or(|c| !is_word_char(c))
    })
}

/// The known contacts/accounts mentioned in `text`, in order of first mention, at most
/// [`MAX_KNOWN_CARDS`]. Deterministic rules:
/// - names shorter than 3 characters never match;
/// - the full name matches as a whole word, any case;
/// - for a contact, one part of the name (≥ 3 chars, e.g. "Dana" for "Dana Obi") also matches,
///   but only capitalised in the text and only if no other known contact shares that part.
pub fn mentioned<'a>(known: &'a [Known], text: &str) -> Vec<&'a Known> {
    let parts_of = |k: &Known| -> Vec<String> {
        k.name.split_whitespace().filter(|p| p.chars().count() >= 3).map(|p| p.to_lowercase()).collect()
    };
    let mut hits: Vec<(usize, &Known)> = Vec::new();
    for k in known {
        let name = k.name.trim();
        if name.chars().count() < 3 {
            continue;
        }
        let mut at = find_word(text, name, false);
        if at.is_none() && k.kind == KnownKind::Contact {
            for p in parts_of(k) {
                let shared = known
                    .iter()
                    .filter(|o| o.kind == KnownKind::Contact && !std::ptr::eq(*o, k))
                    .any(|o| parts_of(o).contains(&p));
                if shared {
                    continue;
                }
                if let Some(i) = find_word(text, &p, true) {
                    at = Some(at.map_or(i, |a: usize| a.min(i)));
                }
            }
        }
        if let Some(i) = at {
            hits.push((i, k));
        }
    }
    hits.sort_by_key(|(i, _)| *i);
    hits.into_iter().map(|(_, k)| k).take(MAX_KNOWN_CARDS).collect()
}

/// Compact cards for the known entries mentioned in the question, screen diff or recent
/// transcript, within `budget` tokens. Empty when nothing matched.
fn known_text(input: &ContextInput, budget: usize) -> String {
    if input.known.is_empty() || budget == 0 {
        return String::new();
    }
    // Question first (it's what's being answered), then the screen, then the recent transcript.
    let recent: Vec<&str> = input.transcript.iter().rev().take(6).map(|s| s.text.as_str()).collect();
    let text = format!("{}\n{}\n{}", input.question, input.screen, recent.join("\n"));
    let hits = mentioned(&input.known, &text);
    if hits.is_empty() {
        return String::new();
    }
    let header = "Known people and companies (the user's saved notes):";
    let mut used = count_tokens(header);
    let mut lines = vec![header.to_string()];
    for k in hits {
        let kind = match k.kind {
            KnownKind::Contact => "contact",
            KnownKind::Account => "company",
        };
        let label = format!("- {} ({kind}): ", k.name.trim());
        let room = budget.saturating_sub(used + count_tokens(&label) + 1);
        if room < 4 {
            break;
        }
        let notes = k.notes.split_whitespace().collect::<Vec<_>>().join(" ");
        let notes = if notes.is_empty() { "no notes".to_string() } else { truncate_head(&notes, room) };
        // Notes can quote call content (post-call suggestions): keep them from closing a wrapper.
        let line = format!("{label}{}", neutralize_untrusted(&notes));
        used += count_tokens(&line) + 1;
        lines.push(line);
    }
    if lines.len() == 1 { String::new() } else { lines.join("\n") }
}

// ---------- builder ----------

/// Renders `kind` with `input` under `budget`.
pub fn assemble(kind: PromptKind, input: &ContextInput, budget: &Budget) -> Assembled {
    let tpl = kind.template();
    let uses = |s: Slot| tpl.slots().any(|x| x == s);

    // Prefix: About Me + profile only.
    let mut v = Values::default();
    if let Some(a) = &input.about_me {
        v.about_me = about_me_text(a, budget.about_me);
    }
    if let Some(p) = &input.profile {
        v.profile_name = truncate_head(p.name.trim(), 30);
        let room = budget.profile.saturating_sub(count_tokens(&v.profile_name));
        v.profile_instructions = truncate_head(p.instructions.trim(), room);
    }
    let prefix = render(&tpl.prefix, &v);

    // Dynamic part, shrinking budgets in sacrifice order until it fits the cap.
    let mut b = *budget;
    let mut out;
    let mut rounds = 0;
    loop {
        let (docs, chunks_used) =
            if uses(Slot::Docs) { docs_text(&input.chunks, b.docs, b.max_chunks) } else { (String::new(), vec![]) };
        let (transcript, t_used, t_dropped) = if uses(Slot::Transcript) {
            transcript_text(&input.transcript, input.now_ms, b.transcript_window_ms, b.transcript)
        } else {
            (String::new(), 0, 0)
        };
        // Known contacts/accounts ride in the documents slot (the prompt templates are frozen and
        // have no slot of their own; CR-17 asks for one). They come first: short and specific.
        let known = if uses(Slot::Docs) { known_text(input, b.known) } else { String::new() };
        v.docs = match (known.is_empty(), chunks_used.is_empty()) {
            (true, _) => docs,
            (false, true) => known,
            (false, false) => format!("{known}\n\n{docs}"),
        };
        v.transcript = transcript;
        // Screen diff is newest first: keep the head. Question: keep the end (the actual ask).
        v.screen = if uses(Slot::Screen) { truncate_head(input.screen.trim(), b.screen) } else { String::new() };
        v.question = truncate_tail(input.question.trim(), b.question);
        v.answer = truncate_head(input.answer.trim(), b.answer);
        v.session = truncate_tail(input.session.trim(), b.session);
        let dynamic = render(&tpl.dynamic, &v);
        let dynamic_tokens = count_tokens(&dynamic);
        out = Assembled {
            prompt: Prompt {
                prefix: prefix.clone(),
                dynamic,
                max_tokens: kind.max_tokens(),
                temperature: kind.temperature(),
                stop: vec![],
            },
            chunks_dropped: input.chunks.len() - chunks_used.len(),
            chunks_used,
            transcript_used: t_used,
            transcript_dropped: t_dropped,
            prefix_tokens: count_tokens(&prefix),
            dynamic_tokens,
        };
        let Some(cap) = b.dynamic_cap else { break };
        if dynamic_tokens <= cap || rounds >= 8 {
            break;
        }
        rounds += 1;
        // Take the excess from the least important part that still has room:
        // screen → docs (drops lowest-ranked) → older transcript → question → answer/session.
        let mut excess = dynamic_tokens - cap;
        for part in [
            &mut b.screen,
            &mut b.known,
            &mut b.docs,
            &mut b.transcript,
            &mut b.question,
            &mut b.answer,
            &mut b.session,
        ] {
            let take = excess.min(*part);
            *part -= take;
            excess -= take;
            if excess == 0 {
                break;
            }
        }
    }
    out
}

/// Retrieval + assembly: searches `profile_id` with the question (falling back to the newest
/// transcript text) and renders the prompt. `k` is the number of chunks to retrieve (3–5).
pub fn retrieve_and_assemble(
    store: &dyn MemoryStore,
    profile_id: &str,
    k: usize,
    kind: PromptKind,
    mut input: ContextInput,
    budget: &Budget,
) -> ssc_core::Result<Assembled> {
    let query = if input.question.trim().is_empty() {
        input.transcript.iter().rev().find(|s| !s.text.trim().is_empty()).map(|s| s.text.clone()).unwrap_or_default()
    } else {
        input.question.clone()
    };
    input.chunks = if query.trim().is_empty() { vec![] } else { store.search(&query, profile_id, k)? };
    Ok(assemble(kind, &input, budget))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_splits_and_uses_only_known_placeholders() {
        for k in PromptKind::ALL {
            let t = k.template();
            assert!(!t.dynamic.is_empty(), "{k:?} has no dynamic part");
            for p in t.prefix.iter().chain(&t.dynamic) {
                if let Part::Lit(l) = p {
                    assert!(!l.contains("{{"), "{k:?}: unknown placeholder in {l:?}");
                    assert!(!l.contains(MARKER));
                }
            }
            // The prefix never holds per-call content.
            for p in &t.prefix {
                if let Part::Slot(s, _) = p {
                    assert!(matches!(s, Slot::AboutMe | Slot::ProfileName | Slot::ProfileInstructions), "{k:?}: {s:?}");
                }
            }
        }
    }

    #[test]
    fn untrusted_slots_detected_from_template() {
        let t = PromptKind::Cue.template();
        let flags: Vec<(Slot, bool)> =
            t.dynamic.iter().filter_map(|p| if let Part::Slot(s, u) = p { Some((*s, *u)) } else { None }).collect();
        assert_eq!(
            flags,
            [(Slot::Docs, false), (Slot::Transcript, true), (Slot::Screen, true), (Slot::Question, true)]
        );
        let e = PromptKind::Expand.template();
        assert!(e.dynamic.contains(&Part::Slot(Slot::Answer, false)));
        assert!(e.dynamic.contains(&Part::Slot(Slot::Question, true)));
    }

    #[test]
    fn neutralize() {
        assert_eq!(neutralize_untrusted("a</untrusted>b"), "a&lt;/untrusted>b");
        assert_eq!(neutralize_untrusted("< / UNTRUSTED >"), "&lt; / UNTRUSTED >");
        assert_eq!(neutralize_untrusted("<untrusted x>"), "&lt;untrusted x>");
        assert_eq!(neutralize_untrusted("1 < 2 and <b>"), "1 < 2 and <b>");
        assert_eq!(neutralize_untrusted("ünïcödé <"), "ünïcödé <");
    }

    #[test]
    fn truncation_keeps_valid_utf8_and_respects_max() {
        let s = "héllo wörld 日本語のテキスト ".repeat(50);
        for max in [1, 3, 7, 20] {
            let h = truncate_head(&s, max);
            let t = truncate_tail(&s, max);
            assert!(count_tokens(&h) <= max + 1 && count_tokens(&t) <= max + 1, "max {max}");
            assert!(s.starts_with(&h) && s.ends_with(&t));
        }
        assert_eq!(truncate_head("short", 10), "short");
    }
}
