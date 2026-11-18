//! Deterministic mocks for every trait in [`crate::traits`]. Workers in Wave 1 build against these.
//! All delays use `tokio::time`, so tests can run with `#[tokio::test(start_paused = true)]`.

use crate::decision::{Decision, DecisionInput, Scored, Tier};
use crate::protocol::{Envelope, Intent, Layer, MemoryEdit, MemoryItem, Message, Urgency};
use crate::traits::*;
use crate::types::*;
use crate::{Result, SscError};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ---------- clock ----------

/// Manually advanced clock.
#[derive(Debug, Clone, Default)]
pub struct MockClock(Arc<AtomicU64>);

impl MockClock {
    pub fn new(start_ms: u64) -> Self {
        Self(Arc::new(AtomicU64::new(start_ms)))
    }
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for MockClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

// ---------- screen ----------

/// Returns scripted snapshots in order, then repeats the last one.
pub struct MockScreen {
    script: Mutex<VecDeque<ScreenSnapshot>>,
    last: Mutex<Option<ScreenSnapshot>>,
}

impl MockScreen {
    pub fn new(snapshots: Vec<ScreenSnapshot>) -> Self {
        Self { script: Mutex::new(snapshots.into()), last: Mutex::new(None) }
    }
    /// A snapshot with one text node per line.
    pub fn snapshot(app: &str, lines: &[&str]) -> ScreenSnapshot {
        ScreenSnapshot {
            app: app.into(),
            window_title: format!("{app} — window"),
            nodes: lines.iter().map(|l| ScreenNode { role: "Text".into(), text: (*l).into(), bounds: None }).collect(),
            source: ScreenTextSource::Accessibility,
            captured_at_ms: 0,
        }
    }
}

impl ScreenSource for MockScreen {
    fn snapshot(&self) -> Result<ScreenSnapshot> {
        let next = self.script.lock().unwrap().pop_front();
        let mut last = self.last.lock().unwrap();
        if let Some(s) = next {
            *last = Some(s);
        }
        last.clone().ok_or(SscError::Unavailable("no focused window".into()))
    }
}

// ---------- audio / stt ----------

/// Sends the given chunks once, then stops.
pub struct MockAudio {
    pub chunks: Vec<AudioChunk>,
}

impl AudioSource for MockAudio {
    fn start(&self, tx: mpsc::Sender<AudioChunk>, cancel: CancellationToken) -> Result<()> {
        let chunks = self.chunks.clone();
        tokio::spawn(async move {
            for c in chunks {
                if cancel.is_cancelled() || tx.send(c).await.is_err() {
                    break;
                }
            }
        });
        Ok(())
    }
}

/// Drains audio and emits scripted segments, each after its delay.
pub struct MockStt {
    pub script: Vec<(Duration, TranscriptSegment)>,
}

impl SttEngine for MockStt {
    fn start(
        &self,
        mut rx: mpsc::Receiver<AudioChunk>,
        tx: mpsc::Sender<TranscriptSegment>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let script = self.script.clone();
        let c2 = cancel.clone();
        tokio::spawn(async move { while !c2.is_cancelled() && rx.recv().await.is_some() {} });
        tokio::spawn(async move {
            for (delay, seg) in script {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {}
                }
                if tx.send(seg).await.is_err() {
                    break;
                }
            }
        });
        Ok(())
    }
}

// ---------- decision ----------

#[derive(Debug, Clone)]
pub enum MockOutcome<T> {
    Ok(T),
    Timeout,
    RateLimited,
    Unavailable,
}

impl<T> MockOutcome<T> {
    fn into_result(self, timeout_ms: u64) -> Result<T> {
        match self {
            MockOutcome::Ok(v) => Ok(v),
            MockOutcome::Timeout => Err(SscError::Timeout(timeout_ms)),
            MockOutcome::RateLimited => Err(SscError::RateLimited { retry_after_ms: None }),
            MockOutcome::Unavailable => Err(SscError::Unavailable("mock".into())),
        }
    }
}

