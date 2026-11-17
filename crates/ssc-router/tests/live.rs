//! Live provider checks (real network, keys from the OS keychain). Never run by the gate:
//! `cargo test -p ssc-router --features live --test live -- --nocapture --test-threads=1`
//! Each test skips (passes with a note) when its key or service is missing.

#![cfg(feature = "live")]

use ssc_core::config::Config;
use ssc_core::decision::DecisionInput;
use ssc_core::secrets::{KeychainStore, SecretKey, SecretStore};
use ssc_core::traits::{AnswerModel, Cancel, DecisionModel};
use ssc_core::types::Prompt;
use ssc_router::providers::anthropic::{ANTHROPIC_BASE_URL, DEFAULT_CLAUDE_MODEL};
use ssc_router::providers::openai::{GEMINI_BASE_URL, GROQ_BASE_URL};
use ssc_router::providers::{Anthropic, GeminiTier, Ollama, OpenAiCompat};
use ssc_router::{SystemOneDecision, parse_answer};
use tokio::sync::mpsc;

fn prompt() -> Prompt {
    let cue = include_str!("../../../contracts/prompts/cue.v1.md");
    let (prefix, dynamic) = cue.split_once("<<<DYNAMIC>>>").unwrap();
    Prompt {
        prefix: prefix
            .replace("{{about_me}}", "Sales engineer")
            .replace("{{profile_name}}", "Sales")
            .replace("{{profile_instructions}}", ""),
        dynamic: dynamic
            .replace("{{docs}}", "")
            .replace("{{transcript}}", "")
            .replace("{{screen}}", "")
            .replace("{{question}}", "How would you run a software project from start to finish?"),
        max_tokens: 200,
        temperature: 0.2,
        stop: vec![],
    }
}

async fn check(m: &dyn AnswerModel) {
    let (tx, mut rx) = mpsc::channel(512);
    let stats = m.stream(&prompt(), tx, Cancel::new()).await.expect("stream");
    let mut text = String::new();
    while let Ok(t) = rx.try_recv() {
        text.push_str(&t.text);
    }
    let parsed = parse_answer(&text);
    println!("{}: first token {:?} ms, {} cues\n{text}", stats.model, stats.first_token_ms, parsed.cues.len());
    assert!(stats.first_token_ms.is_some());
}

#[tokio::test]
async fn live_groq() {
    let Some(k) = KeychainStore.get(&SecretKey::Groq).unwrap() else { return eprintln!("skip: no Groq key") };
    check(&OpenAiCompat::groq(GROQ_BASE_URL, &Config::default().models.groq_model, k)).await;
}

#[tokio::test]
async fn live_gemini() {
    let Some(k) = KeychainStore.get(&SecretKey::Gemini).unwrap() else { return eprintln!("skip: no Gemini key") };
    // Live checks use a test prompt only, so the free tier is acceptable here.
    let g = OpenAiCompat::gemini(GEMINI_BASE_URL, &Config::default().models.gemini_model, k, GeminiTier::Free, true)
        .unwrap();
    check(&g).await;
}

#[tokio::test]
async fn live_claude() {
    let Some(k) = KeychainStore.get(&SecretKey::Strong).unwrap() else { return eprintln!("skip: no strong key") };
    check(&Anthropic::new(ANTHROPIC_BASE_URL, DEFAULT_CLAUDE_MODEL, k)).await;
}

#[tokio::test]
async fn live_ollama() {
    let c = Config::default();
    check(&Ollama::new(&c.models.ollama_url, &c.models.local_answer_model)).await;
}

#[tokio::test]
async fn live_systemone() {
    let c = Config::default();
    let input = DecisionInput {
        question: "How much is the pro plan per seat?".into(),
        screen_diff: String::new(),
        profiles: vec!["sales".into(), "support".into()],
    };
    if let (false, Some(k)) =
        (c.models.decision_cloud_base_url.is_empty(), KeychainStore.get(&SecretKey::TypeSafe).unwrap())
    {
        let d = SystemOneDecision::new(&c.models.decision_cloud_base_url, &c.models.decision_cloud_model, Some(k));
        println!("jev: {:?}", d.decide(&input).await);
    } else {
        eprintln!("skip jev: no base url / key");
    }
    let tev = SystemOneDecision::new(&c.models.ollama_url, &c.models.decision_local_model, None);
    println!("tev1: {:?}", tev.decide(&input).await);
}
