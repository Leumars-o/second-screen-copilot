//! Groq free-tier budget tracker (requests and tokens, per minute and per day). The router asks
//! [`BudgetTracker::try_reserve`] before every Groq call and goes straight to the fallback when the
//! call would cross `safety × limit`, so the provider limit itself is never hit. A real 429 puts
//! the tracker in cool-down for `retry-after` (default 60 s).
//!
//! Windows are rolling (last 60 s, last 24 h), measured with the injected [`Clock`].

use ssc_core::config::GroqBudget;
use ssc_core::traits::Clock;
use ssc_core::types::Prompt;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub const MINUTE_MS: u64 = 60_000;
pub const DAY_MS: u64 = 86_400_000;
/// Switch to the fallback at 90 % of every limit.
pub const DEFAULT_SAFETY: f64 = 0.9;
pub const DEFAULT_COOLDOWN_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub requests_min: u32,
    pub requests_day: u32,
    pub tokens_min: u32,
    pub tokens_day: u32,
}

pub struct BudgetTracker {
    limits: GroqBudget,
    safety: f64,
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// (time ms, tokens) per reserved request.
    events: VecDeque<(u64, u32)>,
    cooldown_until: u64,
}

/// Conservative token estimate for one call: ~4 bytes per input token plus the full output cap.
pub fn estimate_tokens(p: &Prompt) -> u32 {
    let input = (p.prefix.len() + p.dynamic.len()).div_ceil(4) as u32;
    let images = IMAGE_TOKENS_ESTIMATE.saturating_mul(p.images.len() as u32);
    input.saturating_add(images).saturating_add(p.max_tokens)
}

/// Rough input-token cost of one screenshot (≤1600 px long edge) for the rate budget (CR-20).
pub const IMAGE_TOKENS_ESTIMATE: u32 = 1_600;

impl BudgetTracker {
    pub fn new(limits: GroqBudget, clock: Arc<dyn Clock>) -> Self {
        Self::with_safety(limits, clock, DEFAULT_SAFETY)
    }

    pub fn with_safety(limits: GroqBudget, clock: Arc<dyn Clock>, safety: f64) -> Self {
        Self { limits, safety: safety.clamp(0.0, 1.0), clock, inner: Mutex::new(Inner::default()) }
    }

    fn cap(&self, limit: u32) -> u64 {
        // 0 means "no limit configured".
        if limit == 0 { u64::MAX } else { (f64::from(limit) * self.safety).floor() as u64 }
    }

    fn usage_at(inner: &Inner, now: u64) -> Usage {
        let mut u = Usage::default();
        for &(t, tok) in &inner.events {
            if now.saturating_sub(t) < DAY_MS {
                u.requests_day += 1;
                u.tokens_day = u.tokens_day.saturating_add(tok);
            }
            if now.saturating_sub(t) < MINUTE_MS {
                u.requests_min += 1;
                u.tokens_min = u.tokens_min.saturating_add(tok);
            }
        }
        u
    }

    pub fn usage(&self) -> Usage {
        let inner = self.inner.lock().unwrap();
        Self::usage_at(&inner, self.clock.now_ms())
    }

    /// Reserve one request of `tokens`. Returns false (and records nothing) when it would cross a
    /// safety cap or the tracker is cooling down after a 429.
    pub fn try_reserve(&self, tokens: u32) -> bool {
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().unwrap();
        while inner.events.front().is_some_and(|&(t, _)| now.saturating_sub(t) >= DAY_MS) {
            inner.events.pop_front();
        }
        if now < inner.cooldown_until {
            return false;
        }
        let u = Self::usage_at(&inner, now);
        let l = &self.limits;
        let ok = u64::from(u.requests_min) < self.cap(l.requests_per_min)
            && u64::from(u.requests_day) < self.cap(l.requests_per_day)
            && u64::from(u.tokens_min) + u64::from(tokens) <= self.cap(l.tokens_per_min)
            && u64::from(u.tokens_day) + u64::from(tokens) <= self.cap(l.tokens_per_day);
        if ok {
            inner.events.push_back((now, tokens));
        }
        ok
    }

