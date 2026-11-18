# Second Screen Copilot — Build Sheet

Source: `Second Screen Copilot — Product Spec.md` (Nov 11, 2026). Where this sheet and the spec disagree, the spec wins, except for the deviations listed in §10.

---

## 0. Build style (required)

**This project is built by a Claude Code agent swarm. Claude Code implements it automatically, from start to finish.**

- **Lead agent** = the main Claude Code session that opens this file. It never writes feature code. It runs the waves, spawns the workers, merges branches, runs the gates and settles contract disputes.
- **Worker agents** = Claude Code subagents spawned with the `Agent` tool, using `isolation: "worktree"`, one per brief in §5–§7. Each worker gets its brief word for word, plus §3 (Global rules) and §4 (Contracts).
- **Parallel only inside a wave.** A wave starts after the previous wave's gate passes.
- **Contracts first.** Wave 0 freezes every interface between workers, so workers in later waves never wait on each other. They code against the contracts and the mocks.
- **Humans only for the items in §9** (API keys, OS permissions, real calls, Windows hardware, product decisions). Everything else is done by agents.

### Kickoff prompt (paste into Claude Code at the repo root)

```text
Read "Second Screen Copilot — Build Sheet.md" and "Second Screen Copilot — Product Spec.md".
You are the lead agent described in §0. Execute §8 (Orchestration protocol) exactly:
run Wave 0 yourself, pass its gate, then spawn the Wave 1 workers in parallel
(Agent tool, isolation: "worktree"), each with its brief + §3 + §4. Merge, gate,
and continue through Wave 3. Stop only for items listed in §9, and report status
per wave using DONE / DONE_WITH_CONCERNS / BLOCKED / NEEDS_CONTEXT.
```

---

## 1. Outcome and definition of done

The MVP is done when a live sales call gets useful cue answers on the phone with question end → first word **under 1.5 s p50 and 2.5 s p95** (cloud mode).

| Metric | Target | How it's measured (deterministic) |
| --- | --- | --- |
| Question end → first word on phone | p50 < 1.5 s, p95 < 2.5 s | `timing` events (§4.6) aggregated by `bench/latency` |
| PC agent CPU, idle / capturing | < 2% / < 10% | `bench/cpu` samples the process for 10 min |
| Answers rated useful | ≥ 60% | Tester thumbs up/down on the phone, stored in `answer_feedback` |
| Testers who use it on a second real call | 3 of 5 | Session count per tester (manual, §9) |

---

## 2. Repository layout and ownership

One repo, a Cargo workspace plus a pnpm workspace. Every path has exactly **one owning worker**. A worker writes only inside its own paths; anything else goes through a change request (§3.4).

```
/
├── Cargo.toml                    # workspace            — W0
├── package.json, pnpm-workspace.yaml                    — W0
├── scripts/check.sh              # the gate script       — W0
├── contracts/                    # FROZEN after Wave 0   — W0 (lead only after)
│   ├── protocol/                 # JSON Schemas per message type + fixtures/
│   ├── crypto/                   # test vectors (X25519, HKDF, XChaCha20-Poly1305)
│   ├── decision/                 # decision questions + typed result schema + fixtures
│   ├── prompts/                  # cue-mode system prompts (versioned)
│   ├── db/                       # SQLite migrations
│   └── CHANGE_REQUESTS.md
├── crates/
│   ├── ssc-core/                 # shared types, traits, config, timing, mocks — W0
│   ├── ssc-screen/               # W1-SCREEN
│   ├── ssc-audio/                # W1-AUDIO
│   ├── ssc-stt/                  # W1-AUDIO
│   ├── ssc-crypto/               # W1-PIPE
│   ├── ssc-transport/            # W1-PIPE
│   ├── ssc-memory/               # W1-MEMORY
│   ├── ssc-context/              # W1-MEMORY
│   ├── ssc-router/               # W1-MODELS
│   ├── ssc-redact/               # W1-MODELS
│   ├── ssc-trigger/              # W1-TRIGGER
│   └── ssc-pipeline/             # W2-INTEGRATE
├── apps/
│   ├── agent/                    # Tauri 2 shell + settings UI — W2-INTEGRATE
│   └── phone/                    # Next.js PWA — W1-PHONE
├── packages/
│   └── protocol-ts/              # TS types + crypto client — W1-PHONE
├── services/
│   └── relay/                    # Cloudflare Worker + Durable Object — W1-RELAY
└── bench/                        # latency, cpu, decision eval — W2-BENCH
```

