//! CR-20 eval: the screen prompt (`screen.v1.md`) + a screenshot → each vision model.
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
];

fn prompt(label: &str, jpeg: Vec<u8>) -> Prompt {
    let t = include_str!("../../../contracts/prompts/screen.v1.md");
    let (prefix, dynamic) = t.split_once("<<<DYNAMIC>>>").unwrap();
    Prompt {
        prefix: prefix
            .replace("{{about_me}}", "AI engineer at Acme, which sells an AI contact-center product.")
            .replace("{{profile_name}}", "ai-engineer")
            .replace("{{profile_instructions}}", ""),
        dynamic: dynamic
            .replace("{{docs}}", "")
            .replace("{{transcript}}", "")
            .replace("{{question}}", label)
            .replace("{{screen}}", ""),
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
        let res = m.stream(&prompt(label, jpeg), tx, Cancel::new()).await;
        let mut text = String::new();
        while let Ok(tok) = rx.try_recv() {
            text.push_str(&tok.text);
        }
        let ms = t.elapsed().as_millis();
        match res {
            Ok(s) => {
                let line = answer_line(&text);
                let pass = passes(&line, expect);
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
    assert!(ok >= 7, "{ok}/8");
}

#[tokio::test]
async fn screen_eval_gemini() {
    let Some(k) = key(SecretKey::Gemini) else { return println!("skip: no Gemini key") };
    let m = OpenAiCompat::gemini(GEMINI_BASE_URL, "gemini-3.5-flash-lite", k, GeminiTier::Paid, false).unwrap();
    let ok = run(&m, std::time::Duration::ZERO).await;
    assert!(ok >= 7, "{ok}/8");
}
