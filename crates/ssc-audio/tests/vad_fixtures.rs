//! Acceptance: VAD end-of-speech within ±100 ms of the label, on fixture audio.
//! Synthetic fixtures are generated deterministically (`ssc_audio::synth`); the speech WAV in
//! `tests/fixtures/` is real TTS speech with hand-checked labels in its `.labels` file.

use ssc_audio::synth::{Segment, render};
use ssc_audio::vad::{Endpointer, EndpointerConfig, VadEvent};

const BASE_MS: u64 = 1_763_290_000_000;
const TOLERANCE_MS: u64 = 100;

/// Feed in 20 ms chunks; returns (start, end) pairs relative to the clip start.
fn detect(samples: &[i16], cfg: EndpointerConfig) -> Vec<(u64, u64)> {
    let mut ep = Endpointer::new(cfg);
    let mut out = Vec::new();
    for (i, chunk) in samples.chunks(320).enumerate() {
        for ev in ep.push(chunk, BASE_MS + i as u64 * 20) {
            if let VadEvent::SpeechEnd { start_ms, t_ms } = ev {
                out.push((start_ms - BASE_MS, t_ms - BASE_MS));
            }
        }
    }
    if let Some(VadEvent::SpeechEnd { start_ms, t_ms }) = ep.flush() {
        out.push((start_ms - BASE_MS, t_ms - BASE_MS));
    }
    out
}

fn assert_close(got: &[(u64, u64)], want: &[(u64, u64)]) {
    assert_eq!(got.len(), want.len(), "got {got:?}, want {want:?}");
    for (g, w) in got.iter().zip(want) {
        assert!(g.0.abs_diff(w.0) <= TOLERANCE_MS, "start {} vs label {} (got {got:?})", g.0, w.0);
        assert!(g.1.abs_diff(w.1) <= TOLERANCE_MS, "end {} vs label {} (got {got:?})", g.1, w.1);
    }
}

#[test]
fn single_utterance() {
    let clip = render(&[Segment::Silence { ms: 700 }, Segment::Speech { ms: 1_500 }, Segment::Silence { ms: 800 }]);
    assert_close(&detect(&clip.samples, EndpointerConfig::default()), &clip.speech);
}

#[test]
fn several_utterances_and_a_short_pause_inside_one() {
    // The 150 ms gap is shorter than the 300 ms silence window: one utterance, not two.
    let clip = render(&[
        Segment::Silence { ms: 500 },
        Segment::Speech { ms: 900 },
        Segment::Silence { ms: 150 },
        Segment::Speech { ms: 600 },
        Segment::Silence { ms: 1_000 },
        Segment::Speech { ms: 1_200 },
        Segment::Silence { ms: 600 },
    ]);
    let want = vec![(clip.speech[0].0, clip.speech[1].1), clip.speech[2]];
    assert_close(&detect(&clip.samples, EndpointerConfig::default()), &want);
}

#[test]
fn silence_only_and_clicks_produce_nothing() {
    let clip = render(&[Segment::Silence { ms: 3_000 }]);
    assert!(detect(&clip.samples, EndpointerConfig::default()).is_empty());
    // A 40 ms blip is below start_ms/min_speech_ms.
    let clip = render(&[Segment::Silence { ms: 500 }, Segment::Speech { ms: 40 }, Segment::Silence { ms: 800 }]);
    assert!(detect(&clip.samples, EndpointerConfig::default()).is_empty());
}

#[test]
fn speech_at_end_of_stream_is_closed_by_flush() {
    let clip = render(&[Segment::Silence { ms: 300 }, Segment::Speech { ms: 1_000 }]);
    assert_close(&detect(&clip.samples, EndpointerConfig::default()), &clip.speech);
}

#[test]
fn detection_latency_is_the_silence_window() {
    // speech_end is reported once `silence_ms` of silence has been seen: latency budget check.
    let clip = render(&[Segment::Silence { ms: 400 }, Segment::Speech { ms: 1_000 }, Segment::Silence { ms: 1_000 }]);
    let mut ep = Endpointer::new(EndpointerConfig::default());
    let mut fired_at = None;
    for (i, chunk) in clip.samples.chunks(320).enumerate() {
        let t0 = i as u64 * 20;
        if ep.push(chunk, t0).iter().any(|e| matches!(e, VadEvent::SpeechEnd { .. })) {
            fired_at = Some(t0 + 20);
            break;
        }
    }
    let latency = fired_at.expect("no speech_end") - clip.speech[0].1;
    assert!((250..=450).contains(&latency), "speech_end detected {latency} ms after the real end");
}

#[test]
fn chunk_size_does_not_matter() {
    let clip = render(&[Segment::Silence { ms: 500 }, Segment::Speech { ms: 1_000 }, Segment::Silence { ms: 700 }]);
    let base = detect(&clip.samples, EndpointerConfig::default());
    for size in [160usize, 333, 1_600] {
        let mut ep = Endpointer::new(EndpointerConfig::default());
        let mut got = Vec::new();
        let mut t = 0u64;
        for chunk in clip.samples.chunks(size) {
            for ev in ep.push(chunk, BASE_MS + t * 1000 / 16_000) {
                if let VadEvent::SpeechEnd { start_ms, t_ms } = ev {
                    got.push((start_ms - BASE_MS, t_ms - BASE_MS));
                }
            }
            t += chunk.len() as u64;
        }
        assert_eq!(got, base, "chunk size {size}");
    }
}

/// Real speech (macOS `say`, resampled to 16 kHz), padded with silence. Labels were measured
/// once from the TTS output's own sample-accurate boundaries; see the `.labels` file.
#[test]
fn real_speech_fixture() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut r = hound::WavReader::open(format!("{dir}/question.wav")).unwrap();
    assert_eq!(r.spec().sample_rate, 16_000);
    assert_eq!(r.spec().channels, 1);
    let samples: Vec<i16> = r.samples::<i16>().map(|s| s.unwrap()).collect();
    let labels: Vec<(u64, u64)> = std::fs::read_to_string(format!("{dir}/question.labels"))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split_whitespace().map(|v| v.parse::<u64>().unwrap());
            (it.next().unwrap(), it.next().unwrap())
        })
        .collect();
    let got = detect(&samples, EndpointerConfig::default());
    // Only the end boundary is the acceptance criterion; starts are checked loosely because
    // unvoiced onsets ("wh-") are legitimately ambiguous.
    assert_eq!(got.len(), labels.len(), "got {got:?}, want {labels:?}");
    for (g, w) in got.iter().zip(&labels) {
        assert!(g.1.abs_diff(w.1) <= TOLERANCE_MS, "end {} vs label {} ({got:?})", g.1, w.1);
        assert!(g.0.abs_diff(w.0) <= 200, "start {} vs label {} ({got:?})", g.0, w.0);
    }
}

/// Steady broadband noise well above the energy floor (about -35 dBFS) must not be taken for
/// speech, and speech on top of it must still be endpointed.
#[test]
fn steady_noise_is_not_speech() {
    let clip = render(&[Segment::Silence { ms: 800 }, Segment::Speech { ms: 1_200 }, Segment::Silence { ms: 1_500 }]);
    let mut rng: u32 = 0xdead_beef;
    let noisy: Vec<i16> = clip
        .samples
        .iter()
        .map(|&s| {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            let n = ((rng as f64 / u32::MAX as f64) * 2.0 - 1.0) * 0.03 * i16::MAX as f64;
            (s as f64 + n).clamp(-32767.0, 32767.0) as i16
        })
        .collect();
    assert_close(&detect(&noisy, EndpointerConfig::default()), &clip.speech);
}
