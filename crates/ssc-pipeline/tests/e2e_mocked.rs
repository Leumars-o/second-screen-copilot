//! W2-INTEGRATE acceptance: end-to-end with every external dependency mocked.
//! fixture audio → (mock STT) transcript → fixture screen → trigger + mock decision → context
//! from a real SQLite store → mock answer model → MockTransport → phone mock; then phone
//! commands and session end → summary.

mod common;

use common::*;
use ssc_core::decision::Tier;
use ssc_core::mock::{MockAnswer, MockDecision, MockOutcome, MockTransport, decision_answer, decision_no_answer};
use ssc_core::protocol::*;
use ssc_core::timing::{Stage, Timing};
use ssc_core::traits::{AnswerModel, AudioSource, Clock, MemoryStore, Transport};
use ssc_pipeline::{Parts, Pipeline, Settings, TokioClock};
use ssc_router::{AnswerRouter, Route};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

struct Rig {
    pipeline: Pipeline,
    phone: PhoneMock,
    timing: Arc<Timing>,
    answer: Arc<MockAnswer>,
    postcall: Arc<ScriptedAnswer>,
    memory: Arc<ssc_memory::SqliteMemory>,
    decision: Arc<MockDecision>,
}

fn rig(decision: MockDecision, gate: bool) -> Rig {
    let decision = Arc::new(decision);
    let clock = Arc::new(TokioClock::new(BASE_MS));
    let timing = Arc::new(Timing::new(1000));
    let memory = seeded_memory(clock.clone());
    let answer = Arc::new(MockAnswer::new("groq/mock-fast", true, ANSWER_TOKENS));
    let answers = AnswerRouter::new(
        Mode::Cloud,
        Some(ssc_redact::Options::default()),
        vec![Route::new(answer.clone() as Arc<dyn AnswerModel>)],
        None,
    )
    .unwrap();
    let postcall = Arc::new(ScriptedAnswer::new("strong/mock", true, &[SUMMARY_JSON, FOLLOWUP]));
    let (pc, ph) = MockTransport::pair_with_clock(clock.clone());
    let phone = phone(Arc::new(ph), 12);
    let parts = Parts {
        screen: Some(Arc::new(fixture_screen())),
        audio: vec![Arc::new(fixture_audio(BASE_MS)) as Arc<dyn AudioSource>],
        stt: Some(Arc::new(fixture_stt(BASE_MS))),
        decision: decision.clone(),
        decisions_gate: Arc::new(move || gate),
        answers: Arc::new(answers),
        postcall: postcall.clone(),
        before_postcall: None,
        memory: memory.clone(),
        transport: Arc::new(pc),
        rtt_ms: Arc::new(|| Some(40)),
        clock: clock.clone(),
        timing: timing.clone(),
    };
    let mut settings = Settings::new(Default::default());
    settings.screen_poll = Duration::from_millis(250);
    Rig { pipeline: Pipeline::new(parts, settings), phone, timing, answer, postcall, memory, decision }
}

fn stages_for(t: &Timing, id: Uuid) -> Vec<(Stage, u64)> {
    t.events().into_iter().filter(|e| e.answer_id == id).map(|e| (e.stage, e.t_ms)).collect()
}

