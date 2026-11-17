//! WebSocket plumbing: one task per socket (LAN accepted or relay outbound) that forwards raw
//! frames to the core actor and writes what the core sends back. No crypto here.

use crate::core::{Cmd, ConnOut, LinkPath};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;

/// Max simultaneous LAN sockets (authenticated or not).
const MAX_LAN_CONNS: usize = 8;
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
                Some(ConnOut::Frame(bytes)) => {
                    if sink.send(WsMessage::Binary(bytes.into())).await.is_err() {
                        break;
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

pub(crate) async fn lan_server(
    listener: TcpListener,
    cmd_tx: mpsc::Sender<Cmd>,
    ids: Arc<AtomicU64>,
    max_frame: usize,
    cancel: CancellationToken,
) {
    let slots = Arc::new(Semaphore::new(MAX_LAN_CONNS));
    loop {
        let (tcp, _peer) = tokio::select! {
            _ = cancel.cancelled() => return,
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(_) => { tokio::time::sleep(Duration::from_millis(50)).await; continue; }
            },
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            drop(tcp); // full: refuse
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let (cmd_tx, cancel) = (cmd_tx.clone(), cancel.child_token());
        let id = ids.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let _permit = permit;
            let hs = tokio_tungstenite::accept_async_with_config(tcp, Some(ws_config(max_frame)));
            if let Ok(Ok(ws)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, hs).await {
                run_conn(ws, id, LinkPath::Lan, cmd_tx, cancel).await;
            }
        });
    }
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
