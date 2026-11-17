//! `ssc-stt`: streaming speech-to-text behind `ssc_core::traits::SttEngine`.
//!
//! - [`deepgram::DeepgramStt`]: cloud, Deepgram live WebSocket with interim results.
//! - `whisper::WhisperStt` (feature `local-stt`): local whisper.cpp via `whisper-rs`.
//!
//! Both engines split the input per speaker ([`demux`]), so "me" and "them" never share a VAD,
//! buffer or connection, and both run the `ssc_audio::vad::Endpointer` so `ends_utterance` /
//! `t1_ms = speech_end` mean the same thing regardless of provider. Audio is only held in
//! memory; no audio is ever written. [`models`] fetches the whisper model file once.

pub mod deepgram;
pub mod demux;
pub mod models;
pub mod timeline;
#[cfg(feature = "local-stt")]
pub mod whisper;

pub use deepgram::{DeepgramConfig, DeepgramStt};
#[cfg(feature = "local-stt")]
pub use whisper::{WhisperConfig, WhisperStt};
