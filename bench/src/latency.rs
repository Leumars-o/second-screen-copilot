//! `bench latency`: replays fixture calls through a pipeline, reads the `ssc-core::timing` JSONL it
//! wrote, and reports p50/p95 per stage and end to end (speech_end → phone_rendered).
//!
//! Targets (Build Sheet §1, spec "Latency"): cloud p50 < 1.5 s, p95 < 2.5 s. Offline ~2–2.5 s is a
//! design target only, reported but never enforced. Enforcement (non-zero exit) is for `--live`
//! runs and `--enforce`; fixture numbers are scripted, so they only prove the harness works.
//!
//! The pipeline is behind [`PipelineDriver`]. Fixture mode uses [`MockPipeline`] (ssc-core mocks on
//! a mock clock). The real pipeline is the single integration point [`real_pipeline`].

use crate::report::{Report, ms_cell};
use crate::stats::Summary;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use ssc_core::decision::DecisionInput;
use ssc_core::mock::{MockAnswer, MockClock, MockDecision, decision_answer};
use ssc_core::protocol::Intent;
use ssc_core::timing::{Stage, Timing, TimingEvent, spans};
use ssc_core::traits::{AnswerModel, Cancel, Clock, DecisionModel};
use ssc_core::types::Prompt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

pub const CLOUD_P50_MS: u64 = 1500;
pub const CLOUD_P95_MS: u64 = 2500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Cloud,
    Local,
}

impl RunMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cloud" => Some(Self::Cloud),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cloud => "cloud",
            Self::Local => "local",
        }
    }
}

/// One recorded call: the questions asked and, for the mock pipeline, how long each stage took.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureCall {
    pub id: String,
    pub mode: RunMode,
    pub utterances: Vec<Utterance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Utterance {
    pub question: String,
    /// Mock pipeline only: ms spent reaching each stage from the previous one.
    pub stage_ms: StageMs,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StageMs {
    pub transcript_final: u64,
    pub trigger_fired: u64,
    pub decision_done: u64,
    pub context_ready: u64,
    pub model_request: u64,
    pub first_token: u64,
    pub frame_sent: u64,
    /// Heartbeat round trip; `phone_rendered = frame_sent + rtt/2 + delta_ms` (§4.6).
    pub rtt_ms: u64,
    /// The phone's reported first-delta → first-paint time.
    pub phone_delta_ms: u64,
}

pub fn load_calls(path: &Path) -> Result<Vec<FixtureCall>> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))
}

pub fn fixture_calls_path() -> PathBuf {
    crate::bench_path("fixtures/latency/calls.json")
}

/// Drives one call through a pipeline, recording timing events into `timing`.
#[async_trait]
pub trait PipelineDriver: Send + Sync {
    fn name(&self) -> &str;
    async fn replay(&self, call: &FixtureCall, timing: &Timing) -> Result<()>;
}

/// ssc-core mocks wired in pipeline order, stamped on a [`MockClock`] advanced by the fixture's
/// stage durations. The decision and answer calls really happen (through the traits); only time
/// is scripted.
pub struct MockPipeline {
    clock: MockClock,
    decision: MockDecision,
    answer: MockAnswer,
}

impl Default for MockPipeline {
    fn default() -> Self {
        Self {
            clock: MockClock::new(1_763_290_000_000),
            decision: MockDecision::always(decision_answer(Intent::Pricing, "sales")),
            answer: MockAnswer::cue_answer("mock/answer", true).with_first_token_delay(std::time::Duration::ZERO),
        }
    }
}

#[async_trait]
impl PipelineDriver for MockPipeline {
    fn name(&self) -> &str {
        "mock pipeline (ssc-core mocks, scripted stage times)"
    }

