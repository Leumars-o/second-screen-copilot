//! W2-HARDEN security suite for the phone link and the CR-4 Cloudflare client (Build Sheet §7,
//! `docs/security/threat-model.md` §2–4). No real network: loopback sockets, a recording relay
//! and local fake HTTP servers only.

mod common;
mod fakes;

use common::*;
use fakes::*;
use futures_util::{SinkExt, StreamExt};
use ssc_core::config::Config;
use ssc_core::protocol::{AnswerDelta, Block, Command as Cmd, Message};
use ssc_core::secrets::{MemorySecretStore, Secret, SecretKey, SecretStore};
use ssc_core::traits::Transport;
use ssc_crypto::{Identity, SessionKeys, b64, b64_array};
use ssc_transport::lan_tls::cloudflare::CloudflareDns;
use ssc_transport::lan_tls::{AcmeDirectory, Propagation};
use ssc_transport::{LanTlsManager, LanTlsOptions, LanTlsStatus, TransportConfig};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

const MARKER_PC: &str = "MARKER-PC-ANSWER-45000-NAIRA";
const MARKER_PHONE: &str = "MARKER-PHONE-FOLLOWUP-SSO";

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

// ------------------------------------------------------------------ recording relay

/// Like `FakeRelay`, but keeps a copy of every frame it forwards: exactly what a curious relay
/// operator could see.
struct RecordingRelay {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
    rooms: Rooms,
}

type Rooms = Arc<Mutex<HashMap<String, Vec<(u64, mpsc::UnboundedSender<Vec<u8>>)>>>>;

impl RecordingRelay {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let rooms: Rooms = Default::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (s2, r2) = (seen.clone(), rooms.clone());
        tokio::spawn(async move {
            let mut id = 0;
            loop {
                let Ok((tcp, _)) = listener.accept().await else { return };
                id += 1;
                tokio::spawn(relay_conn(tcp, id, r2.clone(), s2.clone()));
            }
        });
        Self { addr, seen, rooms }
    }
    fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }
    fn frames(&self) -> Vec<Vec<u8>> {
        self.seen.lock().unwrap().clone()
    }
}

