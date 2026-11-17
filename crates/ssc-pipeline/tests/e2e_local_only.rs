//! W2-INTEGRATE acceptance: local-only end to end with the network disabled.
//!
//! The outer test re-runs this binary under `sandbox-exec` with a profile that denies every
//! outbound connection except loopback, and meanwhile samples the child's sockets with `lsof`.
//! The inner test (only active inside the sandbox) first proves the sandbox is on (connecting to
//! a TEST-NET address fails with EPERM), then runs the real stack in local-only mode: router
//! factory (Ollama + Tev1 adapters against a fake Ollama on 127.0.0.1), real SQLite memory, real
//! `PcTransport` with pairing and encryption to a phone client on loopback. The answer and the
//! summary must arrive, and the factory must never read an API key.
//!
//! Pass criteria: the child passes (so everything worked with only loopback available), and no
//! `lsof` sample shows a non-loopback remote endpoint.

mod common;

use common::*;
use futures_util::{SinkExt, StreamExt};
use ssc_core::config::Config;
use ssc_core::protocol::*;
use ssc_core::secrets::{MemorySecretStore, Secret, SecretKey, SecretStore};
use ssc_core::timing::{Stage, Timing};
use ssc_core::traits::{Clock, SystemClock};
use ssc_crypto::keys::pairing_proof;
use ssc_crypto::{Counters, Identity, Role, SecureChannel, SessionKeys, b64, b64_array};
use ssc_pipeline::transport::start_transport_with;
use ssc_pipeline::{Parts, Pipeline, RoutedDecisions, Settings};
use ssc_transport::TransportConfig;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const SANDBOX_ENV: &str = "SSC_LOCAL_ONLY_SANDBOXED";
const PROFILE: &str =
    r#"(version 1)(allow default)(deny network-outbound)(allow network-outbound (remote ip "localhost:*"))"#;

