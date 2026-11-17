//! Deepgram live streaming adapter (WebSocket, interim results on).
//!
//! One connection per speaker. Audio goes up as binary linear16 frames; `Results` messages come
//! back as partial (`is_final: false`) and final segments. End of utterance is decided locally
//! by the VAD [`Endpointer`]: on `speech_end` the adapter sends `{"type":"Finalize"}` so Deepgram
//! flushes immediately, and the `from_finalize` result is emitted with `ends_utterance = true`
//! and `t1_ms = speech_end`. If Deepgram has nothing left to flush (its own endpointing already
//! sent the words), that result has empty text: it is still emitted, as an end-of-utterance
//! marker. If no flush result arrives within `finalize_timeout_ms`, the adapter emits the
//! marker itself so downstream triggering never stalls on the provider.
//!
//! The API key comes from the OS keychain ([`SecretKey::Deepgram`]) and is only ever placed in
//! the `Authorization` header.

use crate::demux::{WORKER_QUEUE, route_by_speaker};
use crate::timeline::Timeline;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use ssc_audio::vad::{Endpointer, EndpointerConfig, VadEvent};
use ssc_core::protocol::Speaker;
use ssc_core::secrets::{Secret, SecretKey, SecretStore};
use ssc_core::traits::SttEngine;
use ssc_core::types::{AudioChunk, SAMPLE_RATE_HZ, TranscriptSegment};
use ssc_core::{Result, SscError};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_URL: &str = "wss://api.deepgram.com/v1/listen";

#[derive(Debug, Clone, PartialEq)]
pub struct DeepgramConfig {
    /// `wss://api.deepgram.com/v1/listen`; tests point this at a local fake.
    pub url: String,
    pub model: String,
    pub language: String,
    pub smart_format: bool,
    pub endpointer: EndpointerConfig,
    /// Max wait for the `Finalize` response before emitting a bare end-of-utterance marker.
    pub finalize_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    /// Reconnect backoff: starts at `.0`, doubles up to `.1`.
    pub reconnect_backoff_ms: (u64, u64),
}

impl Default for DeepgramConfig {
    fn default() -> Self {
        Self {
            url: DEFAULT_URL.into(),
            model: "nova-3".into(), // verify against Deepgram's current streaming models
            language: "en".into(),
            smart_format: true,
            endpointer: EndpointerConfig::default(),
            finalize_timeout_ms: 600,
            connect_timeout_ms: 5_000,
            reconnect_backoff_ms: (250, 5_000),
        }
    }
}

impl DeepgramConfig {
    pub fn listen_url(&self) -> String {
        format!(
            "{}?model={}&language={}&encoding=linear16&sample_rate={}&channels=1&interim_results=true&smart_format={}&punctuate=true",
            self.url, self.model, self.language, SAMPLE_RATE_HZ, self.smart_format
        )
    }
}

pub struct DeepgramStt {
    cfg: DeepgramConfig,
    key: Secret,
}

impl DeepgramStt {
    pub fn new(cfg: DeepgramConfig, key: Secret) -> Self {
        Self { cfg, key }
    }

    /// Reads the key from the keychain. `NotFound` if it isn't set.
    pub fn from_store(store: &dyn SecretStore, cfg: DeepgramConfig) -> Result<Self> {
        let key = store.get(&SecretKey::Deepgram)?.ok_or(SscError::NotFound)?;
        Ok(Self::new(cfg, key))
    }
}

impl SttEngine for DeepgramStt {
    fn start(
        &self,
        rx: mpsc::Receiver<AudioChunk>,
        tx: mpsc::Sender<TranscriptSegment>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let cfg = self.cfg.clone();
        let key = self.key.clone();
        let c = cancel.clone();
        route_by_speaker(rx, cancel, move |speaker| {
            let (wtx, wrx) = mpsc::channel(WORKER_QUEUE);
            tokio::spawn(run_speaker(speaker, cfg.clone(), key.clone(), wrx, tx.clone(), c.clone()));
            wtx
        });
        Ok(())
    }
}

// ---------- wire format ----------

#[derive(Debug, Deserialize)]
struct DgMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    start: f64,
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    is_final: bool,
    #[serde(default)]
    from_finalize: bool,
    #[serde(default)]
    channel: Option<DgChannel>,
}

