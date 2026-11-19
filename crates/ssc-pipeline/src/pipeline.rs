//! The running agent: sessions, capture loops, answer jobs, phone commands and post-call.
//!
//! Concurrency model: every long-running piece is a tokio task tied to a `CancellationToken`.
//! - pipeline lifetime: the phone command loop.
//! - session lifetime (child token): audio/STT, screen polling, the trigger engine and its event
//!   loop, and every answer started during the session.
//!
//! Each answer runs in its own task, so a slow model never blocks the hotkey or new triggers.
//!
//! ## Parallel hot path (H2)
//! In cloud mode a detected question waits for the decision model. The trigger engine tells the
//! pipeline ([`Speculation::Start`]) just before that call, and the pipeline starts the answer at
//! once — context assembly and the fast-chain stream — with the heuristic format, fast tier and
//! active profile, but **holds** everything it would send. When the decision confirms with the
//! same format, profile and (effective) tier, the held deltas are released and the stream goes
//! on; when it says "no answer" ([`Speculation::Cancel`]) the stream is cancelled and nothing
//! reaches the phone; when it differs, the speculation is dropped and a normal answer starts.
//! Speculation is skipped when [`AnswerRouter::speculation_allowed`] says the Groq budget is
//! getting tight. Redaction applies as for every answer (it is in the router). The hotkey path
//! never waits for a decision.
//!
//! Timing for a held answer: `context_ready`, `model_request` and `first_token` keep their true
//! times (they may precede `decision_done`); they are written to the timing log only once the
//! answer is confirmed, so cancelled speculation leaves no events. `frame_sent` is always after
//! `decision_done`.
//!
//! `answer.start` is sent right before the first delta (or before `answer.end` if no delta came),
//! so its `model` is the model that actually produced the answer, after any failover (L3).

use crate::limits;
use crate::postcall::{self, ShownAnswer};
use ssc_context::{Budget, ContextInput, Known, KnownKind, PromptKind, assemble, retrieve_and_assemble};
use ssc_core::config::Config;
use ssc_core::decision::Tier;
use ssc_core::protocol::{
    AgentLog, AgentStatus, AnswerDelta, AnswerEnd, AnswerEndReason, AnswerFormat, AnswerStart, Block, CheckState,
    Command, Cue, FeedbackItem, Intent, Layer, MemorySnapshot, Message, Mode, SessionEnd, SessionEndReason,
    SessionStart, SessionSummary, TranscriptPartial, Trigger, Urgency,
};
use ssc_core::timing::{Stage, Timing};
use ssc_core::traits::{
    AnswerModel, AudioSource, Clock, DecisionModel, MemoryStore, ScreenSource, SttEngine, Transport,
};
use ssc_core::types::{Prompt, ScreenGrab, ScreenSnapshot, TranscriptSegment};
use ssc_core::{Result, SscError};
use ssc_memory::{AnswerRecord, SqliteMemory};
use ssc_router::grammar::{ParsedAnswer, merge_detail};
use ssc_router::warm::REWARM_EVERY;
use ssc_router::{AnswerRouter, RoutedDelta, Warmer};
use ssc_trigger::{Speculation, TriggerEngine, TriggerEvent, TriggerInput, TriggerSettings, format_for};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub type RttFn = Arc<dyn Fn() -> Option<u64> + Send + Sync>;
pub type GateFn = Arc<dyn Fn() -> bool + Send + Sync>;
pub type Hook = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
/// "May the background tagger run now?" (M5): true only when the decision model can run without
/// making Ollama swap out the live answer model.
pub type TagGate = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// The transport wrote an answer's first `answer.delta` to the socket (`frame_sent`, §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSentEvent {
    pub answer_id: Uuid,
    pub t_ms: u64,
}

/// Optional parts (all off by default; tests and the bench leave them unset).
#[derive(Default)]
pub struct Extras {
    /// Provider warm-up at session start and while capturing (M2).
    pub warm: Option<Warmer>,
    /// Enables the background decision tag for answers fired without one (offline/local, M5).
    pub tag_gate: Option<TagGate>,
    /// `frame_sent` from the transport (first delta written to the socket). Without it the
    /// pipeline stamps `frame_sent` when `Transport::send` of the first delta returns.
    pub frame_sent: Option<mpsc::Receiver<FrameSentEvent>>,
}

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
    /// Start cloud question answers while the decision runs (H2). On by default.
    pub speculate: bool,
    /// Timeout of a background tag decision (not on the live path).
    pub tag_timeout: Duration,
    /// Re-warm interval while capturing (below the HTTP pool's idle drop).
    pub rewarm_every: Duration,
}

impl Settings {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            screen_poll: Duration::from_millis(1000),
            postcall_timeout: Duration::from_secs(180),
            retrieve_k: 5,
            speculate: true,
            tag_timeout: Duration::from_secs(5),
            rewarm_every: REWARM_EVERY,
        }
    }
}

/// Answers remembered for phone commands (regenerate, shorter, …); the oldest are dropped (L2).
const MAX_REMEMBERED_ANSWERS: usize = 256;
/// Screen grabs (with their images, ~100-400 KB each) kept for "again" / "shorter" on screen
/// answers (CR-20). Older screen answers re-grab the screen.
const MAX_REMEMBERED_GRABS: usize = 8;
/// `first_frame` entries the phone never reported a paint time for are dropped after this (L2).
const FIRST_FRAME_TTL_MS: u64 = 5 * 60_000;

/// What a finished (or in-flight) answer needs for phone commands.
#[derive(Debug, Clone)]
struct AnswerMeta {
    question: String,
    format: AnswerFormat,
    intent: Intent,
    profile: String,
    cues: Vec<Cue>,
    /// The answer as shown (screen answers only; follow-ups quote it, CR-23b G).
    text: String,
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
    /// Fired without a decision in local/offline mode: tag it in the background (M5).
    tag: bool,
    /// CR-20 screen hotkey: the grabbed window (text + image) this answer is about.
    screen: Option<Arc<ScreenGrab>>,
    /// Extra instruction appended to the dynamic prompt (screen "shorter" / "number").
    note: Option<&'static str>,
    /// Screen follow-ups: the earlier answer about the same screenshot (the `{{answer}}` slot).
    prev_answer: Option<String>,
}

