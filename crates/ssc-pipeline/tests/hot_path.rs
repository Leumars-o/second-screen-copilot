//! W3-FIX-PIPE: the parallel hot path (H2) and the answer-path fixes around it, end to end with
//! mocks on paused tokio time (so the millisecond numbers are exact and reproducible).
//!
//! Timeline of the fixture question: speech ends at +1300 ms (the final transcript arrives at
//! +1500 ms), and the 300 ms silence window after the end of speech fires the trigger at +1600 ms. The mock decision takes
//! 560 ms (Jev as measured live) and the mock fast model's first token 400 ms.

mod common;

use common::*;
use ssc_core::decision::Decision;
use ssc_core::mock::{MockAnswer, MockDecision, MockOutcome, MockTransport, decision_answer, decision_no_answer};
use ssc_core::protocol::*;
use ssc_core::timing::{Stage, Timing};
use ssc_core::traits::{AnswerModel, AudioSource, Clock};
use ssc_core::types::Profile;
use ssc_pipeline::pipeline::{Extras, FrameSentEvent, TagGate};
use ssc_pipeline::{Parts, Pipeline, Settings, TokioClock};
use ssc_router::budget::BudgetTracker;
use ssc_router::{AnswerRouter, Route};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const DECISION_MS: u64 = 560;
const FIRST_TOKEN_MS: u64 = 400;

struct Rig {
    pipeline: Pipeline,
    phone: PhoneMock,
    timing: Arc<Timing>,
    answer: Arc<MockAnswer>,
    decision: Arc<MockDecision>,
    memory: Arc<ssc_memory::SqliteMemory>,
}

struct Opts {
    decision: MockDecision,
    gate: bool,
    speculate: bool,
    routes: Vec<Route>,
    extras: Extras,
}

fn opts(decision: MockDecision) -> Opts {
    Opts { decision, gate: true, speculate: true, routes: vec![], extras: Extras::default() }
}

fn fast_model() -> Arc<MockAnswer> {
    Arc::new(
        MockAnswer::new("groq/mock-fast", true, ANSWER_TOKENS)
            .with_first_token_delay(Duration::from_millis(FIRST_TOKEN_MS)),
    )
}

fn rig(o: Opts) -> Rig {
    let decision = Arc::new(o.decision.with_delay(Duration::from_millis(DECISION_MS)));
    let clock = Arc::new(TokioClock::new(BASE_MS));
    let timing = Arc::new(Timing::new(1000));
    let memory = seeded_memory(clock.clone());
    memory
        .put_profile(&Profile { id: "support".into(), name: "Support".into(), instructions: "Help customers.".into() })
        .unwrap();
    let answer = fast_model();
    let routes = if o.routes.is_empty() { vec![Route::new(answer.clone() as Arc<dyn AnswerModel>)] } else { o.routes };
    let answers = AnswerRouter::new(Mode::Cloud, Some(ssc_redact::Options::default()), routes, None).unwrap();
    let (pc, ph) = MockTransport::pair_with_clock(clock.clone());
    let phone = phone(Arc::new(ph), 12);
    let gate = o.gate;
    let parts = Parts {
        // A screen source also keeps the trigger input open after the scripted STT ends.
        screen: Some(Arc::new(fixture_screen())),
        audio: vec![Arc::new(fixture_audio(BASE_MS)) as Arc<dyn AudioSource>],
        stt: Some(Arc::new(fixture_stt(BASE_MS))),
        decision: decision.clone(),
        decisions_gate: Arc::new(move || gate),
        answers: Arc::new(answers),
        postcall: Arc::new(ScriptedAnswer::new("strong/mock", true, &[SUMMARY_JSON, FOLLOWUP])),
        before_postcall: None,
        memory: memory.clone(),
        transport: Arc::new(pc),
        rtt_ms: Arc::new(|| Some(40)),
        clock: clock.clone(),
        timing: timing.clone(),
    };
    let mut settings = Settings::new(Default::default());
    settings.speculate = o.speculate;
    let pipeline = Pipeline::with_extras(parts, settings, o.extras);
    Rig { pipeline, phone, timing, answer, decision, memory }
}