#[allow(clippy::result_large_err)]
async fn relay_conn(tcp: TcpStream, id: u64, rooms: Rooms, seen: Arc<Mutex<Vec<Vec<u8>>>>) {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    let path = Arc::new(Mutex::new(String::new()));
    let p2 = path.clone();
    let cb = move |req: &Request, resp: Response| {
        *p2.lock().unwrap() = req.uri().path().to_string();
        Ok(resp)
    };
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, cb).await else { return };
    let room = path.lock().unwrap().trim_start_matches("/r/").to_string();
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    rooms.lock().unwrap().entry(room.clone()).or_default().push((id, tx));
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(b) => if sink.send(WsMessage::Binary(b.into())).await.is_err() { break },
                None => break,
            },
            msg = stream.next() => match msg {
                Some(Ok(WsMessage::Binary(b))) => {
                    seen.lock().unwrap().push(b.to_vec());
                    let peers: Vec<_> = rooms.lock().unwrap().get(&room).map(|m| m.iter().filter(|(i, _)| *i != id).map(|(_, t)| t.clone()).collect()).unwrap_or_default();
                    for p in peers {
                        let _ = p.send(b.to_vec());
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

/// §7 "the relay sees ciphertext only": pair through the relay, then exchange an answer and a
/// command carrying recognisable text. Every frame the relay saw after the one plaintext
/// `pair.hello` is `0x01 || nonce || ct` with no readable content; the `pair.hello` itself holds
/// no token and no derived key (only the phone's public key, an HMAC proof and the device name,
/// CR-R1).
#[tokio::test]
async fn relay_sees_ciphertext_only() {
    let relay = RecordingRelay::start().await;
    let cfg = TransportConfig { relay_url: Some(relay.url()), ..test_config() };
    let t = start(cfg, Identity::generate(), None).await;
    let mut incoming = t.take_incoming().unwrap();
    let qr = t.begin_pairing().await.unwrap();
    // The PC's relay socket must be in the room before the phone speaks.
    let room = qr.urls[1].rsplit('/').next().unwrap().to_string();
    let deadline = tokio::time::Instant::now() + T;
    while relay.rooms.lock().unwrap().get(&room).is_none_or(|m| m.is_empty()) {
        assert!(tokio::time::Instant::now() < deadline, "PC never joined the relay room");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let phone = Identity::generate();
    let mut ph = FakePhone::pair(&qr, 1, &phone).await;

    let sid = Uuid::new_v4();
    let msg =
        Message::AnswerDelta(AnswerDelta { answer_id: Uuid::new_v4(), block: Block::Cue, text: MARKER_PC.into() });
    t.send(Some(sid), msg).await.unwrap();
    let got = ph.recv(P::Relay).await;
    assert!(matches!(got.message, Message::AnswerDelta(ref d) if d.text.contains(MARKER_PC)));
    ph.send(P::Relay, Some(sid), Message::Command(Cmd::Followup { answer_id: None, text: MARKER_PHONE.into() })).await;
    tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap();

    let frames = relay.frames();
    assert!(frames.len() >= 4, "expected hello, confirmation, answer, command; got {}", frames.len());
    let token: [u8; 32] = b64_array(&qr.token).unwrap();
    let shared = phone.shared_secret(&b64_array(&qr.pc_pub).unwrap()).unwrap();
    let keys = SessionKeys::derive(&shared, &token);
    let forbidden: Vec<Vec<u8>> = vec![
        MARKER_PC.into(),
        MARKER_PHONE.into(),
        qr.token.clone().into(),
        token.to_vec(),
        b64(&keys.pc_to_phone).into(),
        b64(&keys.phone_to_pc).into(),
        keys.pc_to_phone.to_vec(),
        keys.phone_to_pc.to_vec(),
        sid.to_string().into(),
    ];
    let mut plaintext_frames = 0;
    for f in &frames {
        for bad in &forbidden {
            assert!(!contains(f, bad), "relay saw {:?}", String::from_utf8_lossy(bad));
        }
        match f[0] {
            0x00 => {
                plaintext_frames += 1;
                let v: serde_json::Value = serde_json::from_slice(&f[1..]).unwrap();
                assert_eq!(v["type"], "pair.hello", "the only plaintext frame is pair.hello");
            }
            0x01 => {
                for needle in [&b"\"type\""[..], b"payload", b"answer.", b"command", b"heartbeat"] {
                    assert!(!contains(f, needle), "sealed frame shows {:?}", String::from_utf8_lossy(needle));
                }
            }
            v => panic!("unexpected frame version {v:#04x}"),
        }
    }
    assert_eq!(plaintext_frames, 1);
}

// ------------------------------------------------------------------ unpaired devices

/// A device that is not the paired phone (its own keys, well-formed sealed frames) gets nothing
/// delivered, is counted as dropped, and its LAN socket is closed after a few bad frames.
#[tokio::test]
async fn unpaired_device_frames_are_dropped_and_socket_closed() {
    let (pc, _phone, dev) = prepaired();
    let t = start(test_config(), pc, Some(dev)).await;
    let mut incoming = t.take_incoming().unwrap();
    let other = SessionKeys { pc_to_phone: [1; 32], phone_to_pc: [2; 32] };
    let mut stranger = FakePhone::new(&other);
    stranger.lan = Some(ws_connect(&t.lan_url()).await);
    for _ in 0..8 {
        let (_, w) = stranger.seal(None, Message::Command(Cmd::MemoryDelete { id: Uuid::new_v4() }));
        if stranger.lan.as_mut().unwrap().send(WsMessage::Binary(w.into())).await.is_err() {
            break;
        }
    }
    assert!(stranger.closed_within(P::Lan, T).await, "stranger socket must be closed");
    assert!(tokio::time::timeout(Duration::from_millis(200), incoming.recv()).await.is_err(), "nothing delivered");
    assert!(t.status().dropped_frames >= 8);
    assert!(t.status().paired, "the real pairing is untouched");
}

/// `pair.hello` while no QR offer is armed (an attacker on the LAN or in the relay room) is
/// refused and cannot replace the paired phone.
#[tokio::test]
async fn pair_hello_without_an_armed_offer_is_refused() {
    let (pc, _phone, dev) = prepaired();
    let t = start(test_config(), pc, Some(dev.clone())).await;
    let attacker = Identity::generate();
    let mut ws = ws_connect(&t.lan_url()).await;
    ws.send(WsMessage::Binary(hello_bytes(&attacker, &[77u8; 32], "evil").into())).await.unwrap();
    let closed = tokio::time::timeout(T, async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Binary(_))) => panic!("attacker got a sealed frame"),
                Some(Ok(_)) => continue,
                _ => return,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "socket closed");
    let snap = t.device_snapshot().await.unwrap();
    assert_eq!(snap.device_id, dev.device_id, "paired phone not replaced");
    // The genuine phone still works.
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    let mut incoming = t.take_incoming().unwrap();
    let e = ph.send(P::Lan, None, Message::Command(Cmd::Pin { answer_id: Uuid::new_v4() })).await;
    assert_eq!(tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap(), e);
}

/// With no paired phone, `send` refuses: nothing is ever encrypted to an unknown key or queued
/// for whoever pairs next.
#[tokio::test]
async fn nothing_is_sent_without_a_paired_phone() {
    let t = start(test_config(), Identity::generate(), None).await;
    let e = t.send(None, Message::Command(Cmd::Pin { answer_id: Uuid::nil() })).await.unwrap_err();
    assert!(matches!(e, ssc_core::SscError::Unavailable(_)), "{e}");
}

// ------------------------------------------------------------------ oversized frames

/// A frame over `max_frame` (64 KB, the relay's limit) closes the socket without being parsed or
/// delivered; `send` refuses to build one.
#[tokio::test]
async fn oversized_frames_are_refused_both_ways() {
    let (pc, _phone, dev) = prepaired();
    let t = start(test_config(), pc, Some(dev.clone())).await;
    let mut incoming = t.take_incoming().unwrap();
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    let mut big = vec![0x01u8; 64 * 1024 + 1];
    big[1] = 0x42;
    let _ = ph.lan.as_mut().unwrap().send(WsMessage::Binary(big.into())).await;
    assert!(ph.closed_within(P::Lan, T).await, "oversized frame closes the socket");
    assert!(tokio::time::timeout(Duration::from_millis(100), incoming.recv()).await.is_err());

    let huge = "x".repeat(70 * 1024);
    let e = t.send(None, Message::Command(Cmd::Followup { answer_id: None, text: huge })).await.unwrap_err();
    assert!(matches!(e, ssc_core::SscError::Invalid(_)), "{e}");
}

// ------------------------------------------------------------------ replay over the wire

/// On the wire: a tampered copy and a replay of a genuine frame are dropped; the genuine frame is
/// delivered exactly once.
#[tokio::test]
async fn wire_replay_and_tamper_are_not_delivered() {
    let (pc, _phone, dev) = prepaired();
    let t = start(test_config(), pc, Some(dev.clone())).await;
    let mut incoming = t.take_incoming().unwrap();
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    let (env, wire) = ph.seal(None, Message::Command(Cmd::MemoryDelete { id: Uuid::new_v4() }));
    let mut tampered = wire.clone();
    tampered[30] ^= 0x04;
    ph.send_raw(P::Lan, tampered).await;
    ph.send_raw(P::Lan, wire.clone()).await;
    ph.send_raw(P::Lan, wire.clone()).await;
    ph.send_raw(P::Lan, wire).await;
    assert_eq!(tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap(), env);
    assert!(tokio::time::timeout(Duration::from_millis(200), incoming.recv()).await.is_err(), "delivered once");
    wait_for(&t, "3 dropped", |s| s.dropped_frames == 3).await;
}

/// CR-11: the replay window is persisted at most once a second, so after a crash the PC restarts
/// with a stale window and accepts a recorded phone frame (e.g. `memory.delete`) a second time.
#[tokio::test]
async fn replay_window_is_persisted_before_frames_are_delivered() {
    let (pc, _phone, dev) = prepaired();
    let pc_secret = pc.to_secret();
    let t = start(test_config(), pc, Some(dev.clone())).await;
    let mut updates = t.take_device_updates().unwrap();
    let mut incoming = t.take_incoming().unwrap();
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    let mut latest = tokio::time::timeout(T, updates.recv()).await.unwrap().unwrap(); // initial reservation
    let (env, wire) = ph.seal(None, Message::Command(Cmd::MemoryDelete { id: Uuid::new_v4() }));
    ph.send_raw(P::Lan, wire.clone()).await;
    assert_eq!(tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap(), env);
    // Whatever the PC handed over for the keychain by now is what survives a crash.
    while let Ok(Some(d)) = tokio::time::timeout(Duration::from_millis(50), updates.recv()).await {
        latest = d;
    }
    t.shutdown();
    drop(t);

    let pc2 = Identity::from_secret_b64(&pc_secret).unwrap();
    let t2 = start(test_config(), pc2, Some(latest)).await;
    let mut incoming2 = t2.take_incoming().unwrap();
    let mut ws = ws_connect(&t2.lan_url()).await;
    ws.send(WsMessage::Binary(wire.into())).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), incoming2.recv()).await.is_err(),
        "a frame accepted before the crash was accepted again after restart"
    );
}