impl Job {
    fn from_event(ev: TriggerEvent) -> Self {
        Self {
            tag: false,
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
            screen: None,
            note: None,
            prev_answer: None,
        }
    }

    /// CR-20: an answer about a grabbed window.
    fn screen(answer_id: Uuid, trigger: Trigger, profile: String, grab: Arc<ScreenGrab>) -> Self {
        Self {
            answer_id,
            trigger,
            question: grab.label(),
            format: AnswerFormat::Screen,
            intent: Intent::None,
            urgency: Urgency::High,
            tier: Tier::Fast,
            profile,
            kind: PromptKind::Screen,
            shorter: false,
            expand: None,
            tag: false,
            screen: Some(grab),
            note: None,
            prev_answer: None,
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

/// What a held (speculative) answer is waiting for.
#[derive(Debug, Clone)]
enum Gate {
    Hold,
    /// Confirmed; the job carries the decision's tags (intent, urgency, trigger fields).
    Go(Box<Job>),
    Drop,
}

struct Spec {
    job: Job,
    gate: watch::Sender<Gate>,
}

struct Inner {
    parts: Parts,
    settings: Settings,
    session: tokio::sync::Mutex<Option<Session>>,
    live: Mutex<Live>,
    /// answer_id → (insertion seq, meta).
    answers: Mutex<HashMap<Uuid, (u64, AnswerMeta)>>,
    answer_seq: AtomicU64,
    /// answer_id → `frame_sent` of its first delta (for `phone_rendered`).
    first_frame: Mutex<HashMap<Uuid, u64>>,
    /// Held speculative answers by answer_id (H2).
    speculative: Mutex<HashMap<Uuid, Spec>>,
    /// Answers currently running (the background tagger waits for 0).
    active_answers: AtomicUsize,
    /// Last time a model request started (re-warm only when idle).
    last_model_use: AtomicU64,
    warm: Option<Warmer>,
    tag_gate: Option<TagGate>,
    /// `frame_sent` comes from the transport, not from `send()` returning.
    external_frame_sent: bool,
    /// answer_id → the grab a screen answer used (newest last, at most [`MAX_REMEMBERED_GRABS`]).
    screen_grabs: Mutex<Vec<(Uuid, Arc<ScreenGrab>)>>,
    /// CR-23: health facts, the agent's status builder, and a nudge to send status now.
    health: Mutex<PipelineHealth>,
    status_provider: Mutex<Option<StatusProvider>>,
    status_now: tokio::sync::Notify,
    cancel: CancellationToken,
}

/// CR-23: what the pipeline knows about its own health, for the agent's status builder.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PipelineHealth {
    /// A capture session is running.
    pub capturing: bool,
    /// Last answer: (PC ms, state, detail: the model, or the error).
    pub last_answer: Option<(u64, CheckState, String)>,
    /// Last screen grab: (PC ms, state, detail: the window, or the error).
    pub last_grab: Option<(u64, CheckState, String)>,
}

/// Builds `agent.status` from the pipeline's facts plus what only the agent knows (permissions,
/// keys, link). Set with [`Pipeline::set_status_provider`].
pub type StatusProvider = Arc<dyn Fn(&PipelineHealth) -> AgentStatus + Send + Sync>;

/// `agent.status` cadence (the phone treats ~3 missed beats as "PC silent").
pub const STATUS_EVERY: Duration = Duration::from_secs(15);
/// New log lines are batched for this long before one `agent.log` goes out.
const LOG_BATCH: Duration = Duration::from_millis(500);

/// Cheap to clone; all clones drive the same agent.
#[derive(Clone)]
pub struct Pipeline {
    inner: Arc<Inner>,
}

const SHORTER_NOTE: &str = "\nThe user tapped \"shorter\": output exactly 3 cues and no \"---\" detail block.\n";
const SCREEN_SHORTER_NOTE: &str =
    "\nThe user tapped \"shorter\": output only the first line (the answer), no \"---\" block.\n";
const SCREEN_NUMBER_NOTE: &str = "\nThe user tapped \"number\": make the first line the number (with units) the question asks for, if it asks for one.\n";
/// "Shorter" caps the visible answer (3 cues of 2–6 words fit easily).
const SHORTER_MAX_TOKENS: u32 = 60;
const SHORTER_CUES: usize = 3;
const TRANSCRIPT_CONTEXT_SEGMENTS: usize = 200;

fn log(msg: impl AsRef<str>) {
    // CR-23: also to the phone's Logs tab (operational lines only, never prompts).
    let msg = msg.as_ref();
    ssc_core::logbus::emit(ssc_core::logbus::infer_level(msg), "ssc-pipeline", msg);
}

impl Pipeline {
    /// Starts the phone command loop. Capture starts with [`Pipeline::start_session`].
    pub fn new(parts: Parts, settings: Settings) -> Self {
        Self::with_extras(parts, settings, Extras::default())
    }

    /// [`Pipeline::new`] with warm-up, background tagging and transport `frame_sent`.
    pub fn with_extras(parts: Parts, settings: Settings, extras: Extras) -> Self {
        parts.memory.set_keep_transcripts(settings.config.keep_transcripts);
        let incoming = parts.transport.take_incoming();
        let Extras { warm, tag_gate, frame_sent } = extras;
        let inner = Arc::new(Inner {
            parts,
            settings,
            session: tokio::sync::Mutex::new(None),
            live: Mutex::new(Live::default()),
            answers: Mutex::new(HashMap::new()),
            answer_seq: AtomicU64::new(0),
            screen_grabs: Mutex::new(Vec::new()),
            health: Mutex::new(PipelineHealth::default()),
            status_provider: Mutex::new(None),
            status_now: tokio::sync::Notify::new(),
            first_frame: Mutex::new(HashMap::new()),
            speculative: Mutex::new(HashMap::new()),
            active_answers: AtomicUsize::new(0),
            last_model_use: AtomicU64::new(0),
            warm,
            tag_gate,
            external_frame_sent: frame_sent.is_some(),
            cancel: CancellationToken::new(),
        });
        if let Some(rx) = frame_sent {
            tokio::spawn(frame_sent_loop(inner.clone(), rx));
        }
        match incoming {
            Some(rx) => {
                tokio::spawn(command_loop(inner.clone(), rx));
            }
            None => log("transport incoming stream already taken: phone commands disabled"),
        }
        tokio::spawn(log_forward_loop(inner.clone()));
        tokio::spawn(status_loop(inner.clone()));
        Self { inner }
    }

    /// CR-23: the agent's `agent.status` builder. Sends a status right away.
    pub fn set_status_provider(&self, f: StatusProvider) {
        *self.inner.status_provider.lock().unwrap() = Some(f);
        self.inner.status_now.notify_one();
    }

    /// CR-23: the pipeline's health facts.
    pub fn health(&self) -> PipelineHealth {
        health_now(&self.inner)
    }

    /// CR-23: the current `agent.status`, if a provider is set.
    pub fn status(&self) -> Option<AgentStatus> {
        build_status(&self.inner)
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
        let mut engine = TriggerEngine::new(settings, inner.parts.decision.clone(), inner.parts.clock.clone())
            .with_timing(inner.parts.timing.clone());
        if inner.settings.speculate {
            // Weak: the engine lives in `inner.live`, so a strong ref would be a cycle.
            let weak = Arc::downgrade(inner);
            engine = engine.with_speculation(Arc::new(move |s| on_speculation(&weak, s)));
        }
        let engine = Arc::new(engine);
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
        if let Some(w) = inner.warm.clone() {
            tasks.push(tokio::spawn(warm_loop(inner.clone(), w, active.clone(), cancel.clone())));
        }
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
        // CR-20: continuous screen reading is opt-in; the screen hotkey reads on demand.
        if inner.settings.config.screen_watch
            && let Some(screen) = inner.parts.screen.clone()
        {
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

    /// CR-20 screen hotkey: grab the focused window (an image too, unless local-only), answer
    /// what is on it now. Works with or without a session. `None` when there is no screen
    /// reader on this platform.
    pub fn answer_from_screen(&self) -> Option<Uuid> {
        let inner = self.inner.clone();
        let screen = inner.parts.screen.clone()?;
        let id = Uuid::new_v4();
        record(&inner, id, Stage::TriggerFired);
        let cancel = answer_cancel(&inner);
        let profile = current_profile(&inner);
        // Local-only mode sends nothing off the PC: text (with OCR) only.
        let image = inner.settings.config.mode != Mode::Local;
        tokio::spawn(async move {
            let t0 = inner.parts.clock.now_ms();
            let grab = match blocking(move || screen.grab(image)).await {
                Ok(g) => g,
                Err(e) => return capture_failed(&inner, id, &profile, e.to_string()).await,
            };
            let ms = inner.parts.clock.now_ms().saturating_sub(t0);
            // CR-23b: what was captured, so a wrong window shows up in the log (metadata only).
            let size = |s: Option<(i32, i32)>| s.map_or("?".into(), |(w, h)| format!("{w}x{h}"));
            log(format!(
                "screen read: captured {} window {} pt -> image {} ({} KB), {} chars of window text, {ms} ms",
                if grab.app.is_empty() { "unknown" } else { &grab.app },
                size(grab.window_size),
                grab.image_size.map_or("none".into(), |(w, h)| format!("{w}x{h}")),
                grab.image.as_ref().map_or(0, |i| i.data.len() / 1024),
                grab.text.chars().count(),
            ));
            // E: in cloud mode the image IS the input; answering from text alone guesses.
            if image && grab.image.is_none() {
                let why = grab.image_error.clone().unwrap_or_else(|| "no image".into());
                return capture_failed(&inner, id, &profile, why).await;
            }
            let now = inner.parts.clock.now_ms();
            set_health(&inner, |h| h.last_grab = Some((now, CheckState::Ok, grab.label())));
            let grab = Arc::new(grab);
            remember_grab(&inner, id, grab.clone());
            run_answer(inner, Job::screen(id, Trigger::Hotkey, profile, grab), cancel, None).await;
        });
        Some(id)
    }

    /// End the session: stop capture, `session.end`, then summary + follow-up → `session.summary`.
    /// Returns `None` if no session was running.
    pub async fn end_session(&self, reason: SessionEndReason) -> Result<Option<SessionSummary>> {
        let inner = &self.inner;
        let Some(session) = inner.session.lock().await.take() else { return Ok(None) };
        session.cancel.cancel();
        inner.speculative.lock().unwrap().clear(); // held answers end with the session token
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
        inner.speculative.lock().unwrap().clear();
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
        let text = seg.text.trim().to_string();
        let keep = seg.is_final && !text.is_empty();
        // L1: the trigger engine gets the segment first (its silence window starts now); the
        // context copy is updated before that so an answer it fires sees this segment. Phone
        // echo and the (opt-in) transcript store come after.
        if keep {
            inner.live.lock().unwrap().transcript.push(seg.clone());
        }
        if trig.send(TriggerInput::Transcript(seg.clone())).await.is_err() {
            break;
        }
        if !text.is_empty() {
            let tp = TranscriptPartial {
                speaker: seg.speaker,
                text: limits::clip(&text, 4096),
                is_final: seg.is_final,
                t0: seg.t0_ms,
                t1: seg.t1_ms,
            };
            send(&inner, sid, Message::TranscriptPartial(tp)).await;
        }
        if keep && let Some(sid) = sid {
            let mem = inner.parts.memory.clone();
            if let Err(e) = blocking(move || mem.record_transcript(sid, &seg)).await {
                log(format!("transcript store failed: {e}"));
            }
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
        let local = inner.live.lock().unwrap().engine.as_ref().is_some_and(|e| e.mode() == Mode::Local);
        let tag = local && ev.trigger == Trigger::Question && ev.decision.is_none();
        let job = Job { tag, ..Job::from_event(ev) };
        dispatch(&inner, job, cancel.clone());
    }
}

/// Warm the answer providers at session start, then keep them warm while capturing (M2).
async fn warm_loop(inner: Arc<Inner>, w: Warmer, profile: String, cancel: CancellationToken) {
    // A local answer model also gets the session's fixed cue prefix in its KV cache.
    let prefix = if inner.settings.config.mode == Mode::Local {
        let mem = inner.parts.memory.clone();
        let r = blocking(move || {
            let input =
                ContextInput { about_me: mem.about_me()?, profile: mem.profile(&profile)?, ..Default::default() };
            Ok(assemble(PromptKind::Cue, &input, &Budget::local()).prompt.prefix)
        })
        .await;
        r.ok()
    } else {
        None
    };
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = w.warm(prefix.as_deref()) => {}
    }
    let every = inner.settings.rewarm_every;
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // the first tick is immediate
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let idle = inner.parts.clock.now_ms().saturating_sub(inner.last_model_use.load(Ordering::Relaxed));
        // An answer in the last interval kept the connection (and the local model) warm.
        if u128::from(idle) >= every.as_millis() {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = w.warm(prefix.as_deref()) => {}
            }
        }
    }
}

// ---------- answers ----------

fn spawn_job(inner: &Arc<Inner>, job: Job, cancel: CancellationToken) {
    let inner = inner.clone();
    tokio::spawn(async move { run_answer(inner, job, cancel, None).await });
}

/// A trigger event: release its held speculative answer if that one fits, else start one.
fn dispatch(inner: &Arc<Inner>, job: Job, cancel: CancellationToken) {
    let spec = inner.speculative.lock().unwrap().remove(&job.answer_id);
    match spec {
        Some(s) if same_answer(inner, &s.job, &job) => {
            s.gate.send_replace(Gate::Go(Box::new(job)));
        }
        Some(s) => {
            s.gate.send_replace(Gate::Drop);
            spawn_job(inner, job, cancel);
        }
        None => spawn_job(inner, job, cancel),
    }
}

/// Would `confirmed` produce the same prompt and route as the speculative `held` job?
fn same_answer(inner: &Inner, held: &Job, confirmed: &Job) -> bool {
    // Without a strong model both tiers run the fast chain alone.
    let tier = |j: &Job| if inner.parts.answers.has_strong() { j.tier } else { Tier::Fast };
    held.format == confirmed.format
        && held.kind == confirmed.kind
        && held.profile == confirmed.profile
        && held.question == confirmed.question
        && tier(held) == tier(confirmed)
}

/// The trigger engine's speculation callback (synchronous, before the decision call).
fn on_speculation(weak: &Weak<Inner>, s: Speculation) {
    let Some(inner) = weak.upgrade() else { return };
    match s {
        Speculation::Start(ev) => {
            if !inner.parts.answers.speculation_allowed() {
                return; // budget getting tight: the confirmed answer starts after the decision
            }
            let Some(cancel) = inner.live.lock().unwrap().session_cancel.clone() else { return };
            let job = Job::from_event(ev);
            let (gate, rx) = watch::channel(Gate::Hold);
            inner.speculative.lock().unwrap().insert(job.answer_id, Spec { job: job.clone(), gate });
            let i2 = inner.clone();
            tokio::spawn(async move { run_answer(i2, job, cancel, Some(rx)).await });
        }
        Speculation::Cancel(id) => {
            if let Some(s) = inner.speculative.lock().unwrap().remove(&id) {
                s.gate.send_replace(Gate::Drop);
            }
        }
    }
}

fn numbered_cues(cues: &[String]) -> String {
    cues.iter().enumerate().map(|(i, c)| format!("{}. {c}", i + 1)).collect::<Vec<_>>().join("\n")
}

/// Saved contacts and accounts for the context's "known people" cards (M7).
fn known_entities(mem: &SqliteMemory) -> Result<Vec<Known>> {
    let mut out = Vec::new();
    for (layer, kind) in [(Layer::Contact, KnownKind::Contact), (Layer::Account, KnownKind::Account)] {
        out.extend(mem.list(Some(layer))?.into_iter().map(|i| Known { kind, name: i.title, notes: i.body }));
    }
    Ok(out)
}

/// Timing stages of one answer. While the answer is held they are buffered (true times kept)
/// and written only when it is confirmed; a dropped speculation leaves no events.
struct Stamps {
    held: Option<Vec<(Stage, u64)>>,
}

impl Stamps {
    fn record(&mut self, inner: &Inner, id: Uuid, stage: Stage) -> u64 {
        let t = inner.parts.clock.now_ms();
        match &mut self.held {
            Some(v) => v.push((stage, t)),
            None => inner.parts.timing.record(id, stage, t),
        }
        t
    }
    fn commit(&mut self, inner: &Inner, id: Uuid) {
        for (stage, t) in self.held.take().unwrap_or_default() {
            inner.parts.timing.record(id, stage, t);
        }
    }
}

/// Counts running answers (the background tagger runs only when none is).
struct Active<'a>(&'a AtomicUsize);
impl<'a> Active<'a> {
    fn new(c: &'a AtomicUsize) -> Self {
        c.fetch_add(1, Ordering::SeqCst);
        Self(c)
    }
}
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Wait for a held answer's outcome: the confirmed job, or `None` (dropped / session over).
async fn gate_outcome(g: &mut watch::Receiver<Gate>, cancel: &CancellationToken) -> Option<Job> {
    loop {
        match &*g.borrow_and_update() {
            Gate::Go(j) => return Some((**j).clone()),
            Gate::Drop => return None,
            Gate::Hold => {}
        }
        tokio::select! {
            _ = cancel.cancelled() => return None,
            r = g.changed() => if r.is_err() {
                return match &*g.borrow() { Gate::Go(j) => Some((**j).clone()), _ => None };
            },
        }
    }
}

/// Phone-side state of one answer while it streams.
struct Push {
    started: bool,
    first_sent: bool,
    detail_text: String,
    /// Everything streamed (both blocks): kept with the answer for screen follow-ups, and the
    /// debug dump.
    all_text: String,
}

/// `answer.start` (once), naming the model that produced the first token.
async fn ensure_start(
    inner: &Inner,
    job: &Job,
    sid: Option<Uuid>,
    model: &watch::Receiver<Option<String>>,
    p: &mut Push,
) {
    if p.started {
        return;
    }
    p.started = true;
    let model = model.borrow().clone().unwrap_or_else(|| inner.parts.answers.primary_model().to_string());
    let start = AnswerStart {
        answer_id: job.answer_id,
        trigger: job.trigger,
        format: job.format,
        intent: job.intent,
        urgency: job.urgency,
        model: limits::clip(&model, 128),
        thumb: job.screen.as_ref().and_then(|g| g.thumb.as_ref()).map(|t| {
            use base64::Engine;
            format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(t))
        }),
    };
    send(inner, sid, Message::AnswerStart(start)).await;
}