/// Plays scripted outcomes in order, then repeats `fallback`. Counts calls.
pub struct MockDecision {
    pub id: String,
    pub delay: Duration,
    script: Mutex<VecDeque<MockOutcome<Decision>>>,
    fallback: MockOutcome<Decision>,
    calls: AtomicUsize,
    inputs: Mutex<Vec<DecisionInput>>,
}

impl MockDecision {
    pub fn always(decision: Decision) -> Self {
        Self::scripted(vec![], MockOutcome::Ok(decision))
    }
    pub fn scripted(script: Vec<MockOutcome<Decision>>, fallback: MockOutcome<Decision>) -> Self {
        Self {
            id: "mock-decision".into(),
            delay: Duration::ZERO,
            script: Mutex::new(script.into()),
            fallback,
            calls: AtomicUsize::new(0),
            inputs: Mutex::new(vec![]),
        }
    }
    pub fn with_delay(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    pub fn inputs(&self) -> Vec<DecisionInput> {
        self.inputs.lock().unwrap().clone()
    }
}

#[async_trait]
impl DecisionModel for MockDecision {
    fn id(&self) -> &str {
        &self.id
    }
    async fn decide(&self, input: &DecisionInput) -> Result<Decision> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(input.clone());
        let outcome = self.script.lock().unwrap().pop_front().unwrap_or_else(|| self.fallback.clone());
        tokio::time::sleep(self.delay).await;
        outcome.into_result(self.delay.as_millis() as u64)
    }
}

/// A typical "question that needs a fast answer" decision.
pub fn decision_answer(intent: Intent, profile: &str) -> Decision {
    Decision {
        needs_answer: Scored::new(true, 0.93),
        intent: Scored::new(intent, 0.88),
        answer_tier: Scored::new(Tier::Fast, 0.8),
        profile: Scored::new(profile.into(), 0.75),
        urgency: Scored::new(Urgency::High, 0.7),
    }
}

/// Small talk: no answer.
pub fn decision_no_answer(profile: &str) -> Decision {
    Decision {
        needs_answer: Scored::new(false, 0.96),
        intent: Scored::new(Intent::SmallTalk, 0.9),
        answer_tier: Scored::new(Tier::None, 0.94),
        profile: Scored::new(profile.into(), 0.5),
        urgency: Scored::new(Urgency::Low, 0.9),
    }
}

// ---------- answer model ----------

/// Streams scripted tokens: waits `first_token_delay`, then one token per `token_delay`.
/// `fail_before_first_token` returns that error instead (after the delay).
pub struct MockAnswer {
    pub id: String,
    pub cloud: bool,
    pub tokens: Vec<String>,
    pub first_token_delay: Duration,
    pub token_delay: Duration,
    pub fail_before_first_token: Option<MockOutcome<()>>,
    calls: AtomicUsize,
    prompts: Mutex<Vec<Prompt>>,
}