// ------------------------------------------------------------------ LAN DoS

/// CR-12: unauthenticated LAN sockets hold one of 8 slots until `dead_after` (15 s). Anyone on
/// the Wi-Fi can keep all 8 busy and lock the paired phone out of the LAN path.
#[tokio::test]
async fn idle_unauthenticated_sockets_cannot_lock_out_the_phone() {
    let (pc, _phone, dev) = prepaired();
    let cfg = TransportConfig { dead_after: Duration::from_secs(15), ..test_config() };
    let t = start(cfg, pc, Some(dev.clone())).await;
    let mut squatters = Vec::new();
    for _ in 0..8 {
        squatters.push(ws_connect(&t.lan_url()).await);
    }
    // The paired phone must still get through within a few seconds.
    let ok = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if let Ok(Ok((ws, _))) =
                tokio::time::timeout(Duration::from_secs(1), tokio_tungstenite::connect_async(t.lan_url())).await
            {
                return ws;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(ok.is_ok(), "paired phone locked out of the LAN by idle sockets");
    drop(squatters);
}

/// CR-12: the LAN server accepts WebSocket upgrades from any web origin. WebSockets are not
/// subject to CORS, so any page open in a browser on the PC or the Wi-Fi can connect to
/// `ws(s)://<lan>:47823` and hold the slots above (no LAN foothold needed, just a visited page).
#[tokio::test]
async fn cross_origin_browser_upgrades_are_refused() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (pc, _phone, dev) = prepaired();
    let t = start(test_config(), pc, Some(dev)).await;
    let mut req = t.lan_url().into_client_request().unwrap();
    req.headers_mut().insert("origin", "https://evil.example".parse().unwrap());
    let r = tokio::time::timeout(T, tokio_tungstenite::connect_async(req)).await.unwrap();
    assert!(r.is_err(), "upgrade from https://evil.example accepted");
}

