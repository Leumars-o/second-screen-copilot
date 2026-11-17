//! Shared HTTP plumbing: one warm, pooled rustls client per provider, HTTP status → [`SscError`]
//! mapping, and the streaming driver every answer adapter uses (cancellation-aware at every await).

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{RequestBuilder, Response, StatusCode};
use ssc_core::secrets::Secret;
use ssc_core::traits::Cancel;
use ssc_core::types::{StreamStats, Token};
use ssc_core::{Result, SscError};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Idle pooled connections are dropped after this ([`crate::warm::REWARM_EVERY`] is shorter).
pub const POOL_IDLE: Duration = Duration::from_secs(300);

/// A client tuned for low first-token latency: kept-alive pooled connections (HTTP/2 when the
/// server offers it via ALPN), TCP_NODELAY, rustls.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_rustls_tls()
        .connect_timeout(Duration::from_secs(3))
        .pool_idle_timeout(POOL_IDLE)
        .pool_max_idle_per_host(4)
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_nodelay(true)
        .http2_adaptive_window(true)
        .http2_keep_alive_interval(Duration::from_secs(20))
        .http2_keep_alive_while_idle(true)
        .build()
        .expect("reqwest client")
}

/// A header carrying a key; marked sensitive so it never appears in `Debug` output.
pub fn secret_header(name: &'static str, value: &Secret) -> Result<(HeaderName, HeaderValue)> {
    let mut v =
        HeaderValue::from_str(value.expose()).map_err(|_| SscError::Invalid(format!("{name}: bad key format")))?;
    v.set_sensitive(true);
    Ok((HeaderName::from_static(name), v))
}

pub fn map_reqwest(e: reqwest::Error, elapsed_ms: u64) -> SscError {
    if e.is_timeout() {
        SscError::Timeout(elapsed_ms)
    } else if e.is_connect() {
        SscError::Unavailable("connect failed".into())
    } else {
        SscError::Unavailable(e.without_url().to_string())
    }
}

/// Map a non-2xx response. Bodies are not echoed into the error (they can quote prompt text);
/// only the provider's error type/code is kept.
pub fn map_status(status: StatusCode, headers: &HeaderMap, body: &[u8], elapsed_ms: u64) -> SscError {
    let code = error_code(body);
    let what = match &code {
        Some(c) => format!("http {} ({c})", status.as_u16()),
        None => format!("http {}", status.as_u16()),
    };
    match status.as_u16() {
        429 => SscError::RateLimited { retry_after_ms: retry_after_ms(headers) },
        408 | 504 => SscError::Timeout(elapsed_ms),
        400 | 404 | 413 | 415 | 422 => SscError::Invalid(what),
        _ => SscError::Unavailable(what),
    }
}

fn retry_after_ms(h: &HeaderMap) -> Option<u64> {
    let v = h.get("retry-after")?.to_str().ok()?.trim();
    v.parse::<f64>().ok().filter(|s| *s >= 0.0).map(|s| (s * 1000.0) as u64)
}

/// The error object of a provider error body. Shapes seen: `{"error":{…}}` (OpenAI, Groq,
/// Anthropic, Gemini native), `[{"error":{…}}]` (Gemini's OpenAI-compatible endpoint) and
/// `{"detail":{"error_type","message"}}` (TypeSafe `/v1/systemone`, verified live 2026-11-17).
fn error_object(body: &[u8]) -> Option<serde_json::Value> {
    let v = match serde_json::from_slice(body).ok()? {
        serde_json::Value::Array(a) => a.into_iter().next()?,
        v => v,
    };
    let inner = v.get("error").or_else(|| v.get("detail")).filter(|e| e.is_object()).cloned();
    Some(inner.unwrap_or(v))
}

fn error_code(body: &[u8]) -> Option<String> {
    let e = error_object(body)?;
    let pick = |k: &str| e.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let s = pick("code").or_else(|| pick("error_type")).or_else(|| pick("type")).or_else(|| pick("status"))?;
    Some(s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.').take(64).collect())
}

/// The provider's human-readable error message, on one line and capped at `max` chars. Only for
/// endpoints whose messages describe the request shape (e.g. "Unknown model: x"), not the prompt.
/// Answer adapters never use it: chat providers can quote prompt text in their errors.
pub fn error_message(body: &[u8], max: usize) -> Option<String> {
    let e = error_object(body)?;
    let m = e.get("message").and_then(|x| x.as_str())?;
    let s: String = m.chars().map(|c| if c.is_control() { ' ' } else { c }).take(max).collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// What a provider parser extracted from one chunk of the body.
#[derive(Debug, Default)]
pub struct Step {
    pub texts: Vec<String>,
    pub done: bool,
    /// Output token count when the provider reports it.
    pub tokens_out: Option<u32>,
}

/// Provider-specific body parser.
pub trait StreamParser: Send {
    fn feed(&mut self, bytes: &[u8]) -> Result<Step>;
    fn finish(&mut self) -> Result<Step>;
}

async fn cancellable<T>(cancel: &Cancel, f: impl std::future::Future<Output = T>) -> Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(SscError::Cancelled),
        v = f => Ok(v),
    }
}

