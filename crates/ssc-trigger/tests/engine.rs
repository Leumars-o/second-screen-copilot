//! Trigger engine behaviour against `ssc-core` mocks. Engine time comes from `MockClock`
//! (advanced by hand); decision delays use paused tokio time.

use ssc_core::config::Config;
use ssc_core::decision::{Decision, Scored, Tier};
use ssc_core::mock::{MockClock, MockDecision, MockOutcome, decision_answer, decision_no_answer};
use ssc_core::protocol::{AnswerFormat, Intent, Mode, Speaker, Trigger, Urgency};
use ssc_core::timing::{Stage, Timing};
use ssc_core::traits::Clock;
use ssc_core::types::{ScreenDiff, TranscriptSegment};
use ssc_trigger::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const T0: u64 = 1_000_000;
const PROFILES: [&str; 2] = ["sales", "support"];

fn settings(mode: Mode) -> TriggerSettings {
    let cfg = Config { mode, ..Config::default() };
    TriggerSettings::from_config(&cfg, PROFILES.iter().map(|s| s.to_string()).collect(), "sales")
}

fn engine(mode: Mode, decision: Arc<MockDecision>) -> (TriggerEngine, MockClock) {
    let clock = MockClock::new(T0);
    (TriggerEngine::new(settings(mode), decision, Arc::new(clock.clone())), clock)
}

/// A final "them" segment that ends an utterance at the current clock time.
fn them_end(clock: &MockClock, text: &str) -> TranscriptSegment {
    let t1 = clock.now_ms();
    TranscriptSegment {
        speaker: Speaker::Them,
        text: text.into(),
        is_final: true,
        t0_ms: t1 - 1500,
        t1_ms: t1,
        ends_utterance: true,
    }
}

/// Speak `text`, wait out the silence window, return what fired.
async fn ask(e: &TriggerEngine, clock: &MockClock, text: &str) -> Vec<TriggerEvent> {
    let mut out = e.on_transcript(&them_end(clock, text)).await;
    assert!(out.is_empty(), "fired before the silence window: {out:?}");
    clock.advance(e.settings().question_silence_ms);
    out.extend(e.poll().await);
    out
}

fn decision_with_tier(tier: Tier, p: f64) -> Decision {
    Decision { answer_tier: Scored::new(tier, p), ..decision_answer(Intent::Technical, "sales") }
}

// ---------- question detection (table-driven) ----------

struct Case {
    text: &'static str,
    decision: Decision,
    /// Expected (format, intent), or None for "nothing shown".
    want: Option<(AnswerFormat, Intent)>,
    /// Whether the decision model is consulted at all.
    calls: usize,
}