    async fn replay(&self, call: &FixtureCall, timing: &Timing) -> Result<()> {
        for u in &call.utterances {
            let id = Uuid::new_v4();
            let s = u.stage_ms;
            let step = |stage: Stage, ms: u64| {
                self.clock.advance(ms);
                timing.record(id, stage, self.clock.now_ms());
            };
            step(Stage::SpeechEnd, 0);
            step(Stage::TranscriptFinal, s.transcript_final);
            step(Stage::TriggerFired, s.trigger_fired);
            let input = DecisionInput {
                question: u.question.clone(),
                screen_diff: String::new(),
                profiles: vec!["sales".into()],
            };
            let d = self.decision.decide(&input).await?;
            step(Stage::DecisionDone, s.decision_done);
            if !d.needs_answer.value {
                continue;
            }
            step(Stage::ContextReady, s.context_ready);
            step(Stage::ModelRequest, s.model_request);
            let prompt = Prompt {
                prefix: "rules".into(),
                dynamic: u.question.clone(),
                max_tokens: 160,
                temperature: 0.2,
                stop: vec![],
            };
            let (tx, mut rx) = mpsc::channel(64);
            let stream = self.answer.stream(&prompt, tx, Cancel::new());
            let (res, first) = tokio::join!(stream, rx.recv());
            res?;
            anyhow::ensure!(first.is_some(), "mock answer produced no token");
            step(Stage::FirstToken, s.first_token);
            step(Stage::FrameSent, s.frame_sent);
            let frame_sent = self.clock.now_ms();
            timing.record(id, Stage::PhoneRendered, frame_sent + s.rtt_ms / 2 + s.phone_delta_ms);
            self.clock.advance(5_000); // gap before the next question
        }
        Ok(())
    }
}

/// The real `ssc-pipeline` (W2-INTEGRATE): `~/.ssc/config.toml` + keychain keys, real decision and
/// answer models, the user's memory store. Each fixture question is played as a final "them"
/// utterance (`ssc_pipeline::replay`); STT and the phone are stand-ins, so `speech_end →
/// transcript_final` is ~0 and `phone_rendered` uses the fixture's `phone_delta_ms`.
pub fn real_pipeline() -> Result<Arc<dyn PipelineDriver>> {
    Ok(Arc::new(RealPipeline))
}

struct RealPipeline;

#[async_trait]
impl PipelineDriver for RealPipeline {
    fn name(&self) -> &str {
        "real ssc-pipeline (config + keychain models; scripted transcript)"
    }

    async fn replay(&self, call: &FixtureCall, timing: &Timing) -> Result<()> {
        let mut cfg = ssc_core::config::Config::load(&ssc_core::config::Config::default_path())?;
        cfg.mode = match call.mode {
            RunMode::Cloud => ssc_core::protocol::Mode::Cloud,
            RunMode::Local => ssc_core::protocol::Mode::Local,
        };
        let questions: Vec<String> = call.utterances.iter().map(|u| u.question.clone()).collect();
        let delta = call.utterances.first().map(|u| u.stage_ms.phone_delta_ms).unwrap_or(10) as u32;
        let secrets: Arc<dyn ssc_core::secrets::SecretStore> = Arc::new(ssc_core::secrets::KeychainStore);
        ssc_pipeline::replay::replay_questions(&cfg, secrets, &questions, delta, timing).await?;
        Ok(())
    }
}

pub fn read_jsonl(path: &Path) -> Result<Vec<TimingEvent>> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in s.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line).with_context(|| format!("{}:{}", path.display(), i + 1))?);
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct Analysis {
    pub mode: RunMode,
    pub answers: usize,
    /// Consecutive stage pairs.
    pub stages: Vec<(Stage, Stage, Option<Summary>)>,
    pub to_first_token: Option<Summary>,
    pub to_frame_sent: Option<Summary>,
    pub to_phone: Option<Summary>,
    /// True when no `phone_rendered` events exist and end-to-end fell back to `frame_sent`.
    pub e2e_is_frame_sent: bool,
}

