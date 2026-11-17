# Change requests

Workers add entries here instead of editing `contracts/` or `crates/ssc-core/`. The lead reviews them between waves.

Format:

```
## CR-<n> — <title>
- From: <brief id>
- What: ...
- Why: ...
- Proposed diff: ...
- Status: open | accepted (contracts-v1.x) | rejected (reason) | deferred
```

## CR-1 — Router settings missing from `Config`
- From: W1-MODELS
- What: add to `ssc_core::config::Models`: `gemini_tier: "paid" | "free"` (default `paid`), `strong_base_url: Option<String>`; add `testing: bool` (default false) and `redact_emails: bool` (default false) to `Config`.
- Why: the Gemini free-tier gate ("free tier only when `testing`"), optional email redaction and non-standard strong-model providers need user-visible settings. Today they live in `ssc_router::RouterOptions` (defaults: paid, testing off, emails off), so the settings UI can't persist them.
- Proposed diff: the four fields above with `#[serde(default)]`; `ssc_router::factory::build` then reads them from `Config` instead of `RouterOptions`.
- Status: open (workaround in place: `RouterOptions`)
