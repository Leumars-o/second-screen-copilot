//! The slot holding the running system.
//!
//! H3: hotkeys and `get_state` must never wait behind a slow operation (pipeline shutdown with
//! its post-call summary, pairing waiting up to 90 s for LAN TLS). The slot's lock is a plain
//! `std::sync::Mutex` held only to read, take or replace the `Arc` — its guard is `!Send`, so it
//! cannot be held across an `.await` in a Tauri command or spawned task (that would not compile).
//! Callers clone the `Arc` out and await on the clone.
//!
//! Start/stop/restart are serialised separately by [`Slot::lifecycle`], which readers never take.

use std::sync::{Arc, Mutex};

pub struct Slot<T> {
    cur: Mutex<Option<Arc<T>>>,
    lifecycle: tokio::sync::Mutex<()>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self { cur: Mutex::new(None), lifecycle: tokio::sync::Mutex::new(()) }
    }
}

impl<T> Slot<T> {
    /// The running value, if any. Never waits on anything but a pointer copy.
    pub fn get(&self) -> Option<Arc<T>> {
        self.cur.lock().unwrap().clone()
    }

    /// Take the value out (stop). The caller shuts it down after the lock is released.
    pub fn take(&self) -> Option<Arc<T>> {
        self.cur.lock().unwrap().take()
    }

    /// Install a new value; returns the previous one (to shut down) if there was one.
    pub fn replace(&self, v: Option<Arc<T>>) -> Option<Arc<T>> {
        std::mem::replace(&mut *self.cur.lock().unwrap(), v)
    }

    /// Serialises start/stop/restart against each other (not against readers).
    pub async fn lifecycle(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lifecycle.lock().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::Instant;

    struct Sys {
        answered: std::sync::atomic::AtomicUsize,
    }
    impl Sys {
        fn answer_now(&self) {
            self.answered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        async fn slow_shutdown(&self) {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        async fn slow_pairing(&self) {
            tokio::time::sleep(Duration::from_secs(90)).await;
        }
    }

    fn slot() -> Arc<Slot<Sys>> {
        let s = Arc::new(Slot::default());
        s.replace(Some(Arc::new(Sys { answered: 0.into() })));
        s
    }

    /// The pattern `app.rs` uses for `stop_system`: take under the lifecycle lock, then await.
    async fn stop(s: &Slot<Sys>) {
        let _l = s.lifecycle().await;
        if let Some(sys) = s.take() {
            sys.slow_shutdown().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn readers_do_not_wait_for_a_slow_shutdown() {
        let s = slot();
        let s2 = s.clone();
        let stopping = tokio::spawn(async move { stop(&s2).await });
        tokio::task::yield_now().await;
        let t = Instant::now();
        // get_state / on_hotkey: immediate, and see "not running" while it shuts down.
        assert!(s.get().is_none());
        assert_eq!(t.elapsed(), Duration::ZERO);
        assert!(!stopping.is_finished());
        stopping.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn hotkey_works_while_pairing_waits() {
        let s = slot();
        let pairing = {
            let sys = s.get().unwrap();
            tokio::spawn(async move { sys.slow_pairing().await })
        };
        tokio::task::yield_now().await;
        let t = Instant::now();
        let sys = s.get().expect("still running");
        sys.answer_now();
        assert_eq!(t.elapsed(), Duration::ZERO, "the hotkey did not wait for pairing");
        assert_eq!(sys.answered.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!pairing.is_finished());
        pairing.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn lifecycle_serialises_stop_and_start() {
        let s = slot();
        let s2 = s.clone();
        let stopping = tokio::spawn(async move { stop(&s2).await });
        tokio::task::yield_now().await;
        let t = Instant::now();
        {
            let _l = s.lifecycle().await; // a restart's start half waits for the stop to finish
            assert!(t.elapsed() >= Duration::from_secs(30));
            assert!(s.replace(Some(Arc::new(Sys { answered: 0.into() }))).is_none());
        }
        stopping.await.unwrap();
        assert!(s.get().is_some());
    }
}
