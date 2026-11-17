//! A [`Clock`] that follows `tokio::time`, so tests running with paused time get unix-ms
//! timestamps that advance with the runtime (unlike `MockClock`, which needs manual steps, or
//! `SystemClock`, which ignores the paused runtime).

use ssc_core::traits::Clock;
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct TokioClock {
    base_ms: u64,
    start: Instant,
}

impl TokioClock {
    /// `now_ms()` returns `base_ms` at creation, then advances with `tokio::time`.
    /// Must be created inside a tokio runtime.
    pub fn new(base_ms: u64) -> Self {
        Self { base_ms, start: Instant::now() }
    }
}

impl Clock for TokioClock {
    fn now_ms(&self) -> u64 {
        self.base_ms + self.start.elapsed().as_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn follows_paused_time() {
        let c = TokioClock::new(1_000);
        assert_eq!(c.now_ms(), 1_000);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(c.now_ms(), 1_250);
    }
}
