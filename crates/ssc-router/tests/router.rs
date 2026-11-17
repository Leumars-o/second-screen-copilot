//! Answer-router rules with mocks and paused tokio time (deterministic fake clock).

use async_trait::async_trait;
use ssc_core::decision::Tier;
use ssc_core::mock::{MockAnswer, MockClock, MockOutcome};
use ssc_core::protocol::{Block, Mode};
use ssc_core::traits::{AnswerModel, Cancel};
use ssc_core::types::{Prompt, StreamStats, Token};
use ssc_core::{Result, SscError};
use ssc_router::budget::BudgetTracker;
use ssc_router::speculative::{DEFAULT_THRESHOLD, Finalized, Speculation};
use ssc_router::{AnswerRouter, Route, RoutedDelta};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

fn prompt() -> Prompt {
    Prompt {
        prefix: "fixed prefix pwd=hunter2".into(),
        dynamic: "card 4111111111111111 BVN 22212345678".into(),
        max_tokens: 60,
        temperature: 0.2,
        stop: vec![],
    }
}

async fn run(router: &AnswerRouter) -> (Result<StreamStats>, String) {
    let (tx, mut rx) = mpsc::channel(256);
    let r = router.stream(&prompt(), tx, &Cancel::new()).await;
    let mut s = String::new();
    while let Ok(t) = rx.try_recv() {
        s.push_str(&t.text);
    }
    (r, s)
}

/// A model that fails with a fixed error, or fails after streaming one token.
struct Failing {
    id: &'static str,
    err: fn() -> SscError,
    after_first_token: bool,
    calls: AtomicUsize,
}

impl Failing {
    fn new(id: &'static str, err: fn() -> SscError, after_first_token: bool) -> Self {
        Self { id, err, after_first_token, calls: AtomicUsize::new(0) }
    }
}

#[async_trait]
impl AnswerModel for Failing {
    fn id(&self) -> &str {
        self.id
    }
    fn is_cloud(&self) -> bool {
        true
    }
    async fn stream(&self, _: &Prompt, tx: mpsc::Sender<Token>, _: Cancel) -> Result<StreamStats> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.after_first_token {
            let _ = tx.send(Token { text: "1. partial\n".into() }).await;
        }
        Err((self.err)())
    }
}