#[tokio::test]
async fn question_table_cloud() {
    let cases = [
        Case {
            text: "How much is the enterprise plan?",
            decision: decision_answer(Intent::Pricing, "sales"),
            want: Some((AnswerFormat::Fact, Intent::Pricing)),
            calls: 1,
        },
        Case {
            text: "Can you walk me through the onboarding",
            decision: decision_answer(Intent::Technical, "sales"),
            want: Some((AnswerFormat::Cue, Intent::Technical)),
            calls: 1,
        },
        Case {
            text: "When can we go live",
            decision: decision_answer(Intent::Scheduling, "sales"),
            want: Some((AnswerFormat::Fact, Intent::Scheduling)),
            calls: 1,
        },
        Case {
            text: "Honestly that's too expensive for us",
            decision: decision_answer(Intent::Objection, "sales"),
            want: Some((AnswerFormat::Objection, Intent::Objection)),
            calls: 1,
        },
        // Pidgin / Nigerian English
        Case {
            text: "How much be this one",
            decision: decision_answer(Intent::Pricing, "sales"),
            want: Some((AnswerFormat::Fact, Intent::Pricing)),
            calls: 1,
        },
        Case {
            text: "Shey una dey support USSD",
            decision: decision_answer(Intent::Technical, "support"),
            want: Some((AnswerFormat::Cue, Intent::Technical)),
            calls: 1,
        },
        Case {
            text: "this one go work offline abi",
            decision: decision_answer(Intent::Technical, "sales"),
            want: Some((AnswerFormat::Cue, Intent::Technical)),
            calls: 1,
        },
        // Small talk: filtered by the rules, never reaches the decision model.
        Case { text: "How are you doing today?", decision: decision_no_answer("sales"), want: None, calls: 0 },
        Case { text: "How far", decision: decision_no_answer("sales"), want: None, calls: 0 },
        // Not a question.
        Case {
            text: "Okay let me share my screen",
            decision: decision_answer(Intent::None, "sales"),
            want: None,
            calls: 0,
        },
        // Question-shaped but the decision model says no answer / small talk.
        Case { text: "Is it raining there?", decision: decision_no_answer("sales"), want: None, calls: 1 },
        Case {
            text: "Did you watch the match?",
            decision: Decision { needs_answer: Scored::new(true, 0.6), ..decision_answer(Intent::SmallTalk, "sales") },
            want: None,
            calls: 1,
        },
    ];
    for c in cases {
        let d = Arc::new(MockDecision::always(c.decision.clone()));
        let (e, clock) = engine(Mode::Cloud, d.clone());
        let out = ask(&e, &clock, c.text).await;
        let got = out.first().map(|ev| (ev.format, ev.intent));
        assert_eq!(got, c.want, "{:?}", c.text);
        assert_eq!(d.calls(), c.calls, "decision calls for {:?}", c.text);
        if let Some(ev) = out.first() {
            assert_eq!(out.len(), 1);
            assert_eq!(ev.trigger, Trigger::Question);
            assert_eq!(ev.question, c.text);
            assert_eq!(ev.tier, Tier::Fast);
            assert_eq!(ev.profile, c.decision.profile.value);
            assert_eq!(ev.urgency, Urgency::High);
            assert_eq!(ev.speech_end_ms, Some(T0));
            assert_eq!(ev.decision.as_ref(), Some(&c.decision));
        }
    }
}

#[tokio::test]
async fn decision_input_is_question_diff_and_profiles() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let (e, clock) = engine(Mode::Cloud, d.clone());
    e.on_screen_diff(&ScreenDiff { added_lines: vec!["Plan: Enterprise".into()], removed_lines: 0, truncated: false })
        .await;
    ask(&e, &clock, "How much is it?").await;
    let input = &d.inputs()[0];
    assert_eq!(input.question, "How much is it?");
    assert_eq!(input.screen_diff, "Plan: Enterprise");
    assert_eq!(input.profiles, PROFILES);
}

#[tokio::test]
async fn me_speech_never_triggers() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let (e, clock) = engine(Mode::Cloud, d.clone());
    let seg = TranscriptSegment { speaker: Speaker::Me, ..them_end(&clock, "How much is it?") };
    e.on_transcript(&seg).await;
    clock.advance(1000);
    assert!(e.poll().await.is_empty());
    assert_eq!(d.calls(), 0);
}

#[tokio::test]
async fn silence_window_waits_and_continued_speech_extends_the_utterance() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let (e, clock) = engine(Mode::Cloud, d.clone());
    assert!(e.on_transcript(&them_end(&clock, "How much is it")).await.is_empty());
    assert_eq!(e.next_deadline_ms(), Some(T0 + 300));
    clock.advance(250);
    assert!(e.poll().await.is_empty(), "fired inside the silence window");
    // They keep talking (speech started inside the window): the pending question is withdrawn
    // and the continuation is judged together with it.
    let cont = TranscriptSegment { t0_ms: T0 + 200, t1_ms: T0 + 250, ..them_end(&clock, "for ten seats I mean") };
    let out = e.on_transcript(&cont).await;
    assert!(out.is_empty(), "{out:?}");
    assert_eq!(e.next_deadline_ms(), Some(T0 + 550));
    clock.advance(300);
    let out = e.poll().await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].question, "How much is it for ten seats I mean");
    assert_eq!(d.calls(), 1);
}

#[tokio::test]
async fn partials_accumulate_into_one_utterance() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Technical, "sales")));
    let (e, clock) = engine(Mode::Cloud, d);
    let base = them_end(&clock, "");
    e.on_transcript(&TranscriptSegment { text: "Can you explain".into(), ends_utterance: false, ..base.clone() }).await;
    e.on_transcript(&TranscriptSegment { text: "the SSO setup".into(), is_final: false, ..base.clone() }).await;
    clock.advance(300);
    let out = e.poll().await;
    assert_eq!(out[0].question, "Can you explain the SSO setup");
}

