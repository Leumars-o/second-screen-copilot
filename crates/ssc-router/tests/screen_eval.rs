//! CR-20 eval: the screen prompt (`screen.v3.md`) + a screenshot → each vision model.
//! Eight synthetic screens with one known answer each (`fixtures/screens`, regenerate with
//! `gen.swift`). Graded deterministically on the answer line (text before `---`).
//! Live (real network, keychain keys), never run by the gate:
//! `cargo test -p ssc-router --features live --test screen_eval -- --nocapture --test-threads=1`

#![cfg(feature = "live")]

use ssc_core::secrets::{KeychainStore, SecretKey, SecretStore};
use ssc_core::traits::{AnswerModel, Cancel};
use ssc_core::types::{Prompt, PromptImage};
use ssc_router::providers::openai::{GEMINI_BASE_URL, GROQ_BASE_URL};
use ssc_router::providers::{GeminiTier, OpenAiCompat};
use std::time::Instant;
use tokio::sync::mpsc;

/// (fixture, window label, any of these in the answer line passes; all lower-case)
const CASES: &[(&str, &str, &[&str])] = &[
    ("diagram_arrow", "Chrome — Quiz", &["c"]),
    ("word_inference", "Microsoft Word — Notes.docx", &["predict", "trained model", "new data"]),
    ("math_speed", "Safari — Assessment", &["72"]),
    ("mcq_planet", "Chrome — Trivia Night", &["mars"]),
    ("chart_quarter", "Excel — Revenue 2026", &["q3"]),
    ("code_print", "VS Code — interview.py", &["6"]),
    ("no_question", "Safari — Weather", &["no question found"]),
    ("product_price", "Intercom — Customer chat", &["not in your documents"]),
    ("wps_inference", "WPS Office — Interview prep.docx", &["predict", "trained model", "new data", "new input"]),
];

/// Window text sent with a case (the accessibility read), like the lead's WPS run: toolbar noise
/// around the actual question.
fn window_text(name: &str) -> &'static str {
    match name {
        "wps_inference" => {
            "Home\nInsert\nPage Layout\nReferences\nReview\nView\nTools\nPaste\nFormat Painter\nCalibri\n11\nStyles\nFind and Replace\nInterview prep\nWhat is inference in AI?\nPage 1 of 1\n6 words\nEnglish (US)"
        }
        _ => "",
    }
}

/// Realistic context, like the lead's (profile + docs about their own product): it must shape
/// the answer, not replace it.
const ABOUT_ME: &str = "I'm an AI engineer at a Huawei partner. I build and sell AI contact-center solutions \
(voice bots, agent assist, analytics) to banks and telcos in West Africa.";
const DOCS: &str = "[huawei-aicc.md] Huawei AI Contact Center: omnichannel routing, voice bot, agent assist, \
real-time speech analytics. Deployed on Huawei Cloud or on-premises.\n[role.md] AI Engineer role: designs \
contact-center AI flows, integrates CRM, tunes ASR and NLU models.";

/// Meta talk about the screen instead of the answer (the lead's first real run, CR-20).
const META: &[&str] = &[
    "image shows",
    "the image",
    "screenshot",
    "the screen shows",
    "the question asks",
    "the document",
    "wps",
    "this is a conceptual question",
    "the window",
];

fn prompt(label: &str, window: &str, jpeg: Vec<u8>) -> Prompt {
    let t = include_str!("../../../contracts/prompts/screen.v3.md");
    let (prefix, dynamic) = t.split_once("<<<DYNAMIC>>>").unwrap();
    Prompt {
        prefix: prefix
            .replace("{{about_me}}", ABOUT_ME)
            .replace("{{profile_name}}", "ai-engineer")
            .replace("{{profile_instructions}}", "Interviews and client calls about our AI contact center."),
        dynamic: dynamic
            .replace("{{docs}}", DOCS)
            .replace("{{transcript}}", "")
            .replace("{{question}}", label)
            .replace("{{screen}}", window),
        max_tokens: 400,
        temperature: 0.2,
        stop: vec![],
        images: vec![PromptImage { mime: "image/jpeg".into(), data: jpeg }],
    }
}

/// The answer line: text before `---`, first non-empty line.
fn answer_line(text: &str) -> String {
    text.split("\n---").next().unwrap_or("").lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string()
}