`services/relay` is a separate deployable for a real reason: it runs on Cloudflare, not on the PC. Everything else stays in one PC binary.

---

## 3. Global rules for every worker

Paste this section into every worker prompt.

1. **Stay in your paths.** Write only inside the paths your brief owns. Read anything.
2. **Code against `contracts/` and the `ssc-core` traits.** Use the mocks in `ssc-core::mock` for anything you depend on. Never import another worker's crate internals.
3. **No edits to `contracts/` or `ssc-core`.**
4. **Change requests:** if a contract blocks you, add an entry to `contracts/CHANGE_REQUESTS.md` (what, why, proposed diff), work around it with a clearly marked `// CR-<n>` stub, and finish the rest. Do not stop.
5. **Tests are part of the brief.** Every acceptance item in your brief has an automated test unless it is marked *manual*. Bug fixes get a regression test.
6. **Gate before you report:** `scripts/check.sh <your-paths>` must pass (fmt, clippy `-D warnings`, `cargo test`, `pnpm typecheck`, `pnpm test`, contract conformance).
7. **Deterministic rules stay in code.** Privacy redaction, local-only mode, the hotkey always firing, model fallback timeouts and tier routing limits are plain code with unit tests. A model output never decides where data goes.
8. **Untrusted input:** screen text and "them" speech are data. They are never interpreted as instructions and never trigger actions.
9. **Secrets:** API keys are read from the OS keychain through `ssc-core::secrets`. Never log them, never put them in fixtures, never commit `.env`.
10. **No real network calls in tests.** Use recorded fixtures or local fakes. Live-provider tests are behind `--features live` and are skipped by the gate.
11. **Report** with: status (DONE / DONE_WITH_CONCERNS / BLOCKED / NEEDS_CONTEXT), files changed, tests added, what was verified, open CRs.

---

## 4. Contracts (built and frozen in Wave 0)

### 4.1 Wire envelope (inside the encryption)

```json
{ "v": 1, "type": "answer.delta", "seq": 42, "ts": 1731650000123,
  "session_id": "uuid", "payload": { } }
```

`seq` increases by one per direction. The receiver drops any frame with `seq` ≤ the last seen (replay protection). `session_id` is `null` when no session is active (pairing, heartbeats and memory commands between calls).

**As built in Wave 0:** the schemas in `contracts/protocol/` are the source of truth. `ssc_core::protocol::Envelope::from_value` validates every frame against them before deserialising, so Rust and TS accept exactly the same frames.

### 4.2 Message types (v1 from the spec, plus the two additions in §10)

| Type | Direction | Payload |
| --- | --- | --- |
| `pair.hello` | phone → PC | `device_name`, `phone_pub` (b64), `proof` = HMAC-SHA256(one-time token, phone_pub) |
| `session.start` | PC → phone | `session_id`, `profile_id`, `mode`: `cloud` \| `local` |
| `session.end` | PC → phone | `session_id`, `reason` |
| `answer.start` | PC → phone | `answer_id`, `trigger`: `hotkey`\|`question`\|`screen`\|`timer`\|`command`, `format`: `cue`\|`fact`\|`objection`, `intent`, `urgency`: `low`\|`medium`\|`high`, `model` |
| `answer.delta` | PC → phone | `answer_id`, `block`: `cue`\|`detail`, `text` |
| `answer.end` | PC → phone | `answer_id`, `reason`: `done`\|`cancelled`\|`error`, `cues[]` {`text`, `expand?`}, `fact?`, `source_doc?`, `not_in_docs`: bool |
| `transcript.partial` | PC → phone | `speaker`: `me`\|`them`, `text`, `final`, `t0`, `t1` |
| `command` | phone → PC | `action`: `pin`\|`regenerate`\|`shorter`\|`number`\|`followup`\|`expand_all`\|`feedback`\|`memory.list`\|`memory.update`\|`memory.delete`\|`contact.confirm`, `answer_id?`, `text?`, `item?` |
| `heartbeat` | both | `{ echo? }` every 5 s; link is dead after 15 s without one. `echo` = the peer heartbeat's `ts`, for round-trip time |
| `session.summary` *(§10)* | PC → phone | `session_id`, `summary`, `action_items[]`, `followup_email`, `contact_suggestions[]` |
| `memory.snapshot` *(§10)* | PC → phone | `items[]` {`id`, `layer`, `title`, `body`, `updated_at`} |