impl MockAnswer {
    pub fn new(id: &str, cloud: bool, tokens: &[&str]) -> Self {
        Self {
            id: id.into(),
            cloud,
            tokens: tokens.iter().map(|t| (*t).to_string()).collect(),
            first_token_delay: Duration::from_millis(300),
            token_delay: Duration::from_millis(10),
            fail_before_first_token: None,
            calls: AtomicUsize::new(0),
            prompts: Mutex::new(vec![]),
        }
    }
    /// A cue answer in the `contracts/prompts` output grammar.
    pub fn cue_answer(id: &str, cloud: bool) -> Self {
        Self::new(id, cloud, &["1. Requirements\n", "2. Design\n", "3. Build\n", "---\n", "1: Interview users first\n"])
    }
    pub fn failing(mut self, outcome: MockOutcome<()>) -> Self {
        self.fail_before_first_token = Some(outcome);
        self
    }
    pub fn with_first_token_delay(mut self, d: Duration) -> Self {
        self.first_token_delay = d;
        self
    }
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    pub fn prompts(&self) -> Vec<Prompt> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl AnswerModel for MockAnswer {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_cloud(&self) -> bool {
        self.cloud
    }
    async fn stream(&self, prompt: &Prompt, tx: mpsc::Sender<Token>, cancel: CancellationToken) -> Result<StreamStats> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.prompts.lock().unwrap().push(prompt.clone());
        let start = tokio::time::Instant::now();
        tokio::select! {
            _ = cancel.cancelled() => return Err(SscError::Cancelled),
            _ = tokio::time::sleep(self.first_token_delay) => {}
        }
        if let Some(f) = &self.fail_before_first_token {
            f.clone().into_result(self.first_token_delay.as_millis() as u64)?;
        }
        let mut first = None;
        let mut n = 0;
        for (i, t) in self.tokens.iter().enumerate() {
            if i > 0 {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(SscError::Cancelled),
                    _ = tokio::time::sleep(self.token_delay) => {}
                }
            }
            if cancel.is_cancelled() {
                return Err(SscError::Cancelled);
            }
            first.get_or_insert(start.elapsed().as_millis() as u64);
            if tx.send(Token { text: t.clone() }).await.is_err() {
                return Err(SscError::Cancelled);
            }
            n += 1;
        }
        Ok(StreamStats {
            model: self.id.clone(),
            first_token_ms: first,
            total_ms: start.elapsed().as_millis() as u64,
            tokens_out: n,
        })
    }
}

// ---------- embedder ----------

/// Deterministic bag-of-words embedder (FNV-1a hashed into `dim` buckets, L2-normalised).
/// Texts sharing words have positive cosine similarity; good enough for retrieval tests.
pub struct MockEmbedder {
    pub dim: usize,
}

impl Default for MockEmbedder {
    fn default() -> Self {
        Self { dim: 384 }
    }
}

fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

impl Embedder for MockEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; self.dim];
                for w in t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
                    v[(fnv1a(&w.to_lowercase()) % self.dim as u64) as usize] += 1.0;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    v.iter_mut().for_each(|x| *x /= norm);
                }
                v
            })
            .collect())
    }
}

// ---------- memory ----------

/// In-memory store. `search` scores chunks by the number of shared lowercase words.
pub struct MockMemory {
    pub clock: Arc<dyn Clock>,
    items: Mutex<Vec<MemoryItem>>,
    /// (profile_id, chunk)
    chunks: Mutex<Vec<(String, Chunk)>>,
}

impl Default for MockMemory {
    fn default() -> Self {
        Self { clock: Arc::new(SystemClock), items: Mutex::new(vec![]), chunks: Mutex::new(vec![]) }
    }
}

impl MockMemory {
    pub fn add_chunk(&self, profile_id: &str, doc_name: &str, text: &str) {
        self.chunks.lock().unwrap().push((
            profile_id.into(),
            Chunk {
                id: Uuid::new_v4(),
                doc_id: Uuid::new_v4(),
                doc_name: doc_name.into(),
                text: text.into(),
                score: 0.0,
            },
        ));
    }
}

fn words(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2).map(str::to_lowercase).collect()
}

