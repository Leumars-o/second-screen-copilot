//! `bench ttft`: time to first token per answer model, measured on the caller's side (request
//! issued → first non-empty token on the channel), so every provider is timed the same way.
//!
//! Each target runs twice: **cached** (the same byte-stable prompt prefix every run, after one
//! discarded warm-up run) and **uncached** (a unique nonce at the start of the prefix every run, so
//! no provider or Ollama KV cache can reuse it). Local models are preloaded first (warm).
//!
//! All targets use the production adapters: `OpenAiCompat::groq` / `OpenAiCompat::gemini` and
//! `Ollama` (which sends `think: false` by default, so qwen3/gemma answer without thinking).

use crate::report::Report;
use crate::stats::Summary;
use anyhow::Result;
use serde_json::Value;
use ssc_context::{Budget, ContextInput, PromptKind, assemble};
use ssc_core::traits::{AnswerModel, Cancel};
use ssc_core::types::{AboutMe, Chunk, Profile, Prompt, Token};
use ssc_router::providers::Ollama;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Instant;
use uuid::Uuid;

// ---------- prompt ----------

/// A realistic cue prompt rendered by `ssc-context` from synthetic memory.
pub fn bench_prompt(local: bool) -> Prompt {
    let input = ContextInput {
        about_me: Some(AboutMe {
            name: "Ada Obi".into(),
            role: "Account executive".into(),
            company: "Northwind Analytics".into(),
            product: "Northwind Insights, a product-analytics SaaS".into(),
            tone: "warm, concise".into(),
            preferred_answers: "short cues".into(),
        }),
        profile: Some(Profile {
            id: "sales".into(),
            name: "Sales".into(),
            instructions: "Lead with value, then price. Never promise roadmap dates.".into(),
        }),
        chunks: vec![
            chunk(
                "pricing.md",
                "Team plan: $49 per seat per month, billed annually. Enterprise: custom, SSO included.",
            ),
            chunk("security.md", "SOC 2 Type II. Data stays in the EU region on request. SSO via SAML and OIDC."),
            chunk("onboarding.md", "Typical onboarding is two weeks with a named success manager."),
        ],
        screen: "Deal: Contoso — 40 seats — stage: proposal".into(),
        question: "What would 40 seats cost us per year, and is SSO included?".into(),
        ..ContextInput::default()
    };
    let budget = if local { Budget::local() } else { Budget::cloud() };
    assemble(PromptKind::Cue, &input, &budget).prompt
}

fn chunk(doc: &str, text: &str) -> Chunk {
    Chunk { id: Uuid::nil(), doc_id: Uuid::nil(), doc_name: doc.into(), text: text.into(), score: 1.0 }
}

/// Defeat prefix caching: a unique first line.
pub fn uncached(p: &Prompt) -> Prompt {
    Prompt { prefix: format!("[run {}]\n{}", Uuid::new_v4(), p.prefix), ..p.clone() }
}

// ---------- measurement ----------

#[derive(Debug, Clone)]
pub struct Sample {
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
    pub chunks: u32,
    pub error: Option<String>,
}

impl Sample {
    /// Stream chunks per second after the first token (≈ tokens/s for Ollama; providers that pack
    /// several tokens per chunk read low).
    pub fn chunks_per_s(&self) -> Option<f64> {
        let t = self.ttft_ms?;
        let decode = self.total_ms.saturating_sub(t);
        (self.chunks > 1 && decode > 0).then(|| (self.chunks - 1) as f64 * 1000.0 / decode as f64)
    }
}

pub async fn measure(model: &dyn AnswerModel, prompt: &Prompt) -> Sample {
    let (tx, mut rx) = mpsc::channel::<Token>(256);
    let start = Instant::now();
    let consume = async {
        let mut first = None;
        let mut n = 0u32;
        while let Some(t) = rx.recv().await {
            if t.text.is_empty() {
                continue;
            }
            first.get_or_insert(start.elapsed().as_millis() as u64);
            n += 1;
        }
        (first, n)
    };
    let (res, (first, n)) = tokio::join!(model.stream(prompt, tx, Cancel::new()), consume);
    Sample {
        ttft_ms: first,
        total_ms: start.elapsed().as_millis() as u64,
        chunks: n,
        error: res.err().map(|e| e.to_string()),
    }
}

