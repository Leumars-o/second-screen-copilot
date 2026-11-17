//! `ssc-audio`: capture "me" (mic) and "them" (system audio) as two separate 16 kHz mono PCM16
//! streams, plus the VAD that detects end of speech.
//!
//! - [`capture`]: [`MicSource`] and [`SystemAudioSource`] implement `ssc_core::traits::AudioSource`.
//! - [`resample`]: downmix + band-limited resampling to 16 kHz.
//! - [`chunker`]: native buffers → timestamped [`ssc_core::types::AudioChunk`]s.
//! - [`vad`]: WebRTC VAD + [`vad::Endpointer`] (`speech_end`).
//! - [`synth`]: deterministic labelled test audio.
//!
//! Raw audio only ever lives in memory; nothing in this crate writes files.

pub mod capture;
pub mod chunker;
pub mod resample;
pub mod synth;
pub mod vad;

pub use capture::{CaptureStats, MicSource, SystemAudioSource};