/// CR-12: a LAN socket that never authenticates is closed after `auth_deadline`, without waiting
/// for `dead_after`.
#[tokio::test]
async fn unauthenticated_lan_socket_is_closed_after_the_auth_deadline() {
    let (pc, _phone, dev) = prepaired();
    let cfg = TransportConfig { auth_deadline: Duration::from_millis(300), ..test_config() };
    let t = start(cfg, pc, Some(dev.clone())).await;
    let mut idle = FakePhone::from_device(&dev);
    idle.lan = Some(ws_connect(&t.lan_url()).await);
    assert!(idle.closed_within(P::Lan, Duration::from_secs(2)).await, "idle socket still open");

    // An authenticated socket outlives the deadline.
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    let mut incoming = t.take_incoming().unwrap();
    ph.send(P::Lan, None, Message::Command(Cmd::Pin { answer_id: Uuid::new_v4() })).await;
    tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let e = ph.send(P::Lan, None, Message::Command(Cmd::Pin { answer_id: Uuid::new_v4() })).await;
    assert_eq!(tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap(), e);
}

/// CR-12: when the slots are full, newcomers evict unauthenticated sockets, never the phone's.
#[tokio::test]
async fn eviction_spares_the_authenticated_phone() {
    let (pc, _phone, dev) = prepaired();
    let t = start(test_config(), pc, Some(dev.clone())).await;
    let mut incoming = t.take_incoming().unwrap();
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    ph.send(P::Lan, None, Message::Command(Cmd::Pin { answer_id: Uuid::new_v4() })).await;
    tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap();
    let mut squatters = Vec::new();
    for _ in 0..20 {
        if let Ok(Ok((ws, _))) =
            tokio::time::timeout(Duration::from_secs(2), tokio_tungstenite::connect_async(t.lan_url())).await
        {
            squatters.push(ws);
        }
    }
    let e = ph.send(P::Lan, None, Message::Command(Cmd::Pin { answer_id: Uuid::new_v4() })).await;
    assert_eq!(tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap(), e, "phone socket evicted");
    drop(squatters);
}

