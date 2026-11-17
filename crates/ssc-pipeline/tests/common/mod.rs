//! Fixtures shared by the end-to-end tests: a seeded memory store, fixture audio + transcript +
//! screen, a scripted post-call model, and a phone mock that answers like the PWA does.

#![allow(dead_code)]

use async_trait::async_trait;
use ssc_audio::synth::{Segment, render};
use ssc_core::mock::{MockAudio, MockEmbedder, MockScreen, MockStt};
use ssc_core::protocol::{Command, Envelope, FeedbackItem, Message, PhoneStage, Speaker};
use ssc_core::traits::{AnswerModel, Cancel, Clock, Transport};
use ssc_core::types::{AboutMe, AudioChunk, Profile, Prompt, StreamStats, Token, TranscriptSegment};
use ssc_core::{Result, SscError};
use ssc_memory::SqliteMemory;
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

pub const BASE_MS: u64 = 1_763_000_000_000;
pub const PRICING_DOC: &str = "Pricing\n\nThe Enterprise plan costs 45,000 naira per seat per year, billed annually. \
Volume discounts start at 50 seats. SSO is included in Enterprise.";
pub const QUESTION: &str = "How much is the enterprise plan per seat?";

pub fn seeded_memory(clock: Arc<dyn Clock>) -> Arc<SqliteMemory> {
    let m = SqliteMemory::open_in_memory_with_clock(Arc::new(MockEmbedder::default()), clock).unwrap();
    m.set_about_me(&AboutMe {
        name: "Sam".into(),
        role: "Account executive".into(),
        company: "Acme".into(),
        product: "Acme Cloud".into(),
        tone: "Direct".into(),
        preferred_answers: "Numbers first".into(),
    })
    .unwrap();
    m.put_profile(&Profile { id: "sales".into(), name: "Sales".into(), instructions: "Sell Acme Cloud.".into() })
        .unwrap();
    m.ingest_doc("sales", "pricing.md", PRICING_DOC.as_bytes()).unwrap();
    Arc::new(m)
}

/// Fixture call audio: 1.1 s of "them" speech then silence, as 20 ms chunks from `t0_ms`.
pub fn fixture_audio(t0_ms: u64) -> MockAudio {
    let clip = render(&[Segment::Silence { ms: 200 }, Segment::Speech { ms: 1100 }, Segment::Silence { ms: 700 }]);
    let chunks = clip
        .samples
        .chunks(320)
        .enumerate()
        .map(|(i, s)| AudioChunk { speaker: Speaker::Them, t0_ms: t0_ms + i as u64 * 20, samples: s.to_vec() })
        .collect();
    MockAudio { chunks }
}

fn seg(text: &str, is_final: bool, t0: u64, t1: u64, ends: bool) -> TranscriptSegment {
    TranscriptSegment {
        speaker: Speaker::Them,
        text: text.into(),
        is_final,
        t0_ms: t0,
        t1_ms: t1,
        ends_utterance: ends,
    }
}

/// The transcript the fixture audio yields: a partial, then the final question whose speech
/// ended at +1300 ms (the label of `fixture_audio`), delivered at +1500 ms.
pub fn fixture_stt(base: u64) -> MockStt {
    MockStt {
        script: vec![
            (Duration::from_millis(800), seg("How much is the", false, base + 200, base + 800, false)),
            (Duration::from_millis(700), seg(QUESTION, true, base + 200, base + 1300, true)),
        ],
    }
}

pub fn fixture_screen() -> MockScreen {
    let lines_a = ["Zoom — Acme <> Globex", "Agenda", "Intro"];
    let lines_b = ["Zoom — Acme <> Globex", "Agenda", "Intro", "Globex shared: Pricing comparison sheet"];
    MockScreen::new(vec![MockScreen::snapshot("Zoom", &lines_a), MockScreen::snapshot("Zoom", &lines_b)])
}

