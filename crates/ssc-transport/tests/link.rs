//! End-to-end tests of `PcTransport` over real loopback sockets.

mod common;

use common::*;
use futures_util::SinkExt;
use ssc_core::mock::MockClock;
use ssc_core::protocol::*;
use ssc_core::traits::{Clock, SystemClock, Transport};
use ssc_core::{SscError, protocol::Command as Cmd};
use ssc_crypto::{Identity, b64_array};
use ssc_transport::{LinkPath, PcTransport};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

fn answer_start(id: Uuid) -> Message {
    Message::AnswerStart(AnswerStart {
        answer_id: id,
        trigger: Trigger::Hotkey,
        format: AnswerFormat::Cue,
        intent: Intent::Pricing,
        urgency: Urgency::High,
        model: "groq".into(),
    })
}

fn delta(id: Uuid, t: &str) -> Message {
    Message::AnswerDelta(AnswerDelta { answer_id: id, block: Block::Cue, text: t.into() })
}

#[tokio::test]
async fn pairs_over_lan_then_round_trips() {
    let pc = Identity::generate();
    let pc_pub = pc.public_bytes();
    let t = start(test_config(), pc, None).await;
    let mut paired = t.take_device_updates().unwrap();
    let mut incoming = t.take_incoming().unwrap();
    assert!(t.take_incoming().is_none());

    // Nothing to send to before pairing.
    let err = t.send(None, Message::Heartbeat(Heartbeat::default())).await.unwrap_err();
    assert!(matches!(err, SscError::Unavailable(_)));

    let qr = t.begin_pairing().await.unwrap();
    assert_eq!(qr.urls, vec![format!("ws://127.0.0.1:{}", t.local_addr().port())]);
    assert_eq!(b64_array::<32>(&qr.pc_pub).unwrap(), pc_pub);
    assert!(t.status().pairing);

    let phone = Identity::generate();
    let mut ph = FakePhone::pair(&qr, 0, &phone).await;
    let dev = tokio::time::timeout(T, paired.recv()).await.unwrap().unwrap();
    assert_eq!(dev.device_name, "Pixel");
    assert_eq!(dev.phone_pub, phone.public_b64());
    wait_for(&t, "paired", |s| s.paired && !s.pairing && s.path == Some(LinkPath::Lan)).await;

    // phone -> PC
    let sid = Some(Uuid::new_v4());
    let a = Uuid::new_v4();
    let sent = ph.send(P::Lan, sid, Message::Command(Cmd::Pin { answer_id: a })).await;
    let got = tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap();
    assert_eq!(got, sent);
    // PC -> phone
    t.send(sid, answer_start(a)).await.unwrap();
    let env = ph.recv(P::Lan).await;
    assert_eq!(env.message, answer_start(a));
    assert_eq!(env.session_id, sid);

    // Snapshot carries counters for persistence.
    let snap = t.device_snapshot().await.unwrap();
    assert_eq!(snap.keys, dev.keys);
    assert!(snap.counters.next_send > 1 && snap.counters.last_recv == Some(sent.seq));
}