async fn push_delta(
    inner: &Inner,
    job: &Job,
    sid: Option<Uuid>,
    model: &watch::Receiver<Option<String>>,
    d: RoutedDelta,
    p: &mut Push,
) {
    if d.block == Block::Detail {
        if job.shorter {
            return; // "shorter": cues only (L9; the cap alone can't guarantee it)
        }
        p.detail_text.push_str(&d.text);
    }
    // The answer as shown (both blocks): for screen follow-ups (G) and the debug dump.
    if d.block == Block::Detail && !p.all_text.contains("\n---\n") {
        p.all_text.push_str("\n---\n");
    }
    p.all_text.push_str(&d.text);
    ensure_start(inner, job, sid, model, p).await;
    for text in limits::split_chars(&d.text, 4096) {
        send(inner, sid, Message::AnswerDelta(AnswerDelta { answer_id: job.answer_id, block: d.block, text })).await;
    }
    if !p.first_sent {
        p.first_sent = true;
        if !inner.external_frame_sent {
            // Without a transport signal, `send()` returning is the closest stand-in.
            let t = record(inner, job.answer_id, Stage::FrameSent);
            remember_first_frame(inner, job.answer_id, t);
        }
    }
}

fn remember_first_frame(inner: &Inner, id: Uuid, t: u64) {
    insert_first_frame(&mut inner.first_frame.lock().unwrap(), id, t);
}

