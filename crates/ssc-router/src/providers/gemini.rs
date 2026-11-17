//! Gemini `streamGenerateContent?alt=sse` (Flash-Lite as the Groq fallback).
//!
//! Privacy rule (spec "Answer models"): the Gemini free tier may use prompts to improve Google
//! products, so client call content goes only to the paid tier. Tier is a property of the API key's
//! project and can't be detected from a request, so the user declares it; a key declared `Free`
//! is refused unless the `testing` flag is set.

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

pub const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com";

pub use ssc_core::config::GeminiTier;

pub struct Gemini {
    id: String,
    base_url: String,
    model: String,
    key: Secret,
    client: reqwest::Client,
}

impl Gemini {
    /// Fails with [`SscError::Policy`] for a free-tier key unless `testing` is set.
    pub fn new(base_url: &str, model: &str, key: Secret, tier: GeminiTier, testing: bool) -> Result<Self> {
        if tier == GeminiTier::Free && !testing {
            return Err(SscError::Policy(
                "Gemini free tier is for testing only (client calls need the paid tier)".into(),
            ));
        }
        Ok(Self {
            id: model_id("gemini", model),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            key,
            client: crate::http::client(),
        })
    }

    pub fn url(&self) -> String {
        format!("{}/v1beta/models/{}:streamGenerateContent?alt=sse", self.base_url, self.model)
    }

    pub fn body(&self, p: &Prompt) -> Value {
        let mut gen_cfg = json!({
            "maxOutputTokens": p.max_tokens,
            "temperature": p.temperature,
            // No thinking on the live path: first token latency matters more.
            "thinkingConfig": { "thinkingBudget": 0 },
        });
        if !p.stop.is_empty() {
            gen_cfg["stopSequences"] = json!(p.stop.iter().take(5).collect::<Vec<_>>());
        }
        json!({
            "systemInstruction": { "parts": [ { "text": p.prefix } ] },
            "contents": [ { "role": "user", "parts": [ { "text": p.dynamic } ] } ],
            "generationConfig": gen_cfg,
        })
    }

    pub async fn warm(&self) {
        if let Ok((k, v)) = secret_header("x-goog-api-key", &self.key) {
            let _ = self
                .client
                .get(format!("{}/v1beta/models/{}", self.base_url, self.model))
                .header(k, v)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;
        }
    }
}

#[async_trait]
impl AnswerModel for Gemini {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_cloud(&self) -> bool {
        true
    }
    async fn stream(&self, prompt: &Prompt, tx: mpsc::Sender<Token>, cancel: Cancel) -> Result<StreamStats> {
        let (k, v) = secret_header("x-goog-api-key", &self.key)?;
        let req = self.client.post(self.url()).header(k, v).json(&self.body(prompt));
        drive(&self.id, req, GeminiSse::default(), &tx, &cancel).await
    }
}

#[derive(Default)]
pub struct GeminiSse {
    sse: SseDecoder,
}

impl GeminiSse {
    fn events(&mut self, evs: Vec<crate::wire::SseEvent>) -> Result<Step> {
        let mut step = Step::default();
        for ev in evs {
            let v: Value =
                serde_json::from_str(&ev.data).map_err(|_| SscError::Unavailable("bad stream frame".into()))?;
            if let Some(err) = v.get("error") {
                let status = err.get("status").and_then(Value::as_str).unwrap_or("");
                return Err(if status == "RESOURCE_EXHAUSTED" {
                    SscError::RateLimited { retry_after_ms: None }
                } else {
                    super::openai::stream_error(err)
                });
            }
            if let Some(parts) = v.pointer("/candidates/0/content/parts").and_then(Value::as_array) {
                for part in parts {
                    if part.get("thought").and_then(Value::as_bool) == Some(true) {
                        continue;
                    }
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        step.texts.push(t.to_string());
                    }
                }
            }
            if let Some(n) = v.pointer("/usageMetadata/candidatesTokenCount").and_then(Value::as_u64) {
                step.tokens_out = Some(n as u32);
            }
            if v.pointer("/candidates/0/finishReason").and_then(Value::as_str).is_some() {
                step.done = true;
            }
        }
        Ok(step)
    }
}

impl StreamParser for GeminiSse {
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
    fn free_tier_needs_testing_flag() {
        let k = || Secret::new("k");
        assert!(matches!(Gemini::new(GEMINI_BASE_URL, "m", k(), GeminiTier::Free, false), Err(SscError::Policy(_))));
        assert!(Gemini::new(GEMINI_BASE_URL, "m", k(), GeminiTier::Free, true).is_ok());
        assert!(Gemini::new(GEMINI_BASE_URL, "m", k(), GeminiTier::Paid, false).is_ok());
    }

    #[test]
    fn body_and_url() {
        let g =
            Gemini::new(GEMINI_BASE_URL, "gemini-2.5-flash-lite", Secret::new("k"), GeminiTier::Paid, false).unwrap();
        assert_eq!(
            g.url(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-lite:streamGenerateContent?alt=sse"
        );
        assert!(!g.url().contains("key="), "key must travel in a header, not the URL");
        let p = Prompt { prefix: "P".into(), dynamic: "D".into(), max_tokens: 60, temperature: 0.3, stop: vec![] };
        let b = g.body(&p);
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "P");
        assert_eq!(b["contents"][0]["parts"][0]["text"], "D");
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 60);
        assert!(b["generationConfig"].get("stopSequences").is_none());
    }

    #[test]
    fn parses_fixture() {
        let mut p = GeminiSse::default();
        let s = p.feed(include_bytes!("../../tests/fixtures/gemini_stream.sse")).unwrap();
        assert!(s.done);
        assert_eq!(s.texts.concat(), "1. Requirements\n2. Design\n3. Build\n---\n1: Ask about users\n");
        assert_eq!(s.tokens_out, Some(16));
    }
}
