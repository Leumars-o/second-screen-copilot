//! Answer routing: an ordered failover chain of answer models plus an optional strong tier.
//! Every rule here is plain code (Build Sheet §3 rule 7):
//!
//! - **Local-only**: the router refuses to hold any cloud model ([`AnswerRouter::new`] fails with
//!   `Policy`), and re-checks before every call.
//! - **Redaction**: when enabled, every prompt is redacted before it reaches a cloud model.
//! - **Failover**: a route with `first_token_ms` that sends no token in time, or any route that fails
//!   with `RateLimited` / `Timeout` / `Unavailable` *before its first token*, hands over to the next
//!   route. `Policy` (and every other error) never fails over.
//! - **Budget**: a route with a [`BudgetTracker`] is skipped when the call would cross its cap; a 429
//!   puts it in cool-down.
//! - **Strong tier**: the fast chain streams the cues; at the `---` separator it is stopped and the
//!   strong model streams the `detail` block from a prompt built from those cues.
//! - **Cancellation**: the caller's token aborts the in-flight provider task at once (no awaiting
//!   the provider), so a cancel returns well within 50 ms.

use crate::budget::{BudgetTracker, estimate_tokens};
use crate::grammar::{CueStream, ParsedAnswer, merge_detail, parse_answer};
use ssc_core::decision::Tier;
use ssc_core::protocol::{Block, Mode};
use ssc_core::traits::{AnswerModel, Cancel};
use ssc_core::types::{Prompt, StreamStats, Token};
use ssc_core::{Result, SscError};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// Speculative answers start only while the first fast route's budget is below this fraction of
/// every cap (see [`AnswerRouter::speculation_allowed`]).
pub const SPECULATION_BUDGET_FRACTION: f64 = 0.6;

/// One entry of a failover chain.
#[derive(Clone)]
pub struct Route {
    pub model: Arc<dyn AnswerModel>,
    /// Fail over when no token arrives within this many ms of the request.
    pub first_token_ms: Option<u64>,
    /// Requests are reserved against this budget before the call.
    pub budget: Option<Arc<BudgetTracker>>,
}

impl Route {
    pub fn new(model: Arc<dyn AnswerModel>) -> Self {
        Self { model, first_token_ms: None, budget: None }
    }
    pub fn first_token_ms(mut self, ms: u64) -> Self {
        self.first_token_ms = Some(ms);
        self
    }
    pub fn budget(mut self, b: Arc<BudgetTracker>) -> Self {
        self.budget = Some(b);
        self
    }
}

/// A classified piece of answer text, ready to become an `answer.delta` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedDelta {
    pub block: Block,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnswerOutcome {
    pub parsed: ParsedAnswer,
    pub fast: StreamStats,
    pub strong: Option<StreamStats>,
}

/// Errors that move on to the next route (only before the first token).
pub fn fails_over(e: &SscError) -> bool {
    matches!(e, SscError::RateLimited { .. } | SscError::Timeout(_) | SscError::Unavailable(_))
}

pub struct AnswerRouter {
    mode: Mode,
    redaction: Option<ssc_redact::Options>,
    fast: Vec<Route>,
    strong: Option<Route>,
}

