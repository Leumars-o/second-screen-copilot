//! CR-20 screen hotkey: grab the focused window, answer what is on it.
//! - cloud: the image rides in the prompt, the window text too, the answer is a `screen` card;
//! - "again" reuses the same grab (no second capture), "shorter" asks for the answer line only;
//! - local-only: no image is even captured, the text is answered by the local model;
//! - continuous screen reading runs only with `screen_watch`.

mod common;

use common::*;
use ssc_core::Result;
use ssc_core::config::Config;
use ssc_core::mock::{MockDecision, MockTransport, decision_answer};
use ssc_core::protocol::*;
use ssc_core::timing::Timing;
use ssc_core::traits::{AnswerModel, ScreenSource};
use ssc_core::types::{PromptImage, ScreenGrab, ScreenNode, ScreenSnapshot, ScreenTextSource};
use ssc_pipeline::{Parts, Pipeline, Settings, TokioClock};
use ssc_router::{AnswerRouter, Route};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const QUESTION: &str = "Quiz 3: What is inference in AI?";
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 0xFF, 0xD9];

/// A focused Word window holding a typed question. Counts grabs, snapshots and the image flag.
#[derive(Default)]
struct WordWindow {
    grabs: Mutex<Vec<bool>>,
    snapshots: AtomicUsize,
    /// Fail every grab like the lead's "no focused application".
    fail: bool,
    /// The window is read but the image capture fails (E).
    no_image: bool,
}

impl WordWindow {
    fn snap() -> ScreenSnapshot {
        ScreenSnapshot {
            app: "Microsoft Word".into(),
            window_title: "Quiz.docx".into(),
            nodes: vec![ScreenNode { role: "Text".into(), text: QUESTION.into(), bounds: None }],
            source: ScreenTextSource::Accessibility,
            captured_at_ms: 0,
        }
    }
}

impl ScreenSource for WordWindow {
    fn snapshot(&self) -> Result<ScreenSnapshot> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        Ok(Self::snap())
    }
    fn grab(&self, image: bool) -> Result<ScreenGrab> {
        self.grabs.lock().unwrap().push(image);
        if self.fail {
            return Err(ssc_core::SscError::Unavailable("no focused application (AXError -25204)".into()));
        }
        let mut g = ScreenGrab::from_snapshot(Self::snap(), None);
        if image && self.no_image {
            g.image_error = Some("ScreenCaptureKit capture failed: denied".into());
        } else if image {
            g.thumb = Some(vec![0xFF, 0xD8, 9, 0xFF, 0xD9]);
            g.window_size = Some((1440, 872));
            g.image = Some(PromptImage { mime: "image/jpeg".into(), data: JPEG.to_vec() });
            g.image_size = Some((1600, 1000));
        }
        Ok(g)
    }
}

const SCREEN_ANSWER: &str =
    "Running a trained model on new data to get predictions\n---\nTraining learns the weights; inference uses them.\n";

struct Rig {
    pipeline: Pipeline,
    phone: PhoneMock,
    model: Arc<ScriptedAnswer>,
    screen: Arc<WordWindow>,
}

fn rig(mode: Mode, screen_watch: bool, responses: &[&str]) -> Rig {
    rig_with(mode, screen_watch, responses, false)
}

fn rig_window(mode: Mode, responses: &[&str], window: WordWindow) -> Rig {
    rig_full(mode, false, responses, window)
}

fn rig_with(mode: Mode, screen_watch: bool, responses: &[&str], fail_grabs: bool) -> Rig {
    rig_full(mode, screen_watch, responses, WordWindow { fail: fail_grabs, ..Default::default() })
}

