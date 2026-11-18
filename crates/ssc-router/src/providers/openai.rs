//! The one OpenAI-compatible chat-completions adapter (SSE streaming), Build Sheet §10 item 8:
//! Groq (default fast model), Gemini (Groq fallback, via Google's OpenAI-compatible endpoint) and
//! any OpenAI-compatible strong model. Provider differences are only constructor-set body fields.
//!
//! Ollama is deliberately *not* on this adapter; see [`super::ollama`] for why.

use super::{model_id, warm_get};
use crate::http::{Step, StreamParser, drive_with_idle};
use crate::wire::SseDecoder;
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use ssc_core::config::GeminiTier;
use ssc_core::secrets::Secret;
use ssc_core::traits::{AnswerModel, Cancel};
use ssc_core::types::{Prompt, StreamStats, Token};
use ssc_core::{Result, SscError};
use std::time::Duration;
use tokio::sync::mpsc;

pub const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Gemini's OpenAI-compatible endpoint (`Authorization: Bearer <key>`, `POST …/chat/completions`
/// with `stream: true`). Checked against Google's "OpenAI compatibility" page, 2026-11-17.
pub const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

pub struct OpenAiCompat {
    id: String,
    base_url: String,
    model: String,
    key: Secret,
    client: reqwest::Client,
    /// Provider-specific body fields merged into every request.
    extra: Map<String, Value>,
    /// Added to `max_tokens` for reasoning models, whose hidden reasoning tokens count against
    /// the completion limit (L9): the prompt's cap then limits the visible answer, and a small
    /// cap ("shorter") can't be eaten by reasoning and return an empty answer.
    reasoning_headroom: u32,
    /// Inter-chunk idle timeout ([`crate::http::DEFAULT_IDLE_TIMEOUT`]).
    idle: Duration,
}

/// Reasoning headroom for Groq reasoning models at `reasoning_effort: "low"`.
pub const GROQ_REASONING_HEADROOM: u32 = 256;

impl OpenAiCompat {
    pub fn new(provider: &str, base_url: &str, model: &str, key: Secret) -> Self {
        Self {
            id: model_id(provider, model),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            key,
            client: crate::http::client(),
            extra: Map::new(),
            reasoning_headroom: 0,
            idle: crate::http::DEFAULT_IDLE_TIMEOUT,
        }
    }

    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    /// Groq. Reasoning models are told to think briefly and not stream their reasoning, so the
    /// first streamed token is answer text. (Parameter names per Groq's docs; verify with `--features live`.)
    pub fn groq(base_url: &str, model: &str, key: Secret) -> Self {
        let mut m = Self::new("groq", base_url, model, key);
        let lower = model.to_ascii_lowercase();
        if lower.contains("gpt-oss") {
            m.extra.insert("reasoning_effort".into(), json!("low"));
            m.extra.insert("include_reasoning".into(), json!(false));
            m.reasoning_headroom = GROQ_REASONING_HEADROOM;
        } else if lower.contains("qwen") {
            m.extra.insert("reasoning_format".into(), json!("hidden"));
            m.reasoning_headroom = GROQ_REASONING_HEADROOM;
        }
        m
    }

    /// Gemini through its OpenAI-compatible endpoint; ids stay `gemini/<model>`.
    ///
    /// Privacy rule (spec "Answer models"): the Gemini free tier may use prompts to improve Google
    /// products, so client call content goes only to the paid tier. Tier is a property of the API
    /// key's project and can't be detected from a request, so the user declares it; a key declared
    /// `Free` is refused with [`SscError::Policy`] unless `testing` is set.
    ///
    /// Thinking: Google documents `reasoning_effort: "none"` as turning thinking off for 2.5
    /// models, and says it can't be turned off for 2.5 Pro / Gemini 3+; those get `"low"`, the
    /// cheapest level every thinking model accepts. First-token latency matters more than depth.
    pub fn gemini(base_url: &str, model: &str, key: Secret, tier: GeminiTier, testing: bool) -> Result<Self> {
        if tier == GeminiTier::Free && !testing {
            return Err(SscError::Policy(
                "Gemini free tier is for testing only (client calls need the paid tier)".into(),
            ));
        }
        let mut m = Self::new("gemini", base_url, model, key);
        let lower = model.to_ascii_lowercase();
        let effort = if lower.contains("2.5") && !lower.contains("pro") { "none" } else { "low" };
        m.extra.insert("reasoning_effort".into(), json!(effort));
        m.extra.insert("stream_options".into(), json!({ "include_usage": true }));
        Ok(m)
    }

    pub fn with_extra(mut self, key: &str, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }

