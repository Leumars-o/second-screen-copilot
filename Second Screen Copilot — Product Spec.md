# Second Screen Copilot — Product Spec

Nov 11, 2026 · @albert Johnson

## Overview

Second Screen Copilot is a real-time AI assistant for live calls: a light agent on the PC reads the screen and the conversation, and answers stream to the user's phone in under 1.5 seconds. Putting the output on a second device means nothing ever shows up in a screen-share, with no overlay tricks needed.

**Who it's for:** sales reps, founders on investor calls, account managers, and contact-center and people wanting to put their thoughts together.

**How it beats Cluely:**

| Area | Cluely | Second Screen Copilot |
| --- | --- | --- |
| Where answers appear | Overlay on the PC, hidden with paid stealth tech | Phone (or watch): nothing on the shared screen |
| Speed | Reported 4–7 s in a competitor's tests | Target under 1.5 s to the first word |
| Models | Their cloud | Bring your own API key, or fully local |
| Data | Central cloud; 2025 breach exposed 83,000+ users | Local-first, end-to-end encrypted relay |

Sources for the Cluely column: [itsconvo.com](https://www.itsconvo.com/blog/best-cluely-alternatives) (latency, a competitor's claim), [fast.io](https://fast.io/resources/cluely-ai-review-2026/) (breach).

## Architecture

&#91;embedded content: system architecture · 6 PC components, phone, relay\]

Everything heavy runs on the PC; the phone only renders the stream and sends commands back. The relay carries encrypted bytes only when the two devices aren't on the same network.

| Component | Job | Key detail |
| --- | --- | --- |
| Screen reader | Turns the focused window into text | Uses the accessibility APIs first; OCR only on regions that changed |
| Audio pipeline | Transcribes the call as it happens | "Me" (mic) and "them" (system audio) kept apart, so questions from the other side can be detected |
| Trigger engine | Decides when to call the model | Decision model (Jev in cloud mode, Tev1 4B offline) answers needs\_answer, intent and tier in one call; the hotkey always fires |
| Memory store | Long-term context | Local SQLite + vector index; the user sees and edits everything |
| Context builder | Builds one tight prompt | Token budget per part; the newest transcript always wins |
| Model router | Picks the model and streams tokens | Cloud: Groq answers, Gemini Flash-Lite fallback. Offline: Gemma 4 E2B. All on the user's own keys |
| Phone app | The glanceable second screen | PWA first; vibration on new answer; voice follow-ups |
| Blind relay | Reaches the phone when it's off the local network | Forwards encrypted frames only; can be self-hosted |

## Decision model

Yes, add one. A decision model becomes the core of the Trigger engine. **Jev** is the default whenever cloud mode is on, and **Tev1 4B** runs locally for offline and local-only use. A decision model returns typed answers with probabilities instead of text, and answers up to 64 questions about the same input in one call. So one call per trigger replaces a stack of hand-written heuristics.

**What it decides on each trigger, in one call:**

- `needs_answer` (true/false): is the other person asking something the user should answer?
- `intent` (choice): pricing, objection, technical, scheduling, small talk, none
- `answer_tier` (choice): fast model, strong model, or no answer
- `profile` (choice): which profile's docs to search
- `urgency` (score): low / medium / high; controls the phone's vibration

Some rules stay in code and never go to the model: privacy redaction, local-only mode, and the hotkey always firing. A probabilistic guess should never decide where data goes.

| Option | Runs | Size | Context | Accuracy\* | Cost | Best for |
| --- | --- | --- | --- | --- | --- | --- |
| Jev 1.13 / 1.14 (TypeSafe) | Paid API only | Hosted; no weights | 32K–262K, depending on the provider | 76.0% | About $0.04–0.06 per 1M input tokens; output free | Default in cloud mode |
| Nimble 9B (Bespoke Labs) | Local, Ollama | 9.5 GB download; about 7 GB VRAM at Q4 | Up to 256K | 75.7% | Free | 16 GB+ machines that aren't running a local answer model |
| Tev1 4B (Together AI) | Local, Ollama | 4.5 GB | About 2K | 73.3% | Free | **Default offline / local-only** |
| Tev1 0.8B (Together AI) | Local, Ollama | 812 MB | About 2K | 63.5% | Free | Pre-filter only, on low-end laptops |

\*Mean accuracy across Bespoke Labs' 13 public datasets (3,880 decisions), as published on Ollama. These are general classification tasks, not calls like ours.

**Chosen setup: Jev in cloud mode, Tev1 4B offline.** The dev machine is a 16 GB M1 Pro MacBook, which rules out Nimble 9B.

- **Why Jev by default:** TypeSafe reports 70–500 ms end to end (vendor numbers), plus an estimated 150–300 ms network trip from Lagos. That's likely faster than a 4B model on an M1 Pro that is also running transcription. It's also highest on the shared benchmark (76.0%) and costs about $0.08 for 2,000 calls of 1,000 tokens.
- **Why Tev1 4B offline:** it's free, private and doesn't depend on the internet. Estimated 300–800 ms on an M1 Pro with short inputs; not measured yet.
- **Switching:** use Jev when cloud mode is on and the network responds; fall back to Tev1 4B when offline, in local-only mode, or after two Jev timeouts in a row (timeout: 800 ms).
- **Same contract:** both use the TypeSafe `/v1/systemone` format (state + questions), so switching is a change of base URL and model name, not new code.
- **Keep inputs short:** under about 500 tokens (last question + a small screen diff), with every trigger question in one call.
- **Risks:** Tev1 is experimental and Jev 1.13 is labelled experimental. Screen text and the other person's speech are untrusted, so the decision model only routes and never triggers actions. Test both on Nigerian English and Pidgin calls before launch.

**Next steps:**

- [ ] Label about 200 snippets from real calls (question / no question, intent, tier)
- [ ] Run Jev and Tev1 4B on that set; compare accuracy and p50/p95 latency on the M1 Pro (16 GB)
- [ ] Lock the default and the confidence threshold below which the hotkey falls back to the strong model

Sources: [Ollama: Tev1](https://ollama.com/library/tev1:0.8b) (accuracy, sizes), [Ollama: Tev1 notes](https://ollama.com/library/tev1:0.8b-mxfp8) (context, limits), [Ollama: Tev1 caveats](https://ollama.com/library/tev1:0.8b-q8_0), [AlternativeTo](https://alternativeto.net/news/2026/9/ollama-introduces-support-for-jev-style-decision-models-for-local-ai-processing/) (91 ms), [LLM Configurator](https://llmconfigurator.com/en/models/nimble) (VRAM), [TypingMind](https://www.typingmind.com/self-hosted-ai/ollama/nimble) (Nimble size, context), [AI/ML API](https://aimlapi.com/models/typesafe-jev-latest) (Jev price), [AI/ML API: Jev 1.14](https://aimlapi.com/models/typesafe-jev-1-14) (context), [Kanerika](https://kanerika.com/blogs/jev-ai-model/) (Jev latency, price).

## Answer models

In cloud mode, Groq's free tier writes the live answers. Offline, Gemma 4 E2B writes them, and Gemma 4 12B handles post-call work. Cloud models run on each user's own API keys, so every user gets their own free allowance.

| Situation | Answer model | First word (estimate) | Cost |
| --- | --- | --- | --- |
| Cloud mode (default) | Groq free tier: gpt-oss-120b or Qwen3.8 27B | \~0.4–0.7 s | Free: 30 requests/min, 1,000/day, 8K tokens/min, 200K tokens/day |
| Groq over its limit, or no first token within \~1 s | Gemini Flash-Lite (paid tier for client calls) | \~0.5–0.8 s | $0.10 / $0.40 per 1M input/output tokens (2.5 Flash-Lite) |
| Offline / local-only | Gemma 4 E2B on Ollama, with the fixed prompt prefix cached | \~1–2 s | Free |
| Hard technical question (decision tier = strong) | The fast model sends the first cues; a stronger cloud model (Sonnet-class) streams the detail | First cues as above | Paid, optional |
| After the call (summary, follow-up email) | Stronger cloud model, or Gemma 4 12B offline once the live models are unloaded | Not time-critical | Free offline |

**Notes:**

- **Groq capacity:** a cue answer uses about 1,000 tokens in and 60 out, so the free tier covers about 190 answers a day, or 4–6 call-hours.
- **Gemini privacy:** the free tier may use prompts to improve Google products. Use it for testing only; client call content goes only to the paid tier.
- **No Gemma 4 12B during calls** on the 16 GB M1 Pro: about 7.6 GB at Q4 and several seconds to the first word.
- **Offline triggering:** live answers fire from the hotkey and an end-of-question rule, not the decision model, which saves about 0.6 s. Tev1 4B still tags and routes in the background.
- **Limits change:** free-tier limits change often; check the Groq and AI Studio consoles before relying on them.

**Next steps:**

- [ ] Add Groq and Gemini Flash-Lite to the benchmark script; measure time to first token from Lagos
- [ ] Measure Gemma 4 E2B time to first token on the M1 Pro with prefix caching on

Sources: [Groq free-tier limits (benchlm)](https://benchlm.ai/md/free-tier/groq.md), [Groq data policy (Developers Digest)](https://www.developersdigest.tech/models/free-api/groq), [Gemini free plan (CostBench)](https://www.costbench.com/software/llm-api-providers/google-gemini-api/free-plan/), [Gemini pricing (FindSkill)](https://www.findskill.ai/blog/gemini-api-pricing-guide/), [Gemma 4 on Ollama (bestllmfor)](https://bestllmfor.com/guide/gemma-4-en-local-ollama-installation-perfs/).

## Answer format: cue mode

Cue mode is the default answer format. The copilot gives the user cues to speak from, not scripts to read aloud. Long answers are slow to read at a glance, invite generic AI filler, and make the user sound like they're reading. Short cues keep the user in charge of the conversation.

**How a cue answer looks:**

- 3–7 numbered cue bullets of 2–6 words each, readable in about 2 seconds ("1. Requirements → 2. Design → 3. Build → 4. Test → 5. Deploy → 6. Maintain")
- Tap a bullet to expand it to 1–2 supporting points or one example; nothing more
- One key fact or number shown in bold when the question asks for one (a price, a limit, a date), taken from the user's docs
- If the docs don't cover a product-specific question, the card says "Not in your docs". It doesn't guess.

**When it switches format (set by the decision model):**

| Question type | Format |
| --- | --- |
| Explain / walk me through | Cue outline, expandable |
| Factual (price, limit, date) | One bold fact + source doc name |
| Objection or pushback | 2–3 response angles, one line each |
| Small talk / no answer needed | Nothing shown |

**Prompt rules for the answer model:** no full sentences in cues, no intro or summary lines, no filler words, never invent numbers. A word cap on each cue keeps the fast model from padding. A full-text answer is available only by tapping "Expand all"; it is never shown by default.

## Data flow

A single answer takes ten steps from the PC to the phone. Only the trigger decides whether the model runs, which keeps cost and noise down.

1. **Capture:** the screen reader produces structured text from the focused window. The audio pipeline produces partial and final transcript segments, labelled "me" (mic) and "them" (system audio).
2. **Diff:** the agent compares the new text with its last snapshot and keeps only what changed, up to about 2 KB.
3. **Trigger:** one of four events fires: a hotkey, a question detected in "them" speech, a big screen change, or a timer for passive mode (off by default).
4. **Assemble context:** About Me, the active profile (for example "Sales, product X"), the top 3–5 doc chunks retrieved for the question, the last 60–90 s of transcript and the current screen diff.
5. **Route:** the model router picks local or cloud by profile and session settings, and starts streaming.
6. **Push:** each token batch (every \~50 ms) goes over the WebSocket as an encrypted `answer.delta` frame.
7. **Render:** the phone updates the answer card live and vibrates on the first token of a new answer.
8. **Act on the phone:** pin, regenerate, "shorter", "give me a number", or ask a follow-up by voice or typing. Each goes back to the PC as a `command` frame.
9. **Store:** at session end, the transcript and answers are summarised into the memory store; raw audio is never kept.
10. **Follow-up:** the phone shows the summary, action items and a draft follow-up email.

**Message types (v1):** `pair.hello`, `session.start`, `session.end`, `answer.start`, `answer.delta`, `answer.end`, `transcript.partial`, `command`, `heartbeat`.

## Latency budget

In cloud mode, the target is under 1.5 s (p50) from the end of a spoken question to the first word on the phone, with p95 under 2.5 s. Offline mode on the M1 Pro targets about 2–2.5 s. These are design targets to check against real measurements, not benchmarks.

| Stage | Target (ms) | Main technique |
| --- | --- | --- |
| End-of-question detection | 250–400 | Voice-activity detection plus a short silence window; question-word heuristics |
| Final transcript of the last phrase | 150–300 | Streaming speech-to-text; acts on partial transcripts and doesn't wait for the full sentence |
| Context assembly and retrieval | 30–80 | Local vector index kept in memory; profile prompt prepared ahead of time |
| Model time to first token | Cloud 200–700 · offline 300–1,500 | Cloud: Groq free tier, Gemini Flash-Lite as fallback. Offline: Gemma 4 E2B with a cached prompt prefix and a dynamic part under \~300 tokens |
| Transport to the phone | 5–20 | WebSocket on the local network; relay only as a fallback |
| Phone render | <16 | Append-only text card, no layout reflow |

**Techniques:**

- **Speculative drafting:** start the model on the partial transcript and restart only if the final transcript changes the meaning.
- **Two-tier answers:** a one-line "say this" from a fast model first, then the detail from a stronger model underneath.
- **Warm connections:** keep the HTTP/2 or WebSocket connection to the model provider open, and reuse prompt caching for the fixed context.
- **Short outputs by default:** cap the first block at about 40 tokens so it can be read in a glance.
- **Instrumentation:** timestamp every stage and show p50/p95 in a debug panel from day one.

## Memory and context

Memory has four layers, all stored locally on the PC in SQLite with a vector index and synced to the phone only when the user turns that on.

| Layer | What it holds | Written when | Used when |
| --- | --- | --- | --- |
| About Me | Name, role, company, product, tone, answers the user likes to give | Onboarding; edited any time | Every prompt |
| Profiles | Per-scenario instructions and docs: "Sales: product X", "Support: bank app", "Investor call" | User creates; one is active per session | Every prompt in that session |
| Contacts and accounts | Who was on the call, past objections, commitments, open items | Extracted at session end; user confirms | When a known name or company appears on screen or in audio |
| Session history | Summary, action items, key Q&A, follow-up draft | Session end | Search and "ask about past calls" |

**Rules:**

- Raw audio and screenshots are never stored; only text summaries and transcripts the user chose to keep.
- Every memory item is visible and can be edited or deleted from the phone.
- Contact memory is suggested, not saved silently: the user confirms it with one tap after each session.

## Privacy, security and boundaries

Privacy is the main selling point, so it's built in from the start.

- **Pairing:** the phone scans a QR code that holds a one-time key. The two devices derive a shared key (X25519), and every frame is encrypted (XChaCha20-Poly1305), including frames sent through the relay.
- **Relay:** a blind relay that forwards encrypted bytes only. It stores no content, and the user can host it themselves.
- **Keys:** API keys stay in the OS keychain on the PC and are never sent to the phone or the relay.
- **Local mode:** with a local model and local speech-to-text, nothing leaves the PC except encrypted frames to the phone.
- **Redaction:** optional masking of card numbers, BVN/NIN-style IDs and passwords before anything goes to a cloud model.
- **Visible indicator:** a tray icon which can be visible or invisible (background running) on hotkey.
- **Model access:** cloud models use the user's own API keys. Consumer chat subscriptions (Claude.ai, ChatGPT Plus) are not driven through scraping or unofficial routes.

## Tech stack

The stack uses Rust for the speed-critical PC agent, TypeScript everywhere else, and starts the mobile side as a PWA so it ships fast.

| Layer | Choice | Why | Alternative |
| --- | --- | --- | --- |
| PC shell | Tauri 2 | Small binary, Rust core, web UI for settings | Electron (heavier) |
| Screen text | `uiautomation` crate (Windows), AXUIElement (macOS) | Structured text in tens of ms | OCR only (slower) |
| OCR fallback | Windows.Media.Ocr / Apple Vision on changed regions | Built into the OS, free, fast | Tesseract |
| Audio capture | WASAPI loopback (Windows), ScreenCaptureKit (macOS) | System audio and mic as separate streams | BlackHole virtual device |
| Speech-to-text | Deepgram or AssemblyAI streaming; whisper.cpp for local | Partial results in about 300 ms | faster-whisper |
| Models | Groq (answers) and Jev (decisions) APIs, Gemini as fallback; Ollama for Tev1 4B and Gemma 4 | Bring your own key plus a local option | OpenRouter as one gateway |
| Retrieval | SQLite + sqlite-vec, small local embedding model | One file, no server | LanceDB |
| Transport | WebSocket (LAN, mDNS discovery) + blind relay on Cloudflare Workers / Durable Objects | Low latency on the local network, works anywhere | WebRTC data channel |
| Mobile | PWA (Next.js) → React Native (Expo) in v2 | Ship in days; native later for haptics, background use and a watch | Flutter |
| Billing (later) | Paystack + Stripe | Covers both Nigerian and global cards | Lemon Squeezy |

## MVP plan

The MVP takes four weeks, on Windows first. It's done when a live sales call gets useful answers on the phone at under 1.5 s p50.

**Week 1: capture**

- [ ] Tauri tray app with a capture on/off hotkey
- [ ] Read focused-window text through UI Automation, with diffing
- [ ] Separate system-audio and mic streams into streaming speech-to-text
- [ ] Stage timestamps logged from day one

**Week 2: brain and pipe**

- [ ] Trigger engine: hotkey and question detection
- [ ] Model router: Groq + Jev in cloud mode, Gemma 4 E2B + Tev1 4B on Ollama offline
- [ ] QR pairing, encrypted WebSocket on the local network, token streaming to a bare PWA

**Week 3: phone UX and context**

- [ ] Answer card, vibration on new answer, pin / regenerate / shorter
- [ ] About Me and profiles; upload docs to the local index
- [ ] Session summary and a draft follow-up email

**Week 4: memory and polish**

- [ ] Contact and account memory, confirmed by the user
- [ ] Blind relay for use off the local network
- [ ] Latency dashboard (p50/p95 per stage); test with 5 sales or support users

**Success metrics:**

| Metric | MVP target |
| --- | --- |
| Question end → first word on phone (p50 / p95) | <1.5 s / <2.5 s |
| Answers rated useful by testers | ≥60% |
| PC agent CPU while idle / while capturing | <2% / <10% |
| Testers using it on a second real call | 3 of 5 |

**Open questions:**

- [ ] First niche: B2B sales reps, or support agents at banks and fintechs?
- [ ] Pricing: one-time fee for local-only use vs a monthly plan with the relay included?
- [ ] macOS in v1, or Windows only until it fits the market?
- [ ] Should the watch app come before React Native?
