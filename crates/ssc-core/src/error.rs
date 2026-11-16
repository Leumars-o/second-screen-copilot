//! Error type shared by all component traits. Variants the router acts on are explicit.

#[derive(Debug, thiserror::Error)]
pub enum SscError {
    /// Provider returned 429 or a local budget was exhausted.
    #[error("rate limited")]
    RateLimited { retry_after_ms: Option<u64> },
    /// No response (or no first token) within the deadline.
    #[error("timed out after {0} ms")]
    Timeout(u64),
    /// Network down, provider 5xx, model not loaded, device missing.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Input or output failed validation.
    #[error("invalid: {0}")]
    Invalid(String),
    /// Cancelled through a `CancellationToken`.
    #[error("cancelled")]
    Cancelled,
    #[error("not found")]
    NotFound,
    /// Blocked by a deterministic policy (local-only mode, redaction). Never retried on another provider.
    #[error("blocked by policy: {0}")]
    Policy(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T, E = SscError> = std::result::Result<T, E>;
