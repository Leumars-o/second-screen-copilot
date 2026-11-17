//! WebSocket plumbing: one task per socket (LAN accepted or relay outbound) that forwards raw
//! frames to the core actor and writes what the core sends back. No crypto here.

use crate::core::{Cmd, ConnOut, LinkPath};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;

/// Max simultaneous LAN sockets (authenticated or not).
const MAX_LAN_CONNS: usize = 8;
/// Newcomers waiting for an evicted socket's slot (beyond this, connections are refused).
const MAX_LAN_WAITERS: usize = 8;
/// How long a newcomer waits for an evicted socket's slot.
const EVICT_WAIT: Duration = Duration::from_secs(1);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-socket outgoing queue.
pub(crate) const CONN_QUEUE: usize = 1024;

pub(crate) fn ws_config(max_frame: usize) -> WebSocketConfig {
    WebSocketConfig::default().max_message_size(Some(max_frame)).max_frame_size(Some(max_frame))
}

pub(crate) async fn run_conn<S>(
    ws: WebSocketStream<S>,
    id: u64,
    path: LinkPath,
    cmd_tx: mpsc::Sender<Cmd>,
    cancel: CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::channel::<ConnOut>(CONN_QUEUE);
    if cmd_tx.send(Cmd::Opened { id, path, tx }).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = sink.close().await;
                break;
            }
            out = rx.recv() => match out {
                Some(ConnOut::Frame(bytes, note)) => {
                    if sink.send(WsMessage::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                    // Written and flushed to the socket: this is `frame_sent`.
                    if let Some(note) = note {
                        note.report();
                    }
                }
                Some(ConnOut::Close) | None => {
                    let _ = sink.close().await;
                    break;
                }
            },
            msg = stream.next() => match msg {
                Some(Ok(WsMessage::Binary(b))) => {
                    if cmd_tx.send(Cmd::Frame { id, data: b.to_vec() }).await.is_err() {
                        break;
                    }
                }
                // Text frames are not part of the protocol (CR-2: everything is binary).
                Some(Ok(WsMessage::Text(_) | WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_))) => {}
                Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
            },
        }
    }
    let _ = cmd_tx.send(Cmd::Closed { id }).await;
}

/// LAN server settings (CR-12).
pub(crate) struct LanLimits {
    pub max_frame: usize,
    /// TLS + WebSocket upgrade deadline (the same short budget as authentication).
    pub handshake: Duration,
    pub allowed_origins: Arc<Vec<String>>,
}