#[tokio::test]
async fn late_transcript_fires_immediately() {
    // STT can deliver the final after the silence window has already passed.
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let (e, clock) = engine(Mode::Cloud, d);
    let seg = them_end(&clock, "How much is it?");
    clock.advance(600);
    let out = e.on_transcript(&seg).await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].speech_end_ms, Some(T0));
    assert_eq!(out[0].t_ms, T0 + 600);
}

#[tokio::test]
async fn foreign_profile_from_decision_is_ignored() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "someone-elses-profile")));
    let (e, clock) = engine(Mode::Cloud, d);
    let out = ask(&e, &clock, "How much is it?").await;
    assert_eq!(out[0].profile, "sales");
}

#[tokio::test]
async fn low_confidence_decision_routes_question_to_strong() {
    let d = Arc::new(MockDecision::always(decision_with_tier(Tier::Fast, 0.3)));
    let (e, clock) = engine(Mode::Cloud, d);
    assert_eq!(ask(&e, &clock, "How does SSO work?").await[0].tier, Tier::Strong);
}

#[tokio::test]
async fn decision_tier_none_shows_nothing() {
    let d = Arc::new(MockDecision::always(decision_with_tier(Tier::None, 0.9)));
    let (e, clock) = engine(Mode::Cloud, d);
    assert!(ask(&e, &clock, "How does SSO work?").await.is_empty());
}

// ---------- decision failures ----------

