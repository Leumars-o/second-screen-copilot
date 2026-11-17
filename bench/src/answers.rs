//! `bench answers`: cue quality against the prompt rules (`contracts/prompts`). Every rule check is
//! deterministic code; usefulness comes only from tester ratings (`rating`), never a model judge.
//!
//! Rules checked per answer (raw model output, before the parser trims anything):
//! - **grammar**: every non-empty line is a cue, `---`, a detail line, `FACT:`, `SOURCE:` or
//!   `NOT_IN_DOCS` (no intro, summary or filler lines).
//! - **cue count**: 3–7 cues, unless it is a fact answer or "Not in your docs".
//! - **cue words**: every cue has 2–6 words.
//! - **no invented numbers**: every number in the answer appears in the retrieved chunks or the
//!   question (digits only; spelled-out numbers are not checked).
//! - **not in docs**: `NOT_IN_DOCS` exactly when the question is product-specific and retrieval
//!   came back empty.

use crate::report::{Report, pct};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use ssc_router::grammar::{MAX_CUE_WORDS, MAX_CUES, MIN_CUES};
use ssc_router::parse_answer;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const MIN_CUE_WORDS: usize = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerItem {
    pub id: String,
    #[serde(default)]
    pub synthetic: bool,
    /// Prompt used to generate it in `--live`: `cue`, `fact` or `objection`.
    #[serde(default = "default_kind")]
    pub kind: String,
    pub question: String,
    /// True when the question is about the user's product (so docs must back the answer).
    pub product_specific: bool,
    /// Retrieved chunks the answer was generated from (empty = retrieval found nothing).
    #[serde(default)]
    pub chunks: Vec<DocChunk>,
    /// Raw model output. Empty in a set meant only for `--live` generation.
    #[serde(default)]
    pub answer: String,
    /// Tester rating: "up", "down" or null.
    #[serde(default)]
    pub rating: Option<Rating>,
    #[serde(default)]
    pub notes: String,
}

fn default_kind() -> String {
    "cue".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocChunk {
    pub doc: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rating {
    Up,
    Down,
}

pub fn load(path: &Path) -> Result<Vec<AnswerItem>> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    s.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("{}:{}", path.display(), i + 1)))
        .collect()
}

pub fn synthetic_set_path() -> PathBuf {
    crate::bench_path("data/answers.synthetic.jsonl")
}

// ---------- line classification ----------

/// `"12. text"` / `"12: text"` → text.
fn numbered(line: &str, sep: char) -> Option<&str> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = line[digits..].strip_prefix(sep)?;
    rest.starts_with(char::is_whitespace).then(|| rest.trim())
}

fn prefixed<'a>(line: &'a str, p: &str) -> Option<&'a str> {
    line.get(..p.len()).filter(|h| h.eq_ignore_ascii_case(p)).map(|_| line[p.len()..].trim())
}

#[derive(Debug, Clone, Default)]
struct Lines<'a> {
    cues: Vec<&'a str>,
    /// Text of detail, FACT and SOURCE lines (numbers are checked in these too).
    other_text: Vec<&'a str>,
    stray: Vec<&'a str>,
}

fn classify(answer: &str) -> Lines<'_> {
    let mut out = Lines::default();
    let mut detail = false;
    for raw in answer.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let nid = line.trim_end_matches('.').eq_ignore_ascii_case("NOT_IN_DOCS");
        // SOURCE: doc names may contain digits ("pricing-2026.md"); not a claim, so not collected.
        let source = prefixed(line, "SOURCE:").is_some();
        if line == "---" {
            detail = true;
        } else if nid || source {
            continue;
        } else if let Some(t) = prefixed(line, "FACT:") {
            out.other_text.push(t);
        } else if let (false, Some(t)) = (detail, numbered(line, '.')) {
            out.cues.push(t);
        } else if let (true, Some(t)) = (detail, numbered(line, ':')) {
            out.other_text.push(t);
        } else {
            out.stray.push(line);
        }
    }
    out
}