/// CR-12: the PWA's own origin (configured), and native clients (no `Origin`), are accepted.
#[tokio::test]
async fn allowed_origin_and_native_clients_are_accepted() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (pc, _phone, dev) = prepaired();
    let cfg = TransportConfig { allowed_origins: vec!["https://*.example.com".into()], ..test_config() };
    let t = start(cfg, pc, Some(dev)).await;
    for (origin, ok) in [
        (Some("https://app.example.com"), true),
        (None, true),
        (Some("https://example.com.evil.test"), false),
        (Some("http://app.example.com"), false),
        (Some("null"), false),
    ] {
        let mut req = t.lan_url().into_client_request().unwrap();
        if let Some(o) = origin {
            req.headers_mut().insert("origin", o.parse().unwrap());
        }
        let r = tokio::time::timeout(T, tokio_tungstenite::connect_async(req)).await.unwrap();
        assert_eq!(r.is_ok(), ok, "{origin:?}");
    }
}

#[test]
fn pwa_origins_and_origin_matching() {
    use ssc_transport::{origin_allowed, pwa_origins};
    let cfg = Config {
        relay_url: Some("wss://relay.live-preview.space/".into()),
        lan_domain: Some("lan.live-preview.space".into()),
        ..Default::default()
    };
    let o = pwa_origins(&cfg);
    assert!(
        o.contains(&"https://live-preview.space".to_string())
            && o.contains(&"https://*.live-preview.space".to_string())
    );
    for (origin, ok) in [
        ("https://live-preview.space", true),
        ("https://app.live-preview.space", true),
        ("HTTPS://App.Live-Preview.Space/", true),
        ("https://evil-live-preview.space", false),
        ("https://live-preview.space.evil.test", false),
        ("http://live-preview.space", false),
        ("https://app.live-preview.space:8443", false),
        ("null", false),
        ("", false),
    ] {
        assert_eq!(origin_allowed(origin, &o), ok, "{origin}");
    }
    // The relay host is allowed exactly, never its parent (could be a shared suffix).
    let cfg = Config { relay_url: Some("wss://foo.workers.dev:8443/x".into()), ..Default::default() };
    let o = pwa_origins(&cfg);
    assert!(o.contains(&"https://foo.workers.dev".to_string()), "{o:?}");
    assert!(!origin_allowed("https://evil.workers.dev", &o));
    // A two-label lan_domain is its own base (never a bare TLD); IP relays add nothing.
    let cfg = Config { lan_domain: Some("example.com".into()), ..Default::default() };
    assert_eq!(pwa_origins(&cfg)[..2], ["https://example.com".to_string(), "https://*.example.com".to_string()]);
    let cfg = Config { relay_url: Some("ws://127.0.0.1:9000".into()), ..Default::default() };
    assert!(pwa_origins(&cfg).iter().all(|o| !o.contains("127.0.0.1")));
    // CR-19: pwa_url's origin is allowed exactly (port kept), wherever it is hosted.
    let cfg = Config { pwa_url: Some("https://Phone.Example.org:8443/app/?x#y".into()), ..Default::default() };
    let o = pwa_origins(&cfg);
    assert_eq!(o, ["https://phone.example.org:8443".to_string()]);
    assert!(origin_allowed("https://phone.example.org:8443", &o));
    assert!(!origin_allowed("https://phone.example.org", &o));
    assert!(!origin_allowed("https://x.phone.example.org:8443", &o));
    let cfg = Config { pwa_url: Some("https://user@evil.test/".into()), ..Default::default() };
    assert!(pwa_origins(&cfg).is_empty());
}

// ------------------------------------------------------------------ Cloudflare client

