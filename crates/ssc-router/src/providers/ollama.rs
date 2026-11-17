//! Ollama `/api/chat` with NDJSON streaming (Gemma 4 E2B live, Gemma 4 12B post-call).
//!
//! Prefix caching: the fixed prompt prefix is always the first (system) message and byte-stable,
//! so Ollama reuses its KV cache for it; `keep_alive` keeps the model loaded between answers, and
//! [`Ollama::warm_prefix`] pre-fills the cache before the first question.
//!
//! Why this is NOT on the shared OpenAI-compatible adapter (Build Sheet §10 item 8 exception,
//! checked against Ollama's "OpenAI compatibility" docs, 2026-11-17): `/v1/chat/completions`
//! accepts no `keep_alive` and no `options` (the docs: "The OpenAI API does not have a way of
//! setting the context size"; `num_ctx` only via a Modelfile). Every request resets the model's
//! unload timer to the request's `keep_alive`, so `/v1` calls would fall back to the 5-minute
//! default and the live model would be unloaded between calls — a multi-second cold load on the
//! next question. Thinking control there is only `reasoning_effort` mapped onto `think`. The
//! KV-prefix reuse itself is server-side and would survive, but losing `keep_alive` and
//! per-request `options` is material for first-token latency, so the native path stays.

use super::{is_loopback_url, model_id};
use crate::http::{Step, StreamParser, drive_with_idle};
use crate::wire::NdjsonDecoder;
use async_trait::async_trait;
use serde_json::{Value, json};
use ssc_core::traits::{AnswerModel, Cancel};
use ssc_core::types::{Prompt, StreamStats, Token};
use ssc_core::{Result, SscError};
use tokio::sync::mpsc;

pub const DEFAULT_KEEP_ALIVE: &str = "30m";

pub struct Ollama {
    id: String,
    base_url: String,
    model: String,
    keep_alive: String,
    client: reqwest::Client,
    local: bool,
    /// `think` field: `Some(false)` (default) turns thinking off on thinking-capable models
    /// (qwen3, gemma) so the first streamed token is answer text; models that can't turn it off
    /// use their lowest level and stream it in `message.thinking`, which is never shown.
    /// `None` leaves the model default.
    think: Option<bool>,
    idle: std::time::Duration,
}

/// Names of the models Ollama has in memory right now (`GET /api/ps`).
pub async fn loaded_models(client: &reqwest::Client, base_url: &str) -> Result<Vec<String>> {
    let r = client
        .get(format!("{}/api/ps", base_url.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .map_err(|e| crate::http::map_reqwest(e, 0))?;
    if !r.status().is_success() {
        return Err(SscError::Unavailable(format!("http {}", r.status())));
    }
    let v: Value = r.json().await.map_err(|_| SscError::Unavailable("bad /api/ps body".into()))?;
    Ok(parse_ps(&v))
}

fn parse_ps(v: &Value) -> Vec<String> {
    v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["name"].as_str().or_else(|| m["model"].as_str()).map(String::from))
        .collect()
}

/// `have` contains `want` (an untagged name matches `:latest`).
pub fn has_model(have: &[String], want: &str) -> bool {
    have.iter().any(|h| h == want || h.strip_suffix(":latest") == Some(want) || want.strip_suffix(":latest") == Some(h))
}

impl Ollama {
    pub fn new(base_url: &str, model: &str) -> Self {
        Self {
            id: model_id("ollama", model),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            keep_alive: DEFAULT_KEEP_ALIVE.into(),
            client: crate::http::client(),
            local: is_loopback_url(base_url),
            think: Some(false),
            idle: crate::http::DEFAULT_IDLE_TIMEOUT,
        }
    }

    pub fn with_idle_timeout(mut self, idle: std::time::Duration) -> Self {
        self.idle = idle;
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The Ollama model name (without the `ollama/` prefix).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Models Ollama currently holds in memory.
    pub async fn loaded(&self) -> Result<Vec<String>> {
        loaded_models(&self.client, &self.base_url).await
    }

    pub fn with_keep_alive(mut self, keep_alive: &str) -> Self {
        self.keep_alive = keep_alive.into();
        self
    }

    /// Set the `think` field (`None` = model default). Used for the post-call model, which isn't
    /// latency-bound.
    pub fn with_think(mut self, think: Option<bool>) -> Self {
        self.think = think;
        self
    }

    pub fn body(&self, p: &Prompt) -> Value {
        let mut options = json!({ "temperature": p.temperature, "num_predict": p.max_tokens });
        if !p.stop.is_empty() {
            options["stop"] = json!(p.stop);
        }
        let mut b = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": p.prefix },
                { "role": "user", "content": p.dynamic },
            ],
            "stream": true,
            "keep_alive": self.keep_alive,
            "options": options,
        });
        if let Some(t) = self.think {
            b["think"] = json!(t);
        }
        b
    }

    /// Load the model (empty `messages` loads without generating).
    pub async fn preload(&self) -> Result<()> {
        let body = json!({ "model": self.model, "messages": [], "keep_alive": self.keep_alive });
        let r = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| crate::http::map_reqwest(e, 0))?;
        if r.status().is_success() { Ok(()) } else { Err(SscError::Unavailable(format!("http {}", r.status()))) }
    }

    /// Evaluate `prefix` once (one output token) so the next answer reuses its KV cache.
    pub async fn warm_prefix(&self, prefix: &str) -> Result<()> {
        let p = Prompt { prefix: prefix.into(), dynamic: String::new(), max_tokens: 1, temperature: 0.0, stop: vec![] };
        let (tx, mut rx) = mpsc::channel(8);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let r = self.stream(&p, tx, Cancel::new()).await.map(|_| ());
        let _ = drain.await;
        r
    }
}