Deliverables: one JSON Schema per type in `contracts/protocol/`, at least one valid and one invalid fixture each, Rust `serde` types in `ssc-core::protocol`, TS types in `packages/protocol-ts` (W1-PHONE generates them from the schemas). A conformance test runs every fixture through both languages.

### 4.3 Crypto and pairing

- QR payload: `{ "v":1, "urls":["ws://<lan-ip>:<port>", "wss://<relay>/r/<room>"], "pc_pub":"b64", "token":"b64(32B)", "exp": <unix s, now+120> }`. With `pwa_url` set, the QR text is `<pwa_url>#pair=<base64url(that JSON)>` (CR-19).
- Phone makes an X25519 keypair and sends `pair.hello` **in plaintext, once**. The PC checks `proof` and `exp`, then burns the token.
- Shared secret = X25519(own_priv, peer_pub). Keys = HKDF-SHA256(ikm = shared, salt = token, info = `"ssc/v1/pc->phone"` and `"ssc/v1/phone->pc"`), 32 bytes each.
- Frame on the wire: `0x01 | nonce(24 random bytes) | XChaCha20-Poly1305(key, nonce, plaintext = envelope JSON, aad = UTF-8 session_id (or "") ‖ seq as u64 BE)`.
- Relay room id = `HKDF-SHA256(ikm = token, salt = empty, info = "ssc/v1/room")`, 16 bytes, hex. Full algorithm: `contracts/crypto/README.md`.
- Rust: `x25519-dalek`, `hkdf`, `sha2`, `chacha20poly1305`. TS: `libsodium-wrappers` (scalarmult and AEAD) plus WebCrypto HKDF.
- `contracts/crypto/vectors.json`: fixed keys → expected shared secret, derived keys and ciphertext. Both languages must reproduce them byte for byte.
- Paired devices stored as `{device_id, phone_pub, keys, created_at}` in the keychain. They can be revoked from the settings UI.

### 4.4 Rust traits (`ssc-core::traits`), with a mock for each in `ssc-core::mock`

As built (`crates/ssc-core/src/traits.rs` is authoritative):

```rust
trait ScreenSource   { fn snapshot(&self) -> Result<ScreenSnapshot>; }
trait AudioSource    { fn start(&self, tx: mpsc::Sender<AudioChunk>, cancel: CancellationToken) -> Result<()>; } // 16 kHz mono PCM16
trait SttEngine      { fn start(&self, rx: mpsc::Receiver<AudioChunk>, tx: mpsc::Sender<TranscriptSegment>, cancel: CancellationToken) -> Result<()>; }
trait DecisionModel  { fn id(&self) -> &str; async fn decide(&self, input: &DecisionInput) -> Result<Decision>; }
trait AnswerModel    { fn id(&self) -> &str; fn is_cloud(&self) -> bool;
                       async fn stream(&self, prompt: &Prompt, tx: mpsc::Sender<Token>, cancel: CancellationToken) -> Result<StreamStats>; }
trait Embedder       { fn dim(&self) -> usize; fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>; }
trait MemoryStore    { fn list(..); fn upsert(..); fn delete(..); fn search(&self, query: &str, profile_id: &str, k: usize) -> Result<Vec<Chunk>>; }
trait Transport      { async fn send(&self, session_id: Option<Uuid>, message: Message) -> Result<()>;   // assigns seq + ts
                       fn take_incoming(&self) -> Option<mpsc::Receiver<Envelope>>; }
trait Clock          { fn now_ms(&self) -> u64; }
```

Errors are `ssc_core::SscError`. The router acts on `RateLimited`, `Timeout`, `Unavailable` and `Policy`.

`Decision` = `{ needs_answer: (bool, p), intent: (Intent, p), answer_tier: (Fast|Strong|None, p), profile: (ProfileId, p), urgency: (Low|Medium|High, p) }`.

### 4.5 Decision contract

