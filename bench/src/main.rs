//! `ssc-bench <harness> [--live] [options]` — see bench/README.md.

use anyhow::{Context, Result, bail};
use ssc_bench::{answers, cpu, decision, default_report, latency, stt, ttft};
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "usage: ssc-bench <latency|ttft|stt|decision|cpu|answers|all> [--live] [--out FILE] [options]
  latency  [--jsonl FILE]... [--mode cloud|local] [--enforce]
  ttft     [--runs N] [--groq-model M] [--ollama-models a,b,c]          (--live needs --features live)
  stt      [--manifest FILE] [--whisper-model FILE]                      (--live needs --features live)
  decision [--data FILE] [--jev-url URL]                                 (--live needs --features live)
  cpu      --live (--pid N | --name NAME) [--idle-secs S] [--capture-secs S] [--interval-ms MS] [--save FILE]
  answers  [--data FILE] [--model groq|ollama:<model>]                   (--live needs --features live)
  all      every harness in fixture mode";

struct Args {
    rest: Vec<String>,
}

impl Args {
    fn flag(&mut self, name: &str) -> bool {
        let before = self.rest.len();
        self.rest.retain(|a| a != name);
        before != self.rest.len()
    }
    fn values(&mut self, name: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        while let Some(i) = self.rest.iter().position(|a| a == name) {
            anyhow::ensure!(i + 1 < self.rest.len(), "{name} needs a value");
            out.push(self.rest.remove(i + 1));
            self.rest.remove(i);
        }
        Ok(out)
    }
    fn value(&mut self, name: &str) -> Result<Option<String>> {
        Ok(self.values(name)?.pop())
    }
    fn num(&mut self, name: &str, default: u64) -> Result<u64> {
        match self.value(name)? {
            Some(v) => v.parse().with_context(|| format!("{name}: not a number")),
            None => Ok(default),
        }
    }
    fn done(self) -> Result<()> {
        if !self.rest.is_empty() {
            bail!("unknown arguments: {}\n{USAGE}", self.rest.join(" "));
        }
        Ok(())
    }
}