// ---------- targets ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cache {
    Cached,
    Uncached,
}

pub struct Target {
    pub label: String,
    /// Which adapter produced the numbers.
    pub adapter: String,
    pub model: Arc<dyn AnswerModel>,
    pub local: bool,
    /// Loads the model before timing (Ollama `preload`). None for cloud.
    pub preload: Option<Arc<Ollama>>,
}

#[derive(Debug, Clone)]
pub struct TargetResult {
    pub label: String,
    pub adapter: String,
    pub cache: Cache,
    pub samples: Vec<Sample>,
}

impl TargetResult {
    pub fn ttft(&self) -> Option<Summary> {
        Summary::of(&self.samples.iter().filter_map(|s| s.ttft_ms).collect::<Vec<_>>())
    }
    pub fn chunks_per_s(&self) -> Option<f64> {
        crate::stats::mean_f64(&self.samples.iter().filter_map(Sample::chunks_per_s).collect::<Vec<_>>())
    }
    pub fn errors(&self) -> Vec<&str> {
        self.samples.iter().filter_map(|s| s.error.as_deref()).collect()
    }
}

/// Runs `runs` timed requests per cache variant (after one discarded warm-up each).
pub async fn run_target(t: &Target, runs: usize) -> Vec<TargetResult> {
    if let Some(o) = &t.preload {
        let _ = o.preload().await;
    }
    let base = bench_prompt(t.local);
    let mut out = Vec::new();
    for cache in [Cache::Cached, Cache::Uncached] {
        let prompt_for = |_: usize| if cache == Cache::Cached { base.clone() } else { uncached(&base) };
        let _ = measure(t.model.as_ref(), &prompt_for(0)).await; // warm-up: connection, model, cache
        let mut samples = Vec::with_capacity(runs);
        for i in 0..runs {
            samples.push(measure(t.model.as_ref(), &prompt_for(i)).await);
        }
        out.push(TargetResult { label: t.label.clone(), adapter: t.adapter.clone(), cache, samples });
    }
    out
}

pub fn render(r: &mut Report, results: &[TargetResult], skipped: &[(String, String)]) {
    let rows: Vec<Vec<String>> = results
        .iter()
        .map(|res| {
            let s = res.ttft();
            let errs = res.errors();
            vec![
                res.label.clone(),
                match res.cache {
                    Cache::Cached => "cached prefix".into(),
                    Cache::Uncached => "no cache".into(),
                },
                s.map(|s| s.p50.to_string()).unwrap_or("n/a".into()),
                s.map(|s| s.p95.to_string()).unwrap_or("n/a".into()),
                s.map(|s| format!("{}–{}", s.min, s.max)).unwrap_or("n/a".into()),
                res.chunks_per_s().map(|v| format!("{v:.1}")).unwrap_or("n/a".into()),
                format!("{}/{}", res.samples.len() - errs.len(), res.samples.len()),
                errs.first().map(|e| e.to_string()).unwrap_or_default(),
                res.adapter.clone(),
            ]
        })
        .collect();
    r.table(
        &["target", "prefix", "TTFT p50 ms", "TTFT p95 ms", "range ms", "chunks/s", "ok", "first error", "adapter"],
        &rows,
    );
    if !skipped.is_empty() {
        r.h2("Skipped");
        for (t, why) in skipped {
            r.bullet(&format!("{t}: {why}"));
        }
        r.end_list();
    }
}

// ---------- fixture mode ----------

fn router_fixture(name: &str) -> Result<String> {
    let p = crate::bench_path(&format!("../crates/ssc-router/tests/fixtures/{name}"));
    std::fs::read_to_string(&p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))
}

/// SSE body → one part per event; NDJSON → one part per line.
fn split_parts(body: &str, sse: bool) -> Vec<Vec<u8>> {
    if sse {
        body.split("\n\n").filter(|e| !e.trim().is_empty()).map(|e| format!("{e}\n\n").into_bytes()).collect()
    } else {
        body.lines().filter(|l| !l.trim().is_empty()).map(|l| format!("{l}\n").into_bytes()).collect()
    }
}

