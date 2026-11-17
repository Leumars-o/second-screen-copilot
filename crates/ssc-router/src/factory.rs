//! Builds the answer and decision routers from [`Config`] + keys. The mode decides which clients
//! exist: in local-only mode no cloud adapter is constructed and the secret store is never read.

use crate::answer::{AnswerRouter, Route};
use crate::budget::BudgetTracker;
use crate::decision::{DecisionRouter, DecisionRouterConfig, DecisionSlot, HttpProbe, NetworkProbe, SystemOneDecision};
use crate::providers::anthropic::ANTHROPIC_BASE_URL;
use crate::providers::openai::{GEMINI_BASE_URL, GROQ_BASE_URL, OPENAI_BASE_URL};
use crate::providers::{Anthropic, Ollama, OpenAiCompat, is_loopback_url};
use crate::warm::Warmer;
use ssc_core::config::Config;
use ssc_core::protocol::Mode;
use ssc_core::secrets::{Secret, SecretKey, SecretStore};
use ssc_core::traits::{AnswerModel, Clock};
use ssc_core::{Result, SscError};
use std::sync::Arc;
use std::time::Duration;

/// Internal knobs that aren't user settings (user settings live in `Config`, CR-5): timings and
/// provider base URLs that tests point at mock servers.
#[derive(Debug, Clone)]
pub struct RouterOptions {
    /// Strong model: fail over to the fast chain when no token arrives within this.
    pub strong_first_token_ms: u64,
    pub decision_probe_interval: Duration,
    pub groq_base_url: String,
    pub gemini_base_url: String,
    pub anthropic_base_url: String,
    pub openai_base_url: String,
}

impl Default for RouterOptions {
    fn default() -> Self {
        Self {
            strong_first_token_ms: 4000,
            decision_probe_interval: Duration::from_secs(10),
            groq_base_url: GROQ_BASE_URL.into(),
            gemini_base_url: GEMINI_BASE_URL.into(),
            anthropic_base_url: ANTHROPIC_BASE_URL.into(),
            openai_base_url: OPENAI_BASE_URL.into(),
        }
    }
}

pub struct Models {
    pub answers: AnswerRouter,
    pub decisions: DecisionRouter,
    /// Summary / follow-up model (not time-critical): the strong model if configured in cloud
    /// mode, else the local post-call model.
    pub postcall: Arc<dyn AnswerModel>,
    /// The Groq budget, for the debug panel.
    pub groq_budget: Option<Arc<BudgetTracker>>,
    /// Session-start / keep-alive warm-up of the answer providers (M2).
    pub warm: Warmer,
    /// The local decision model (Tev1) and the Ollama it runs on, for the background tagger's
    /// "no model swap" check (M5).
    pub tev1: LocalModels,
}

/// Names the pipeline needs to check what Ollama holds in memory.
#[derive(Debug, Clone, Default)]
pub struct LocalModels {
    pub ollama_url: String,
    pub answer_model: String,
    pub decision_model: String,
}

/// Gemini (paid tier) fails over to the local model when no token arrives in this long (M3).
pub const GEMINI_FIRST_TOKEN_MS: u64 = 1500;

fn key(secrets: &dyn SecretStore, k: SecretKey) -> Result<Option<Secret>> {
    Ok(secrets.get(&k)?.filter(|s| !s.expose().trim().is_empty()))
}