#[tokio::test(start_paused = true)]
async fn failed_decision_still_answers_a_detected_question() {
    for outcome in [MockOutcome::Unavailable, MockOutcome::RateLimited, MockOutcome::Timeout] {
        let d = Arc::new(MockDecision::scripted(vec![], outcome.clone()));
        let (e, clock) = engine(Mode::Cloud, d);
        let out = ask(&e, &clock, "How much be this one").await;
        assert_eq!(out.len(), 1, "{outcome:?}");
        assert_eq!(
            (out[0].format, out[0].intent, out[0].decision.clone()),
            (AnswerFormat::Fact, Intent::Pricing, None)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn hung_decision_times_out_at_the_configured_limit() {
    let d =
        Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")).with_delay(Duration::from_secs(60)));
    let (e, clock) = engine(Mode::Cloud, d);
    e.on_transcript(&them_end(&clock, "How much is it?")).await;
    clock.advance(300);
    let start = tokio::time::Instant::now();
    let out = e.poll().await;
    assert_eq!(start.elapsed(), Duration::from_millis(800));
    assert_eq!(out.len(), 1);
    assert!(out[0].decision.is_none());
}

// ---------- hotkey ----------

#[tokio::test(start_paused = true)]
async fn hotkey_fires_when_the_decision_model_always_errors() {
    let d = Arc::new(MockDecision::scripted(vec![], MockOutcome::Unavailable));
    let (e, clock) = engine(Mode::Cloud, d.clone());
    // Nothing said yet.
    let ev = e.hotkey();
    assert_eq!(
        (ev.trigger, ev.tier, ev.format, ev.urgency),
        (Trigger::Hotkey, Tier::Fast, AnswerFormat::Cue, Urgency::High)
    );
    // After a question whose decision failed.
    ask(&e, &clock, "Can you explain the pricing tiers?").await;
    let ev = e.hotkey();
    assert_eq!(ev.trigger, Trigger::Hotkey);
    assert_eq!(ev.question, "Can you explain the pricing tiers?");
    assert_eq!(ev.tier, Tier::Fast);
    assert!(ev.decision.is_none());
    // The hotkey itself never calls the model.
    assert_eq!(d.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn hotkey_fires_on_small_talk_and_statements() {
    let d = Arc::new(MockDecision::always(decision_no_answer("sales")));
    let (e, clock) = engine(Mode::Cloud, d);
    assert!(ask(&e, &clock, "Is it raining there?").await.is_empty());
    let ev = e.hotkey();
    assert_eq!(ev.format, AnswerFormat::Cue);
    assert_eq!(ev.intent, Intent::SmallTalk);
}

#[tokio::test]
async fn hotkey_tier_follows_decision_confidence() {
    let threshold = settings(Mode::Cloud).confidence_threshold;
    let cases = [
        (decision_with_tier(Tier::Fast, threshold + 0.1), Tier::Fast),
        (decision_with_tier(Tier::Fast, threshold - 0.1), Tier::Strong),
        (decision_with_tier(Tier::Strong, 0.9), Tier::Strong),
        // needs_answer false, confident: the user still asked, fast.
        (Decision { answer_tier: Scored::new(Tier::None, 0.9), ..decision_no_answer("sales") }, Tier::Fast),
    ];
    for (decision, want) in cases {
        let d = Arc::new(MockDecision::always(decision.clone()));
        let (e, clock) = engine(Mode::Cloud, d);
        ask(&e, &clock, "How does it work?").await;
        let ev = e.hotkey();
        assert_eq!(ev.tier, want, "{decision:?}");
        assert_eq!(ev.decision, Some(decision));
    }
}

#[tokio::test]
async fn hotkey_ignores_decision_for_an_older_utterance() {
    let d = Arc::new(MockDecision::always(decision_with_tier(Tier::Fast, 0.1)));
    let (e, clock) = engine(Mode::Cloud, d);
    ask(&e, &clock, "How does it work?").await;
    clock.advance(2000);
    e.on_transcript(&them_end(&clock, "We spoke with your colleague last week")).await;
    let ev = e.hotkey();
    assert_eq!(ev.tier, Tier::Fast);
    assert!(ev.decision.is_none());
}

/// Engine clock driven by tokio's (paused) clock, for the run-loop tests.
struct TokioClock(tokio::time::Instant);
impl Clock for TokioClock {
    fn now_ms(&self) -> u64 {
        T0 + self.0.elapsed().as_millis() as u64
    }
}

#[tokio::test(start_paused = true)]
async fn run_loop_hotkey_not_blocked_by_hung_decision() {
    let d =
        Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")).with_delay(Duration::from_secs(3600)));
    let clock = Arc::new(TokioClock(tokio::time::Instant::now()));
    let e = Arc::new(TriggerEngine::new(settings(Mode::Cloud), d.clone(), clock.clone()));
    let (in_tx, in_rx) = mpsc::channel(16);
    let (out_tx, mut out_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let task = e.clone().spawn(in_rx, out_tx, cancel.clone());

    let seg = TranscriptSegment {
        t1_ms: clock.now_ms(),
        t0_ms: clock.now_ms() - 1000,
        ..them_end(&MockClock::new(T0), "How much is it?")
    };
    in_tx.send(TriggerInput::Transcript(seg)).await.unwrap();
    // Let the silence window pass so the decision call is in flight.
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(d.calls(), 1);
    let before = tokio::time::Instant::now();
    in_tx.send(TriggerInput::Hotkey).await.unwrap();
    let ev = out_rx.recv().await.unwrap();
    assert_eq!(ev.trigger, Trigger::Hotkey);
    assert_eq!(before.elapsed(), Duration::ZERO, "hotkey waited on the decision model");
    // L6: the hotkey answered this utterance, so the automatic one is dropped once the decision
    // times out (before: a second, duplicate answer).
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(out_rx.try_recv().is_err(), "no duplicate answer for the same utterance");
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn run_loop_fires_question_after_silence_window() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let clock = Arc::new(TokioClock(tokio::time::Instant::now()));
    let e = Arc::new(TriggerEngine::new(settings(Mode::Cloud), d, clock.clone()));
    let (in_tx, in_rx) = mpsc::channel(16);
    let (out_tx, mut out_rx) = mpsc::channel(16);
    let task = e.spawn(in_rx, out_tx, CancellationToken::new());
    let start = tokio::time::Instant::now();
    let seg = TranscriptSegment {
        t1_ms: clock.now_ms(),
        t0_ms: clock.now_ms() - 1000,
        ..them_end(&MockClock::new(T0), "How much is it?")
    };
    in_tx.send(TriggerInput::Transcript(seg)).await.unwrap();
    let ev = out_rx.recv().await.unwrap();
    assert_eq!(ev.trigger, Trigger::Question);
    assert_eq!(start.elapsed(), Duration::from_millis(300));
    drop(in_tx);
    task.await.unwrap();
}

/// Regression (CR-20): the inputs closing (the transcript stream ended, no screen loop holding a
/// sender) must not drop a question still in its silence window. The loop used to exit at once.
#[tokio::test(start_paused = true)]
async fn run_loop_still_fires_a_pending_question_after_inputs_close() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let clock = Arc::new(TokioClock(tokio::time::Instant::now()));
    let e = Arc::new(TriggerEngine::new(settings(Mode::Cloud), d, clock.clone()));
    let (in_tx, in_rx) = mpsc::channel(16);
    let (out_tx, mut out_rx) = mpsc::channel(16);
    let task = e.spawn(in_rx, out_tx, CancellationToken::new());
    let start = tokio::time::Instant::now();
    let seg = TranscriptSegment {
        t1_ms: clock.now_ms(),
        t0_ms: clock.now_ms() - 1000,
        ..them_end(&MockClock::new(T0), "How much is it?")
    };
    in_tx.send(TriggerInput::Transcript(seg)).await.unwrap();
    drop(in_tx); // closed while the question waits out its silence window
    let ev = out_rx.recv().await.expect("pending question fired");
    assert_eq!(ev.trigger, Trigger::Question);
    assert_eq!(start.elapsed(), Duration::from_millis(300));
    // Nothing left pending: the loop ends by itself.
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("loop exits").unwrap();
}

// ---------- dedupe ----------

#[tokio::test]
async fn same_question_within_window_is_deduped() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let (e, clock) = engine(Mode::Cloud, d.clone());
    assert_eq!(ask(&e, &clock, "How much is it?").await.len(), 1);
    clock.advance(2000);
    // Same words, different punctuation/case.
    assert!(ask(&e, &clock, "how much is it").await.is_empty());
    assert_eq!(d.calls(), 1, "a deduped question must not cost a decision call");
    // A different question is not affected.
    assert_eq!(ask(&e, &clock, "When can we start?").await.len(), 1);
    // Window (5 s from the first fire) passes.
    clock.advance(5000);
    assert_eq!(ask(&e, &clock, "How much is it?").await.len(), 1);
}

#[tokio::test]
async fn hotkey_is_never_deduped_but_blocks_a_repeat_auto_answer() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let (e, clock) = engine(Mode::Cloud, d);
    assert_eq!(ask(&e, &clock, "How much is it?").await.len(), 1);
    assert_eq!(e.hotkey().trigger, Trigger::Hotkey);
    assert_eq!(e.hotkey().trigger, Trigger::Hotkey);
    clock.advance(4000);
    assert!(ask(&e, &clock, "How much is it?").await.is_empty());
}

#[tokio::test]
async fn hotkey_during_silence_window_supersedes_auto_answer() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let (e, clock) = engine(Mode::Cloud, d.clone());
    e.on_transcript(&them_end(&clock, "How much is it?")).await;
    let ev = e.hotkey();
    assert_eq!(ev.question, "How much is it?");
    clock.advance(500);
    assert!(e.poll().await.is_empty());
    assert_eq!(d.calls(), 0);
}

// ---------- offline ----------

#[tokio::test(start_paused = true)]
async fn offline_fires_without_waiting_for_the_decision_model() {
    let d = Arc::new(
        MockDecision::always(decision_answer(Intent::Pricing, "support")).with_delay(Duration::from_millis(500)),
    );
    let (tag_tx, mut tag_rx) = mpsc::unbounded_channel();
    let clock = MockClock::new(T0);
    let e = TriggerEngine::new(settings(Mode::Local), d.clone(), Arc::new(clock.clone())).with_tag_sink(tag_tx);
    let start = tokio::time::Instant::now();
    let out = ask(&e, &clock, "How much be this one").await;
    assert_eq!(start.elapsed(), Duration::ZERO, "offline path waited on the decision model");
    assert_eq!(out.len(), 1);
    let ev = &out[0];
    assert_eq!((ev.format, ev.intent, ev.tier), (AnswerFormat::Fact, Intent::Pricing, Tier::Fast));
    assert_eq!(ev.profile, "sales");
    assert!(ev.decision.is_none());
    // The decision model still tags it in the background.
    let tag = tag_rx.recv().await.unwrap();
    assert_eq!(tag.answer_id, ev.answer_id);
    assert_eq!(tag.decision.profile.value, "support");
    assert_eq!(start.elapsed(), Duration::from_millis(500));
    assert_eq!(d.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn offline_small_talk_and_statements_show_nothing() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let (e, clock) = engine(Mode::Local, d.clone());
    assert!(ask(&e, &clock, "How you dey").await.is_empty());
    assert!(ask(&e, &clock, "Thanks that makes sense").await.is_empty());
    tokio::task::yield_now().await;
    assert_eq!(d.calls(), 0);
}

// ---------- screen change ----------

fn diff(n: usize) -> ScreenDiff {
    ScreenDiff { added_lines: (0..n).map(|i| format!("line {i}")).collect(), removed_lines: 0, truncated: false }
}

#[tokio::test]
async fn big_screen_change_debounces_then_fires_once() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Technical, "sales")));
    let (e, clock) = engine(Mode::Cloud, d.clone());
    let lines = settings(Mode::Cloud).big_screen_change_lines;
    // Small changes never start a screen trigger.
    assert!(e.on_screen_diff(&diff(lines - 1)).await.is_empty());
    clock.advance(5000);
    assert!(e.poll().await.is_empty());
    // A big change, then the screen keeps changing for a bit (scrolling).
    assert!(e.on_screen_diff(&diff(lines)).await.is_empty());
    for _ in 0..3 {
        clock.advance(600);
        assert!(e.on_screen_diff(&diff(2)).await.is_empty());
    }
    clock.advance(DEFAULT_SCREEN_SETTLE_MS);
    let out = e.poll().await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].trigger, Trigger::Screen);
    assert!(d.inputs()[0].screen_diff.starts_with("line 0\nline 1\nline 0"), "newest lines first");
    // Another big change inside the cooldown: nothing.
    e.on_screen_diff(&diff(lines)).await;
    clock.advance(DEFAULT_SCREEN_SETTLE_MS);
    assert!(e.poll().await.is_empty());
    assert_eq!(d.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn screen_change_needs_the_decision_model() {
    for d in [MockDecision::always(decision_no_answer("sales")), MockDecision::scripted(vec![], MockOutcome::Timeout)] {
        let (e, clock) = engine(Mode::Cloud, Arc::new(d));
        e.on_screen_diff(&diff(50)).await;
        clock.advance(DEFAULT_SCREEN_SETTLE_MS);
        assert!(e.poll().await.is_empty());
    }
}

// ---------- passive timer ----------

#[tokio::test]
async fn passive_timer_off_by_default() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::None, "sales")));
    let (e, clock) = engine(Mode::Cloud, d.clone());
    assert_eq!(e.next_deadline_ms(), None);
    clock.advance(3_600_000);
    assert!(e.poll().await.is_empty());
    assert_eq!(d.calls(), 0);
}