`contracts/decision/questions.json` holds the five questions from the spec with their allowed choices. The input is under 500 tokens: the last "them" question, a screen diff of at most 2 KB trimmed to fit, and the active profile names. Jev (cloud) and Tev1 4B (Ollama) both speak the TypeSafe `/v1/systemone` format, so one adapter with a configurable `base_url` and `model` serves both. **Not verified:** the exact request and response shape of `/v1/systemone`. W1-MODELS confirms it from the provider docs and records fixtures before writing the adapter (see §9).

### 4.6 Timing events

Every stage emits `timing { answer_id, stage, t_ms }`. `stage` is one of: `speech_end`, `transcript_final`, `trigger_fired`, `decision_done`, `context_ready`, `model_request`, `first_token`, `frame_sent`, `phone_rendered`. To avoid cross-device clock skew, the phone reports only `delta_ms` (first delta received → first paint) in a `feedback` command with `item.kind = "timing"`. The PC computes `phone_rendered = frame_sent + RTT/2 + delta_ms`, with RTT from the heartbeat `echo`. Events are written as JSONL to `~/.ssc/timing/<date>.jsonl` and kept in a ring buffer for the debug panel.

### 4.7 SQLite schema (`contracts/db/0001_init.sql`)

Tables: `about_me`, `profiles`, `docs`, `chunks` (+ `vec_chunks` sqlite-vec virtual table, 384 dimensions, in `0002_vec.sql` because it needs the extension loaded), `contacts`, `accounts`, `sessions`, `session_items` (summary, action items, Q&A, follow-up), `transcripts` (only if the user chose to keep them), `answers`, `answer_feedback`, `pending_contact_suggestions`. No column stores raw audio or images. A test asserts this.

### 4.8 Cue-mode prompts (`contracts/prompts/`)

`cue.v1.md`, `fact.v1.md`, `objection.v1.md`, `expand.v1.md`, `summary.v1.md`, `followup.v1.md`. The rules from the spec are written into the prompts: 3–7 cues of 2–6 words, no full sentences, no intro or summary lines, no filler, never invent numbers, "Not in your docs" when retrieval comes back empty for a product-specific question, first block capped at about 40 tokens. The fixed prefix comes first so prompt caching works.

---

## 5. Wave 0 — Contracts and scaffold (lead agent, sequential)

**Owns:** root files, `scripts/`, `contracts/`, `crates/ssc-core/`.

Build:
1. `git init`, `.gitignore` (target/, node_modules/, .env*, *.sqlite, .ssc/), Cargo and pnpm workspaces, empty crates and apps that compile.
2. Everything in §4: schemas, fixtures, crypto vectors (generated once with a reference script and committed), traits, mocks, protocol types, timing, config, `secrets` wrapper over `keyring`, the migration and the prompts.
3. `scripts/check.sh`: fmt, clippy, tests, typecheck, and the contract conformance check (every fixture parses or fails exactly as labelled, in Rust).
4. CI: GitHub Actions matrix `macos-latest` + `windows-latest` running `scripts/check.sh`. Windows-only code is compiled and unit-tested there.

**Gate 0:** `scripts/check.sh` is green on both CI platforms. Mocks cover every trait. The lead commits `contracts-v1` and tags it.

---

## 6. Wave 1 — Components in parallel (8 workers)

Every worker codes against the mocks. None depends on another worker's output.

### W1-SCREEN — Screen reader + diff
**Owns:** `crates/ssc-screen/`
- Windows: UI Automation through the `uiautomation` crate. Walk the focused window's tree, collect names, values and text patterns, with a node cap and a 50 ms time cap.
- macOS: AXUIElement through `accessibility-sys` / `core-foundation`.
- OCR fallback only on regions that changed: Windows.Media.Ocr (`windows` crate) and Apple Vision (`objc2-vision`). Use it when the accessibility text is empty or under 20 characters.
- `diff(prev, next) -> ScreenDiff`, line-level, capped at 2 KB, newest content kept first.
- Password fields (`IsPassword` / `AXSecureTextField`) are never read.

**Accept:** diff unit tests (no change → empty, append, replace, cap); password-field exclusion test using a fake tree; snapshot p95 under 50 ms on the fixture tree (bench test); *manual:* reads a real Chrome or Zoom window on macOS.

