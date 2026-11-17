//! The running agent: sessions, capture loops, answer jobs, phone commands and post-call.
//!
//! Concurrency model: every long-running piece is a tokio task tied to a `CancellationToken`.
//! - pipeline lifetime: the phone command loop.
//! - session lifetime (child token): audio/STT, screen polling, the trigger engine and its event
//!   loop, and every answer started during the session.
//!
//! Each answer runs in its own task, so a slow model never blocks the hotkey or new triggers.

use crate::limits;
use crate::postcall::{self, ShownAnswer};
use ssc_context::{Budget, ContextInput, PromptKind, assemble, retrieve_and_assemble};
use ssc_core::config::Config;
use ssc_core::decision::Tier;
use ssc_core::protocol::{
    AnswerDelta, AnswerEnd, AnswerEndReason, AnswerFormat, AnswerStart, Block, Command, Cue, FeedbackItem, Intent,
    Layer, MemorySnapshot, Message, Mode, SessionEnd, SessionEndReason, SessionStart, SessionSummary,
    TranscriptPartial, Trigger, Urgency,
};
use ssc_core::timing::{Stage, Timing};
use ssc_core::traits::{
    AnswerModel, AudioSource, Clock, DecisionModel, MemoryStore, ScreenSource, SttEngine, Transport,
};
use ssc_core::types::{Prompt, ScreenSnapshot, TranscriptSegment};
use ssc_core::{Result, SscError};
use ssc_memory::{AnswerRecord, SqliteMemory};
use ssc_router::grammar::{ParsedAnswer, merge_detail};
use ssc_router::{AnswerRouter, RoutedDelta};
use ssc_trigger::{TriggerEngine, TriggerEvent, TriggerInput, TriggerSettings, format_for};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub type RttFn = Arc<dyn Fn() -> Option<u64> + Send + Sync>;
pub type GateFn = Arc<dyn Fn() -> bool + Send + Sync>;
pub type Hook = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Every component the pipeline drives. Real ones come from [`crate::system`]; tests use mocks.
pub struct Parts {
    pub screen: Option<Arc<dyn ScreenSource>>,
    /// Mic ("me") and system audio ("them"). All feed one STT engine, which splits by speaker.
    pub audio: Vec<Arc<dyn AudioSource>>,
    pub stt: Option<Arc<dyn SttEngine>>,
    /// What the trigger engine calls (Jev → Tev1 through [`crate::RoutedDecisions`]).
    pub decision: Arc<dyn DecisionModel>,
    /// True while cloud decisions gate answers (`DecisionRouter::decisions_gate_answers`). When
    /// false in cloud mode (offline), questions fire from the end-of-question rule and the
    /// decision model only tags them.
    pub decisions_gate: GateFn,
    pub answers: Arc<AnswerRouter>,
    /// Summary / follow-up model.
    pub postcall: Arc<dyn AnswerModel>,
    /// Runs before a local post-call model starts (unload the live models to free memory).
    pub before_postcall: Option<Hook>,
    pub memory: Arc<SqliteMemory>,
    pub transport: Arc<dyn Transport>,
    /// Last heartbeat round trip (for `phone_rendered`).
    pub rtt_ms: RttFn,
    pub clock: Arc<dyn Clock>,
    pub timing: Arc<Timing>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub config: Config,
    pub screen_poll: Duration,
    /// Per post-call model call.
    pub postcall_timeout: Duration,
    /// Doc chunks retrieved per answer (spec: 3–5).
    pub retrieve_k: usize,
}

impl Settings {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            screen_poll: Duration::from_millis(1000),
            postcall_timeout: Duration::from_secs(180),
            retrieve_k: 5,
        }
    }
}

/// What a finished (or in-flight) answer needs for phone commands.
#[derive(Debug, Clone)]
struct AnswerMeta {
    question: String,
    format: AnswerFormat,
    intent: Intent,
    profile: String,
    cues: Vec<Cue>,
}

#[derive(Debug, Clone)]
struct Job {
    answer_id: Uuid,
    trigger: Trigger,
    question: String,
    format: AnswerFormat,
    intent: Intent,
    urgency: Urgency,
    tier: Tier,
    profile: String,
    kind: PromptKind,
    /// "Shorter": cap the answer at 3 cues, no detail block.
    shorter: bool,
    /// "Expand all": the cues being expanded.
    expand: Option<Vec<Cue>>,
}

