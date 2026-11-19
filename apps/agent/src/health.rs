//! CR-23: `agent.status` for the phone. [`build`] is pure (inputs in, status out) so every rule
//! is unit-tested; [`provider`] gathers the inputs from the running agent.

use crate::permissions::{self, Status};
use ssc_core::config::Config;
use ssc_core::protocol::{AgentStatus, CheckState, Health, HealthCheck, Mode};
use ssc_core::secrets::{SecretKey, SecretStore};
use ssc_pipeline::{PipelineHealth, StatusProvider};
use ssc_transport::LanTlsStatus;
use std::sync::Arc;

/// Everything the status is built from.
pub struct Inputs {
    pub mode: Mode,
    /// (id, status) from the first-run checklist: "screen", "accessibility", "microphone".
    pub permissions: Vec<(&'static str, Status)>,
    pub groq_key: bool,
    pub gemini_key: bool,
    pub lan: LanTlsStatus,
    pub pipeline: PipelineHealth,
    pub now_ms: u64,
}

fn check(id: &str, label: &str, state: CheckState, detail: impl Into<String>) -> HealthCheck {
    let detail: String = detail.into();
    HealthCheck { id: id.into(), label: label.into(), state, detail: detail.chars().take(200).collect() }
}

fn ago(now: u64, t: u64) -> String {
    let s = now.saturating_sub(t) / 1000;
    match s {
        0..=59 => format!("{s} s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        _ => format!("{} h ago", s / 3600),
    }
}

pub fn build(i: &Inputs) -> AgentStatus {
    let mut checks = vec![check("agent", "Agent", CheckState::Ok, "running")];
    checks.push(if i.pipeline.capturing {
        check("capture", "Capture", CheckState::Ok, "listening (mic + call audio)")
    } else {
        check("capture", "Capture", CheckState::Off, "stopped; ⌘⇧Space to start (screen hotkey works anyway)")
    });

    let perm = |id: &str| i.permissions.iter().find(|(p, _)| *p == id).map(|(_, s)| *s);
    let permission = |id: &str, label: &str, needed_for: &str, missing: CheckState| match perm(id) {
        Some(Status::Granted) => check(id, label, CheckState::Ok, "granted"),
        Some(Status::Missing) => check(id, label, missing, format!("missing: needed for {needed_for}")),
        _ => check(id, label, CheckState::Off, "not checked on this OS"),
    };
    checks.push(permission("screen", "Screen Recording", "screen answers and call audio", CheckState::Error));
    checks.push(permission("accessibility", "Accessibility", "reading window text", CheckState::Warn));
    checks.push(permission("microphone", "Microphone", "transcribing you", CheckState::Warn));

    checks.push(match (i.mode, i.groq_key, i.gemini_key) {
        (Mode::Local, _, _) => check("models", "Answer models", CheckState::Ok, "local-only (no cloud keys used)"),
        (_, true, true) => check("models", "Answer models", CheckState::Ok, "Groq + Gemini fallback"),
        (_, true, false) => check(
            "models",
            "Answer models",
            CheckState::Warn,
            "Groq only: no Gemini key, no fallback when rate-limited",
        ),
        (_, false, true) => check("models", "Answer models", CheckState::Warn, "Gemini only (slower): no Groq key"),
        (_, false, false) => {
            check("models", "Answer models", CheckState::Error, "no Groq or Gemini key: cloud answers will fail")
        }
    });

    checks.push(match &i.lan {
        LanTlsStatus::Active { warning: None, domain, .. } => {
            check("wifi_link", "Wi-Fi link", CheckState::Ok, format!("secured ({domain})"))
        }
        LanTlsStatus::Active { warning: Some(w), .. } => check("wifi_link", "Wi-Fi link", CheckState::Warn, w.clone()),
        LanTlsStatus::Starting => check("wifi_link", "Wi-Fi link", CheckState::Warn, "certificate starting up"),
        LanTlsStatus::Failed { message, .. } => {
            check("wifi_link", "Wi-Fi link", CheckState::Warn, format!("{message} (relay still works)"))
        }
        LanTlsStatus::Off => check("wifi_link", "Wi-Fi link", CheckState::Off, "no LAN domain set: relay only"),
    });

    let fact = |id: &str, label: &str, f: &Option<(u64, CheckState, String)>, none: &str| match f {
        Some((t, s, d)) => check(id, label, *s, format!("{d} · {}", ago(i.now_ms, *t))),
        None => check(id, label, CheckState::Off, none),
    };
    checks.push(fact("last_answer", "Last answer", &i.pipeline.last_answer, "none yet"));
    checks.push(fact("last_screen", "Last screen read", &i.pipeline.last_grab, "none yet"));

    let worst = |s: CheckState| checks.iter().find(|c| c.state == s);
    let (health, summary) = if let Some(c) = worst(CheckState::Error) {
        (Health::Error, format!("{}: {}", c.label, c.detail))
    } else if let Some(c) = worst(CheckState::Warn) {
        (Health::Warn, format!("{}: {}", c.label, c.detail))
    } else {
        let capture = if i.pipeline.capturing { "capturing" } else { "capture off" };
        (Health::Ok, format!("All good · {capture} · {}", if i.mode == Mode::Local { "local" } else { "cloud" }))
    };
    AgentStatus {
        health,
        summary: summary.chars().take(200).collect(),
        checks,
        capturing: i.pipeline.capturing,
        t: i.now_ms,
    }
}

/// Current LAN TLS status (read through a weak handle to the running system).
pub type LanStatusFn = Arc<dyn Fn() -> LanTlsStatus + Send + Sync>;

/// The provider handed to [`ssc_pipeline::Pipeline::set_status_provider`].
pub fn provider(cfg: Config, secrets: Arc<dyn SecretStore>, lan: LanStatusFn) -> StatusProvider {
    Arc::new(move |p: &PipelineHealth| {
        let key = |k: SecretKey| secrets.get(&k).ok().flatten().is_some_and(|s| !s.is_empty());
        build(&Inputs {
            mode: cfg.mode,
            permissions: permissions::checklist().into_iter().map(|c| (c.id, c.status)).collect(),
            groq_key: key(SecretKey::Groq),
            gemini_key: key(SecretKey::Gemini),
            lan: lan(),
            pipeline: p.clone(),
            now_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> Inputs {
        Inputs {
            mode: Mode::Cloud,
            permissions: vec![
                ("screen", Status::Granted),
                ("accessibility", Status::Granted),
                ("microphone", Status::Granted),
            ],
            groq_key: true,
            gemini_key: true,
            lan: LanTlsStatus::Active { domain: "relay.example.com".into(), ip: None, not_after: 0, warning: None },
            pipeline: PipelineHealth::default(),
            now_ms: 100_000,
        }
    }

    #[test]
    fn all_good_when_nothing_is_wrong() {
        let s = build(&inputs());
        assert_eq!(s.health, Health::Ok);
        assert_eq!(s.summary, "All good · capture off · cloud");
        assert!(s.checks.len() <= 16);
        assert!(
            s.checks.iter().all(|c| c.id.len() <= 32 && c.id.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'))
        );
        // The protocol schema accepts it.
        let env = serde_json::json!({
            "v": 1, "type": "agent.status", "seq": 0, "ts": 0, "session_id": null,
            "payload": serde_json::to_value(&s).unwrap(),
        });
        ssc_core::protocol::validate(&env).unwrap();
    }

    #[test]
    fn errors_beat_warnings_and_lead_the_summary() {
        let mut i = inputs();
        i.gemini_key = false; // warn
        i.pipeline.last_grab = Some((40_000, CheckState::Error, "no focused application".into()));
        let s = build(&i);
        assert_eq!(s.health, Health::Error);
        assert_eq!(s.summary, "Last screen read: no focused application · 1 min ago");

        let mut i = inputs();
        i.gemini_key = false;
        let s = build(&i);
        assert_eq!(s.health, Health::Warn);
        assert!(s.summary.starts_with("Answer models: Groq only"), "{}", s.summary);
    }

    #[test]
    fn missing_screen_recording_is_an_error_missing_mic_a_warning() {
        let mut i = inputs();
        i.permissions = vec![("screen", Status::Missing), ("microphone", Status::Missing)];
        let s = build(&i);
        let st = |id: &str| s.checks.iter().find(|c| c.id == id).unwrap().state;
        assert_eq!(st("screen"), CheckState::Error);
        assert_eq!(st("microphone"), CheckState::Warn);
        assert_eq!(s.health, Health::Error);
    }

    #[test]
    fn capture_off_and_relay_only_are_not_problems() {
        let mut i = inputs();
        i.lan = LanTlsStatus::Off;
        i.pipeline.last_answer = Some((99_000, CheckState::Ok, "groq/qwen/qwen3.8-27b".into()));
        let s = build(&i);
        assert_eq!(s.health, Health::Ok);
        let la = s.checks.iter().find(|c| c.id == "last_answer").unwrap();
        assert_eq!(la.detail, "groq/qwen/qwen3.8-27b · 1 s ago");
        assert_eq!(ago(400_000, 400_000 - 3 * 60_000), "3 min ago");
        assert_eq!(ago(1_000, 5_000), "0 s ago", "clock skew never underflows");
    }
}
