//! `bench decision`: Jev (cloud) vs Tev1 4B (local) on the labelled snippet set → accuracy per
//! decision question plus p50/p95 latency. Both run through the production adapter
//! (`ssc_router::SystemOneDecision`, `POST /v1/systemone`).
//!
//! Data: `bench/data/snippets.jsonl` (human-labelled from real calls, Build Sheet §9; one JSON
//! object per line, format [`Snippet`]). Until it exists, `bench/data/snippets.synthetic.jsonl`
//! shows the format; any report over synthetic items says so at the top.

use crate::report::{Report, ms_cell, pct};
use crate::stats::Summary;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use ssc_core::decision::{Decision, DecisionInput, Tier};
use ssc_core::protocol::{Intent, Urgency};
use ssc_core::traits::DecisionModel;
use std::path::{Path, PathBuf};
use tokio::time::Instant;

/// The decision router's budget (`Config::timeouts.decision_ms`); slower answers are counted.
pub const DECISION_BUDGET_MS: u64 = 800;

/// One labelled snippet. A `null` label field means "not labelled" and is skipped for that question.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snippet {
    pub id: String,
    /// True for made-up examples. Real snippets set `false`.
    #[serde(default)]
    pub synthetic: bool,
    /// Where it came from (e.g. "call 2026-11-20 tester 3, 12:04"); free text, no personal data.
    #[serde(default)]
    pub source: String,
    /// The other person's last utterance (what the trigger engine sends).
    pub question: String,
    #[serde(default)]
    pub screen_diff: String,
    /// Profile ids offered to the model.
    pub profiles: Vec<String>,
    pub label: Label,
    #[serde(default)]
    pub labeller: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Label {
    pub needs_answer: Option<bool>,
    pub intent: Option<Intent>,
    pub answer_tier: Option<Tier>,
    pub profile: Option<String>,
    pub urgency: Option<Urgency>,
}

impl Snippet {
    pub fn input(&self) -> DecisionInput {
        DecisionInput {
            question: self.question.clone(),
            screen_diff: self.screen_diff.clone(),
            profiles: self.profiles.clone(),
        }
    }
}

pub fn load(path: &Path) -> Result<Vec<Snippet>> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut out = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for (i, line) in s.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let sn: Snippet = serde_json::from_str(line).with_context(|| format!("{}:{}", path.display(), i + 1))?;
        anyhow::ensure!(ids.insert(sn.id.clone()), "{}:{}: duplicate id {}", path.display(), i + 1, sn.id);
        if let Some(p) = &sn.label.profile {
            anyhow::ensure!(sn.profiles.contains(p), "{}:{}: label profile {p} not in profiles", path.display(), i + 1);
        }
        out.push(sn);
    }
    Ok(out)
}

pub fn real_set_path() -> PathBuf {
    crate::bench_path("data/snippets.jsonl")
}

pub fn synthetic_set_path() -> PathBuf {
    crate::bench_path("data/snippets.synthetic.jsonl")
}

pub const QUESTIONS: [&str; 5] = ["needs_answer", "intent", "answer_tier", "profile", "urgency"];

/// Per question: Some(correct?) if labelled, None if not.
pub fn grade(label: &Label, d: &Decision) -> [Option<bool>; 5] {
    [
        label.needs_answer.map(|v| v == d.needs_answer.value),
        label.intent.map(|v| v == d.intent.value),
        label.answer_tier.map(|v| v == d.answer_tier.value),
        label.profile.as_ref().map(|v| *v == d.profile.value),
        label.urgency.map(|v| v == d.urgency.value),
    ]
}

#[derive(Debug, Clone)]
pub struct ItemResult {
    pub id: String,
    pub latency_ms: u64,
    pub decision: Result<Decision, String>,
    pub grades: [Option<bool>; 5],
}

#[derive(Debug, Clone)]
pub struct ModelResult {
    pub label: String,
    pub items: Vec<ItemResult>,
}