impl Job {
    fn from_event(ev: TriggerEvent) -> Self {
        Self {
            answer_id: ev.answer_id,
            trigger: ev.trigger,
            question: ev.question,
            format: ev.format,
            intent: ev.intent,
            urgency: ev.urgency,
            tier: ev.tier,
            profile: ev.profile,
            kind: PromptKind::for_format(ev.format),
            shorter: false,
            expand: None,
        }
    }
}

struct Session {
    id: Uuid,
    profile: String,
    profiles: Vec<String>,
    cancel: CancellationToken,
    engine: Arc<TriggerEngine>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

#[derive(Default)]
struct Live {
    session_id: Option<Uuid>,
    profile: String,
    engine: Option<Arc<TriggerEngine>>,
    /// Session lifetime token; answers outside a session use the pipeline token.
    session_cancel: Option<CancellationToken>,
    screen_diff: String,
    /// Final segments of the current session, chronological.
    transcript: Vec<TranscriptSegment>,
    shown: Vec<ShownAnswer>,
}

struct Inner {
    parts: Parts,
    settings: Settings,
    session: tokio::sync::Mutex<Option<Session>>,
    live: Mutex<Live>,
    answers: Mutex<HashMap<Uuid, AnswerMeta>>,
    /// answer_id → `frame_sent` of its first delta (for `phone_rendered`).
    first_frame: Mutex<HashMap<Uuid, u64>>,
    cancel: CancellationToken,
}

/// Cheap to clone; all clones drive the same agent.
#[derive(Clone)]
pub struct Pipeline {
    inner: Arc<Inner>,
}

const SHORTER_NOTE: &str = "\nThe user tapped \"shorter\": output exactly 3 cues and no \"---\" detail block.\n";
const TRANSCRIPT_CONTEXT_SEGMENTS: usize = 200;

fn log(msg: impl AsRef<str>) {
    eprintln!("[ssc-pipeline] {}", msg.as_ref());
}

impl Pipeline {
    /// Starts the phone command loop. Capture starts with [`Pipeline::start_session`].
    pub fn new(parts: Parts, settings: Settings) -> Self {
        parts.memory.set_keep_transcripts(settings.config.keep_transcripts);
        let incoming = parts.transport.take_incoming();
        let inner = Arc::new(Inner {
            parts,
            settings,
            session: tokio::sync::Mutex::new(None),
            live: Mutex::new(Live::default()),
            answers: Mutex::new(HashMap::new()),
            first_frame: Mutex::new(HashMap::new()),
            cancel: CancellationToken::new(),
        });
        match incoming {
            Some(rx) => {
                tokio::spawn(command_loop(inner.clone(), rx));
            }
            None => log("transport incoming stream already taken: phone commands disabled"),
        }
        Self { inner }
    }

    pub fn timing(&self) -> Arc<Timing> {
        self.inner.parts.timing.clone()
    }

    pub fn mode(&self) -> Mode {
        self.inner.settings.config.mode
    }

    pub fn session_id(&self) -> Option<Uuid> {
        self.inner.live.lock().unwrap().session_id
    }

    pub fn is_capturing(&self) -> bool {
        self.session_id().is_some()
    }

    /// Debug panel: p50/p95 per stage.
    pub fn stage_stats(&self) -> Vec<crate::StageStat> {
        crate::stage_stats(&self.inner.parts.timing.events())
    }