/// Insert unless present; drop entries older than [`FIRST_FRAME_TTL_MS`] (L2).
fn insert_first_frame(ff: &mut HashMap<Uuid, u64>, id: Uuid, t: u64) {
    ff.retain(|_, sent| t.saturating_sub(*sent) < FIRST_FRAME_TTL_MS);
    ff.entry(id).or_insert(t);
}

/// `frame_sent` from the transport: the first delta of an answer hit the socket.
async fn frame_sent_loop(inner: Arc<Inner>, mut rx: mpsc::Receiver<FrameSentEvent>) {
    loop {
        let ev = tokio::select! {
            _ = inner.cancel.cancelled() => break,
            e = rx.recv() => match e { Some(e) => e, None => break },
        };
        inner.parts.timing.record(ev.answer_id, Stage::FrameSent, ev.t_ms);
        remember_first_frame(&inner, ev.answer_id, ev.t_ms);
    }
}

async fn run_answer(inner: Arc<Inner>, job: Job, cancel: CancellationToken, gate: Option<watch::Receiver<Gate>>) {
    let _active = Active::new(&inner.active_answers);
    let mut job = job;
    let id = job.answer_id;
    let mut stamps = Stamps { held: gate.is_some().then(Vec::new) };
    let mut gate = gate;
    // Per-answer token: a dropped speculation (or this task ending) stops its model at once.
    let answer_cancel = cancel.child_token();
    let _stop_model = answer_cancel.clone().drop_guard();
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
    // CR-20: a screen answer reads its own grab, not the watch loop's diff.
    let screen = job.screen.as_ref().map_or(screen, |g| g.text.clone());
    let mut input = ContextInput {
        transcript,
        screen,
        question: job.question.clone(),
        now_ms: inner.parts.clock.now_ms(),
        answer: match (&job.expand, &job.prev_answer) {
            (Some(c), _) => numbered_cues(&c.iter().map(|c| c.text.clone()).collect::<Vec<_>>()),
            (None, Some(prev)) => prev.clone(),
            (None, None) => String::new(),
        },
        ..Default::default()
    };
    let assembled = blocking(move || {
        input.about_me = mem.about_me()?;
        input.profile = mem.profile(&profile_id)?;
        input.known = known_entities(&mem)?;
        let a = retrieve_and_assemble(&*mem, &profile_id, k, kind, input.clone(), &budget)?;
        Ok((a, input))
    })
    .await;
    let (assembled, input) = match assembled {
        Ok(v) => v,
        Err(e) => {
            log(format!("context assembly failed: {e}"));
            if let Some(g) = gate.as_mut() {
                // Speculative: nothing was shown; retry from scratch if the decision confirms.
                if let Some(j) = gate_outcome(g, &cancel).await {
                    spawn_job(&inner, Job { answer_id: id, ..j }, cancel);
                }
                return;
            }
            finish(&inner, &job, sid, error_end(id), String::new()).await;
            return;
        }
    };
    stamps.record(&inner, id, Stage::ContextReady);

    let mut prompt: Prompt = assembled.prompt;
    if let Some(img) = job.screen.as_ref().and_then(|g| g.image.clone())
        && mode != Mode::Local
    {
        prompt.images = vec![img];
    }
    if let Some(note) = job.note {
        prompt.dynamic.push_str(note);
    }
    let debug_prompt = debug_answers_on().then(|| (prompt.prefix.clone(), prompt.dynamic.clone(), image_note(&prompt)));
    if job.shorter && job.format == AnswerFormat::Screen {
        prompt.dynamic.push_str(SCREEN_SHORTER_NOTE);
        prompt.max_tokens = prompt.max_tokens.min(SHORTER_MAX_TOKENS);
    } else if job.shorter {
        prompt.dynamic.push_str(SHORTER_NOTE);
        // Visible-answer cap; reasoning models get their own headroom in the adapter (L9).
        prompt.max_tokens = prompt.max_tokens.min(SHORTER_MAX_TOKENS);
    }

    // 5. Route. The strong tier's detail prompt reuses the retrieved chunks.
    let detail_input = ContextInput { chunks: assembled.chunks_used.clone(), ..input };
    let detail = move |cues: &[String]| {
        let mut i = detail_input;
        i.answer = numbered_cues(cues);
        assemble(PromptKind::Expand, &i, &budget).prompt
    };
    let (dtx, mut drx) = mpsc::channel::<RoutedDelta>(64);
    let (mtx, mrx) = watch::channel::<Option<String>>(None);
    let router = inner.parts.answers.clone();
    let (tier, c2) = (if job.shorter { Tier::Fast } else { job.tier }, answer_cancel.clone());
    let t_req = stamps.record(&inner, id, Stage::ModelRequest);
    inner.last_model_use.store(t_req, Ordering::Relaxed);
    let mut run =
        tokio::spawn(async move { router.answer_reporting(tier, &prompt, detail, dtx, &c2, Some(&mtx)).await });

    // Held (speculative): buffer deltas until the decision says go / drop.
    let mut held: Vec<RoutedDelta> = Vec::new();
    let mut first_token = false;
    let mut stream_open = true;
    let mut early: Option<std::result::Result<Result<ssc_router::AnswerOutcome>, tokio::task::JoinError>> = None;
    if let Some(g) = gate.as_mut() {
        let confirmed = loop {
            match &*g.borrow_and_update() {
                Gate::Go(j) => break Some((**j).clone()),
                Gate::Drop => break None,
                Gate::Hold => {}
            }
            tokio::select! {
                _ = cancel.cancelled() => break None,
                r = g.changed() => if r.is_err() {
                    break match &*g.borrow() { Gate::Go(j) => Some((**j).clone()), _ => None };
                },
                d = drx.recv(), if stream_open => match d {
                    Some(d) => {
                        if !first_token {
                            first_token = true;
                            stamps.record(&inner, id, Stage::FirstToken);
                        }
                        held.push(d);
                    }
                    None => stream_open = false,
                },
            }
        };
        let Some(j) = confirmed else { return }; // dropped: the guard stops the model; nothing was sent
        job = Job { answer_id: id, ..j };
        if !stream_open && held.is_empty() {
            // The speculative stream ended with nothing to show: if it failed, start over now.
            let r = (&mut run).await;
            if let Ok(Err(e)) = &r
                && !matches!(e, SscError::Cancelled)
            {
                log(format!("speculative answer {id} failed ({e}); restarting"));
                spawn_job(&inner, job, cancel);
                return;
            }
            early = Some(r);
        }
        stamps.commit(&inner, id);
    }

    // 6. Push.
    let mut p = Push { started: false, first_sent: false, detail_text: String::new(), all_text: String::new() };
    for d in held {
        push_delta(&inner, &job, sid, &mrx, d, &mut p).await;
    }
    if stream_open {
        while let Some(d) = drx.recv().await {
            if !first_token {
                first_token = true;
                record(&inner, id, Stage::FirstToken);
            }
            push_delta(&inner, &job, sid, &mrx, d, &mut p).await;
        }
    }
    let res = match early {
        Some(r) => r,
        None => run.await,
    };
    let fallback_model = || mrx.borrow().clone().unwrap_or_else(|| inner.parts.answers.primary_model().to_string());
    let now = inner.parts.clock.now_ms();
    match &res {
        Ok(Ok(o)) => set_health(&inner, |h| h.last_answer = Some((now, CheckState::Ok, o.fast.model.clone()))),
        Ok(Err(SscError::Cancelled)) => {}
        Ok(Err(e)) => set_health(&inner, |h| h.last_answer = Some((now, CheckState::Error, e.to_string()))),
        Err(e) => set_health(&inner, |h| h.last_answer = Some((now, CheckState::Error, format!("answer task: {e}")))),
    }
    let (mut end, model) = match res {
        Ok(Ok(outcome)) => {
            let parsed = match &job.expand {
                // Expand all: the original cues with every expansion filled in.
                Some(cues) => merge_detail(ParsedAnswer { cues: cues.clone(), ..Default::default() }, &p.detail_text),
                None => outcome.parsed,
            };
            (parsed.into_answer_end(id, AnswerEndReason::Done), outcome.fast.model)
        }
        Ok(Err(SscError::Cancelled)) => (end_with(id, AnswerEndReason::Cancelled), fallback_model()),
        Ok(Err(e)) => {
            log(format!("answer {id} failed: {e}"));
            (error_end(id), fallback_model())
        }
        Err(e) => {
            log(format!("answer task {id} panicked: {e}"));
            (error_end(id), fallback_model())
        }
    };
    if job.shorter {
        end.cues.truncate(SHORTER_CUES);
        for c in &mut end.cues {
            c.expand = None;
        }
    }
    // The card exists on the phone before its end (no delta came: error or empty answer).
    if end.reason != AnswerEndReason::Cancelled {
        ensure_start(&inner, &job, sid, &mrx, &mut p).await;
    }
    if let Some((prefix, dynamic, images)) = debug_prompt {
        print_debug_answer(&job, &model, &prefix, &dynamic, &images, &p.all_text);
    }
    finish(&inner, &job, sid, end, model).await;
    if job.format == AnswerFormat::Screen
        && let Some((_, m)) = inner.answers.lock().unwrap().get_mut(&id)
    {
        m.text = limits::clip(p.all_text.trim(), 2000);
    }
}