/// Fixture mode: the real Groq / Gemini / Ollama adapters against loopback fakes
/// replaying `crates/ssc-router/tests/fixtures` with scripted first-chunk delays.
pub async fn run_fixture(report_path: &Path, runs: usize) -> Result<(Report, Vec<TargetResult>)> {
    use crate::fake::{Reply, http};
    use ssc_core::secrets::Secret;
    use ssc_router::providers::{GeminiTier, OpenAiCompat};
    use std::time::Duration;

    let ms = Duration::from_millis;
    let groq = http(vec![Reply::stream(
        "text/event-stream",
        split_parts(&router_fixture("groq_stream.sse")?, true),
        ms(40),
        ms(3),
    )])
    .await?;
    let gemini = http(vec![Reply::stream(
        "text/event-stream",
        split_parts(&router_fixture("gemini_stream.sse")?, true),
        ms(60),
        ms(3),
    )])
    .await?;
    let ollama = http(vec![Reply::stream(
        "application/x-ndjson",
        split_parts(&router_fixture("ollama_chat.ndjson")?, false),
        ms(25),
        ms(3),
    )])
    .await?;

    let key = || Secret::new("fixture-key-not-real");
    let targets = vec![
        Target {
            label: "groq openai/gpt-oss-120b".into(),
            adapter: "ssc_router OpenAiCompat::groq".into(),
            model: Arc::new(OpenAiCompat::groq(&groq.url(), "openai/gpt-oss-120b", key())),
            local: false,
            preload: None,
        },
        Target {
            label: "gemini gemini-2.5-flash-lite".into(),
            adapter: "ssc_router OpenAiCompat::gemini".into(),
            model: Arc::new(OpenAiCompat::gemini(
                &gemini.url(),
                "gemini-2.5-flash-lite",
                key(),
                GeminiTier::Paid,
                false,
            )?),
            local: false,
            preload: None,
        },
        Target {
            label: "ollama gemma4:e2b".into(),
            adapter: "ssc_router Ollama".into(),
            model: Arc::new(Ollama::new(&ollama.url(), "gemma4:e2b")),
            local: true,
            preload: None,
        },
    ];
    let mut results = Vec::new();
    for t in &targets {
        results.extend(run_target(t, runs).await);
    }
    let mut r = Report::new("bench/ttft — fixture mode");
    r.para(
        "Real adapters against loopback fakes replaying recorded streams with scripted delays \
         (Groq 40 ms, Gemini 60 ms, Ollama 25 ms to the first chunk). Numbers prove the harness only.",
    );
    render(&mut r, &results, &[]);
    r.write(report_path)?;
    // Keep the fakes alive until the runs are done; also used by tests to inspect requests.
    let ollama_body = ollama.requests().last().map(|q| q.body.clone()).unwrap_or_default();
    let ollama_think_off = serde_json::from_slice::<Value>(&ollama_body).ok().and_then(|v| v.get("think").cloned());
    anyhow::ensure!(ollama_think_off == Some(Value::Bool(false)), "Ollama request did not carry think=false");
    drop((groq, gemini));
    Ok((r, results))
}

// ---------- live mode ----------

#[cfg(feature = "live")]
pub mod live {
    use super::*;
    use crate::live::{Keys, has_model, load_config, ollama_models};
    use ssc_core::secrets::SecretKey;
    use ssc_router::providers::OpenAiCompat;
    use ssc_router::providers::openai::GEMINI_BASE_URL;
    use ssc_router::providers::openai::GROQ_BASE_URL;

    pub struct LiveOpts {
        pub runs: usize,
        pub groq_model: Option<String>,
        pub ollama_models: Vec<String>,
    }

