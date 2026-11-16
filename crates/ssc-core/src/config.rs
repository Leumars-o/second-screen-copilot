//! User settings, stored as TOML at `~/.ssc/config.toml`. Missing fields take defaults.
//! Model ids marked "verify" come from the spec and must be checked against the provider consoles.

use crate::protocol::Mode;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub mode: Mode,
    /// Mask card numbers, BVN/NIN-style ids and passwords before any cloud call.
    pub redaction: bool,
    /// Store transcripts in the session history (summaries are always stored).
    pub keep_transcripts: bool,
    /// Passive-mode timer in seconds; None = off (spec default).
    pub passive_timer_s: Option<u32>,
    /// e.g. "wss://relay.example.com". None = LAN only.
    pub relay_url: Option<String>,
    pub lan_port: u16,
    pub hotkeys: Hotkeys,
    pub models: Models,
    pub timeouts: Timeouts,
    pub groq_budget: GroqBudget,
    /// Below this decision confidence, the hotkey answer uses the strong model.
    pub decision_confidence_threshold: f64,
    /// Lines changed in a screen diff that count as a "big screen change" trigger.
    pub big_screen_change_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub capture_toggle: String,
    pub answer_now: String,
    pub tray_toggle: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Models {
    pub groq_model: String,
    pub gemini_model: String,
    /// Optional Sonnet-class model for the strong tier, "provider/model". None = disabled.
    pub strong_model: Option<String>,
    pub ollama_url: String,
    pub local_answer_model: String,
    pub local_postcall_model: String,
    /// TypeSafe /v1/systemone base URL. Empty until confirmed (Build Sheet §9).
    pub decision_cloud_base_url: String,
    pub decision_cloud_model: String,
    pub decision_local_model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Timeouts {
    pub decision_ms: u64,
    /// Consecutive Jev timeouts before switching to Tev1 4B.
    pub decision_failover_after: u32,
    pub first_token_ms: u64,
    /// Silence after speech that ends a question.
    pub question_silence_ms: u64,
    /// No second answer for the same question within this window.
    pub dedupe_window_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroqBudget {
    pub requests_per_min: u32,
    pub requests_per_day: u32,
    pub tokens_per_min: u32,
    pub tokens_per_day: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Cloud,
            redaction: true,
            keep_transcripts: false,
            passive_timer_s: None,
            relay_url: None,
            lan_port: 47823,
            hotkeys: Hotkeys::default(),
            models: Models::default(),
            timeouts: Timeouts::default(),
            groq_budget: GroqBudget::default(),
            decision_confidence_threshold: 0.6,
            big_screen_change_lines: 15,
        }
    }
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            capture_toggle: "CmdOrCtrl+Shift+Space".into(),
            answer_now: "CmdOrCtrl+Shift+A".into(),
            tray_toggle: "CmdOrCtrl+Shift+H".into(),
        }
    }
}

impl Default for Models {
    fn default() -> Self {
        Self {
            groq_model: "openai/gpt-oss-120b".into(),     // verify
            gemini_model: "gemini-2.5-flash-lite".into(), // verify
            strong_model: None,
            ollama_url: "http://127.0.0.1:11434".into(),
            local_answer_model: "gemma4:e2b".into(),   // verify
            local_postcall_model: "gemma4:12b".into(), // verify
            decision_cloud_base_url: String::new(),
            decision_cloud_model: "jev-1.14".into(), // verify
            decision_local_model: "tev1:4b".into(),  // verify
        }
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            decision_ms: 800,
            decision_failover_after: 2,
            first_token_ms: 1000,
            question_silence_ms: 300,
            dedupe_window_ms: 5000,
        }
    }
}

impl Default for GroqBudget {
    fn default() -> Self {
        // Spec: 30 req/min, 1,000/day, 8K tokens/min, 200K tokens/day. Limits change; verify.
        Self { requests_per_min: 30, requests_per_day: 1000, tokens_per_min: 8000, tokens_per_day: 200_000 }
    }
}

/// `~/.ssc` (or `%USERPROFILE%\.ssc`). Overridable with `SSC_HOME`.
pub fn data_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("SSC_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".ssc")
}

impl Config {
    pub fn default_path() -> PathBuf {
        data_dir().join("config.toml")
    }

    /// Load from `path`; a missing file gives defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.mode, Mode::Cloud);
        assert_eq!(c.passive_timer_s, None);
        assert_eq!(c.timeouts.decision_ms, 800);
        assert_eq!(c.timeouts.decision_failover_after, 2);
        assert_eq!(c.timeouts.first_token_ms, 1000);
        assert!(!c.keep_transcripts);
    }

    #[test]
    fn roundtrip_and_partial_file() {
        let dir = std::env::temp_dir().join(format!("ssc-cfg-{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        let c = Config { mode: Mode::Local, relay_url: Some("wss://relay.example.com".into()), ..Default::default() };
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        std::fs::write(&path, "mode = \"local\"\n[timeouts]\ndecision_ms = 500\n").unwrap();
        let p = Config::load(&path).unwrap();
        assert_eq!(p.mode, Mode::Local);
        assert_eq!(p.timeouts.decision_ms, 500);
        assert_eq!(p.timeouts.first_token_ms, 1000);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
