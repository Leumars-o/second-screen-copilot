//! Connection and model warm-up (M2).
//!
//! - Cloud answer providers: one cheap `GET /models` opens the pooled TLS/HTTP-2 connection, so
//!   the first answer doesn't pay the handshake. The pool drops idle connections after
//!   [`crate::http::POOL_IDLE`]; [`REWARM_EVERY`] is shorter, so a capturing session re-warms
//!   before that happens (the caller skips the re-warm when an answer used the pool recently).
//! - Local answer model (Ollama, when it is the primary answer model): load it and evaluate the
//!   session's fixed prompt prefix once, so the first question reuses the KV cache (measured on
//!   the M1 Pro: gemma4:e2b first token 36 ms warm vs 704 ms cold).
//!
//! `GET /models` is not a completion: it reserves nothing from the Groq budget.

use crate::providers::{Ollama, OpenAiCompat};
use std::sync::Arc;
use std::time::Duration;

/// Re-warm interval while capturing; below the pool's idle timeout.
pub const REWARM_EVERY: Duration = Duration::from_secs(240);

#[derive(Clone, Default)]
pub struct Warmer {
    pub(crate) cloud: Vec<Arc<OpenAiCompat>>,
    /// The local answer model when it is the primary answerer (local-only mode). In cloud mode
    /// the local model is the last-resort fallback and is not preloaded (it would hold ~7 GB of
    /// the 16 GB machine for a path that rarely runs).
    pub(crate) local: Option<Arc<Ollama>>,
}

impl Warmer {
    /// Number of things this warmer touches (tests, debug).
    pub fn targets(&self) -> usize {
        self.cloud.len() + usize::from(self.local.is_some())
    }

    /// Session start: open every cloud connection and, for a local answer model, load it and
    /// pre-fill `local_prefix`. All in parallel; failures are ignored (warm-up is best effort).
    pub async fn warm(&self, local_prefix: Option<&str>) {
        let cloud = self.rewarm_cloud();
        let local = async {
            if let Some(o) = &self.local {
                let _ = o.preload().await;
                if let Some(p) = local_prefix.filter(|p| !p.is_empty()) {
                    let _ = o.warm_prefix(p).await;
                }
            }
        };
        tokio::join!(cloud, local);
    }

    /// Keep the cloud connections in the pool.
    pub async fn rewarm_cloud(&self) {
        let tasks: Vec<_> = self
            .cloud
            .iter()
            .map(|c| {
                let c = c.clone();
                tokio::spawn(async move { c.warm().await })
            })
            .collect();
        for t in tasks {
            let _ = t.await;
        }
    }
}