/// Longest gap between two body chunks once the response has started (M3). A provider that stalls
/// mid-stream returns `Timeout` instead of hanging the answer: before any text was forwarded the
/// router fails over, after that it ends the answer with what it has.
///
/// Not applied to the wait for the response headers: Ollama sends them only once the model is
/// loaded (seconds on a cold load), and cloud routes have the router's first-token deadline.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(8);

/// Send `req`, then stream parsed text to `tx` until the body ends, the parser says done, or
/// `cancel` fires. Dropping the response on cancel closes the stream immediately.
pub async fn drive(
    model_id: &str,
    req: RequestBuilder,
    parser: impl StreamParser,
    tx: &mpsc::Sender<Token>,
    cancel: &Cancel,
) -> Result<StreamStats> {
    drive_with_idle(model_id, req, parser, tx, cancel, DEFAULT_IDLE_TIMEOUT).await
}

/// [`drive`] with an explicit inter-chunk idle timeout.
pub async fn drive_with_idle(
    model_id: &str,
    req: RequestBuilder,
    mut parser: impl StreamParser,
    tx: &mpsc::Sender<Token>,
    cancel: &Cancel,
    idle: Duration,
) -> Result<StreamStats> {
    let start = Instant::now();
    let ms = |s: Instant| s.elapsed().as_millis() as u64;
    let resp: Response = cancellable(cancel, req.send()).await?.map_err(|e| map_reqwest(e, ms(start)))?;
    let mut resp = resp;
    if !resp.status().is_success() {
        let status = resp.status();
        let headers = resp.headers().clone();
        let mut body = Vec::new();
        while body.len() < 4096 {
            match cancellable(cancel, resp.chunk()).await? {
                Ok(Some(b)) => body.extend_from_slice(&b),
                _ => break,
            }
        }
        return Err(map_status(status, &headers, &body, ms(start)));
    }

    let mut first: Option<u64> = None;
    let mut count = 0u32;
    let mut reported = None;
    loop {
        let chunk = match cancellable(cancel, tokio::time::timeout(idle, resp.chunk())).await? {
            Ok(c) => c,
            Err(_) => return Err(SscError::Timeout(ms(start))),
        };
        let (step, end) = match chunk {
            Ok(Some(b)) => (parser.feed(&b)?, false),
            Ok(None) => (parser.finish()?, true),
            Err(e) => return Err(map_reqwest(e, ms(start))),
        };
        if step.tokens_out.is_some() {
            reported = step.tokens_out;
        }
        for text in step.texts {
            if text.is_empty() {
                continue;
            }
            first.get_or_insert_with(|| ms(start));
            count += 1;
            if cancellable(cancel, tx.send(Token { text })).await?.is_err() {
                return Err(SscError::Cancelled); // receiver gone
            }
        }
        if step.done || end {
            break;
        }
    }
    Ok(StreamStats {
        model: model_id.to_string(),
        first_token_ms: first,
        total_ms: ms(start),
        tokens_out: reported.unwrap_or(count),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("2"));
        assert!(matches!(
            map_status(StatusCode::TOO_MANY_REQUESTS, &h, b"", 5),
            SscError::RateLimited { retry_after_ms: Some(2000) }
        ));
        assert!(matches!(map_status(StatusCode::GATEWAY_TIMEOUT, &HeaderMap::new(), b"", 7), SscError::Timeout(7)));
        assert!(matches!(map_status(StatusCode::BAD_REQUEST, &HeaderMap::new(), b"", 0), SscError::Invalid(_)));
        let e = map_status(
            StatusCode::SERVICE_UNAVAILABLE,
            &HeaderMap::new(),
            br#"{"error":{"message":"secret prompt text","type":"overloaded_error"}}"#,
            0,
        );
        let s = e.to_string();
        assert!(s.contains("503") && s.contains("overloaded_error"), "{s}");
        assert!(!s.contains("secret prompt text"));
    }

    #[test]
    fn error_body_shapes() {
        let gemini_compat = br#"[{"error":{"code":429,"message":"quota","status":"RESOURCE_EXHAUSTED"}}]"#;
        let e = map_status(StatusCode::BAD_GATEWAY, &HeaderMap::new(), gemini_compat, 0);
        assert!(e.to_string().contains("RESOURCE_EXHAUSTED"), "{e}");
        let ts = br#"{"detail":{"error_type":"api_usage_error","message":"Unknown model: jev-1.14\nmore"}}"#;
        let e = map_status(StatusCode::BAD_REQUEST, &HeaderMap::new(), ts, 0);
        assert!(e.to_string().contains("api_usage_error"), "{e}");
        assert_eq!(error_message(ts, 200).as_deref(), Some("Unknown model: jev-1.14 more"));
        assert_eq!(error_message(ts, 7).as_deref(), Some("Unknown"));
        assert_eq!(error_message(b"not json", 50), None);
        assert_eq!(error_message(br#"{"detail":"x"}"#, 50), None);
    }

    #[test]
    fn secret_header_is_sensitive() {
        let (_, v) = secret_header("x-api-key", &Secret::new("sk-123")).unwrap();
        assert!(v.is_sensitive());
        assert!(!format!("{v:?}").contains("sk-123"));
    }
}