/// LAN accept loop. CR-12: browsers from another origin are refused at the upgrade; when all
/// slots are taken, the newcomer asks the core to evict the oldest not-yet-authenticated socket
/// and waits briefly for its slot (the paired phone authenticates within one round trip, so idle
/// squatters cannot lock it out); the core also closes sockets that stay unauthenticated past
/// `auth_deadline`.
pub(crate) async fn lan_server(
    listener: TcpListener,
    tls: Arc<RwLock<Option<TlsAcceptor>>>,
    cmd_tx: mpsc::Sender<Cmd>,
    ids: Arc<AtomicU64>,
    limits: LanLimits,
    cancel: CancellationToken,
) {
    let slots = Arc::new(Semaphore::new(MAX_LAN_CONNS));
    let waiters = Arc::new(Semaphore::new(MAX_LAN_WAITERS));
    let limits = Arc::new(limits);
    loop {
        let (tcp, _peer) = tokio::select! {
            _ = cancel.cancelled() => return,
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(_) => { tokio::time::sleep(Duration::from_millis(50)).await; continue; }
            },
        };
        let permit = match slots.clone().try_acquire_owned() {
            Ok(p) => Slot::Ready(p),
            Err(_) => match waiters.clone().try_acquire_owned() {
                Ok(w) => Slot::Wait(w),
                Err(_) => {
                    drop(tcp); // full and already evicting for others: refuse
                    continue;
                }
            },
        };
        let _ = tcp.set_nodelay(true);
        let (cmd_tx, cancel, slots, limits) = (cmd_tx.clone(), cancel.child_token(), slots.clone(), limits.clone());
        let id = ids.fetch_add(1, Ordering::Relaxed);
        // CR-4: plain or TLS is fixed per socket when it is accepted.
        let acceptor = tls.read().expect("lock").clone();
        tokio::spawn(async move {
            let _permit = match permit {
                Slot::Ready(p) => p,
                Slot::Wait(_waiting) => {
                    let _ = cmd_tx.send(Cmd::EvictUnauthed).await;
                    match tokio::time::timeout(EVICT_WAIT, slots.acquire_owned()).await {
                        Ok(Ok(p)) => p,
                        _ => return,
                    }
                }
            };
            let cfg = Some(ws_config(limits.max_frame));
            let allowed = limits.allowed_origins.clone();
            // The callback signature (and its large `Err`) is tungstenite's `Callback` trait.
            #[allow(clippy::result_large_err)]
            let check_origin = move |req: &Request, resp: Response| match req.headers().get("origin") {
                None => Ok(resp),
                Some(o) if o.to_str().is_ok_and(|o| crate::origin_allowed(o, &allowed)) => Ok(resp),
                Some(_) => {
                    let mut e = ErrorResponse::new(Some("origin not allowed".into()));
                    *e.status_mut() = StatusCode::FORBIDDEN;
                    Err(e)
                }
            };
            match acceptor {
                None => {
                    let hs = tokio_tungstenite::accept_hdr_async_with_config(tcp, check_origin, cfg);
                    if let Ok(Ok(ws)) = tokio::time::timeout(limits.handshake, hs).await {
                        run_conn(ws, id, LinkPath::Lan, cmd_tx, cancel).await;
                    }
                }
                Some(acceptor) => {
                    // TLS and the WebSocket upgrade share one deadline.
                    let hs = async move {
                        let stream = acceptor.accept(tcp).await.ok()?;
                        tokio_tungstenite::accept_hdr_async_with_config(stream, check_origin, cfg).await.ok()
                    };
                    if let Ok(Some(ws)) = tokio::time::timeout(limits.handshake, hs).await {
                        run_conn(ws, id, LinkPath::Lan, cmd_tx, cancel).await;
                    }
                }
            }
        });
    }
}

enum Slot {
    Ready(OwnedSemaphorePermit),
    /// No free slot: evict an unauthenticated socket and wait for its slot.
    Wait(#[allow(dead_code)] OwnedSemaphorePermit),
}

/// Keep one outbound socket to `<relay>/r/<room>` for the current room, reconnecting with
/// backoff. A room change drops the socket and reconnects to the new room.
pub(crate) async fn relay_client(
    base: String,
    mut room_rx: watch::Receiver<Option<String>>,
    cmd_tx: mpsc::Sender<Cmd>,
    ids: Arc<AtomicU64>,
    max_frame: usize,
    cancel: CancellationToken,
) {
    const MIN_BACKOFF: Duration = Duration::from_millis(250);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);
    let mut backoff = MIN_BACKOFF;
    loop {
        let room = room_rx.borrow_and_update().clone();
        let Some(room) = room else {
            tokio::select! {
                _ = cancel.cancelled() => return,
                r = room_rx.changed() => if r.is_err() { return } else { continue },
            }
        };
        let url = format!("{}/r/{}", base.trim_end_matches('/'), room);
        let started = tokio::time::Instant::now();
        let connect = tokio_tungstenite::connect_async_with_config(url, Some(ws_config(max_frame)), true);
        let result = tokio::select! {
            _ = cancel.cancelled() => return,
            r = tokio::time::timeout(HANDSHAKE_TIMEOUT * 2, connect) => r,
        };
        if let Ok(Ok((ws, _resp))) = result {
            let id = ids.fetch_add(1, Ordering::Relaxed);
            let conn_cancel = cancel.child_token();
            let conn = run_conn(ws, id, LinkPath::Relay, cmd_tx.clone(), conn_cancel.clone());
            tokio::pin!(conn);
            tokio::select! {
                _ = &mut conn => {}
                _ = room_rx.changed() => { conn_cancel.cancel(); conn.await; }
            }
            if cancel.is_cancelled() {
                return;
            }
            if started.elapsed() > Duration::from_secs(5) {
                backoff = MIN_BACKOFF; // it was a healthy connection; reconnect quickly
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
            r = room_rx.changed() => if r.is_err() { return },
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
