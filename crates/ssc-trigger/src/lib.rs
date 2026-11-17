//! `ssc-trigger`: decides when an answer should be generated (Build Sheet §6 W1-TRIGGER,
//! spec "Decision model", "Answer format: cue mode", "Data flow" step 3).
//!
//! Inputs: transcript segments, screen diffs, hotkey presses and an optional passive timer.
//! Output: a [`TriggerEvent`] (fire an answer) or nothing.
//!
//! ## Rules that stay in code (Build Sheet §3 rule 7)
//! - [`TriggerEngine::hotkey`] is a synchronous function: it never awaits the decision model, so a
//!   decision error, timeout or hang cannot block it. It always returns an event.
//! - The engine holds the session [`Mode`] only as a read-only setting chosen by code
//!   ([`TriggerSettings::mode`], [`TriggerEngine::set_mode`]). Nothing here takes a [`Decision`]
//!   and changes mode or redaction; [`TriggerEvent`] has no mode or redaction field. A decision
//!   only picks intent/format/tier/profile/urgency, and its profile is accepted only if it is one
//!   of the allowed profile ids. A decision whose tier confidence is below the threshold routes
//!   to the strong tier (hotkey and automatic triggers alike).
//!
//! ## Paths
//! - **Hotkey:** fires now. Tier is fast unless the latest decision for the current "them"
//!   utterance is below [`TriggerSettings::confidence_threshold`] (or itself chose strong).
//! - **Question in "them" speech:** an utterance ends (`ends_utterance`), [`heuristics::classify`]
//!   says it's a question or pushback, and no more "them" speech arrives within the silence
//!   window. Cloud mode: one [`DecisionModel`] call (timeout [`TriggerSettings::decision_timeout_ms`])
//!   gates and tags it; if that call fails the heuristic result fires instead. Local mode: fires
//!   immediately from the heuristics; the decision model only tags in the background
//!   ([`TriggerEngine::with_tag_sink`]).
//! - **Big screen change:** a diff with at least `big_screen_change_lines` changed lines starts a
//!   trailing debounce; the trigger fires once the screen has been quiet for
//!   [`TriggerSettings::screen_settle_ms`], at most once per dedupe window. Gated by the decision
//!   model (both modes); a decision failure means nothing fires.
//! - **Passive timer:** off by default; when set, fires after that long without any trigger.
//!   Gated like a screen change.
//!
//! Dedupe: the same question (normalised text) gets no second automatic answer within
//! `dedupe_window_ms`. Small talk or `needs_answer = false` gives nothing.

pub mod heuristics;

use heuristics::{classify, heuristic_intent, normalize};
use serde::{Deserialize, Serialize};
use ssc_core::config::Config;
use ssc_core::decision::{Decision, DecisionInput, Tier};
use ssc_core::protocol::{AnswerFormat, Intent, Mode, Speaker, Trigger, Urgency};
use ssc_core::timing::{Stage, Timing};
use ssc_core::traits::{Clock, DecisionModel};
use ssc_core::types::{SCREEN_DIFF_MAX_BYTES, ScreenDiff, TranscriptSegment};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// An answer should be generated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerEvent {
    pub answer_id: Uuid,
    pub trigger: Trigger,
    /// The "them" utterance the answer is for (may be empty for screen/timer/hotkey).
    pub question: String,
    pub format: AnswerFormat,
    pub intent: Intent,
    /// Always `Fast` or `Strong`, never `None`.
    pub tier: Tier,
    /// One of [`TriggerSettings::profiles`].
    pub profile: String,
    pub urgency: Urgency,
    /// The decision that tagged this trigger, if one was awaited (or cached, for the hotkey).
    pub decision: Option<Decision>,
    /// When the trigger fired (engine clock, unix ms).
    pub t_ms: u64,
    /// End of the question's speech (`speech_end`), for question triggers.
    pub speech_end_ms: Option<u64>,
}

/// A background decision for an answer that already fired (offline mode).
#[derive(Debug, Clone, PartialEq)]
pub struct Tag {
    pub answer_id: Uuid,
    pub decision: Decision,
}