/// A minimal HTTP/1.1 stub on loopback: records each request head and answers with `respond`.
async fn http_stub(respond: impl Fn(&str) -> String + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let (log2, respond) = (log.clone(), Arc::new(respond));
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            let (log, respond) = (log2.clone(), respond.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                while !contains(&buf, b"\r\n\r\n") {
                    match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                log.lock().unwrap().push(head.clone());
                let _ = s.write_all(respond(&head).as_bytes()).await;
            });
        }
    });
    (base, log)
}

fn json_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A lax or hostile API that ignores the `type`/`name` filters and returns every record in the
/// zone: the client still only ever writes to its own record.
#[tokio::test]
async fn lax_api_cannot_widen_cloudflare_writes() {
    let all = r#"{"success":true,"errors":[],"result":[
        {"id":"apex","type":"A","name":"example.com","content":"203.0.113.10","proxied":true,"ttl":1},
        {"id":"www","type":"A","name":"www.example.com","content":"203.0.113.11","proxied":true,"ttl":1},
        {"id":"mx","type":"MX","name":"example.com","content":"mail.example.com","ttl":300},
        {"id":"ours","type":"A","name":"relay.example.com","content":"10.0.0.9","proxied":false,"ttl":60,"comment":"managed-by: second-screen-copilot"},
        {"id":"chal-other","type":"TXT","name":"_acme-challenge.www.example.com","content":"\"x\"","ttl":60}]}"#;
    let one = r#"{"success":true,"errors":[],"result":{"id":"ours","type":"A","name":"relay.example.com","content":"10.0.0.7","proxied":false,"ttl":60}}"#;
    let (base, log) =
        http_stub(
            move |head| {
                if head.starts_with("GET") { json_response("200 OK", all) } else { json_response("200 OK", one) }
            },
        )
        .await;
    let cf = CloudflareDns::with_base(Secret::new(TOKEN), &format!("{base}/client/v4"));

    let a = cf.list("z", "A", "relay.example.com").await.unwrap();
    assert_eq!(a.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["ours"]);
    let txt = cf.list("z", "TXT", "_acme-challenge.relay.example.com").await.unwrap();
    assert!(txt.is_empty(), "another name's challenge record must not be ours to delete: {txt:?}");

    cf.upsert_a("z", "relay.example.com", "10.0.0.7".parse().unwrap(), 60).await.unwrap();
    let writes: Vec<String> = log.lock().unwrap().iter().filter(|h| !h.starts_with("GET")).cloned().collect();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(writes[0].starts_with("PUT /client/v4/zones/z/dns_records/ours "), "{}", writes[0]);
}

