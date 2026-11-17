//! `ssc-router`: model adapters and deterministic routing (Build Sheet §6 W1-MODELS).
//!
//! - [`providers`]: streaming answer adapters — Groq / OpenAI-compatible (SSE), Gemini (SSE),
//!   Ollama (NDJSON, `keep_alive`, stable prefix), Anthropic-style Messages (SSE).
//! - [`decision`]: the `/v1/systemone` decision adapter (Jev + Tev1 4B; wire shape UNVERIFIED, see
//!   [`decision::systemone`]) and the decision router (800 ms timeout, failover, network probe).
//! - [`answer`]: the answer router (failover chain, first-token deadline, budget, strong tier,
//!   redaction before cloud calls, local-only enforcement, fast cancellation).
//! - [`budget`]: Groq free-tier tracker. [`speculative`]: drafting on partial transcripts.
//! - [`grammar`]: the cue output grammar parser (`contracts/prompts/README.md`).
//! - [`factory`]: builds everything from `Config` + keychain; local-only builds no cloud client.
//!
//! Routing never reads model output to decide where data goes.

pub mod answer;
pub mod budget;
pub mod decision;
pub mod factory;
pub mod grammar;
pub mod http;
pub mod providers;
pub mod speculative;
pub mod wire;

pub use answer::{AnswerOutcome, AnswerRouter, Route, RoutedDelta};
pub use decision::{Decided, DecisionRouter, DecisionSource, SystemOneDecision};
pub use factory::{Models, RouterOptions, build};
pub use grammar::{CueStream, ParsedAnswer, parse_answer};