/// `SSC_DEBUG_ANSWERS=1`: each answer's prompt text (images as size only), model and raw output
/// are printed to the agent's terminal, to diagnose bad answers (CR-20). Never written to disk:
/// prompts hold call and screen content (security_pipeline `only_reviewed_code_writes_files`).
fn debug_answers_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SSC_DEBUG_ANSWERS").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")))
}

fn image_note(p: &Prompt) -> String {
    p.images.iter().map(|i| format!("{} {} bytes", i.mime, i.data.len())).collect::<Vec<_>>().join(", ")
}

fn print_debug_answer(job: &Job, model: &str, prefix: &str, dynamic: &str, images: &str, output: &str) {
    // stderr only: prompts hold screen/call content and must never reach the phone log feed.
    let id = job.answer_id;
    let images = if images.is_empty() { "none" } else { images };
    eprintln!(
        "[ssc-pipeline] debug answer {id} format {:?} trigger {:?} model {model} images: {images}\n===== PREFIX =====\n{prefix}\n===== DYNAMIC =====\n{dynamic}\n===== OUTPUT =====\n{output}\n===== END {id} =====",
        job.format, job.trigger,
    );
}

fn end_with(id: Uuid, reason: AnswerEndReason) -> AnswerEnd {
    AnswerEnd { answer_id: id, reason, cues: vec![], fact: None, source_doc: None, not_in_docs: false }
}

