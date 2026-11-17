//! Acceptance (Build Sheet §7 W2-BENCH): every harness runs on fixtures with no network and writes
//! its report. Reports go to a temp dir, not bench/reports.

use ssc_bench::{answers, cpu, decision, latency, stt, ttft};
use std::path::PathBuf;

fn out(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ssc-bench-gate-{}", uuid::Uuid::new_v4())).join(format!("{name}.md"))
}

fn written(p: &PathBuf) -> String {
    let s = std::fs::read_to_string(p).expect("report written");
    let _ = std::fs::remove_dir_all(p.parent().unwrap());
    s
}

#[tokio::test]
async fn latency_fixture() {
    let p = out("latency");
    latency::run_fixture(&p).await.unwrap();
    let s = written(&p);
    assert!(s.contains("cloud mode") && s.contains("local mode"), "{s}");
    assert!(s.contains("speech_end → phone_rendered (end to end)"));
    assert!(s.contains("**PASS**"), "scripted cloud fixture is inside the targets:\n{s}");
    assert!(s.contains("decision_done → context_ready"));
}

#[tokio::test]
async fn latency_jsonl_roundtrip() {
    // The fixture run writes real ssc-core timing JSONL; re-analysing it gives the same result.
    let p = out("latency");
    latency::run_fixture(&p).await.unwrap();
    let jsonl = p.parent().unwrap().join("latency-jsonl/timing-cloud.jsonl");
    let (_, a) = latency::run_jsonl(&[jsonl], latency::RunMode::Cloud, &p).unwrap();
    assert_eq!(a.answers, 20);
    assert_eq!(a.meets_targets(), Some(true));
    assert!(!a.e2e_is_frame_sent);
    written(&p);
}

#[tokio::test]
async fn ttft_fixture() {
    let p = out("ttft");
    let (_, results) = ttft::run_fixture(&p, 2).await.unwrap();
    assert_eq!(results.len(), 6, "3 targets × cached/uncached");
    for r in &results {
        assert!(r.errors().is_empty(), "{}: {:?}", r.label, r.errors());
        let s = r.ttft().unwrap();
        assert_eq!(s.n, 2);
        assert!(s.p50 >= 20, "{} ttft {} ms below the scripted delay", r.label, s.p50);
    }
    let s = written(&p);
    assert!(s.contains("groq openai/gpt-oss-120b") && s.contains("gemini") && s.contains("ollama gemma4:e2b"));
}

#[tokio::test]
async fn stt_fixture() {
    let p = out("stt");
    let (_, runs) = stt::run_fixture(&p).await.unwrap();
    assert_eq!(runs.len(), 2);
    for r in &runs {
        assert!(r.error.is_none(), "{}: {:?}", r.engine, r.error);
        assert_eq!(r.latencies.len(), 1, "{}: one end-of-utterance final", r.engine);
        assert_eq!(r.wer, Some(0.0), "{}: {:?}", r.engine, r.transcript);
    }
    let s = written(&p);
    assert!(s.contains("deepgram (loopback replay)") && s.contains("MockStt"));
}

#[tokio::test]
async fn decision_fixture() {
    let p = out("decision");
    let (_, results) = decision::run_fixture(&p).await.unwrap();
    let snippets = decision::load(&decision::synthetic_set_path()).unwrap();
    let jev = &results[0];
    let tev = &results[1];
    assert_eq!(jev.errors() + tev.errors(), 0);
    // Deliberate mistakes in the recorded fixtures (scripts/gen_decision_fixtures.py).
    assert_eq!(jev.accuracy(0, &snippets), (16, 16));
    assert_eq!(jev.accuracy(3, &snippets), (15, 16));
    assert_eq!(jev.accuracy(4, &snippets), (15, 16));
    assert_eq!(tev.accuracy(0, &snippets), (14, 16));
    assert_eq!(tev.accuracy(1, &snippets), (14, 16));
    assert_eq!(tev.accuracy(2, &snippets), (14, 16));
    assert_eq!(tev.needs_answer_confusion(&snippets), (11, 1, 1));
    let s = written(&p);
    assert!(s.contains("SYNTHETIC DATA"), "{s}");
    assert!(s.contains("`syn-007`: wrong needs_answer, intent, answer_tier"), "{s}");
}

#[test]
fn cpu_fixture() {
    let p = out("cpu");
    let (_, stats) = cpu::run_fixture(&p).unwrap();
    assert_eq!(stats.len(), 2);
    assert!(stats.iter().all(cpu::PhaseStats::passes));
    let s = written(&p);
    assert!(s.contains("| idle |") && s.contains("| capturing |"));
}

#[test]
fn answers_fixture() {
    let p = out("answers");
    let (_, checks) = answers::run_fixture(&p).unwrap();
    assert_eq!(checks.iter().filter(|c| c.all_pass()).count(), 6);
    let s = written(&p);
    assert!(s.contains("Rated useful: 62.5% (5/8)"), "{s}");
    assert!(s.contains("`ans-004`") && s.contains("99.99"));
}