fn stages(t: &Timing, id: Uuid) -> HashMap<Stage, Vec<u64>> {
    let mut m: HashMap<Stage, Vec<u64>> = HashMap::new();
    for e in t.events().into_iter().filter(|e| e.answer_id == id) {
        m.entry(e.stage).or_default().push(e.t_ms);
    }
    m
}

async fn first_answer(r: &mut Rig) -> (AnswerStart, AnswerEnd) {
    let env = r.phone.wait_for("answer.start", |m| matches!(m, Message::AnswerStart(_))).await;
    let Message::AnswerStart(s) = env.message else { unreachable!() };
    let id = s.answer_id;
    let env = r.phone.wait_for("answer.end", move |m| matches!(m, Message::AnswerEnd(e) if e.answer_id == id)).await;
    let Message::AnswerEnd(e) = env.message else { unreachable!() };
    (s, e)
}

/// speech_end → frame_sent for the first automatic answer, and every stage checked.
async fn measure(speculate: bool) -> u64 {
    let mut r = rig(Opts { speculate, ..opts(MockDecision::always(decision_answer(Intent::Pricing, "sales"))) });
    r.pipeline.start_session(Some("sales".into())).await.unwrap();
    let (s, end) = first_answer(&mut r).await;
    assert_eq!((s.trigger, s.format, s.intent), (Trigger::Question, AnswerFormat::Fact, Intent::Pricing));
    assert_eq!(end.reason, AnswerEndReason::Done);
    assert_eq!(end.fact.as_deref(), Some("45,000 naira per seat per year"));
    tokio::time::sleep(Duration::from_millis(500)).await; // phone_rendered feedback
    let st = stages(&r.timing, s.answer_id);
    for stage in Stage::ALL {
        assert_eq!(st.get(&stage).map(Vec::len), Some(1), "{stage:?} recorded exactly once: {st:?}");
    }
    let t = |s: Stage| st[&s][0];
    assert_eq!(t(Stage::SpeechEnd), BASE_MS + 1300);
    assert_eq!(t(Stage::TriggerFired), BASE_MS + 1600);
    assert_eq!(t(Stage::DecisionDone), t(Stage::TriggerFired) + DECISION_MS);
    // Causal order that must hold either way.
    assert!(t(Stage::ContextReady) <= t(Stage::ModelRequest));
    assert!(t(Stage::ModelRequest) + FIRST_TOKEN_MS <= t(Stage::FirstToken));
    assert!(t(Stage::FrameSent) >= t(Stage::DecisionDone), "nothing reaches the phone before the decision");
    assert!(t(Stage::FrameSent) >= t(Stage::FirstToken));
    assert_eq!(t(Stage::PhoneRendered), t(Stage::FrameSent) + 20 + 12);
    if speculate {
        assert!(t(Stage::ModelRequest) < t(Stage::DecisionDone), "model started during the decision");
    } else {
        assert!(t(Stage::ContextReady) >= t(Stage::DecisionDone), "serial: context after the decision");
    }
    assert_eq!(r.answer.calls(), 1);
    r.pipeline.discard_session().await;
    t(Stage::FrameSent) - t(Stage::SpeechEnd)
}

/// H2 before/after with the mocked E2E timing (decision 560 ms, first token 400 ms).
#[tokio::test(start_paused = true)]
async fn parallel_hot_path_before_and_after() {
    let serial = measure(false).await;
    let parallel = measure(true).await;
    eprintln!("speech_end → frame_sent: serial {serial} ms, parallel {parallel} ms");
    // Serial: 300 (silence) + 560 (decision) + context + 400 (first token).
    assert!(serial >= 300 + DECISION_MS + FIRST_TOKEN_MS, "{serial}");
    // Parallel: the model's 400 ms hides inside the decision's 560 ms.
    assert!(parallel <= 300 + DECISION_MS + 20, "{parallel}");
    assert!(serial - parallel >= FIRST_TOKEN_MS, "saved {} ms", serial - parallel);
}

