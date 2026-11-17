//! System audio ("them") on Windows: WASAPI loopback on the default render device, shared
//! mode, event driven. Captured as f32 at the device mix rate, then downmixed/resampled by
//! [`ChunkAssembler`] like every other source.
//!
//! WASAPI delivers no packets while nothing is playing; the gap shows up as a timestamp jump
//! (the assembler re-anchors) and the STT side closes an open utterance on input idle.

use super::{Delivery, SystemAudioSource, captured_at, spawn_capture};
use crate::chunker::ChunkAssembler;
use ssc_core::protocol::Speaker;
use ssc_core::traits::{AudioSource, Clock, SystemClock};
use ssc_core::types::AudioChunk;
use ssc_core::{Result, SscError};
use std::collections::VecDeque;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

impl AudioSource for SystemAudioSource {
    fn start(&self, tx: mpsc::Sender<AudioChunk>, cancel: CancellationToken) -> Result<()> {
        let chunk_ms = self.chunk_ms;
        let stats = self.stats.clone();
        spawn_capture("ssc-system-audio", move |ready| {
            if let Err(e) = run(chunk_ms, tx, stats, cancel, ready) {
                ready(Err(e));
            }
        })
    }
}

fn unavailable(e: impl std::fmt::Display) -> SscError {
    SscError::Unavailable(format!("system audio (WASAPI loopback): {e}"))
}

fn run(
    chunk_ms: u32,
    tx: mpsc::Sender<AudioChunk>,
    stats: super::CaptureStats,
    cancel: CancellationToken,
    ready: &dyn Fn(Result<()>),
) -> Result<()> {
    let _ = initialize_mta();
    let enumerator = DeviceEnumerator::new().map_err(unavailable)?;
    let device = enumerator.get_default_device(Direction::Render).map_err(unavailable)?;
    let mut client = device.get_iaudioclient().map_err(unavailable)?;
    let mix = client.get_mixformat().map_err(unavailable)?;
    let rate = mix.get_samplespersec();
    let channels = mix.get_nchannels() as usize;
    let format = WaveFormat::new(32, 32, SampleType::Float, rate as usize, channels, None);
    let (_default_period, min_period) = client.get_device_period().map_err(unavailable)?;
    // Render device + Capture direction in shared mode = loopback.
    client
        .initialize_client(
            &format,
            Direction::Capture,
            StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: min_period },
            None,
        )
        .map_err(unavailable)?;
    let event = client.set_get_eventhandle().map_err(unavailable)?;
    let capture = client.get_audiocaptureclient().map_err(unavailable)?;
    client.start_stream().map_err(unavailable)?;
    ready(Ok(()));

    let mut delivery = Delivery::new(ChunkAssembler::new(Speaker::Them, rate, channels, chunk_ms), tx.clone(), stats);
    let mut bytes: VecDeque<u8> = VecDeque::new();
    let mut frame_f32: Vec<f32> = Vec::new();
    let block = 4 * channels;
    while !cancel.is_cancelled() && !tx.is_closed() {
        // Short timeout so cancellation is noticed promptly even when nothing is playing.
        if event.wait_for_event(100).is_err() {
            continue;
        }
        if capture.read_from_device_to_deque(&mut bytes).is_err() {
            break;
        }
        let whole = bytes.len() / block * block;
        if whole == 0 {
            continue;
        }
        frame_f32.clear();
        let (a, b) = bytes.as_slices();
        decode_f32(a, b, whole, &mut frame_f32);
        bytes.drain(..whole);
        let at = captured_at(SystemClock.now_ms(), frame_f32.len() / channels, rate);
        delivery.deliver_interleaved(&frame_f32, at);
    }
    let _ = client.stop_stream();
    Ok(())
}

/// Little-endian f32 from the first `n` bytes of a split deque.
fn decode_f32(a: &[u8], b: &[u8], n: usize, out: &mut Vec<f32>) {
    let mut it = a.iter().chain(b.iter()).take(n).copied();
    while let (Some(b0), Some(b1), Some(b2), Some(b3)) = (it.next(), it.next(), it.next(), it.next()) {
        out.push(f32::from_le_bytes([b0, b1, b2, b3]));
    }
}