#[test]
fn local_only_e2e_without_network() {
    if std::env::var_os(SANDBOX_ENV).is_some() {
        return; // we are the child; the inner test does the work
    }
    let exe = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new("/usr/bin/sandbox-exec")
        .args(["-p", PROFILE])
        .arg(&exe)
        .args(["--exact", "local_only_inner", "--nocapture", "--test-threads", "1"])
        .env(SANDBOX_ENV, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("sandbox-exec");
    let pid = child.id().to_string();
    let mut bad = Vec::new();
    let mut samples = 0;
    let mut loopback_seen = 0;
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        // -a: AND the filters; -i: internet sockets; -n/-P: no name/port lookups.
        if let Ok(out) = std::process::Command::new("/usr/sbin/lsof").args(["-nP", "-a", "-p", &pid, "-i"]).output() {
            samples += 1;
            for line in String::from_utf8_lossy(&out.stdout).lines().skip(1) {
                if let Some(remote) = line.split("->").nth(1) {
                    let host = remote.split_whitespace().next().unwrap_or("");
                    let ok = host.starts_with("127.") || host.starts_with("[::1]") || host.starts_with("localhost");
                    if ok {
                        loopback_seen += 1;
                    } else {
                        bad.push(line.to_string());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut out).unwrap();
    assert!(status.success(), "sandboxed local-only run failed:\n{out}");
    assert!(out.contains("1 passed"), "inner test did not run:\n{out}");
    assert!(out.contains("LOCAL_ONLY_E2E_DONE"), "inner test returned early:\n{out}");
    assert!(samples > 0, "lsof never sampled");
    // The parser really sees connections (phone link, fake Ollama), so an empty `bad` means something.
    assert!(loopback_seen > 0, "lsof saw no connected sockets at all");
    assert!(bad.is_empty(), "non-loopback sockets seen: {bad:#?}");
}

/// Fails the test if anything reads a secret other than the PC identity / paired device.
struct DeviceKeysOnly(MemorySecretStore);
impl SecretStore for DeviceKeysOnly {
    fn get(&self, key: &SecretKey) -> ssc_core::Result<Option<Secret>> {
        assert!(
            matches!(key, SecretKey::PcIdentity | SecretKey::PairedDevice(_)),
            "local-only mode read provider key {key:?}"
        );
        self.0.get(key)
    }
    fn set(&self, key: &SecretKey, v: &Secret) -> ssc_core::Result<()> {
        self.0.set(key, v)
    }
    fn delete(&self, key: &SecretKey) -> ssc_core::Result<()> {
        self.0.delete(key)
    }
}

fn ndjson(tokens: &[&str]) -> String {
    let mut s = String::new();
    for t in tokens {
        let line = serde_json::json!({"model": "fake", "message": {"role": "assistant", "content": t}, "done": false});
        s.push_str(&format!("{line}\n"));
    }
    s.push_str(&format!("{}\n", serde_json::json!({"model": "fake", "message": {"role": "assistant", "content": ""}, "done": true, "done_reason": "stop"})));
    s
}

const SYSTEMONE: &str = r#"{"model":"tev1:4b","answers":{
 "needs_answer":{"type":"noul","noul":0.94},
 "intent":{"type":"choice","choice":"pricing","probabilities":{"pricing":0.81,"objection":0.07,"technical":0.05,"scheduling":0.03,"small_talk":0.02,"none":0.02}},
 "answer_tier":{"type":"choice","choice":"fast","probabilities":{"fast":0.72,"strong":0.2,"none":0.08}},
 "profile":{"type":"choice","choice":"sales","probabilities":{"sales":1.0}},
 "urgency":{"type":"choice","choice":"high","probabilities":{"low":0.05,"medium":0.25,"high":0.7}}},
 "usage":{"input_tokens":100,"output_tokens":0}}"#;

/// A fake Ollama on loopback: `/api/chat` (answers, summary, follow-up, load/unload) and
/// `/v1/systemone` (Tev1). Returns the base URL and a log of request paths.
async fn fake_ollama() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            let log = log2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let (head_end, len) = loop {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                            .unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let (ctype, resp) = if path == "/v1/systemone" {
                    ("application/json", SYSTEMONE.to_string())
                } else if body.contains("You summarise a finished call") {
                    ("application/x-ndjson", ndjson(&[SUMMARY_JSON]))
                } else if body.contains("follow-up email") {
                    ("application/x-ndjson", ndjson(&[FOLLOWUP]))
                } else if body.contains("\"messages\":[]") {
                    ("application/json", r#"{"model":"fake","done":true}"#.to_string())
                } else {
                    ("application/x-ndjson", ndjson(ANSWER_TOKENS))
                };
                let msg = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}",
                    resp.len()
                );
                let _ = s.write_all(msg.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    (url, log)
}

async fn wait(erx: &mut mpsc::UnboundedReceiver<Envelope>, what: &str, f: fn(&Message) -> bool) -> Envelope {
    let fut = async {
        loop {
            let e = erx.recv().await.expect("phone closed");
            if f(&e.message) {
                return e;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), fut).await.unwrap_or_else(|_| panic!("timeout: {what}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_only_inner() {
    if std::env::var_os(SANDBOX_ENV).is_none() {
        return; // only meaningful inside the sandbox (see the outer test)
    }
    // The sandbox is really on: a non-loopback connect is refused by the kernel at once.
    let canary = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect("192.0.2.1:80")).await;
    match canary {
        Ok(Err(e)) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}"),
        other => panic!("sandbox not active: {other:?}"),
    }

    let dir = std::env::temp_dir().join(format!("ssc-local-e2e-{}", uuid::Uuid::new_v4()));
    let (ollama, requests) = fake_ollama().await;
    let mut cfg = Config { mode: Mode::Local, ..Config::default() };
    cfg.models.ollama_url = ollama;
    // Cloud settings present but must be ignored in local-only mode.
    cfg.models.strong_model = Some("anthropic/claude-x".into());

    let secrets: Arc<dyn SecretStore> = Arc::new(DeviceKeysOnly(MemorySecretStore::default()));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let models = ssc_router::build(&cfg, &*secrets, clock.clone(), &ssc_router::RouterOptions::default()).unwrap();
    assert!(models.answers.models().iter().all(|m| !m.is_cloud()) && !models.postcall.is_cloud());
    assert!(!models.decisions.has_cloud());
    let decisions = Arc::new(models.decisions);

    let tcfg =
        TransportConfig { lan_bind: "127.0.0.1:0".parse().unwrap(), mdns: false, ..TransportConfig::from_core(&cfg) };
    let transport = start_transport_with(tcfg, secrets.clone(), clock.clone(), dir.clone()).await.unwrap();

    // Pair a phone over loopback (QR → pair.hello → sealed heartbeat).
    let qr = transport.begin_pairing().await.unwrap();
    let phone_id = Identity::generate();
    let token: [u8; 32] = b64_array(&qr.token).unwrap();
    let pc_pub: [u8; 32] = b64_array(&qr.pc_pub).unwrap();
    let keys = SessionKeys::derive(&phone_id.shared_secret(&pc_pub).unwrap(), &token);
    let mut ch = SecureChannel::new(Role::Phone, &keys, Counters::default());
    let (mut ws, _) = tokio_tungstenite::connect_async(&qr.urls[0]).await.unwrap();
    let hello = Message::PairHello(PairHello {
        device_name: "Phone".into(),
        phone_pub: phone_id.public_b64(),
        proof: b64(&pairing_proof(&token, &phone_id.public_bytes())),
    });
    let mut frame = vec![0u8];
    frame.extend(Envelope::new(0, SystemClock.now_ms(), None, hello).to_bytes());
    ws.send(WsMessage::Binary(frame.into())).await.unwrap();

    // Phone task: open frames, echo heartbeats, report paint time, forward everything.
    let (etx, mut erx) = mpsc::unbounded_channel::<Envelope>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<(Option<uuid::Uuid>, Message)>();
    tokio::spawn(async move {
        let mut painted = std::collections::HashSet::new();
        loop {
            tokio::select! {
                m = ws.next() => {
                    let Some(Ok(WsMessage::Binary(b))) = m else { if m.is_none() { break } else { continue } };
                    let env = ch.open(&b).expect("phone opens PC frame");
                    let mut reply = None;
                    match &env.message {
                        Message::Heartbeat(Heartbeat { echo: None }) => reply = Some(Message::Heartbeat(Heartbeat { echo: Some(env.ts) })),
                        Message::AnswerDelta(d) if painted.insert(d.answer_id) => {
                            reply = Some(Message::Command(Command::Feedback {
                                answer_id: d.answer_id,
                                item: FeedbackItem::Timing { stage: PhoneStage::PhoneRendered, delta_ms: 9 },
                            }));
                        }
                        _ => {}
                    }
                    if let Some(r) = reply {
                        let (_, wire) = ch.seal(SystemClock.now_ms(), env.session_id, r).unwrap();
                        ws.send(WsMessage::Binary(wire.into())).await.unwrap();
                    }
                    let _ = etx.send(env);
                }
                o = out_rx.recv() => {
                    let Some((sid, msg)) = o else { break };
                    let (_, wire) = ch.seal(SystemClock.now_ms(), sid, msg).unwrap();
                    ws.send(WsMessage::Binary(wire.into())).await.unwrap();
                }
            }
        }
    });
    wait(&mut erx, "pairing confirmation", |m| matches!(m, Message::Heartbeat(_))).await;

    std::fs::create_dir_all(&dir).unwrap();
    let memory = seeded_memory(clock.clone());
    let timing = Arc::new(Timing::new(1000));
    let rtt = transport.clone();
    let gate = decisions.clone();
    let parts = Parts {
        screen: Some(Arc::new(fixture_screen())),
        audio: vec![Arc::new(fixture_audio(clock.now_ms()))],
        stt: Some(Arc::new(fixture_stt(clock.now_ms()))),
        decision: Arc::new(RoutedDecisions(decisions.clone())),
        decisions_gate: Arc::new(move || gate.decisions_gate_answers()),
        answers: Arc::new(models.answers),
        postcall: models.postcall,
        before_postcall: None,
        memory: memory.clone(),
        transport: transport.clone(),
        rtt_ms: Arc::new(move || rtt.rtt_ms()),
        clock: clock.clone(),
        timing: timing.clone(),
    };
    let pipeline = Pipeline::new(parts, Settings::new(cfg));
    let sid = pipeline.start_session(Some("sales".into())).await.unwrap();
    let s = wait(&mut erx, "session.start", |m| matches!(m, Message::SessionStart(_))).await;
    assert!(matches!(s.message, Message::SessionStart(SessionStart { mode: Mode::Local, .. })));

    let env = wait(&mut erx, "answer.start", |m| matches!(m, Message::AnswerStart(_))).await;
    let Message::AnswerStart(start) = env.message else { unreachable!() };
    assert_eq!(start.trigger, Trigger::Question);
    assert!(start.model.starts_with("ollama/"));
    let env = wait(&mut erx, "answer.end", |m| matches!(m, Message::AnswerEnd(_))).await;
    let Message::AnswerEnd(end) = env.message else { unreachable!() };
    assert_eq!(end.reason, AnswerEndReason::Done);
    assert_eq!(end.fact.as_deref(), Some("45,000 naira per seat per year"));

    // Phone command through the encrypted link.
    out_tx.send((Some(sid), Message::Command(Command::MemoryList { layer: None }))).unwrap();
    wait(&mut erx, "memory.snapshot", |m| matches!(m, Message::MemorySnapshot(_))).await;

    // Timing (local mode: decision_done is a background tag, so it is not ordered).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let id = start.answer_id;
    loop {
        let ev: Vec<_> =
            timing.events().into_iter().filter(|e| e.answer_id == id && e.stage != Stage::DecisionDone).collect();
        if ev.iter().any(|e| e.stage == Stage::PhoneRendered) {
            let order: Vec<Stage> = ev.iter().map(|e| e.stage).collect();
            let want: Vec<Stage> = Stage::ALL.into_iter().filter(|s| *s != Stage::DecisionDone).collect();
            assert_eq!(order, want);
            assert!(ev.windows(2).all(|w| w[0].t_ms <= w[1].t_ms), "{ev:?}");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "phone_rendered missing: {ev:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let summary = pipeline.end_session(SessionEndReason::User).await.unwrap().unwrap();
    assert_eq!(summary.summary, "Globex asked about Enterprise pricing.");
    assert!(summary.followup_email.starts_with("Subject:"));
    wait(&mut erx, "session.summary", |m| matches!(m, Message::SessionSummary(_))).await;

    let paths = requests.lock().unwrap().clone();
    assert!(paths.iter().any(|p| p == "/api/chat"), "{paths:?}");
    pipeline.shutdown().await;
    transport.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    println!("LOCAL_ONLY_E2E_DONE");
}
