//! System audio ("them") on macOS via ScreenCaptureKit (macOS 13+). Needs the Screen Recording
//! permission. Video is configured to the minimum (2×2 px, 1 fps) and discarded; this app's own
//! audio is excluded so answers read aloud (if ever) don't loop back.

use super::{Delivery, SystemAudioSource, captured_at, park_until_stopped, spawn_capture};
use crate::chunker::ChunkAssembler;
use crate::resample::{downmix_interleaved, downmix_planar};
use screencapturekit::prelude::*;
use ssc_core::protocol::Speaker;
use ssc_core::traits::{AudioSource, Clock, SystemClock};
use ssc_core::types::{AudioChunk, SAMPLE_RATE_HZ};
use ssc_core::{Result, SscError};
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

impl AudioSource for SystemAudioSource {
    fn start(&self, tx: mpsc::Sender<AudioChunk>, cancel: CancellationToken) -> Result<()> {
        let chunk_ms = self.chunk_ms;
        let stats = self.stats.clone();
        spawn_capture("ssc-system-audio", move |ready| {
            let handler = AudioHandler { chunk_ms, state: Mutex::new(None), tx: tx.clone(), stats };
            let stream = match open(handler) {
                Ok(s) => s,
                Err(e) => return ready(Err(e)),
            };
            ready(Ok(()));
            park_until_stopped(&cancel, &tx);
            let _ = stream.stop_capture();
        })
    }
}

fn unavailable(e: impl std::fmt::Display) -> SscError {
    SscError::Unavailable(format!("system audio (ScreenCaptureKit): {e}"))
}

fn open(handler: AudioHandler) -> Result<SCStream> {
    let content = SCShareableContent::get().map_err(unavailable)?;
    let display = content.displays().into_iter().next().ok_or_else(|| unavailable("no display"))?;
    let filter = SCContentFilter::create().with_display(&display).with_excluding_windows(&[]).build();
    let config = SCStreamConfiguration::new()
        .with_width(2)
        .with_height(2)
        .with_minimum_frame_interval(&CMTime::new(1, 1))
        .with_captures_audio(true)
        .with_excludes_current_process_audio(true)
        .with_sample_rate(SAMPLE_RATE_HZ as i32)
        .with_channel_count(1);
    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(NoVideo, SCStreamOutputType::Screen).ok_or_else(|| unavailable("add video handler"))?;
    stream.add_output_handler(handler, SCStreamOutputType::Audio).ok_or_else(|| unavailable("add audio handler"))?;
    stream.start_capture().map_err(unavailable)?;
    Ok(stream)
}

/// Registered so ScreenCaptureKit has a sink for the (tiny) video frames instead of logging
/// "stream output not found" for each one.
struct NoVideo;
impl SCStreamOutputTrait for NoVideo {
    fn did_output_sample_buffer(&self, _sample: CMSampleBuffer, _of_type: SCStreamOutputType) {}
}

struct AudioHandler {
    chunk_ms: u32,
    /// Built on the first buffer, from the format ScreenCaptureKit actually delivers.
    state: Mutex<Option<(u32, Delivery)>>,
    tx: mpsc::Sender<AudioChunk>,
    stats: super::CaptureStats,
}

impl SCStreamOutputTrait for AudioHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !matches!(of_type, SCStreamOutputType::Audio) {
            return;
        }
        let Some(fmt) = sample.format_description() else { return };
        let rate = fmt.audio_sample_rate().unwrap_or(SAMPLE_RATE_HZ as f64).round() as u32;
        let is_float = fmt.audio_is_float();
        let bits = fmt.audio_bits_per_channel().unwrap_or(32);
        let Some(list) = sample.audio_buffer_list() else { return };
        let buffers: Vec<(u32, Vec<f32>)> =
            list.iter().map(|b| (b.number_channels, decode_pcm(b.data(), is_float, bits))).collect();
        let mut mono = Vec::new();
        mix_buffers(&buffers, &mut mono);
        if mono.is_empty() {
            return;
        }
        let at = captured_at(SystemClock.now_ms(), mono.len(), rate);
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.as_ref().is_none_or(|(r, _)| *r != rate) {
            let asm = ChunkAssembler::new(Speaker::Them, rate, 1, self.chunk_ms);
            *state = Some((rate, Delivery::new(asm, self.tx.clone(), self.stats.clone())));
        }
        if let Some((_, d)) = state.as_mut() {
            d.deliver_mono(&mono, at);
        }
    }
}

/// Little-endian PCM bytes → f32 samples (f32, i16 or i32 sources).
pub(crate) fn decode_pcm(bytes: &[u8], is_float: bool, bits: u32) -> Vec<f32> {
    match (is_float, bits) {
        (true, 32) => bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect(),
        (false, 16) => bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect(),
        (false, 32) => bytes
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0)
            .collect(),
        _ => Vec::new(),
    }
}

/// One buffer with N channels = interleaved; several 1-channel buffers = planar.
pub(crate) fn mix_buffers(buffers: &[(u32, Vec<f32>)], out: &mut Vec<f32>) {
    match buffers {
        [] => {}
        [(ch, data)] => downmix_interleaved(data, *ch as usize, out),
        many => {
            let planes: Vec<&[f32]> = many.iter().map(|(_, d)| d.as_slice()).collect();
            downmix_planar(&planes, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_and_mix() {
        let f: Vec<u8> = [0.5f32, -0.5].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(decode_pcm(&f, true, 32), vec![0.5, -0.5]);
        let i: Vec<u8> = [16_384i16].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(decode_pcm(&i, false, 16), vec![0.5]);
        assert!(decode_pcm(&i, false, 24).is_empty());
        let mut out = Vec::new();
        mix_buffers(&[(1, vec![1.0, 0.0]), (1, vec![0.0, 0.0])], &mut out);
        assert_eq!(out, vec![0.5, 0.0]);
        out.clear();
        mix_buffers(&[(2, vec![1.0, 0.0, 0.5, 0.5])], &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
    }

    /// Manual: needs Screen Recording permission and something playing.
    #[test]
    #[ignore = "real device: run manually with audio playing"]
    fn captures_system_audio() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(1000);
            let cancel = CancellationToken::new();
            SystemAudioSource::default().start(tx, cancel.clone()).unwrap();
            let mut n = 0;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
            while let Ok(Some(c)) = tokio::time::timeout_at(deadline, rx.recv()).await {
                assert_eq!(c.speaker, Speaker::Them);
                n += c.samples.len();
            }
            cancel.cancel();
            assert!(n > 16_000, "only {n} samples in 3 s");
        });
    }
}