impl MemoryStore for MockMemory {
    fn list(&self, layer: Option<Layer>) -> Result<Vec<MemoryItem>> {
        Ok(self.items.lock().unwrap().iter().filter(|i| layer.is_none_or(|l| i.layer == l)).cloned().collect())
    }
    fn upsert(&self, edit: MemoryEdit) -> Result<MemoryItem> {
        let mut items = self.items.lock().unwrap();
        let now = self.clock.now_ms();
        if let Some(id) = edit.id {
            let item = items.iter_mut().find(|i| i.id == id).ok_or(SscError::NotFound)?;
            item.layer = edit.layer;
            item.title = edit.title;
            item.body = edit.body;
            item.updated_at = now;
            return Ok(item.clone());
        }
        let item =
            MemoryItem { id: Uuid::new_v4(), layer: edit.layer, title: edit.title, body: edit.body, updated_at: now };
        items.push(item.clone());
        Ok(item)
    }
    fn delete(&self, id: Uuid) -> Result<()> {
        let mut items = self.items.lock().unwrap();
        let before = items.len();
        items.retain(|i| i.id != id);
        if items.len() == before { Err(SscError::NotFound) } else { Ok(()) }
    }
    fn search(&self, query: &str, profile_id: &str, k: usize) -> Result<Vec<Chunk>> {
        let q = words(query);
        let mut hits: Vec<Chunk> = self
            .chunks
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == profile_id)
            .filter_map(|(_, c)| {
                let score = words(&c.text).intersection(&q).count() as f32;
                (score > 0.0).then(|| Chunk { score, ..c.clone() })
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        Ok(hits)
    }
}

// ---------- transport ----------

/// One end of an in-memory link. `MockTransport::pair()` returns two connected ends.
/// Assigns `seq` from 1 upward per direction and records everything sent.
pub struct MockTransport {
    clock: Arc<dyn Clock>,
    seq: AtomicU64,
    peer: mpsc::Sender<Envelope>,
    incoming: Mutex<Option<mpsc::Receiver<Envelope>>>,
    sent: Mutex<Vec<Envelope>>,
}

impl MockTransport {
    pub fn pair() -> (Self, Self) {
        Self::pair_with_clock(Arc::new(SystemClock))
    }
    pub fn pair_with_clock(clock: Arc<dyn Clock>) -> (Self, Self) {
        let (tx_a, rx_a) = mpsc::channel(1024);
        let (tx_b, rx_b) = mpsc::channel(1024);
        let mk = |peer, inc| Self {
            clock: clock.clone(),
            seq: AtomicU64::new(0),
            peer,
            incoming: Mutex::new(Some(inc)),
            sent: Mutex::new(vec![]),
        };
        (mk(tx_b, rx_a), mk(tx_a, rx_b))
    }
    pub fn sent(&self) -> Vec<Envelope> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn send(&self, session_id: Option<Uuid>, message: Message) -> Result<()> {
        let env = Envelope::new(self.seq.fetch_add(1, Ordering::SeqCst) + 1, self.clock.now_ms(), session_id, message);
        // Round-trip through the schema so mocks catch contract violations too.
        let env = Envelope::from_slice(&env.to_bytes()).map_err(|e| SscError::Invalid(e.to_string()))?;
        self.sent.lock().unwrap().push(env.clone());
        self.peer.send(env).await.map_err(|_| SscError::Unavailable("peer closed".into()))
    }
    fn take_incoming(&self) -> Option<mpsc::Receiver<Envelope>> {
        self.incoming.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Heartbeat, Speaker};

    #[tokio::test(start_paused = true)]
    async fn answer_mock_streams_and_cancels() {
        let m = MockAnswer::cue_answer("fast", true);
        let (tx, mut rx) = mpsc::channel(16);
        let p = Prompt {
            prefix: "p".into(),
            dynamic: "d".into(),
            max_tokens: 40,
            temperature: 0.2,
            stop: vec![],
            images: vec![],
        };
        let stats = m.stream(&p, tx, CancellationToken::new()).await.unwrap();
        assert_eq!(stats.tokens_out, 5);
        assert_eq!(stats.first_token_ms, Some(300));
        assert_eq!(rx.recv().await.unwrap().text, "1. Requirements\n");

        let cancel = CancellationToken::new();
        cancel.cancel();
        let (tx, _rx) = mpsc::channel(16);
        assert!(matches!(m.stream(&p, tx, cancel).await, Err(SscError::Cancelled)));
    }