/// Input for [`TriggerEngine::spawn`].
#[derive(Debug, Clone, PartialEq)]
pub enum TriggerInput {
    Transcript(TranscriptSegment),
    Screen(ScreenDiff),
    Hotkey,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TriggerSettings {
    /// Chosen by the user / pipeline code. Never changed by a decision.
    pub mode: Mode,
    pub question_silence_ms: u64,
    pub dedupe_window_ms: u64,
    pub decision_timeout_ms: u64,
    pub confidence_threshold: f64,
    pub big_screen_change_lines: usize,
    /// Quiet time after a big screen change before it fires.
    pub screen_settle_ms: u64,
    /// None = passive timer off (default).
    pub passive_timer_ms: Option<u64>,
    /// Allowed profile ids (sent to the decision model).
    pub profiles: Vec<String>,
    /// Used when no decision picked a valid profile.
    pub active_profile: String,
}

/// Default quiet time after a big screen change before triggering.
pub const DEFAULT_SCREEN_SETTLE_MS: u64 = 1000;

impl TriggerSettings {
    pub fn from_config(cfg: &Config, profiles: Vec<String>, active_profile: impl Into<String>) -> Self {
        Self {
            mode: cfg.mode,
            question_silence_ms: cfg.timeouts.question_silence_ms,
            dedupe_window_ms: cfg.timeouts.dedupe_window_ms,
            decision_timeout_ms: cfg.timeouts.decision_ms,
            confidence_threshold: cfg.decision_confidence_threshold,
            big_screen_change_lines: cfg.big_screen_change_lines,
            screen_settle_ms: DEFAULT_SCREEN_SETTLE_MS,
            passive_timer_ms: cfg.passive_timer_s.map(|s| u64::from(s) * 1000),
            profiles,
            active_profile: active_profile.into(),
        }
    }
}

/// Spec table "When it switches format". `None` = nothing shown (small talk).
pub fn format_for(intent: Intent) -> Option<AnswerFormat> {
    match intent {
        Intent::Pricing | Intent::Scheduling => Some(AnswerFormat::Fact),
        Intent::Objection => Some(AnswerFormat::Objection),
        Intent::Technical | Intent::None => Some(AnswerFormat::Cue),
        Intent::SmallTalk => None,
    }
}

// ---------- decision input budget ----------

/// Conservative token estimate (~3 bytes per token; English averages closer to 4).
pub fn estimate_tokens(input: &DecisionInput) -> usize {
    let bytes =
        input.question.len() + input.screen_diff.len() + input.profiles.iter().map(|p| p.len() + 2).sum::<usize>();
    bytes.div_ceil(3)
}

/// Total bytes for question + screen diff + profiles: keeps [`estimate_tokens`] under 500.
pub const DECISION_INPUT_MAX_BYTES: usize = 1400;
const QUESTION_MAX_BYTES: usize = 480;
const PROFILES_MAX_BYTES: usize = 200;

/// Longest prefix of `s` within `max` bytes, on a char boundary.
fn head(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

/// Longest suffix of `s` within `max` bytes, on a char boundary.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = s.len() - max;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

/// Build a decision input under the token budget. The question keeps its end (where the ask is);
/// the screen diff keeps its start (newest content first, per [`ScreenDiff`]).
pub fn decision_input(question: &str, screen_diff: &str, profiles: &[String]) -> DecisionInput {
    let question = tail(question.trim(), QUESTION_MAX_BYTES).to_string();
    let mut used = 0;
    let mut kept = vec![];
    for p in profiles {
        if used + p.len() + 2 > PROFILES_MAX_BYTES {
            break;
        }
        used += p.len() + 2;
        kept.push(p.clone());
    }
    let profiles = kept;
    let left = DECISION_INPUT_MAX_BYTES.saturating_sub(question.len() + used);
    let screen_diff = head(screen_diff, left.min(SCREEN_DIFF_MAX_BYTES)).to_string();
    DecisionInput { question, screen_diff, profiles }
}

// ---------- engine ----------

#[derive(Debug, Clone)]
struct PendingQuestion {
    answer_id: Uuid,
    text: String,
    speech_end_ms: u64,
    deadline_ms: u64,
}

#[derive(Debug, Clone)]
struct PendingScreen {
    lines: Vec<String>,
    deadline_ms: u64,
}

#[derive(Default)]
struct State {
    /// Final "them" text of the utterance in progress.
    them_buffer: String,
    /// Latest partial "them" text after the buffer.
    them_partial: String,
    /// Last completed "them" utterance.
    last_utterance: String,
    /// Latest screen diff text, for decision inputs.
    last_screen_diff: String,
    pending_question: Option<PendingQuestion>,
    pending_screen: Option<PendingScreen>,
    last_screen_fire_ms: Option<u64>,
    next_timer_ms: Option<u64>,
    /// normalised question → last trigger time.
    answered: HashMap<String, u64>,
    /// Latest decision and the normalised utterance it was for (hotkey tier).
    last_decision: Option<(String, Decision)>,
}

/// A trigger that fired and still needs its decision step.
#[derive(Debug, Clone)]
struct Candidate {
    answer_id: Uuid,
    trigger: Trigger,
    question: String,
    screen_diff: String,
    t_ms: u64,
    speech_end_ms: Option<u64>,
}

pub struct TriggerEngine {
    settings: Mutex<TriggerSettings>,
    decision: Arc<dyn DecisionModel>,
    clock: Arc<dyn Clock>,
    timing: Option<Arc<Timing>>,
    tags: Option<mpsc::UnboundedSender<Tag>>,
    state: Mutex<State>,
}

impl TriggerEngine {
    pub fn new(settings: TriggerSettings, decision: Arc<dyn DecisionModel>, clock: Arc<dyn Clock>) -> Self {
        let next_timer_ms = settings.passive_timer_ms.map(|p| clock.now_ms() + p);
        Self {
            settings: Mutex::new(settings),
            decision,
            clock,
            timing: None,
            tags: None,
            state: Mutex::new(State { next_timer_ms, ..State::default() }),
        }
    }

    /// Record `speech_end`, `transcript_final`, `trigger_fired` and `decision_done`.
    pub fn with_timing(mut self, timing: Arc<Timing>) -> Self {
        self.timing = Some(timing);
        self
    }

    /// Receive background decisions for answers that fired without waiting for one (offline).
    pub fn with_tag_sink(mut self, tx: mpsc::UnboundedSender<Tag>) -> Self {
        self.tags = Some(tx);
        self
    }

    pub fn mode(&self) -> Mode {
        self.settings.lock().unwrap().mode
    }

    /// Switch between cloud and local triggering (user setting or network loss). Code only.
    pub fn set_mode(&self, mode: Mode) {
        self.settings.lock().unwrap().mode = mode;
    }

    pub fn settings(&self) -> TriggerSettings {
        self.settings.lock().unwrap().clone()
    }

    fn record(&self, answer_id: Uuid, stage: Stage, t_ms: u64) {
        if let Some(t) = &self.timing {
            t.record(answer_id, stage, t_ms);
        }
    }

    /// The hotkey ("answer now"). Synchronous: it cannot wait on the decision model, so it
    /// always fires immediately.
    pub fn hotkey(&self) -> TriggerEvent {
        let s = self.settings();
        let now = self.clock.now_ms();
        let mut st = self.state.lock().unwrap();
        let question = current_utterance(&st);
        let key = normalize(&question);
        let decision = st.last_decision.as_ref().filter(|(k, _)| *k == key && !key.is_empty()).map(|(_, d)| d.clone());
        let tier = match &decision {
            Some(d) if d.answer_tier.p < s.confidence_threshold || d.answer_tier.value == Tier::Strong => Tier::Strong,
            _ => Tier::Fast,
        };
        let intent = decision.as_ref().map(|d| d.intent.value).unwrap_or_else(|| heuristic_intent(&question));
        // The user asked explicitly: small talk still gets a cue answer.
        let format = format_for(intent).unwrap_or(AnswerFormat::Cue);
        let profile = decision.as_ref().and_then(|d| valid_profile(&s, d)).unwrap_or_else(|| s.active_profile.clone());
        if !key.is_empty() {
            st.answered.insert(key, now);
        }
        // An explicit answer supersedes a pending automatic one for the same speech.
        st.pending_question = None;
        reset_timer(&mut st, &s, now);
        drop(st);
        let ev = TriggerEvent {
            answer_id: Uuid::new_v4(),
            trigger: Trigger::Hotkey,
            question,
            format,
            intent,
            tier,
            profile,
            urgency: Urgency::High,
            decision,
            t_ms: now,
            speech_end_ms: None,
        };
        self.record(ev.answer_id, Stage::TriggerFired, now);
        ev
    }

    /// Feed one transcript segment; returns any triggers that fired (decisions awaited).
    pub async fn on_transcript(&self, seg: &TranscriptSegment) -> Vec<TriggerEvent> {
        let mut due = self.due();
        self.ingest_transcript(seg);
        due.extend(self.due());
        self.resolve_all(due).await
    }

    /// Feed one screen diff; returns any triggers that fired.
    pub async fn on_screen_diff(&self, diff: &ScreenDiff) -> Vec<TriggerEvent> {
        let mut due = self.due();
        self.ingest_screen(diff);
        due.extend(self.due());
        self.resolve_all(due).await
    }

    /// Fire whatever is due by the clock (silence windows, screen settle, passive timer).
    pub async fn poll(&self) -> Vec<TriggerEvent> {
        self.resolve_all(self.due()).await
    }

    async fn resolve_all(&self, due: Vec<Candidate>) -> Vec<TriggerEvent> {
        let mut out = vec![];
        for c in due {
            if let Some(ev) = self.resolve(c).await {
                out.push(ev);
            }
        }
        out
    }

    /// Next time (engine clock) at which [`Self::poll`] could fire something.
    pub fn next_deadline_ms(&self) -> Option<u64> {
        let st = self.state.lock().unwrap();
        [
            st.pending_question.as_ref().map(|p| p.deadline_ms),
            st.pending_screen.as_ref().map(|p| p.deadline_ms),
            st.next_timer_ms,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn ingest_transcript(&self, seg: &TranscriptSegment) {
        if seg.speaker != Speaker::Them {
            return;
        }
        let s = self.settings();
        let now = self.clock.now_ms();
        let mut st = self.state.lock().unwrap();
        let text = seg.text.trim();
        // More "them" speech that started inside the silence window: they weren't done. Keep the
        // buffer so the continued utterance is judged as a whole.
        if !text.is_empty() && st.pending_question.as_ref().is_some_and(|p| seg.t0_ms < p.deadline_ms) {
            let p = st.pending_question.take().unwrap();
            st.them_buffer = p.text;
        }
        if seg.is_final {
            if !text.is_empty() {
                if !st.them_buffer.is_empty() {
                    st.them_buffer.push(' ');
                }
                st.them_buffer.push_str(text);
            }
            st.them_partial.clear();
        } else {
            st.them_partial = text.to_string();
        }
        if !seg.ends_utterance {
            return;
        }
        let mut utterance = std::mem::take(&mut st.them_buffer);
        if !seg.is_final && !st.them_partial.is_empty() {
            if !utterance.is_empty() {
                utterance.push(' ');
            }
            utterance.push_str(&std::mem::take(&mut st.them_partial));
        }
        if utterance.is_empty() {
            return;
        }
        st.last_utterance = utterance.clone();
        if !classify(&utterance).wants_answer() {
            return;
        }
        let answer_id = Uuid::new_v4();
        drop(st);
        self.record(answer_id, Stage::SpeechEnd, seg.t1_ms);
        self.record(answer_id, Stage::TranscriptFinal, now);
        self.state.lock().unwrap().pending_question = Some(PendingQuestion {
            answer_id,
            text: utterance,
            speech_end_ms: seg.t1_ms,
            deadline_ms: seg.t1_ms + s.question_silence_ms,
        });
    }

    fn ingest_screen(&self, diff: &ScreenDiff) {
        if diff.is_empty() {
            return;
        }
        let s = self.settings();
        let now = self.clock.now_ms();
        let mut st = self.state.lock().unwrap();
        st.last_screen_diff = diff.text();
        let deadline_ms = now + s.screen_settle_ms;
        if let Some(p) = st.pending_screen.as_mut() {
            // Still changing: push the deadline and keep the newest lines first.
            let mut lines = diff.added_lines.clone();
            lines.append(&mut p.lines);
            p.lines = lines;
            p.deadline_ms = deadline_ms;
        } else if diff.changed_lines() >= s.big_screen_change_lines {
            st.pending_screen = Some(PendingScreen { lines: diff.added_lines.clone(), deadline_ms });
        }
    }

    /// Take everything due now, apply dedupe, record `trigger_fired`.
    fn due(&self) -> Vec<Candidate> {
        let s = self.settings();
        let now = self.clock.now_ms();
        let mut st = self.state.lock().unwrap();
        st.answered.retain(|_, t| now.saturating_sub(*t) < s.dedupe_window_ms);
        let mut out = vec![];

        if st.pending_question.as_ref().is_some_and(|p| now >= p.deadline_ms) {
            let p = st.pending_question.take().unwrap();
            let screen_diff = st.last_screen_diff.clone();
            if let std::collections::hash_map::Entry::Vacant(slot) = st.answered.entry(normalize(&p.text)) {
                slot.insert(now);
                out.push(Candidate {
                    answer_id: p.answer_id,
                    trigger: Trigger::Question,
                    question: p.text,
                    screen_diff,
                    t_ms: now,
                    speech_end_ms: Some(p.speech_end_ms),
                });
            }
        }

        if st.pending_screen.as_ref().is_some_and(|p| now >= p.deadline_ms) {
            let p = st.pending_screen.take().unwrap();
            let cooled = st.last_screen_fire_ms.is_none_or(|t| now.saturating_sub(t) >= s.dedupe_window_ms);
            if cooled {
                st.last_screen_fire_ms = Some(now);
                out.push(Candidate {
                    answer_id: Uuid::new_v4(),
                    trigger: Trigger::Screen,
                    question: st.last_utterance.clone(),
                    screen_diff: p.lines.join("\n"),
                    t_ms: now,
                    speech_end_ms: None,
                });
            }
        }

        if st.next_timer_ms.is_some_and(|t| now >= t) {
            out.push(Candidate {
                answer_id: Uuid::new_v4(),
                trigger: Trigger::Timer,
                question: st.last_utterance.clone(),
                screen_diff: st.last_screen_diff.clone(),
                t_ms: now,
                speech_end_ms: None,
            });
        }
        if !out.is_empty() {
            reset_timer(&mut st, &s, now);
        }
        drop(st);
        for c in &out {
            self.record(c.answer_id, Stage::TriggerFired, c.t_ms);
        }
        out
    }

    async fn decide(&self, c: &Candidate, s: &TriggerSettings) -> Option<Decision> {
        decide_with(&*self.decision, &*self.clock, self.timing.as_deref(), c, s).await
    }

    /// The decision step for one candidate.
    async fn resolve(&self, c: Candidate) -> Option<TriggerEvent> {
        let s = self.settings();
        if c.trigger == Trigger::Question && s.mode == Mode::Local {
            // Offline: fire from the end-of-question rule; tag in the background.
            self.spawn_background_tag(&c, &s);
            return heuristic_event(&c, &s);
        }
        match self.decide(&c, &s).await {
            Some(d) => {
                if c.trigger == Trigger::Question {
                    self.state.lock().unwrap().last_decision = Some((normalize(&c.question), d.clone()));
                }
                decided_event(&c, &s, d)
            }
            // A failed decision never blocks a detected question; screen/timer stay quiet.
            None if c.trigger == Trigger::Question => heuristic_event(&c, &s),
            None => None,
        }
    }

    fn spawn_background_tag(&self, c: &Candidate, s: &TriggerSettings) {
        let (decision, clock, timing, tags) =
            (self.decision.clone(), self.clock.clone(), self.timing.clone(), self.tags.clone());
        let (c, s) = (c.clone(), s.clone());
        tokio::spawn(async move {
            if let Some(d) = decide_with(&*decision, &*clock, timing.as_deref(), &c, &s).await
                && let Some(tx) = tags
            {
                let _ = tx.send(Tag { answer_id: c.answer_id, decision: d });
            }
        });
    }

    /// Run the engine as a task: inputs in, events out. Each decision call runs in its own task,
    /// so a slow or hung decision model never delays the hotkey or later inputs.
    pub fn spawn(
        self: Arc<Self>,
        mut rx: mpsc::Receiver<TriggerInput>,
        tx: mpsc::Sender<TriggerEvent>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let wait = self
                    .next_deadline_ms()
                    .map(|d| Duration::from_millis(d.saturating_sub(self.clock.now_ms())))
                    .unwrap_or(Duration::from_secs(3600));
                let input = tokio::select! {
                    _ = cancel.cancelled() => break,
                    input = rx.recv() => match input {
                        None => break,
                        Some(i) => Some(i),
                    },
                    _ = tokio::time::sleep(wait) => None,
                };
                // Anything already due fires before new input can supersede it.
                let mut due = self.due();
                match input {
                    Some(TriggerInput::Hotkey) => {
                        if tx.send(self.hotkey()).await.is_err() {
                            break;
                        }
                    }
                    Some(TriggerInput::Transcript(seg)) => self.ingest_transcript(&seg),
                    Some(TriggerInput::Screen(diff)) => self.ingest_screen(&diff),
                    None => {}
                }
                due.extend(self.due());
                for c in due {
                    let (engine, tx) = (self.clone(), tx.clone());
                    tokio::spawn(async move {
                        if let Some(ev) = engine.resolve(c).await {
                            let _ = tx.send(ev).await;
                        }
                    });
                }
            }
        })
    }
}

/// The question in progress (or the last completed one) for the hotkey.
fn current_utterance(st: &State) -> String {
    let live = [st.them_buffer.as_str(), st.them_partial.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if live.is_empty() { st.last_utterance.clone() } else { live }
}

fn reset_timer(st: &mut State, s: &TriggerSettings, now: u64) {
    st.next_timer_ms = s.passive_timer_ms.map(|p| now + p);
}

fn valid_profile(s: &TriggerSettings, d: &Decision) -> Option<String> {
    s.profiles.iter().find(|p| **p == d.profile.value).cloned()
}

async fn decide_with(
    model: &dyn DecisionModel,
    clock: &dyn Clock,
    timing: Option<&Timing>,
    c: &Candidate,
    s: &TriggerSettings,
) -> Option<Decision> {
    let input = decision_input(&c.question, &c.screen_diff, &s.profiles);
    let res = tokio::time::timeout(Duration::from_millis(s.decision_timeout_ms), model.decide(&input)).await;
    let d = match res {
        Ok(Ok(d)) => Some(d),
        Ok(Err(_)) | Err(_) => None,
    };
    if d.is_some()
        && let Some(t) = timing
    {
        t.record(c.answer_id, Stage::DecisionDone, clock.now_ms());
    }
    d
}

/// Event from the deterministic rules alone (offline, or the decision failed).
fn heuristic_event(c: &Candidate, s: &TriggerSettings) -> Option<TriggerEvent> {
    let intent = heuristic_intent(&c.question);
    Some(TriggerEvent {
        answer_id: c.answer_id,
        trigger: c.trigger,
        question: c.question.clone(),
        format: format_for(intent)?,
        intent,
        tier: Tier::Fast,
        profile: s.active_profile.clone(),
        urgency: Urgency::Medium,
        decision: None,
        t_ms: c.t_ms,
        speech_end_ms: c.speech_end_ms,
    })
}

/// Event gated and tagged by a decision. Small talk / no answer / tier none → nothing.
fn decided_event(c: &Candidate, s: &TriggerSettings, d: Decision) -> Option<TriggerEvent> {
    if !d.needs_answer.value || d.answer_tier.value == Tier::None {
        return None;
    }
    let format = format_for(d.intent.value)?;
    let tier = if d.answer_tier.value == Tier::Strong || d.answer_tier.p < s.confidence_threshold {
        Tier::Strong
    } else {
        Tier::Fast
    };
    Some(TriggerEvent {
        answer_id: c.answer_id,
        trigger: c.trigger,
        question: c.question.clone(),
        format,
        intent: d.intent.value,
        tier,
        profile: valid_profile(s, &d).unwrap_or_else(|| s.active_profile.clone()),
        urgency: d.urgency.value,
        decision: Some(d),
        t_ms: c.t_ms,
        speech_end_ms: c.speech_end_ms,
    })
}
