//! Microphone ("me") via cpal: WASAPI on Windows, CoreAudio on macOS.

use super::{CaptureStats, Delivery, MicSource, captured_at, park_until_stopped, spawn_capture};
use crate::chunker::ChunkAssembler;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use ssc_core::protocol::Speaker;
use ssc_core::traits::{AudioSource, Clock, SystemClock};
use ssc_core::types::AudioChunk;
use ssc_core::{Result, SscError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

impl AudioSource for MicSource {
    fn start(&self, tx: mpsc::Sender<AudioChunk>, cancel: CancellationToken) -> Result<()> {
        let device_id = self.device_id.clone();
        let chunk_ms = self.chunk_ms;
        let stats = self.stats.clone();
        spawn_capture("ssc-mic", move |ready| {
            let stream = match open(device_id.as_deref(), chunk_ms, tx.clone(), stats) {
                Ok(s) => s,
                Err(e) => return ready(Err(e)),
            };
            if let Err(e) = stream.play() {
                return ready(Err(SscError::Unavailable(format!("mic: play: {e}"))));
            }
            ready(Ok(()));
            park_until_stopped(&cancel, &tx);
            drop(stream);
        })
    }
}

fn unavailable(e: impl std::fmt::Display) -> SscError {
    SscError::Unavailable(format!("mic: {e}"))
}

fn open(
    device_id: Option<&str>,
    chunk_ms: u32,
    tx: mpsc::Sender<AudioChunk>,
    stats: CaptureStats,
) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = match device_id {
        Some(id) => host.device_by_id(&id.parse().map_err(unavailable)?),
        None => host.default_input_device(),
    }
    .ok_or_else(|| unavailable("no input device"))?;
    let supported = device.default_input_config().map_err(unavailable)?;
    let rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let delivery = Delivery::new(ChunkAssembler::new(Speaker::Me, rate, channels, chunk_ms), tx, stats);
    match format {
        SampleFormat::F32 => build::<f32>(&device, config, delivery, rate, channels),
        SampleFormat::I16 => build::<i16>(&device, config, delivery, rate, channels),
        SampleFormat::I32 => build::<i32>(&device, config, delivery, rate, channels),
        SampleFormat::U16 => build::<u16>(&device, config, delivery, rate, channels),
        SampleFormat::I8 => build::<i8>(&device, config, delivery, rate, channels),
        other => Err(unavailable(format!("unsupported sample format {other}"))),
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut delivery: Delivery,
    rate: u32,
    channels: usize,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _info| {
                let at = captured_at(SystemClock.now_ms(), data.len() / channels.max(1), rate);
                scratch.clear();
                scratch.extend(data.iter().map(|&s| f32::from_sample_(s)));
                delivery.deliver_interleaved(&scratch, at);
            },
            // Stream errors (device unplugged, xrun) end up as silence on this source; the
            // pipeline keeps running on the other one.
            |_err| {},
            None,
        )
        .map_err(unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual: needs a microphone (and mic permission on macOS). Speak during the 3 s.
    #[test]
    #[ignore = "real device: run manually"]
    fn captures_mic() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(1000);
            let cancel = CancellationToken::new();
            MicSource::default().start(tx, cancel.clone()).unwrap();
            let mut n = 0;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
            while let Ok(Some(c)) = tokio::time::timeout_at(deadline, rx.recv()).await {
                assert_eq!(c.speaker, Speaker::Me);
                assert_eq!(c.samples.len(), 320);
                n += c.samples.len();
            }
            cancel.cancel();
            assert!(n > 40_000, "only {n} samples in 3 s");
        });
    }
}
