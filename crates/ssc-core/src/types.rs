//! Data passed between components on the PC.

use crate::protocol::{Layer, Speaker};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use crate::protocol::{MemoryEdit, MemoryItem};

// ---------- screen ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenTextSource {
    Accessibility,
    Ocr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// One text-bearing element of the focused window. Password fields are never included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenNode {
    /// Accessibility role, e.g. "Edit", "Text", "AXStaticText".
    pub role: String,
    pub text: String,
    pub bounds: Option<Rect>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub app: String,
    pub window_title: String,
    pub nodes: Vec<ScreenNode>,
    pub source: ScreenTextSource,
    pub captured_at_ms: u64,
}

impl ScreenSnapshot {
    /// Node texts joined by newlines, in tree order.
    pub fn text(&self) -> String {
        self.nodes.iter().map(|n| n.text.as_str()).collect::<Vec<_>>().join("\n")
    }
}

/// Max size of a screen diff passed downstream (spec: "up to about 2 KB").
pub const SCREEN_DIFF_MAX_BYTES: usize = 2048;

/// What changed since the last snapshot. Newest content first, at most [`SCREEN_DIFF_MAX_BYTES`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScreenDiff {
    pub added_lines: Vec<String>,
    pub removed_lines: usize,
    /// True when content was dropped to respect the byte cap.
    pub truncated: bool,
}

impl ScreenDiff {
    pub fn is_empty(&self) -> bool {
        self.added_lines.is_empty() && self.removed_lines == 0
    }
    pub fn text(&self) -> String {
        self.added_lines.join("\n")
    }
    /// Changed lines (added + removed): the trigger engine's "big screen change" measure.
    pub fn changed_lines(&self) -> usize {
        self.added_lines.len() + self.removed_lines
    }
}

// ---------- audio / transcript ----------

/// All audio inside the agent is 16 kHz mono PCM16. Sources resample before sending.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

#[derive(Debug, Clone, PartialEq)]
pub struct AudioChunk {
    pub speaker: Speaker,
    /// Capture time of the first sample, unix ms.
    pub t0_ms: u64,
    pub samples: Vec<i16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub speaker: Speaker,
    pub text: String,
    pub is_final: bool,
    pub t0_ms: u64,
    pub t1_ms: u64,
    /// Set by VAD when this segment ends an utterance (silence followed); `t1_ms` is the
    /// `speech_end` timestamp used for latency.
    pub ends_utterance: bool,
}

// ---------- prompts / models ----------

/// A rendered prompt. `prefix` is byte-stable within a session (prompt caching); `dynamic` changes per call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    pub prefix: String,
    pub dynamic: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub stop: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamStats {
    /// Provider/model id actually used, e.g. "groq/openai/gpt-oss-120b".
    pub model: String,
    /// Request sent → first token, ms. None when no token arrived.
    pub first_token_ms: Option<u64>,
    pub total_ms: u64,
    pub tokens_out: u32,
}

// ---------- memory ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AboutMe {
    pub name: String,
    pub role: String,
    pub company: String,
    pub product: String,
    pub tone: String,
    pub preferred_answers: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Slug, e.g. "sales-product-x".
    pub id: String,
    pub name: String,
    pub instructions: String,
}

/// A retrieved document chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: Uuid,
    pub doc_id: Uuid,
    /// Shown as `source_doc` on the answer card.
    pub doc_name: String,
    pub text: String,
    /// Higher is more relevant.
    pub score: f32,
}

/// Helper for the memory layer of an item, re-exported for convenience.
pub type MemoryLayer = Layer;