    /// Start a call session: `session.start` to the phone, then capture → trigger → answers.
    /// `profile`: a profile id; `None` = the first profile.
    pub async fn start_session(&self, profile: Option<String>) -> Result<Uuid> {
        let inner = &self.inner;
        let mut slot = inner.session.lock().await;
        if let Some(s) = slot.as_ref() {
            return Err(SscError::Invalid(format!("session {} already running", s.id)));
        }
        let mem = inner.parts.memory.clone();
        let stored: Vec<String> = blocking(move || mem.profiles()).await?.into_iter().map(|p| p.id).collect();
        let active = profile.or_else(|| stored.first().cloned()).unwrap_or_else(|| "default".into());
        let profiles = if stored.is_empty() { vec![active.clone()] } else { stored.clone() };
        let id = Uuid::new_v4();
        let mode = inner.settings.config.mode;
        let mem = inner.parts.memory.clone();
        let db_profile = stored.contains(&active).then(|| active.clone());
        blocking(move || mem.start_session(id, db_profile.as_deref(), mode)).await?;

        let cancel = inner.cancel.child_token();
        let settings = TriggerSettings::from_config(&inner.settings.config, profiles.clone(), active.clone());
        let engine = Arc::new(
            TriggerEngine::new(settings, inner.parts.decision.clone(), inner.parts.clock.clone())
                .with_timing(inner.parts.timing.clone()),
        );
        sync_trigger_mode(inner, &engine);
        {
            let mut live = inner.live.lock().unwrap();
            *live = Live {
                session_id: Some(id),
                profile: active.clone(),
                engine: Some(engine.clone()),
                session_cancel: Some(cancel.clone()),
                ..Live::default()
            };
        }
        send(inner, Some(id), Message::SessionStart(SessionStart { session_id: id, profile_id: active.clone(), mode }))
            .await;

        let mut tasks = Vec::new();
        let (in_tx, in_rx) = mpsc::channel::<TriggerInput>(256);
        let (ev_tx, ev_rx) = mpsc::channel::<TriggerEvent>(64);
        tasks.push(engine.clone().spawn(in_rx, ev_tx, cancel.clone()));
        tasks.push(tokio::spawn(event_loop(inner.clone(), ev_rx, cancel.clone())));

        if let Some(stt) = inner.parts.stt.clone()
            && !inner.parts.audio.is_empty()
        {
            let (atx, arx) = mpsc::channel(256);
            for src in &inner.parts.audio {
                if let Err(e) = src.start(atx.clone(), cancel.clone()) {
                    log(format!("audio source failed to start: {e}"));
                }
            }
            drop(atx);
            let (stx, srx) = mpsc::channel(256);
            match stt.start(arx, stx, cancel.clone()) {
                Ok(()) => tasks.push(tokio::spawn(transcript_loop(inner.clone(), srx, in_tx.clone(), cancel.clone()))),
                Err(e) => log(format!("speech-to-text failed to start: {e}")),
            }
        }
        if let Some(screen) = inner.parts.screen.clone() {
            tasks.push(tokio::spawn(screen_loop(inner.clone(), screen, in_tx.clone(), cancel.clone())));
        }
        drop(in_tx);

        *slot = Some(Session { id, profile: active, profiles, cancel, engine, tasks });
        Ok(id)
    }

    /// The "answer now" hotkey. Never waits on the decision model; returns the answer id, or
    /// `None` when no session is running.
    pub fn answer_now(&self) -> Option<Uuid> {
        let (engine, cancel) = {
            let live = self.inner.live.lock().unwrap();
            (live.engine.clone()?, live.session_cancel.clone()?)
        };
        let ev = engine.hotkey();
        let id = ev.answer_id;
        spawn_job(&self.inner, Job::from_event(ev), cancel);
        Some(id)
    }

    /// End the session: stop capture, `session.end`, then summary + follow-up → `session.summary`.
    /// Returns `None` if no session was running.
    pub async fn end_session(&self, reason: SessionEndReason) -> Result<Option<SessionSummary>> {
        let inner = &self.inner;
        let Some(session) = inner.session.lock().await.take() else { return Ok(None) };
        session.cancel.cancel();
        for t in session.tasks {
            let _ = t.await;
        }
        let (transcript, shown) = {
            let mut live = inner.live.lock().unwrap();
            let t = std::mem::take(&mut live.transcript);
            let s = std::mem::take(&mut live.shown);
            *live = Live::default();
            (t, s)
        };
        let _ = (&session.profiles, &session.engine);
        send(inner, Some(session.id), Message::SessionEnd(SessionEnd { session_id: session.id, reason })).await;

        let parsed = match post_call(inner, &session.profile, &transcript, &shown).await {
            Ok(p) => p,
            Err(e) => {
                log(format!("post-call summary failed: {e}"));
                postcall::ParsedSummary {
                    outcome: ssc_memory::SessionOutcome {
                        summary: format!("Summary unavailable ({e})."),
                        ..Default::default()
                    },
                    contacts: vec![],
                }
            }
        };
        let mem = inner.parts.memory.clone();
        let sid = session.id;
        let summary = blocking(move || mem.end_session(sid, &parsed.outcome, &parsed.contacts))
            .await
            .map(limits::session_summary)?;
        send(inner, Some(session.id), Message::SessionSummary(summary.clone())).await;
        Ok(Some(summary))
    }

