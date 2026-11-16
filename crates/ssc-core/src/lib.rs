//! `ssc-core`: the frozen contracts every other crate codes against (Build Sheet §4).
//!
//! - [`protocol`]: wire envelope + message types, validated against `contracts/protocol/*.schema.json`.
//! - [`decision`]: normalised decision-model result.
//! - [`types`]: data passed between components (screen, audio, transcript, prompts, memory).
//! - [`traits`]: component interfaces. [`mock`]: an implementation of each for tests.
//! - [`timing`]: per-stage timestamps and percentiles. [`config`]: settings. [`secrets`]: OS keychain.
//!
//! Workers must not edit this crate; file a change request in `contracts/CHANGE_REQUESTS.md`.

pub mod config;
pub mod decision;
pub mod error;
pub mod mock;
pub mod protocol;
pub mod secrets;
pub mod timing;
pub mod traits;
pub mod types;

pub use error::{Result, SscError};
