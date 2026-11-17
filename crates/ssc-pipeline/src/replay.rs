//! Replay entry point for `bench/latency --live`: scripted "them" questions in, real decision
//! and answer models (from `Config` + keychain), timing events out. Speech-to-text and the phone
//! are stand-ins (the transcript is the fixture text; a mock phone reports a fixed paint time),
//! so the numbers cover trigger → decision → context → model → frame, which is where the
//! latency budget is spent.

use crate::pipeline::{Parts, Pipeline, Settings};
use crate::{RoutedDecisions, system};
use ssc_core::config::Config;
use ssc_core::mock::{MockAudio, MockStt, MockTransport};
use ssc_core::protocol::{Command, FeedbackItem, Message, PhoneStage, Speaker};
use ssc_core::secrets::SecretStore;
use ssc_core::timing::Timing;
use ssc_core::traits::{Clock, SystemClock, Transport};
use ssc_core::types::TranscriptSegment;
use ssc_core::{Result, SscError};
use ssc_memory::SqliteMemory;
use std::sync::Arc;
use std::time::Duration;

/// Gap between scripted questions (longer than the dedupe window and any answer).
pub const QUESTION_GAP: Duration = Duration::from_secs(8);

/// Plays `questions` as final "them" utterances through the real pipeline, then copies every
/// timing event into `out`. `phone_delta_ms`: the paint time the mock phone reports.
pub async fn replay_questions(
    cfg: &Config,
    secrets: Arc<dyn SecretStore>,
    questions: &[String],
    phone_delta_ms: u32,
    out: &Timing,
) -> Result<()> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let models = ssc_router::build(cfg, &*secrets, clock.clone(), &ssc_router::RouterOptions::default())?;
    let decisions = Arc::new(models.decisions);
    let gate = decisions.clone();
    let memory = {
        let path = ssc_core::config::data_dir().join("memory.sqlite");
        let emb = system::embedder(&|_| {}).await?;
        Arc::new(
            tokio::task::spawn_blocking(move || SqliteMemory::open(&path, emb))
                .await
                .map_err(|e| SscError::Other(e.into()))??,
        )
    };
    // Each question "ends" when the mock STT emits it; t1 is stamped at emit time by the clock
    // the trigger engine uses, so speech_end → transcript_final is ~0 here (no real STT).
    let start = clock.now_ms();
    let script = questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let t = start + (i as u64 + 1) * QUESTION_GAP.as_millis() as u64;
            (
                QUESTION_GAP,
                TranscriptSegment {
                    speaker: Speaker::Them,
                    text: q.clone(),
                    is_final: true,
                    t0_ms: t.saturating_sub(2000),
                    t1_ms: t,
                    ends_utterance: true,
                },
            )
        })
        .collect();
    let (pc, phone) = MockTransport::pair_with_clock(clock.clone());
    let phone: Arc<dyn Transport> = Arc::new(phone);
    let mut incoming = phone.take_incoming().ok_or(SscError::Invalid("phone incoming taken".into()))?;
    let timing = Arc::new(Timing::new(10_000));
    let parts = Parts {
        screen: None,
        audio: vec![Arc::new(MockAudio { chunks: vec![] })],
        stt: Some(Arc::new(MockStt { script })),
        decision: Arc::new(RoutedDecisions(decisions.clone())),
        decisions_gate: Arc::new(move || gate.decisions_gate_answers()),
        answers: Arc::new(models.answers),
        postcall: models.postcall,
        before_postcall: None,
        memory,
        transport: Arc::new(pc),
        rtt_ms: Arc::new(|| Some(0)),
        clock: clock.clone(),
        timing: timing.clone(),
    };
    let p = Pipeline::new(parts, Settings::new(cfg.clone()));
    p.start_session(None).await?;

    // Phone stand-in: report the paint time per new answer; count finished answers.
    let deadline = tokio::time::Instant::now() + QUESTION_GAP * (questions.len() as u32 + 2);
    let mut seen = std::collections::HashSet::new();
    let mut ended = 0;
    while ended < questions.len() {
        let env = match tokio::time::timeout_at(deadline, incoming.recv()).await {
            Ok(Some(e)) => e,
            _ => break, // questions the trigger skipped never produce an answer
        };
        match env.message {
            Message::AnswerDelta(d) if seen.insert(d.answer_id) => {
                let item = FeedbackItem::Timing { stage: PhoneStage::PhoneRendered, delta_ms: phone_delta_ms };
                let fb = Command::Feedback { answer_id: d.answer_id, item };
                phone.send(env.session_id, Message::Command(fb)).await?;
            }
            Message::AnswerEnd(_) => ended += 1,
            _ => {}
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await; // let the last feedback land
    p.discard_session().await;
    for e in timing.events() {
        out.record(e.answer_id, e.stage, e.t_ms);
    }
    Ok(())
}