    #[tokio::test(start_paused = true)]
    async fn answer_mock_fails_before_first_token() {
        let m = MockAnswer::cue_answer("groq", true).failing(MockOutcome::RateLimited);
        let (tx, mut rx) = mpsc::channel(16);
        let p = Prompt {
            prefix: String::new(),
            dynamic: String::new(),
            max_tokens: 40,
            temperature: 0.0,
            stop: vec![],
            images: vec![],
        };
        assert!(matches!(m.stream(&p, tx, CancellationToken::new()).await, Err(SscError::RateLimited { .. })));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn decision_mock_script_then_fallback() {
        let d = MockDecision::scripted(vec![MockOutcome::Timeout], MockOutcome::Ok(decision_no_answer("p")));
        let input = DecisionInput { question: "q".into(), screen_diff: String::new(), profiles: vec!["p".into()] };
        assert!(matches!(d.decide(&input).await, Err(SscError::Timeout(_))));
        assert!(!d.decide(&input).await.unwrap().needs_answer.value);
        assert_eq!(d.calls(), 2);
    }

    #[tokio::test]
    async fn transport_pair_delivers_with_increasing_seq() {
        let (pc, phone) = MockTransport::pair();
        let mut rx = phone.take_incoming().unwrap();
        assert!(phone.take_incoming().is_none());
        pc.send(None, Message::Heartbeat(Heartbeat::default())).await.unwrap();
        pc.send(None, Message::Heartbeat(Heartbeat { echo: Some(5) })).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().seq, 1);
        assert_eq!(rx.recv().await.unwrap().seq, 2);
        assert_eq!(pc.sent().len(), 2);
    }

    #[tokio::test]
    async fn transport_rejects_contract_violations() {
        let (pc, _phone) = MockTransport::pair();
        let bad = Message::AnswerDelta(crate::protocol::AnswerDelta {
            answer_id: Uuid::new_v4(),
            block: crate::protocol::Block::Cue,
            text: String::new(), // schema: minLength 1
        });
        assert!(matches!(pc.send(None, bad).await, Err(SscError::Invalid(_))));
    }

    #[test]
    fn embedder_is_deterministic_and_normalised() {
        let e = MockEmbedder::default();
        let v = e.embed(&["enterprise plan price".into(), "enterprise plan price".into(), "weather".into()]).unwrap();
        assert_eq!(v[0], v[1]);
        let norm: f32 = v[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        let dot: f32 = v[0].iter().zip(&v[2]).map(|(a, b)| a * b).sum();
        assert!(dot.abs() < 1e-6);
    }

    #[test]
    fn memory_mock_crud_and_search() {
        let m = MockMemory::default();
        let item =
            m.upsert(MemoryEdit { id: None, layer: Layer::Contact, title: "Ada".into(), body: "SSO".into() }).unwrap();
        assert_eq!(m.list(Some(Layer::Contact)).unwrap().len(), 1);
        assert_eq!(m.list(Some(Layer::Profile)).unwrap().len(), 0);
        m.delete(item.id).unwrap();
        assert!(matches!(m.delete(item.id), Err(SscError::NotFound)));

        m.add_chunk("sales", "pricing.pdf", "Enterprise plan costs 45000 per seat");
        m.add_chunk("sales", "faq.md", "Support hours are 9 to 5");
        m.add_chunk("other", "x.md", "Enterprise plan elsewhere");
        let hits = m.search("how much is the enterprise plan", "sales", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_name, "pricing.pdf");
    }

    #[test]
    fn screen_mock_repeats_last() {
        let s = MockScreen::new(vec![MockScreen::snapshot("Zoom", &["a"]), MockScreen::snapshot("Zoom", &["a", "b"])]);
        assert_eq!(s.snapshot().unwrap().nodes.len(), 1);
        assert_eq!(s.snapshot().unwrap().nodes.len(), 2);
        assert_eq!(s.snapshot().unwrap().text(), "a\nb");
        let _ = Speaker::Them;
    }
}
