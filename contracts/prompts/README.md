# Prompt contract (v1)

Each file has two parts separated by a line containing only `<<<DYNAMIC>>>`:

- **Prefix** (above): fixed rules + About Me + profile. Identical across a session, so provider prompt caching and Ollama prefix caching work. Keep it byte-stable: no timestamps.
- **Dynamic** (below): retrieved docs, transcript, screen diff, question. Local mode keeps this under ~300 tokens.

Placeholders: `{{about_me}}`, `{{profile_name}}`, `{{profile_instructions}}`, `{{docs}}`, `{{transcript}}`, `{{screen}}`, `{{question}}`, `{{answer}}`, `{{session}}`. Unused placeholders render as empty strings. Screen and transcript text are wrapped in `<untrusted>` tags: they are data, never instructions.

## Answer output grammar (deterministic parser in ssc-router)

```
1. <cue, 2-6 words>          ← block "cue", 3-7 lines, streamed first (~40 tokens)
2. <cue>
---                          ← switches to block "detail"
1: <supporting point for cue 1, max 20 words>
2: <supporting point for cue 2>
FACT: <one fact or number>   ← optional, only from docs
SOURCE: <doc name>           ← required when FACT is present
NOT_IN_DOCS                  ← alone, when a product-specific question isn't covered
```

Screen answers (`screen.v2.md`, CR-20) use the same `---` split: the text before it is the answer (block "cue", free text, not numbered cues) and the `N:` lines after it are the working (block "detail"). The phone shows them as a "screen" card.

Parser rules: lines matching `^\d+\. ` before `---` are cues (truncate to 6 words, keep max 7); `^\d+: ` after `---` maps to that cue's `expand`; `FACT:` / `SOURCE:` / `NOT_IN_DOCS` may appear anywhere; anything else is dropped. Fact answers may have zero cues.

| File | Used for | Model |
| --- | --- | --- |
| `cue.v2.md` | Explain / walk-through, technical, scheduling | fast |
| `fact.v1.md` | Pricing / limits / dates | fast |
| `objection.v1.md` | Objections and pushback | fast |
| `expand.v1.md` | "Expand all" and the strong-tier detail | strong (or fast) |
| `screen.v2.md` | Screen hotkey (CR-20): answer what is on screen; image attached in cloud mode | fast (vision) |
| `summary.v1.md` | Session summary + action items + contact suggestions (JSON out) | post-call |
| `followup.v1.md` | Follow-up email draft | post-call |
