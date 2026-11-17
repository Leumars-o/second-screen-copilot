//! `bench cpu`: samples a process's CPU while idle and while capturing.
//!
//! CPU% per interval = Δ(cumulative CPU time) / Δ(wall time), from `ps -o time=` (10 ms
//! resolution), expressed as % of one core (Activity Monitor's convention; targets: idle < 2%,
//! capturing < 10%, Build Sheet §1) and as % of the whole machine.

use crate::report::Report;
use crate::stats::percentile_f64;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub const IDLE_TARGET_PCT: f64 = 2.0;
pub const CAPTURE_TARGET_PCT: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Capturing,
}

impl Phase {
    pub fn target(self) -> f64 {
        match self {
            Self::Idle => IDLE_TARGET_PCT,
            Self::Capturing => CAPTURE_TARGET_PCT,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Capturing => "capturing",
        }
    }
}

/// (wall ms since start, cumulative process CPU ms).
pub type Sample = (u64, u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseSamples {
    pub phase: Phase,
    pub samples: Vec<Sample>,
}

/// Parse `ps -o time=`: `[[dd-]hh:]mm:ss.cc` → ms.
pub fn parse_ps_time(s: &str) -> Option<u64> {
    let s = s.trim();
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let secs: f64 = parts.last()?.parse().ok()?;
    let mut total = secs * 1000.0;
    let mut mult = 60_000.0;
    for p in parts.iter().rev().skip(1) {
        total += p.parse::<f64>().ok()? * mult;
        mult *= 60.0;
    }
    Some(days * 86_400_000 + total.round() as u64)
}

/// Cumulative CPU time of `pid` in ms.
pub fn cpu_time_ms(pid: u32) -> Result<u64> {
    let out = Command::new("ps").args(["-o", "time=", "-p", &pid.to_string()]).output().context("run ps")?;
    anyhow::ensure!(out.status.success(), "process {pid} not found");
    let s = String::from_utf8_lossy(&out.stdout);
    parse_ps_time(&s).with_context(|| format!("unparseable ps time {s:?}"))
}

/// First pid whose name is exactly `name` (`pgrep -x`).
pub fn pid_of(name: &str) -> Result<u32> {
    let out = Command::new("pgrep").args(["-x", name]).output().context("run pgrep")?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.trim().parse().ok())
        .with_context(|| format!("no running process named {name}"))
}

/// Sample `pid` every `interval` for `duration`.
pub fn sample(pid: u32, duration: Duration, interval: Duration) -> Result<Vec<Sample>> {
    let start = Instant::now();
    let mut out = vec![(0, cpu_time_ms(pid)?)];
    while start.elapsed() < duration {
        std::thread::sleep(interval);
        out.push((start.elapsed().as_millis() as u64, cpu_time_ms(pid)?));
    }
    Ok(out)
}

