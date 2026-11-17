//! Real-device capture. Two independent sources, never mixed:
//! - [`MicSource`] → `Speaker::Me` (cpal; WASAPI on Windows, CoreAudio on macOS).
//! - [`SystemAudioSource`] → `Speaker::Them` (WASAPI loopback on Windows, ScreenCaptureKit on
//!   macOS).
//!
//! Each source owns a dedicated OS thread that keeps the native stream alive until the
//! `CancellationToken` fires or the receiver is dropped. Samples are downmixed, resampled to
//! 16 kHz and converted to PCM16 in memory, then handed to the channel with `try_send` (the
//! audio callback never blocks). Raw audio is never written to disk.

use crate::chunker::{ChunkAssembler, DEFAULT_CHUNK_MS};
use ssc_core::Result;
#[cfg(any(target_os = "macos", windows))]
use ssc_core::SscError;
use ssc_core::types::AudioChunk;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
#[cfg(any(target_os = "macos", windows))]
use tokio_util::sync::CancellationToken;

#[cfg(any(target_os = "macos", windows))]
mod mic_cpal;
#[cfg(target_os = "macos")]
mod sck;
#[cfg(windows)]
mod wasapi_loopback;

/// Microphone capture ("me").
#[derive(Debug, Clone)]
pub struct MicSource {
    /// cpal device id (as printed by `cpal::Device::id`); `None` = system default input.
    pub device_id: Option<String>,
    pub chunk_ms: u32,
    stats: CaptureStats,
}

/// System audio capture ("them"): everything the PC plays, excluding this app's own output
/// where the OS allows it (macOS).
#[derive(Debug, Clone)]
pub struct SystemAudioSource {
    pub chunk_ms: u32,
    stats: CaptureStats,
}

impl Default for MicSource {
    fn default() -> Self {
        Self { device_id: None, chunk_ms: DEFAULT_CHUNK_MS, stats: CaptureStats::default() }
    }
}

impl Default for SystemAudioSource {
    fn default() -> Self {
        Self { chunk_ms: DEFAULT_CHUNK_MS, stats: CaptureStats::default() }
    }
}

impl MicSource {
    pub fn stats(&self) -> &CaptureStats {
        &self.stats
    }
}

impl SystemAudioSource {
    pub fn stats(&self) -> &CaptureStats {
        &self.stats
    }
}

/// Counters shared with the capture thread.
#[derive(Debug, Clone, Default)]
pub struct CaptureStats {
    sent: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl CaptureStats {
    /// Chunks delivered to the channel.
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }
    /// Chunks dropped because the consumer fell behind.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Glue between a native callback and the channel: assemble chunks, `try_send` them.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
pub(crate) struct Delivery {
    assembler: ChunkAssembler,
    tx: mpsc::Sender<AudioChunk>,
    stats: CaptureStats,
}

#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
impl Delivery {
    pub(crate) fn new(assembler: ChunkAssembler, tx: mpsc::Sender<AudioChunk>, stats: CaptureStats) -> Self {
        Self { assembler, tx, stats }
    }

    pub(crate) fn deliver_interleaved(&mut self, interleaved: &[f32], captured_at_ms: u64) {
        let chunks = self.assembler.push_interleaved(interleaved, captured_at_ms);
        self.send(chunks);
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn deliver_mono(&mut self, mono: &[f32], captured_at_ms: u64) {
        let chunks = self.assembler.push_mono(mono, captured_at_ms);
        self.send(chunks);
    }

    fn send(&self, chunks: Vec<AudioChunk>) {
        for c in chunks {
            match self.tx.try_send(c) {
                Ok(()) => self.stats.sent.fetch_add(1, Ordering::Relaxed),
                Err(_) => self.stats.dropped.fetch_add(1, Ordering::Relaxed),
            };
        }
    }
}

/// Wall-clock capture time of the first frame of a buffer of `frames` frames at `rate` that
/// just arrived.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
pub(crate) fn captured_at(now_ms: u64, frames: usize, rate: u32) -> u64 {
    now_ms.saturating_sub(frames as u64 * 1000 / rate.max(1) as u64)
}

/// Run `body` on a named thread; `body` sets up the native stream, reports readiness through
/// the callback, then blocks until cancelled. `start()` waits for readiness so device errors
/// surface synchronously.
#[cfg(any(target_os = "macos", windows))]
pub(crate) fn spawn_capture<F>(name: &str, body: F) -> Result<()>
where
    F: FnOnce(&dyn Fn(Result<()>)) + Send + 'static,
{
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            body(&|r| {
                let _ = ready_tx.try_send(r);
            })
        })
        .map_err(|e| SscError::Unavailable(format!("{name}: spawn: {e}")))?;
    // Long enough for a first-run permission prompt to be answered.
    match ready_rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => r,
        Err(_) => Err(SscError::Timeout(30_000)),
    }
}

/// Block the capture thread until cancelled or the consumer is gone.
#[cfg(any(target_os = "macos", windows))]
pub(crate) fn park_until_stopped(cancel: &CancellationToken, tx: &mpsc::Sender<AudioChunk>) {
    while !cancel.is_cancelled() && !tx.is_closed() {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported {
    use super::*;
    use ssc_core::SscError;
    use ssc_core::traits::AudioSource;
    use tokio_util::sync::CancellationToken;

    impl AudioSource for MicSource {
        fn start(&self, _tx: mpsc::Sender<AudioChunk>, _cancel: CancellationToken) -> Result<()> {
            Err(SscError::Unavailable("mic capture is only implemented on macOS and Windows".into()))
        }
    }

    impl AudioSource for SystemAudioSource {
        fn start(&self, _tx: mpsc::Sender<AudioChunk>, _cancel: CancellationToken) -> Result<()> {
            Err(SscError::Unavailable("system audio capture is only implemented on macOS and Windows".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::protocol::Speaker;

    #[test]
    fn delivery_counts_drops_and_never_blocks() {
        let (tx, mut rx) = mpsc::channel(2);
        let stats = CaptureStats::default();
        let mut d = Delivery::new(ChunkAssembler::new(Speaker::Me, 16_000, 1, 20), tx, stats.clone());
        // 5 chunks into a channel of 2.
        d.deliver_mono(&vec![0.1; 320 * 5], 1_000);
        assert_eq!((stats.sent(), stats.dropped()), (2, 3));
        assert_eq!(rx.try_recv().unwrap().speaker, Speaker::Me);
    }

    #[test]
    fn captured_at_backdates_by_buffer_length() {
        assert_eq!(captured_at(10_000, 480, 48_000), 9_990);
        assert_eq!(captured_at(5, 48_000, 48_000), 0);
    }
}