impl Analysis {
    pub fn e2e(&self) -> Option<Summary> {
        if self.e2e_is_frame_sent { self.to_frame_sent } else { self.to_phone }
    }

    /// Cloud thresholds; local is never enforced. None when there is no end-to-end sample.
    pub fn meets_targets(&self) -> Option<bool> {
        match self.mode {
            RunMode::Local => Some(true),
            RunMode::Cloud => self.e2e().map(|s| s.p50 <= CLOUD_P50_MS && s.p95 <= CLOUD_P95_MS),
        }
    }
}

pub fn analyse(events: &[TimingEvent], mode: RunMode) -> Analysis {
    let answers = events.iter().map(|e| e.answer_id).collect::<std::collections::HashSet<_>>().len();
    let stages =
        Stage::ALL.windows(2).map(|w| (w[0], w[1], Summary::of(&spans(events, w[0], w[1])))).collect::<Vec<_>>();
    let to_phone = Summary::of(&spans(events, Stage::SpeechEnd, Stage::PhoneRendered));
    Analysis {
        mode,
        answers,
        stages,
        to_first_token: Summary::of(&spans(events, Stage::SpeechEnd, Stage::FirstToken)),
        to_frame_sent: Summary::of(&spans(events, Stage::SpeechEnd, Stage::FrameSent)),
        to_phone,
        e2e_is_frame_sent: to_phone.is_none(),
    }
}