pub const ANSWER_TOKENS: &[&str] = &[
    "1. Enterprise: 45,000 per seat\n",
    "2. Billed annually\n",
    "3. Discounts from 50 seats\n",
    "---\n",
    "1: Per seat, per year\n",
    "FACT: 45,000 naira per seat per year\n",
    "SOURCE: pricing.md\n",
];

pub const SUMMARY_JSON: &str = r#"{"summary": "Globex asked about Enterprise pricing.", "action_items": ["Sam sends the pricing sheet by Friday"], "key_qa": [{"q": "Price per seat?", "a": "45,000 naira"}], "contact_suggestions": [{"name": "Dana Obi", "company": "Globex", "notes": "Wants SSO"}]}"#;
pub const FOLLOWUP: &str = "Subject: Acme pricing\n\nHi Dana,\n\nThanks for the call. Pricing sheet attached.\n\nSam";

/// Post-call model: plays one scripted response per call, in order.
pub struct ScriptedAnswer {
    pub id: String,
    pub cloud: bool,
    responses: Mutex<VecDeque<String>>,
    pub prompts: Mutex<Vec<Prompt>>,
}

impl ScriptedAnswer {
    pub fn new(id: &str, cloud: bool, responses: &[&str]) -> Self {
        Self {
            id: id.into(),
            cloud,
            responses: Mutex::new(responses.iter().map(|s| s.to_string()).collect()),
            prompts: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl AnswerModel for ScriptedAnswer {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_cloud(&self) -> bool {
        self.cloud
    }
    async fn stream(&self, prompt: &Prompt, tx: mpsc::Sender<Token>, _cancel: Cancel) -> Result<StreamStats> {
        self.prompts.lock().unwrap().push(prompt.clone());
        let r = self.responses.lock().unwrap().pop_front().ok_or(SscError::Unavailable("script empty".into()))?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        for piece in r.split_inclusive(' ') {
            tx.send(Token { text: piece.into() }).await.map_err(|_| SscError::Cancelled)?;
        }
        Ok(StreamStats { model: self.id.clone(), first_token_ms: Some(50), total_ms: 50, tokens_out: 1 })
    }
}

/// Everything the phone received, plus a channel of the same envelopes for waiting.
pub struct PhoneMock {
    pub got: Arc<Mutex<Vec<Envelope>>>,
    pub rx: mpsc::UnboundedReceiver<Envelope>,
    pub transport: Arc<dyn Transport>,
}

/// Runs the phone side of `transport`: records every frame and, like the PWA, reports
/// `delta_ms` (first delta → first paint) for each new answer.
pub fn phone(transport: Arc<dyn Transport>, paint_ms: u32) -> PhoneMock {
    let mut incoming = transport.take_incoming().expect("phone incoming");
    let got = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::unbounded_channel();
    let (g, t) = (got.clone(), transport.clone());
    tokio::spawn(async move {
        let mut painted = HashSet::new();
        while let Some(env) = incoming.recv().await {
            if let Message::AnswerDelta(d) = &env.message
                && painted.insert(d.answer_id)
            {
                let fb = Command::Feedback {
                    answer_id: d.answer_id,
                    item: FeedbackItem::Timing { stage: PhoneStage::PhoneRendered, delta_ms: paint_ms },
                };
                let _ = t.send(env.session_id, Message::Command(fb)).await;
            }
            g.lock().unwrap().push(env.clone());
            let _ = tx.send(env);
        }
    });
    PhoneMock { got, rx, transport }
}

impl PhoneMock {
    /// Next envelope matching `f` (others are skipped but still recorded in `got`).
    pub async fn wait_for(&mut self, what: &str, f: impl Fn(&Message) -> bool) -> Envelope {
        let fut = async {
            loop {
                let env = self.rx.recv().await.expect("phone link closed");
                if f(&env.message) {
                    return env;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), fut)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    pub async fn send(&self, sid: Option<Uuid>, c: Command) {
        self.transport.send(sid, Message::Command(c)).await.unwrap();
    }

    pub fn types(&self) -> Vec<&'static str> {
        self.got.lock().unwrap().iter().map(|e| e.message.type_name()).collect()
    }
}
