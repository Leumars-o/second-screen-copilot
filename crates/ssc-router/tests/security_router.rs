//! W2-HARDEN security suite for model routing (Build Sheet §7; `docs/security/threat-model.md`
//! §5–6): local-only mode never reaches a cloud client, redaction runs before every cloud call
//! (answers, strong tier, failover, decisions, on the wire), and provider keys never end up in
//! error strings. Local fake HTTP servers only.

mod common;

use common::{Reply, serve};
use ssc_core::config::Config;
use ssc_core::decision::{DecisionInput, Tier};
use ssc_core::mock::{MockAnswer, MockClock, MockDecision, MockOutcome, decision_answer};
use ssc_core::protocol::{Intent, Mode};
use ssc_core::secrets::{MemorySecretStore, Secret, SecretKey, SecretStore};
use ssc_core::traits::{AnswerModel, Cancel, DecisionModel};
use ssc_core::types::Prompt;
use ssc_core::{Result, SscError};
use ssc_router::decision::{DecisionRouterConfig, DecisionSlot};
use ssc_router::providers::{Anthropic, Ollama, OpenAiCompat, is_loopback_url};
use ssc_router::{AnswerRouter, DecisionRouter, Route, RouterOptions, SystemOneDecision, build};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Sensitive values planted in prompts. None may reach a cloud model while redaction is on.
const SENSITIVE: &[&str] = &["4111 1111 1111 1111", "4111111111111111", "22212345678", "hunter2", "sk-live-PLANTED"];

fn sensitive_text() -> String {
    "Card 4111 1111 1111 1111, also 4111111111111111. My BVN is 22212345678. password: hunter2 api_key=sk-live-PLANTED"
        .into()
}

fn prompt() -> Prompt {
    Prompt {
        prefix: format!("About me: {}", sensitive_text()),
        dynamic: sensitive_text(),
        max_tokens: 60,
        temperature: 0.2,
        stop: vec![],
        images: vec![],
    }
}

fn assert_clean(what: &str, text: &str) {
    for s in SENSITIVE {
        assert!(!text.contains(s), "{what} reached a cloud model with {s:?}: {text}");
    }
}

fn opts() -> ssc_redact::Options {
    ssc_redact::Options { emails: true }
}

/// Fails the test on any keychain read.
struct NoSecrets;
impl SecretStore for NoSecrets {
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>> {
        panic!("local-only mode read {key:?}")
    }
    fn set(&self, key: &SecretKey, _: &Secret) -> Result<()> {
        panic!("local-only mode wrote {key:?}")
    }
    fn delete(&self, key: &SecretKey) -> Result<()> {
        panic!("local-only mode deleted {key:?}")
    }
}

// ------------------------------------------------------------------ local-only

/// Local-only with every cloud option configured: the factory reads no key, builds no cloud
/// answer, strong, post-call or decision client.
#[test]
fn local_only_factory_builds_nothing_cloud_and_reads_no_key() {
    let mut cfg = Config { mode: Mode::Local, ..Config::default() };
    cfg.models.strong_model = Some("anthropic/claude-opus-5-5".into());
    cfg.models.strong_base_url = Some("https://strong.example.com".into());
    cfg.models.decision_cloud_base_url = "https://api.typesafe.ai".into();
    let m = build(&cfg, &NoSecrets, Arc::new(MockClock::new(0)), &RouterOptions::default()).unwrap();
    assert!(m.answers.models().iter().all(|x| !x.is_cloud()));
    assert!(!m.answers.has_strong());
    assert!(!m.postcall.is_cloud());
    assert!(!m.decisions.has_cloud());
    assert_eq!(m.answers.mode(), Mode::Local);
    assert_eq!(m.decisions.mode(), Mode::Local);
}

/// Local-only refuses any Ollama host that is not this machine, including look-alike host names.
#[test]
fn local_only_refuses_non_loopback_ollama_and_lookalikes() {
    for url in [
        "http://192.168.1.50:11434",
        "http://127.0.0.1.evil.example:11434",
        "http://localhost.evil.example:11434",
        "http://evil.example/127.0.0.1",
        "http://[::ffff:c0a8:0101]:11434",
        "http://0.0.0.0:11434",
        "http://ollama:11434",
    ] {
        assert!(!is_loopback_url(url), "{url} classified as loopback");
        let mut cfg = Config { mode: Mode::Local, ..Config::default() };
        cfg.models.ollama_url = url.into();
        let r = build(&cfg, &NoSecrets, Arc::new(MockClock::new(0)), &RouterOptions::default());
        assert!(matches!(r, Err(SscError::Policy(_))), "{url}: {:?}", r.as_ref().err());
        assert!(Ollama::new(url, "m").is_cloud(), "{url} must count as cloud");
    }
    for url in ["http://127.0.0.1:11434", "http://localhost:11434", "http://[::1]:11434", "http://127.8.9.10:1"] {
        assert!(is_loopback_url(url), "{url}");
    }
}