#[tokio::test(start_paused = true)]
async fn no_answer_cancels_the_speculation_and_sends_nothing() {
    let r = rig(opts(MockDecision::always(decision_no_answer("sales"))));
    r.pipeline.start_session(Some("sales".into())).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(r.answer.calls(), 1, "the speculative stream did start");
    let types = r.phone.types();
    assert!(!types.contains(&"answer.start") && !types.contains(&"answer.delta"), "{types:?}");
    let leaked: Vec<_> = r
        .timing
        .events()
        .into_iter()
        .filter(|e| matches!(e.stage, Stage::ContextReady | Stage::ModelRequest | Stage::FirstToken | Stage::FrameSent))
        .collect();
    assert!(leaked.is_empty(), "cancelled speculation leaves no answer timing: {leaked:?}");
    r.pipeline.discard_session().await;
}

#[tokio::test(start_paused = true)]
async fn different_profile_restarts_with_the_decided_one() {
    let mut r = rig(opts(MockDecision::always(decision_answer(Intent::Pricing, "support"))));
    r.pipeline.start_session(Some("sales".into())).await.unwrap();
    let (s, end) = first_answer(&mut r).await;
    assert_eq!(end.reason, AnswerEndReason::Done);
    let prompts = r.answer.prompts();
    assert_eq!(prompts.len(), 2, "speculation (sales) dropped, restarted (support)");
    assert!(prompts[0].prefix.contains("Sell Acme Cloud."));
    assert!(prompts[1].prefix.contains("Help customers."));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let starts = r.phone.types().iter().filter(|t| **t == "answer.start").count();
    assert_eq!(starts, 1, "one answer on the phone");
    let st = stages(&r.timing, s.answer_id);
    assert_eq!(st[&Stage::ContextReady].len(), 1, "only the restart's stages: {st:?}");
    assert!(st[&Stage::ContextReady][0] >= st[&Stage::DecisionDone][0]);
    r.pipeline.discard_session().await;
}

#[tokio::test(start_paused = true)]
async fn tight_budget_skips_speculation() {
    let model = fast_model();
    let budget = Arc::new(BudgetTracker::new(Default::default(), Arc::new(TokioClock::new(BASE_MS))));
    while budget.below(ssc_router::answer::SPECULATION_BUDGET_FRACTION) {
        assert!(budget.try_reserve(10));
    }
    let o = Opts {
        routes: vec![Route::new(model.clone() as Arc<dyn AnswerModel>).budget(budget)],
        ..opts(MockDecision::always(decision_answer(Intent::Pricing, "sales")))
    };
    let mut r = rig(o);
    r.pipeline.start_session(Some("sales".into())).await.unwrap();
    let (s, _) = first_answer(&mut r).await;
    let st = stages(&r.timing, s.answer_id);
    assert!(st[&Stage::ModelRequest][0] >= st[&Stage::DecisionDone][0], "no speculative call: {st:?}");
    assert_eq!(model.calls(), 1);
    r.pipeline.discard_session().await;
}

/// L3: `answer.start.model` names the model that produced the answer after failover.
#[tokio::test(start_paused = true)]
async fn answer_start_names_the_model_that_answered() {
    let groq = Arc::new(MockAnswer::new("groq/down", true, ANSWER_TOKENS).failing(MockOutcome::Unavailable));
    let gemini = Arc::new(MockAnswer::new("gemini/up", true, ANSWER_TOKENS));
    let o = Opts {
        routes: vec![Route::new(groq as Arc<dyn AnswerModel>), Route::new(gemini as Arc<dyn AnswerModel>)],
        ..opts(MockDecision::always(decision_answer(Intent::Pricing, "sales")))
    };
    let mut r = rig(o);
    r.pipeline.start_session(Some("sales".into())).await.unwrap();
    let (s, end) = first_answer(&mut r).await;
    assert_eq!(s.model, "gemini/up");
    assert_eq!(end.reason, AnswerEndReason::Done);
    r.pipeline.discard_session().await;
}

