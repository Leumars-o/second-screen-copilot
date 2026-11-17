//! Acceptance: Deepgram adapter against a recorded-format WebSocket fixture, served by a local
//! fake (tests/common). No network, no real key.

mod common;

use common::{Script, load_fixture};
use ssc_audio::synth::{Segment, render};
use ssc_core::protocol::Speaker;
use ssc_core::secrets::Secret;
use ssc_core::traits::SttEngine;
use ssc_core::types::{AudioChunk, TranscriptSegment};
use ssc_stt::{DeepgramConfig, DeepgramStt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const BASE_MS: u64 = 1_763_290_000_000;

fn chunks(speaker: Speaker, samples: &[i16]) -> Vec<AudioChunk> {
    samples
        .chunks(320)
        .enumerate()
        .map(|(i, c)| AudioChunk { speaker, t0_ms: BASE_MS + i as u64 * 20, samples: c.to_vec() })
        .collect()
}

fn engine(url: &str, finalize_timeout_ms: u64) -> DeepgramStt {
    let cfg = DeepgramConfig { url: url.into(), finalize_timeout_ms, ..DeepgramConfig::default() };
    DeepgramStt::new(cfg, Secret::new("test-key-not-real"))
}

/// Run audio through the engine, close input, collect segments until the engine finishes.
async fn run(stt: &DeepgramStt, audio: Vec<AudioChunk>) -> Vec<TranscriptSegment> {
    let (atx, arx) = mpsc::channel(4096);
    let (stx, mut srx) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    stt.start(arx, stx, cancel.clone()).unwrap();
    for c in audio {
        atx.send(c).await.unwrap();
    }
    drop(atx);
    let mut out = Vec::new();
    while let Ok(Some(seg)) = tokio::time::timeout(Duration::from_secs(5), srx.recv()).await {
        out.push(seg);
    }
    cancel.cancel();
    out
}

#[tokio::test]
async fn interim_final_and_end_of_utterance() {
    let fake =
        common::start(Script::Fixture { lines: load_fixture("deepgram_session.jsonl"), answer_finalize: true }).await;
    let clip = render(&[Segment::Silence { ms: 500 }, Segment::Speech { ms: 1_500 }, Segment::Silence { ms: 1_000 }]);
    let segs = run(&engine(&fake.url, 2_000), chunks(Speaker::Them, &clip.samples)).await;

    let summary: Vec<(&str, bool, bool)> =
        segs.iter().map(|s| (s.text.as_str(), s.is_final, s.ends_utterance)).collect();
    assert_eq!(
        summary,
        vec![
            ("what is your", false, false),
            ("What is your", true, false),
            ("pricing", false, false),
            ("pricing?", true, true),
        ]
    );
    assert!(segs.iter().all(|s| s.speaker == Speaker::Them));
    // Provider-relative times are mapped back to capture time.
    assert_eq!((segs[0].t0_ms, segs[0].t1_ms), (BASE_MS + 500, BASE_MS + 1_200));
    // The final segment ends at the VAD's speech_end, within ±100 ms of the label (2000 ms).
    let end = segs[3].t1_ms - BASE_MS;
    assert!(end.abs_diff(clip.speech[0].1) <= 100, "speech_end at {end} ms");

    let conns = fake.conns.lock().unwrap();
    assert_eq!(conns.len(), 1);
    let c = conns[0].lock().unwrap();
    assert_eq!(c.auth.as_deref(), Some("Token test-key-not-real"));
    for p in ["encoding=linear16", "sample_rate=16000", "channels=1", "interim_results=true"] {
        assert!(c.uri.contains(p), "{}", c.uri);
    }
    assert!(!c.uri.contains("test-key"), "key must not be in the URL");
    assert_eq!(c.samples, clip.samples, "audio is sent unchanged as linear16");
    assert_eq!(c.control, vec![r#"{"type":"Finalize"}"#, r#"{"type":"CloseStream"}"#]);
}

#[tokio::test]
async fn end_marker_when_provider_does_not_answer_finalize() {
    let fake =
        common::start(Script::Fixture { lines: load_fixture("deepgram_session.jsonl"), answer_finalize: false }).await;
    let clip = render(&[Segment::Silence { ms: 500 }, Segment::Speech { ms: 1_500 }, Segment::Silence { ms: 1_000 }]);
    let segs = run(&engine(&fake.url, 150), chunks(Speaker::Me, &clip.samples)).await;
    let ends: Vec<&TranscriptSegment> = segs.iter().filter(|s| s.ends_utterance).collect();
    assert_eq!(ends.len(), 1, "{segs:?}");
    assert!(ends[0].text.is_empty() && ends[0].is_final && ends[0].speaker == Speaker::Me);
    assert!((ends[0].t1_ms - BASE_MS).abs_diff(clip.speech[0].1) <= 100);
}

#[tokio::test]
async fn two_speakers_use_separate_connections_and_never_mix() {
    let fake = common::start(Script::EchoSign).await;
    // "me" audio is all positive, "them" all negative; interleave them on one channel.
    let me = chunks(Speaker::Me, &vec![1_000i16; 320 * 20]);
    let them = chunks(Speaker::Them, &vec![-1_000i16; 320 * 20]);
    let audio: Vec<AudioChunk> = me.into_iter().zip(them).flat_map(|(a, b)| [a, b]).collect();
    let segs = run(&engine(&fake.url, 200), audio).await;
    assert_eq!(segs.len(), 2, "{segs:?}");
    for s in &segs {
        let want = if s.speaker == Speaker::Me { "positive" } else { "negative" };
        assert_eq!(s.text, want);
    }
    let conns = fake.conns.lock().unwrap();
    assert_eq!(conns.len(), 2);
    for c in conns.iter() {
        let c = c.lock().unwrap();
        assert_eq!(c.samples.len(), 320 * 20);
        assert!(c.samples.iter().all(|&s| s == c.samples[0]), "a connection received both speakers");
    }
}

#[tokio::test]
async fn reconnects_after_connection_failure() {
    // Nothing listening yet: the first attempts fail, then the server comes up on that port.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let url = format!("ws://127.0.0.1:{port}/v1/listen");
    let cfg = DeepgramConfig { url, reconnect_backoff_ms: (50, 100), ..DeepgramConfig::default() };
    let stt = DeepgramStt::new(cfg, Secret::new("k"));
    let (atx, arx) = mpsc::channel(64);
    let (stx, mut srx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    stt.start(arx, stx, cancel.clone()).unwrap();
    atx.send(AudioChunk { speaker: Speaker::Them, t0_ms: 0, samples: vec![-5; 320] }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        use futures_util::{SinkExt, StreamExt};
        while let Some(Ok(m)) = ws.next().await {
            if m.is_binary() {
                let r = r#"{"type":"Results","start":0,"duration":0.02,"is_final":true,"channel":{"alternatives":[{"transcript":"back"}]}}"#;
                ws.send(tokio_tungstenite::tungstenite::Message::text(r)).await.unwrap();
                break;
            }
        }
    });
    // Keep feeding audio until the reconnected session answers.
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        let mut t = 20;
        loop {
            let _ = atx.try_send(AudioChunk { speaker: Speaker::Them, t0_ms: t, samples: vec![-5; 320] });
            t += 20;
            if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(20), srx.recv()).await {
                return s;
            }
        }
    })
    .await
    .expect("no transcript after reconnect");
    assert_eq!(got.text, "back");
    cancel.cancel();
}