#[tokio::test]
async fn pairing_rejects_bad_proof_replay_and_expiry() {
    let clock = MockClock::new(1_763_290_000_000);
    let t = PcTransport::start(test_config(), Identity::generate(), None, Arc::new(clock.clone())).await.unwrap();
    let qr = t.begin_pairing().await.unwrap();
    let token: [u8; 32] = b64_array(&qr.token).unwrap();
    let phone = Identity::generate();

    // Wrong proof (token guess): socket closed, not paired, offer still armed (1 of 5 failures).
    let mut ws = ws_connect(&qr.urls[0]).await;
    ws.send(WsMessage::Binary(hello_bytes(&phone, &[0u8; 32], "evil").into())).await.unwrap();
    let mut ph = FakePhone {
        ch: FakePhone::new(&ssc_crypto::SessionKeys { pc_to_phone: [0; 32], phone_to_pc: [0; 32] }).ch,
        lan: Some(ws),
        relay: None,
    };
    assert!(ph.closed_within(P::Lan, T).await);
    assert!(!t.status().paired && t.status().pairing);

    // Garbage that is not a pair.hello: closed too.
    let mut ws = ws_connect(&qr.urls[0]).await;
    ws.send(WsMessage::Binary(plaintext_frame(br#"{"hello":1}"#).into())).await.unwrap();
    ph.lan = Some(ws);
    assert!(ph.closed_within(P::Lan, T).await);

    // Expired: 121 s later the right proof fails and the offer is gone.
    clock.advance(121_000);
    let mut ws = ws_connect(&qr.urls[0]).await;
    ws.send(WsMessage::Binary(hello_bytes(&phone, &token, "late").into())).await.unwrap();
    ph.lan = Some(ws);
    assert!(ph.closed_within(P::Lan, T).await);
    wait_for(&t, "offer burned", |s| !s.pairing && !s.paired).await;

    // A fresh offer pairs; replaying the same hello afterwards is refused (token burned).
    let qr = t.begin_pairing().await.unwrap();
    let token: [u8; 32] = b64_array(&qr.token).unwrap();
    let _ok = FakePhone::pair(&qr, 0, &phone).await;
    wait_for(&t, "paired", |s| s.paired).await;
    let mut ws = ws_connect(&qr.urls[0]).await;
    ws.send(WsMessage::Binary(hello_bytes(&Identity::generate(), &token, "replay").into())).await.unwrap();
    ph.lan = Some(ws);
    assert!(ph.closed_within(P::Lan, T).await);
}

#[tokio::test]
async fn five_bad_proofs_burn_the_token() {
    let t = start(test_config(), Identity::generate(), None).await;
    let qr = t.begin_pairing().await.unwrap();
    let phone = Identity::generate();
    for _ in 0..5 {
        let mut ws = ws_connect(&qr.urls[0]).await;
        ws.send(WsMessage::Binary(hello_bytes(&phone, &[1u8; 32], "guess").into())).await.unwrap();
        let mut ph = FakePhone {
            ch: FakePhone::new(&ssc_crypto::SessionKeys { pc_to_phone: [0; 32], phone_to_pc: [0; 32] }).ch,
            lan: Some(ws),
            relay: None,
        };
        assert!(ph.closed_within(P::Lan, T).await);
    }
    wait_for(&t, "offer burned", |s| !s.pairing).await;
    // The right token no longer works.
    let token: [u8; 32] = b64_array(&qr.token).unwrap();
    let mut ws = ws_connect(&qr.urls[0]).await;
    ws.send(WsMessage::Binary(hello_bytes(&phone, &token, "late").into())).await.unwrap();
    let mut ph = FakePhone {
        ch: FakePhone::new(&ssc_crypto::SessionKeys { pc_to_phone: [0; 32], phone_to_pc: [0; 32] }).ch,
        lan: Some(ws),
        relay: None,
    };
    assert!(ph.closed_within(P::Lan, T).await);
    assert!(!t.status().paired);
}

#[tokio::test]
async fn drops_tampered_replayed_and_wrong_direction_frames() {
    let (pc, _phone, dev) = prepaired();
    let t = start(test_config(), pc, Some(dev.clone())).await;
    let mut incoming = t.take_incoming().unwrap();
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);

    let a = Uuid::new_v4();
    let (e1, w1) = ph.seal(None, Message::Command(Cmd::Pin { answer_id: a }));
    let mut tampered = w1.clone();
    *tampered.last_mut().unwrap() ^= 1;
    ph.send_raw(P::Lan, tampered).await;
    ph.send_raw(P::Lan, w1.clone()).await;
    ph.send_raw(P::Lan, w1.clone()).await; // replay
    // Wrong direction: a frame sealed with the PC's own key, reflected back.
    let mut reflect = ssc_crypto::SecureChannel::new(ssc_crypto::Role::Pc, &dev.keys, Default::default());
    let (_, wr) = reflect.seal(1, None, Message::Command(Cmd::Pin { answer_id: a })).unwrap();
    ph.send_raw(P::Lan, wr).await;
    let (e2, w2) = ph.seal(None, Message::Command(Cmd::Regenerate { answer_id: a }));
    ph.send_raw(P::Lan, w2).await;

    assert_eq!(tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap(), e1);
    assert_eq!(tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap(), e2);
    assert!(tokio::time::timeout(Duration::from_millis(100), incoming.recv()).await.is_err(), "nothing else delivered");
    wait_for(&t, "3 dropped", |s| s.dropped_frames == 3).await;
}

#[tokio::test]
async fn batches_answer_deltas() {
    let (pc, _phone, dev) = prepaired();
    let t = start(test_config(), pc, Some(dev.clone())).await;
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat { echo: Some(0) })).await; // mark LAN live
    wait_for(&t, "lan live", |s| s.phone_live).await;

    let a = Uuid::new_v4();
    // 40 deltas of 2 chars: the first goes out alone and at once (M1), then two 32-char frames
    // right away, the last 14 chars after 50 ms.
    let t0 = Instant::now();
    for _ in 0..40 {
        t.send(None, delta(a, "ab")).await.unwrap();
    }
    let Message::AnswerDelta(first) = ph.recv(P::Lan).await.message else { panic!("expected delta") };
    assert_eq!(first.text, "ab");
    assert!(t0.elapsed() < Duration::from_millis(45), "first delta is not held for the window");
    let mut texts = vec![];
    for _ in 0..3 {
        let Message::AnswerDelta(d) = ph.recv(P::Lan).await.message else { panic!("expected delta") };
        texts.push(d.text);
    }
    let lens: Vec<_> = texts.iter().map(|s| s.len()).collect();
    assert_eq!(lens, vec![32, 32, 14]);
    assert!(t0.elapsed() >= Duration::from_millis(45), "tail waits for the 50 ms window");

    // A non-delta message flushes the pending delta first (order preserved).
    t.send(None, delta(a, "x")).await.unwrap();
    t.send(
        None,
        Message::AnswerEnd(AnswerEnd {
            answer_id: a,
            reason: AnswerEndReason::Done,
            cues: vec![],
            fact: None,
            source_doc: None,
            not_in_docs: false,
        }),
    )
    .await
    .unwrap();
    let t1 = Instant::now();
    assert!(matches!(ph.recv(P::Lan).await.message, Message::AnswerDelta(d) if d.text == "x"));
    assert!(matches!(ph.recv(P::Lan).await.message, Message::AnswerEnd(_)));
    assert!(t1.elapsed() < Duration::from_millis(45), "flushed without waiting for the window");

    // Schema violations are refused before queueing.
    assert!(matches!(t.send(None, delta(a, "")).await, Err(SscError::Invalid(_))));
}