fn error_end(id: Uuid) -> AnswerEnd {
    end_with(id, AnswerEndReason::Error)
}

fn remember_answer(inner: &Inner, id: Uuid, meta: AnswerMeta) {
    let seq = inner.answer_seq.fetch_add(1, Ordering::Relaxed);
    insert_capped(&mut inner.answers.lock().unwrap(), id, seq, meta, MAX_REMEMBERED_ANSWERS);
}

/// Insert, then drop the lowest-`seq` entries above `cap` (L2).
fn insert_capped<T>(m: &mut HashMap<Uuid, (u64, T)>, id: Uuid, seq: u64, v: T, cap: usize) {
    m.insert(id, (seq, v));
    while m.len() > cap {
        let Some(oldest) = m.iter().min_by_key(|(_, (s, _))| *s).map(|(k, _)| *k) else { break };
        m.remove(&oldest);
    }
}

/// `answer.end`, then remember and store the answer.
async fn finish(inner: &Arc<Inner>, job: &Job, sid: Option<Uuid>, end: AnswerEnd, model: String) {
    let end = limits::answer_end(end);
    send(inner, sid, Message::AnswerEnd(end.clone())).await;
    if end.reason != AnswerEndReason::Done {
        return;
    }
    remember_answer(
        inner,
        job.answer_id,
        AnswerMeta {
            question: job.question.clone(),
            format: job.format,
            intent: job.intent,
            profile: job.profile.clone(),
            cues: end.cues.clone(),
            text: String::new(),
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
    let Some(session_id) = sid else { return };
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
    if job.tag
        && let Some(gate) = inner.tag_gate.clone()
    {
        // M5: the record is stored once the background tag is in (or skipped), so the stored
        // intent is the decision model's when it ran.
        let inner = inner.clone();
        let screen = inner.live.lock().unwrap().screen_diff.clone();
        tokio::spawn(async move {
            let mut rec = rec;
            if let Some(d) = background_tag(&inner, &gate, &rec.question, &screen, rec.id).await {
                rec.intent = d.intent.value;
                if let Some((_, m)) = inner.answers.lock().unwrap().get_mut(&rec.id) {
                    m.intent = d.intent.value;
                }
            }
            store_answer(&inner, rec).await;
        });
        return;
    }
    store_answer(inner, rec).await;
}

async fn store_answer(inner: &Inner, rec: AnswerRecord) {
    let mem = inner.parts.memory.clone();
    if let Err(e) = blocking(move || mem.record_answer(&rec)).await {
        log(format!("answer store failed: {e}"));
    }
}

/// The decision model's tags for an answer that fired without one (local/offline mode). Runs
/// only after the answer ended, when no other answer is running and the gate says the decision
/// model can run without Ollama swapping out the live answer model; else skipped (`None`).
async fn background_tag(
    inner: &Inner,
    gate: &TagGate,
    question: &str,
    screen: &str,
    id: Uuid,
) -> Option<ssc_core::decision::Decision> {
    if inner.active_answers.load(Ordering::SeqCst) > 0 || !gate().await {
        return None;
    }
    let profiles = inner.live.lock().unwrap().engine.as_ref().map(|e| e.settings().profiles).unwrap_or_default();
    let input = ssc_trigger::decision_input(question, screen, &profiles);
    let d = tokio::time::timeout(inner.settings.tag_timeout, inner.parts.decision.decide(&input)).await.ok()?.ok()?;
    record(inner, id, Stage::DecisionDone);
    Some(d)
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

fn health_now(inner: &Inner) -> PipelineHealth {
    let mut h = inner.health.lock().unwrap().clone();
    h.capturing = inner.live.lock().unwrap().session_id.is_some();
    h
}

fn build_status(inner: &Inner) -> Option<AgentStatus> {
    let f = inner.status_provider.lock().unwrap().clone()?;
    Some(f(&health_now(inner)))
}

/// Record a health fact and push a fresh status.
fn set_health(inner: &Inner, f: impl FnOnce(&mut PipelineHealth)) {
    f(&mut inner.health.lock().unwrap());
    inner.status_now.notify_one();
}

/// CR-23: `agent.status` every [`STATUS_EVERY`] and whenever nudged. Sends go straight to the
/// transport and are never logged (a dead link must not feed the log it is reporting).
async fn status_loop(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(STATUS_EVERY);
    loop {
        tokio::select! {
            _ = inner.cancel.cancelled() => break,
            _ = tick.tick() => {}
            _ = inner.status_now.notified() => {}
        }
        if let Some(s) = build_status(&inner) {
            let _ = inner.parts.transport.send(None, Message::AgentStatus(s)).await;
        }
    }
}

/// CR-23: new log lines to the phone, batched. Same no-logging rule as [`status_loop`].
async fn log_forward_loop(inner: Arc<Inner>) {
    use tokio::sync::broadcast::error::RecvError;
    let mut rx = ssc_core::logbus::subscribe();
    loop {
        let first = tokio::select! {
            _ = inner.cancel.cancelled() => break,
            r = rx.recv() => match r {
                Ok(e) => e,
                Err(RecvError::Lagged(_)) => continue, // the backlog on agent.sync has them
                Err(RecvError::Closed) => break,
            },
        };
        let mut entries = vec![first];
        let deadline = tokio::time::Instant::now() + LOG_BATCH;
        while entries.len() < ssc_core::logbus::BACKLOG {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(e)) => entries.push(e),
                Ok(Err(RecvError::Lagged(_))) => {}
                _ => break,
            }
        }
        let _ = inner.parts.transport.send(None, Message::AgentLog(AgentLog { entries })).await;
    }
}

fn meta(inner: &Inner, id: Uuid) -> Option<AnswerMeta> {
    inner.answers.lock().unwrap().get(&id).map(|(_, m)| m.clone())
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
        tag: false,
        screen: None,
        note: None,
        prev_answer: None,
    }
}