fn rig_full(mode: Mode, screen_watch: bool, responses: &[&str], window: WordWindow) -> Rig {
    let clock = Arc::new(TokioClock::new(BASE_MS));
    let memory = seeded_memory(clock.clone());
    let model = Arc::new(ScriptedAnswer::new(
        if mode == Mode::Local { "ollama/mock" } else { "groq/mock-vision" },
        mode != Mode::Local,
        responses,
    ));
    let answers = AnswerRouter::new(
        mode,
        Some(ssc_redact::Options::default()),
        vec![Route::new(model.clone() as Arc<dyn AnswerModel>)],
        None,
    )
    .unwrap();
    let (pc, ph) = MockTransport::pair_with_clock(clock.clone());
    let phone = phone(Arc::new(ph), 12);
    let screen = Arc::new(window);
    let parts = Parts {
        screen: Some(screen.clone() as Arc<dyn ScreenSource>),
        audio: vec![],
        stt: None,
        decision: Arc::new(MockDecision::always(decision_answer(Intent::Technical, "sales"))),
        decisions_gate: Arc::new(|| true),
        answers: Arc::new(answers),
        postcall: Arc::new(ScriptedAnswer::new("strong/mock", true, &[SUMMARY_JSON, FOLLOWUP])),
        before_postcall: None,
        memory,
        transport: Arc::new(pc),
        rtt_ms: Arc::new(|| Some(40)),
        clock: clock.clone(),
        timing: Arc::new(Timing::new(1000)),
    };
    let mut settings = Settings::new(Config { mode, screen_watch, ..Config::default() });
    settings.screen_poll = Duration::from_millis(250);
    Rig { pipeline: Pipeline::new(parts, settings), phone, model, screen }
}

async fn wait_end(r: &mut Rig) -> AnswerEnd {
    let env = r.phone.wait_for("answer.end", |m| matches!(m, Message::AnswerEnd(_))).await;
    let Message::AnswerEnd(e) = env.message else { unreachable!() };
    e
}