#[tokio::test]
async fn heartbeats_measure_rtt_and_dead_links_close() {
    let (pc, _phone, dev) = prepaired();
    let cfg = ssc_transport::TransportConfig {
        heartbeat_interval: Duration::from_millis(100),
        dead_after: Duration::from_millis(400),
        ..test_config()
    };
    let t = start(cfg, pc, Some(dev.clone())).await;
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);

    // Phone heartbeat without echo -> PC answers at once with echo = our ts.
    let sent = ph.send(P::Lan, None, Message::Heartbeat(Heartbeat::default())).await;
    let t_sent = Instant::now();
    loop {
        // (periodic PC heartbeats may arrive first)
        let reply = ph.recv_raw(P::Lan).await.unwrap();
        if reply.message == Message::Heartbeat(Heartbeat { echo: Some(sent.ts) }) {
            break;
        }
        assert!(t_sent.elapsed() < Duration::from_secs(1), "no echo reply");
    }

    // PC heartbeats every 100 ms; the phone echoes them; RTT shows up in status.
    let hb = loop {
        let hb = ph.recv_raw(P::Lan).await.unwrap();
        if hb.message == Message::Heartbeat(Heartbeat { echo: None }) {
            break hb;
        }
    };
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat { echo: Some(hb.ts) })).await;
    wait_for(&t, "rtt", |s| s.rtt_ms.is_some()).await;
    assert!(t.rtt_ms().unwrap() < 1000);

    // Phone goes silent: after 400 ms the PC declares the link dead and closes the socket.
    let t0 = Instant::now();
    assert!(ph.closed_within(P::Lan, Duration::from_secs(3)).await);
    assert!(t0.elapsed() >= Duration::from_millis(250), "not closed early: {:?}", t0.elapsed());
    wait_for(&t, "link down", |s| s.path.is_none() && !s.phone_live).await;
}