#[cfg(not(feature = "live"))]
fn needs_live_feature() -> Result<()> {
    bail!("--live needs a build with `--features live` (cargo run -p ssc-bench --features live -- ...)")
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        println!("{USAGE}");
        return Ok(());
    }
    let cmd = argv.remove(0);
    let mut a = Args { rest: argv };
    let live = a.flag("--live");
    let out = a.value("--out")?.map(PathBuf::from).unwrap_or_else(|| default_report(&cmd));

    match cmd.as_str() {
        "latency" => {
            let files: Vec<PathBuf> = a.values("--jsonl")?.into_iter().map(PathBuf::from).collect();
            let mode = match a.value("--mode")? {
                Some(m) => latency::RunMode::parse(&m).context("--mode must be cloud or local")?,
                None => latency::RunMode::Cloud,
            };
            let enforce = a.flag("--enforce") || live;
            a.done()?;
            if live && files.is_empty() {
                let driver = latency::real_pipeline()?;
                let calls = latency::load_calls(&latency::fixture_calls_path())?;
                let dir = out.parent().unwrap_or(std::path::Path::new(".")).join("latency-jsonl");
                let res = latency::replay_and_analyse(driver.as_ref(), &calls, &dir).await?;
                let mut r = ssc_bench::report::Report::new("bench/latency — live");
                for (an, p) in &res {
                    latency::render(&mut r, an, &format!("`{}`", p.display()));
                }
                r.write(&out)?;
                print!("{}", r.as_str());
                if res.iter().any(|(an, _)| an.meets_targets() == Some(false)) {
                    bail!("cloud latency targets missed");
                }
            } else if !files.is_empty() {
                let (r, an) = latency::run_jsonl(&files, mode, &out)?;
                print!("{}", r.as_str());
                if enforce && an.meets_targets() != Some(true) {
                    bail!("latency targets missed (or no end-to-end samples)");
                }
            } else {
                let r = latency::run_fixture(&out).await?;
                print!("{}", r.as_str());
            }
        }
        "ttft" => {
            let runs = a.num("--runs", if live { 10 } else { 3 })? as usize;
            let _groq_model = a.value("--groq-model")?;
            let _ollama = a.value("--ollama-models")?;
            a.done()?;
            if live {
                #[cfg(feature = "live")]
                {
                    let o = ttft::live::LiveOpts {
                        runs,
                        groq_model: _groq_model,
                        ollama_models: _ollama
                            .unwrap_or_else(|| "gemma4:e2b,qwen3:8b,gemma4:12b".into())
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    };
                    print!("{}", ttft::live::run(&out, &o).await?.as_str());
                }
                #[cfg(not(feature = "live"))]
                needs_live_feature()?;
            } else {
                print!("{}", ttft::run_fixture(&out, runs).await?.0.as_str());
            }
        }
        "stt" => {
            let _manifest = a.value("--manifest")?.map(PathBuf::from).unwrap_or_else(stt::manifest_path);
            let _whisper = a.value("--whisper-model")?.map(PathBuf::from);
            a.done()?;
            if live {
                #[cfg(feature = "live")]
                print!(
                    "{}",
                    stt::live::run(&out, &stt::live::LiveOpts { manifest: _manifest, whisper_model: _whisper })
                        .await?
                        .as_str()
                );
                #[cfg(not(feature = "live"))]
                needs_live_feature()?;
            } else {
                print!("{}", stt::run_fixture(&out).await?.0.as_str());
            }
        }
        "decision" => {
            let _data = a.value("--data")?.map(PathBuf::from).unwrap_or_else(decision::real_set_path);
            let _jev = a.value("--jev-url")?;
            a.done()?;
            if live {
                #[cfg(feature = "live")]
                print!(
                    "{}",
                    decision::live::run(&out, &decision::live::LiveOpts { data: _data, jev_url: _jev }).await?.as_str()
                );
                #[cfg(not(feature = "live"))]
                needs_live_feature()?;
            } else {
                print!("{}", decision::run_fixture(&out).await?.0.as_str());
            }
        }
        "cpu" => {
            let pid = a.value("--pid")?;
            let name = a.value("--name")?;
            let idle = a.num("--idle-secs", 600)?;
            let capture = a.num("--capture-secs", 600)?;
            let interval = a.num("--interval-ms", 1000)?;
            let save = a.value("--save")?.map(PathBuf::from);
            a.done()?;
            if live {
                let pid = match (pid, name) {
                    (Some(p), _) => p.parse().context("--pid")?,
                    (None, Some(n)) => cpu::pid_of(&n)?,
                    (None, None) => cpu::pid_of("ssc-agent")?,
                };
                let o = cpu::LiveOpts {
                    pid,
                    idle: Duration::from_secs(idle),
                    capture: Duration::from_secs(capture),
                    interval: Duration::from_millis(interval.max(100)),
                    save,
                };
                print!("{}", cpu::run_live(&out, &o)?.as_str());
            } else {
                print!("{}", cpu::run_fixture(&out)?.0.as_str());
            }
        }
        "answers" => {
            let _data = a.value("--data")?.map(PathBuf::from).unwrap_or_else(answers::synthetic_set_path);
            let _model = a.value("--model")?.unwrap_or_else(|| "groq".into());
            a.done()?;
            if live {
                #[cfg(feature = "live")]
                print!(
                    "{}",
                    answers::live::run(&out, &answers::live::LiveOpts { data: _data, model: _model }).await?.as_str()
                );
                #[cfg(not(feature = "live"))]
                needs_live_feature()?;
            } else {
                let items = answers::load(&_data)?;
                let mut r = ssc_bench::report::Report::new("bench/answers");
                r.para(&format!("Data: `{}`.", _data.display()));
                answers::render(&mut r, &items);
                r.write(&out)?;
                print!("{}", r.as_str());
            }
        }
        "all" => {
            a.done()?;
            anyhow::ensure!(!live, "`all` runs fixture mode only");
            latency::run_fixture(&default_report("latency")).await?;
            ttft::run_fixture(&default_report("ttft"), 3).await?;
            stt::run_fixture(&default_report("stt")).await?;
            decision::run_fixture(&default_report("decision")).await?;
            cpu::run_fixture(&default_report("cpu"))?;
            answers::run_fixture(&default_report("answers"))?;
            println!("reports written to {}", ssc_bench::bench_path("reports").display());
        }
        _ => bail!("unknown harness {cmd}\n{USAGE}"),
    }
    Ok(())
}
