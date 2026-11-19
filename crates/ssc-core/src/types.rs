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
    /// CR-20: images attached to the user turn (the screen hotkey). Adapters that cannot send
    /// images refuse a prompt that has any (`SscError::Unavailable`), so the router fails over.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<PromptImage>,
}

/// CR-20 screen hotkey: the focused window at the moment the hotkey was pressed.
#[derive(Debug, Clone)]
pub struct ScreenGrab {
    pub app: String,
    pub window_title: String,
    /// Accessibility text (or OCR text when no image was taken), in reading order.
    pub text: String,
    pub text_source: ScreenTextSource,
    pub image: Option<PromptImage>,
    /// Pixel size of `image`.
    pub image_size: Option<(u32, u32)>,
    /// Why an image was asked for but not produced (permission, no bounds, ...).
    pub image_error: Option<String>,
    /// CR-23b: small JPEG of what was captured (≤480 px, ≤30 KB), shown on the phone's card.
    pub thumb: Option<Vec<u8>>,
    /// Captured window size in points (before scaling), for the capture log line.
    pub window_size: Option<(i32, i32)>,
}

impl ScreenGrab {
    /// Text-only grab from a snapshot.
    pub fn from_snapshot(snap: ScreenSnapshot, image_error: Option<String>) -> Self {
        Self {
            text: snap.text(),
            app: snap.app,
            window_title: snap.window_title,
            text_source: snap.source,
            image: None,
            image_size: None,
            image_error,
            thumb: None,
            window_size: None,
        }
    }

    /// "App — window title" for the prompt.
    pub fn label(&self) -> String {
        match (self.app.trim(), self.window_title.trim()) {
            ("", "") => String::new(),
            (a, "") => a.to_string(),
            ("", t) => t.to_string(),
            (a, t) => format!("{a} — {t}"),
        }
    }
}

/// An image for a vision model: raw bytes of an encoded image plus its media type.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptImage {
    /// "image/jpeg" or "image/png".
    pub mime: String,
    pub data: Vec<u8>,
}

impl PromptImage {
    pub fn base64(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&self.data)
    }
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.base64())
    }
}

impl std::fmt::Debug for PromptImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PromptImage({}, {} bytes)", self.mime, self.data.len())
    }
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