#[derive(Debug, Deserialize)]
struct DgChannel {
    #[serde(default)]
    alternatives: Vec<DgAlternative>,
}

#[derive(Debug, Deserialize)]
struct DgAlternative {
    #[serde(default)]
    transcript: String,
}

const FINALIZE: &str = r#"{"type":"Finalize"}"#;
const CLOSE_STREAM: &str = r#"{"type":"CloseStream"}"#;
const KEEPALIVE: &str = r#"{"type":"KeepAlive"}"#;
const KEEPALIVE_EVERY: Duration = Duration::from_secs(4);

/// Per-connection state that turns Deepgram messages into segments.
struct Session {
    speaker: Speaker,
    timeline: Timeline,
    /// `speech_end` times awaiting a `from_finalize` result, with their fallback deadline.
    pending_ends: VecDeque<(u64, Instant)>,
}

impl Session {
    fn new(speaker: Speaker) -> Self {
        Self { speaker, timeline: Timeline::new(), pending_ends: VecDeque::new() }
    }

    fn marker(&self, t_end: u64) -> TranscriptSegment {
        TranscriptSegment {
            speaker: self.speaker,
            text: String::new(),
            is_final: true,
            t0_ms: t_end,
            t1_ms: t_end,
            ends_utterance: true,
        }
    }

    /// Parse one text frame. Non-`Results` messages (Metadata, SpeechStarted, UtteranceEnd) and
    /// empty non-final results yield nothing.
    fn on_text(&mut self, text: &str) -> Option<TranscriptSegment> {
        let msg: DgMessage = serde_json::from_str(text).ok()?;
        if msg.kind != "Results" {
            return None;
        }
        let transcript = msg
            .channel
            .as_ref()
            .and_then(|c| c.alternatives.first())
            .map(|a| a.transcript.trim().to_string())
            .unwrap_or_default();
        let t0_ms = self.timeline.to_unix_ms(msg.start);
        let t1_ms = self.timeline.to_unix_ms(msg.start + msg.duration);
        if msg.is_final
            && msg.from_finalize
            && let Some((t_end, _)) = self.pending_ends.pop_front()
        {
            return Some(TranscriptSegment {
                speaker: self.speaker,
                text: transcript,
                is_final: true,
                t0_ms: t0_ms.min(t_end),
                t1_ms: t_end,
                ends_utterance: true,
            });
        }
        if transcript.is_empty() {
            return None;
        }
        Some(TranscriptSegment {
            speaker: self.speaker,
            text: transcript,
            is_final: msg.is_final,
            t0_ms,
            t1_ms,
            ends_utterance: false,
        })
    }
}

fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(cfg: &DeepgramConfig, key: &Secret) -> Result<Ws> {
    let mut req =
        cfg.listen_url().into_client_request().map_err(|e| SscError::Invalid(format!("deepgram url: {e}")))?;
    let auth = HeaderValue::from_str(&format!("Token {}", key.expose()))
        .map_err(|_| SscError::Invalid("deepgram key has invalid characters".into()))?;
    req.headers_mut().insert("Authorization", auth);
    let fut = tokio_tungstenite::connect_async(req);
    match tokio::time::timeout(Duration::from_millis(cfg.connect_timeout_ms), fut).await {
        Err(_) => Err(SscError::Timeout(cfg.connect_timeout_ms)),
        // The error text never contains the key (it's only in our request headers).
        Ok(Err(e)) => Err(SscError::Unavailable(format!("deepgram connect: {e}"))),
        Ok(Ok((ws, _resp))) => Ok(ws),
    }
}

enum SessionEnd {
    /// Cancelled or input closed: stop the worker.
    Done,
    /// Connection lost: reconnect.
    Lost,
}