#[tokio::test]
async fn passive_timer_fires_after_quiet_period() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::None, "sales")));
    let clock = MockClock::new(T0);
    let cfg = Config { passive_timer_s: Some(30), ..Config::default() };
    let s = TriggerSettings::from_config(&cfg, vec!["sales".into()], "sales");
    let e = TriggerEngine::new(s, d, Arc::new(clock.clone()));
    clock.advance(20_000);
    // Any trigger resets the timer.
    e.hotkey();
    clock.advance(20_000);
    assert!(e.poll().await.is_empty());
    clock.advance(10_000);
    let out = e.poll().await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].trigger, Trigger::Timer);
    assert_eq!(e.next_deadline_ms(), Some(T0 + 80_000));
}

// ---------- timing ----------

#[tokio::test]
async fn timing_stages_recorded() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let timing = Arc::new(Timing::new(64));
    let clock = MockClock::new(T0);
    let e = TriggerEngine::new(settings(Mode::Cloud), d, Arc::new(clock.clone())).with_timing(timing.clone());
    let out = ask(&e, &clock, "How much is it?").await;
    let id = out[0].answer_id;
    let stages: Vec<(Stage, u64)> =
        timing.events().into_iter().filter(|ev| ev.answer_id == id).map(|ev| (ev.stage, ev.t_ms)).collect();
    assert_eq!(
        stages,
        vec![
            (Stage::SpeechEnd, T0),
            (Stage::TranscriptFinal, T0),
            (Stage::TriggerFired, T0 + 300),
            (Stage::DecisionDone, T0 + 300),
        ]
    );
    let hk = e.hotkey();
    assert!(timing.events().iter().any(|ev| ev.answer_id == hk.answer_id && ev.stage == Stage::TriggerFired));
}

