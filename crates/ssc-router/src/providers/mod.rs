//! Answer-model adapters (Build Sheet §10 item 8). Each implements
//! [`ssc_core::traits::AnswerModel`] over a warm pooled client and streams through
//! [`crate::http::drive`]; SSE framing is shared ([`crate::wire::SseDecoder`]). Keys come from
//! `SecretStore` and are only ever placed in sensitive headers.
//!
//! - [`OpenAiCompat`]: the one OpenAI-compatible chat-completions adapter — Groq, Gemini
//!   ([`OpenAiCompat::gemini`]) and OpenAI-compatible strong models.
//! - [`Anthropic`]: native Messages API for Claude (prompt caching, adaptive thinking, effort).
//! - [`Ollama`]: native `/api/chat` (kept on purpose; see the module docs for what `/v1` loses).

pub mod anthropic;
pub mod ollama;
pub mod openai;

pub use anthropic::Anthropic;
pub use ollama::Ollama;
pub use openai::OpenAiCompat;
pub use ssc_core::config::GeminiTier;

use ssc_core::secrets::Secret;

pub(crate) fn model_id(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// Fire a cheap GET to open the TLS/HTTP-2 connection before the first real request.
/// The result is ignored: any response means the connection is now pooled.
pub(crate) async fn warm_get(client: &reqwest::Client, url: &str, bearer: Option<&Secret>) {
    let mut r = client.get(url).timeout(std::time::Duration::from_secs(5));
    if let Some(k) = bearer {
        r = r.bearer_auth(k.expose());
    }
    let _ = r.send().await;
}

/// True when `url` points at this machine. Used to classify an Ollama endpoint: a remote Ollama
/// host sends data off the PC, so it counts as cloud.
pub fn is_loopback_url(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    let Some(h) = u.host_str() else { return false };
    let bare = h.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    bare.eq_ignore_ascii_case("localhost")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback() {
        assert!(is_loopback_url("http://127.0.0.1:11434"));
        assert!(is_loopback_url("http://localhost:11434/"));
        assert!(is_loopback_url("http://[::1]:11434"));
        assert!(!is_loopback_url("http://192.168.1.20:11434"));
        assert!(!is_loopback_url("https://ollama.example.com"));
        assert!(!is_loopback_url("not a url"));
    }
}