    pub fn body(&self, p: &Prompt) -> Value {
        // CR-20: images go in the user turn as `image_url` parts (data URLs); text stays first.
        let user = if p.images.is_empty() {
            json!(p.dynamic)
        } else {
            let mut parts = vec![json!({ "type": "text", "text": p.dynamic })];
            parts.extend(p.images.iter().map(|i| json!({ "type": "image_url", "image_url": { "url": i.data_url() } })));
            Value::Array(parts)
        };
        let mut b = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": p.prefix },
                { "role": "user", "content": user },
            ],
            "stream": true,
            "max_tokens": p.max_tokens.saturating_add(self.reasoning_headroom),
            "temperature": p.temperature,
        });
        if !p.stop.is_empty() {
            b["stop"] = json!(p.stop.iter().take(4).collect::<Vec<_>>());
        }
        if let Value::Object(o) = &mut b {
            o.extend(self.extra.clone());
        }
        b
    }

    /// Open the pooled connection (TLS + HTTP/2) ahead of the first answer.
    pub async fn warm(&self) {
        warm_get(&self.client, &format!("{}/models", self.base_url), Some(&self.key)).await;
    }
}

#[async_trait]
impl AnswerModel for OpenAiCompat {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_cloud(&self) -> bool {
        true
    }
    async fn stream(&self, prompt: &Prompt, tx: mpsc::Sender<Token>, cancel: Cancel) -> Result<StreamStats> {
        let req = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(self.key.expose())
            .json(&self.body(prompt));
        drive_with_idle(&self.id, req, ChatSse::default(), &tx, &cancel, self.idle).await
    }
}

/// `data: {"choices":[{"delta":{"content":"…"}}]}` … `data: [DONE]`. Only `delta.content` is
/// answer text; `reasoning` / `reasoning_content` deltas are ignored.
#[derive(Default)]
pub struct ChatSse {
    sse: SseDecoder,
}

impl ChatSse {
    fn events(&mut self, evs: Vec<crate::wire::SseEvent>) -> Result<Step> {
        let mut step = Step::default();
        for ev in evs {
            if ev.data.trim() == "[DONE]" {
                step.done = true;
                break;
            }
            let v: Value =
                serde_json::from_str(&ev.data).map_err(|_| SscError::Unavailable("bad stream frame".into()))?;
            // Gemini's compatibility layer wraps error frames in a one-element array.
            let v = match v {
                Value::Array(a) => a.into_iter().next().unwrap_or(Value::Null),
                v => v,
            };
            if let Some(err) = v.get("error") {
                return Err(stream_error(err));
            }
            if let Some(t) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                step.texts.push(t.to_string());
            }
            let usage = v.get("usage").filter(|u| !u.is_null()).or_else(|| v.pointer("/x_groq/usage"));
            if let Some(n) = usage.and_then(|u| u.get("completion_tokens")).and_then(Value::as_u64) {
                step.tokens_out = Some(n as u32);
            }
        }
        Ok(step)
    }
}

/// Map an in-stream error object (`type` / `code` / `status`) without echoing its message.
pub(crate) fn stream_error(err: &Value) -> SscError {
    let s = |k: &str| err.get(k).and_then(Value::as_str);
    let kind = s("type")
        .or_else(|| s("code"))
        .or_else(|| s("status"))
        .unwrap_or("error")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect::<String>();
    let code = err.get("code").and_then(Value::as_u64);
    if kind.contains("rate_limit") || kind == "RESOURCE_EXHAUSTED" || code == Some(429) {
        SscError::RateLimited { retry_after_ms: None }
    } else {
        SscError::Unavailable(format!("stream error ({kind})"))
    }
}

impl StreamParser for ChatSse {
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
    fn images_ride_in_the_user_turn_as_data_urls() {
        let o = OpenAiCompat::groq(GROQ_BASE_URL, "qwen/qwen3.8-27b", Secret::new("k"));
        let p = Prompt {
            prefix: "P".into(),
            dynamic: "D".into(),
            max_tokens: 10,
            temperature: 0.2,
            stop: vec![],
            images: vec![ssc_core::types::PromptImage {
                mime: "image/jpeg".into(),
                data: vec![0xFF, 0xD8, 0xFF, 0xD9],
            }],
        };
        let b = o.body(&p);
        assert_eq!(b["messages"][0]["content"], "P", "system stays plain text");
        let u = &b["messages"][1]["content"];
        assert_eq!(u[0], json!({ "type": "text", "text": "D" }));
        assert_eq!(u[1]["type"], "image_url");
        assert_eq!(u[1]["image_url"]["url"], "data:image/jpeg;base64,/9j/2Q==");
        // No images: plain string content as before.
        let b = o.body(&Prompt { images: vec![], ..p });
        assert_eq!(b["messages"][1]["content"], "D");
    }

