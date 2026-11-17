//! Test doubles: a phone that speaks the wire protocol with `ssc-crypto`, and a local fake relay
//! that pairs two sockets per `/r/<room>` and forwards binary frames as they are.

#![allow(dead_code)]

use futures_util::{SinkExt, StreamExt};
use ssc_core::protocol::{Envelope, Heartbeat, Message, PairHello, QrPayload};
use ssc_core::traits::{Clock, SystemClock};
use ssc_crypto::keys::pairing_proof;
use ssc_crypto::{Counters, Identity, PairedDevice, PairingOffer, Role, SecureChannel, SessionKeys, b64, b64_array};
use ssc_transport::{PcTransport, TransportConfig};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub const T: Duration = Duration::from_secs(5);

pub fn test_config() -> TransportConfig {
    TransportConfig {
        lan_bind: "127.0.0.1:0".parse().unwrap(),
        mdns: false,
        heartbeat_interval: Duration::from_secs(600),
        dead_after: Duration::from_secs(600),
        ..Default::default()
    }
}

/// A PC identity and a device already paired with `phone` (skips the QR flow).
pub fn prepaired() -> (Identity, Identity, PairedDevice) {
    let pc = Identity::generate();
    let phone = Identity::generate();
    let token = [77u8; 32];
    let mut offer = PairingOffer::with_token(token, u64::MAX);
    let hello = PairHello {
        device_name: "Test phone".into(),
        phone_pub: phone.public_b64(),
        proof: b64(&pairing_proof(&token, &phone.public_bytes())),
    };
    let dev = offer.accept(&pc, &hello, 0, 0).unwrap();
    (pc, phone, dev)
}

pub async fn start(cfg: TransportConfig, pc: Identity, dev: Option<PairedDevice>) -> PcTransport {
    PcTransport::start(cfg, pc, dev, Arc::new(SystemClock)).await.unwrap()
}

pub async fn ws_connect(url: &str) -> Ws {
    tokio_tungstenite::connect_async(url).await.unwrap().0
}

pub fn hello_bytes(phone: &Identity, token: &[u8; 32], name: &str) -> Vec<u8> {
    let msg = Message::PairHello(PairHello {
        device_name: name.into(),
        phone_pub: phone.public_b64(),
        proof: b64(&pairing_proof(token, &phone.public_bytes())),
    });
    plaintext_frame(&Envelope::new(0, SystemClock.now_ms(), None, msg).to_bytes())
}

/// CR-2: `0x00 || JSON`.
pub fn plaintext_frame(json: &[u8]) -> Vec<u8> {
    let mut f = vec![0x00];
    f.extend_from_slice(json);
    f
}

/// Phone side of the link. One channel (keys + seq + replay window) shared by both paths.
pub struct FakePhone {
    pub ch: SecureChannel,
    pub lan: Option<Ws>,
    pub relay: Option<Ws>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum P {
    Lan,
    Relay,
}

impl FakePhone {
    pub fn new(keys: &SessionKeys) -> Self {
        Self { ch: SecureChannel::new(Role::Phone, keys, Counters::default()), lan: None, relay: None }
    }

    pub fn from_device(dev: &PairedDevice) -> Self {
        Self::new(&dev.keys)
    }

    /// Scan `qr`, connect to `qr.urls[url_index]`, send `pair.hello`, expect the sealed
    /// confirmation heartbeat.
    pub async fn pair(qr: &QrPayload, url_index: usize, phone: &Identity) -> Self {
        let token: [u8; 32] = b64_array(&qr.token).unwrap();
        let pc_pub: [u8; 32] = b64_array(&qr.pc_pub).unwrap();
        let shared = phone.shared_secret(&pc_pub).unwrap();
        let mut me = Self::new(&SessionKeys::derive(&shared, &token));
        let mut ws = ws_connect(&qr.urls[url_index]).await;
        ws.send(WsMessage::Binary(hello_bytes(phone, &token, "Pixel").into())).await.unwrap();
        let p = if url_index == 0 { P::Lan } else { P::Relay };
        match p {
            P::Lan => me.lan = Some(ws),
            P::Relay => me.relay = Some(ws),
        }
        let env = me.recv_raw(p).await.expect("pairing confirmation");
        assert!(matches!(env.message, Message::Heartbeat(_)), "confirmation is a heartbeat");
        me
    }

    fn sock(&mut self, p: P) -> &mut Ws {
        match p {
            P::Lan => self.lan.as_mut().expect("lan socket"),
            P::Relay => self.relay.as_mut().expect("relay socket"),
        }
    }

    pub fn seal(&mut self, sid: Option<Uuid>, msg: Message) -> (Envelope, Vec<u8>) {
        self.ch.seal(SystemClock.now_ms(), sid, msg).unwrap()
    }