    /// Stop the session without a summary (benches, errors). Nothing is stored for it beyond what
    /// was already written; the session row stays without `ended_at`.
    pub async fn discard_session(&self) {
        let inner = &self.inner;
        let Some(session) = inner.session.lock().await.take() else { return };
        session.cancel.cancel();
        for t in session.tasks {
            let _ = t.await;
        }
        *inner.live.lock().unwrap() = Live::default();
        let end = SessionEnd { session_id: session.id, reason: SessionEndReason::Error };
        send(inner, Some(session.id), Message::SessionEnd(end)).await;
    }

    /// Send the current memory items to the phone (`memory.snapshot`).
    pub async fn push_memory_snapshot(&self) {
        snapshot(&self.inner, None).await;
    }

    /// Stop everything (session tasks, answers, command loop).
    pub async fn shutdown(&self) {
        let _ = self.end_session(SessionEndReason::User).await;
        self.inner.cancel.cancel();
    }
}

// ---------- helpers ----------

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| SscError::Other(anyhow::anyhow!("blocking task: {e}")))?
}

/// Sends to the phone. A missing phone is normal (the agent works without one); other errors
/// are logged, never fatal.
async fn send(inner: &Inner, session_id: Option<Uuid>, msg: Message) -> bool {
    match inner.parts.transport.send(session_id, msg).await {
        Ok(()) => true,
        Err(SscError::Unavailable(_)) => false,
        Err(e) => {
            log(format!("send failed: {e}"));
            false
        }
    }
}

fn record(inner: &Inner, id: Uuid, stage: Stage) -> u64 {
    let t = inner.parts.clock.now_ms();
    inner.parts.timing.record(id, stage, t);
    t
}

/// Trigger mode follows the user's mode and, in cloud mode, whether Jev currently gates answers.
fn sync_trigger_mode(inner: &Inner, engine: &TriggerEngine) {
    let want = if inner.settings.config.mode == Mode::Cloud && (inner.parts.decisions_gate)() {
        Mode::Cloud
    } else {
        Mode::Local
    };
    if engine.mode() != want {
        engine.set_mode(want);
    }
}

// ---------- capture loops ----------

async fn transcript_loop(
    inner: Arc<Inner>,
    mut rx: mpsc::Receiver<TranscriptSegment>,
    trig: mpsc::Sender<TriggerInput>,
    cancel: CancellationToken,
) {
    loop {
        let seg = tokio::select! {
            _ = cancel.cancelled() => break,
            s = rx.recv() => match s { Some(s) => s, None => break },
        };
        let (sid, engine) = {
            let live = inner.live.lock().unwrap();
            (live.session_id, live.engine.clone())
        };
        if let Some(e) = &engine {
            sync_trigger_mode(&inner, e);
        }
        let text = seg.text.trim();
        if !text.is_empty() {
            let tp = TranscriptPartial {
                speaker: seg.speaker,
                text: limits::clip(text, 4096),
                is_final: seg.is_final,
                t0: seg.t0_ms,
                t1: seg.t1_ms,
            };
            send(&inner, sid, Message::TranscriptPartial(tp)).await;
        }
        if seg.is_final && !text.is_empty() {
            inner.live.lock().unwrap().transcript.push(seg.clone());
            if let Some(sid) = sid {
                let (mem, s) = (inner.parts.memory.clone(), seg.clone());
                if let Err(e) = blocking(move || mem.record_transcript(sid, &s)).await {
                    log(format!("transcript store failed: {e}"));
                }
            }
        }
        if trig.send(TriggerInput::Transcript(seg)).await.is_err() {
            break;
        }
    }
}

