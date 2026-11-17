//! A Rust phone client for the real [`PcTransport`] on loopback (bench `latency --live`, M6).
//!
//! It pairs like the PWA (QR offer → `pair.hello` → sealed frames), answers the PC's heartbeat
//! probes with an echo (so the transport measures RTT), and on the first `answer.delta` of each
//! answer reports `feedback.timing` with a fixed paint time, like the PWA's first-paint report.
//! Everything else the phone receives is forwarded to [`LoopbackPhone::events`].

use futures_util::{SinkExt, StreamExt};
use ssc_core::protocol::{Command, Envelope, FeedbackItem, Heartbeat, Message, PairHello, PhoneStage};
use ssc_core::traits::{Clock, SystemClock};
use ssc_core::{Result, SscError};
use ssc_crypto::keys::pairing_proof;
use ssc_crypto::{Counters, Identity, Role, SecureChannel, SessionKeys, b64, b64_array};
use ssc_transport::PcTransport;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

pub struct LoopbackPhone {
    /// Every envelope the phone opened, in order.
    pub events: mpsc::UnboundedReceiver<Envelope>,
    /// Send a message from the phone (sealed on its channel).
    pub out: mpsc::UnboundedSender<(Option<Uuid>, Message)>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for LoopbackPhone {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn err(e: impl std::fmt::Display) -> SscError {
    SscError::Unavailable(format!("loopback phone: {e}"))
}

/// Pair a new phone with `pc` over its first QR URL and wait for the pairing confirmation.
pub async fn pair(pc: &PcTransport, paint_ms: u32) -> Result<LoopbackPhone> {
    let qr = pc.begin_pairing().await?;
    let url = qr.urls.first().ok_or_else(|| err("no pairing URL"))?.clone();
    let phone = Identity::generate();
    let token: [u8; 32] = b64_array(&qr.token).map_err(err)?;
    let pc_pub: [u8; 32] = b64_array(&qr.pc_pub).map_err(err)?;
    let shared = phone.shared_secret(&pc_pub).map_err(err)?;
    let keys = SessionKeys::derive(&shared, &token);
    let mut ch = SecureChannel::new(Role::Phone, &keys, Counters::default());
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str()).await.map_err(err)?;
    let hello = Message::PairHello(PairHello {
        device_name: "Loopback bench phone".into(),
        phone_pub: phone.public_b64(),
        proof: b64(&pairing_proof(&token, &phone.public_bytes())),
    });
    let mut frame = vec![0u8];
    frame.extend(Envelope::new(0, SystemClock.now_ms(), None, hello).to_bytes());
    ws.send(WsMessage::Binary(frame.into())).await.map_err(err)?;

    let (etx, mut events) = mpsc::unbounded_channel::<Envelope>();
    let (out, mut out_rx) = mpsc::unbounded_channel::<(Option<Uuid>, Message)>();
    let task = tokio::spawn(async move {
        let mut painted = HashSet::new();
        loop {
            tokio::select! {
                m = ws.next() => {
                    let b = match m {
                        Some(Ok(WsMessage::Binary(b))) => b,
                        Some(Ok(_)) => continue,
                        _ => break,
                    };
                    let Ok(env) = ch.open(&b) else { continue };
                    let reply = match &env.message {
                        // A PC probe: echo it so the transport can measure RTT.
                        Message::Heartbeat(Heartbeat { echo: None }) => {
                            Some(Message::Heartbeat(Heartbeat { echo: Some(env.ts) }))
                        }
                        Message::AnswerDelta(d) if painted.insert(d.answer_id) => {
                            Some(Message::Command(Command::Feedback {
                                answer_id: d.answer_id,
                                item: FeedbackItem::Timing { stage: PhoneStage::PhoneRendered, delta_ms: paint_ms },
                            }))
                        }
                        _ => None,
                    };
                    if let Some(r) = reply
                        && let Ok((_, wire)) = ch.seal(SystemClock.now_ms(), env.session_id, r)
                        && ws.send(WsMessage::Binary(wire.into())).await.is_err()
                    {
                        break;
                    }
                    let _ = etx.send(env);
                }
                o = out_rx.recv() => {
                    let Some((sid, msg)) = o else { break };
                    let Ok((_, wire)) = ch.seal(SystemClock.now_ms(), sid, msg) else { continue };
                    if ws.send(WsMessage::Binary(wire.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    // The PC confirms the pairing with its first sealed frame (a heartbeat).
    let confirmed = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(e) = events.recv().await {
            if matches!(e.message, Message::Heartbeat(_)) {
                return true;
            }
        }
        false
    })
    .await;
    if !matches!(confirmed, Ok(true)) {
        task.abort();
        return Err(err("pairing not confirmed"));
    }
    Ok(LoopbackPhone { events, out, task })
}