#[tokio::test]
async fn falls_back_to_relay_when_lan_dies_and_back() {
    let relay = FakeRelay::start().await;
    let (pc, _phone, dev) = prepaired();
    let cfg = ssc_transport::TransportConfig { relay_url: Some(relay.url()), ..test_config() };
    let t = start(cfg, pc, Some(dev.clone())).await;
    let mut incoming = t.take_incoming().unwrap();
    let mut ph = FakePhone::from_device(&dev);

    // PC's relay socket comes up in the device's room; it is not a path until the phone speaks.
    wait_for(&t, "relay up", |s| s.relay_connected && s.path.is_none()).await;
    ph.relay = Some(ws_connect(&format!("{}/r/{}", relay.url(), dev.room_id)).await);
    ph.send(P::Relay, None, Message::Heartbeat(Heartbeat::default())).await;
    wait_for(&t, "phone on relay", |s| s.path == Some(LinkPath::Relay)).await;
    // Read the echo now (one replay window across both sockets, so keep frames in order).
    assert!(matches!(ph.recv_raw(P::Relay).await.unwrap().message, Message::Heartbeat(Heartbeat { echo: Some(_) })));
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat { echo: Some(0) })).await;
    wait_for(&t, "lan preferred", |s| s.path == Some(LinkPath::Lan)).await;

    let a = Uuid::new_v4();
    t.send(None, answer_start(a)).await.unwrap();
    assert_eq!(ph.recv(P::Lan).await.message, answer_start(a));
    let relayed_before = *relay.forwarded.lock().unwrap();

    // Kill the LAN socket. The PC switches to the relay, where the phone was heard recently.
    drop(ph.lan.take());
    wait_for(&t, "relay fallback", |s| s.path == Some(LinkPath::Relay)).await;
    let b = Uuid::new_v4();
    t.send(None, answer_start(b)).await.unwrap();
    assert_eq!(ph.recv(P::Relay).await.message, answer_start(b));
    // And phone -> PC over the relay; same channel, so seq continues and nothing is replayed.
    let sent = ph.send(P::Relay, None, Message::Command(Cmd::Pin { answer_id: b })).await;
    assert_eq!(tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap(), sent);
    assert!(*relay.forwarded.lock().unwrap() >= relayed_before + 2);

    // LAN comes back: the phone's first LAN frame moves the PC back to LAN.
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat { echo: Some(0) })).await;
    wait_for(&t, "back on lan", |s| s.path == Some(LinkPath::Lan)).await;
    t.send(None, answer_start(a)).await.unwrap();
    assert_eq!(ph.recv(P::Lan).await.message, answer_start(a));
}

#[tokio::test]
async fn pairs_through_the_relay() {
    let relay = FakeRelay::start().await;
    let cfg = ssc_transport::TransportConfig { relay_url: Some(relay.url()), ..test_config() };
    let t = start(cfg, Identity::generate(), None).await;
    let qr = t.begin_pairing().await.unwrap();
    assert_eq!(qr.urls.len(), 2);
    let token: [u8; 32] = b64_array(&qr.token).unwrap();
    assert_eq!(qr.urls[1], format!("{}/r/{}", relay.url(), ssc_crypto::keys::room_id(&token)));
    wait_for(&t, "relay up in pairing room", |s| s.relay_connected).await;

    let mut ph = FakePhone::pair(&qr, 1, &Identity::generate()).await;
    wait_for(&t, "paired", |s| s.paired).await;
    let a = Uuid::new_v4();
    t.send(None, answer_start(a)).await.unwrap();
    assert_eq!(ph.recv(P::Relay).await.message, answer_start(a));
}