impl AnswerRouter {
    /// `redaction: None` disables redaction. Fails with `Policy` if `mode` is local and any route is cloud.
    pub fn new(
        mode: Mode,
        redaction: Option<ssc_redact::Options>,
        fast: Vec<Route>,
        strong: Option<Route>,
    ) -> Result<Self> {
        if mode == Mode::Local
            && let Some(r) = fast.iter().chain(strong.iter()).find(|r| r.model.is_cloud())
        {
            return Err(SscError::Policy(format!("local-only mode cannot use cloud model {}", r.model.id())));
        }
        if fast.is_empty() {
            return Err(SscError::Invalid("no answer model configured".into()));
        }
        Ok(Self { mode, redaction, fast, strong })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Every model this router can call (fast chain, then strong).
    pub fn models(&self) -> Vec<Arc<dyn AnswerModel>> {
        self.fast.iter().chain(self.strong.iter()).map(|r| r.model.clone()).collect()
    }

    pub fn has_strong(&self) -> bool {
        self.strong.is_some()
    }

    /// True when a speculative answer may start (H2): the first fast route has no budget, or its
    /// budget is below [`SPECULATION_BUDGET_FRACTION`] of every cap and not cooling down. A
    /// speculation that the decision cancels still costs a provider request, so it is skipped
    /// well before the budget gets tight; the confirmed answer then starts after the decision.
    pub fn speculation_allowed(&self) -> bool {
        match self.fast.first().and_then(|r| r.budget.as_ref()) {
            Some(b) => b.below(SPECULATION_BUDGET_FRACTION),
            None => true,
        }
    }

    /// The fast-chain route ids, in order (the first one is the model expected to answer).
    pub fn primary_model(&self) -> &str {
        self.fast.first().map(|r| r.model.id()).unwrap_or("unknown")
    }

    /// Stream raw tokens from the fast chain (no grammar handling).
    pub async fn stream(&self, prompt: &Prompt, tx: mpsc::Sender<Token>, cancel: &Cancel) -> Result<StreamStats> {
        self.run_chain(&self.fast, prompt, tx, cancel, &Cancel::new(), None).await
    }

    /// Full answer: classified deltas to `tx`, parsed card in the result.
    ///
    /// `Tier::Strong` with a strong model configured: fast cues first, then the strong model streams
    /// the detail from `detail_prompt(cues)` (typically `expand.v1` with the cues as `{{answer}}`).
    /// Without a strong model, or for `Fast` / `None` (the hotkey always answers), the fast chain
    /// writes the whole answer.
    pub async fn answer<F>(
        &self,
        tier: Tier,
        prompt: &Prompt,
        detail_prompt: F,
        tx: mpsc::Sender<RoutedDelta>,
        cancel: &Cancel,
    ) -> Result<AnswerOutcome>
    where
        F: FnOnce(&[String]) -> Prompt + Send,
    {
        self.answer_reporting(tier, prompt, detail_prompt, tx, cancel, None).await
    }

    /// [`Self::answer`], and `answering` is set to the id of the fast-chain model that produced
    /// the first token, before that token is forwarded (L3: `answer.start.model` names the model
    /// that actually answers, after any failover).
    pub async fn answer_reporting<F>(
        &self,
        tier: Tier,
        prompt: &Prompt,
        detail_prompt: F,
        tx: mpsc::Sender<RoutedDelta>,
        cancel: &Cancel,
        answering: Option<&watch::Sender<Option<String>>>,
    ) -> Result<AnswerOutcome>
    where
        F: FnOnce(&[String]) -> Prompt + Send,
    {
        let strong = (tier == Tier::Strong).then_some(()).and(self.strong.as_ref());
        let stop = Cancel::new();
        let (ftx, mut frx) = mpsc::channel::<Token>(64);
        let mut cls = CueStream::new(Block::Cue);
        let fast = self.run_chain(&self.fast, prompt, ftx, cancel, &stop, answering);
        tokio::pin!(fast);
        let mut fast_res = None;
        loop {
            tokio::select! {
                biased;
                t = frx.recv() => match t {
                    Some(tok) => {
                        if strong.is_some() && cls.block() == Block::Detail {
                            continue; // the strong model writes the detail
                        }
                        for (block, text) in cls.push(&tok.text) {
                            if strong.is_some() && block == Block::Detail {
                                continue;
                            }
                            send(&tx, RoutedDelta { block, text }, cancel).await?;
                        }
                        if strong.is_some() && cls.block() == Block::Detail {
                            stop.cancel();
                        }
                    }
                    None => break,
                },
                r = &mut fast, if fast_res.is_none() => fast_res = Some(r),
            }
        }
        let fast_stats = match fast_res {
            Some(r) => r?,
            None => fast.await?,
        };
        for (block, text) in cls.finish() {
            if strong.is_none() || block == Block::Cue {
                send(&tx, RoutedDelta { block, text }, cancel).await?;
            }
        }
        let parsed = parse_answer(cls.text());

        let Some(strong) = strong else {
            return Ok(AnswerOutcome { parsed, fast: fast_stats, strong: None });
        };
        let cues: Vec<String> = parsed.cues.iter().map(|c| c.text.clone()).collect();
        let dprompt = detail_prompt(&cues);
        let (detail_text, strong_stats) = match self.detail(std::slice::from_ref(strong), &dprompt, &tx, cancel).await {
            Ok(r) => r,
            // Strong model unavailable before its first token: the fast chain writes the detail.
            Err(e) if fails_over(&e) => self.detail(&self.fast, &dprompt, &tx, cancel).await?,
            Err(e) => return Err(e),
        };
        Ok(AnswerOutcome { parsed: merge_detail(parsed, &detail_text), fast: fast_stats, strong: Some(strong_stats) })
    }

    async fn detail(
        &self,
        routes: &[Route],
        prompt: &Prompt,
        tx: &mpsc::Sender<RoutedDelta>,
        cancel: &Cancel,
    ) -> Result<(String, StreamStats)> {
        let (stx, mut srx) = mpsc::channel::<Token>(64);
        let mut cls = CueStream::new(Block::Detail);
        let never = Cancel::new();
        let run = self.run_chain(routes, prompt, stx, cancel, &never, None);
        tokio::pin!(run);
        let mut res = None;
        loop {
            tokio::select! {
                biased;
                t = srx.recv() => match t {
                    Some(tok) => for (block, text) in cls.push(&tok.text) {
                        send(tx, RoutedDelta { block, text }, cancel).await?;
                    },
                    None => break,
                },
                r = &mut run, if res.is_none() => res = Some(r),
            }
        }
        let stats = match res {
            Some(r) => r?,
            None => run.await?,
        };
        for (block, text) in cls.finish() {
            send(tx, RoutedDelta { block, text }, cancel).await?;
        }
        Ok((cls.text().to_string(), stats))
    }

    /// Run `routes` in order until one streams. `stop` ends a stream that has started, returning
    /// its stats so far; `cancel` aborts with `Cancelled`. `tx` is dropped when this returns.
    async fn run_chain(
        &self,
        routes: &[Route],
        prompt: &Prompt,
        tx: mpsc::Sender<Token>,
        cancel: &Cancel,
        stop: &Cancel,
        answering: Option<&watch::Sender<Option<String>>>,
    ) -> Result<StreamStats> {
        let mut last_err: Option<SscError> = None;
        for route in routes {
            if cancel.is_cancelled() {
                return Err(SscError::Cancelled);
            }
            let model = route.model.clone();
            let cloud = model.is_cloud();
            if cloud && self.mode == Mode::Local {
                return Err(SscError::Policy(format!("local-only mode: refusing cloud model {}", model.id())));
            }
            if let Some(b) = &route.budget
                && !b.try_reserve(estimate_tokens(prompt))
            {
                last_err = Some(SscError::RateLimited { retry_after_ms: None });
                continue;
            }
            let p = match (cloud, self.redaction) {
                (true, Some(o)) => ssc_redact::redact_prompt(prompt, o).0,
                _ => prompt.clone(),
            };
            let id = model.id().to_string();
            let child = cancel.child_token();
            let (itx, mut irx) = mpsc::channel::<Token>(64);
            let start = Instant::now();
            let c2 = child.clone();
            let mut handle = tokio::spawn(async move { model.stream(&p, itx, c2).await });
            let abort = |h: &tokio::task::JoinHandle<_>| {
                child.cancel();
                h.abort();
            };

            let deadline = async {
                match route.first_token_ms {
                    Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(deadline);
            let first = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    abort(&handle);
                    return Err(SscError::Cancelled);
                }
                t = irx.recv() => t,
                _ = &mut deadline => {
                    abort(&handle);
                    last_err = Some(SscError::Timeout(route.first_token_ms.unwrap_or_default()));
                    continue;
                }
            };

            let Some(tok) = first else {
                // Finished without a token: success with an empty answer, or an error.
                let res = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        abort(&handle);
                        return Err(SscError::Cancelled);
                    }
                    r = &mut handle => r.unwrap_or_else(|_| Err(SscError::Unavailable("answer task failed".into()))),
                };
                match res {
                    Ok(stats) => return Ok(stats),
                    Err(e) => {
                        if let (SscError::RateLimited { retry_after_ms }, Some(b)) = (&e, &route.budget) {
                            b.mark_rate_limited(*retry_after_ms);
                        }
                        if fails_over(&e) {
                            last_err = Some(e);
                            continue;
                        }
                        return Err(e);
                    }
                }
            };

            let first_ms = start.elapsed().as_millis() as u64;
            if let Some(w) = answering {
                w.send_replace(Some(id.clone()));
            }
            let mut n = 1u32;
            let partial = |n: u32| StreamStats {
                model: id.clone(),
                first_token_ms: Some(first_ms),
                total_ms: start.elapsed().as_millis() as u64,
                tokens_out: n,
            };
            if send(&tx, tok, cancel).await.is_err() {
                abort(&handle);
                return Err(SscError::Cancelled);
            }
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        abort(&handle);
                        return Err(SscError::Cancelled);
                    }
                    _ = stop.cancelled() => {
                        abort(&handle);
                        return Ok(partial(n));
                    }
                    t = irx.recv() => match t {
                        Some(t) => {
                            n += 1;
                            if send(&tx, t, cancel).await.is_err() {
                                abort(&handle);
                                return Err(SscError::Cancelled);
                            }
                        }
                        None => break,
                    },
                }
            }
            let res = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    abort(&handle);
                    return Err(SscError::Cancelled);
                }
                r = &mut handle => r.unwrap_or_else(|_| Err(SscError::Unavailable("answer task failed".into()))),
            };
            // An error after the first token can't fail over: text is already on the phone. A
            // mid-stream stall (idle timeout, M3) ends the answer cleanly with what arrived.
            return match res {
                Err(SscError::Timeout(_)) => Ok(partial(n)),
                r => r,
            };
        }
        Err(last_err.unwrap_or_else(|| SscError::Unavailable("no answer model available".into())))
    }
}

/// Send unless cancelled or the receiver is gone.
async fn send<T>(tx: &mpsc::Sender<T>, v: T, cancel: &Cancel) -> Result<()> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(SscError::Cancelled),
        r = tx.send(v) => r.map_err(|_| SscError::Cancelled),
    }
}
