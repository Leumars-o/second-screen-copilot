//! Component interfaces (Build Sheet §4.4). Every trait has a mock in [`crate::mock`].
//!
//! Conventions:
//! - Long-running producers take a `CancellationToken` and return immediately after spawning.
//! - Async traits use `async_trait` so they can be boxed (`Arc<dyn AnswerModel>`) for routing.

use crate::Result;
use crate::decision::{Decision, DecisionInput};
use crate::protocol::{Envelope, Layer, MemoryEdit, MemoryItem, Message};
use crate::types::*;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub use tokio_util::sync::CancellationToken as Cancel;

/// Focused window → structured text. Must return within ~50 ms (p95).
pub trait ScreenSource: Send + Sync {
    fn snapshot(&self) -> Result<ScreenSnapshot>;
    /// CR-20 screen hotkey: the focused window now, with an image when `image` is set and the
    /// platform can capture one. Default: the snapshot's text only.
    fn grab(&self, image: bool) -> Result<crate::types::ScreenGrab> {
        let err = image.then(|| "no colour capture on this platform".to_string());
        Ok(crate::types::ScreenGrab::from_snapshot(self.snapshot()?, err))
    }
}

/// Mic ("me") or system audio ("them"), 16 kHz mono PCM16.
pub trait AudioSource: Send + Sync {
    fn start(&self, tx: mpsc::Sender<AudioChunk>, cancel: CancellationToken) -> Result<()>;
}

/// Streaming speech-to-text. Emits partial and final segments, keeping the speaker label.
pub trait SttEngine: Send + Sync {
    fn start(
        &self,
        rx: mpsc::Receiver<AudioChunk>,
        tx: mpsc::Sender<TranscriptSegment>,
        cancel: CancellationToken,
    ) -> Result<()>;
}

/// Jev (cloud) or Tev1 4B (local): one call answers every decision question.
#[async_trait]
pub trait DecisionModel: Send + Sync {
    fn id(&self) -> &str;
    async fn decide(&self, input: &DecisionInput) -> Result<Decision>;
}

/// Streams answer tokens. Returns [`crate::SscError::RateLimited`] / [`crate::SscError::Timeout`]
/// before the first token so the router can fail over.
#[async_trait]
pub trait AnswerModel: Send + Sync {
    fn id(&self) -> &str;
    /// True if this model sends data off the PC. The router refuses these in local-only mode.
    fn is_cloud(&self) -> bool;
    async fn stream(&self, prompt: &Prompt, tx: mpsc::Sender<Token>, cancel: CancellationToken) -> Result<StreamStats>;
}

pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Local memory (SQLite + vector index). Sync: callers use `spawn_blocking`.
pub trait MemoryStore: Send + Sync {
    fn list(&self, layer: Option<Layer>) -> Result<Vec<MemoryItem>>;
    fn upsert(&self, edit: MemoryEdit) -> Result<MemoryItem>;
    fn delete(&self, id: Uuid) -> Result<()>;
    /// Top-`k` doc chunks for `query` within one profile, best first.
    fn search(&self, query: &str, profile_id: &str, k: usize) -> Result<Vec<Chunk>>;
}

/// Encrypted link to the phone (LAN or relay — callers can't tell). The transport assigns
/// `seq` and `ts`, encrypts, and drops replayed or invalid incoming frames.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, session_id: Option<Uuid>, message: Message) -> Result<()>;
    /// Incoming frames, already decrypted and validated. Returns `Some` once.
    fn take_incoming(&self) -> Option<mpsc::Receiver<Envelope>>;
}

pub trait Clock: Send + Sync {
    /// Unix ms.
    fn now_ms(&self) -> u64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }
}