/// Build every model client for `cfg.mode`.
pub fn build(cfg: &Config, secrets: &dyn SecretStore, clock: Arc<dyn Clock>, opts: &RouterOptions) -> Result<Models> {
    let redaction = cfg.redaction.then_some(ssc_redact::Options { emails: cfg.redact_emails });
    let m = &cfg.models;
    let local_answer = Arc::new(Ollama::new(&m.ollama_url, &m.local_answer_model));
    let postcall_local = Arc::new(Ollama::new(&m.ollama_url, &m.local_postcall_model).with_think(None));
    let tev1 = DecisionSlot {
        model: Arc::new(SystemOneDecision::new(&m.ollama_url, &m.decision_local_model, None)),
        is_cloud: !is_loopback_url(&m.ollama_url),
    };
    let tev1_names = LocalModels {
        ollama_url: m.ollama_url.clone(),
        answer_model: m.local_answer_model.clone(),
        decision_model: m.decision_local_model.clone(),
    };
    let dcfg = DecisionRouterConfig {
        timeout: Duration::from_millis(cfg.timeouts.decision_ms),
        failover_after: cfg.timeouts.decision_failover_after,
        probe_interval: opts.decision_probe_interval,
        redaction,
    };

    if cfg.mode == Mode::Local {
        // Local-only: Ollama on this machine, nothing else. No secret is read, no cloud client built.
        if !is_loopback_url(&m.ollama_url) {
            return Err(SscError::Policy(format!(
                "local-only mode needs Ollama on this machine, not {}",
                m.ollama_url
            )));
        }
        let warm = Warmer { cloud: vec![], local: Some(local_answer.clone()) };
        let answers = AnswerRouter::new(Mode::Local, redaction, vec![Route::new(local_answer)], None)?;
        let decisions = DecisionRouter::new(Mode::Local, None, tev1, dcfg, None)?;
        return Ok(Models { answers, decisions, postcall: postcall_local, groq_budget: None, warm, tev1: tev1_names });
    }

    // ---- cloud mode ----
    let mut fast = Vec::new();
    let mut groq_budget = None;
    let mut warm = Warmer::default();
    if let Some(k) = key(secrets, SecretKey::Groq)? {
        let b = Arc::new(BudgetTracker::new(cfg.groq_budget.clone(), clock));
        let groq = Arc::new(OpenAiCompat::groq(&opts.groq_base_url, &m.groq_model, k));
        warm.cloud.push(groq.clone());
        fast.push(Route::new(groq).first_token_ms(cfg.timeouts.first_token_ms).budget(b.clone()));
        groq_budget = Some(b);
    }
    if let Some(k) = key(secrets, SecretKey::Gemini)? {
        let g = OpenAiCompat::gemini(&opts.gemini_base_url, &m.gemini_model, k, m.gemini_tier, cfg.testing)?;
        let g = Arc::new(g);
        warm.cloud.push(g.clone());
        fast.push(Route::new(g).first_token_ms(GEMINI_FIRST_TOKEN_MS));
    }
    // Offline in cloud mode: the local model still answers.
    fast.push(Route::new(local_answer));

    let strong = match &m.strong_model {
        Some(spec) if !spec.trim().is_empty() => build_strong(spec, m.strong_base_url.as_deref(), secrets, opts)?,
        _ => None,
    };
    let postcall: Arc<dyn AnswerModel> = strong.clone().unwrap_or(postcall_local);
    let strong_route = strong.map(|s| Route::new(s).first_token_ms(opts.strong_first_token_ms));
    let answers = AnswerRouter::new(Mode::Cloud, redaction, fast, strong_route)?;

    let base = m.decision_cloud_base_url.trim();
    let (jev, probe) = match key(secrets, SecretKey::TypeSafe)? {
        Some(k) if !base.is_empty() => {
            let jev = SystemOneDecision::new(base, &m.decision_cloud_model, Some(k));
            let probe: Arc<dyn NetworkProbe> = Arc::new(HttpProbe::new(base));
            (Some(DecisionSlot { is_cloud: jev.is_cloud(), model: Arc::new(jev) }), Some(probe))
        }
        _ => (None, None),
    };
    let decisions = DecisionRouter::new(Mode::Cloud, jev, tev1, dcfg, probe)?;
    Ok(Models { answers, decisions, postcall, groq_budget, warm, tev1: tev1_names })
}

