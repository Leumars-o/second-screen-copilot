//! Acceptance: whisper adapter on a fixture WAV. Feature `local-stt` only (not in the gate).
//! Needs a ggml model: set `SSC_WHISPER_MODEL=/path/to/ggml-tiny.en.bin`
//! (https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin). The app itself
//! uses `ssc_stt::models::ensure_whisper`, which pins ggml-large-v3-turbo.
#![cfg(feature = "local-stt")]

use ssc_core::protocol::Speaker;
use ssc_core::traits::SttEngine;
use ssc_core::types::AudioChunk;
use ssc_stt::{WhisperConfig, WhisperStt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
async fn transcribes_fixture_wav_with_utterance_ends() {
    let model = std::env::var("SSC_WHISPER_MODEL").expect("set SSC_WHISPER_MODEL to a ggml whisper model file");
    let stt = tokio::task::spawn_blocking(move || WhisperStt::new(WhisperConfig::new(model))).await.unwrap().unwrap();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../ssc-audio/tests/fixtures/question.wav");
    let samples: Vec<i16> = hound::WavReader::open(path).unwrap().samples::<i16>().map(|s| s.unwrap()).collect();
    let (atx, arx) = mpsc::channel(4096);
    let (stx, mut srx) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    stt.start(arx, stx, cancel.clone()).unwrap();
    for (i, c) in samples.chunks(320).enumerate() {
        atx.send(AudioChunk { speaker: Speaker::Them, t0_ms: i as u64 * 20, samples: c.to_vec() }).await.unwrap();
    }
    drop(atx);
    let mut finals = Vec::new();
    let mut partials = 0;
    while let Ok(Some(s)) = tokio::time::timeout(Duration::from_secs(60), srx.recv()).await {
        assert_eq!(s.speaker, Speaker::Them);
        if s.is_final {
            finals.push(s);
        } else {
            partials += 1;
        }
    }
    cancel.cancel();
    eprintln!("finals: {finals:?}, partials: {partials}");
    assert_eq!(finals.len(), 2, "{finals:?}");
    assert!(finals.iter().all(|s| s.ends_utterance));
    assert!(finals[0].text.to_lowercase().contains("pricing"), "{}", finals[0].text);
    assert!(finals[1].text.to_lowercase().contains("onboarding"), "{}", finals[1].text);
    // Labels: 800–2840 ms and 4460–6410 ms.
    assert!(finals[0].t1_ms.abs_diff(2_840) <= 100 && finals[1].t1_ms.abs_diff(6_410) <= 100);
    assert!(partials >= 1);
}