impl ModelResult {
    /// (correct, labelled-and-answered) per question. Errors count as wrong.
    pub fn accuracy(&self, q: usize, snippets: &[Snippet]) -> (usize, usize) {
        let mut ok = 0;
        let mut n = 0;
        for (it, sn) in self.items.iter().zip(snippets) {
            let labelled = grade_mask(&sn.label)[q];
            if !labelled {
                continue;
            }
            n += 1;
            if it.grades[q] == Some(true) {
                ok += 1;
            }
        }
        (ok, n)
    }
    pub fn latency(&self) -> Option<Summary> {
        Summary::of(&self.items.iter().filter(|i| i.decision.is_ok()).map(|i| i.latency_ms).collect::<Vec<_>>())
    }
    pub fn errors(&self) -> usize {
        self.items.iter().filter(|i| i.decision.is_err()).count()
    }
    pub fn over_budget(&self) -> usize {
        self.items.iter().filter(|i| i.latency_ms > DECISION_BUDGET_MS).count()
    }
    /// needs_answer confusion: (true pos, false pos, false neg).
    pub fn needs_answer_confusion(&self, snippets: &[Snippet]) -> (usize, usize, usize) {
        let (mut tp, mut fp, mut fneg) = (0, 0, 0);
        for (it, sn) in self.items.iter().zip(snippets) {
            let (Some(truth), Ok(d)) = (sn.label.needs_answer, &it.decision) else { continue };
            match (truth, d.needs_answer.value) {
                (true, true) => tp += 1,
                (false, true) => fp += 1,
                (true, false) => fneg += 1,
                _ => {}
            }
        }
        (tp, fp, fneg)
    }
}

fn grade_mask(l: &Label) -> [bool; 5] {
    [l.needs_answer.is_some(), l.intent.is_some(), l.answer_tier.is_some(), l.profile.is_some(), l.urgency.is_some()]
}

/// Runs every snippet through `model`, one at a time, timing each call.
pub async fn run_model(label: &str, model: &dyn DecisionModel, snippets: &[Snippet]) -> ModelResult {
    let mut items = Vec::with_capacity(snippets.len());
    for sn in snippets {
        let start = Instant::now();
        let r = model.decide(&sn.input()).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        let grades = match &r {
            Ok(d) => grade(&sn.label, d),
            Err(_) => [None; 5],
        };
        items.push(ItemResult { id: sn.id.clone(), latency_ms, decision: r.map_err(|e| e.to_string()), grades });
    }
    ModelResult { label: label.into(), items }
}

pub fn render(r: &mut Report, snippets: &[Snippet], results: &[ModelResult], skipped: &[(String, String)]) {
    let synthetic = snippets.iter().filter(|s| s.synthetic).count();
    if synthetic > 0 {
        r.para(&format!(
            "**SYNTHETIC DATA: {synthetic} of {} snippets are synthetic examples. These numbers say \
             nothing about model quality.** The real set is ~200 human-labelled snippets from real \
             calls in `bench/data/snippets.jsonl` (Build Sheet §9).",
            snippets.len()
        ));
    }
    let mut header = vec!["question"];
    header.extend(results.iter().map(|m| m.label.as_str()));
    let mut rows: Vec<Vec<String>> = QUESTIONS
        .iter()
        .enumerate()
        .map(|(q, name)| {
            let mut row = vec![name.to_string()];
            row.extend(results.iter().map(|m| {
                let (ok, n) = m.accuracy(q, snippets);
                pct(ok, n)
            }));
            row
        })
        .collect();
    rows.push(
        std::iter::once("needs_answer TP / FP / FN".to_string())
            .chain(results.iter().map(|m| {
                let (tp, fp, fneg) = m.needs_answer_confusion(snippets);
                format!("{tp} / {fp} / {fneg}")
            }))
            .collect(),
    );
    rows.push(
        std::iter::once("latency p50 / p95 ms".to_string())
            .chain(results.iter().map(|m| ms_cell(m.latency())))
            .collect(),
    );
    rows.push(
        std::iter::once(format!("over {DECISION_BUDGET_MS} ms budget"))
            .chain(results.iter().map(|m| pct(m.over_budget(), m.items.len())))
            .collect(),
    );
    rows.push(
        std::iter::once("errors".to_string()).chain(results.iter().map(|m| pct(m.errors(), m.items.len()))).collect(),
    );
    r.table(&header, &rows);

    r.h2("Disagreements");
    let mut any = false;
    for m in results {
        for (it, sn) in m.items.iter().zip(snippets) {
            let wrong: Vec<&str> =
                QUESTIONS.iter().zip(it.grades).filter(|(_, g)| *g == Some(false)).map(|(q, _)| *q).collect();
            match &it.decision {
                Err(e) => {
                    any = true;
                    r.bullet(&format!("{} `{}`: error: {e}", m.label, sn.id));
                }
                Ok(_) if !wrong.is_empty() => {
                    any = true;
                    r.bullet(&format!("{} `{}`: wrong {}", m.label, sn.id, wrong.join(", ")));
                }
                _ => {}
            }
        }
    }
    if !any {
        r.bullet("none");
    }
    r.end_list();
    if !skipped.is_empty() {
        r.h2("Skipped");
        for (m, why) in skipped {
            r.bullet(&format!("{m}: {why}"));
        }
        r.end_list();
    }
}

