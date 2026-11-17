//! Local fake of Deepgram's live WebSocket API. No real network, no real key.
//!
//! Behaviour per connection, driven by a JSONL fixture of Deepgram messages:
//! - `Results` with `from_finalize: false` are sent once the audio received covers
//!   `start + duration` seconds (like the real service, which can't transcribe audio it hasn't got);
//! - `Results` with `from_finalize: true` are sent when the client sends `{"type":"Finalize"}`
//!   (unless `answer_finalize` is false);
//! - `Metadata` is sent after `{"type":"CloseStream"}`, then the socket closes.
#![allow(dead_code, clippy::result_large_err)]

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

#[derive(Debug, Default, Clone)]
pub struct ConnLog {
    pub uri: String,
    pub auth: Option<String>,
    pub audio_bytes: usize,
    pub samples: Vec<i16>,
    pub control: Vec<String>,
}

#[derive(Clone)]
pub struct FakeDeepgram {
    pub url: String,
    pub conns: Arc<Mutex<Vec<Arc<Mutex<ConnLog>>>>>,
}

pub enum Script {
    /// Replay this fixture on every connection.
    Fixture { lines: Vec<String>, answer_finalize: bool },
    /// Reply once with a Results message whose transcript is the sign of the first sample:
    /// lets a test prove which audio went to which connection.
    EchoSign,
}

pub fn load_fixture(name: &str) -> Vec<String> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap().lines().filter(|l| !l.trim().is_empty()).map(String::from).collect()
}

pub async fn start(script: Script) -> FakeDeepgram {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let conns: Arc<Mutex<Vec<Arc<Mutex<ConnLog>>>>> = Arc::default();
    let script = Arc::new(script);
    let c2 = conns.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let log = Arc::new(Mutex::new(ConnLog::default()));
            c2.lock().unwrap().push(log.clone());
            let script = script.clone();
            tokio::spawn(async move {
                let l2 = log.clone();
                let cb = move |req: &Request, resp: Response| {
                    let mut l = l2.lock().unwrap();
                    l.uri = req.uri().to_string();
                    l.auth = req.headers().get("authorization").map(|v| v.to_str().unwrap().to_string());
                    Ok(resp)
                };
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, cb).await else { return };
                serve(ws, log, &script).await;
            });
        }
    });
    FakeDeepgram { url: format!("ws://{addr}/v1/listen"), conns }
}

async fn serve(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    log: Arc<Mutex<ConnLog>>,
    script: &Script,
) {
    let (mut tx, mut rx) = ws.split();
    let mut queued: Vec<(f64, String)> = Vec::new();
    let mut on_finalize: Vec<String> = Vec::new();
    let mut on_close: Vec<String> = Vec::new();
    let mut answer = true;
    if let Script::Fixture { lines, answer_finalize } = script {
        answer = *answer_finalize;
        for l in lines {
            let v: Value = serde_json::from_str(l).unwrap();
            match v["type"].as_str() {
                Some("Results") if v["from_finalize"].as_bool() == Some(true) => on_finalize.push(l.clone()),
                Some("Results") => {
                    let end = v["start"].as_f64().unwrap() + v["duration"].as_f64().unwrap();
                    queued.push((end, l.clone()));
                }
                _ => on_close.push(l.clone()),
            }
        }
    }
    let mut echoed = false;
    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Binary(b) => {
                let secs = {
                    let mut l = log.lock().unwrap();
                    l.audio_bytes += b.len();
                    l.samples.extend(b.chunks_exact(2).map(|p| i16::from_le_bytes([p[0], p[1]])));
                    l.audio_bytes as f64 / 32_000.0
                };
                while let Some(pos) = queued.iter().position(|(end, _)| *end <= secs) {
                    let (_, line) = queued.remove(pos);
                    tx.send(Message::text(line)).await.unwrap();
                }
                if matches!(script, Script::EchoSign) && !echoed {
                    echoed = true;
                    let first = i16::from_le_bytes([b[0], b[1]]);
                    let word = if first >= 0 { "positive" } else { "negative" };
                    let m = format!(
                        r#"{{"type":"Results","start":0.0,"duration":0.02,"is_final":false,"channel":{{"alternatives":[{{"transcript":"{word}"}}]}}}}"#
                    );
                    tx.send(Message::text(m)).await.unwrap();
                }
            }
            Message::Text(t) => {
                let t = t.to_string();
                log.lock().unwrap().control.push(t.clone());
                if t.contains("Finalize") && answer {
                    for l in on_finalize.drain(..) {
                        tx.send(Message::text(l)).await.unwrap();
                    }
                }
                if t.contains("CloseStream") {
                    for l in on_close.drain(..) {
                        let _ = tx.send(Message::text(l)).await;
                    }
                    let _ = tx.send(Message::Close(None)).await;
                    return;
                }
            }
            _ => {}
        }
    }
}