#[cfg(feature = "live")]
#[tokio::test]
async fn live_deepgram_smoke() {
    // Manual/live: needs a Deepgram key in the OS keychain. Never run by the gate.
    use ssc_core::secrets::{KeychainStore, SecretStore};
    let store = KeychainStore;
    let stt = DeepgramStt::from_store(&store as &dyn SecretStore, DeepgramConfig::default()).expect("no Deepgram key");
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../ssc-audio/tests/fixtures/question.wav");
    let samples: Vec<i16> = hound::WavReader::open(path).unwrap().samples::<i16>().map(|s| s.unwrap()).collect();
    let (atx, arx) = mpsc::channel(4096);
    let (stx, mut srx) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    stt.start(arx, stx, cancel.clone()).unwrap();
    for c in chunks(Speaker::Them, &samples) {
        atx.send(c).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await; // real time
    }
    drop(atx);
    let mut text = String::new();
    let mut ends = 0;
    while let Ok(Some(s)) = tokio::time::timeout(Duration::from_secs(5), srx.recv()).await {
        if s.is_final {
            text.push_str(&s.text);
            text.push(' ');
        }
        ends += s.ends_utterance as usize;
    }
    eprintln!("live transcript: {text}");
    assert!(text.to_lowercase().contains("pricing"));
    assert_eq!(ends, 2);
}
