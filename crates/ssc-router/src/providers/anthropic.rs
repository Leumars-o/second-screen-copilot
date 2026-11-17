//! Native Claude Messages API with SSE streaming, for the strong tier (never an OpenAI shim:
//! Build Sheet §10 item 8). Request shape checked against platform.claude.com, 2026-11-17:
//!
//! - `anthropic-version: 2026-10-21`. That version removes `temperature` / `top_p` / `top_k`
//!   (400 on every model), and Claude Opus 5.5 rejects them anyway, so none is sent.
//! - The fixed prefix goes in `system` with `cache_control` so prompt caching applies; thinking
//!   config and effort are constant per adapter, which keeps the cache valid between requests.
//! - `thinking: {type: "adaptive", display: "omitted"}`: Claude decides whether to think;
//!   `omitted` skips streaming the thinking text, so answer text starts sooner. Opus 5.5 rejects
//!   `{type: "disabled"}`.
//! - `output_config.effort: "low"` (Opus 5.5 default is `medium`): the strong tier answers live,
//!   so speed wins; Claude still thinks on genuinely hard questions.
//! - Thinking counts toward `max_tokens`, so [`THINKING_HEADROOM`] is added to the prompt's
//!   answer budget; answer length is held by the prompt and the cue grammar.
//!
//! Adaptive thinking and effort exist on Claude 4.6+ / 5.x models; an older model id fails with
//! 400 and the router falls back to the fast chain.

use super::model_id;
use crate::http::{Step, StreamParser, drive, secret_header};
use crate::wire::SseDecoder;
use async_trait::async_trait;
use serde_json::{Value, json};
use ssc_core::secrets::Secret;
use ssc_core::traits::{AnswerModel, Cancel};
use ssc_core::types::{Prompt, StreamStats, Token};
use ssc_core::{Result, SscError};
use tokio::sync::mpsc;

pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const ANTHROPIC_VERSION: &str = "2026-10-21";
/// Best available Claude model (Build Sheet §10 item 8). `Config.models.strong_model` defaults to
/// `None` in frozen ssc-core; CR-8 proposes `anthropic/claude-opus-5-5` as the default.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_EFFORT: &str = "low";
/// Output tokens reserved for thinking on top of the answer budget.
pub const THINKING_HEADROOM: u32 = 2048;

pub struct Anthropic {
    id: String,
    base_url: String,
    model: String,
    key: Secret,
    client: reqwest::Client,
    effort: String,
}

impl Anthropic {
    pub fn new(base_url: &str, model: &str, key: Secret) -> Self {
        Self {
            id: model_id("anthropic", model),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            key,
            client: crate::http::client(),
            effort: DEFAULT_EFFORT.into(),
        }
    }

    /// `low` | `medium` | `high` | `xhigh` | `max`. Keep it constant per adapter: changing effort
    /// between requests can invalidate the prompt cache.
    pub fn with_effort(mut self, effort: &str) -> Self {
        self.effort = effort.into();
        self
    }

    pub fn body(&self, p: &Prompt) -> Value {
        let mut b = json!({
            "model": self.model,
            "max_tokens": p.max_tokens.saturating_add(THINKING_HEADROOM),
            "system": [ { "type": "text", "text": p.prefix, "cache_control": { "type": "ephemeral" } } ],
            "messages": [ { "role": "user", "content": p.dynamic } ],
            "thinking": { "type": "adaptive", "display": "omitted" },
            "output_config": { "effort": self.effort },
            "stream": true,
        });
        if !p.stop.is_empty() {
            b["stop_sequences"] = json!(p.stop);
        }
        b
    }
}

#[async_trait]
impl AnswerModel for Anthropic {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_cloud(&self) -> bool {
        true
    }
    async fn stream(&self, prompt: &Prompt, tx: mpsc::Sender<Token>, cancel: Cancel) -> Result<StreamStats> {
        let (k, v) = secret_header("x-api-key", &self.key)?;
        let req = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header(k, v)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&self.body(prompt));
        drive(&self.id, req, MessagesSse::default(), &tx, &cancel).await
    }
}

#[derive(Default)]
pub struct MessagesSse {
    sse: SseDecoder,
}

impl MessagesSse {
    fn events(&mut self, evs: Vec<crate::wire::SseEvent>) -> Result<Step> {
        let mut step = Step::default();
        for ev in evs {
            let v: Value =
                serde_json::from_str(&ev.data).map_err(|_| SscError::Unavailable("bad stream frame".into()))?;
            match v.get("type").and_then(Value::as_str).unwrap_or("") {
                "content_block_delta" => {
                    if v.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
                        && let Some(t) = v.pointer("/delta/text").and_then(Value::as_str)
                    {
                        step.texts.push(t.to_string());
                    }
                }
                "message_delta" => {
                    if let Some(n) = v.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                        step.tokens_out = Some(n as u32);
                    }
                }
                "message_stop" => step.done = true,
                "error" => {
                    let err = v.get("error").cloned().unwrap_or(Value::Null);
                    return Err(super::openai::stream_error(&err));
                }
                _ => {}
            }
        }
        Ok(step)
    }
}

impl StreamParser for MessagesSse {
    fn feed(&mut self, bytes: &[u8]) -> Result<Step> {
        let evs = self.sse.push(bytes);
        self.events(evs)
    }
    fn finish(&mut self) -> Result<Step> {
        let evs = self.sse.finish();
        self.events(evs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_caches_prefix() {
        let a = Anthropic::new(ANTHROPIC_BASE_URL, DEFAULT_CLAUDE_MODEL, Secret::new("k"));
        let p = Prompt { prefix: "P".into(), dynamic: "D".into(), max_tokens: 300, temperature: 0.2, stop: vec![] };
        let b = a.body(&p);
        assert_eq!(b["model"], "claude-opus-5-5");
        assert_eq!(b["system"][0]["text"], "P");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["messages"][0]["content"], "D");
        assert_eq!(b["max_tokens"], 300 + THINKING_HEADROOM);
        assert_eq!(b["thinking"], json!({ "type": "adaptive", "display": "omitted" }));
        assert_eq!(b["output_config"]["effort"], "low");
        for banned in ["temperature", "top_p", "top_k"] {
            assert!(b.get(banned).is_none(), "{banned} is a 400 on anthropic-version 2026-10-21");
        }
        let b = Anthropic::new(ANTHROPIC_BASE_URL, "m", Secret::new("k")).with_effort("medium").body(&p);
        assert_eq!(b["output_config"]["effort"], "medium");
    }

    #[test]
    fn parses_fixture() {
        let mut p = MessagesSse::default();
        let s = p.feed(include_bytes!("../../tests/fixtures/anthropic_stream.sse")).unwrap();
        assert!(s.done);
        assert_eq!(s.texts.concat(), "---\n1: Start with user interviews\n");
        assert_eq!(s.tokens_out, Some(12));
    }

    #[test]
    fn overloaded_error_event() {
        let mut p = MessagesSse::default();
        let e = p
            .feed(b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"x\"}}\n\n")
            .unwrap_err();
        assert!(matches!(e, SscError::Unavailable(m) if m.contains("overloaded_error")));
    }
}