// ---------- safety: a decision cannot change mode or redaction ----------

/// Compile-level: these exhaustive destructurings fail to compile if anyone adds a field to
/// `Decision`, `TriggerEvent` or `TriggerSettings` (e.g. a mode or redaction switch) without
/// revisiting this test.
#[allow(dead_code, unused_variables)]
fn no_mode_or_redaction_fields(d: Decision, ev: TriggerEvent, s: TriggerSettings) {
    let Decision { needs_answer, intent, answer_tier, profile, urgency } = d;
    let TriggerEvent {
        answer_id,
        trigger,
        question,
        format,
        intent,
        tier,
        profile,
        urgency,
        decision,
        t_ms,
        speech_end_ms,
    } = ev;
    let TriggerSettings {
        mode,
        question_silence_ms,
        dedupe_window_ms,
        decision_timeout_ms,
        confidence_threshold,
        big_screen_change_lines,
        screen_settle_ms,
        passive_timer_ms,
        profiles,
        active_profile,
    } = s;
}

#[tokio::test(start_paused = true)]
async fn decisions_never_change_mode() {
    for mode in [Mode::Cloud, Mode::Local] {
        let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "support")));
        let (e, clock) = engine(mode, d);
        let before = e.settings();
        for q in ["How much is it?", "When can we start?", "How does SSO work?"] {
            ask(&e, &clock, q).await;
            e.hotkey();
            clock.advance(10_000);
        }
        tokio::task::yield_now().await;
        assert_eq!(e.mode(), mode);
        assert_eq!(e.settings(), before);
    }
}

