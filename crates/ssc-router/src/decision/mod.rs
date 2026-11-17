//! Decision models (Jev in the cloud, Tev1 4B on Ollama) and the deterministic decision router.
//!
//! Router rules (Build Sheet §6 W1-MODELS; spec "Decision model"):
//! - Local-only mode: no cloud decision model exists at all; Tev1 4B decides.
//! - Cloud mode: Jev decides with an 800 ms timeout (`timeouts.decision_ms`). A timeout or network
//!   failure is answered by Tev1 for that call; `decision_failover_after` (2) in a row switch every
//!   call to Tev1 until a network check passes again.
//! - When decisions come from Tev1 (offline or local-only), they only tag: live answers fire from
//!   the hotkey and the end-of-question rule ([`DecisionRouter::decisions_gate_answers`]).
//! - Redaction runs on the input before any cloud call. `Policy` errors are returned, never retried
//!   on another model.

pub mod systemone;

use crate::http::{map_reqwest, map_status};
use async_trait::async_trait;
use ssc_core::decision::{Decision, DecisionInput};
use ssc_core::protocol::Mode;
use ssc_core::secrets::Secret;
use ssc_core::traits::DecisionModel;
use ssc_core::{Result, SscError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// One adapter for both Jev and Tev1 4B: `POST {base_url}/v1/systemone`.
pub struct SystemOneDecision {
    id: String,
    base_url: String,
    model: String,
    key: Option<Secret>,
    client: reqwest::Client,
}

impl SystemOneDecision {
    pub fn new(base_url: &str, model: &str, key: Option<Secret>) -> Self {
        let base = base_url.trim_end_matches('/').trim_end_matches("/v1").to_string();
        Self {
            id: format!("systemone/{model}"),
            base_url: base,
            model: model.into(),
            key,
            client: crate::http::client(),
        }
    }

    pub fn url(&self) -> String {
        format!("{}/v1/systemone", self.base_url)
    }

    pub fn is_cloud(&self) -> bool {
        !crate::providers::is_loopback_url(&self.base_url)
    }
}

#[async_trait]
impl DecisionModel for SystemOneDecision {
    fn id(&self) -> &str {
        &self.id
    }

    async fn decide(&self, input: &DecisionInput) -> Result<Decision> {
        let start = Instant::now();
        let (body, plan) = systemone::build_request(&self.model, input);
        let mut req = self.client.post(self.url()).json(&body);
        if let Some(k) = &self.key {
            req = req.bearer_auth(k.expose());
        }
        let ms = || start.elapsed().as_millis() as u64;
        let resp = req.send().await.map_err(|e| map_reqwest(e, ms()))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await.map_err(|e| map_reqwest(e, ms()))?;
        if !status.is_success() {
            return Err(with_detail(map_status(status, &headers, &bytes, ms()), &bytes));
        }
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| SscError::Invalid("systemone response: not JSON".into()))?;
        let normalised = systemone::map_response(&v, &plan)?;
        Decision::from_value(&normalised).map_err(|e| SscError::Invalid(e.to_string()))
    }
}

/// Longest provider error message kept in an error.
const DETAIL_MAX_CHARS: usize = 160;

/// Append the provider's `detail.message` (e.g. "Unknown model: jev-1.14") so a failing call is
/// diagnosable. Safe here, unlike in answer adapters: systemone errors describe the request shape,
/// the body never contains our key, and the input was already redacted before a cloud call.
fn with_detail(e: SscError, body: &[u8]) -> SscError {
    let Some(msg) = crate::http::error_message(body, DETAIL_MAX_CHARS) else { return e };
    match e {
        SscError::Invalid(s) => SscError::Invalid(format!("systemone {s}: {msg}")),
        SscError::Unavailable(s) => SscError::Unavailable(format!("systemone {s}: {msg}")),
        other => other,
    }
}

/// "Is the cloud decision endpoint reachable again?"
#[async_trait]
pub trait NetworkProbe: Send + Sync {
    async fn reachable(&self) -> bool;
}

/// Any HTTP response from `url` within the timeout counts as reachable.
pub struct HttpProbe {
    client: reqwest::Client,
    url: String,
    timeout: Duration,
}

impl HttpProbe {
    pub fn new(url: &str) -> Self {
        Self { client: crate::http::client(), url: url.into(), timeout: Duration::from_millis(1500) }
    }
}

#[async_trait]
impl NetworkProbe for HttpProbe {
    async fn reachable(&self) -> bool {
        self.client.get(&self.url).timeout(self.timeout).send().await.is_ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    /// Jev answered.
    Cloud,
    /// Tev1 answered because Jev timed out / failed, or the router is degraded.
    LocalFallback,
    /// Local-only mode (or no cloud model configured).
    LocalOnly,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decided {
    pub decision: Decision,
    pub source: DecisionSource,
    pub model: String,
}

/// A decision model plus whether it sends data off the PC.
#[derive(Clone)]
pub struct DecisionSlot {
    pub model: Arc<dyn DecisionModel>,
    pub is_cloud: bool,
}

#[derive(Debug, Clone)]
pub struct DecisionRouterConfig {
    pub timeout: Duration,
    pub failover_after: u32,
    pub probe_interval: Duration,
    pub redaction: Option<ssc_redact::Options>,
}

impl Default for DecisionRouterConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(800),
            failover_after: 2,
            probe_interval: Duration::from_secs(10),
            redaction: Some(ssc_redact::Options::default()),
        }
    }
}