    pub async fn run(report_path: &Path, o: &LiveOpts) -> Result<Report> {
        let cfg = load_config();
        let keys = Keys;
        let mut targets = Vec::new();
        let mut skipped = Vec::new();

        let groq_model = o.groq_model.clone().unwrap_or(cfg.models.groq_model.clone());
        match keys.get(SecretKey::Groq) {
            Some(k) => {
                let m = OpenAiCompat::groq(GROQ_BASE_URL, &groq_model, k);
                m.warm().await;
                targets.push(Target {
                    label: format!("groq {groq_model}"),
                    adapter: "ssc_router OpenAiCompat::groq".into(),
                    model: Arc::new(m),
                    local: false,
                    preload: None,
                });
            }
            None => skipped.push((format!("groq {groq_model}"), "no `api.groq` key in the keychain".into())),
        }
        let gm = cfg.models.gemini_model.clone();
        match keys.get(SecretKey::Gemini) {
            Some(k) => match OpenAiCompat::gemini(GEMINI_BASE_URL, &gm, k, cfg.models.gemini_tier, cfg.testing) {
                Ok(g) => {
                    g.warm().await;
                    targets.push(Target {
                        label: format!("gemini {gm}"),
                        adapter: "ssc_router OpenAiCompat::gemini".into(),
                        model: Arc::new(g),
                        local: false,
                        preload: None,
                    });
                }
                Err(e) => skipped.push((format!("gemini {gm}"), e.to_string())),
            },
            None => skipped.push((format!("gemini {gm}"), "no `api.gemini` key in the keychain".into())),
        }
        let url = cfg.models.ollama_url.clone();
        match ollama_models(&url).await {
            Ok(have) => {
                for m in &o.ollama_models {
                    if !has_model(&have, m) {
                        skipped.push((format!("ollama {m}"), format!("not pulled (`ollama pull {m}`)")));
                        continue;
                    }
                    targets.push(Target {
                        label: format!("ollama {m}"),
                        adapter: "ssc_router Ollama (think off)".into(),
                        model: Arc::new(Ollama::new(&url, m)),
                        local: true,
                        preload: Some(Arc::new(Ollama::new(&url, m))),
                    });
                }
            }
            Err(e) => skipped.push(("ollama (all models)".into(), format!("Ollama not reachable at {url}: {e}"))),
        }

        let mut results = Vec::new();
        for t in &targets {
            eprintln!("ttft: {} ({} runs × 2)", t.label, o.runs);
            results.extend(run_target(t, o.runs).await);
        }
        let mut r = Report::new("bench/ttft — live");
        r.para(&format!(
            "{} timed runs per row after one warm-up; local models preloaded (warm). Cached = same \
             prompt prefix every run; no cache = unique nonce at the start of the prefix.",
            o.runs
        ));
        render(&mut r, &results, &skipped);
        r.write(report_path)?;
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::mock::{MockAnswer, MockOutcome};
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn measures_first_token_and_errors() {
        let m = MockAnswer::new("m", true, &["", "a", "b", "c"]).with_first_token_delay(Duration::from_millis(300));
        let s = measure(&m, &bench_prompt(false)).await;
        // The empty first token is ignored; "a" arrives one token_delay (10 ms) later.
        assert_eq!(s.ttft_ms, Some(310));
        assert_eq!(s.chunks, 3);
        assert!(s.error.is_none());
        let bad = MockAnswer::new("m", true, &["a"]).failing(MockOutcome::RateLimited);
        let s = measure(&bad, &bench_prompt(false)).await;
        assert_eq!(s.ttft_ms, None);
        assert_eq!(s.error.as_deref(), Some("rate limited"));
    }

    #[test]
    fn prompts() {
        let c = bench_prompt(false);
        let l = bench_prompt(true);
        assert!(c.prefix.contains("Northwind") && c.dynamic.contains("40 seats"));
        assert!(ssc_context::count_tokens(&l.dynamic) <= 300, "local dynamic part must stay under 300 tokens");
        let u1 = uncached(&c);
        let u2 = uncached(&c);
        assert_ne!(u1.prefix, u2.prefix);
        assert!(u1.prefix.ends_with(&c.prefix));
        assert_eq!(u1.dynamic, c.dynamic);
    }

    #[test]
    fn ollama_body_turns_thinking_off() {
        let o = Ollama::new("http://127.0.0.1:11434", "qwen3:8b");
        let b = o.body(&bench_prompt(true));
        assert_eq!(b["think"], Value::Bool(false));
        assert_eq!(b["model"], "qwen3:8b");
        assert_eq!(b["stream"], Value::Bool(true));
        assert!(!o.is_cloud());
    }
}
