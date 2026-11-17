use super::*;
use ssc_core::mock::{MockDecision, MockOutcome, decision_answer};
use ssc_core::protocol::Intent;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn input() -> DecisionInput {
    DecisionInput {
        question: "my card is 4111 1111 1111 1111, how much?".into(),
        screen_diff: "password=hunter2".into(),
        profiles: vec!["sales".into(), "support".into()],
    }
}

fn ok() -> MockOutcome<Decision> {
    MockOutcome::Ok(decision_answer(Intent::Pricing, "sales"))
}

struct Probe {
    up: AtomicBool,
    calls: AtomicUsize,
}

#[async_trait]
impl NetworkProbe for Probe {
    async fn reachable(&self) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.up.load(Ordering::SeqCst)
    }
}

struct Setup {
    router: DecisionRouter,
    jev: Arc<MockDecision>,
    tev: Arc<MockDecision>,
    probe: Arc<Probe>,
}

fn setup(jev: MockDecision) -> Setup {
    let jev = Arc::new(jev);
    let tev = Arc::new(MockDecision::always(decision_answer(Intent::Technical, "support")));
    let probe = Arc::new(Probe { up: AtomicBool::new(false), calls: AtomicUsize::new(0) });
    let router = DecisionRouter::new(
        Mode::Cloud,
        Some(DecisionSlot { model: jev.clone(), is_cloud: true }),
        DecisionSlot { model: tev.clone(), is_cloud: false },
        DecisionRouterConfig::default(),
        Some(probe.clone()),
    )
    .unwrap();
    Setup { router, jev, tev, probe }
}

#[tokio::test(start_paused = true)]
async fn jev_answers_within_800ms() {
    let s =
        setup(MockDecision::always(decision_answer(Intent::Pricing, "sales")).with_delay(Duration::from_millis(700)));
    let d = s.router.decide(&input()).await.unwrap();
    assert_eq!(d.source, DecisionSource::Cloud);
    assert_eq!(d.decision.intent.value, Intent::Pricing);
    assert_eq!(s.tev.calls(), 0);
    assert!(s.router.decisions_gate_answers());
}

#[tokio::test(start_paused = true)]
async fn two_timeouts_in_a_row_switch_to_tev1() {
    let s =
        setup(MockDecision::always(decision_answer(Intent::Pricing, "sales")).with_delay(Duration::from_millis(900)));
    let t0 = Instant::now();
    let d = s.router.decide(&input()).await.unwrap();
    assert_eq!(t0.elapsed(), Duration::from_millis(800), "Jev cut off at exactly 800 ms");
    assert_eq!(d.source, DecisionSource::LocalFallback, "a single timeout is answered by Tev1");
    assert_eq!(s.router.source(), DecisionSource::Cloud, "one timeout does not switch yet");

    s.router.decide(&input()).await.unwrap();
    assert_eq!(s.router.source(), DecisionSource::LocalFallback);
    assert!(!s.router.decisions_gate_answers(), "offline: decisions only tag");

    let t1 = Instant::now();
    let d = s.router.decide(&input()).await.unwrap();
    assert_eq!(t1.elapsed(), Duration::ZERO, "degraded calls skip Jev entirely");
    assert_eq!(d.source, DecisionSource::LocalFallback);
    assert_eq!(s.jev.calls(), 2);
    assert_eq!(s.tev.calls(), 3);
}

#[tokio::test(start_paused = true)]
async fn success_resets_the_count() {
    let s = setup(MockDecision::scripted(vec![MockOutcome::Timeout, ok(), MockOutcome::Unavailable], ok()));
    for _ in 0..3 {
        s.router.decide(&input()).await.unwrap();
    }
    assert_eq!(s.router.source(), DecisionSource::Cloud);
    assert_eq!(s.jev.calls(), 3);
}

#[tokio::test(start_paused = true)]
async fn unavailable_counts_as_network_failure_rate_limit_does_not() {
    let s = setup(MockDecision::scripted(
        vec![MockOutcome::RateLimited, MockOutcome::RateLimited, MockOutcome::Unavailable, MockOutcome::Unavailable],
        ok(),
    ));
    s.router.decide(&input()).await.unwrap();
    s.router.decide(&input()).await.unwrap();
    assert_eq!(s.router.source(), DecisionSource::Cloud);
    s.router.decide(&input()).await.unwrap();
    s.router.decide(&input()).await.unwrap();
    assert_eq!(s.router.source(), DecisionSource::LocalFallback);
}