/// The routers themselves refuse cloud models in local-only mode, whatever the caller hands them.
#[test]
fn local_only_routers_refuse_cloud_models() {
    let cloud: Arc<dyn AnswerModel> = Arc::new(MockAnswer::cue_answer("cloud/x", true));
    let local: Arc<dyn AnswerModel> = Arc::new(MockAnswer::cue_answer("ollama/x", false));
    let r = AnswerRouter::new(Mode::Local, None, vec![Route::new(local.clone()), Route::new(cloud.clone())], None);
    assert!(matches!(r, Err(SscError::Policy(_))));
    let r = AnswerRouter::new(Mode::Local, None, vec![Route::new(local)], Some(Route::new(cloud)));
    assert!(matches!(r, Err(SscError::Policy(_))));

    let d = |cloud: bool| DecisionSlot {
        model: Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "p"))) as Arc<dyn DecisionModel>,
        is_cloud: cloud,
    };
    let cfg = DecisionRouterConfig::default;
    assert!(matches!(DecisionRouter::new(Mode::Local, Some(d(true)), d(false), cfg(), None), Err(SscError::Policy(_))));
    assert!(matches!(DecisionRouter::new(Mode::Local, None, d(true), cfg(), None), Err(SscError::Policy(_))));
}

// ------------------------------------------------------------------ redaction

/// Every cloud call of one answer is redacted: the failed first route, the failover route and
/// the strong model's detail prompt.
#[tokio::test(start_paused = true)]
async fn redaction_runs_before_every_cloud_answer_call() {
    let down = Arc::new(MockAnswer::cue_answer("cloud/down", true).failing(MockOutcome::Unavailable));
    let fast = Arc::new(MockAnswer::cue_answer("cloud/fast", true));
    let strong = Arc::new(MockAnswer::new("cloud/strong", true, &["1: detail\n"]));
    let router = AnswerRouter::new(
        Mode::Cloud,
        Some(opts()),
        vec![Route::new(down.clone()), Route::new(fast.clone())],
        Some(Route::new(strong.clone())),
    )
    .unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    let detail = |_: &[String]| prompt();
    let out = router.answer(Tier::Strong, &prompt(), detail, tx, &Cancel::new()).await.unwrap();
    while rx.try_recv().is_ok() {}
    assert!(out.strong.is_some(), "strong tier ran");
    for m in [&down, &fast, &strong] {
        assert_eq!(m.calls(), 1, "{} called", m.id);
        for p in m.prompts() {
            assert_clean(&m.id, &p.prefix);
            assert_clean(&m.id, &p.dynamic);
        }
    }
}

/// Control: with redaction off, the same values do reach the model (so the test above really
/// checks redaction, not a mock that drops text).
#[tokio::test(start_paused = true)]
async fn redaction_off_passes_text_through() {
    let fast = Arc::new(MockAnswer::cue_answer("cloud/fast", true));
    let router = AnswerRouter::new(Mode::Cloud, None, vec![Route::new(fast.clone())], None).unwrap();
    let (tx, _rx) = mpsc::channel(256);
    router.answer(Tier::Fast, &prompt(), |_: &[String]| prompt(), tx, &Cancel::new()).await.unwrap();
    assert!(fast.prompts()[0].dynamic.contains("22212345678"));
}

/// Cloud decisions (Jev) get redacted input.
#[tokio::test]
async fn redaction_runs_before_cloud_decisions() {
    let jev = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let tev = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let cfg = DecisionRouterConfig { redaction: Some(opts()), ..Default::default() };
    let r = DecisionRouter::new(
        Mode::Cloud,
        Some(DecisionSlot { model: jev.clone(), is_cloud: true }),
        DecisionSlot { model: tev.clone(), is_cloud: false },
        cfg,
        None,
    )
    .unwrap();
    let input =
        DecisionInput { question: sensitive_text(), screen_diff: sensitive_text(), profiles: vec!["sales".into()] };
    r.decide(&input).await.unwrap();
    let seen = jev.inputs();
    assert_eq!(seen.len(), 1);
    assert_clean("jev question", &seen[0].question);
    assert_clean("jev screen", &seen[0].screen_diff);
}

const SSE_OK: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"1. Ok\\n\"}}]}\n\ndata: [DONE]\n\n";