### W1-AUDIO — Audio capture + speech-to-text
**Owns:** `crates/ssc-audio/`, `crates/ssc-stt/`
- Windows: WASAPI loopback for "them" plus the mic for "me", as two separate streams (`wasapi` crate). macOS: ScreenCaptureKit system audio (`screencapturekit` crate) plus `cpal` for the mic.
- Resample to 16 kHz mono PCM16. VAD (`webrtc-vad` or Silero via `ort`) emits `speech_end`.
- STT: a Deepgram streaming adapter (WebSocket, interim results on) and a whisper.cpp adapter (`whisper-rs`) for local mode. Both emit partial and final `TranscriptSegment`s carrying their speaker.
- Raw audio is never written to disk. A test checks that no file is created.

**Accept:** resampler tests; VAD end-of-speech on fixture WAVs within ±100 ms of the label; Deepgram adapter against a recorded WebSocket fixture; whisper adapter on a fixture WAV (feature `local-stt`); the two speakers never mixed.

### W1-PIPE — Crypto + PC transport + pairing
**Owns:** `crates/ssc-crypto/`, `crates/ssc-transport/`
- Everything in §4.3 on the Rust side: keygen, HKDF, seal and open, replay window, the pairing state machine, QR payload generation (`qrcode` crate → SVG for the settings UI).
- LAN WebSocket server (`tokio-tungstenite`) plus mDNS advertising (`mdns-sd`, `_ssc._tcp`).
- Relay client: connects out to `wss://relay/r/<room>`. LAN is preferred and the relay is the fallback. The switch is transparent to `Transport` users.
- Token batching: coalesce `answer.delta` every 50 ms or 32 characters, whichever comes first.
- Heartbeat and dead-link detection.

**Accept:** byte-for-byte match with `contracts/crypto/vectors.json`; tampered, replayed and expired frames are rejected; pairing with a wrong proof or an expired token fails; loopback LAN round-trip p95 under 20 ms in a test; relay fallback when the LAN socket is killed (against a local fake relay).

### W1-PHONE — PWA + TS protocol/crypto
**Owns:** `apps/phone/`, `packages/protocol-ts/`
- `protocol-ts`: types generated from `contracts/protocol` (`json-schema-to-typescript`), the crypto client (§4.3), and an envelope codec. It must pass the same vectors and fixtures as Rust.
- PWA (Next.js, static export, service worker): QR scan (`@zxing/browser`), pairing, connection that tries LAN then the relay.
- Answer card: append-only rendering, no layout reflow; numbered cues; tap to expand one cue; bold fact plus source doc; "Not in your docs" state; vibration (`navigator.vibrate`) on the first token of a new answer, with the pattern set by `urgency`.
- Actions: pin, regenerate, shorter, give me a number, expand all, thumbs up/down, follow-up by typing or voice (Web Speech API where available, typing otherwise).
- Screens: live, session summary (follow-up email with a copy button), memory browser (list, edit, delete through `command` + `memory.snapshot`), contact confirmation (one tap).
- Sends a `phone_rendered` timing event on the first paint of each answer.

**Accept:** vitest for the codec, crypto vectors and fixtures; Playwright against a mock PC server (`packages/protocol-ts/mock-server`): pair → stream an answer → expand → pin → summary; render-time test (a delta applied in under 16 ms over 200 deltas); Lighthouse PWA installable.

### W1-MEMORY — Memory store + retrieval + context builder
**Owns:** `crates/ssc-memory/`, `crates/ssc-context/`
- `rusqlite` with the bundled build, the §4.7 migration, and `sqlite-vec`. Embeddings from a small local model (`fastembed-rs`, bge-small or similar) behind `Embedder`.
- Doc upload: PDF, DOCX, MD and TXT → text → chunks of about 400 tokens with overlap → embeddings. Kept per profile.
- `search(query, profile, k=5)`, under 30 ms on 10k chunks with the index in memory.
- Context builder: About Me + active profile + top 3–5 chunks + last 60–90 s of transcript + screen diff, each part with its own token budget (`tiktoken-rs` count). When over budget, the oldest transcript and lowest-ranked chunks are cut first, and the newest transcript always stays. Outputs a fixed prefix (cacheable) plus a dynamic part under about 300 tokens for local mode.
- Contact suggestions go to `pending_contact_suggestions` until confirmed. Nothing is saved silently.