/// `"provider/model"`: `anthropic/…` (Messages API), `openai/…`, `groq/…`, or any other provider
/// name with `strong_base_url` set (OpenAI-compatible). Missing key → no strong tier.
fn build_strong(
    spec: &str,
    strong_base_url: Option<&str>,
    secrets: &dyn SecretStore,
    opts: &RouterOptions,
) -> Result<Option<Arc<dyn AnswerModel>>> {
    let (provider, model) = spec
        .split_once('/')
        .ok_or_else(|| SscError::Invalid(format!("strong_model must be provider/model: {spec}")))?;
    let strong_key = key(secrets, SecretKey::Strong)?;
    let m: Arc<dyn AnswerModel> = match provider {
        "anthropic" => match strong_key {
            Some(k) => Arc::new(Anthropic::new(&opts.anthropic_base_url, model, k)),
            None => return Ok(None),
        },
        "openai" => match strong_key {
            Some(k) => Arc::new(OpenAiCompat::new("openai", &opts.openai_base_url, model, k)),
            None => return Ok(None),
        },
        "groq" => match strong_key.or(key(secrets, SecretKey::Groq)?) {
            Some(k) => Arc::new(OpenAiCompat::groq(&opts.groq_base_url, model, k)),
            None => return Ok(None),
        },
        other => {
            let base = strong_base_url
                .ok_or_else(|| SscError::Invalid(format!("strong provider '{other}' needs strong_base_url")))?;
            match strong_key {
                Some(k) => Arc::new(OpenAiCompat::new(other, base, model, k)),
                None => return Ok(None),
            }
        }
    };
    Ok(Some(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::mock::MockClock;
    use ssc_core::secrets::MemorySecretStore;

    /// A secret store that fails the test if anything reads it.
    struct NoSecrets;
    impl SecretStore for NoSecrets {
        fn get(&self, key: &SecretKey) -> Result<Option<Secret>> {
            panic!("local-only mode read secret {key:?}");
        }
        fn set(&self, _: &SecretKey, _: &Secret) -> Result<()> {
            panic!("set")
        }
        fn delete(&self, _: &SecretKey) -> Result<()> {
            panic!("delete")
        }
    }

    fn all_keys() -> MemorySecretStore {
        let s = MemorySecretStore::default();
        for k in [SecretKey::Groq, SecretKey::Gemini, SecretKey::TypeSafe, SecretKey::Strong] {
            s.set(&k, &Secret::new("test-key")).unwrap();
        }
        s
    }

    fn cloud_cfg() -> Config {
        let mut c = Config::default();
        c.models.strong_model = Some("anthropic/claude-opus-5-5".into());
        c.models.decision_cloud_base_url = "https://decisions.example.com".into();
        c
    }

    #[test]
    fn local_only_builds_no_cloud_client_and_reads_no_secret() {
        // Every cloud option configured, all keys "available": local mode must ignore them all.
        let cfg = Config { mode: Mode::Local, ..cloud_cfg() };
        let m = build(&cfg, &NoSecrets, Arc::new(MockClock::new(0)), &RouterOptions::default()).unwrap();
        let models = m.answers.models();
        assert_eq!(models.len(), 1);
        assert!(models.iter().all(|x| !x.is_cloud()), "{:?}", models.iter().map(|x| x.id()).collect::<Vec<_>>());
        assert!(models[0].id().starts_with("ollama/"));
        assert!(!m.answers.has_strong());
        assert!(!m.decisions.has_cloud());
        assert!(!m.postcall.is_cloud());
        assert!(m.groq_budget.is_none());
        assert_eq!(m.warm.targets(), 1, "the local answer model is warmed");
    }

    #[test]
    fn local_only_refuses_remote_ollama() {
        let mut cfg = Config { mode: Mode::Local, ..Config::default() };
        cfg.models.ollama_url = "http://192.168.1.50:11434".into();
        let r = build(&cfg, &NoSecrets, Arc::new(MockClock::new(0)), &RouterOptions::default());
        assert!(matches!(r, Err(SscError::Policy(_))));
    }

    #[test]
    fn cloud_chain_order() {
        let m = build(&cloud_cfg(), &all_keys(), Arc::new(MockClock::new(0)), &RouterOptions::default()).unwrap();
        let ids: Vec<String> = m.answers.models().iter().map(|x| x.id().to_string()).collect();
        assert_eq!(
            ids,
            vec![
                "groq/openai/gpt-oss-120b",
                "gemini/gemini-2.5-flash-lite",
                "ollama/gemma4:e2b",
                "anthropic/claude-opus-5-5"
            ]
        );
        assert!(m.decisions.has_cloud());
        assert!(m.postcall.is_cloud());
        assert!(m.groq_budget.is_some());
        assert_eq!(m.warm.targets(), 2, "groq + gemini connections warmed; local fallback not preloaded");
    }

    #[test]
    fn cloud_without_keys_falls_back_to_local() {
        let m =
            build(&cloud_cfg(), &MemorySecretStore::default(), Arc::new(MockClock::new(0)), &RouterOptions::default())
                .unwrap();
        let ids: Vec<String> = m.answers.models().iter().map(|x| x.id().to_string()).collect();
        assert_eq!(ids, vec!["ollama/gemma4:e2b"]);
        assert!(!m.decisions.has_cloud());
    }

    #[test]
    fn gemini_free_tier_refused_outside_testing() {
        let o = RouterOptions::default();
        let mut cfg = cloud_cfg();
        cfg.models.gemini_tier = ssc_core::config::GeminiTier::Free;
        let r = build(&cfg, &all_keys(), Arc::new(MockClock::new(0)), &o);
        assert!(matches!(r, Err(SscError::Policy(_))));
        cfg.testing = true;
        assert!(build(&cfg, &all_keys(), Arc::new(MockClock::new(0)), &o).is_ok());
    }

    #[test]
    fn strong_provider_variants() {
        let s = all_keys();
        let o = RouterOptions::default();
        assert_eq!(build_strong("openai/gpt-x", None, &s, &o).unwrap().unwrap().id(), "openai/gpt-x");
        assert_eq!(build_strong("groq/qwen/qwen3", None, &s, &o).unwrap().unwrap().id(), "groq/qwen/qwen3");
        assert!(build_strong("openrouter/x", None, &s, &o).is_err());
        let base = Some("https://openrouter.ai/api/v1");
        assert_eq!(build_strong("openrouter/a/b", base, &s, &o).unwrap().unwrap().id(), "openrouter/a/b");
        assert!(build_strong("nomodel", None, &s, &o).is_err());
        assert!(build_strong("anthropic/x", None, &MemorySecretStore::default(), &o).unwrap().is_none());
    }
}