/// On the wire: the real factory in cloud mode (Groq adapter) and a cloud decision router over
/// the real Jev adapter, both against local fakes, send request bodies without the planted
/// values; the key travels only in the `Authorization` header. (The factory classifies a
/// loopback `decision_cloud_base_url` as local, so the Jev slot is built by hand here with
/// `is_cloud: true`, as the factory does for the real `https://api.typesafe.ai`.)
#[tokio::test]
async fn redaction_holds_on_the_wire_for_real_adapters() {
    let groq = serve(vec![Reply::sse(SSE_OK)]).await;
    let jev = serve(vec![Reply::ok("application/json", include_bytes!("fixtures/systemone_response.json"))]).await;
    let secrets = MemorySecretStore::default();
    secrets.set(&SecretKey::Groq, &Secret::new("gsk-PLANTED-KEY")).unwrap();
    secrets.set(&SecretKey::TypeSafe, &Secret::new("ts-PLANTED-KEY")).unwrap();
    let mut cfg = Config { redaction: true, redact_emails: true, ..Config::default() };
    cfg.models.strong_model = None;
    let ro = RouterOptions { groq_base_url: groq.url(), ..RouterOptions::default() };
    let m = build(&cfg, &secrets, Arc::new(MockClock::new(0)), &ro).unwrap();

    let (tx, _rx) = mpsc::channel(256);
    m.answers.answer(Tier::Fast, &prompt(), |_: &[String]| prompt(), tx, &Cancel::new()).await.unwrap();
    let jev_slot = DecisionSlot {
        model: Arc::new(SystemOneDecision::new(&jev.url(), "jev-1.14.0", Some(Secret::new("ts-PLANTED-KEY")))),
        is_cloud: true,
    };
    let local = DecisionSlot {
        model: Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales"))),
        is_cloud: false,
    };
    let dcfg = DecisionRouterConfig { redaction: Some(opts()), ..Default::default() };
    let decisions = DecisionRouter::new(Mode::Cloud, Some(jev_slot), local, dcfg, None).unwrap();
    let input = DecisionInput {
        question: sensitive_text(),
        screen_diff: sensitive_text(),
        profiles: vec!["sales".into(), "support".into()],
    };
    let d = decisions.decide(&input).await.unwrap();
    assert_eq!(d.source, ssc_router::DecisionSource::Cloud);
    assert!(m.answers.models()[0].is_cloud(), "groq adapter counts as cloud");

    let reqs: Vec<_> = groq.requests().into_iter().chain(jev.requests()).filter(|r| r.method == "POST").collect();
    assert_eq!(reqs.len(), 2, "one answer call and one decision call");
    for r in &reqs {
        let body = String::from_utf8_lossy(&r.body);
        assert_clean(&format!("POST {}", r.path), &body);
        for k in ["gsk-PLANTED-KEY", "ts-PLANTED-KEY"] {
            assert!(!body.contains(k) && !r.path.contains(k), "key in request body/path");
        }
    }
}

// ------------------------------------------------------------------ keys in errors

const KEY: &str = "sk-PLANTED-0123456789abcdef";

fn echo_body() -> String {
    format!(
        r#"{{"error":{{"message":"Incorrect API key provided: {KEY}","type":"invalid_request_error","code":"invalid_api_key"}},"detail":{{"error_type":"auth","message":"Invalid key {KEY}"}}}}"#
    )
}

async fn answer_err(model: Arc<dyn AnswerModel>) -> String {
    let (tx, _rx) = mpsc::channel(16);
    model.stream(&prompt(), tx, Cancel::new()).await.unwrap_err().to_string()
}

/// Answer adapters never copy a provider's error message (which may echo the key or the prompt)
/// into their errors; those strings reach logs and the settings UI.
#[tokio::test]
async fn answer_adapter_errors_never_contain_the_key() {
    for status in [401u16, 400, 403, 500] {
        let srv = serve(vec![Reply::status(status, &echo_body())]).await;
        let groq: Arc<dyn AnswerModel> = Arc::new(OpenAiCompat::groq(&srv.url(), "m", Secret::new(KEY)));
        let claude: Arc<dyn AnswerModel> = Arc::new(Anthropic::new(&srv.url(), "m", Secret::new(KEY)));
        for (name, m) in [("groq", groq), ("anthropic", claude)] {
            let e = answer_err(m).await;
            assert!(!e.contains(KEY), "{name} HTTP {status}: key in error: {e}");
            assert_clean(name, &e);
        }
    }
}

/// CR-10: the systemone (Jev) adapter appends the provider's `detail.message` to its error; a
/// provider that echoes the key (or the redaction-free question) puts it in logs and the UI.
#[tokio::test]
async fn decision_adapter_errors_never_contain_the_key() {
    for status in [401u16, 400, 403, 500] {
        let srv = serve(vec![Reply::status(status, &echo_body())]).await;
        let d = SystemOneDecision::new(&srv.url(), "jev-1.14.0", Some(Secret::new(KEY)));
        let input =
            DecisionInput { question: "q".into(), screen_diff: String::new(), profiles: vec!["a".into(), "b".into()] };
        let e = d.decide(&input).await.unwrap_err().to_string();
        assert!(!e.contains(KEY), "systemone HTTP {status}: key in error: {e}");
    }
}
