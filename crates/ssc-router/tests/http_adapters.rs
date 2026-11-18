//! Adapters against recorded / hand-built SSE and NDJSON fixtures served by a local fake server.

mod common;

use common::{Reply, serve};
use ssc_core::SscError;
use ssc_core::decision::{DecisionInput, Tier};
use ssc_core::mock::MockClock;
use ssc_core::protocol::{Intent, Mode};
use ssc_core::secrets::Secret;
use ssc_core::traits::{AnswerModel, Cancel, DecisionModel};
use ssc_core::types::{Prompt, Token};
use ssc_router::budget::BudgetTracker;
use ssc_router::providers::{Anthropic, GeminiTier, Ollama, OpenAiCompat};
use ssc_router::{AnswerRouter, Route, SystemOneDecision};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const GROQ: &[u8] = include_bytes!("fixtures/groq_stream.sse");
const GEMINI: &[u8] = include_bytes!("fixtures/gemini_stream.sse");
const ANTHROPIC: &[u8] = include_bytes!("fixtures/anthropic_stream.sse");
const OLLAMA: &[u8] = include_bytes!("fixtures/ollama_chat.ndjson");
const CUES: &str = "1. Requirements\n2. Design\n3. Build\n---\n1: Ask about users\n";

fn prompt() -> Prompt {
    Prompt {
        prefix: "You are a copilot.".into(),
        dynamic: "Question: my card 4111 1111 1111 1111 — how much?".into(),
        max_tokens: 80,
        temperature: 0.2,
        stop: vec![],
        images: vec![],
    }
}

async fn collect(m: &dyn AnswerModel) -> (ssc_core::Result<ssc_core::types::StreamStats>, String) {
    let (tx, mut rx) = mpsc::channel(256);
    let res = m.stream(&prompt(), tx, Cancel::new()).await;
    let mut out = String::new();
    while let Ok(t) = rx.try_recv() {
        out.push_str(&t.text);
    }
    (res, out)
}

#[tokio::test]
async fn groq_streams_fixture() {
    let srv = serve(vec![Reply::sse(GROQ)]).await;
    let m = OpenAiCompat::groq(&srv.url(), "openai/gpt-oss-120b", Secret::new("test-key"));
    let (res, text) = collect(&m).await;
    let stats = res.unwrap();
    assert_eq!(text, CUES);
    assert_eq!(stats.tokens_out, 17);
    assert_eq!(stats.model, "groq/openai/gpt-oss-120b");
    assert!(stats.first_token_ms.is_some());
    let req = &srv.requests()[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/chat/completions");
    assert_eq!(req.header("authorization"), Some("Bearer test-key"));
    assert_eq!(req.json()["stream"], true);
}

#[tokio::test]
async fn gemini_streams_fixture() {
    let srv = serve(vec![Reply::sse(GEMINI)]).await;
    let m = OpenAiCompat::gemini(&srv.url(), "gemini-2.5-flash-lite", Secret::new("test-key"), GeminiTier::Paid, false)
        .unwrap();
    let (res, text) = collect(&m).await;
    assert_eq!(text, CUES);
    assert_eq!(res.unwrap().tokens_out, 16);
    let req = &srv.requests()[0];
    // Same OpenAI-compatible adapter as Groq: `{base}/chat/completions`, Bearer key.
    assert_eq!(req.path, "/chat/completions");
    assert_eq!(req.header("authorization"), Some("Bearer test-key"));
    assert!(!req.path.contains("key="), "key must travel in a header, not the URL");
    let body = req.json();
    assert_eq!(body["model"], "gemini-2.5-flash-lite");
    assert_eq!(body["stream"], true);
    assert_eq!(body["reasoning_effort"], "none");
}

#[tokio::test]
async fn ollama_streams_ndjson_fixture() {
    let srv = serve(vec![Reply::ok("application/x-ndjson", OLLAMA)]).await;
    let m = Ollama::new(&srv.url(), "gemma4:e2b");
    assert!(!m.is_cloud(), "127.0.0.1 is local");
    let (res, text) = collect(&m).await;
    assert_eq!(text, CUES);
    assert_eq!(res.unwrap().tokens_out, 15);
    let req = &srv.requests()[0];
    assert_eq!(req.path, "/api/chat");
    assert_eq!(req.json()["keep_alive"], "30m");
    assert_eq!(req.json()["think"], false);
    assert_eq!(req.json()["messages"][0]["content"], "You are a copilot.");
}

#[tokio::test]
async fn anthropic_streams_fixture() {
    let srv = serve(vec![Reply::sse(ANTHROPIC)]).await;
    let m = Anthropic::new(&srv.url(), "claude-opus-5-5", Secret::new("test-key"));
    let (res, text) = collect(&m).await;
    assert_eq!(text, "---\n1: Start with user interviews\n", "thinking blocks never reach the output");
    assert_eq!(res.unwrap().tokens_out, 12);
    let req = &srv.requests()[0];
    assert_eq!(req.path, "/v1/messages");
    assert_eq!(req.header("x-api-key"), Some("test-key"));
    assert!(req.header("authorization").is_none());
    assert_eq!(req.header("anthropic-version"), Some("2026-10-21"));
    let body = req.json();
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "low");
    assert!(body.get("temperature").is_none());
}

