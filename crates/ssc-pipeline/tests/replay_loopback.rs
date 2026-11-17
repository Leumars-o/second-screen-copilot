//! M6: the `bench latency --live` replay path — real `PcTransport` on loopback, the Rust phone
//! client, the transport's `frame_sent` and the scripted STT stand-in — with mock models (no
//! keys, no network beyond 127.0.0.1).

mod common;

use common::*;
use ssc_core::mock::{MockAnswer, MockAudio, MockDecision, MockStt, decision_answer};
use ssc_core::protocol::*;
use ssc_core::secrets::{MemorySecretStore, SecretStore};
use ssc_core::timing::{Stage, Timing};
use ssc_core::traits::{AnswerModel, Clock, SystemClock};
use ssc_pipeline::pipeline::{Extras, FrameSentEvent};
use ssc_pipeline::replay::{STT_FINALIZE_MS, stt_script};
use ssc_pipeline::transport::start_transport_with;
use ssc_pipeline::{Parts, Pipeline, Settings, loopback_phone};
use ssc_router::{AnswerRouter, Route};
use ssc_transport::TransportConfig;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_runs_through_the_real_transport_on_loopback() {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let dir = std::env::temp_dir().join(format!("ssc-replay-test-{}", uuid::Uuid::new_v4()));
    let tcfg = TransportConfig {
        lan_bind: "127.0.0.1:0".parse().unwrap(),
        lan_host: Some("127.0.0.1".into()),
        mdns: false,
        ..TransportConfig::default()
    };
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let transport = start_transport_with(tcfg, secrets, clock.clone(), dir.clone()).await.unwrap();
    let mut phone = loopback_phone::pair(&transport, 9).await.unwrap();
    let mut fs = transport.take_frame_sent().expect("frame_sent receiver");
    let (fs_tx, fs_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(f) = fs.recv().await {
            let _ = fs_tx.send(FrameSentEvent { answer_id: f.answer_id, t_ms: f.t_ms }).await;
        }
    });

    let answer = Arc::new(MockAnswer::new("groq/mock", true, ANSWER_TOKENS));
    let answers = AnswerRouter::new(Mode::Cloud, None, vec![Route::new(answer as Arc<dyn AnswerModel>)], None).unwrap();
    let timing = Arc::new(Timing::new(1000));
    let rtt = transport.clone();
    // The first question ends QUESTION_GAP (8 s) after start: shift the script so it ends in 1 s.
    let start = clock.now_ms();
    let mut script = stt_script(&[QUESTION.to_string()], start.saturating_sub(7_000));
    script[0].0 = Duration::from_millis(400);
    let parts = Parts {
        screen: Some(Arc::new(fixture_screen())),
        audio: vec![Arc::new(MockAudio { chunks: vec![] })],
        stt: Some(Arc::new(MockStt { script })),
        decision: Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales"))),
        decisions_gate: Arc::new(|| true),
        answers: Arc::new(answers),
        postcall: Arc::new(ScriptedAnswer::new("strong", true, &[SUMMARY_JSON, FOLLOWUP])),
        before_postcall: None,
        memory: seeded_memory(clock.clone()),
        transport: transport.clone(),
        rtt_ms: Arc::new(move || rtt.rtt_ms()),
        clock: clock.clone(),
        timing: timing.clone(),
    };
    let extras = Extras { frame_sent: Some(fs_rx), ..Default::default() };
    let p = Pipeline::with_extras(parts, Settings::new(Default::default()), extras);
    p.start_session(Some("sales".into())).await.unwrap();

    let id = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let env = phone.events.recv().await.expect("phone open");
            if let Message::AnswerEnd(e) = env.message {
                return e.answer_id;
            }
        }
    })
    .await
    .expect("answer over the real link");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let ev = loop {
        let ev: Vec<_> = timing.events().into_iter().filter(|e| e.answer_id == id).collect();
        if ev.iter().any(|e| e.stage == Stage::PhoneRendered) {
            break ev;
        }
        assert!(tokio::time::Instant::now() < deadline, "phone_rendered missing: {ev:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let t = |s: Stage| ev.iter().find(|e| e.stage == s).map(|e| e.t_ms).unwrap_or_else(|| panic!("{s:?}: {ev:?}"));
    assert_eq!(ev.iter().filter(|e| e.stage == Stage::FrameSent).count(), 1, "one frame_sent, from the transport");
    assert!(t(Stage::TranscriptFinal) >= t(Stage::SpeechEnd) + STT_FINALIZE_MS, "STT stand-in finalize delay");
    assert!(t(Stage::FrameSent) >= t(Stage::FirstToken));
    let half_rtt = transport.rtt_ms().unwrap_or(0) / 2;
    assert!(t(Stage::PhoneRendered) >= t(Stage::FrameSent) + 9);
    assert!(t(Stage::PhoneRendered) <= t(Stage::FrameSent) + half_rtt + 9 + 5);

    p.discard_session().await;
    p.shutdown().await;
    transport.shutdown();
    drop(phone);
    let _ = std::fs::remove_dir_all(dir);
}