async fn screen_loop(
    inner: Arc<Inner>,
    screen: Arc<dyn ScreenSource>,
    trig: mpsc::Sender<TriggerInput>,
    cancel: CancellationToken,
) {
    let mut prev: Option<ScreenSnapshot> = None;
    let mut tick = tokio::time::interval(inner.settings.screen_poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {}
        }
        let s = screen.clone();
        let next = match tokio::task::spawn_blocking(move || s.snapshot()).await {
            Ok(Ok(n)) => n,
            // No focused window / permission missing: try again next tick.
            _ => continue,
        };
        let diff = ssc_screen::diff(prev.as_ref(), &next);
        let first = prev.is_none();
        prev = Some(next);
        if diff.is_empty() {
            continue;
        }
        inner.live.lock().unwrap().screen_diff = diff.text();
        // The first snapshot is the baseline: context, not a "big screen change".
        if !first && trig.send(TriggerInput::Screen(diff)).await.is_err() {
            break;
        }
    }
}

async fn event_loop(inner: Arc<Inner>, mut rx: mpsc::Receiver<TriggerEvent>, cancel: CancellationToken) {
    loop {
        let ev = tokio::select! {
            _ = cancel.cancelled() => break,
            e = rx.recv() => match e { Some(e) => e, None => break },
        };
        spawn_job(&inner, Job::from_event(ev), cancel.clone());
    }
}

// ---------- answers ----------

fn spawn_job(inner: &Arc<Inner>, job: Job, cancel: CancellationToken) {
    let inner = inner.clone();
    tokio::spawn(async move { run_answer(inner, job, cancel).await });
}

fn numbered_cues(cues: &[String]) -> String {
    cues.iter().enumerate().map(|(i, c)| format!("{}. {c}", i + 1)).collect::<Vec<_>>().join("\n")
}

async fn run_answer(inner: Arc<Inner>, job: Job, cancel: CancellationToken) {
    let id = job.answer_id;
    let (sid, transcript, screen) = {
        let live = inner.live.lock().unwrap();
        let n = live.transcript.len().saturating_sub(TRANSCRIPT_CONTEXT_SEGMENTS);
        (live.session_id, live.transcript[n..].to_vec(), live.screen_diff.clone())
    };
    let mode = inner.settings.config.mode;
    let budget = if mode == Mode::Local { Budget::local() } else { Budget::cloud() };

    // 4. Context.
    let mem = inner.parts.memory.clone();
    let (profile_id, kind, k) = (job.profile.clone(), job.kind, inner.settings.retrieve_k);
    let mut input = ContextInput {
        transcript,
        screen,
        question: job.question.clone(),
        now_ms: inner.parts.clock.now_ms(),
        answer: job
            .expand
            .as_ref()
            .map(|c| numbered_cues(&c.iter().map(|c| c.text.clone()).collect::<Vec<_>>()))
            .unwrap_or_default(),
        ..Default::default()
    };
    let assembled = blocking(move || {
        input.about_me = mem.about_me()?;
        input.profile = mem.profile(&profile_id)?;
        let a = retrieve_and_assemble(&*mem, &profile_id, k, kind, input.clone(), &budget)?;
        Ok((a, input))
    })
    .await;
    let (assembled, input) = match assembled {
        Ok(v) => v,
        Err(e) => {
            log(format!("context assembly failed: {e}"));
            finish(&inner, &job, sid, error_end(id), String::new()).await;
            return;
        }
    };
    record(&inner, id, Stage::ContextReady);

    let mut prompt: Prompt = assembled.prompt;
    if job.shorter {
        prompt.dynamic.push_str(SHORTER_NOTE);
        prompt.max_tokens = prompt.max_tokens.min(60);
    }
    let model = inner.parts.answers.models().first().map(|m| m.id().to_string()).unwrap_or_else(|| "unknown".into());
    let start = AnswerStart {
        answer_id: id,
        trigger: job.trigger,
        format: job.format,
        intent: job.intent,
        urgency: job.urgency,
        model: limits::clip(&model, 128),
    };
    send(&inner, sid, Message::AnswerStart(start)).await;

    // 5. Route. The strong tier's detail prompt reuses the retrieved chunks.
    let detail_input = ContextInput { chunks: assembled.chunks_used.clone(), ..input };
    let detail = move |cues: &[String]| {
        let mut i = detail_input;
        i.answer = numbered_cues(cues);
        assemble(PromptKind::Expand, &i, &budget).prompt
    };
    let (dtx, mut drx) = mpsc::channel::<RoutedDelta>(64);
    let router = inner.parts.answers.clone();
    let (tier, c2) = (if job.shorter { Tier::Fast } else { job.tier }, cancel.clone());
    record(&inner, id, Stage::ModelRequest);
    let run = tokio::spawn(async move { router.answer(tier, &prompt, detail, dtx, &c2).await });

    // 6. Push.
    let mut first = true;
    let mut detail_text = String::new();
    while let Some(d) = drx.recv().await {
        if first {
            record(&inner, id, Stage::FirstToken);
        }
        if d.block == Block::Detail {
            detail_text.push_str(&d.text);
        }
        for text in limits::split_chars(&d.text, 4096) {
            send(&inner, sid, Message::AnswerDelta(AnswerDelta { answer_id: id, block: d.block, text })).await;
        }
        if first {
            let t = record(&inner, id, Stage::FrameSent);
            inner.first_frame.lock().unwrap().insert(id, t);
            first = false;
        }
    }
    let (end, model) = match run.await {
        Ok(Ok(outcome)) => {
            let parsed = match &job.expand {
                // Expand all: the original cues with every expansion filled in.
                Some(cues) => merge_detail(ParsedAnswer { cues: cues.clone(), ..Default::default() }, &detail_text),
                None => outcome.parsed,
            };
            (parsed.into_answer_end(id, AnswerEndReason::Done), outcome.fast.model)
        }
        Ok(Err(SscError::Cancelled)) => (end_with(id, AnswerEndReason::Cancelled), model),
        Ok(Err(e)) => {
            log(format!("answer {id} failed: {e}"));
            (error_end(id), model)
        }
        Err(e) => {
            log(format!("answer task {id} panicked: {e}"));
            (error_end(id), model)
        }
    };
    finish(&inner, &job, sid, end, model).await;
}