#[tokio::test]
async fn re_pairing_cuts_off_the_previous_phone() {
    let (pc, _phone, dev) = prepaired();
    let t = start(test_config(), pc, Some(dev.clone())).await;
    let mut old = FakePhone::from_device(&dev);
    old.lan = Some(ws_connect(&t.lan_url()).await);
    old.send(P::Lan, None, Message::Heartbeat(Heartbeat { echo: Some(0) })).await;
    wait_for(&t, "old phone live", |s| s.phone_live).await;

    let qr = t.begin_pairing().await.unwrap();
    let mut new = FakePhone::pair(&qr, 0, &Identity::generate()).await;
    assert!(old.closed_within(P::Lan, T).await, "old phone's socket is closed");
    let a = Uuid::new_v4();
    t.send(None, answer_start(a)).await.unwrap();
    assert_eq!(new.recv(P::Lan).await.message, answer_start(a));
    assert_ne!(t.device_snapshot().await.unwrap().keys, dev.keys);
}

#[tokio::test]
async fn pair_hello_must_be_a_binary_0x00_frame() {
    let t = start(test_config(), Identity::generate(), None).await;
    let qr = t.begin_pairing().await.unwrap();
    let token: [u8; 32] = b64_array(&qr.token).unwrap();
    let phone = Identity::generate();
    let framed = hello_bytes(&phone, &token, "Pixel");
    let bare_json = framed[1..].to_vec();

    let mut ws = ws_connect(&qr.urls[0]).await;
    // Text frame: ignored. Bare JSON without the 0x00 prefix: dropped as a bad sealed frame.
    ws.send(WsMessage::Text(String::from_utf8(bare_json.clone()).unwrap().into())).await.unwrap();
    ws.send(WsMessage::Binary(bare_json.into())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!t.status().paired && t.status().pairing);
    // The proper frame on the same socket pairs.
    ws.send(WsMessage::Binary(framed.into())).await.unwrap();
    wait_for(&t, "paired", |s| s.paired).await;
}

#[tokio::test]
async fn seq_is_per_pairing_and_survives_restart() {
    // CR-3: counters are reserved in blocks, handed out for persistence, and a restarted PC
    // resumes above every seq it ever sent.
    let (pc, _phone, dev) = prepaired();
    let pc_secret = pc.to_secret();
    let t = start(test_config(), pc, Some(dev.clone())).await;
    let mut updates = t.take_device_updates().unwrap();
    let first = tokio::time::timeout(T, updates.recv()).await.unwrap().unwrap();
    assert_eq!(first.counters.next_send, 1 + ssc_crypto::frame::SEQ_RESERVE_BLOCK);

    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat { echo: Some(0) })).await;
    wait_for(&t, "live", |s| s.phone_live).await;
    let n = ssc_crypto::frame::SEQ_RESERVE_BLOCK + 5;
    for _ in 0..n {
        t.send(None, answer_start(Uuid::new_v4())).await.unwrap();
    }
    let mut last_seq = 0;
    for _ in 0..n {
        last_seq = ph.recv(P::Lan).await.seq;
    }
    // A new block was reserved and reported.
    let mut latest = first;
    while let Ok(Some(d)) = tokio::time::timeout(Duration::from_millis(200), updates.recv()).await {
        latest = d;
    }
    assert!(latest.counters.next_send > last_seq, "{latest:?} vs {last_seq}");

    // Restart the PC from the persisted record; the same phone keeps its channel state.
    t.shutdown();
    drop(t);
    let pc = Identity::from_secret_b64(&pc_secret).unwrap();
    let t2 = start(test_config(), pc, Some(latest.clone())).await;
    ph.lan = Some(ws_connect(&t2.lan_url()).await);
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat { echo: Some(0) })).await;
    wait_for(&t2, "live", |s| s.phone_live).await;
    t2.send(None, answer_start(Uuid::new_v4())).await.unwrap();
    let env = ph.recv(P::Lan).await;
    assert!(env.seq > last_seq, "restarted PC resumed at {} after {last_seq}", env.seq);
}

