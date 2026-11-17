//! Splits one audio stream into one worker per speaker, so "me" and "them" never share a VAD,
//! a buffer or a provider connection.

use ssc_core::protocol::Speaker;
use ssc_core::types::AudioChunk;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Per-speaker queue depth. 20 ms chunks → 10 s of audio.
pub const WORKER_QUEUE: usize = 500;

/// Reads `rx` and forwards each chunk to the worker for its speaker, creating workers on first
/// use via `spawn_worker` (which must start the worker and return its input). Workers see their
/// input close when `rx` closes or `cancel` fires.
pub fn route_by_speaker<F>(mut rx: mpsc::Receiver<AudioChunk>, cancel: CancellationToken, mut spawn_worker: F)
where
    F: FnMut(Speaker) -> mpsc::Sender<AudioChunk> + Send + 'static,
{
    tokio::spawn(async move {
        let mut workers: HashMap<Speaker, mpsc::Sender<AudioChunk>> = HashMap::new();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => break,
                c = rx.recv() => match c { Some(c) => c, None => break },
            };
            let speaker = chunk.speaker;
            let tx = workers.entry(speaker).or_insert_with(|| spawn_worker(speaker));
            if tx.send(chunk).await.is_err() {
                // Worker gone (e.g. cancelled); drop its audio from now on.
                workers.remove(&speaker);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn speakers_never_mixed() {
        let (tx, rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        type Seen = Arc<Mutex<Vec<(Speaker, Vec<AudioChunk>)>>>;
        let seen: Seen = Arc::default();
        let (done_tx, mut done_rx) = mpsc::channel::<()>(4);
        let seen2 = seen.clone();
        route_by_speaker(rx, cancel.clone(), move |sp| {
            let (wtx, mut wrx) = mpsc::channel::<AudioChunk>(16);
            let seen = seen2.clone();
            let done = done_tx.clone();
            tokio::spawn(async move {
                let mut got = Vec::new();
                while let Some(c) = wrx.recv().await {
                    got.push(c);
                }
                seen.lock().unwrap().push((sp, got));
                let _ = done.send(()).await;
            });
            wtx
        });
        for i in 0..50u64 {
            let speaker = if i % 3 == 0 { Speaker::Me } else { Speaker::Them };
            tx.send(AudioChunk { speaker, t0_ms: i, samples: vec![i as i16; 4] }).await.unwrap();
        }
        drop(tx);
        done_rx.recv().await.unwrap();
        done_rx.recv().await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        for (sp, chunks) in seen.iter() {
            assert!(chunks.iter().all(|c| c.speaker == *sp));
            let want = if *sp == Speaker::Me { 17 } else { 33 };
            assert_eq!(chunks.len(), want);
            // Order preserved per speaker.
            assert!(chunks.windows(2).all(|w| w[0].t0_ms < w[1].t0_ms));
        }
    }
}