#[derive(Default)]
struct State {
    consecutive_failures: u32,
    degraded: bool,
    probe_in_flight: bool,
    last_probe: Option<Instant>,
}

pub struct DecisionRouter {
    mode: Mode,
    cloud: Option<DecisionSlot>,
    local: DecisionSlot,
    cfg: DecisionRouterConfig,
    probe: Option<Arc<dyn NetworkProbe>>,
    state: Arc<Mutex<State>>,
}

impl DecisionRouter {
    /// Fails with `Policy` if `mode` is local and any slot is cloud.
    pub fn new(
        mode: Mode,
        cloud: Option<DecisionSlot>,
        local: DecisionSlot,
        cfg: DecisionRouterConfig,
        probe: Option<Arc<dyn NetworkProbe>>,
    ) -> Result<Self> {
        if mode == Mode::Local && (cloud.is_some() || local.is_cloud) {
            return Err(SscError::Policy("local-only mode cannot use a cloud decision model".into()));
        }
        Ok(Self { mode, cloud, local, cfg, probe, state: Arc::default() })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn has_cloud(&self) -> bool {
        self.cloud.is_some()
    }

    /// Where the next decision will come from.
    pub fn source(&self) -> DecisionSource {
        match &self.cloud {
            None => DecisionSource::LocalOnly,
            Some(_) if self.state.lock().unwrap().degraded => DecisionSource::LocalFallback,
            Some(_) => DecisionSource::Cloud,
        }
    }

    /// True only while Jev decides. Otherwise (offline / local-only) decisions are tags only and
    /// live answers fire from the hotkey and the end-of-question rule.
    pub fn decisions_gate_answers(&self) -> bool {
        self.source() == DecisionSource::Cloud
    }

    /// External signal that the network is back (e.g. a provider call succeeded).
    pub fn network_restored(&self) {
        let mut s = self.state.lock().unwrap();
        s.degraded = false;
        s.consecutive_failures = 0;
    }

    fn prepare(&self, input: &DecisionInput, cloud: bool) -> DecisionInput {
        match (cloud, self.cfg.redaction) {
            (true, Some(o)) => DecisionInput {
                question: ssc_redact::redact(&input.question, o).text,
                screen_diff: ssc_redact::redact(&input.screen_diff, o).text,
                profiles: input.profiles.clone(),
            },
            _ => input.clone(),
        }
    }

    pub async fn decide(&self, input: &DecisionInput) -> Result<Decided> {
        if let Some(cloud) = &self.cloud {
            let degraded = self.state.lock().unwrap().degraded;
            if degraded {
                self.maybe_probe();
            } else {
                let inp = self.prepare(input, cloud.is_cloud);
                let res = tokio::time::timeout(self.cfg.timeout, cloud.model.decide(&inp)).await;
                let network_failure = match res {
                    Ok(Ok(decision)) => {
                        self.state.lock().unwrap().consecutive_failures = 0;
                        return Ok(Decided { decision, source: DecisionSource::Cloud, model: cloud.model.id().into() });
                    }
                    Ok(Err(e @ (SscError::Policy(_) | SscError::Cancelled))) => return Err(e),
                    Err(_) | Ok(Err(SscError::Timeout(_) | SscError::Unavailable(_))) => true,
                    // Rate limit or a malformed answer: use Tev1 for this call, but the network is fine.
                    Ok(Err(_)) => false,
                };
                if network_failure {
                    let mut s = self.state.lock().unwrap();
                    s.consecutive_failures += 1;
                    if s.consecutive_failures >= self.cfg.failover_after.max(1) {
                        s.degraded = true;
                        s.last_probe = Some(Instant::now());
                    }
                }
            }
        }
        let source = if self.cloud.is_some() { DecisionSource::LocalFallback } else { DecisionSource::LocalOnly };
        let inp = self.prepare(input, self.local.is_cloud);
        let decision = self.local.model.decide(&inp).await?;
        Ok(Decided { decision, source, model: self.local.model.id().into() })
    }

    /// While degraded, check the network at most once per `probe_interval`, in the background.
    fn maybe_probe(&self) {
        let Some(probe) = self.probe.clone() else { return };
        {
            let mut s = self.state.lock().unwrap();
            let due = s.last_probe.is_none_or(|t| t.elapsed() >= self.cfg.probe_interval);
            if s.probe_in_flight || !due {
                return;
            }
            s.probe_in_flight = true;
        }
        let state = self.state.clone();
        tokio::spawn(async move {
            let ok = probe.reachable().await;
            let mut s = state.lock().unwrap();
            s.probe_in_flight = false;
            s.last_probe = Some(Instant::now());
            if ok {
                s.degraded = false;
                s.consecutive_failures = 0;
            }
        });
    }
}

#[cfg(test)]
mod tests;
