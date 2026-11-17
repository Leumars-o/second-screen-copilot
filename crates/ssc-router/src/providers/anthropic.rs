//! Anthropic-style Messages API with SSE streaming, for the optional strong tier.
//! The fixed prefix goes in `system` with `cache_control` so prompt caching applies.

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
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct Anthropic {
    id: String,
    base_url: String,
    model: String,
    key: Secret,
    client: reqwest::Client,
}

impl Anthropic {
    pub fn new(base_url: &str, model: &str, key: Secret) -> Self {
        Self {
            id: model_id("anthropic", model),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            key,
            client: crate::http::client(),
        }
    }

    pub fn body(&self, p: &Prompt) -> Value {
        let mut b = json!({
            "model": self.model,
            "max_tokens": p.max_tokens,
            "system": [ { "type": "text", "text": p.prefix, "cache_control": { "type": "ephemeral" } } ],
            "messages": [ { "role": "user", "content": p.dynamic } ],
            "stream": true,
            "temperature": p.temperature,
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
        let a = Anthropic::new(ANTHROPIC_BASE_URL, "strong-model", Secret::new("k"));
        let p = Prompt { prefix: "P".into(), dynamic: "D".into(), max_tokens: 300, temperature: 0.2, stop: vec![] };
        let b = a.body(&p);
        assert_eq!(b["system"][0]["text"], "P");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["messages"][0]["content"], "D");
        assert_eq!(b["max_tokens"], 300);
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