#[async_trait]
impl AnswerModel for Ollama {
    fn id(&self) -> &str {
        &self.id
    }
    /// A non-loopback Ollama host sends data off the PC, so it counts as cloud.
    fn is_cloud(&self) -> bool {
        !self.local
    }
    async fn stream(&self, prompt: &Prompt, tx: mpsc::Sender<Token>, cancel: Cancel) -> Result<StreamStats> {
        let req = self.client.post(format!("{}/api/chat", self.base_url)).json(&self.body(prompt));
        drive_with_idle(&self.id, req, OllamaNdjson::default(), &tx, &cancel, self.idle).await
    }
}

#[derive(Default)]
pub struct OllamaNdjson {
    nd: NdjsonDecoder,
}

impl OllamaNdjson {
    fn lines(lines: Vec<String>) -> Result<Step> {
        let mut step = Step::default();
        for l in lines {
            let v: Value = serde_json::from_str(&l).map_err(|_| SscError::Unavailable("bad stream frame".into()))?;
            if let Some(e) = v.get("error") {
                let msg = e.as_str().unwrap_or("error");
                // e.g. "model 'x' not found": the model isn't pulled.
                let short: String = msg.chars().take(120).collect();
                return Err(SscError::Unavailable(format!("ollama: {short}")));
            }
            if let Some(t) = v.pointer("/message/content").and_then(Value::as_str) {
                step.texts.push(t.to_string());
            }
            if v.get("done").and_then(Value::as_bool) == Some(true) {
                step.done = true;
                if let Some(n) = v.get("eval_count").and_then(Value::as_u64) {
                    step.tokens_out = Some(n as u32);
                }
            }
        }
        Ok(step)
    }
}

impl StreamParser for OllamaNdjson {
    fn feed(&mut self, bytes: &[u8]) -> Result<Step> {
        Self::lines(self.nd.push(bytes))
    }
    fn finish(&mut self) -> Result<Step> {
        Self::lines(self.nd.finish().into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_keeps_prefix_first_and_keep_alive() {
        let o = Ollama::new("http://127.0.0.1:11434/", "gemma4:e2b");
        let p = Prompt { prefix: "FIXED".into(), dynamic: "Q".into(), max_tokens: 60, temperature: 0.2, stop: vec![] };
        let b = o.body(&p);
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][0]["content"], "FIXED");
        assert_eq!(b["keep_alive"], DEFAULT_KEEP_ALIVE);
        assert_eq!(b["options"]["num_predict"], 60);
        assert_eq!(b["think"], false, "live path turns thinking off");
        assert!(o.with_think(None).body(&p).get("think").is_none());
        let o = Ollama::new("http://127.0.0.1:11434/", "gemma4:e2b");
        assert!(!o.is_cloud());
        assert_eq!(o.id(), "ollama/gemma4:e2b");
        assert!(Ollama::new("http://10.0.0.5:11434", "m").is_cloud());
    }

    #[test]
    fn parses_fixture() {
        let mut p = OllamaNdjson::default();
        let s = p.feed(include_bytes!("../../tests/fixtures/ollama_chat.ndjson")).unwrap();
        assert!(s.done);
        assert_eq!(s.texts.concat(), "1. Requirements\n2. Design\n3. Build\n---\n1: Ask about users\n");
        assert_eq!(s.tokens_out, Some(15));
    }

    #[test]
    fn ps_names() {
        let v = json!({"models": [{"name": "gemma4:e2b", "model": "gemma4:e2b"}, {"model": "tev1:4b"}]});
        let have = parse_ps(&v);
        assert_eq!(have, vec!["gemma4:e2b", "tev1:4b"]);
        assert!(has_model(&have, "tev1:4b") && !has_model(&have, "qwen3:8b"));
        assert!(has_model(&["m:latest".into()], "m") && has_model(&["m".into()], "m:latest"));
        assert!(parse_ps(&json!({})).is_empty());
    }

    #[test]
    fn error_line() {
        let mut p = OllamaNdjson::default();
        let e = p.feed(b"{\"error\":\"model 'gemma4:e2b' not found\"}\n").unwrap_err();
        assert!(matches!(e, SscError::Unavailable(m) if m.contains("not found")));
    }
}