/// E (CR-23b): the screen could not be captured. A card that says so (instead of an answer
/// guessed from the window text), an error in the health line, and a log line.
async fn capture_failed(inner: &Arc<Inner>, id: Uuid, profile: &str, why: String) {
    log(format!("screen read: could not capture the screen: {why}"));
    let now = inner.parts.clock.now_ms();
    set_health(inner, |h| h.last_grab = Some((now, CheckState::Error, why.clone())));
    let empty = ScreenGrab::from_snapshot(
        ScreenSnapshot {
            app: String::new(),
            window_title: String::new(),
            nodes: vec![],
            source: ssc_core::types::ScreenTextSource::Accessibility,
            captured_at_ms: now,
        },
        Some(why.clone()),
    );
    let job = Job::screen(id, Trigger::Hotkey, profile.to_string(), Arc::new(empty));
    let sid = inner.live.lock().unwrap().session_id;
    let (_mtx, mrx) = watch::channel::<Option<String>>(Some("none".into()));
    let mut p = Push { started: false, first_sent: false, detail_text: String::new(), all_text: String::new() };
    ensure_start(inner, &job, sid, &mrx, &mut p).await;
    let text = limits::clip(&format!("Couldn't capture the screen: {why}"), 2000);
    send(inner, sid, Message::AnswerDelta(AnswerDelta { answer_id: id, block: Block::Cue, text })).await;
    finish(inner, &job, sid, error_end(id), "none".into()).await;
}