fn end_with(id: Uuid, reason: AnswerEndReason) -> AnswerEnd {
    AnswerEnd { answer_id: id, reason, cues: vec![], fact: None, source_doc: None, not_in_docs: false }
}

fn error_end(id: Uuid) -> AnswerEnd {
    end_with(id, AnswerEndReason::Error)
}

/// `answer.end`, then remember and store the answer.
async fn finish(inner: &Arc<Inner>, job: &Job, sid: Option<Uuid>, end: AnswerEnd, model: String) {
    let end = limits::answer_end(end);
    send(inner, sid, Message::AnswerEnd(end.clone())).await;
    if end.reason != AnswerEndReason::Done {
        return;
    }
    inner.answers.lock().unwrap().insert(
        job.answer_id,
        AnswerMeta {
            question: job.question.clone(),
            format: job.format,
            intent: job.intent,
            profile: job.profile.clone(),
            cues: end.cues.clone(),
        },
    );
    {
        let mut live = inner.live.lock().unwrap();
        if live.session_id.is_some() && live.session_id == sid {
            live.shown.push(ShownAnswer {
                question: job.question.clone(),
                cues: end.cues.iter().map(|c| c.text.clone()).collect(),
            });
        }
    }
    if let Some(session_id) = sid {
        let rec = AnswerRecord {
            id: job.answer_id,
            session_id,
            trigger: job.trigger,
            format: job.format,
            intent: job.intent,
            model: if model.is_empty() { "unknown".into() } else { model },
            question: job.question.clone(),
            end,
        };
        let mem = inner.parts.memory.clone();
        if let Err(e) = blocking(move || mem.record_answer(&rec)).await {
            log(format!("answer store failed: {e}"));
        }
    }
}

// ---------- phone commands ----------

async fn command_loop(inner: Arc<Inner>, mut rx: mpsc::Receiver<ssc_core::protocol::Envelope>) {
    loop {
        let env = tokio::select! {
            _ = inner.cancel.cancelled() => break,
            e = rx.recv() => match e { Some(e) => e, None => break },
        };
        if let Message::Command(c) = env.message {
            // Each command in its own task: an answer it starts must not block the next command.
            let inner = inner.clone();
            tokio::spawn(async move { handle_command(inner, c).await });
        }
    }
}