**Accept:** budget tests (the newest transcript survives every overflow case); retrieval quality on a fixture set (recall@5 ≥ 0.8 on 30 labelled queries); assembly p95 under 30 ms; CRUD and delete really removes rows and vectors; a schema test shows no audio or image columns.

### W1-MODELS — Model router + decision adapter + redaction
**Owns:** `crates/ssc-router/`, `crates/ssc-redact/`
- Answer adapters (streaming, warm HTTP/2 clients via `reqwest`): Groq (OpenAI-compatible; gpt-oss-120b, Qwen3.8 27B), Gemini Flash-Lite (paid tier only for client calls; the free tier is gated behind a `testing` flag), Ollama (Gemma 4 E2B live, Gemma 4 12B post-call), and an optional strong cloud model for the `Strong` tier.
- Decision adapter (§4.5) for Jev and Tev1 4B.
- Routing rules, all deterministic and unit-tested with a fake clock:
  - local-only mode → only Ollama and whisper. A test proves no cloud client is even built.
  - Groq returns 429 or sends no first token within 1,000 ms → Gemini Flash-Lite.
  - Jev timeout 800 ms; two timeouts in a row → Tev1 4B until the network check passes again.
  - Tier `Strong` → fast model streams the first cues, then the strong model streams `block: detail`.
  - Offline: live answers fire from the hotkey and the end-of-question rule; Tev1 runs in the background and only tags.
  - Groq budget tracker (requests per minute and day, tokens per minute and day) switches to the fallback before the limit is hit.
- Speculative drafting: start on a partial transcript and cancel and restart only if the final transcript changes meaning (normalised edit distance over 0.2).
- `ssc-redact`: card numbers (Luhn-checked), 11-digit BVN and NIN, password-like `key=value` pairs, emails optional. Runs on every prompt before any cloud call, when enabled.

**Accept:** every routing rule has a test; redaction tests with positive and negative cases (Luhn false positives); adapters tested against recorded SSE and NDJSON fixtures; cancellation stops the stream within 50 ms.

### W1-TRIGGER — Trigger engine
**Owns:** `crates/ssc-trigger/`
- Four events: hotkey (always fires, skips the decision model, tier = fast unless decision confidence is below threshold, in which case strong), a question in "them" speech, a big screen change (diff over N lines, debounced), and the passive timer (off by default).
- Question detection: end of speech plus a silence window (250–400 ms) plus question-word and prosody heuristics on "them" speech, then a `DecisionModel` call (cloud) for `needs_answer` / `intent` / `tier` / `profile` / `urgency`.
- Debounce and dedupe: no second answer for the same question within 5 s.
- Maps `intent` → answer format (§ spec "When it switches format"). Small talk → nothing shown.

**Accept:** table-driven tests over fixture transcripts (question / not a question / small talk); the hotkey fires even with a decision mock that always errors; the decision model never changes local-only mode or redaction (type-level: `Decision` has no field that can).

### W1-RELAY — Blind relay
**Owns:** `services/relay/`
- Cloudflare Worker plus one Durable Object per room (`wrangler`). Pairs exactly two sockets per room and forwards binary frames as they are. No logging of payloads, no storage, room expires after 24 h idle.
- Room id = HKDF of the pairing secret (the PC computes it, the phone gets it from the QR code). The relay never sees keys.
- Rate limit per room. Max frame 64 KB.
- Self-hosting: `README` with `wrangler deploy` steps.

**Accept:** Miniflare tests: two clients relay frames; a third client is rejected; frames over 64 KB are dropped; nothing is written to storage (assert via the DO storage mock). *Deploy is a §9 item.*

**Gate 1:** all 8 branches merged in the order PIPE → PHONE → MEMORY → MODELS → TRIGGER → SCREEN → AUDIO → RELAY, `scripts/check.sh` green on both platforms after each merge, every CR resolved or explicitly deferred by the lead.

---

## 7. Wave 2 — Integration (3 workers), Wave 3 — Verification