/// Every number written with digits: `$23,520` → "23520", `99.99%` → "99.99". A trailing period
/// (end of sentence) is not part of the number.
pub fn numbers(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut s = String::new();
        while i < chars.len() {
            let c = chars[i];
            let joins_digits =
                (c == ',' || c == '.') && chars.get(i + 1).is_some_and(char::is_ascii_digit) && !s.is_empty();
            if c.is_ascii_digit() {
                s.push(c);
            } else if joins_digits {
                if c == '.' {
                    s.push('.');
                }
            } else {
                break;
            }
            i += 1;
        }
        out.insert(s);
    }
    out
}

fn word_count(cue: &str) -> usize {
    cue.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).count()
}

// ---------- checks ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Checks {
    pub grammar: bool,
    pub cue_count: bool,
    pub cue_words: bool,
    pub no_invented_numbers: bool,
    pub not_in_docs: bool,
    /// Human-readable reasons for each failure.
    pub problems: Vec<String>,
}

impl Checks {
    pub fn all_pass(&self) -> bool {
        self.grammar && self.cue_count && self.cue_words && self.no_invented_numbers && self.not_in_docs
    }
}

pub fn check(item: &AnswerItem) -> Checks {
    let parsed = parse_answer(&item.answer);
    let lines = classify(&item.answer);
    let mut c = Checks { grammar: lines.stray.is_empty(), ..Checks::default() };

    if !c.grammar {
        c.problems.push(format!("{} line(s) outside the grammar, e.g. {:?}", lines.stray.len(), lines.stray[0]));
    }

    let n = lines.cues.len();
    c.cue_count = (MIN_CUES..=MAX_CUES).contains(&n) || (n == 0 && (parsed.fact.is_some() || parsed.not_in_docs));
    if !c.cue_count {
        c.problems.push(format!("{n} cues (want {MIN_CUES}–{MAX_CUES})"));
    }

    let bad: Vec<String> = lines
        .cues
        .iter()
        .filter(|q| !(MIN_CUE_WORDS..=MAX_CUE_WORDS).contains(&word_count(q)))
        .map(|q| format!("{q:?} ({} words)", word_count(q)))
        .collect();
    c.cue_words = bad.is_empty();
    if !c.cue_words {
        c.problems.push(format!("cue length {MIN_CUE_WORDS}–{MAX_CUE_WORDS} words: {}", bad.join(", ")));
    }

    let mut allowed = numbers(&item.question);
    for ch in &item.chunks {
        allowed.extend(numbers(&ch.text));
    }
    let mut used = BTreeSet::new();
    for t in lines.cues.iter().chain(&lines.other_text) {
        used.extend(numbers(t));
    }
    let invented: Vec<&String> = used.difference(&allowed).collect();
    c.no_invented_numbers = invented.is_empty();
    if !c.no_invented_numbers {
        let list: Vec<&str> = invented.iter().map(|s| s.as_str()).collect();
        c.problems.push(format!("numbers not in docs or question: {}", list.join(", ")));
    }

    let should = item.product_specific && item.chunks.is_empty();
    c.not_in_docs = parsed.not_in_docs == should;
    if !c.not_in_docs {
        c.problems.push(if should {
            "should say Not in your docs (product question, nothing retrieved)".into()
        } else {
            "said Not in your docs although it shouldn't".into()
        });
    }
    c
}

// ---------- report ----------

pub fn render(r: &mut Report, items: &[AnswerItem]) {
    let synthetic = items.iter().filter(|i| i.synthetic).count();
    if synthetic > 0 {
        r.para(&format!(
            "**SYNTHETIC DATA: {synthetic} of {} answers and ratings are synthetic examples.**",
            items.len()
        ));
    }
    let checks: Vec<Checks> = items.iter().map(check).collect();
    let count = |f: fn(&Checks) -> bool| checks.iter().filter(|c| f(c)).count();
    let n = items.len();
    let rows = vec![
        vec!["grammar only (no intro/filler lines)".to_string(), pct(count(|c| c.grammar), n)],
        vec![format!("cue count {MIN_CUES}–{MAX_CUES}"), pct(count(|c| c.cue_count), n)],
        vec![format!("words per cue {MIN_CUE_WORDS}–{MAX_CUE_WORDS}"), pct(count(|c| c.cue_words), n)],
        vec!["no invented numbers".to_string(), pct(count(|c| c.no_invented_numbers), n)],
        vec!["\"Not in your docs\" when (and only when) it should".to_string(), pct(count(|c| c.not_in_docs), n)],
        vec!["**all rules**".to_string(), pct(count(Checks::all_pass), n)],
    ];
    r.table(&["rule", "pass"], &rows);

    let rated: Vec<Rating> = items.iter().filter_map(|i| i.rating).collect();
    let up = rated.iter().filter(|r| **r == Rating::Up).count();
    r.h2("Usefulness (tester ratings)");
    r.para(&format!(
        "Rated useful: {} — target ≥ 60%. Unrated: {}. Ratings come from testers' thumbs up/down \
         (`answer_feedback`), never from a model.",
        pct(up, rated.len()),
        n - rated.len()
    ));

    r.h2("Failures");
    let mut any = false;
    for (it, c) in items.iter().zip(&checks) {
        if !c.all_pass() {
            any = true;
            r.bullet(&format!("`{}`: {}", it.id, c.problems.join("; ")));
        }
    }
    if !any {
        r.bullet("none");
    }
    r.end_list();
}