// ---------- fixture mode ----------

#[derive(Debug, Deserialize)]
struct RecordedResponse {
    id: String,
    delay_ms: u64,
    response: serde_json::Value,
}

/// Fixture mode: the production `SystemOneDecision` adapter against loopback fakes replaying
/// recorded (synthetic) `/v1/systemone` responses for Jev and Tev1, over the synthetic set.
pub async fn run_fixture(report_path: &Path) -> Result<(Report, Vec<ModelResult>)> {
    use crate::fake::{Reply, http};
    use ssc_core::secrets::Secret;
    use ssc_router::SystemOneDecision;
    use std::time::Duration;

    let snippets = load(&synthetic_set_path())?;
    let mut results = Vec::new();
    let mut fakes = Vec::new();
    for (key, model, label) in [("jev", "jev-1.14.0", "Jev (jev-1.14.0)"), ("tev1", "tev1:4b", "Tev1 4B (tev1:4b)")] {
        let path = crate::bench_path(&format!("fixtures/decision/{key}.jsonl"));
        let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
        let recs: Vec<RecordedResponse> =
            text.lines().filter(|l| !l.trim().is_empty()).map(serde_json::from_str).collect::<Result<_, _>>()?;
        anyhow::ensure!(
            recs.iter().map(|r| &r.id).eq(snippets.iter().map(|s| &s.id)),
            "{}: ids must match the snippet order",
            path.display()
        );
        let replies = recs.iter().map(|r| Reply::json(&r.response, Duration::from_millis(r.delay_ms))).collect();
        let fake = http(replies).await?;
        let key_secret = (key == "jev").then(|| Secret::new("fixture-key-not-real"));
        let adapter = SystemOneDecision::new(&fake.url(), model, key_secret);
        results.push(run_model(label, &adapter, &snippets).await);
        anyhow::ensure!(
            fake.requests().iter().all(|q| q.method == "POST" && q.path == "/v1/systemone"),
            "unexpected request path"
        );
        fakes.push(fake);
    }
    let mut r = Report::new("bench/decision — fixture mode");
    r.para(
        "Production `/v1/systemone` adapter against loopback fakes replaying recorded synthetic \
         responses with scripted delays. Latency here is loopback + scripted delay.",
    );
    render(&mut r, &snippets, &results, &[]);
    r.write(report_path)?;
    Ok((r, results))
}

// ---------- live mode ----------

#[cfg(feature = "live")]
pub mod live {
    use super::*;
    use crate::live::{Keys, has_model, load_config, ollama_models, ollama_version};
    use ssc_core::secrets::SecretKey;
    use ssc_router::SystemOneDecision;

    pub struct LiveOpts {
        pub data: PathBuf,
        pub jev_url: Option<String>,
    }