    fn prompt() -> Prompt {
        Prompt {
            prefix: "P".into(),
            dynamic: "D".into(),
            max_tokens: 80,
            temperature: 0.2,
            stop: vec!["\n\n\n".into()],
            images: vec![],
        }
    }

    #[test]
    fn groq_body_shape() {
        let m = OpenAiCompat::groq(GROQ_BASE_URL, "openai/gpt-oss-120b", Secret::new("k"));
        let b = m.body(&prompt());
        assert_eq!(b["model"], "openai/gpt-oss-120b");
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][0]["content"], "P");
        assert_eq!(b["messages"][1]["content"], "D");
        assert_eq!(b["stream"], true);
        // Reasoning tokens count against the limit: 80 answer tokens + headroom (L9).
        assert_eq!(b["max_tokens"], 80 + GROQ_REASONING_HEADROOM);
        assert_eq!(b["reasoning_effort"], "low");
        assert_eq!(b["include_reasoning"], false);
        assert_eq!(b["stop"][0], "\n\n\n");
        assert!(b.get("stream_options").is_none());
        assert_eq!(m.id(), "groq/openai/gpt-oss-120b");
        let q = OpenAiCompat::groq(GROQ_BASE_URL, "qwen/qwen3-32b", Secret::new("k")).body(&prompt());
        assert_eq!(q["reasoning_format"], "hidden");
        assert!(q.get("reasoning_effort").is_none());
    }

    #[test]
    fn gemini_free_tier_needs_testing_flag() {
        let k = || Secret::new("k");
        let g = |tier, testing| OpenAiCompat::gemini(GEMINI_BASE_URL, "m", k(), tier, testing);
        assert!(matches!(g(GeminiTier::Free, false), Err(SscError::Policy(_))));
        assert!(g(GeminiTier::Free, true).is_ok());
        assert!(g(GeminiTier::Paid, false).is_ok());
    }

    #[test]
    fn gemini_body_shape() {
        let g =
            OpenAiCompat::gemini(GEMINI_BASE_URL, "gemini-2.5-flash-lite", Secret::new("k"), GeminiTier::Paid, false)
                .unwrap();
        assert_eq!(g.id(), "gemini/gemini-2.5-flash-lite");
        assert_eq!(g.base_url, "https://generativelanguage.googleapis.com/v1beta/openai");
        let b = g.body(&prompt());
        assert_eq!(b["model"], "gemini-2.5-flash-lite");
        assert_eq!(b["messages"][0]["content"], "P");
        assert_eq!(b["reasoning_effort"], "none");
        assert_eq!(b["stream_options"]["include_usage"], true);
        let effort = |m: &str| {
            OpenAiCompat::gemini(GEMINI_BASE_URL, m, Secret::new("k"), GeminiTier::Paid, false).unwrap().body(&prompt())
                ["reasoning_effort"]
                .clone()
        };
        assert_eq!(effort("gemini-2.5-pro"), "low", "2.5 Pro can't turn thinking off");
        assert_eq!(effort("gemini-3-flash"), "low");
    }

    #[test]
    fn parses_groq_fixture() {
        let mut p = ChatSse::default();
        let s = p.feed(include_bytes!("../../tests/fixtures/groq_stream.sse")).unwrap();
        assert!(s.done);
        assert_eq!(s.texts.concat(), "1. Requirements\n2. Design\n3. Build\n---\n1: Ask about users\n");
        assert_eq!(s.tokens_out, Some(17));
    }

    #[test]
    fn parses_gemini_fixture() {
        let mut p = ChatSse::default();
        let s = p.feed(include_bytes!("../../tests/fixtures/gemini_stream.sse")).unwrap();
        assert!(s.done);
        assert_eq!(s.texts.concat(), "1. Requirements\n2. Design\n3. Build\n---\n1: Ask about users\n");
        assert_eq!(s.tokens_out, Some(16));
    }

    #[test]
    fn stream_error_frames() {
        let mut p = ChatSse::default();
        let e = p.feed(b"data: {\"error\":{\"message\":\"x\",\"type\":\"rate_limit_exceeded\"}}\n\n").unwrap_err();
        assert!(matches!(e, SscError::RateLimited { .. }));
        let mut p = ChatSse::default();
        let e = p
            .feed(b"data: [{\"error\":{\"code\":429,\"message\":\"q\",\"status\":\"RESOURCE_EXHAUSTED\"}}]\n\n")
            .unwrap_err();
        assert!(matches!(e, SscError::RateLimited { .. }), "{e:?}");
        let mut p = ChatSse::default();
        let e =
            p.feed(b"data: {\"error\":{\"code\":500,\"message\":\"secret\",\"status\":\"INTERNAL\"}}\n\n").unwrap_err();
        assert!(matches!(&e, SscError::Unavailable(m) if m.contains("INTERNAL") && !m.contains("secret")), "{e:?}");
    }
}
