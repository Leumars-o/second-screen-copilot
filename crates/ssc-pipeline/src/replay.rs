//! Replay entry point for `bench/latency --live`: scripted "them" questions in, real decision
//! and answer models (from `Config` + keychain), timing events out.
//!
//! The phone link is real (M6): a [`PcTransport`] on loopback with a paired Rust phone client
//! ([`crate::loopback_phone`]), so `answer.delta` batching, encryption and the transport's own
//! `frame_sent` signal are in the numbers, and `phone_rendered = frame_sent + RTT/2 + delta_ms`
//! uses the transport's measured RTT (≈ 0 on loopback; add the Wi-Fi RTT to read a phone number).
//!
//! Speech-to-text is a stand-in: each question streams a partial, then its final transcript
//! arrives [`STT_FINALIZE_MS`] after the end of speech (Deepgram-like endpointing), so
//! `speech_end → transcript_final` is non-zero; real STT latency is measured by `bench stt`.

use crate::loopback_phone;
use crate::pipeline::{Extras, FrameSentEvent, Parts, Pipeline, Settings};
use crate::transport::start_transport_with;
use crate::{RoutedDecisions, system};
use ssc_core::config::Config;
use ssc_core::mock::{MockAudio, MockStt};
use ssc_core::protocol::{Message, Speaker};
use ssc_core::secrets::{MemorySecretStore, SecretStore};
use ssc_core::timing::Timing;
use ssc_core::traits::{Clock, SystemClock};
use ssc_core::types::TranscriptSegment;
use ssc_core::{Result, SscError};
use ssc_memory::SqliteMemory;
use ssc_transport::TransportConfig;
use std::sync::Arc;
use std::time::Duration;

/// Gap between scripted questions (longer than the dedupe window and any answer).
pub const QUESTION_GAP: Duration = Duration::from_secs(8);
/// Stand-in STT: end of speech → final transcript.
pub const STT_FINALIZE_MS: u64 = 250;
/// Stand-in STT: a partial this long before the end of speech.
const PARTIAL_BEFORE_END_MS: u64 = 600;

/// The scripted transcript: per question, a partial (first half of the words) and the final.
/// Returns `MockStt` steps (delay since the previous step, segment); `start` = replay start.
pub fn stt_script(questions: &[String], start: u64) -> Vec<(Duration, TranscriptSegment)> {
    let mut steps = Vec::new();
    let mut prev = start;
    let gap = QUESTION_GAP.as_millis() as u64;
    for (i, q) in questions.iter().enumerate() {
        let end = start + (i as u64 + 1) * gap;
        let t0 = end.saturating_sub(2000);
        let words: Vec<&str> = q.split_whitespace().collect();
        let half = words[..words.len().div_ceil(2)].join(" ");
        let seg = |text: String, is_final: bool, t1: u64| TranscriptSegment {
            speaker: Speaker::Them,
            text,
            is_final,
            t0_ms: t0,
            t1_ms: t1,
            ends_utterance: is_final,
        };
        let partial_at = end - PARTIAL_BEFORE_END_MS;
        steps.push((Duration::from_millis(partial_at - prev), seg(half, false, partial_at)));
        let final_at = end + STT_FINALIZE_MS;
        steps.push((Duration::from_millis(final_at - partial_at), seg(q.clone(), true, end)));
        prev = final_at;
    }
    steps
}

/// Plays `questions` as "them" utterances through the real pipeline and phone link, then copies
/// every timing event into `out`. `phone_delta_ms`: the paint time the phone reports.
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

    // Real transport on loopback; its identity and pairing live in memory and a temp dir, so the
    // user's paired phone and keychain device entries are never touched.
    let dir = std::env::temp_dir().join(format!("ssc-replay-{}", uuid::Uuid::new_v4()));
    let tcfg = TransportConfig {
        lan_bind: "127.0.0.1:0".parse().expect("addr"),
        lan_host: Some("127.0.0.1".into()),
        mdns: false,
        relay_url: None,
        ..TransportConfig::default()
    };
    let link_secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let transport = start_transport_with(tcfg, link_secrets, clock.clone(), dir.clone()).await?;
    let mut phone = loopback_phone::pair(&transport, phone_delta_ms).await?;
    let frame_sent = transport.take_frame_sent().map(|mut rx| {
        let (tx, out) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            while let Some(f) = rx.recv().await {
                let _ = tx.send(FrameSentEvent { answer_id: f.answer_id, t_ms: f.t_ms }).await;
            }
        });
        out
    });

    let timing = Arc::new(Timing::new(10_000));
    let rtt = transport.clone();
    let parts = Parts {
        screen: None,
        audio: vec![Arc::new(MockAudio { chunks: vec![] })],
        stt: Some(Arc::new(MockStt { script: stt_script(questions, clock.now_ms()) })),
        decision: Arc::new(RoutedDecisions(decisions.clone())),
        decisions_gate: Arc::new(move || gate.decisions_gate_answers()),
        answers: Arc::new(models.answers),
        postcall: models.postcall,
        before_postcall: None,
        memory,
        transport: transport.clone(),
        rtt_ms: Arc::new(move || rtt.rtt_ms()),
        clock: clock.clone(),
        timing: timing.clone(),
    };
    let extras = Extras { warm: Some(models.warm), tag_gate: None, frame_sent };
    let p = Pipeline::with_extras(parts, Settings::new(cfg.clone()), extras);
    p.start_session(None).await?;

    // Count finished answers; the phone client reports paint times itself.
    let deadline = tokio::time::Instant::now() + QUESTION_GAP * (questions.len() as u32 + 2);
    let mut ended = 0;
    while ended < questions.len() {
        match tokio::time::timeout_at(deadline, phone.events.recv()).await {
            Ok(Some(env)) => {
                if matches!(env.message, Message::AnswerEnd(_)) {
                    ended += 1;
                }
            }
            _ => break, // questions the trigger skipped never produce an answer
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await; // let the last feedback land
    p.discard_session().await;
    p.shutdown().await;
    transport.shutdown();
    drop(phone);
    let _ = std::fs::remove_dir_all(&dir);
    for e in timing.events() {
        out.record(e.answer_id, e.stage, e.t_ms);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_has_partial_then_final_after_endpointing() {
        let q = vec!["How much is the enterprise plan?".to_string(), "When can we start?".to_string()];
        let s = stt_script(&q, 1_000_000);
        assert_eq!(s.len(), 4);
        let gap = QUESTION_GAP.as_millis() as u64;
        let mut t = 1_000_000;
        for (i, (d, seg)) in s.iter().enumerate() {
            t += d.as_millis() as u64;
            let end = 1_000_000 + (i as u64 / 2 + 1) * gap;
            if i % 2 == 0 {
                assert!(!seg.is_final && !seg.ends_utterance);
                assert_eq!(t, end - PARTIAL_BEFORE_END_MS);
            } else {
                assert!(seg.is_final && seg.ends_utterance);
                assert_eq!(seg.t1_ms, end);
                assert_eq!(t, end + STT_FINALIZE_MS, "final arrives after endpointing");
            }
        }
        assert_eq!(s[0].1.text, "How much is");
    }
}