### W2-INTEGRATE — Pipeline + Tauri shell
**Owns:** `crates/ssc-pipeline/`, `apps/agent/`
- Wire the ten-step data flow from the spec with real components and `tokio` channels: capture → diff → trigger → context → route → push → (phone) → commands → store → follow-up.
- Session end: summary, action items, follow-up draft and contact suggestions through `summary.v1` / `followup.v1` (strong cloud model, or Gemma 4 12B after the live models are unloaded) → `session.summary`. Transcripts are kept only if the user turned that on.
- Commands from the phone: regenerate, shorter, number, follow-up, expand all, memory edits, contact confirmation.
- Tauri 2 shell: tray icon (visible or hidden by hotkey), global hotkeys (capture on/off, answer now, hide tray) via `tauri-plugin-global-shortcut`, settings UI (About Me, profiles, doc upload, API keys into the keychain, mode: cloud / local-only, redaction on/off, paired devices + QR, relay URL).
- Debug panel: p50/p95 per stage from the timing ring buffer.
- macOS permission prompts (Screen Recording, Accessibility, Microphone) with a clear first-run checklist.

**Accept:** end-to-end test with every external dependency mocked (fixture audio → fixture screen → mock models → phone mock client): the answer arrives, timing events are complete and in order, and the summary is produced; a local-only E2E with the network disabled (no outbound socket except loopback); CPU idle check in a test harness.

### W2-BENCH — Latency, CPU and decision eval harness
**Owns:** `bench/`
- `bench/latency`: replays fixture calls through the real pipeline and reads the timing JSONL → p50/p95 per stage plus end to end. Fails if the cloud p50 is over 1.5 s or the p95 is over 2.5 s (`--live` only).
- `bench/ttft`: time to first token for Groq, Gemini Flash-Lite and Gemma 4 E2B (with and without the prefix cache) — the spec's "next steps".
- `bench/decision`: runs Jev and Tev1 4B on the labelled set (`bench/data/snippets.jsonl`, 200 items from §9) → accuracy per question, plus p50/p95 latency. Writes a markdown report.
- `bench/cpu`: samples the agent's CPU while idle and while capturing.
- `bench/answers`: an eval of cue quality against the prompt rules (cue count 3–7, words per cue 2–6, no invented numbers checked against the retrieved chunks, "Not in your docs" when it should be). The rule checks are deterministic; usefulness comes from tester ratings, not a model judge.

**Accept:** each harness runs on fixtures in the gate (no network) and produces its report; `--live` modes documented.

### W2-HARDEN — Privacy and security pass
**Owns:** nothing new. It writes tests in a `tests/security/` folder inside each crate, and only fixes code by CR to the lead.
- Threat model for the pairing, relay and keychain paths. Check: the relay sees ciphertext only, keys never leave the keychain, nothing reaches a cloud client in local-only mode, redaction runs before every cloud call, screen and speech text can't trigger actions (prompt-injection fixtures), no audio or screenshots on disk.

**Accept:** every check above has an automated test; findings filed as CRs with severity.

**Gate 2:** E2E green on both platforms; CRs from W2-HARDEN fixed (by the owning worker, re-spawned with the CR as its brief) or deferred with a reason.

### Wave 3 — Independent review + fix (lead)
1. Spawn one fresh reviewer agent (no worktree, read-only) with the whole repo, the spec and this sheet. It looks for correctness, spec gaps, security issues and regressions, and reports concrete findings with file:line.
2. The lead sends each confirmed finding to the owning worker (re-spawned with the finding as its brief), merges, and re-runs the gates.
3. One review round. If a major issue remains, fix it and review that area once more; then stop and report.
4. Final report: status, gate results per platform, bench numbers on fixtures, open CRs, and the §9 list of what the human must do next.

---

## 8. Orchestration protocol (lead agent)

```
Wave 0  lead builds §5                    → Gate 0 → tag contracts-v1
Wave 1  spawn 8 workers in parallel       → merge in the §6 order, gate after each merge
Wave 2  spawn INTEGRATE, BENCH, HARDEN    → HARDEN starts after INTEGRATE's first E2E is green
Wave 3  1 reviewer → fix → gate           → final report
```