#[tokio::test(start_paused = true)]
async fn network_check_restores_jev() {
    let s = setup(MockDecision::scripted(vec![MockOutcome::Timeout, MockOutcome::Timeout], ok()));
    s.router.decide(&input()).await.unwrap();
    s.router.decide(&input()).await.unwrap();
    assert_eq!(s.router.source(), DecisionSource::LocalFallback);

    // Probe not due yet (interval 10 s from the switch).
    s.router.decide(&input()).await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(s.probe.calls.load(Ordering::SeqCst), 0);

    // Due, but the network is still down.
    tokio::time::advance(Duration::from_secs(10)).await;
    s.router.decide(&input()).await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(s.probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(s.router.source(), DecisionSource::LocalFallback);

    // Back up: the next due probe restores Jev.
    s.probe.up.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(10)).await;
    let d = s.router.decide(&input()).await.unwrap();
    assert_eq!(d.source, DecisionSource::LocalFallback, "the probing call itself still uses Tev1");
    tokio::task::yield_now().await;
    assert_eq!(s.router.source(), DecisionSource::Cloud);
    let d = s.router.decide(&input()).await.unwrap();
    assert_eq!(d.source, DecisionSource::Cloud);
    assert_eq!(s.jev.calls(), 3);
}

#[tokio::test(start_paused = true)]
async fn redacts_before_cloud_only() {
    let s = setup(MockDecision::scripted(vec![MockOutcome::Timeout], ok()));
    s.router.decide(&input()).await.unwrap();
    let sent = &s.jev.inputs()[0];
    assert!(sent.question.contains("[CARD]") && !sent.question.contains("4111"), "{}", sent.question);
    assert_eq!(sent.screen_diff, "password=[REDACTED]");
    assert_eq!(s.tev.inputs()[0], input(), "local model gets the raw text");
}

struct PolicyDecision(AtomicUsize);

#[async_trait]
impl DecisionModel for PolicyDecision {
    fn id(&self) -> &str {
        "policy"
    }
    async fn decide(&self, _: &DecisionInput) -> Result<Decision> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(SscError::Policy("blocked".into()))
    }
}

#[tokio::test(start_paused = true)]
async fn policy_error_never_fails_over() {
    let tev = Arc::new(MockDecision::always(decision_answer(Intent::Technical, "support")));
    let r = DecisionRouter::new(
        Mode::Cloud,
        Some(DecisionSlot { model: Arc::new(PolicyDecision(AtomicUsize::new(0))), is_cloud: true }),
        DecisionSlot { model: tev.clone(), is_cloud: false },
        DecisionRouterConfig::default(),
        None,
    )
    .unwrap();
    assert!(matches!(r.decide(&input()).await, Err(SscError::Policy(_))));
    assert_eq!(tev.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn local_only_has_no_cloud_decision_model() {
    let tev: Arc<dyn DecisionModel> = Arc::new(MockDecision::always(decision_answer(Intent::Technical, "support")));
    let jev: Arc<dyn DecisionModel> = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let err = DecisionRouter::new(
        Mode::Local,
        Some(DecisionSlot { model: jev, is_cloud: true }),
        DecisionSlot { model: tev.clone(), is_cloud: false },
        DecisionRouterConfig::default(),
        None,
    );
    assert!(matches!(err, Err(SscError::Policy(_))));
    let remote_local = DecisionRouter::new(
        Mode::Local,
        None,
        DecisionSlot { model: tev.clone(), is_cloud: true },
        Default::default(),
        None,
    );
    assert!(matches!(remote_local, Err(SscError::Policy(_))));

    let r =
        DecisionRouter::new(Mode::Local, None, DecisionSlot { model: tev, is_cloud: false }, Default::default(), None)
            .unwrap();
    let d = r.decide(&input()).await.unwrap();
    assert_eq!(d.source, DecisionSource::LocalOnly);
    assert!(!r.decisions_gate_answers());
    assert!(!r.has_cloud());
}