/// H1: RTT comes only from echoes of the PC's own probes. A phone echoing an old or made-up ts
/// (the old phone echoed the PC's last heartbeat ts on its own 5 s tick) must not count.
#[tokio::test]
async fn rtt_ignores_echoes_of_unknown_probes() {
    let (pc, _phone, dev) = prepaired();
    let clock = MockClock::new(1_763_290_000_000);
    let cfg = ssc_transport::TransportConfig { heartbeat_interval: Duration::from_millis(50), ..test_config() };
    let t = PcTransport::start(cfg, pc, Some(dev.clone()), Arc::new(clock.clone())).await.unwrap();
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat::default())).await; // mark live
    let probe = loop {
        let hb = ph.recv_raw(P::Lan).await.unwrap();
        if hb.message == Message::Heartbeat(Heartbeat { echo: None }) {
            break hb;
        }
    };
    clock.advance(30);
    // Stale / invented echoes: an older ts, a phone-side ts, zero.
    for bogus in [probe.ts - 5_000, probe.ts + 1, 0] {
        ph.send(P::Lan, None, Message::Heartbeat(Heartbeat { echo: Some(bogus) })).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(t.rtt_ms(), None, "no RTT from echoes the PC never probed");
    // The real echo gives the exact round trip on the mock clock.
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat { echo: Some(probe.ts) })).await;
    wait_for(&t, "rtt", |s| s.rtt_ms.is_some()).await;
    assert_eq!(t.rtt_ms(), Some(30));
    // Replaying the same echo later is ignored (the probe was consumed).
    clock.advance(10_000);
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat { echo: Some(probe.ts) })).await;
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat { echo: Some(probe.ts - 20_000) })).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(t.rtt_ms(), Some(30));
}

/// M4: with the phone away (relay socket open, nobody in the room) answers are queued, not sent
/// into the void; when the phone comes back they are delivered exactly once, and `FrameSent`
/// fires only then.
#[tokio::test]
async fn answers_wait_for_an_absent_phone_and_arrive_once() {
    let relay = FakeRelay::start().await;
    let (pc, _phone, dev) = prepaired();
    let cfg = ssc_transport::TransportConfig {
        relay_url: Some(relay.url()),
        heartbeat_interval: Duration::from_millis(100),
        dead_after: Duration::from_millis(300),
        ..test_config()
    };
    let t = start(cfg, pc, Some(dev.clone())).await;
    let mut sent_rx = t.take_frame_sent().unwrap();
    assert!(t.take_frame_sent().is_none());
    wait_for(&t, "relay up", |s| s.relay_connected).await;

    // Phone was on the LAN and went away.
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat::default())).await;
    wait_for(&t, "lan live", |s| s.path == Some(LinkPath::Lan)).await;
    drop(ph.lan.take());
    wait_for(&t, "phone away", |s| s.path.is_none() && !s.phone_live).await;
    let forwarded_before = *relay.forwarded.lock().unwrap();

    let a = Uuid::new_v4();
    t.send(None, answer_start(a)).await.unwrap();
    t.send(None, delta(a, "hello")).await.unwrap();
    wait_for(&t, "queued", |s| s.queued == 2).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(*relay.forwarded.lock().unwrap(), forwarded_before, "nothing sent into an empty relay room");
    assert!(sent_rx.try_recv().is_err(), "no frame_sent while queued");

    // Phone returns via the relay; its first frame flushes the queue.
    ph.relay = Some(ws_connect(&format!("{}/r/{}", relay.url(), dev.room_id)).await);
    let t_back = SystemClock.now_ms();
    ph.send(P::Relay, None, Message::Heartbeat(Heartbeat::default())).await;
    assert_eq!(ph.recv(P::Relay).await.message, answer_start(a));
    assert_eq!(ph.recv(P::Relay).await.message, delta(a, "hello"));
    let ev = tokio::time::timeout(T, sent_rx.recv()).await.unwrap().unwrap();
    assert_eq!((ev.answer_id, ev.path), (a, LinkPath::Relay));
    assert!(ev.t_ms >= t_back, "stamped when written, not when queued");
    wait_for(&t, "queue empty", |s| s.queued == 0).await;

    // Exactly once: the next non-heartbeat frame is a new one.
    let b = Uuid::new_v4();
    t.send(None, answer_start(b)).await.unwrap();
    assert_eq!(ph.recv(P::Relay).await.message, answer_start(b));
}

