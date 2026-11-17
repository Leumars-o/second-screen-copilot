//! M2: providers are warmed at session start (local answer model: load + the session's prefix)
//! and re-warmed while capturing when no answer used them, against a fake Ollama on loopback.

mod common;

use common::*;
use ssc_core::config::Config;
use ssc_core::mock::{MockAudio, MockDecision, decision_answer};
use ssc_core::protocol::*;
use ssc_core::secrets::MemorySecretStore;
use ssc_core::timing::Timing;
use ssc_core::traits::{Clock, SystemClock};
use ssc_pipeline::pipeline::Extras;
use ssc_pipeline::{Parts, Pipeline, Settings};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Records request bodies; answers `/api/chat` with an empty finished stream.
async fn fake_ollama() -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
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
                log.lock().unwrap().push(String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string());
                let body = "{\"model\":\"m\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n";
                let msg = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(msg.as_bytes()).await;
            });
        }
    });
    (url, log)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_model_and_prefix_warmed_at_start_and_kept_warm() {
    let (url, log) = fake_ollama().await;
    let mut cfg = Config { mode: Mode::Local, ..Config::default() };
    cfg.models.ollama_url = url;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let models = ssc_router::build(&cfg, &MemorySecretStore::default(), clock.clone(), &Default::default()).unwrap();
    assert_eq!(models.warm.targets(), 1);
    let (pc, _ph) = ssc_core::mock::MockTransport::pair_with_clock(clock.clone());
    let parts = Parts {
        screen: None,
        audio: vec![Arc::new(MockAudio { chunks: vec![] })],
        stt: None,
        decision: Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales"))),
        decisions_gate: Arc::new(|| false),
        answers: Arc::new(models.answers),
        postcall: models.postcall,
        before_postcall: None,
        memory: seeded_memory(clock.clone()),
        transport: Arc::new(pc),
        rtt_ms: Arc::new(|| None),
        clock,
        timing: Arc::new(Timing::new(100)),
    };
    let mut settings = Settings::new(cfg);
    settings.rewarm_every = Duration::from_millis(300);
    let p = Pipeline::with_extras(parts, settings, Extras { warm: Some(models.warm), ..Default::default() });
    p.start_session(Some("sales".into())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let first = log.lock().unwrap().clone();
    assert!(first.iter().any(|b| b.contains("\"messages\":[]")), "model preloaded: {first:?}");
    let prefix = first.iter().find(|b| b.contains("\"num_predict\":1")).expect("prefix warmed");
    assert!(prefix.contains("Sell Acme Cloud."), "the session's profile is in the warmed prefix");
    // No answers: the interval passes idle, so it re-warms (twice in ~1 s).
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let n = log.lock().unwrap().len();
    assert!(n >= first.len() + 4, "re-warmed while idle: {n} requests");
    p.discard_session().await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = log.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(log.lock().unwrap().len(), after, "no warming after the session ended");
}
