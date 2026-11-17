# bench — latency, TTFT, STT, decision, CPU and answer-quality harnesses

One crate (`ssc-bench`, Build Sheet §7 W2-BENCH). Every harness has:

- **fixture mode** (default): no network, no keys. Run by the gate (`cargo test -p ssc-bench`)
  and by hand. Fixture numbers are scripted or replayed, so they show the harness works. They say
  nothing about how fast or how good the product is.
- **`--live` mode**: real providers and models. API keys are read from the macOS keychain
  (service `com.second-screen-copilot.agent`) through `ssc_core::secrets::KeychainStore`; macOS may
  ask once per key. A missing key, model or service **skips that target** and the report says why.
  Build with `--features live` (`cpu --live` and `latency --jsonl` need no feature).

Reports are markdown, written to `bench/reports/<harness>.md` (git-ignored), or wherever `--out FILE` points.
They are also printed to stdout.

```sh
cargo run -p ssc-bench -- all                       # every harness, fixture mode
cargo run -p ssc-bench -- <harness> [--out FILE]    # one harness, fixture mode
cargo run -p ssc-bench --features live -- <harness> --live [options]
```

Live runs use your `config.toml` (`ssc_core::config::Config::default_path()`) for model names,
the Ollama URL, the Jev base URL and the Gemini tier. Without a config file, the defaults apply.

## latency

Takes the `ssc-core::timing` JSONL and reports p50/p95 for each stage (`speech_end → transcript_final → …
→ phone_rendered`) and end to end (`speech_end → phone_rendered`). Cloud targets are p50 ≤ 1.5 s
and p95 ≤ 2.5 s. The offline target (about 2–2.5 s) is reported but not enforced.

- Fixture: `fixtures/latency/calls.json` (scripted stage times, `scripts/gen_latency_fixture.py`)
  is replayed through a mock pipeline (ssc-core mocks on a mock clock) into real timing JSONL,
  which is then analysed.
- Analyse a real run's log (works today):
  `cargo run -p ssc-bench -- latency --jsonl ~/.ssc/timing/2026-11-20.jsonl --mode cloud --enforce`
  exits non-zero when the targets are missed.
- `--live` replays the fixture calls through the real pipeline (`ssc_pipeline::replay`): your
  config + keychain models and memory, and the real `PcTransport` on loopback with a paired Rust
  phone client, so batching, encryption and the transport's `frame_sent` are in the numbers.
  STT is a stand-in (final transcript 250 ms after the end of speech; `bench stt` measures real
  STT) and the loopback RTT is ≈ 0, so add the phone's Wi-Fi RTT/2 for a real-phone figure.
  The live path's daily log is `~/.ssc/timing/<UTC date>.jsonl` (one file per day).

## ttft

Measures time to first token from the caller's side: from the moment the request is issued to the first non-empty
token. Every target runs **cached** (same byte-stable prefix, after one discarded warm-up) and
**no cache** (unique nonce at the start of the prefix each run). Local models are preloaded first.
Production adapters only: `OpenAiCompat::groq`, `OpenAiCompat::gemini`, `Ollama` (which sends
`think: false`). The `chunks/s` column counts stream chunks per second after the first token. For
Ollama this roughly equals tokens/s. Cloud providers pack several tokens per chunk, so their figure reads low.

```sh
cargo run -p ssc-bench --features live -- ttft --live \
  [--runs 10] [--groq-model openai/gpt-oss-120b] [--ollama-models gemma4:e2b,qwen3:8b,gemma4:12b]
```

Groq needs `api.groq`. Gemini Flash-Lite needs `api.gemini` and is skipped without it. A paid tier is required unless
`testing = true`. Ollama models that aren't pulled are skipped. For reference, earlier warm
measurements on the M1 Pro: gemma4:12b TTFT 0.8–1.5 s at ~8 tok/s; qwen3:8b ~0.25 s at ~16 tok/s.

## stt

Runs Deepgram streaming and local Whisper on the same clips. For each engine it reports the time from end of speech to the final
transcript (p50/p95), the WER against the reference text, and the CPU this process uses while transcribing (the engine
runs in-process; % of one core).

- Fixture: the production `DeepgramStt` against a loopback fake replaying
  `crates/ssc-stt/tests/fixtures/deepgram_session.jsonl`, plus `MockStt` standing in for Whisper,
  both on a synthetic clip ("What is your pricing?"). The audio is fed unpaced.
- Live: the audio is paced in real time.
  ```sh
  bench/scripts/make_stt_clips.sh      # SYNTHETIC macOS-TTS clips → bench/data/stt/clips (git-ignored)
  SSC_WHISPER_MODEL=~/models/ggml-base.en.bin \
    cargo run -p ssc-bench --features live,local-stt -- stt --live [--manifest FILE]
  ```
  Deepgram needs `api.deepgram`. Whisper needs `--features local-stt` (cmake) and a ggml model
  (`SSC_WHISPER_MODEL` or `--whisper-model`). If either is missing, that engine is skipped.