/// M1: the first delta of an answer is reported via `FrameSent` once per answer, as soon as it
/// is written (no batching delay).
#[tokio::test]
async fn frame_sent_once_per_answer_without_batching_delay() {
    let (pc, _phone, dev) = prepaired();
    let t = start(test_config(), pc, Some(dev.clone())).await;
    let mut sent_rx = t.take_frame_sent().unwrap();
    let mut ph = FakePhone::from_device(&dev);
    ph.lan = Some(ws_connect(&t.lan_url()).await);
    ph.send(P::Lan, None, Message::Heartbeat(Heartbeat::default())).await;
    wait_for(&t, "lan live", |s| s.phone_live).await;

    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let t0 = Instant::now();
    let ms0 = SystemClock.now_ms();
    t.send(None, answer_start(a)).await.unwrap();
    for _ in 0..5 {
        t.send(None, delta(a, "xy")).await.unwrap();
    }
    let ev = tokio::time::timeout(T, sent_rx.recv()).await.unwrap().unwrap();
    assert!(t0.elapsed() < Duration::from_millis(45), "reported before the 50 ms window: {:?}", t0.elapsed());
    assert_eq!((ev.answer_id, ev.path), (a, LinkPath::Lan));
    assert!(ev.t_ms >= ms0);
    t.send(None, delta(b, "z")).await.unwrap();
    let ev = tokio::time::timeout(T, sent_rx.recv()).await.unwrap().unwrap();
    assert_eq!(ev.answer_id, b);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(sent_rx.try_recv().is_err(), "one event per answer");
    assert_eq!(ph.recv(P::Lan).await.message, answer_start(a));
    assert_eq!(ph.recv(P::Lan).await.message, delta(a, "xy"));
}

/// L4: the phone resends its pair.hello until confirmed. A repeat of the hello the current
/// pairing came from is ignored (socket kept, pairing unchanged) instead of closing the socket,
/// and the phone's sealed probe on that socket then confirms.
#[tokio::test]
async fn repeated_pair_hello_is_idempotent() {
    let t = start(test_config(), Identity::generate(), None).await;
    let mut updates = t.take_device_updates().unwrap();
    let qr = t.begin_pairing().await.unwrap();
    let token: [u8; 32] = b64_array(&qr.token).unwrap();
    let phone = Identity::generate();
    let mut ph = FakePhone::pair(&qr, 0, &phone).await;
    let dev = tokio::time::timeout(T, updates.recv()).await.unwrap().unwrap();
    // Confirmation "lost": the phone reconnects and sends the same hello again, then a probe.
    drop(ph.lan.take());
    let mut ws = ws_connect(&qr.urls[0]).await;
    ws.send(WsMessage::Binary(hello_bytes(&phone, &token, "Pixel").into())).await.unwrap();
    ph.lan = Some(ws);
    let probe = ph.send(P::Lan, None, Message::Heartbeat(Heartbeat::default())).await;
    let reply = ph.recv_raw(P::Lan).await.expect("socket kept open");
    assert_eq!(reply.message, Message::Heartbeat(Heartbeat { echo: Some(probe.ts) }));
    let snap = t.device_snapshot().await.unwrap();
    assert_eq!((snap.device_id, snap.keys), (dev.device_id, dev.keys), "pairing unchanged");
    assert_eq!(t.status().dropped_frames, 0, "a repeated hello is not a bad frame");
    // While a new offer is armed, the old hello still does not count as a failed attempt.
    let qr2 = t.begin_pairing().await.unwrap();
    for _ in 0..6 {
        ph.send_raw(P::Lan, hello_bytes(&phone, &token, "Pixel")).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(t.status().pairing, "offer not burned by repeats");
    let _new = FakePhone::pair(&qr2, 0, &Identity::generate()).await;
    wait_for(&t, "re-paired", |s| s.paired && !s.pairing).await;
}