/// L9: "shorter" yields at most 3 cues and no detail, with a small visible-answer cap.
#[tokio::test(start_paused = true)]
async fn shorter_caps_the_answer() {
    let mut r = rig(opts(MockDecision::always(decision_answer(Intent::Technical, "sales"))));
    let sid = r.pipeline.start_session(Some("sales".into())).await.unwrap();
    let (s, end) = first_answer(&mut r).await;
    assert_eq!(end.cues.len(), 3);
    r.phone.send(Some(sid), Command::Shorter { answer_id: s.answer_id }).await;
    let env = r
        .phone
        .wait_for("shorter start", |m| matches!(m, Message::AnswerStart(a) if a.trigger == Trigger::Command))
        .await;
    let Message::AnswerStart(s2) = env.message else { unreachable!() };
    let id2 = s2.answer_id;
    let env = r.phone.wait_for("shorter end", move |m| matches!(m, Message::AnswerEnd(e) if e.answer_id == id2)).await;
    let Message::AnswerEnd(e2) = env.message else { unreachable!() };
    assert!(e2.cues.len() <= 3 && e2.cues.iter().all(|c| c.expand.is_none()), "{e2:?}");
    let details = r
        .phone
        .got
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(&e.message, Message::AnswerDelta(d) if d.answer_id == id2 && d.block == Block::Detail))
        .count();
    assert_eq!(details, 0, "no detail block for a shorter answer");
    let p = r.answer.prompts().last().unwrap().clone();
    assert_eq!(p.max_tokens, 60);
    assert!(p.dynamic.contains("shorter"));
    r.pipeline.discard_session().await;
}

/// M5: offline/local answers get the decision's tags in the background, after the answer, and
/// only when the gate allows (no Ollama model swap).
#[tokio::test(start_paused = true)]
async fn background_tag_updates_the_answer_after_it_ended() {
    let objection = Decision { ..decision_answer(Intent::Objection, "sales") };
    for allowed in [true, false] {
        let gate: TagGate = Arc::new(move || Box::pin(async move { allowed }));
        let o = Opts {
            gate: false, // decisions don't gate: fires from the end-of-question rule
            extras: Extras { tag_gate: Some(gate), ..Default::default() },
            ..opts(MockDecision::always(objection.clone()))
        };
        let mut r = rig(o);
        let sid = r.pipeline.start_session(Some("sales".into())).await.unwrap();
        let env = r.phone.wait_for("answer.start", |m| matches!(m, Message::AnswerStart(_))).await;
        let Message::AnswerStart(s) = env.message else { unreachable!() };
        assert_eq!(s.intent, Intent::Pricing, "fired from the heuristics");
        assert_eq!(r.decision.calls(), 0, "nothing ran before or during the answer");
        let id = s.answer_id;
        r.phone.wait_for("answer.end", move |m| matches!(m, Message::AnswerEnd(e) if e.answer_id == id)).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(r.decision.calls(), usize::from(allowed));
        assert!(r.memory.pin_answer(s.answer_id, true).is_ok(), "answer stored");
        // Regenerate uses the remembered tags.
        r.phone.send(Some(sid), Command::Regenerate { answer_id: s.answer_id }).await;
        let env = r
            .phone
            .wait_for("regenerate", |m| matches!(m, Message::AnswerStart(a) if a.trigger == Trigger::Command))
            .await;
        let Message::AnswerStart(s2) = env.message else { unreachable!() };
        assert_eq!(s2.intent, if allowed { Intent::Objection } else { Intent::Pricing });
        r.pipeline.discard_session().await;
    }
}

/// `frame_sent` from the transport replaces the pipeline's own stamp.
#[tokio::test(start_paused = true)]
async fn frame_sent_comes_from_the_transport_when_wired() {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let o = Opts {
        extras: Extras { frame_sent: Some(rx), ..Default::default() },
        ..opts(MockDecision::always(decision_answer(Intent::Pricing, "sales")))
    };
    let mut r = rig(o);
    r.pipeline.start_session(Some("sales".into())).await.unwrap();
    let (s, _) = first_answer(&mut r).await;
    assert!(!stages(&r.timing, s.answer_id).contains_key(&Stage::FrameSent), "no self-stamp");
    let t = TokioClock::new(BASE_MS).now_ms();
    tx.send(FrameSentEvent { answer_id: s.answer_id, t_ms: t }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(stages(&r.timing, s.answer_id)[&Stage::FrameSent], vec![t]);
    r.pipeline.discard_session().await;
}