// ---------- pure helpers ----------

#[test]
fn intent_to_format_table() {
    assert_eq!(format_for(Intent::Technical), Some(AnswerFormat::Cue));
    assert_eq!(format_for(Intent::None), Some(AnswerFormat::Cue));
    assert_eq!(format_for(Intent::Pricing), Some(AnswerFormat::Fact));
    assert_eq!(format_for(Intent::Scheduling), Some(AnswerFormat::Fact));
    assert_eq!(format_for(Intent::Objection), Some(AnswerFormat::Objection));
    assert_eq!(format_for(Intent::SmallTalk), None);
}

#[test]
fn decision_input_stays_under_500_tokens() {
    let question = format!("{} so how much is the enterprise plan?", "blah ".repeat(2000));
    let screen = "é".repeat(5000);
    let profiles: Vec<String> = (0..100).map(|i| format!("profile-number-{i}")).collect();
    let input = decision_input(&question, &screen, &profiles);
    assert!(estimate_tokens(&input) < 500, "{}", estimate_tokens(&input));
    assert!(input.question.ends_with("how much is the enterprise plan?"));
    assert!(!input.screen_diff.is_empty());
    assert!(!input.profiles.is_empty());
    // Small inputs pass through untouched.
    let small = decision_input(" How much? ", "Plan: Pro", &["sales".into()]);
    assert_eq!((small.question.as_str(), small.screen_diff.as_str()), ("How much?", "Plan: Pro"));
}

// ---------- speculation (H2), hotkey supersede (L6), no discarded tags (M5) ----------

fn recorder() -> (SpeculationFn, Arc<std::sync::Mutex<Vec<Speculation>>>) {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let l2 = log.clone();
    (Arc::new(move |s| l2.lock().unwrap().push(s)), log)
}