pub fn run_fixture(report_path: &Path) -> Result<(Report, Vec<Checks>)> {
    let items = load(&synthetic_set_path())?;
    let mut r = Report::new("bench/answers — fixture mode");
    r.para("Recorded (synthetic) answers in `data/answers.synthetic.jsonl`, checked against the prompt rules.");
    render(&mut r, &items);
    r.write(report_path)?;
    Ok((r, items.iter().map(check).collect()))
}

// ---------- live mode ----------

#[cfg(feature = "live")]
pub mod live {
    use super::*;
    use crate::live::{Keys, load_config};
    use ssc_context::{Budget, ContextInput, PromptKind, assemble};
    use ssc_core::secrets::SecretKey;
    use ssc_core::traits::{AnswerModel, Cancel};
    use ssc_core::types::{Chunk, Profile};
    use ssc_router::providers::openai::GROQ_BASE_URL;
    use ssc_router::providers::{Ollama, OpenAiCompat};
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    pub struct LiveOpts {
        pub data: PathBuf,
        /// `groq` (default) or `ollama:<model>`.
        pub model: String,
    }

    fn prompt_for(item: &AnswerItem, local: bool) -> ssc_core::types::Prompt {
        let kind = match item.kind.as_str() {
            "fact" => PromptKind::Fact,
            "objection" => PromptKind::Objection,
            _ => PromptKind::Cue,
        };
        let input = ContextInput {
            profile: Some(Profile { id: "bench".into(), name: "Bench".into(), instructions: String::new() }),
            chunks: item
                .chunks
                .iter()
                .map(|c| Chunk {
                    id: Uuid::nil(),
                    doc_id: Uuid::nil(),
                    doc_name: c.doc.clone(),
                    text: c.text.clone(),
                    score: 1.0,
                })
                .collect(),
            question: item.question.clone(),
            ..ContextInput::default()
        };
        assemble(kind, &input, &if local { Budget::local() } else { Budget::cloud() }).prompt
    }

