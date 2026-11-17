//! `ssc-memory`: the local memory store (Build Sheet §6 W1-MEMORY, spec "Memory and context").
//!
//! - SQLite (bundled) with the `contracts/db` migrations; sqlite-vec holds every chunk embedding.
//! - Four layers exposed as protocol [`MemoryItem`](ssc_core::protocol::MemoryItem)s through
//!   [`MemoryStore`](ssc_core::traits::MemoryStore): About Me, profiles, contacts/accounts, sessions.
//! - Doc ingest (PDF, DOCX, Markdown, text) → ~400-token chunks with overlap → [`Embedder`](ssc_core::traits::Embedder).
//! - `search` is exact cosine over an in-memory per-profile index (see `index.rs` for why).
//! - Contact suggestions wait in `pending_contact_suggestions` until confirmed; transcripts are
//!   stored only when `keep_transcripts` is on. No raw audio or images are ever stored.
//!
//! The real embedder (bge-small-en-v1.5 via `fastembed`) is behind the `fastembed` feature. Its
//! files are fetched by [`download`] / [`models::ensure_embedder`] (pinned, SHA-256 checked);
//! `ssc-stt` uses the same downloader for the whisper model.

mod db;
pub mod download;
#[cfg(feature = "fastembed")]
pub mod embed;
mod index;
pub mod ingest;
pub mod items;
pub mod models;
mod session;
mod store;

pub use db::SCHEMA_VERSION;
pub use ingest::{ChunkConfig, DocKind};
pub use items::{ABOUT_ME_ID, SessionOutcome, profile_uuid};
pub use session::{AnswerRecord, PendingSuggestion};
pub use store::{DocInfo, EMBED_DIM, SqliteMemory};