/// The Cloudflare token goes only to the configured API host: a redirect to another origin must
/// not carry the `Authorization` header.
#[tokio::test]
async fn cloudflare_token_is_not_forwarded_on_redirect() {
    let (elsewhere, seen_elsewhere) =
        http_stub(|_| json_response("200 OK", r#"{"success":true,"errors":[],"result":[]}"#)).await;
    let target = format!("{elsewhere}/steal");
    let (base, _) = http_stub(move |_| {
        format!(
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: {target}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        )
    })
    .await;
    let cf = CloudflareDns::with_base(Secret::new(TOKEN), &format!("{base}/client/v4"));
    let _ = cf.list("z", "A", "relay.example.com").await;
    for head in seen_elsewhere.lock().unwrap().iter() {
        assert!(!head.to_ascii_lowercase().contains("authorization"), "token forwarded: {head}");
        assert!(!head.contains(TOKEN));
    }
}

const DOMAIN: &str = "relay.example.com";
const ZONE: &str = "zone-example";

fn lan_opts(cf: &FakeCloudflare, acme: &FakeAcme, dir: &std::path::Path) -> LanTlsOptions {
    LanTlsOptions {
        acme: AcmeDirectory::Custom(DIR.into()),
        cloudflare_api: cf.api(),
        zone_id: Some(ZONE.into()),
        data_dir: dir.to_path_buf(),
        propagation: Propagation {
            doh_url: Some(cf.doh()),
            min_wait: Duration::ZERO,
            poll: Duration::from_millis(20),
            timeout: Duration::from_secs(2),
        },
        acme_retry: instant_acme::RetryPolicy::new()
            .initial_delay(Duration::from_millis(5))
            .backoff(1.0)
            .timeout(Duration::from_secs(5)),
        lan_ip: Arc::new(|| Some("192.168.1.20".parse().unwrap())),
        ip_poll: Duration::from_millis(50),
        acme_http: Some(acme.factory()),
        ..Default::default()
    }
}

fn cf_secrets() -> Arc<MemorySecretStore> {
    let s = MemorySecretStore::default();
    s.set(&SecretKey::Cloudflare, &Secret::new(TOKEN)).unwrap();
    Arc::new(s)
}

async fn settle(m: &LanTlsManager) -> LanTlsStatus {
    let deadline = tokio::time::Instant::now() + T;
    while matches!(m.status(), LanTlsStatus::Starting) {
        assert!(tokio::time::Instant::now() < deadline, "manager never settled");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    m.status()
}

/// CR-13: the client re-points any existing `A` record with the configured name, including the
/// zone apex or a record the user made for something else. A typo in `lan_domain` (or setting it
/// to the bare zone) would silently point the user's website at a private LAN IP.
#[tokio::test]
async fn refuses_to_repoint_records_it_does_not_own() {
    for (domain, existing) in [("example.com", "203.0.113.10"), (DOMAIN, "203.0.113.20")] {
        let mut st =
            CfState { zones: vec![(ZONE.into(), "example.com".into())], zone_read: true, ..Default::default() };
        st.add(ZONE, "A", domain, existing, false);
        let cf = FakeCloudflare::start(st).await;
        let acme = FakeAcme::new(cf.state.clone(), 90);
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config { lan_domain: Some(domain.into()), ..Default::default() };
        let _ =
            ssc_transport::lan_tls::ensure_lan_tls_with(&cfg, cf_secrets().as_ref(), &lan_opts(&cf, &acme, dir.path()))
                .await;
        let recs = cf.state.lock().unwrap().find("A", domain);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].content, existing, "{domain}: a record the agent did not create was re-pointed");
    }
}

/// CR-14: local-only mode promises that nothing but encrypted phone frames leaves the PC, yet the
/// LAN TLS manager still calls the Cloudflare API (publishing the LAN IP) and the ACME CA.
#[tokio::test]
async fn local_only_mode_makes_no_cloudflare_or_acme_calls() {
    let cf = FakeCloudflare::start(CfState {
        zones: vec![(ZONE.into(), "example.com".into())],
        zone_read: true,
        ..Default::default()
    })
    .await;
    let acme = FakeAcme::new(cf.state.clone(), 90);
    let dir = tempfile::tempdir().unwrap();
    let t = start(test_config(), Identity::generate(), None).await;
    let cfg = Config { mode: ssc_core::protocol::Mode::Local, lan_domain: Some(DOMAIN.into()), ..Default::default() };
    let m = LanTlsManager::spawn(&cfg, cf_secrets(), lan_opts(&cf, &acme, dir.path()), t.lan_tls_handle());
    let _ = settle(&m).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = cf.state.lock().unwrap().log.clone();
    assert!(log.is_empty(), "local-only mode called Cloudflare: {log:?}");
    assert_eq!(acme.state.lock().unwrap().orders(), 0, "local-only mode ordered a certificate");
}

/// Control for the above: the same setup in cloud mode does provision (so the CR-14 test is
/// meaningful once fixed, and the fakes are wired correctly).
#[tokio::test]
async fn cloud_mode_lan_tls_provisions_with_fakes() {
    let cf = FakeCloudflare::start(CfState {
        zones: vec![(ZONE.into(), "example.com".into())],
        zone_read: true,
        ..Default::default()
    })
    .await;
    let acme = FakeAcme::new(cf.state.clone(), 90);
    let dir = tempfile::tempdir().unwrap();
    let t = start(test_config(), Identity::generate(), None).await;
    let cfg = Config { lan_domain: Some(DOMAIN.into()), ..Default::default() };
    let m = LanTlsManager::spawn(&cfg, cf_secrets(), lan_opts(&cf, &acme, dir.path()), t.lan_tls_handle());
    assert!(matches!(settle(&m).await, LanTlsStatus::Active { .. }), "{:?}", m.status());
    // Only our two names were ever written.
    for w in cf.state.lock().unwrap().records.iter() {
        assert!(w.name == DOMAIN || w.name == format!("_acme-challenge.{DOMAIN}"), "{w:?}");
    }
}