    /// Generate a fresh answer for every item with the chosen model, then run the rule checks.
    /// Ratings in the file are dropped: they rated a different answer.
    pub async fn run(report_path: &Path, o: &LiveOpts) -> Result<Report> {
        let mut items = load(&o.data)?;
        let cfg = load_config();
        let (model, local): (Arc<dyn AnswerModel>, bool) = match o.model.strip_prefix("ollama:") {
            Some(m) => (Arc::new(Ollama::new(&cfg.models.ollama_url, m)), true),
            None => {
                let Some(k) = Keys.get(SecretKey::Groq) else {
                    anyhow::bail!("no `api.groq` key in the keychain; use --model ollama:<model> for a local run")
                };
                (Arc::new(OpenAiCompat::groq(GROQ_BASE_URL, &cfg.models.groq_model, k)), false)
            }
        };
        let mut failures = 0;
        for it in &mut items {
            let (tx, mut rx) = mpsc::channel::<ssc_core::types::Token>(256);
            let p = prompt_for(it, local);
            let collect = async {
                let mut s = String::new();
                while let Some(t) = rx.recv().await {
                    s.push_str(&t.text);
                }
                s
            };
            let (res, text) = tokio::join!(model.stream(&p, tx, Cancel::new()), collect);
            if let Err(e) = res {
                failures += 1;
                eprintln!("answers: {} failed: {e}", it.id);
            }
            it.answer = text;
            it.rating = None;
        }
        let mut r = Report::new("bench/answers — live");
        r.para(&format!(
            "Model: {}. Data: `{}`. Generation errors: {failures}. Usefulness needs tester ratings \
             of these answers; rule checks below are deterministic.",
            model.id(),
            o.data.display()
        ));
        render(&mut r, &items);
        r.h2("Generated answers");
        for it in &items {
            r.para(&format!("`{}` — {}\n\n```\n{}\n```", it.id, it.question, it.answer.trim()));
        }
        r.write(report_path)?;
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(answer: &str, product: bool, chunks: &[&str]) -> AnswerItem {
        AnswerItem {
            id: "t".into(),
            synthetic: true,
            kind: "cue".into(),
            question: "What about 40 seats?".into(),
            product_specific: product,
            chunks: chunks.iter().map(|t| DocChunk { doc: "d.md".into(), text: (*t).into() }).collect(),
            answer: answer.into(),
            rating: None,
            notes: String::new(),
        }
    }

    #[test]
    fn number_extraction() {
        let n = numbers("$23,520 a year, 99.99% uptime, SOC 2. Version 3.");
        assert_eq!(n.into_iter().collect::<Vec<_>>(), vec!["2", "23520", "3", "99.99"]);
        assert!(numbers("no digits").is_empty());
    }

    #[test]
    fn good_cue_answer_passes() {
        let c = check(&item(
            "1. Two weeks typical\n2. Named manager\n3. Kickoff first\n---\n1: Live in 2 weeks.",
            true,
            &["Onboarding takes 2 weeks."],
        ));
        assert!(c.all_pass(), "{:?}", c.problems);
    }

    #[test]
    fn each_rule_fails_on_its_own() {
        let ok_chunks = ["Price is $49."];
        // grammar
        let c = check(&item("Sure!\n1. a b\n2. c d\n3. e f", true, &ok_chunks));
        assert!(!c.grammar && c.cue_count && c.cue_words);
        // cue count
        let c = check(&item("1. a b\n2. c d", true, &ok_chunks));
        assert!(!c.cue_count && c.grammar);
        let eight: String = (1..=8).map(|i| format!("{i}. cue words\n")).collect();
        assert!(!check(&item(&eight, true, &ok_chunks)).cue_count);
        // words per cue (raw, before the parser's 6-word truncation)
        let c = check(&item("1. one\n2. a b\n3. this cue has far too many words", true, &ok_chunks));
        assert!(!c.cue_words && c.cue_count);
        // invented numbers: 49 and 40 are backed, 12 is not
        assert!(
            check(&item("1. $49 per seat\n2. 40 seats total\n3. ask budget", true, &ok_chunks)).no_invented_numbers
        );
        let c = check(&item("1. $49 per seat\n2. 12 month term\n3. ask budget", true, &ok_chunks));
        assert!(!c.no_invented_numbers && c.problems.iter().any(|p| p.contains("12")));
        // not in docs: missing and spurious
        assert!(!check(&item("1. a b\n2. c d\n3. e f", true, &[])).not_in_docs);
        assert!(check(&item("NOT_IN_DOCS", true, &[])).all_pass());
        assert!(!check(&item("NOT_IN_DOCS", true, &ok_chunks)).not_in_docs);
        // not product-specific with no docs: cues are fine
        assert!(check(&item("1. keep it short\n2. ask theirs\n3. steer to agenda", false, &[])).all_pass());
        // fact answer: zero cues allowed, SOURCE digits ignored
        assert!(check(&item("FACT: $49 per seat\nSOURCE: pricing-2026.md", true, &ok_chunks)).all_pass());
    }

    #[test]
    fn synthetic_set_expected_failures() {
        let items = load(&synthetic_set_path()).unwrap();
        assert!(items.iter().all(|i| i.synthetic));
        let failing: Vec<&str> = items.iter().filter(|i| !check(i).all_pass()).map(|i| i.id.as_str()).collect();
        assert_eq!(failing, vec!["ans-004", "ans-006", "ans-008", "ans-009"]);
    }
}
