//! A tiny local HTTP/1.1 server for adapter tests: replays canned responses (status, headers,
//! body chunks with delays, chunked encoding) and records every request. No real network.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// (delay before this chunk, bytes)
    pub chunks: Vec<(Duration, Vec<u8>)>,
}

impl Reply {
    pub fn ok(content_type: &str, body: &[u8]) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), content_type.into())],
            chunks: vec![(Duration::ZERO, body.to_vec())],
        }
    }
    pub fn sse(body: &[u8]) -> Self {
        Self::ok("text/event-stream", body)
    }
    pub fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            chunks: vec![(Duration::ZERO, body.as_bytes().to_vec())],
        }
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
    /// Split the body into chunks separated by `delay`.
    pub fn chunked(content_type: &str, parts: &[&[u8]], first_delay: Duration, delay: Duration) -> Self {
        let chunks =
            parts.iter().enumerate().map(|(i, p)| (if i == 0 { first_delay } else { delay }, p.to_vec())).collect();
        Self { status: 200, headers: vec![("content-type".into(), content_type.into())], chunks }
    }
}

pub struct FakeServer {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
    /// Set when a write to the client failed (client hung up mid-stream).
    pub client_hung_up: Arc<Mutex<bool>>,
}

impl FakeServer {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

/// Serve `replies` in order (one per request); the last one repeats.
pub async fn serve(replies: Vec<Reply>) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let hung = Arc::new(Mutex::new(false));
    let queue = Arc::new(Mutex::new(VecDeque::from(replies)));
    let (r2, h2) = (requests.clone(), hung.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let reply = {
                let mut q = queue.lock().unwrap();
                if q.len() > 1 { q.pop_front().unwrap() } else { q.front().cloned().unwrap() }
            };
            let (r3, h3) = (r2.clone(), h2.clone());
            tokio::spawn(async move {
                let Some(req) = read_request(&mut sock).await else { return };
                r3.lock().unwrap().push(req);
                let mut head =
                    format!("HTTP/1.1 {} X\r\ntransfer-encoding: chunked\r\nconnection: close\r\n", reply.status);
                for (k, v) in &reply.headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                if sock.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let _ = sock.flush().await;
                for (delay, bytes) in &reply.chunks {
                    tokio::time::sleep(*delay).await;
                    let frame =
                        [format!("{:x}\r\n", bytes.len()).into_bytes(), bytes.clone(), b"\r\n".to_vec()].concat();
                    if sock.write_all(&frame).await.is_err() || sock.flush().await.is_err() {
                        *h3.lock().unwrap() = true;
                        return;
                    }
                }
                let _ = sock.write_all(b"0\r\n\r\n").await;
                let _ = sock.flush().await;
            });
        }
    });
    FakeServer { addr, requests, client_hung_up: hung }
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
    let first = lines.next()?;
    let mut parts = first.split(' ');
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Some(Recorded { method, path, headers, body })
}

impl Reply {
    /// Delay before the last chunk (a mid-stream stall).
    pub fn delay_last(mut self, d: Duration) -> Self {
        if let Some(last) = self.chunks.last_mut() {
            last.0 = d;
        }
        self
    }
}