    /// True when usage is below `fraction` of every configured limit and no 429 cool-down is
    /// running. Used to decide whether optional (speculative) calls may spend budget.
    pub fn below(&self, fraction: f64) -> bool {
        let now = self.clock.now_ms();
        let inner = self.inner.lock().unwrap();
        if now < inner.cooldown_until {
            return false;
        }
        let u = Self::usage_at(&inner, now);
        let l = &self.limits;
        let under = |used: u32, limit: u32| limit == 0 || f64::from(used) < fraction * f64::from(limit);
        under(u.requests_min, l.requests_per_min)
            && under(u.requests_day, l.requests_per_day)
            && under(u.tokens_min, l.tokens_per_min)
            && under(u.tokens_day, l.tokens_per_day)
    }

    /// The provider said 429: stop using it until `retry_after` (or the default cool-down) passes.
    pub fn mark_rate_limited(&self, retry_after_ms: Option<u64>) {
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().unwrap();
        inner.cooldown_until = now + retry_after_ms.unwrap_or(DEFAULT_COOLDOWN_MS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::mock::MockClock;

    fn tracker(limits: GroqBudget) -> (BudgetTracker, MockClock) {
        let c = MockClock::new(1_000_000);
        (BudgetTracker::new(limits, Arc::new(c.clone())), c)
    }

    #[test]
    fn requests_per_minute_switch_before_limit() {
        let (b, c) = tracker(GroqBudget::default()); // 30 rpm → cap 27
        for i in 0..27 {
            assert!(b.try_reserve(10), "request {i}");
        }
        assert!(!b.try_reserve(10), "28th request is refused: 90% of 30");
        assert_eq!(b.usage().requests_min, 27);
        c.advance(MINUTE_MS);
        assert!(b.try_reserve(10), "minute window rolled over");
    }

    #[test]
    fn tokens_per_minute() {
        let (b, c) = tracker(GroqBudget::default()); // 8000 tpm → cap 7200
        assert!(b.try_reserve(7000));
        assert!(!b.try_reserve(201));
        assert!(b.try_reserve(200));
        c.advance(MINUTE_MS - 1);
        assert!(!b.try_reserve(1));
        c.advance(1);
        assert!(b.try_reserve(7200));
    }

    #[test]
    fn per_day_limits() {
        let limits = GroqBudget { requests_per_min: 0, requests_per_day: 10, tokens_per_min: 0, tokens_per_day: 1_000 };
        let (b, c) = tracker(limits);
        for _ in 0..9 {
            assert!(b.try_reserve(10));
            c.advance(MINUTE_MS);
        }
        assert!(!b.try_reserve(10), "cap 9 of 10 requests/day");
        c.advance(DAY_MS);
        assert!(b.try_reserve(890));
        assert!(!b.try_reserve(11), "cap 900 of 1000 tokens/day");
        assert_eq!(b.usage().tokens_day, 890);
    }

    #[test]
    fn cooldown_after_429() {
        let (b, c) = tracker(GroqBudget::default());
        b.mark_rate_limited(Some(2_000));
        assert!(!b.try_reserve(1));
        c.advance(1_999);
        assert!(!b.try_reserve(1));
        c.advance(1);
        assert!(b.try_reserve(1));
        b.mark_rate_limited(None);
        c.advance(DEFAULT_COOLDOWN_MS - 1);
        assert!(!b.try_reserve(1));
    }

    #[test]
    fn below_fraction_for_optional_calls() {
        let (b, c) = tracker(GroqBudget::default()); // 30 rpm, 8000 tpm
        assert!(b.below(0.6));
        for _ in 0..17 {
            assert!(b.try_reserve(10));
        }
        assert!(b.below(0.6), "17 of 30 requests");
        assert!(b.try_reserve(10));
        assert!(!b.below(0.6), "18 of 30 = 60%");
        c.advance(MINUTE_MS);
        assert!(b.below(0.6));
        assert!(b.try_reserve(4_800));
        assert!(!b.below(0.6), "tokens at 60%");
        c.advance(MINUTE_MS);
        b.mark_rate_limited(Some(1000));
        assert!(!b.below(0.6), "cooling down");
    }

    #[test]
    fn estimate() {
        let p = Prompt {
            prefix: "a".repeat(400),
            dynamic: "b".repeat(3),
            max_tokens: 60,
            temperature: 0.0,
            stop: vec![],
            images: vec![],
        };
        assert_eq!(estimate_tokens(&p), 101 + 60);
    }
}