fn current_profile(inner: &Inner) -> String {
    let p = inner.live.lock().unwrap().profile.clone();
    if p.is_empty() { "default".into() } else { p }
}

fn remember_grab(inner: &Inner, id: Uuid, grab: Arc<ScreenGrab>) {
    let mut g = inner.screen_grabs.lock().unwrap();
    g.push((id, grab));
    let excess = g.len().saturating_sub(MAX_REMEMBERED_GRABS);
    g.drain(..excess);
}

fn remembered_grab(inner: &Inner, id: Uuid) -> Option<Arc<ScreenGrab>> {
    inner.screen_grabs.lock().unwrap().iter().rev().find(|(k, _)| *k == id).map(|(_, g)| g.clone())
}

/// "Again" / "shorter" / "number" on a screen answer: the same grab again (CR-20). `None` when
/// the grab is no longer remembered; the caller then grabs the screen afresh.
fn screen_command_job(inner: &Inner, m: &AnswerMeta, of: Uuid) -> Option<Job> {
    let grab = remembered_grab(inner, of)?;
    let id = Uuid::new_v4();
    remember_grab(inner, id, grab.clone());
    Some(Job::screen(id, Trigger::Command, m.profile.clone(), grab))
}

fn answer_cancel(inner: &Inner) -> CancellationToken {
    inner.live.lock().unwrap().session_cancel.clone().unwrap_or_else(|| inner.cancel.child_token())
}

async fn handle_command(inner: Arc<Inner>, c: Command) {
    let mem = inner.parts.memory.clone();
    let job = match c {
        Command::AgentSync => {
            // CR-23: status first (the phone's status line), then the backlog.
            if let Some(s) = build_status(&inner) {
                let _ = inner.parts.transport.send(None, Message::AgentStatus(s)).await;
            }
            let entries = ssc_core::logbus::backlog();
            if !entries.is_empty() {
                let _ = inner.parts.transport.send(None, Message::AgentLog(AgentLog { entries })).await;
            }
            None
        }
        Command::Screen => {
            // CR-21: same path as the screen hotkey.
            let _ = Pipeline { inner: inner.clone() }.answer_from_screen();
            None
        }
        Command::Pin { answer_id } => {
            if let Err(e) = blocking(move || mem.pin_answer(answer_id, true)).await {
                log(format!("pin {answer_id}: {e}"));
            }
            None
        }
        Command::Regenerate { answer_id } | Command::Shorter { answer_id } | Command::Number { answer_id }
            if meta(&inner, answer_id).is_some_and(|m| m.format == AnswerFormat::Screen) =>
        {
            let m = meta(&inner, answer_id).expect("checked");
            let (shorter, note) = match c {
                Command::Shorter { .. } => (true, None),
                Command::Number { .. } => (false, Some(SCREEN_NUMBER_NOTE)),
                _ => (false, None),
            };
            match screen_command_job(&inner, &m, answer_id) {
                Some(j) => Some(Job { shorter, note, ..j }),
                None => {
                    // The grab was dropped (older than the last few screen answers): read the
                    // screen again.
                    let _ = Pipeline { inner: inner.clone() }.answer_from_screen();
                    None
                }
            }
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
        // G (CR-23b): a follow-up on a screen card is about that screenshot: same grab, the
        // user's question, and the earlier answer, through the screen prompt.
        Command::Followup { answer_id: Some(of), text }
            if meta(&inner, of).is_some_and(|m| m.format == AnswerFormat::Screen)
                && remembered_grab(&inner, of).is_some() =>
        {
            let m = meta(&inner, of).expect("checked");
            let grab = remembered_grab(&inner, of).expect("checked");
            let id = Uuid::new_v4();
            remember_grab(&inner, id, grab.clone());
            let label = grab.label();
            let question = if label.is_empty() {
                format!("The user asks: {text}")
            } else {
                format!("{label}\nThe user asks: {text}")
            };
            Some(Job {
                question,
                prev_answer: (!m.text.is_empty()).then(|| m.text.clone()),
                ..Job::screen(id, Trigger::Command, m.profile.clone(), grab)
            })
        }
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
        run_answer(inner, job, cancel, None).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_answers_are_capped_oldest_first() {
        let mut m = HashMap::new();
        let ids: Vec<Uuid> = (0..10).map(|_| Uuid::new_v4()).collect();
        for (i, id) in ids.iter().enumerate() {
            insert_capped(&mut m, *id, i as u64, i, 4);
        }
        assert_eq!(m.len(), 4);
        assert!(ids[6..].iter().all(|id| m.contains_key(id)));
        assert!(ids[..6].iter().all(|id| !m.contains_key(id)));
    }

    #[test]
    fn unreported_first_frames_expire() {
        let mut ff = HashMap::new();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        insert_first_frame(&mut ff, a, 1_000);
        insert_first_frame(&mut ff, a, 2_000);
        assert_eq!(ff[&a], 1_000, "the first frame wins");
        insert_first_frame(&mut ff, b, 1_000 + FIRST_FRAME_TTL_MS);
        assert!(!ff.contains_key(&a), "older than the TTL: dropped");
        assert!(ff.contains_key(&b));
    }
}