fn stage_name(s: Stage) -> String {
    serde_json::to_value(s).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

pub fn render(r: &mut Report, a: &Analysis, source: &str) {
    r.h2(&format!("{} mode — {source}", a.mode.as_str()));
    r.bullet(&format!("answers with events: {}", a.answers));
    let verdict = match (a.mode, a.meets_targets()) {
        (RunMode::Local, _) => "local: reported only (design target ~2–2.5 s, not enforced)".to_string(),
        (RunMode::Cloud, None) => "cloud: no end-to-end samples".to_string(),
        (RunMode::Cloud, Some(ok)) => format!(
            "cloud target p50 ≤ {CLOUD_P50_MS} ms, p95 ≤ {CLOUD_P95_MS} ms: **{}**",
            if ok { "PASS" } else { "FAIL" }
        ),
    };
    r.bullet(&verdict);
    if a.e2e_is_frame_sent {
        r.bullet("no `phone_rendered` events: end to end uses `frame_sent` (phone render time missing)");
    }
    r.end_list();
    let mut rows: Vec<Vec<String>> = a
        .stages
        .iter()
        .map(|(f, t, s)| vec![format!("{} → {}", stage_name(*f), stage_name(*t)), ms_cell(*s)])
        .collect();
    rows.push(vec!["**speech_end → first_token**".into(), ms_cell(a.to_first_token)]);
    rows.push(vec!["**speech_end → frame_sent**".into(), ms_cell(a.to_frame_sent)]);
    rows.push(vec!["**speech_end → phone_rendered (end to end)**".into(), ms_cell(a.to_phone)]);
    r.table(&["span", "p50 / p95 ms"], &rows);
}

/// Replays `calls` through `driver`, one JSONL per mode in `jsonl_dir`, then analyses each file.
pub async fn replay_and_analyse(
    driver: &dyn PipelineDriver,
    calls: &[FixtureCall],
    jsonl_dir: &Path,
) -> Result<Vec<(Analysis, PathBuf)>> {
    let mut out = Vec::new();
    for mode in [RunMode::Cloud, RunMode::Local] {
        let mine: Vec<&FixtureCall> = calls.iter().filter(|c| c.mode == mode).collect();
        if mine.is_empty() {
            continue;
        }
        let path = jsonl_dir.join(format!("timing-{}.jsonl", mode.as_str()));
        let _ = std::fs::remove_file(&path);
        let timing = Timing::new(100_000).with_jsonl(&path)?;
        for c in mine {
            driver.replay(c, &timing).await.with_context(|| format!("call {}", c.id))?;
        }
        drop(timing);
        let events = read_jsonl(&path)?;
        out.push((analyse(&events, mode), path));
    }
    Ok(out)
}

/// Fixture mode: mock pipeline over `fixtures/latency/calls.json`. Returns the report text.
pub async fn run_fixture(report_path: &Path) -> Result<Report> {
    let calls = load_calls(&fixture_calls_path())?;
    let dir = report_path.parent().unwrap_or(Path::new(".")).join("latency-jsonl");
    let driver = MockPipeline::default();
    let results = replay_and_analyse(&driver, &calls, &dir).await?;
    let mut r = Report::new("bench/latency — fixture mode");
    r.para(&format!(
        "Pipeline: {}. Stage times are scripted from `fixtures/latency/calls.json`, so these numbers \
         prove the harness (timing JSONL → percentiles → verdict), not the product. Thresholds are \
         enforced only with `--live` or `--enforce`.",
        driver.name()
    ));
    for (a, path) in &results {
        render(&mut r, a, &format!("`{}`", path.display()));
    }
    r.write(report_path)?;
    Ok(r)
}

/// Analyse existing timing JSONL files (e.g. `~/.ssc/timing/<date>.jsonl` from a real run).
pub fn run_jsonl(files: &[PathBuf], mode: RunMode, report_path: &Path) -> Result<(Report, Analysis)> {
    let mut events = Vec::new();
    for f in files {
        events.extend(read_jsonl(f)?);
    }
    let a = analyse(&events, mode);
    let mut r = Report::new("bench/latency — timing log");
    let names: Vec<String> = files.iter().map(|f| format!("`{}`", f.display())).collect();
    render(&mut r, &a, &names.join(", "));
    r.write(report_path)?;
    Ok((r, a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: Uuid, stage: Stage, t: u64) -> TimingEvent {
        TimingEvent { answer_id: id, stage, t_ms: t }
    }

    #[test]
    fn cloud_thresholds() {
        let mut events = Vec::new();
        for i in 0..20u64 {
            let id = Uuid::new_v4();
            events.push(ev(id, Stage::SpeechEnd, 0));
            events.push(ev(id, Stage::PhoneRendered, 1000 + i * 10));
        }
        let a = analyse(&events, RunMode::Cloud);
        assert_eq!(a.meets_targets(), Some(true));
        // One slow outlier above p95 doesn't fail; a slow tail does.
        let id = Uuid::new_v4();
        events.push(ev(id, Stage::SpeechEnd, 0));
        events.push(ev(id, Stage::PhoneRendered, 9000));
        assert_eq!(analyse(&events, RunMode::Cloud).meets_targets(), Some(true));
        for _ in 0..3 {
            let id = Uuid::new_v4();
            events.push(ev(id, Stage::SpeechEnd, 0));
            events.push(ev(id, Stage::PhoneRendered, 3000));
        }
        let a = analyse(&events, RunMode::Cloud);
        assert_eq!(a.e2e().unwrap().p95, 3000);
        assert_eq!(a.meets_targets(), Some(false));
        assert_eq!(analyse(&events, RunMode::Local).meets_targets(), Some(true));
    }

    #[test]
    fn falls_back_to_frame_sent() {
        let id = Uuid::new_v4();
        let a = analyse(&[ev(id, Stage::SpeechEnd, 10), ev(id, Stage::FrameSent, 900)], RunMode::Cloud);
        assert!(a.e2e_is_frame_sent);
        assert_eq!(a.e2e().unwrap().p50, 890);
        assert_eq!(analyse(&[], RunMode::Cloud).meets_targets(), None);
    }

    #[test]
    fn real_pipeline_is_wired() {
        // Constructing the driver touches nothing; replay (keys, network) only runs with --live.
        assert!(real_pipeline().unwrap().name().contains("real ssc-pipeline"));
    }
}