fn meta(inner: &Inner, id: Uuid) -> Option<AnswerMeta> {
    inner.answers.lock().unwrap().get(&id).cloned()
}

fn command_job(inner: &Inner, m: Option<&AnswerMeta>, question: String) -> Job {
    let profile = m.map(|m| m.profile.clone()).unwrap_or_else(|| inner.live.lock().unwrap().profile.clone());
    let profile = if profile.is_empty() { "default".into() } else { profile };
    let (format, intent) = match m {
        Some(m) => (m.format, m.intent),
        None => {
            let intent = ssc_trigger::heuristics::heuristic_intent(&question);
            (format_for(intent).unwrap_or(AnswerFormat::Cue), intent)
        }
    };
    Job {
        answer_id: Uuid::new_v4(),
        trigger: Trigger::Command,
        question,
        format,
        intent,
        urgency: Urgency::High,
        tier: Tier::Fast,
        profile,
        kind: PromptKind::for_format(format),
        shorter: false,
        expand: None,
    }
}

fn answer_cancel(inner: &Inner) -> CancellationToken {
    inner.live.lock().unwrap().session_cancel.clone().unwrap_or_else(|| inner.cancel.child_token())
}

async fn handle_command(inner: Arc<Inner>, c: Command) {
    let mem = inner.parts.memory.clone();
    let job = match c {
        Command::Pin { answer_id } => {
            if let Err(e) = blocking(move || mem.pin_answer(answer_id, true)).await {
                log(format!("pin {answer_id}: {e}"));
            }
            None
        }
        Command::Regenerate { answer_id } => {
            meta(&inner, answer_id).map(|m| command_job(&inner, Some(&m), m.question.clone()))
        }
        Command::Shorter { answer_id } => {
            meta(&inner, answer_id).map(|m| Job { shorter: true, ..command_job(&inner, Some(&m), m.question.clone()) })
        }
        Command::Number { answer_id } => meta(&inner, answer_id).map(|m| Job {
            format: AnswerFormat::Fact,
            kind: PromptKind::Fact,
            ..command_job(&inner, Some(&m), m.question.clone())
        }),
        Command::ExpandAll { answer_id } => meta(&inner, answer_id).filter(|m| !m.cues.is_empty()).map(|m| Job {
            kind: PromptKind::Expand,
            expand: Some(m.cues.clone()),
            ..command_job(&inner, Some(&m), m.question.clone())
        }),
        Command::Followup { answer_id, text } => {
            let m = answer_id.and_then(|id| meta(&inner, id));
            let mut j = command_job(&inner, None, text);
            if let Some(m) = m {
                j.profile = m.profile;
            }
            Some(j)
        }
        Command::Feedback { answer_id, item: FeedbackItem::Rating { value } } => {
            if let Err(e) = blocking(move || mem.rate_answer(answer_id, value)).await {
                log(format!("rating {answer_id}: {e}"));
            }
            None
        }
        Command::Feedback { answer_id, item: FeedbackItem::Timing { delta_ms, .. } } => {
            // phone_rendered = frame_sent + RTT/2 + delta_ms (no cross-device clock, §4.6).
            if let Some(sent) = inner.first_frame.lock().unwrap().remove(&answer_id) {
                let half_rtt = (inner.parts.rtt_ms)().unwrap_or(0) / 2;
                inner.parts.timing.record(answer_id, Stage::PhoneRendered, sent + half_rtt + u64::from(delta_ms));
            }
            None
        }
        Command::MemoryList { layer } => {
            snapshot(&inner, layer).await;
            None
        }
        Command::MemoryUpdate { item } => {
            if let Err(e) = blocking(move || mem.upsert(item)).await {
                log(format!("memory.update: {e}"));
            }
            snapshot(&inner, None).await;
            None
        }
        Command::MemoryDelete { id } => {
            if let Err(e) = blocking(move || mem.delete(id)).await {
                log(format!("memory.delete: {e}"));
            }
            snapshot(&inner, None).await;
            None
        }
        Command::ContactConfirm { suggestion_id, accept } => {
            if let Err(e) = blocking(move || mem.confirm_contact(suggestion_id, accept)).await {
                log(format!("contact.confirm: {e}"));
            }
            snapshot(&inner, None).await;
            None
        }
    };
    if let Some(job) = job {
        let cancel = answer_cancel(&inner);
        run_answer(inner, job, cancel).await;
    }
}