#[tokio::test(start_paused = true)]
async fn groq_first_then_nothing_else() {
    let groq = Arc::new(MockAnswer::cue_answer("groq", true));
    let gem = Arc::new(MockAnswer::cue_answer("gemini", true));
    let r = AnswerRouter::new(
        Mode::Cloud,
        None,
        vec![Route::new(groq.clone()).first_token_ms(1000), Route::new(gem.clone())],
        None,
    )
    .unwrap();
    let (res, text) = run(&r).await;
    assert_eq!(res.unwrap().model, "groq");
    assert!(text.starts_with("1. Requirements"));
    assert_eq!(gem.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn groq_429_goes_to_gemini_and_cools_down() {
    let clock = MockClock::new(0);
    let budget = Arc::new(BudgetTracker::new(Default::default(), Arc::new(clock.clone())));
    let groq = Arc::new(MockAnswer::cue_answer("groq", true).failing(MockOutcome::RateLimited));
    let gem = Arc::new(MockAnswer::cue_answer("gemini", true));
    let r = AnswerRouter::new(
        Mode::Cloud,
        None,
        vec![Route::new(groq.clone()).first_token_ms(1000).budget(budget), Route::new(gem.clone())],
        None,
    )
    .unwrap();
    assert_eq!(run(&r).await.0.unwrap().model, "gemini");
    assert_eq!(run(&r).await.0.unwrap().model, "gemini");
    assert_eq!(groq.calls(), 1, "second answer skips Groq during the cool-down");
    clock.advance(60_000);
    let _ = run(&r).await;
    assert_eq!(groq.calls(), 2, "cool-down over");
}

#[tokio::test(start_paused = true)]
async fn no_first_token_in_1000ms_goes_to_gemini() {
    let groq = Arc::new(MockAnswer::cue_answer("groq", true).with_first_token_delay(Duration::from_millis(1500)));
    let gem = Arc::new(MockAnswer::cue_answer("gemini", true).with_first_token_delay(Duration::from_millis(300)));
    let r = AnswerRouter::new(
        Mode::Cloud,
        None,
        vec![Route::new(groq.clone()).first_token_ms(1000), Route::new(gem.clone())],
        None,
    )
    .unwrap();
    let t = Instant::now();
    let stats = run(&r).await.0.unwrap();
    assert_eq!(stats.model, "gemini");
    // 1000 ms deadline + Gemini's 300 ms first token + 4 tokens × 10 ms.
    assert_eq!(t.elapsed(), Duration::from_millis(1340));

    // 999 ms is still in time.
    let groq2 = Arc::new(MockAnswer::cue_answer("groq", true).with_first_token_delay(Duration::from_millis(999)));
    let r2 = AnswerRouter::new(Mode::Cloud, None, vec![Route::new(groq2).first_token_ms(1000), Route::new(gem)], None)
        .unwrap();
    assert_eq!(run(&r2).await.0.unwrap().model, "groq");
}

#[tokio::test(start_paused = true)]
async fn budget_switches_before_the_limit() {
    let clock = MockClock::new(0);
    let limits =
        ssc_core::config::GroqBudget { requests_per_min: 3, requests_per_day: 0, tokens_per_min: 0, tokens_per_day: 0 };
    let budget = Arc::new(BudgetTracker::new(limits, Arc::new(clock.clone())));
    let groq = Arc::new(MockAnswer::cue_answer("groq", true));
    let gem = Arc::new(MockAnswer::cue_answer("gemini", true));
    let r = AnswerRouter::new(
        Mode::Cloud,
        None,
        vec![Route::new(groq.clone()).budget(budget), Route::new(gem.clone())],
        None,
    )
    .unwrap();
    // 90% of 3 rpm = 2 requests; the third goes to Gemini without touching Groq.
    let models: Vec<String> = {
        let mut v = vec![];
        for _ in 0..3 {
            v.push(run(&r).await.0.unwrap().model);
        }
        v
    };
    assert_eq!(models, vec!["groq", "groq", "gemini"]);
    assert_eq!(groq.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn policy_never_fails_over() {
    let groq = Arc::new(Failing::new("groq", || SscError::Policy("blocked".into()), false));
    let gem = Arc::new(MockAnswer::cue_answer("gemini", true));
    let r = AnswerRouter::new(Mode::Cloud, None, vec![Route::new(groq), Route::new(gem.clone())], None).unwrap();
    assert!(matches!(run(&r).await.0, Err(SscError::Policy(_))));
    assert_eq!(gem.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn which_errors_fail_over() {
    let cases: Vec<(fn() -> SscError, bool)> = vec![
        (|| SscError::Unavailable("down".into()), true),
        (|| SscError::Timeout(5), true),
        (|| SscError::RateLimited { retry_after_ms: None }, true),
        (|| SscError::Invalid("bad request".into()), false),
        (|| SscError::Cancelled, false),
    ];
    for (err, fails_over) in cases {
        let gem = Arc::new(MockAnswer::cue_answer("gemini", true));
        let r = AnswerRouter::new(
            Mode::Cloud,
            None,
            vec![Route::new(Arc::new(Failing::new("groq", err, false))), Route::new(gem.clone())],
            None,
        )
        .unwrap();
        let res = run(&r).await.0;
        assert_eq!(res.is_ok(), fails_over, "{:?}", err());
        assert_eq!(gem.calls(), usize::from(fails_over));
    }
}

#[tokio::test(start_paused = true)]
async fn error_after_first_token_does_not_fail_over() {
    let gem = Arc::new(MockAnswer::cue_answer("gemini", true));
    let r = AnswerRouter::new(
        Mode::Cloud,
        None,
        vec![
            Route::new(Arc::new(Failing::new("groq", || SscError::Unavailable("x".into()), true))),
            Route::new(gem.clone()),
        ],
        None,
    )
    .unwrap();
    let (res, text) = run(&r).await;
    assert!(matches!(res, Err(SscError::Unavailable(_))));
    assert_eq!(text, "1. partial\n");
    assert_eq!(gem.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn all_routes_fail_returns_last_error() {
    let r = AnswerRouter::new(
        Mode::Cloud,
        None,
        vec![Route::new(Arc::new(Failing::new("a", || SscError::Unavailable("a".into()), false)))],
        None,
    )
    .unwrap();
    assert!(matches!(run(&r).await.0, Err(SscError::Unavailable(_))));
}

#[test]
fn local_only_refuses_cloud_models() {
    let local = Arc::new(MockAnswer::cue_answer("ollama", false));
    let cloud = Arc::new(MockAnswer::cue_answer("groq", true));
    assert!(matches!(
        AnswerRouter::new(Mode::Local, None, vec![Route::new(local.clone()), Route::new(cloud.clone())], None),
        Err(SscError::Policy(_))
    ));
    assert!(matches!(
        AnswerRouter::new(Mode::Local, None, vec![Route::new(local.clone())], Some(Route::new(cloud))),
        Err(SscError::Policy(_))
    ));
    assert!(AnswerRouter::new(Mode::Local, None, vec![Route::new(local)], None).is_ok());
}

#[tokio::test(start_paused = true)]
async fn redaction_before_cloud_calls_only() {
    let cloud = Arc::new(MockAnswer::cue_answer("groq", true).failing(MockOutcome::Unavailable));
    let local = Arc::new(MockAnswer::cue_answer("ollama", false));
    let r = AnswerRouter::new(
        Mode::Cloud,
        Some(Default::default()),
        vec![Route::new(cloud.clone()), Route::new(local.clone())],
        None,
    )
    .unwrap();
    run(&r).await.0.unwrap();
    let sent = &cloud.prompts()[0];
    assert_eq!(sent.prefix, "fixed prefix pwd=[REDACTED]");
    assert_eq!(sent.dynamic, "card [CARD] BVN [ID]");
    assert_eq!(local.prompts()[0], prompt(), "local model sees the raw prompt");

    // Redaction off: raw text goes out.
    let cloud2 = Arc::new(MockAnswer::cue_answer("groq", true));
    let r2 = AnswerRouter::new(Mode::Cloud, None, vec![Route::new(cloud2.clone())], None).unwrap();
    run(&r2).await.0.unwrap();
    assert_eq!(cloud2.prompts()[0], prompt());
}

async fn answer(
    r: &AnswerRouter,
    tier: Tier,
    seen_cues: Arc<std::sync::Mutex<Vec<String>>>,
) -> (Result<ssc_router::AnswerOutcome>, Vec<RoutedDelta>) {
    let (tx, mut rx) = mpsc::channel(256);
    let res = r
        .answer(
            tier,
            &prompt(),
            move |cues: &[String]| {
                *seen_cues.lock().unwrap() = cues.to_vec();
                Prompt {
                    prefix: "expand".into(),
                    dynamic: cues.join("\n"),
                    max_tokens: 200,
                    temperature: 0.2,
                    stop: vec![],
                }
            },
            tx,
            &Cancel::new(),
        )
        .await;
    let mut deltas: Vec<RoutedDelta> = Vec::new();
    while let Ok(d) = rx.try_recv() {
        match deltas.last_mut() {
            Some(last) if last.block == d.block => last.text.push_str(&d.text),
            _ => deltas.push(d),
        }
    }
    (res, deltas)
}

#[tokio::test(start_paused = true)]
async fn strong_tier_fast_cues_then_strong_detail() {
    let fast = Arc::new(MockAnswer::cue_answer("groq", true));
    let strong = Arc::new(MockAnswer::new("strong", true, &["---\n", "1: strong one\n", "3: strong three\n"]));
    let r =
        AnswerRouter::new(Mode::Cloud, None, vec![Route::new(fast.clone())], Some(Route::new(strong.clone()))).unwrap();
    let cues = Arc::new(std::sync::Mutex::new(vec![]));
    let (res, deltas) = answer(&r, Tier::Strong, cues.clone()).await;
    let out = res.unwrap();
    assert_eq!(
        deltas,
        vec![
            RoutedDelta { block: Block::Cue, text: "1. Requirements\n2. Design\n3. Build\n".into() },
            RoutedDelta { block: Block::Detail, text: "1: strong one\n3: strong three\n".into() },
        ],
        "fast detail is not forwarded; strong writes the detail block"
    );
    assert_eq!(*cues.lock().unwrap(), vec!["Requirements", "Design", "Build"]);
    assert_eq!(strong.prompts()[0].dynamic, "Requirements\nDesign\nBuild");
    assert_eq!(out.parsed.cues[0].expand.as_deref(), Some("strong one"));
    assert_eq!(out.parsed.cues[2].expand.as_deref(), Some("strong three"));
    assert_eq!(out.fast.tokens_out, 4, "fast stream stopped at the separator");
    assert_eq!(out.strong.unwrap().model, "strong");
}

#[tokio::test(start_paused = true)]
async fn fast_tier_ignores_strong_model() {
    let fast = Arc::new(MockAnswer::cue_answer("groq", true));
    let strong = Arc::new(MockAnswer::new("strong", true, &["---\n", "1: x\n"]));
    let r = AnswerRouter::new(Mode::Cloud, None, vec![Route::new(fast)], Some(Route::new(strong.clone()))).unwrap();
    let (res, deltas) = answer(&r, Tier::Fast, Default::default()).await;
    let out = res.unwrap();
    assert_eq!(strong.calls(), 0);
    assert_eq!(deltas[1], RoutedDelta { block: Block::Detail, text: "1: Interview users first\n".into() });
    assert_eq!(out.parsed.cues[0].expand.as_deref(), Some("Interview users first"));
    assert!(out.strong.is_none());
}

#[tokio::test(start_paused = true)]
async fn strong_unavailable_fast_chain_writes_detail() {
    let fast = Arc::new(MockAnswer::cue_answer("groq", true));
    let strong = Arc::new(MockAnswer::new("strong", true, &["x"]).failing(MockOutcome::Unavailable));
    let r = AnswerRouter::new(Mode::Cloud, None, vec![Route::new(fast.clone())], Some(Route::new(strong))).unwrap();
    let (res, _) = answer(&r, Tier::Strong, Default::default()).await;
    let out = res.unwrap();
    assert_eq!(fast.calls(), 2, "cues, then the detail prompt");
    assert_eq!(out.strong.unwrap().model, "groq");
}

#[tokio::test(start_paused = true)]
async fn cancellation_returns_immediately() {
    let slow = Arc::new(MockAnswer::cue_answer("groq", true).with_first_token_delay(Duration::from_secs(5)));
    let r = Arc::new(AnswerRouter::new(Mode::Cloud, None, vec![Route::new(slow)], None).unwrap());
    let cancel = Cancel::new();
    let (r2, c2) = (r.clone(), cancel.clone());
    let task = tokio::spawn(async move {
        let (tx, _rx) = mpsc::channel(8);
        r2.stream(&prompt(), tx, &c2).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t = Instant::now();
    cancel.cancel();
    let res = task.await.unwrap();
    assert!(matches!(res, Err(SscError::Cancelled)));
    assert!(t.elapsed() < Duration::from_millis(50));
}

// ---------- speculative drafting ----------

#[tokio::test(start_paused = true)]
async fn speculative_keep_or_restart() {
    let calls = Arc::new(AtomicUsize::new(0));
    let model = Arc::new(MockAnswer::cue_answer("groq", true));
    let launch = {
        let (calls, model) = (calls.clone(), model.clone());
        move |text: String, cancel: Cancel| {
            calls.fetch_add(1, Ordering::SeqCst);
            let model = model.clone();
            async move {
                let (tx, _rx) = mpsc::channel(64);
                let p = Prompt { prefix: String::new(), dynamic: text, max_tokens: 60, temperature: 0.2, stop: vec![] };
                model.stream(&p, tx, cancel).await.map(|_| p.dynamic)
            }
        }
    };

    // Small transcript fix-up: the draft is kept.
    let s = Speculation::start("how much does the pro plan cost", &launch);
    let (s, f) = s.finalize("How much does the Pro plan cost?", DEFAULT_THRESHOLD, &launch);
    assert_eq!(f, Finalized::Kept);
    assert_eq!(s.join().await.unwrap().unwrap(), "how much does the pro plan cost");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Meaning changed: the draft is cancelled and restarted on the final text.
    let s = Speculation::start("what does it cost", &launch);
    let first_cancel_seen = s.basis().to_string();
    let (s, f) = s.finalize("when can we schedule the onboarding call", DEFAULT_THRESHOLD, &launch);
    assert_eq!(f, Finalized::Restarted);
    assert_eq!(first_cancel_seen, "what does it cost");
    assert_eq!(s.basis(), "when can we schedule the onboarding call");
    assert_eq!(s.join().await.unwrap().unwrap(), "when can we schedule the onboarding call");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn dropping_a_speculation_cancels_it() {
    let seen = Arc::new(std::sync::Mutex::new(None::<Cancel>));
    let launch = {
        let seen = seen.clone();
        move |_: String, cancel: Cancel| {
            *seen.lock().unwrap() = Some(cancel.clone());
            async move { cancel.cancelled().await }
        }
    };
    let s = Speculation::start("x", &launch);
    drop(s);
    assert!(seen.lock().unwrap().as_ref().unwrap().is_cancelled());
}