#[tokio::test]
async fn http_errors_map_to_router_errors() {
    let srv = serve(vec![
        Reply::status(429, r#"{"error":{"message":"slow down","type":"rate_limit_exceeded"}}"#)
            .header("retry-after", "1"),
        Reply::status(503, r#"{"error":{"message":"busy","type":"overloaded"}}"#),
    ])
    .await;
    let m = OpenAiCompat::groq(&srv.url(), "m", Secret::new("k"));
    let (r1, _) = collect(&m).await;
    assert!(matches!(r1, Err(SscError::RateLimited { retry_after_ms: Some(1000) })), "{r1:?}");
    let (r2, _) = collect(&m).await;
    assert!(matches!(r2, Err(SscError::Unavailable(_))), "{r2:?}");
}

#[tokio::test]
async fn connection_refused_is_unavailable() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let (r, _) = collect(&Ollama::new(&url, "m")).await;
    assert!(matches!(r, Err(SscError::Unavailable(_))), "{r:?}");
}

/// Acceptance: cancellation stops the stream within 50 ms, and the HTTP stream is closed.
#[tokio::test]
async fn cancellation_stops_stream_within_50ms() {
    let parts: Vec<&[u8]> = vec![
        b"data: {\"choices\":[{\"delta\":{\"content\":\"1. Req\"}}]}\n\n",
        b"data: {\"choices\":[{\"delta\":{\"content\":\"uirements\\n\"}}]}\n\n",
        b"data: {\"choices\":[{\"delta\":{\"content\":\"2. never\\n\"}}]}\n\n",
        b"data: {\"choices\":[{\"delta\":{\"content\":\"3. never\\n\"}}]}\n\n",
        b"data: {\"choices\":[{\"delta\":{\"content\":\"4. never\\n\"}}]}\n\n",
    ];
    let srv =
        serve(vec![Reply::chunked("text/event-stream", &parts, Duration::ZERO, Duration::from_millis(300))]).await;
    let m = Arc::new(OpenAiCompat::groq(&srv.url(), "m", Secret::new("k")));
    let (tx, mut rx) = mpsc::channel::<Token>(16);
    let cancel = Cancel::new();
    let (m2, c2) = (m.clone(), cancel.clone());
    let task = tokio::spawn(async move { m2.stream(&prompt(), tx, c2).await });
    assert_eq!(rx.recv().await.unwrap().text, "1. Req");
    let t = Instant::now();
    cancel.cancel();
    let res = task.await.unwrap();
    let took = t.elapsed();
    assert!(matches!(res, Err(SscError::Cancelled)), "{res:?}");
    assert!(took < Duration::from_millis(50), "cancel took {took:?}");
    // The server notices the hang-up on a following write.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(*srv.client_hung_up.lock().unwrap(), "stream was not closed");
}

#[tokio::test]
async fn router_cancellation_within_50ms_through_real_adapter() {
    let parts: Vec<&[u8]> =
        vec![b"data: {\"choices\":[{\"delta\":{\"content\":\"1. A\\n\"}}]}\n\n", b"data: [DONE]\n\n"];
    let srv = serve(vec![Reply::chunked("text/event-stream", &parts, Duration::ZERO, Duration::from_secs(5))]).await;
    let groq = Arc::new(OpenAiCompat::groq(&srv.url(), "m", Secret::new("k")));
    let router = Arc::new(AnswerRouter::new(Mode::Cloud, None, vec![Route::new(groq)], None).unwrap());
    let (tx, mut rx) = mpsc::channel(16);
    let cancel = Cancel::new();
    let (r2, c2) = (router.clone(), cancel.clone());
    let task = tokio::spawn(async move { r2.stream(&prompt(), tx, &c2).await });
    rx.recv().await.unwrap();
    let t = Instant::now();
    cancel.cancel();
    let res = task.await.unwrap();
    assert!(matches!(res, Err(SscError::Cancelled)));
    assert!(t.elapsed() < Duration::from_millis(50), "{:?}", t.elapsed());
}

/// Groq 429 → Gemini, end to end over HTTP, with redaction applied to every cloud request body.
#[tokio::test]
async fn groq_429_fails_over_to_gemini_with_redaction() {
    let groq_srv = serve(vec![Reply::status(429, r#"{"error":{"type":"rate_limit_exceeded"}}"#)]).await;
    let gem_srv = serve(vec![Reply::sse(GEMINI)]).await;
    let clock = Arc::new(MockClock::new(0));
    let budget = Arc::new(BudgetTracker::new(Default::default(), clock));
    let groq = Arc::new(OpenAiCompat::groq(&groq_srv.url(), "m", Secret::new("k")));
    let gem = Arc::new(OpenAiCompat::gemini(&gem_srv.url(), "g", Secret::new("k"), GeminiTier::Paid, false).unwrap());
    let router = AnswerRouter::new(
        Mode::Cloud,
        Some(Default::default()),
        vec![Route::new(groq).first_token_ms(1000).budget(budget.clone()), Route::new(gem)],
        None,
    )
    .unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    let stats = router.stream(&prompt(), tx, &Cancel::new()).await.unwrap();
    assert_eq!(stats.model, "gemini/g");
    let mut text = String::new();
    while let Ok(t) = rx.try_recv() {
        text.push_str(&t.text);
    }
    assert_eq!(text, CUES);
    for srv in [&groq_srv, &gem_srv] {
        let body = String::from_utf8(srv.requests()[0].body.clone()).unwrap();
        assert!(!body.contains("4111"), "card number left the PC: {body}");
        assert!(body.contains("[CARD]"));
    }
    assert!(!budget.try_reserve(1), "429 puts Groq in cool-down");
}

/// No first token within the deadline → next provider (real HTTP, short deadline).
#[tokio::test]
async fn slow_first_token_fails_over() {
    let slow = serve(vec![Reply::chunked("text/event-stream", &[GROQ], Duration::from_secs(2), Duration::ZERO)]).await;
    let fast = serve(vec![Reply::sse(GEMINI)]).await;
    let router = AnswerRouter::new(
        Mode::Cloud,
        None,
        vec![
            Route::new(Arc::new(OpenAiCompat::groq(&slow.url(), "m", Secret::new("k")))).first_token_ms(200),
            Route::new(Arc::new(
                OpenAiCompat::gemini(&fast.url(), "g", Secret::new("k"), GeminiTier::Paid, false).unwrap(),
            )),
        ],
        None,
    )
    .unwrap();
    let (tx, _rx) = mpsc::channel(256);
    let t = Instant::now();
    let stats = router.stream(&prompt(), tx, &Cancel::new()).await.unwrap();
    assert_eq!(stats.model, "gemini/g");
    assert!(t.elapsed() < Duration::from_millis(1500));
}

#[tokio::test]
async fn systemone_decision_over_http() {
    let srv = serve(vec![Reply::ok("application/json", include_bytes!("fixtures/systemone_response.json"))]).await;
    let d = SystemOneDecision::new(&srv.url(), "jev-1.14.0", Some(Secret::new("ts-key")));
    let input = DecisionInput {
        question: "How much is the pro plan?".into(),
        screen_diff: "Pricing page".into(),
        profiles: vec!["sales".into(), "support".into()],
    };
    let out = d.decide(&input).await.unwrap();
    assert_eq!(out.intent.value, Intent::Pricing);
    let req = &srv.requests()[0];
    assert_eq!(req.path, "/v1/systemone");
    assert_eq!(req.header("authorization"), Some("Bearer ts-key"));
    let expected: serde_json::Value = serde_json::from_str(include_str!("fixtures/systemone_request.json")).unwrap();
    assert_eq!(req.json(), expected);
}

#[tokio::test]
async fn systemone_bad_payload_is_invalid() {
    let srv = serve(vec![Reply::ok("application/json", br#"{"answers":{"needs_answer":{"noul":"yes"}}}"#)]).await;
    let d = SystemOneDecision::new(&srv.url(), "tev1:4b", None);
    let input =
        DecisionInput { question: "q".into(), screen_diff: String::new(), profiles: vec!["a".into(), "b".into()] };
    assert!(matches!(d.decide(&input).await, Err(SscError::Invalid(_))));
    assert!(srv.requests()[0].header("authorization").is_none());
}

/// Verbatim error from the live API (2026-11-17): the message must survive into our error.
#[tokio::test]
async fn systemone_error_detail_is_reported() {
    let srv = serve(vec![Reply::status(
        400,
        r#"{"detail":{"error_type":"api_usage_error","message":"Unknown model: jev-1.14"}}"#,
    )])
    .await;
    let d = SystemOneDecision::new(&srv.url(), "jev-1.14", Some(Secret::new("ts-key")));
    let input =
        DecisionInput { question: "q".into(), screen_diff: String::new(), profiles: vec!["a".into(), "b".into()] };
    let e = d.decide(&input).await.unwrap_err();
    let s = e.to_string();
    assert!(matches!(e, SscError::Invalid(_)), "{e:?}");
    assert!(s.contains("api_usage_error") && s.contains("Unknown model: jev-1.14"), "{s}");
    assert!(!s.contains("ts-key"));
}

fn sse_frame(text: &str) -> Vec<u8> {
    format!("data: {}\n\n", serde_json::json!({"choices": [{"delta": {"content": text}}]})).into_bytes()
}

/// M3: a provider that stalls after its first tokens ends the answer cleanly (no hang, no error):
/// the cues that arrived are kept and the router reports success.
#[tokio::test]
async fn mid_stream_stall_ends_the_answer_cleanly() {
    let (a, b, c) = (sse_frame("1. Requirements\n"), sse_frame("2. Design\n"), sse_frame("3. never\n"));
    let parts: Vec<&[u8]> = vec![&a, &b, &c];
    let reply =
        Reply::chunked("text/event-stream", &parts, Duration::ZERO, Duration::ZERO).delay_last(Duration::from_secs(5));
    let srv = serve(vec![reply]).await;
    let m = OpenAiCompat::groq(&srv.url(), "m", Secret::new("k")).with_idle_timeout(Duration::from_millis(300));
    let router =
        AnswerRouter::new(Mode::Cloud, None, vec![Route::new(Arc::new(m)).first_token_ms(1000)], None).unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    let (wtx, wrx) = tokio::sync::watch::channel(None);
    let t = Instant::now();
    let out = router
        .answer_reporting(Tier::Fast, &prompt(), |_: &[String]| prompt(), tx, &Cancel::new(), Some(&wtx))
        .await
        .expect("a stall after the first token is not an error");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    let texts: Vec<String> = out.parsed.cues.iter().map(|c| c.text.clone()).collect();
    assert_eq!(texts, ["Requirements", "Design"]);
    let mut got = String::new();
    while let Ok(d) = rx.try_recv() {
        got.push_str(&d.text);
    }
    assert!(got.contains("Design") && !got.contains("never"));
    assert_eq!(wrx.borrow().as_deref(), Some("groq/m"));
}

/// M3: a stall before any text was forwarded fails over to the next route, and `answering`
/// names the model that actually answered (L3).
#[tokio::test]
async fn stall_before_first_text_fails_over_and_reports_the_answering_model() {
    // Headers + an SSE comment, then nothing: no text token. The first-token deadline is long,
    // so the idle timeout is what trips.
    let parts: Vec<&[u8]> = vec![b": ping\n\n", b"data: [DONE]\n\n"];
    let stalled =
        serve(vec![Reply::chunked("text/event-stream", &parts, Duration::ZERO, Duration::from_secs(5))]).await;
    let good = serve(vec![Reply::sse(GEMINI)]).await;
    let first = OpenAiCompat::groq(&stalled.url(), "m", Secret::new("k")).with_idle_timeout(Duration::from_millis(300));
    let second = OpenAiCompat::gemini(&good.url(), "g", Secret::new("k"), GeminiTier::Paid, false).unwrap();
    let router = AnswerRouter::new(
        Mode::Cloud,
        None,
        vec![Route::new(Arc::new(first)).first_token_ms(10_000), Route::new(Arc::new(second))],
        None,
    )
    .unwrap();
    let (tx, _rx) = mpsc::channel(256);
    let (wtx, wrx) = tokio::sync::watch::channel(None);
    let t = Instant::now();
    let out = router
        .answer_reporting(Tier::Fast, &prompt(), |_: &[String]| prompt(), tx, &Cancel::new(), Some(&wtx))
        .await
        .unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(out.fast.model, "gemini/g");
    assert_eq!(wrx.borrow().as_deref(), Some("gemini/g"));
}

/// H2 budget rule: speculation stops well before the Groq budget is tight.
#[tokio::test]
async fn speculation_respects_the_budget() {
    let clock = Arc::new(MockClock::new(0));
    let budget = Arc::new(BudgetTracker::new(Default::default(), clock));
    let groq = Arc::new(OpenAiCompat::groq("http://127.0.0.1:9", "m", Secret::new("k")));
    let router = AnswerRouter::new(Mode::Cloud, None, vec![Route::new(groq).budget(budget.clone())], None).unwrap();
    assert!(router.speculation_allowed());
    while budget.below(ssc_router::answer::SPECULATION_BUDGET_FRACTION) {
        assert!(budget.try_reserve(1));
    }
    assert!(!router.speculation_allowed());
    assert!(budget.try_reserve(1), "the confirmed answer still has budget left");
}

/// M2: the factory's warmer opens the Groq and Gemini connections with `GET /models` (no
/// completion, nothing reserved from the budget).
#[tokio::test]
async fn warmer_opens_cloud_connections() {
    use ssc_core::secrets::{MemorySecretStore, SecretKey, SecretStore};
    let groq = serve(vec![Reply::ok("application/json", b"{}")]).await;
    let gem = serve(vec![Reply::ok("application/json", b"{}")]).await;
    let s = MemorySecretStore::default();
    s.set(&SecretKey::Groq, &Secret::new("gk")).unwrap();
    s.set(&SecretKey::Gemini, &Secret::new("mk")).unwrap();
    let opts =
        ssc_router::RouterOptions { groq_base_url: groq.url(), gemini_base_url: gem.url(), ..Default::default() };
    let m = ssc_router::build(&ssc_core::config::Config::default(), &s, Arc::new(MockClock::new(0)), &opts).unwrap();
    m.warm.warm(None).await;
    for (srv, key) in [(&groq, "Bearer gk"), (&gem, "Bearer mk")] {
        let r = srv.requests();
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].method.as_str(), r[0].path.as_str()), ("GET", "/models"));
        assert_eq!(r[0].header("authorization"), Some(key));
    }
    assert_eq!(m.groq_budget.unwrap().usage().requests_min, 0, "warm-up spends no budget");
}