- **Spawn:** `Agent(subagent_type: "general-purpose", isolation: "worktree", prompt: <brief> + §3 + §4 + "contracts are at tag contracts-v1")`. One worker per brief. Workers in the same wave are spawned in one message so they run in parallel. Use the best available model (no downgrade).
- **Merge:** each worker ends on a branch `w/<brief-id>`. The lead rebases it on `main`, runs `scripts/check.sh`, and merges only when it's green. On a red gate, the lead re-spawns that worker with the failure output as its brief (max 2 retries, then BLOCKED with the log).
- **Change requests:** the lead reviews `contracts/CHANGE_REQUESTS.md` between waves, never mid-wave. An accepted CR becomes `contracts-v1.x`, and every affected worker is re-spawned with only the diff.
- **Commits:** one commit per merged brief, message `<brief-id>: <summary>`, ending with the project's co-author line. Push only if a remote exists and the human has asked for pushes.
- **Never:** force-push, `--no-verify`, commit secrets, or run `sudo`. Commands that need `sudo` go into the final report for the human.

---

## 9. Human-only items (the swarm stops and asks only for these)

| Item | When it's needed | Why agents can't do it |
| --- | --- | --- |
| Groq, Gemini (paid), Jev/TypeSafe, Deepgram, optional strong-model API keys → put into the keychain from the settings UI | Wave 2 `--live` runs | They are the user's own accounts |
| `ollama pull` for Gemma 4 E2B, Gemma 4 12B, Tev1 4B | Local-mode live runs | Large downloads on the user's machine (agents can run it if approved) |
| Confirm the `/v1/systemone` request/response format from TypeSafe's docs or a live call | Before W1-MODELS finalises the decision adapter | Not verified; the spec doesn't define the wire shape |
| macOS permissions: Screen Recording, Accessibility, Microphone | First manual run | OS dialogs need a human click |
| A Windows machine for manual capture tests | Gate 1 manual items | MVP is Windows-first, but the dev machine is an M1 Pro Mac; CI only covers unit tests |
| Cloudflare account + `wrangler login` + deploy relay | Off-LAN testing | User's account |
| Label about 200 call snippets (question / no question, intent, tier) | `bench/decision` | Needs real calls and human judgment |
| 5 tester calls, Nigerian English and Pidgin included | MVP success metrics | Real users |
| Open product questions: first niche, pricing, macOS in v1, watch app vs React Native | Before launch, not before build | Business decisions |

---

## 10. Deviations from the spec (lead must confirm these at Gate 0)

1. **Two message types added** — `session.summary` and `memory.snapshot`. The spec requires the phone to show summaries and to view and edit every memory item, but the v1 message list has no PC → phone frame for either.
2. **`command.action` values listed explicitly** (§4.2), including `feedback` for usefulness ratings and phone render timing. The spec needs both but doesn't define them.
3. **macOS backends built alongside Windows.** The spec says Windows first, but the only dev machine named is a Mac. Both are built behind the same traits; Windows is verified in CI plus a manual pass.
4. **LLM access uses the user's own API keys and Ollama**, as the spec requires (BYOK is a product feature), not the local Claude Code route.
5. **Contract details fixed in Wave 0:** `session_id` can be `null`; `heartbeat` has an optional `echo` (for round-trip time); `answer.start.trigger` adds `command` (answers caused by a phone action); phone render time is reported as `delta_ms`, not an absolute timestamp; `apps/agent` is a plain binary placeholder until W2-INTEGRATE turns it into the Tauri app.
6. **v1 ships macOS only** (user decision, 2026-11-17). Windows code stays behind the same traits and is not deleted, but Windows builds, cross-checks, CI and manual passes are out of scope until a later build. Supersedes the Windows half of item 3.
7. **CR-4 resolved: LAN `wss://` on a public sub-domain with a real certificate** (user decision, 2026-11-17; lowest latency and strongest transport security of the three options). The agent keeps a DNS-only (not proxied) A record on the user's Cloudflare sub-domain pointing at the PC's LAN IP, updates it when the IP changes, and obtains a Let's Encrypt certificate by DNS-01 using a Cloudflare API token scoped to DNS edit on that zone (stored in the keychain). The relay remains the fallback.
8. **Answer models use one OpenAI-compatible chat-completions adapter for Groq, Gemini and Ollama; Claude keeps a native Messages adapter** (needed for prompt caching and Claude-specific parameters).
9. **macOS screen capture moves to ScreenCaptureKit** (replacing the deprecated `CGWindowListCreateImage`), sharing the Screen Recording permission with system-audio capture.