fn passes(line: &str, expect: &[&str]) -> bool {
    let l = line.to_lowercase();
    expect.iter().any(|e| {
        if e.len() <= 2 {
            // Short answers ("c", "6", "q3"): a whole token, not a substring of a word.
            l.split(|c: char| !c.is_alphanumeric()).any(|w| w == *e)
        } else {
            l.contains(e)
        }
    })
}

/// `pause` between cases: Groq's per-minute token limit (8k on this key) counts image tokens.
async fn run(m: &dyn AnswerModel, pause: std::time::Duration) -> usize {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/screens");
    let mut ok = 0;
    for (i, (name, label, expect)) in CASES.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(pause).await;
        }
        let jpeg = std::fs::read(format!("{dir}/{name}.jpg")).unwrap();
        let (tx, mut rx) = mpsc::channel(1024);
        let t = Instant::now();
        let res = m.stream(&prompt(label, window_text(name), jpeg), tx, Cancel::new()).await;
        let mut text = String::new();
        while let Ok(tok) = rx.try_recv() {
            text.push_str(&tok.text);
        }
        let ms = t.elapsed().as_millis();
        match res {
            Ok(s) => {
                let line = answer_line(&text);
                let lower = text.to_lowercase();
                // "No question found" must say what is on screen: describing it is the answer.
                let meta = if *name == "no_question" { None } else { META.iter().find(|m| lower.contains(*m)) };
                let echoed = name.ends_with("_inference") && line.to_lowercase().contains("what is inference");
                // v3 (CR-21): no numbered points or bullets after the answer.
                let after = text.split_once("\n---").map_or("", |(_, d)| d);
                let listy = after.lines().map(str::trim).any(|l| {
                    l.starts_with("- ")
                        || l.starts_with("* ")
                        || l.starts_with("• ")
                        || l.split_once([':', '.', ')']).is_some_and(|(n, _)| {
                            !n.is_empty() && n.len() <= 2 && n.chars().all(|c| c.is_ascii_digit())
                        })
                });
                let pass = passes(&line, expect) && meta.is_none() && !echoed && !listy;
                if listy {
                    println!("     numbered/bulleted lines: {}", text.replace('\n', " | "));
                }
                if let Some(m) = meta {
                    println!("     meta talk ({m:?}): {}", text.replace('\n', " | "));
                }
                if echoed {
                    println!("     answer line restates the question");
                }
                ok += usize::from(pass);
                println!(
                    "{} {:<15} ttft {:>5} ms total {:>5} ms | {}",
                    if pass { "PASS" } else { "FAIL" },
                    name,
                    s.first_token_ms.unwrap_or(0),
                    ms,
                    line
                );
            }
            Err(e) => println!("ERR  {name:<15} {e}"),
        }
    }
    println!("== {}: {ok}/{} ==", m.id(), CASES.len());
    ok
}

fn key(k: SecretKey) -> Option<ssc_core::secrets::Secret> {
    KeychainStore.get(&k).ok().flatten()
}

#[test]
fn grading_is_strict_about_short_answers() {
    assert!(passes("C — the arrow points to box C", &["c"]));
    assert!(!passes("Correct answer is B", &["c"]));
    assert!(passes("72 km/h", &["72"]));
    assert!(!passes("172 km/h", &["72"]));
    assert_eq!(answer_line("\n Mars (B)\n---\n1: red dust\n"), "Mars (B)");
}

#[tokio::test]
async fn screen_eval_groq_qwen() {
    let Some(k) = key(SecretKey::Groq) else { return println!("skip: no Groq key") };
    let m = OpenAiCompat::groq(GROQ_BASE_URL, "qwen/qwen3.8-27b", k);
    let ok = run(&m, std::time::Duration::from_secs(25)).await;
    assert!(ok + 1 >= CASES.len(), "{ok}/{}", CASES.len());
}

#[tokio::test]
async fn screen_eval_gemini() {
    let Some(k) = key(SecretKey::Gemini) else { return println!("skip: no Gemini key") };
    let m = OpenAiCompat::gemini(GEMINI_BASE_URL, "gemini-3.5-flash-lite", k, GeminiTier::Paid, false).unwrap();
    let ok = run(&m, std::time::Duration::ZERO).await;
    assert!(ok + 1 >= CASES.len(), "{ok}/{}", CASES.len());
}