async fn run_speaker(
    speaker: Speaker,
    cfg: DeepgramConfig,
    key: Secret,
    mut audio: mpsc::Receiver<AudioChunk>,
    tx: mpsc::Sender<TranscriptSegment>,
    cancel: CancellationToken,
) {
    let mut backoff = cfg.reconnect_backoff_ms.0;
    loop {
        let ws = tokio::select! {
            _ = cancel.cancelled() => return,
            r = connect(&cfg, &key) => r,
        };
        let ws = match ws {
            Ok(ws) => {
                backoff = cfg.reconnect_backoff_ms.0;
                ws
            }
            Err(_) => {
                // Drop audio while disconnected; nothing is buffered or written anywhere.
                let until = Instant::now() + Duration::from_millis(backoff);
                backoff = (backoff * 2).min(cfg.reconnect_backoff_ms.1);
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep_until(until) => break,
                        c = audio.recv() => if c.is_none() { return },
                    }
                }
                continue;
            }
        };
        match run_session(speaker, &cfg, ws, &mut audio, &tx, &cancel).await {
            SessionEnd::Done => return,
            SessionEnd::Lost => continue,
        }
    }
}

async fn run_session(
    speaker: Speaker,
    cfg: &DeepgramConfig,
    ws: Ws,
    audio: &mut mpsc::Receiver<AudioChunk>,
    tx: &mpsc::Sender<TranscriptSegment>,
    cancel: &CancellationToken,
) -> SessionEnd {
    let (mut sink, mut stream) = ws.split();
    let mut session = Session::new(speaker);
    let mut vad = Endpointer::new(cfg.endpointer.clone());
    let finalize_after = Duration::from_millis(cfg.finalize_timeout_ms);
    let mut input_open = true;
    // After CloseStream, wait this long for trailing results before giving up.
    let mut closing_deadline: Option<Instant> = None;
    let idle_after = Duration::from_millis(cfg.endpointer.silence_ms as u64);
    let mut last_audio = Instant::now();
    let mut last_sent = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let fallback = session.pending_ends.front().map(|&(_, d)| d);
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = sink.send(Message::text(CLOSE_STREAM)).await;
                return SessionEnd::Done;
            }
            c = audio.recv(), if input_open => match c {
                Some(chunk) => {
                    if chunk.speaker != speaker {
                        continue; // defensive: the demux never does this
                    }
                    session.timeline.push(chunk.t0_ms, chunk.samples.len());
                    if sink.send(Message::binary(pcm_bytes(&chunk.samples))).await.is_err() {
                        return SessionEnd::Lost;
                    }
                    last_audio = Instant::now();
                    last_sent = last_audio;
                    for ev in vad.push(&chunk.samples, chunk.t0_ms) {
                        if let VadEvent::SpeechEnd { t_ms, .. } = ev {
                            session.pending_ends.push_back((t_ms, Instant::now() + finalize_after));
                            if sink.send(Message::text(FINALIZE)).await.is_err() {
                                return SessionEnd::Lost;
                            }
                        }
                    }
                }
                None => {
                    input_open = false;
                    if let Some(VadEvent::SpeechEnd { t_ms, .. }) = vad.flush() {
                        session.pending_ends.push_back((t_ms, Instant::now() + finalize_after));
                        let _ = sink.send(Message::text(FINALIZE)).await;
                    }
                    let _ = sink.send(Message::text(CLOSE_STREAM)).await;
                    closing_deadline = Some(Instant::now() + finalize_after.max(Duration::from_secs(2)));
                }
            },
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    if let Some(seg) = session.on_text(t.as_str())
                        && tx.send(seg).await.is_err()
                    {
                        return SessionEnd::Done;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                    flush_markers(&mut session, tx).await;
                    return if input_open { SessionEnd::Lost } else { SessionEnd::Done };
                }
                Some(Ok(_)) => {}
            },
            _ = tick.tick(), if input_open => {
                // Sources may stop delivering packets during silence (WASAPI loopback does):
                // treat an input gap as silence so an open utterance still ends.
                if vad.in_speech() && last_audio.elapsed() >= idle_after
                    && let Some(VadEvent::SpeechEnd { t_ms, .. }) = vad.flush()
                {
                    session.pending_ends.push_back((t_ms, Instant::now() + finalize_after));
                    if sink.send(Message::text(FINALIZE)).await.is_err() {
                        return SessionEnd::Lost;
                    }
                    last_sent = Instant::now();
                }
                // Deepgram closes a stream after ~10 s without data.
                if last_sent.elapsed() >= KEEPALIVE_EVERY {
                    if sink.send(Message::text(KEEPALIVE)).await.is_err() {
                        return SessionEnd::Lost;
                    }
                    last_sent = Instant::now();
                }
            }
            _ = sleep_until_opt(fallback) => {
                if let Some((t_end, _)) = session.pending_ends.pop_front()
                    && tx.send(session.marker(t_end)).await.is_err()
                {
                    return SessionEnd::Done;
                }
            }
            _ = sleep_until_opt(closing_deadline) => {
                flush_markers(&mut session, tx).await;
                return SessionEnd::Done;
            }
        }
    }
}

