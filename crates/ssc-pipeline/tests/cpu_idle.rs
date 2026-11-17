//! W2-INTEGRATE acceptance: CPU idle check in a test harness (Build Sheet §1: idle < 2%,
//! capturing < 10%). Own test binary, so no other test shares the process. CPU time comes from
//! `ps -o time=` (10 ms resolution) around a few seconds of wall time on the real runtime.
//! Mock components stand in for capture, so "capturing" here measures the pipeline's own loops
//! (screen polling, trigger engine, transport), not the OS capture APIs; `bench/cpu` measures
//! the real agent.

mod common;

use common::*;
use ssc_core::mock::{MockAnswer, MockAudio, MockDecision, MockStt, MockTransport, decision_no_answer};
use ssc_core::protocol::Mode;
use ssc_core::timing::Timing;
use ssc_core::traits::SystemClock;
use ssc_pipeline::{Parts, Pipeline, Settings};
use ssc_router::{AnswerRouter, Route};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn cpu_ms() -> u64 {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "time=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // [[HH:]MM:]SS.cc
    let mut total = 0.0f64;
    for part in s.split(':') {
        total = total * 60.0 + part.parse::<f64>().unwrap_or_else(|_| panic!("ps time {s:?}"));
    }
    (total * 1000.0) as u64
}

/// CPU % of one core over `wall`.
async fn measure(wall: Duration) -> f64 {
    let (c0, t0) = (cpu_ms(), Instant::now());
    tokio::time::sleep(wall).await;
    let (c1, el) = (cpu_ms(), t0.elapsed());
    (c1 - c0) as f64 / el.as_millis() as f64 * 100.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_and_capturing_cpu_within_budget() {
    let clock = Arc::new(SystemClock);
    let answers =
        AnswerRouter::new(Mode::Cloud, None, vec![Route::new(Arc::new(MockAnswer::cue_answer("m", true)))], None)
            .unwrap();
    let (pc, ph) = MockTransport::pair();
    let _phone = phone(Arc::new(ph), 5);
    let parts = Parts {
        screen: Some(Arc::new(fixture_screen())),
        audio: vec![Arc::new(MockAudio { chunks: vec![] })],
        stt: Some(Arc::new(MockStt { script: vec![] })),
        decision: Arc::new(MockDecision::always(decision_no_answer("sales"))),
        decisions_gate: Arc::new(|| true),
        answers: Arc::new(answers),
        postcall: Arc::new(MockAnswer::new("post", false, &["{}"])),
        before_postcall: None,
        memory: seeded_memory(clock.clone()),
        transport: Arc::new(pc),
        rtt_ms: Arc::new(|| None),
        clock,
        timing: Arc::new(Timing::new(100)),
    };
    let p = Pipeline::new(parts, Settings::new(Default::default()));
    tokio::time::sleep(Duration::from_millis(300)).await; // let start-up settle

    let idle = measure(Duration::from_secs(3)).await;
    println!("idle CPU: {idle:.2}%");
    assert!(idle < 2.0, "idle CPU {idle:.2}% >= 2%");

    p.start_session(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let capturing = measure(Duration::from_secs(3)).await;
    println!("capturing CPU (mock capture): {capturing:.2}%");
    assert!(capturing < 10.0, "capturing CPU {capturing:.2}% >= 10%");
    p.shutdown().await;
}