#[tokio::test(start_paused = true)]
async fn cloud_screen_hotkey_sends_the_image_and_window_text_and_streams_a_screen_card() {
    let mut r = rig(Mode::Cloud, false, &[SCREEN_ANSWER, SCREEN_ANSWER, "Running a trained model\n"]);
    // No session needed.
    let id = r.pipeline.answer_from_screen().expect("screen reader present");

    let env = r.phone.wait_for("answer.start", |m| matches!(m, Message::AnswerStart(_))).await;
    let Message::AnswerStart(s) = env.message else { unreachable!() };
    assert_eq!((s.answer_id, s.trigger, s.format), (id, Trigger::Hotkey, AnswerFormat::Screen));
    let end = wait_end(&mut r).await;
    assert_eq!(end.reason, AnswerEndReason::Done);

    let got = r.phone.got.lock().unwrap().clone();
    let text = |b: Block| -> String {
        got.iter()
            .filter_map(|e| match &e.message {
                Message::AnswerDelta(d) if d.answer_id == id && d.block == b => Some(d.text.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(text(Block::Cue).trim(), "Running a trained model on new data to get predictions");
    assert!(text(Block::Detail).contains("Training learns the weights"));

    {
        let prompts = r.model.prompts.lock().unwrap();
        let p = &prompts[0];
        assert_eq!(p.images.len(), 1, "the screenshot rides along in cloud mode");
        assert_eq!(p.images[0].data, JPEG);
        assert!(p.dynamic.contains(QUESTION), "window text reaches the model:\n{}", p.dynamic);
        assert!(p.dynamic.contains("Microsoft Word — Quiz.docx"));
        assert!(p.prefix.contains("pressed the screen hotkey"), "screen.v4 prompt");
    }
    assert_eq!(*r.screen.grabs.lock().unwrap(), vec![true]);

    // "Again" reuses the same grab: no second capture, same image.
    r.phone.send(None, Command::Regenerate { answer_id: id }).await;
    let env =
        r.phone.wait_for("answer.start (again)", |m| matches!(m, Message::AnswerStart(s) if s.answer_id != id)).await;
    let Message::AnswerStart(again) = env.message else { unreachable!() };
    assert_eq!((again.format, again.trigger), (AnswerFormat::Screen, Trigger::Command));
    wait_end(&mut r).await;
    // "Shorter": answer line only.
    r.phone.send(None, Command::Shorter { answer_id: again.answer_id }).await;
    r.phone
        .wait_for(
            "answer.start (shorter)",
            |m| matches!(m, Message::AnswerStart(s) if s.answer_id != id && s.answer_id != again.answer_id),
        )
        .await;
    wait_end(&mut r).await;

    assert_eq!(r.screen.grabs.lock().unwrap().len(), 1, "commands reuse the grab");
    let prompts = r.model.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 3);
    assert!(prompts.iter().all(|p| p.images.len() == 1 && p.images[0].data == JPEG));
    assert!(prompts[2].dynamic.contains("output only the first line"), "{}", prompts[2].dynamic);
    // Continuous screen reading stayed off.
    assert_eq!(r.screen.snapshots.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn local_only_never_captures_an_image_and_answers_from_text() {
    let mut r = rig(Mode::Local, false, &[SCREEN_ANSWER]);
    r.pipeline.answer_from_screen().unwrap();
    let env = r.phone.wait_for("answer.start", |m| matches!(m, Message::AnswerStart(_))).await;
    let Message::AnswerStart(s) = env.message else { unreachable!() };
    assert_eq!(s.format, AnswerFormat::Screen);
    assert!(s.model.starts_with("ollama/"));
    wait_end(&mut r).await;
    assert_eq!(*r.screen.grabs.lock().unwrap(), vec![false], "local-only asks for text only");
    let prompts = r.model.prompts.lock().unwrap();
    assert!(prompts[0].images.is_empty());
    assert!(prompts[0].dynamic.contains(QUESTION));
}

#[tokio::test(start_paused = true)]
async fn screen_watch_gates_continuous_screen_reading() {
    let r = rig(Mode::Cloud, false, &[]);
    r.pipeline.start_session(None).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(r.screen.snapshots.load(Ordering::SeqCst), 0, "off by default");

    let r = rig(Mode::Cloud, true, &[]);
    r.pipeline.start_session(None).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(r.screen.snapshots.load(Ordering::SeqCst) >= 2, "on: polled");
}

/// CR-21: the phone's "Read screen" button is the screen hotkey.
#[tokio::test(start_paused = true)]
async fn phone_screen_command_reads_the_screen_and_answers() {
    let mut r = rig(Mode::Cloud, false, &[SCREEN_ANSWER]);
    r.phone.send(None, Command::Screen).await;
    let env = r.phone.wait_for("answer.start", |m| matches!(m, Message::AnswerStart(_))).await;
    let Message::AnswerStart(s) = env.message else { unreachable!() };
    assert_eq!((s.format, s.trigger), (AnswerFormat::Screen, Trigger::Hotkey));
    assert_eq!(wait_end(&mut r).await.reason, AnswerEndReason::Done);
    assert_eq!(*r.screen.grabs.lock().unwrap(), vec![true]);
    assert_eq!(r.model.prompts.lock().unwrap()[0].images.len(), 1);
}

/// CR-23: a failed screen read shows up in agent.status (via the agent's provider) and in the
/// agent.log backlog the phone gets on agent.sync.
#[tokio::test(start_paused = true)]
async fn failed_screen_read_reaches_the_phone_status_and_logs() {
    let mut r = rig_with(Mode::Cloud, false, &[], true);
    r.pipeline.set_status_provider(Arc::new(|h: &ssc_pipeline::PipelineHealth| {
        let (health, summary) = match &h.last_grab {
            Some((_, CheckState::Error, d)) => (Health::Error, format!("Last screen read: {d}")),
            _ => (Health::Ok, "All good".to_string()),
        };
        AgentStatus { health, summary, checks: vec![], capturing: h.capturing, t: 0 }
    }));
    let first = r.phone.wait_for("initial status", |m| matches!(m, Message::AgentStatus(_))).await;
    let Message::AgentStatus(s) = first.message else { unreachable!() };
    assert_eq!(s.health, Health::Ok);

    r.pipeline.answer_from_screen().unwrap();
    let env =
        r.phone.wait_for("error status", |m| matches!(m, Message::AgentStatus(s) if s.health == Health::Error)).await;
    let Message::AgentStatus(s) = env.message else { unreachable!() };
    assert!(s.summary.contains("no focused application"), "{}", s.summary);
    // The card closes with an error instead of hanging, and the log line streams live
    // (the batch goes out 0.5 s after the line).
    tokio::time::sleep(Duration::from_secs(1)).await;
    {
        let got = r.phone.got.lock().unwrap();
        assert!(got.iter().any(|e| matches!(&e.message, Message::AnswerEnd(x) if x.reason == AnswerEndReason::Error)));
        assert!(got.iter().any(|e| matches!(&e.message, Message::AgentLog(l)
            if l.entries.iter().any(|e| e.level == LogLevel::Error && e.text.contains("could not capture the screen")))));
    }
    r.phone.send(None, Command::AgentSync).await;
    r.phone.wait_for("status on sync", |m| matches!(m, Message::AgentStatus(_))).await;
    let env = r.phone.wait_for("backlog", |m| matches!(m, Message::AgentLog(l) if l.entries.len() > 1 || l.entries.iter().any(|e| e.text.contains("focused window")))).await;
    let Message::AgentLog(b) = env.message else { unreachable!() };
    assert!(b.entries.iter().any(|e| e.text.contains("could not capture the screen")));
    // Never prompts or screen text in the log feed.
    assert!(b.entries.iter().all(|e| !e.text.contains("===== PREFIX")));
}

/// B: the card shows what was captured. E: a failed image capture is an error card, never an
/// answer guessed from the window text (no model call).
#[tokio::test(start_paused = true)]
async fn thumbnail_rides_on_the_card_and_a_failed_capture_says_so() {
    let mut r = rig(Mode::Cloud, false, &[SCREEN_ANSWER]);
    r.pipeline.answer_from_screen().unwrap();
    let env = r.phone.wait_for("answer.start", |m| matches!(m, Message::AnswerStart(_))).await;
    let Message::AnswerStart(s) = env.message else { unreachable!() };
    assert_eq!(s.thumb.as_deref(), Some("data:image/jpeg;base64,/9gJ/9k="));
    wait_end(&mut r).await;

    let mut r = rig_window(Mode::Cloud, &[SCREEN_ANSWER], WordWindow { no_image: true, ..Default::default() });
    r.pipeline.answer_from_screen().unwrap();
    let end = wait_end(&mut r).await;
    assert_eq!(end.reason, AnswerEndReason::Error);
    let got = r.phone.got.lock().unwrap().clone();
    let card: String = got
        .iter()
        .filter_map(|e| match &e.message {
            Message::AnswerDelta(d) => Some(d.text.clone()),
            _ => None,
        })
        .collect();
    assert!(card.starts_with("Couldn't capture the screen: ScreenCaptureKit capture failed"), "{card}");
    assert!(r.model.prompts.lock().unwrap().is_empty(), "no answer guessed from text");
}

/// G: a follow-up on a screen card asks about the same screenshot, with the earlier answer.
#[tokio::test(start_paused = true)]
async fn follow_up_on_a_screen_card_keeps_the_screenshot_and_the_answer() {
    let mut r = rig(Mode::Cloud, false, &[SCREEN_ANSWER, "Because it runs a trained model.\n"]);
    let id = r.pipeline.answer_from_screen().unwrap();
    wait_end(&mut r).await;
    r.phone.send(None, Command::Followup { answer_id: Some(id), text: "why is that the answer?".into() }).await;
    let env = r.phone.wait_for("follow-up start", |m| matches!(m, Message::AnswerStart(s) if s.answer_id != id)).await;
    let Message::AnswerStart(s) = env.message else { unreachable!() };
    assert_eq!(s.format, AnswerFormat::Screen);
    wait_end(&mut r).await;
    assert_eq!(r.screen.grabs.lock().unwrap().len(), 1, "no new capture");
    let prompts = r.model.prompts.lock().unwrap();
    let p = &prompts[1];
    assert_eq!(p.images.len(), 1, "same screenshot");
    assert_eq!(p.images[0].data, JPEG);
    assert!(p.dynamic.contains("The user asks: why is that the answer?"), "{}", p.dynamic);
    assert!(p.dynamic.contains("Running a trained model on new data"), "earlier answer quoted:\n{}", p.dynamic);
}