    /// `0.35.0` ≤ `v`? Tev1 needs Ollama's native `/v1/systemone` (≥ 0.35).
    pub fn ollama_at_least_035(v: &str) -> bool {
        let mut it = v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse::<u64>());
        match (it.next(), it.next()) {
            (Some(Ok(major)), Some(Ok(minor))) => (major, minor) >= (0, 35),
            _ => false,
        }
    }

    pub async fn run(report_path: &Path, o: &LiveOpts) -> Result<Report> {
        let snippets = load(&o.data).with_context(|| {
            format!(
                "no labelled set at {} (§9: ~200 human-labelled snippets). To try the harness on the \
                 synthetic examples pass --data {}",
                o.data.display(),
                synthetic_set_path().display()
            )
        })?;
        let cfg = load_config();
        let keys = Keys;
        let mut skipped = Vec::new();
        let mut results = Vec::new();

        let jev_model = cfg.models.decision_cloud_model.clone();
        let jev_url = o.jev_url.clone().unwrap_or(cfg.models.decision_cloud_base_url.clone());
        match (keys.get(SecretKey::TypeSafe), jev_url.trim().is_empty()) {
            (None, _) => skipped.push((format!("Jev ({jev_model})"), "no `api.typesafe` key in the keychain".into())),
            (Some(_), true) => skipped.push((
                format!("Jev ({jev_model})"),
                "no Jev base URL: set `models.decision_cloud_base_url` in config.toml or pass --jev-url".into(),
            )),
            (Some(k), false) => {
                eprintln!("decision: Jev on {} snippets", snippets.len());
                let a = SystemOneDecision::new(&jev_url, &jev_model, Some(k));
                results.push(run_model(&format!("Jev ({jev_model})"), &a, &snippets).await);
            }
        }

        let tev_model = cfg.models.decision_local_model.clone();
        let url = cfg.models.ollama_url.clone();
        let tev_label = format!("Tev1 ({tev_model})");
        match (ollama_version(&url).await, ollama_models(&url).await) {
            (Ok(v), Ok(have)) if ollama_at_least_035(&v) && has_model(&have, &tev_model) => {
                eprintln!("decision: Tev1 on {} snippets", snippets.len());
                let a = SystemOneDecision::new(&url, &tev_model, None);
                results.push(run_model(&tev_label, &a, &snippets).await);
            }
            (Ok(v), Ok(_)) if !ollama_at_least_035(&v) => {
                skipped.push((tev_label, format!("Ollama {v} has no /v1/systemone (need ≥ 0.35)")))
            }
            (Ok(_), Ok(_)) => skipped.push((tev_label, format!("not pulled (`ollama pull {tev_model}`)"))),
            (Err(e), _) | (_, Err(e)) => skipped.push((tev_label, format!("Ollama not reachable at {url}: {e}"))),
        }

        let mut r = Report::new("bench/decision — live");
        r.para(&format!("Data: `{}` ({} snippets).", o.data.display(), snippets.len()));
        render(&mut r, &snippets, &results, &skipped);
        r.write(report_path)?;
        Ok(r)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn version_gate() {
            assert!(super::ollama_at_least_035("0.35.0"));
            assert!(super::ollama_at_least_035("0.40.16"));
            assert!(super::ollama_at_least_035("1.0.0"));
            assert!(!super::ollama_at_least_035("0.32.1"));
            assert!(!super::ollama_at_least_035("garbage"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::mock::{MockDecision, MockOutcome, decision_answer};
    use std::time::Duration;

    #[test]
    fn synthetic_set_is_valid_and_marked() {
        let s = load(&synthetic_set_path()).unwrap();
        assert!((10..=20).contains(&s.len()));
        assert!(s.iter().all(|x| x.synthetic && x.labeller == "synthetic"));
    }

    #[test]
    fn rejects_bad_lines() {
        let dir = std::env::temp_dir().join(format!("ssc-bench-dec-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let ok = r#"{"id":"a","question":"q","profiles":["x","y"],"label":{"needs_answer":true,"intent":null,"answer_tier":null,"profile":"x","urgency":null}}"#;
        std::fs::write(&p, format!("{ok}\n{ok}\n")).unwrap();
        assert!(load(&p).unwrap_err().to_string().contains("duplicate"));
        std::fs::write(&p, ok.replace("\"profile\":\"x\"", "\"profile\":\"z\"")).unwrap();
        assert!(load(&p).unwrap_err().to_string().contains("not in profiles"));
        std::fs::write(&p, ok.replace("\"intent\":null", "\"intent\":\"weather\"")).unwrap();
        assert!(load(&p).is_err());
        std::fs::write(&p, ok).unwrap();
        assert_eq!(load(&p).unwrap()[0].label.intent, None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn grades_times_and_counts_errors() {
        let snippets = load(&synthetic_set_path()).unwrap();
        let first_two = &snippets[..2]; // syn-001 pricing, syn-002 objection
        let m = MockDecision::scripted(
            vec![MockOutcome::Ok(decision_answer(Intent::Pricing, "sales")), MockOutcome::Timeout],
            MockOutcome::Unavailable,
        )
        .with_delay(Duration::from_millis(900));
        let r = run_model("mock", &m, first_two).await;
        assert_eq!(r.items[0].latency_ms, 900);
        assert_eq!(r.accuracy(1, first_two), (1, 2)); // intent: right, then error
        assert_eq!(r.errors(), 1);
        assert_eq!(r.over_budget(), 2);
        assert_eq!(r.latency().unwrap().p50, 900);
        assert_eq!(r.needs_answer_confusion(first_two), (1, 0, 0));
    }
}