#[tokio::test(start_paused = true)]
async fn question_to_phone_to_summary() {
    let mut r = rig(MockDecision::always(decision_answer(Intent::Pricing, "sales")), true);
    let sid = r.pipeline.start_session(Some("sales".into())).await.unwrap();
    let start = r.phone.wait_for("session.start", |m| matches!(m, Message::SessionStart(_))).await;
    assert_eq!(start.session_id, Some(sid));

    // The question in "them" speech fires an answer.
    let env = r.phone.wait_for("answer.start", |m| matches!(m, Message::AnswerStart(_))).await;
    let Message::AnswerStart(s) = env.message else { unreachable!() };
    assert_eq!((s.trigger, s.format, s.intent), (Trigger::Question, AnswerFormat::Fact, Intent::Pricing));
    assert_eq!(env.session_id, Some(sid));
    let id = s.answer_id;
    let env = r.phone.wait_for("answer.end", |m| matches!(m, Message::AnswerEnd(_))).await;
    let Message::AnswerEnd(end) = env.message else { unreachable!() };
    assert_eq!(end.answer_id, id);
    assert_eq!(end.reason, AnswerEndReason::Done);
    assert_eq!(end.cues.len(), 3);
    assert_eq!(end.cues[0].expand.as_deref(), Some("Per seat, per year"));
    assert_eq!(end.fact.as_deref(), Some("45,000 naira per seat per year"));
    assert_eq!(end.source_doc.as_deref(), Some("pricing.md"));

    // Deltas arrived before the end, cue block first.
    let types = r.phone.types();
    let first_delta = types.iter().position(|t| *t == "answer.delta").unwrap();
    assert!(first_delta < types.iter().position(|t| *t == "answer.end").unwrap());
    assert!(types.contains(&"transcript.partial"), "{types:?}");

    // The decision saw the screen change; the (fact) prompt carried the doc chunk and question.
    assert!(r.decision.inputs()[0].screen_diff.contains("Pricing comparison sheet"), "screen diff to decision");
    let p = &r.answer.prompts()[0];
    assert!(p.dynamic.contains("45,000 naira"), "doc chunk in prompt");
    assert!(p.dynamic.contains(QUESTION));
    assert!(p.prefix.contains("Sam") && p.prefix.contains("Sell Acme Cloud."));

    // Timing: every stage, in order, with phone_rendered = frame_sent + RTT/2 + delta_ms.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while stages_for(&r.timing, id).iter().all(|(s, _)| *s != Stage::PhoneRendered) {
        assert!(tokio::time::Instant::now() < deadline, "phone_rendered never recorded");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let ev = stages_for(&r.timing, id);
    let order: Vec<Stage> = ev.iter().map(|(s, _)| *s).collect();
    assert_eq!(order, Stage::ALL.to_vec(), "timing events complete and in order");
    assert!(ev.windows(2).all(|w| w[0].1 <= w[1].1), "timestamps non-decreasing: {ev:?}");
    let t = |st| ev.iter().find(|(s, _)| *s == st).unwrap().1;
    assert_eq!(t(Stage::SpeechEnd), BASE_MS + 1300);
    assert_eq!(t(Stage::PhoneRendered), t(Stage::FrameSent) + 20 + 12);
    assert!(t(Stage::FirstToken) - t(Stage::SpeechEnd) < 1500, "fixture latency within the p50 target");
    assert!(r.pipeline.stage_stats().iter().any(|s| s.count > 0));

    // Phone commands: pin, memory list, expand all, regenerate.
    r.phone.send(Some(sid), Command::Pin { answer_id: id }).await;
    r.phone.send(Some(sid), Command::MemoryList { layer: None }).await;
    let snap = r.phone.wait_for("memory.snapshot", |m| matches!(m, Message::MemorySnapshot(_))).await;
    let Message::MemorySnapshot(snap) = snap.message else { unreachable!() };
    assert!(snap.items.iter().any(|i| i.layer == Layer::AboutMe));
    r.phone.send(Some(sid), Command::ExpandAll { answer_id: id }).await;
    let env = r.phone.wait_for("expand answer", |m| matches!(m, Message::AnswerStart(_))).await;
    let Message::AnswerStart(s2) = env.message else { unreachable!() };
    assert_eq!(s2.trigger, Trigger::Command);
    let env =
        r.phone.wait_for("expand end", |m| matches!(m, Message::AnswerEnd(e) if e.answer_id == s2.answer_id)).await;
    let Message::AnswerEnd(e2) = env.message else { unreachable!() };
    assert_eq!(
        e2.cues.iter().map(|c| &c.text).collect::<Vec<_>>(),
        end.cues.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
    assert!(r.answer.prompts().last().unwrap().dynamic.contains("1. Enterprise"), "expand prompt has the cues");
    r.phone
        .send(Some(sid), Command::Feedback { answer_id: id, item: FeedbackItem::Rating { value: Rating::Up } })
        .await;

    // Session end → session.end, then session.summary.
    let summary = r.pipeline.end_session(SessionEndReason::User).await.unwrap().unwrap();
    assert_eq!(summary.summary, "Globex asked about Enterprise pricing.");
    assert_eq!(summary.action_items, vec!["Sam sends the pricing sheet by Friday"]);
    assert!(summary.followup_email.starts_with("Subject: Acme pricing"));
    assert_eq!(summary.contact_suggestions.len(), 1);
    r.phone.wait_for("session.end", |m| matches!(m, Message::SessionEnd(_))).await;
    let env = r.phone.wait_for("session.summary", |m| matches!(m, Message::SessionSummary(_))).await;
    assert_eq!(env.message, Message::SessionSummary(summary.clone()));
    assert!(!r.pipeline.is_capturing());

    // The summary prompt saw the transcript and the shown answer; contacts wait for confirmation.
    let sp = r.postcall.prompts.lock().unwrap()[0].clone();
    assert!(sp.dynamic.contains(QUESTION) && sp.dynamic.contains("Answers shown"));
    let pending = r.memory.pending_contact_suggestions().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(r.memory.list(Some(Layer::Contact)).unwrap().is_empty(), "nothing saved silently");
    r.phone
        .send(None, Command::ContactConfirm { suggestion_id: summary.contact_suggestions[0].id, accept: true })
        .await;
    r.phone
        .wait_for(
            "snapshot with contact",
            |m| matches!(m, Message::MemorySnapshot(s) if s.items.iter().any(|i| i.layer == Layer::Contact)),
        )
        .await;
    // Transcripts were not kept (default off).
    assert!(r.memory.transcript(sid).unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn hotkey_fires_even_when_the_decision_model_always_fails() {
    let mut r = rig(MockDecision::scripted(vec![], MockOutcome::Unavailable), true);
    assert!(r.pipeline.answer_now().is_none(), "no session, no answer");
    r.pipeline.start_session(None).await.unwrap();
    let id = r.pipeline.answer_now().unwrap();
    let env = r.phone.wait_for("hotkey answer", |m| matches!(m, Message::AnswerStart(s) if s.answer_id == id)).await;
    let Message::AnswerStart(s) = env.message else { unreachable!() };
    assert_eq!(s.trigger, Trigger::Hotkey);
    r.phone
        .wait_for(
            "hotkey end",
            |m| matches!(m, Message::AnswerEnd(e) if e.answer_id == id && e.reason == AnswerEndReason::Done),
        )
        .await;
    r.pipeline.end_session(SessionEndReason::User).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn small_talk_shows_nothing_but_offline_fires_from_the_rule() {
    // Cloud decisions gate: small talk → no answer.
    let r = rig(MockDecision::always(decision_no_answer("sales")), true);
    r.pipeline.start_session(Some("sales".into())).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!r.phone.types().contains(&"answer.start"), "{:?}", r.phone.types());
    r.pipeline.end_session(SessionEndReason::User).await.unwrap();

    // Jev degraded (gate false): the end-of-question rule fires without waiting for a decision.
    let mut r = rig(MockDecision::always(decision_no_answer("sales")), false);
    r.pipeline.start_session(Some("sales".into())).await.unwrap();
    let env = r.phone.wait_for("offline answer", |m| matches!(m, Message::AnswerStart(_))).await;
    let Message::AnswerStart(s) = env.message else { unreachable!() };
    assert_eq!(s.trigger, Trigger::Question);
    let _ = Tier::Fast;
    r.pipeline.end_session(SessionEndReason::User).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn keep_transcripts_stores_finals_and_cloud_postcall_is_redacted() {
    let clock = Arc::new(TokioClock::new(BASE_MS));
    let memory = seeded_memory(clock.clone());
    let answers =
        AnswerRouter::new(Mode::Cloud, None, vec![Route::new(Arc::new(MockAnswer::cue_answer("m", true)))], None)
            .unwrap();
    let postcall = Arc::new(ScriptedAnswer::new("strong", true, &[SUMMARY_JSON, FOLLOWUP]));
    let (pc, ph) = MockTransport::pair_with_clock(clock.clone());
    let _phone = phone(Arc::new(ph), 5);
    let card = "4111 1111 1111 1111";
    let stt = ssc_core::mock::MockStt {
        script: vec![(
            Duration::from_millis(100),
            ssc_core::types::TranscriptSegment {
                speaker: Speaker::Me,
                text: format!("my card is {card}"),
                is_final: true,
                t0_ms: BASE_MS,
                t1_ms: BASE_MS + 90,
                ends_utterance: true,
            },
        )],
    };
    let parts = Parts {
        screen: None,
        audio: vec![Arc::new(fixture_audio(BASE_MS))],
        stt: Some(Arc::new(stt)),
        decision: Arc::new(MockDecision::always(decision_answer(Intent::Technical, "sales"))),
        decisions_gate: Arc::new(|| true),
        answers: Arc::new(answers),
        postcall: postcall.clone(),
        before_postcall: None,
        memory: memory.clone(),
        transport: Arc::new(pc),
        rtt_ms: Arc::new(|| None),
        clock: clock.clone(),
        timing: Arc::new(Timing::new(100)),
    };
    let cfg = ssc_core::config::Config { keep_transcripts: true, ..Default::default() };
    let p = Pipeline::new(parts, Settings::new(cfg));
    let sid = p.start_session(None).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    p.end_session(SessionEndReason::User).await.unwrap().unwrap();
    assert_eq!(memory.transcript(sid).unwrap().len(), 1);
    let prompts = postcall.prompts.lock().unwrap();
    assert!(!prompts.is_empty());
    assert!(prompts.iter().all(|p| !p.dynamic.contains(card)), "card number redacted before the cloud call");
    let _ = clock.now_ms();
    let _: &dyn Transport = &*_phone.transport;
}