/// CPU % of one core for each interval between consecutive samples.
pub fn interval_pcts(samples: &[Sample]) -> Vec<f64> {
    samples
        .windows(2)
        .filter_map(|w| {
            let dw = w[1].0.checked_sub(w[0].0)?;
            let dc = w[1].1.saturating_sub(w[0].1);
            (dw > 0).then(|| 100.0 * dc as f64 / dw as f64)
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct PhaseStats {
    pub phase: Phase,
    pub seconds: f64,
    /// Whole-phase average: total CPU / total wall.
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

impl PhaseStats {
    pub fn of(p: &PhaseSamples) -> Option<Self> {
        let pcts = interval_pcts(&p.samples);
        let (first, last) = (p.samples.first()?, p.samples.last()?);
        let wall = last.0.checked_sub(first.0).filter(|w| *w > 0)?;
        Some(Self {
            phase: p.phase,
            seconds: wall as f64 / 1000.0,
            mean: 100.0 * last.1.saturating_sub(first.1) as f64 / wall as f64,
            p50: percentile_f64(&pcts, 50.0)?,
            p95: percentile_f64(&pcts, 95.0)?,
            max: pcts.iter().copied().fold(0.0, f64::max),
        })
    }
    pub fn passes(&self) -> bool {
        self.mean < self.phase.target()
    }
}

pub fn render(r: &mut Report, phases: &[PhaseSamples], cores: usize) {
    let rows: Vec<Vec<String>> = phases
        .iter()
        .filter_map(PhaseStats::of)
        .map(|s| {
            vec![
                s.phase.as_str().to_string(),
                format!("{:.0}", s.seconds),
                format!("{:.2}", s.mean),
                format!("{:.2}", s.p50),
                format!("{:.2}", s.p95),
                format!("{:.2}", s.max),
                format!("{:.2}", s.mean / cores as f64),
                format!("< {}% : **{}**", s.phase.target(), if s.passes() { "PASS" } else { "FAIL" }),
            ]
        })
        .collect();
    r.table(
        &["phase", "seconds", "mean % (1 core)", "p50 %", "p95 %", "max %", "mean % of machine", "target (mean)"],
        &rows,
    );
}

pub fn fixture_path() -> PathBuf {
    crate::bench_path("fixtures/cpu/samples.json")
}

/// Fixture mode: recorded (synthetic) samples → report. Returns the stats per phase.
pub fn run_fixture(report_path: &Path) -> Result<(Report, Vec<PhaseStats>)> {
    let text = std::fs::read_to_string(fixture_path())?;
    let phases: Vec<PhaseSamples> = serde_json::from_str(&text)?;
    let mut r = Report::new("bench/cpu — fixture mode");
    r.para(
        "Synthetic recorded samples (`fixtures/cpu/samples.json`, 8-core machine assumed): proves the \
         sampling maths and report, not the agent.",
    );
    render(&mut r, &phases, 8);
    r.write(report_path)?;
    Ok((r, phases.iter().filter_map(PhaseStats::of).collect()))
}

pub struct LiveOpts {
    pub pid: u32,
    pub idle: Duration,
    pub capture: Duration,
    pub interval: Duration,
    /// Optional path to save the raw samples (JSON, same format as the fixture).
    pub save: Option<PathBuf>,
}

/// Live mode: sample the running agent. Between phases, waits for Enter on stdin.
pub fn run_live(report_path: &Path, o: &LiveOpts) -> Result<Report> {
    let mut phases = Vec::new();
    if !o.idle.is_zero() {
        eprintln!("cpu: sampling pid {} idle for {:?} — leave the agent idle (no call, no capture)", o.pid, o.idle);
        phases.push(PhaseSamples { phase: Phase::Idle, samples: sample(o.pid, o.idle, o.interval)? });
    }
    if !o.capture.is_zero() {
        if !o.idle.is_zero() {
            eprintln!("cpu: start capturing in the agent (start a call / session), then press Enter");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
        }
        eprintln!("cpu: sampling pid {} while capturing for {:?}", o.pid, o.capture);
        phases.push(PhaseSamples { phase: Phase::Capturing, samples: sample(o.pid, o.capture, o.interval)? });
    }
    if let Some(p) = &o.save {
        std::fs::write(p, serde_json::to_string_pretty(&phases)?)?;
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut r = Report::new("bench/cpu — live");
    r.para(&format!("pid {}, {cores} logical cores, sample every {:?}.", o.pid, o.interval));
    render(&mut r, &phases, cores);
    r.write(report_path)?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_time_formats() {
        assert_eq!(parse_ps_time("0:00.12"), Some(120));
        assert_eq!(parse_ps_time(" 12:34.56\n"), Some(754_560));
        assert_eq!(parse_ps_time("1:02:03.45"), Some(3_723_450));
        assert_eq!(parse_ps_time("2-01:00:00"), Some(2 * 86_400_000 + 3_600_000));
        assert_eq!(parse_ps_time("00:00:01"), Some(1000));
        assert_eq!(parse_ps_time("junk"), None);
        assert_eq!(parse_ps_time(""), None);
    }

    #[test]
    fn interval_maths() {
        let s = vec![(0, 100), (1000, 110), (2000, 130), (3000, 130)];
        assert_eq!(interval_pcts(&s), vec![1.0, 2.0, 0.0]);
        let st = PhaseStats::of(&PhaseSamples { phase: Phase::Idle, samples: s }).unwrap();
        assert!((st.mean - 1.0).abs() < 1e-9 && st.passes());
        assert_eq!(st.max, 2.0);
        assert!(PhaseStats::of(&PhaseSamples { phase: Phase::Idle, samples: vec![(0, 0)] }).is_none());
    }

    #[test]
    fn samples_this_process() {
        let pid = std::process::id();
        let before = cpu_time_ms(pid).unwrap();
        let t = Instant::now();
        let mut x = 0u64;
        while t.elapsed() < Duration::from_millis(150) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        assert!(cpu_time_ms(pid).unwrap() >= before);
        assert!(cpu_time_ms(u32::MAX - 7).is_err());
    }
}