#[tokio::test(start_paused = true)]
async fn speculation_starts_before_the_decision_and_is_confirmed() {
    let d = Arc::new(
        MockDecision::always(decision_answer(Intent::Pricing, "support")).with_delay(Duration::from_millis(560)),
    );
    let (f, log) = recorder();
    let clock = MockClock::new(T0);
    let e = TriggerEngine::new(settings(Mode::Cloud), d.clone(), Arc::new(clock.clone())).with_speculation(f);
    let out = ask(&e, &clock, "How much is the enterprise plan?").await;
    assert_eq!(out.len(), 1);
    let log = log.lock().unwrap().clone();
    assert_eq!(log.len(), 1, "only Start; the confirm is the event itself: {log:?}");
    let Speculation::Start(spec) = &log[0] else { panic!("{log:?}") };
    assert_eq!(spec.answer_id, out[0].answer_id);
    assert_eq!((spec.format, spec.tier, spec.profile.as_str()), (AnswerFormat::Fact, Tier::Fast, "sales"));
    assert!(spec.decision.is_none());
    // The decision may change the profile: the pipeline sees the difference and restarts.
    assert_eq!(out[0].profile, "support");
}

#[tokio::test(start_paused = true)]
async fn speculation_is_cancelled_when_the_decision_says_no() {
    let d = Arc::new(MockDecision::always(decision_no_answer("sales")));
    let (f, log) = recorder();
    let clock = MockClock::new(T0);
    let e = TriggerEngine::new(settings(Mode::Cloud), d, Arc::new(clock.clone())).with_speculation(f);
    assert!(ask(&e, &clock, "How much is the enterprise plan?").await.is_empty());
    let log = log.lock().unwrap().clone();
    let [Speculation::Start(s), Speculation::Cancel(id)] = log.as_slice() else { panic!("{log:?}") };
    assert_eq!(s.answer_id, *id);
}

#[tokio::test(start_paused = true)]
async fn no_speculation_for_small_talk_screen_or_local_mode() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let (f, log) = recorder();
    let clock = MockClock::new(T0);
    let e = TriggerEngine::new(settings(Mode::Cloud), d.clone(), Arc::new(clock.clone())).with_speculation(f.clone());
    let _ = ask(&e, &clock, "How you dey").await; // small talk: the rules say nothing to show
    assert!(log.lock().unwrap().is_empty());
    let e = TriggerEngine::new(settings(Mode::Local), d, Arc::new(clock.clone())).with_speculation(f);
    clock.advance(10_000);
    assert_eq!(ask(&e, &clock, "How much is the enterprise plan?").await.len(), 1);
    assert!(log.lock().unwrap().is_empty(), "local mode fires at once; nothing to overlap");
}

/// L6: the hotkey pressed while the automatic decision for the same utterance is in flight gives
/// one answer, not two.
#[tokio::test(start_paused = true)]
async fn hotkey_during_in_flight_decision_supersedes_it() {
    let d = Arc::new(
        MockDecision::always(decision_answer(Intent::Pricing, "sales")).with_delay(Duration::from_millis(500)),
    );
    let (f, log) = recorder();
    let clock = MockClock::new(T0);
    let e = Arc::new(TriggerEngine::new(settings(Mode::Cloud), d, Arc::new(clock.clone())).with_speculation(f));
    assert!(e.on_transcript(&them_end(&clock, "How much is the enterprise plan?")).await.is_empty());
    clock.advance(e.settings().question_silence_ms);
    let e2 = e.clone();
    let auto = tokio::spawn(async move { e2.poll().await });
    tokio::time::sleep(Duration::from_millis(100)).await; // decision in flight
    let hot = e.hotkey();
    assert_eq!(hot.question, "How much is the enterprise plan?");
    assert!(auto.await.unwrap().is_empty(), "the automatic answer is dropped");
    let log = log.lock().unwrap().clone();
    assert!(matches!(log.as_slice(), [Speculation::Start(_), Speculation::Cancel(_)]), "{log:?}");
}

/// M5: local mode without a tag consumer runs no background decision at all.
#[tokio::test(start_paused = true)]
async fn local_mode_without_a_tag_sink_runs_no_decision() {
    let d = Arc::new(MockDecision::always(decision_answer(Intent::Pricing, "sales")));
    let (e, clock) = engine(Mode::Local, d.clone());
    assert_eq!(ask(&e, &clock, "How much is the enterprise plan?").await.len(), 1);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(d.calls(), 0);
}