    pub async fn send_raw(&mut self, p: P, bytes: Vec<u8>) {
        self.sock(p).send(WsMessage::Binary(bytes.into())).await.unwrap();
    }

    pub async fn send(&mut self, p: P, sid: Option<Uuid>, msg: Message) -> Envelope {
        let (env, wire) = self.seal(sid, msg);
        self.send_raw(p, wire).await;
        env
    }

    /// Next envelope (heartbeats included). `None` if the socket closed. Panics after 5 s.
    pub async fn recv_raw(&mut self, p: P) -> Option<Envelope> {
        loop {
            let msg = tokio::time::timeout(T, self.sock(p).next()).await.expect("phone recv timed out");
            match msg {
                Some(Ok(WsMessage::Binary(b))) => return Some(self.ch.open(&b).expect("phone opens PC frame")),
                Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => return None,
                Some(Ok(_)) => continue,
            }
        }
    }

    /// Next non-heartbeat envelope; answers PC heartbeats like the real phone does.
    pub async fn recv(&mut self, p: P) -> Envelope {
        loop {
            let env = self.recv_raw(p).await.expect("socket closed");
            match env.message {
                Message::Heartbeat(Heartbeat { echo: None }) => {
                    self.send(p, None, Message::Heartbeat(Heartbeat { echo: Some(env.ts) })).await;
                }
                Message::Heartbeat(_) => {}
                _ => return env,
            }
        }
    }

    /// True if the socket gets closed by the peer within `within`.
    pub async fn closed_within(&mut self, p: P, within: Duration) -> bool {
        let ws = self.sock(p);
        tokio::time::timeout(within, async {
            loop {
                match ws.next().await {
                    Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(_)) => continue,
                }
            }
        })
        .await
        .is_ok()
    }
}

/// Wait until `f(status)` holds (polls every 5 ms, panics after 5 s).
pub async fn wait_for(t: &PcTransport, what: &str, f: impl Fn(&ssc_transport::LinkStatus) -> bool) {
    let deadline = tokio::time::Instant::now() + T;
    while !f(&t.status()) {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}: {:?}", t.status());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Local stand-in for the blind relay: pairs the first two sockets of each `/r/<room>` and
/// forwards binary frames unchanged. A third socket in a room is closed.
pub struct FakeRelay {
    pub addr: SocketAddr,
    pub forwarded: Arc<Mutex<u64>>,
}

type Rooms = Arc<Mutex<HashMap<String, Vec<(u64, mpsc::UnboundedSender<Vec<u8>>)>>>>;

impl FakeRelay {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let rooms: Rooms = Default::default();
        let forwarded = Arc::new(Mutex::new(0));
        let fwd = forwarded.clone();
        tokio::spawn(async move {
            let mut next_id = 0u64;
            loop {
                let Ok((tcp, _)) = listener.accept().await else { return };
                next_id += 1;
                tokio::spawn(relay_conn(tcp, next_id, rooms.clone(), fwd.clone()));
            }
        });
        Self { addr, forwarded }
    }

    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }
}

#[allow(clippy::result_large_err)]
async fn relay_conn(tcp: TcpStream, id: u64, rooms: Rooms, forwarded: Arc<Mutex<u64>>) {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    let path = Arc::new(Mutex::new(String::new()));
    let p2 = path.clone();
    let cb = move |req: &Request, resp: Response| {
        *p2.lock().unwrap() = req.uri().path().to_string();
        Ok(resp)
    };
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, cb).await else { return };
    let room = path.lock().unwrap().strip_prefix("/r/").unwrap_or("").to_string();
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let admitted = {
        let mut r = rooms.lock().unwrap();
        let members = r.entry(room.clone()).or_default();
        let ok = !room.is_empty() && members.len() < 2;
        if ok {
            members.push((id, tx));
        }
        ok
    };
    if !admitted {
        let _ = sink.close().await;
        return;
    }
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(b) => if sink.send(WsMessage::Binary(b.into())).await.is_err() { break },
                None => break,
            },
            msg = stream.next() => match msg {
                Some(Ok(WsMessage::Binary(b))) => {
                    let peer = rooms.lock().unwrap().get(&room).and_then(|m| m.iter().find(|(i, _)| *i != id).map(|(_, t)| t.clone()));
                    if let Some(peer) = peer {
                        *forwarded.lock().unwrap() += 1;
                        let _ = peer.send(b.to_vec());
                    }
                }
                Some(Ok(_)) => {}
                _ => break,
            },
        }
    }
    if let Some(m) = rooms.lock().unwrap().get_mut(&room) {
        m.retain(|(i, _)| *i != id);
    }
}