async fn snapshot(inner: &Arc<Inner>, layer: Option<Layer>) {
    let mem = inner.parts.memory.clone();
    match blocking(move || mem.list(layer)).await {
        Ok(items) => {
            let sid = inner.live.lock().unwrap().session_id;
            send(inner, sid, Message::MemorySnapshot(MemorySnapshot { items: limits::memory_items(items) })).await;
        }
        Err(e) => log(format!("memory.list: {e}")),
    }
}

// ---------- post-call ----------

/// Runs one post-call prompt to completion. Local-only never reaches a cloud model; cloud
/// prompts are redacted first when redaction is on.
async fn complete(inner: &Inner, prompt: Prompt) -> Result<String> {
    let model = inner.parts.postcall.clone();
    let cfg = &inner.settings.config;
    let prompt = if model.is_cloud() {
        if cfg.mode == Mode::Local {
            return Err(SscError::Policy(format!("local-only mode cannot use cloud model {}", model.id())));
        }
        if cfg.redaction {
            ssc_redact::redact_prompt(&prompt, ssc_redact::Options { emails: cfg.redact_emails }).0
        } else {
            prompt
        }
    } else {
        prompt
    };
    let (tx, mut rx) = mpsc::channel(256);
    let cancel = inner.cancel.child_token();
    let c2 = cancel.clone();
    let run = tokio::spawn(async move { model.stream(&prompt, tx, c2).await });
    let collect = async {
        let mut out = String::new();
        while let Some(t) = rx.recv().await {
            out.push_str(&t.text);
        }
        out
    };
    let text = match tokio::time::timeout(inner.settings.postcall_timeout, collect).await {
        Ok(t) => t,
        Err(_) => {
            cancel.cancel();
            return Err(SscError::Timeout(inner.settings.postcall_timeout.as_millis() as u64));
        }
    };
    run.await.map_err(|e| SscError::Other(anyhow::anyhow!("post-call task: {e}")))??;
    Ok(text)
}

async fn post_call(
    inner: &Arc<Inner>,
    profile: &str,
    transcript: &[TranscriptSegment],
    shown: &[ShownAnswer],
) -> Result<postcall::ParsedSummary> {
    let session = postcall::session_text(transcript, shown);
    if session.trim().is_empty() {
        return Ok(postcall::ParsedSummary {
            outcome: ssc_memory::SessionOutcome {
                summary: "No conversation was captured.".into(),
                ..Default::default()
            },
            contacts: vec![],
        });
    }
    if !inner.parts.postcall.is_cloud()
        && let Some(hook) = &inner.parts.before_postcall
    {
        hook().await;
    }
    let mem = inner.parts.memory.clone();
    let p = profile.to_string();
    let (about_me, prof) = blocking(move || Ok((mem.about_me()?, mem.profile(&p)?))).await?;
    let base = ContextInput { about_me, profile: prof, now_ms: inner.parts.clock.now_ms(), ..Default::default() };

    let summary_prompt =
        assemble(PromptKind::Summary, &ContextInput { session, ..base.clone() }, &Budget::post_call()).prompt;
    let mut parsed = postcall::parse_summary(&complete(inner, summary_prompt).await?);

    let fu_session = postcall::followup_session(&parsed.outcome);
    let mem = inner.parts.memory.clone();
    let (p, k) = (profile.to_string(), inner.settings.retrieve_k);
    let fu_input = ContextInput { session: fu_session.clone(), question: fu_session, ..base };
    let fu_prompt =
        blocking(move || retrieve_and_assemble(&*mem, &p, k, PromptKind::Followup, fu_input, &Budget::post_call()))
            .await?
            .prompt;
    match complete(inner, fu_prompt).await {
        Ok(email) => parsed.outcome.followup_email = email.trim().to_string(),
        Err(e) => log(format!("follow-up draft failed: {e}")),
    }
    Ok(parsed)
}