/// Emit markers for every utterance end still waiting on the provider.
async fn flush_markers(session: &mut Session, tx: &mpsc::Sender<TranscriptSegment>) {
    while let Some((t_end, _)) = session.pending_ends.pop_front() {
        let _ = tx.send(session.marker(t_end)).await;
    }
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        let mut s = Session::new(Speaker::Them);
        s.timeline.push(1_000_000, 16_000 * 5);
        s
    }

    #[test]
    fn interim_and_final_mapping() {
        let mut s = session();
        let interim = r#"{"type":"Results","start":0.5,"duration":1.0,"is_final":false,"speech_final":false,
            "channel":{"alternatives":[{"transcript":"what is","confidence":0.9}]}}"#;
        let seg = s.on_text(interim).unwrap();
        assert_eq!((seg.t0_ms, seg.t1_ms, seg.is_final, seg.ends_utterance), (1_000_500, 1_001_500, false, false));
        assert_eq!(seg.speaker, Speaker::Them);
        assert_eq!(seg.text, "what is");
        // Empty interim → nothing.
        let empty = r#"{"type":"Results","start":0,"duration":0.1,"is_final":false,"channel":{"alternatives":[{"transcript":""}]}}"#;
        assert!(s.on_text(empty).is_none());
        assert!(s.on_text(r#"{"type":"Metadata","request_id":"x"}"#).is_none());
        assert!(s.on_text("not json").is_none());
    }

    #[test]
    fn finalize_result_ends_utterance_at_vad_time() {
        let mut s = session();
        s.pending_ends.push_back((1_001_800, Instant::now()));
        let fin = r#"{"type":"Results","start":0.5,"duration":1.6,"is_final":true,"speech_final":false,"from_finalize":true,
            "channel":{"alternatives":[{"transcript":"What is your pricing?"}]}}"#;
        let seg = s.on_text(fin).unwrap();
        assert!(seg.is_final && seg.ends_utterance);
        assert_eq!((seg.t0_ms, seg.t1_ms), (1_000_500, 1_001_800));
        // Empty finalize result is still an end marker; without a pending end it's dropped.
        s.pending_ends.push_back((1_003_000, Instant::now()));
        let empty_fin = r#"{"type":"Results","start":2.1,"duration":0.9,"is_final":true,"from_finalize":true,"channel":{"alternatives":[{"transcript":""}]}}"#;
        let m = s.on_text(empty_fin).unwrap();
        assert!(m.ends_utterance && m.text.is_empty() && m.t1_ms == 1_003_000);
        assert!(s.on_text(empty_fin).is_none());
    }

    #[test]
    fn url_has_streaming_params_and_no_key() {
        let u = DeepgramConfig::default().listen_url();
        assert!(u.starts_with("wss://api.deepgram.com/v1/listen?"));
        for p in ["encoding=linear16", "sample_rate=16000", "channels=1", "interim_results=true"] {
            assert!(u.contains(p), "{u}");
        }
        assert!(!u.contains("token"));
    }

    #[test]
    fn from_store_requires_key() {
        let st = ssc_core::secrets::MemorySecretStore::default();
        assert!(matches!(DeepgramStt::from_store(&st, DeepgramConfig::default()), Err(SscError::NotFound)));
        st.set(&SecretKey::Deepgram, &Secret::new("k")).unwrap();
        assert!(DeepgramStt::from_store(&st, DeepgramConfig::default()).is_ok());
    }

    #[test]
    fn pcm_is_little_endian() {
        assert_eq!(pcm_bytes(&[1, -2]), vec![1, 0, 0xFE, 0xFF]);
    }
}
