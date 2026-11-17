//! Loopback fakes for fixture mode: an HTTP/1.1 server replaying recorded responses (chunked, with
//! per-chunk delays) and a Deepgram live WebSocket replaying a recorded session. They bind to
//! 127.0.0.1 only, so fixture mode never touches the network.

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

// ---------- HTTP ----------

#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    /// (delay before this chunk, bytes)
    pub chunks: Vec<(Duration, Vec<u8>)>,
}

impl Reply {
    /// A body split into parts: `first_delay` before the first part, `gap` before each later one.
    pub fn stream(content_type: &str, parts: Vec<Vec<u8>>, first_delay: Duration, gap: Duration) -> Self {
        let chunks = parts.into_iter().enumerate().map(|(i, p)| (if i == 0 { first_delay } else { gap }, p)).collect();
        Self { status: 200, content_type: content_type.into(), chunks }
    }

    pub fn json(body: &Value, delay: Duration) -> Self {
        Self {
            status: 200,
            content_type: "application/json".into(),
            chunks: vec![(delay, serde_json::to_vec(body).unwrap_or_default())],
        }
    }
}

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

pub struct FakeHttp {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeHttp {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

/// Serve `replies` in order, one per request; the last one repeats.
pub async fn http(replies: Vec<Reply>) -> std::io::Result<FakeHttp> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::new(Mutex::new(VecDeque::from(replies)));
    let r2 = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (r3, q2) = (r2.clone(), queue.clone());
            tokio::spawn(async move {
                // Keep-alive: serve every request on this connection.
                while let Some(req) = read_request(&mut sock).await {
                    let reply = {
                        let mut q = q2.lock().unwrap();
                        match q.len() {
                            0 => None,
                            1 => q.front().cloned(),
                            _ => q.pop_front(),
                        }
                    };
                    r3.lock().unwrap().push(req);
                    let Some(reply) = reply else { return };
                    if write_reply(&mut sock, &reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Ok(FakeHttp { addr, requests })
}

async fn write_reply(sock: &mut tokio::net::TcpStream, reply: &Reply) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} X\r\ntransfer-encoding: chunked\r\ncontent-type: {}\r\n\r\n",
        reply.status, reply.content_type
    );
    sock.write_all(head.as_bytes()).await?;
    sock.flush().await?;
    for (delay, bytes) in &reply.chunks {
        tokio::time::sleep(*delay).await;
        let frame = [format!("{:x}\r\n", bytes.len()).into_bytes(), bytes.clone(), b"\r\n".to_vec()].concat();
        sock.write_all(&frame).await?;
        sock.flush().await?;
    }
    sock.write_all(b"0\r\n\r\n").await?;
    sock.flush().await
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<Recorded> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let len: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Some(Recorded { method, path, body })
}

// ---------- Deepgram live WebSocket ----------

/// Replays a recorded Deepgram session (JSONL of server messages) on every connection:
/// non-finalize `Results` once the audio received covers `start + duration`; `from_finalize`
/// results when the client sends `Finalize`; everything else after `CloseStream`.
pub async fn deepgram(lines: Vec<String>) -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let lines = Arc::new(lines);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let lines = lines.clone();
            tokio::spawn(async move {
                if let Ok(ws) = tokio_tungstenite::accept_async(tcp).await {
                    serve_deepgram(ws, &lines).await;
                }
            });
        }
    });
    Ok(format!("ws://{addr}/v1/listen"))
}

async fn serve_deepgram(ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>, lines: &[String]) {
    let (mut tx, mut rx) = ws.split();
    let mut queued: Vec<(f64, String)> = Vec::new();
    let mut on_finalize: VecDeque<String> = VecDeque::new();
    let mut on_close: Vec<String> = Vec::new();
    for l in lines {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        match v["type"].as_str() {
            Some("Results") if v["from_finalize"].as_bool() == Some(true) => on_finalize.push_back(l.clone()),
            Some("Results") => {
                let end = v["start"].as_f64().unwrap_or(0.0) + v["duration"].as_f64().unwrap_or(0.0);
                queued.push((end, l.clone()));
            }
            _ => on_close.push(l.clone()),
        }
    }
    let mut bytes = 0usize;
    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Binary(b) => {
                bytes += b.len();
                let secs = bytes as f64 / 32_000.0;
                while let Some(pos) = queued.iter().position(|(end, _)| *end <= secs) {
                    let (_, line) = queued.remove(pos);
                    if tx.send(Message::text(line)).await.is_err() {
                        return;
                    }
                }
            }
            Message::Text(t) => {
                if t.contains("Finalize")
                    && let Some(l) = on_finalize.pop_front()
                    && tx.send(Message::text(l)).await.is_err()
                {
                    return;
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