- Clips are listed in `data/stt/manifest.jsonl` (`wav`, `reference`, `accent`, `source`, `license`).
  The four current clips are **synthetic TTS** (en-US, en-GB, en-IN). The **Nigerian English and
  Pidgin entries are empty slots**: no openly licensed audio was sourced. One candidate for Nigerian English is
  OpenSLR SLR70 (Google crowdsourced Nigerian English, CC BY-SA 4.0). Pidgin should come from
  consented tester recordings (§9). Add a 16 kHz mono PCM16 WAV and its exact transcript to fill a slot.

## decision

Runs Jev (cloud) and Tev1 4B (local, Ollama ≥ 0.35 serving `/v1/systemone`) on the labelled set
through the production `SystemOneDecision` adapter. For each question (`needs_answer`, `intent`, `answer_tier`,
`profile`, `urgency`) it reports accuracy, plus the needs_answer TP/FP/FN, p50/p95 latency, the number of calls over the 800 ms
budget, and errors.

- **Labelled set: `data/snippets.jsonl` — ~200 human-labelled snippets from real calls (Build
  Sheet §9, human-only; not in the repo yet).** Format: one JSON object per line:
  ```json
  {"id":"call07-0412","synthetic":false,"source":"tester 3, call 2026-11-20, 04:12",
   "question":"so what would forty seats run us?","screen_diff":"Deal: Contoso, 40 seats",
   "profiles":["sales","support"],
   "label":{"needs_answer":true,"intent":"pricing","answer_tier":"fast","profile":"sales","urgency":"high"},
   "labeller":"leumas","notes":""}
  ```
  `intent` ∈ pricing, objection, technical, scheduling, small_talk, none. `answer_tier` ∈ fast,
  strong, none. `urgency` ∈ low, medium, high. `profile` must be one of `profiles`. A `null` label field
  is skipped for that question. Ids must be unique. Keep personal data out of `source`.
- `data/snippets.synthetic.jsonl`: 16 **synthetic** examples of the format. Any report that includes synthetic
  items says so at the top.
- Fixture: the recorded synthetic `/v1/systemone` responses (`fixtures/decision/*.jsonl`,
  `scripts/gen_decision_fixtures.py`) are served by a loopback fake.
- Live:
  ```sh
  cargo run -p ssc-bench --features live -- decision --live [--data bench/data/snippets.jsonl] [--jev-url https://api.typesafe.ai]
  ```
  Jev needs `api.typesafe`. Tev1 is skipped if Ollama is older than 0.35 or `tev1:4b` isn't pulled.

## cpu

Samples a process's CPU time (`ps -o time=`) at a fixed interval and reports the mean, p50, p95 and max as % of one
core, and the mean as % of the whole machine. The targets are idle < 2% and capturing < 10% (mean).

- Fixture: `fixtures/cpu/samples.json` (synthetic, `scripts/gen_cpu_fixture.py`).
- Live: no keys or feature needed. Start the agent. The harness samples the idle phase, then asks you to start capturing and
  press Enter:
  ```sh
  cargo run -p ssc-bench -- cpu --live --name ssc-agent [--idle-secs 600 --capture-secs 600 --interval-ms 1000 --save samples.json]
  ```
  `--pid N` picks a process by id. Set `--idle-secs 0` or `--capture-secs 0` to run only one phase.

## answers

Checks cue quality against the prompt rules. The checks are deterministic: only grammar lines are allowed (no intro or filler), 3–7 cues, 2–6 words
per cue, no numbers that aren't in the retrieved chunks or the question (only numbers written as digits are checked), and `NOT_IN_DOCS`
appears exactly when the question is product-specific and nothing was retrieved. Usefulness is the share of
tester thumbs-up ratings (target ≥ 60%). No model acts as judge.

- Fixture / default: `data/answers.synthetic.jsonl` (**synthetic** answers and ratings). Pass your own
  set with `--data FILE`. Format: `{id, kind: cue|fact|objection, question, product_specific,
  chunks: [{doc, text}], answer, rating: up|down|null}`.
- Live: generates a fresh answer per item with the real prompt (`ssc-context`) and model, then runs
  the checks. The ratings in the file are dropped because they rated a different answer:
  ```sh
  cargo run -p ssc-bench --features live -- answers --live [--data FILE] [--model groq | --model ollama:gemma4:e2b]
  ```

## Regenerating fixtures

```sh
python3 bench/scripts/gen_latency_fixture.py > bench/fixtures/latency/calls.json
python3 bench/scripts/gen_decision_fixtures.py
python3 bench/scripts/gen_cpu_fixture.py > bench/fixtures/cpu/samples.json
```
