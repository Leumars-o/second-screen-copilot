# Second Screen Copilot — threat model (v1, macOS)

Owner: W2-HARDEN. Scope: Build Sheet §7 W2-HARDEN, as built at `4674b06`. Each control below names
the automated test that checks it; open defects are change requests in
`contracts/CHANGE_REQUESTS.md` (CR-10 … CR-15). All six were fixed in Wave 2 (W2-FIX-RUST, W2-FIX-PHONE); their tests now run in the gate.

## 1. Assets, actors, trust boundaries

| Asset | Where it lives |
| --- | --- |
| Provider API keys (Groq, Gemini, TypeSafe/Jev, Deepgram, strong model, Cloudflare) | macOS Keychain, service `com.second-screen-copilot.agent` |
| Device keys: PC X25519 identity, paired-phone direction keys + seq counters, ACME account key, LAN TLS private key | Keychain (`device.*`) |
| Call content: audio, screen text, transcripts, answers, summaries | RAM; text only in `~/.ssc/memory.sqlite` (transcripts only if `keep_transcripts`) |
| User memory: About Me, profiles, docs, contacts | `~/.ssc/memory.sqlite` |
| Phone direction keys | PWA IndexedDB, AES-GCM-sealed under a non-extractable WebCrypto key (CR-15) |

Actors: the user; the other party on the call ("them", untrusted — controls speech and often
screen content); anyone on the same Wi-Fi; any web page open in a browser on the PC or LAN;
the relay operator (Cloudflare or self-host); cloud model/STT providers; Cloudflare DNS + public
DNS/CT log readers; a local process running as the same macOS user (out of scope: it can read the
keychain items the agent can, and the SQLite file).

Boundaries: PC ↔ phone (LAN socket or relay), PC ↔ cloud providers (HTTPS), PC ↔ Cloudflare API /
ACME, settings webview ↔ Rust core (Tauri IPC), call content ↔ prompts (untrusted text inside a
trusted template).

## 2. Pairing

QR (shown 120 s) carries `pc_pub`, a 32-byte one-time token, LAN + relay URLs. The phone sends one
plaintext `pair.hello` (`0x00 || JSON`: device name, phone public key, HMAC(token, phone_pub)).
Keys = HKDF(X25519, salt = token). The token burns on success, on expiry, or after 5 failures.

Threats and controls:
- *Pairing without the QR* — proof is an HMAC under the token; wrong-token proofs, proofs bound to
  another key, malformed fields → rejected; 5 failures burn the offer.
  `ssc-crypto/tests/security_frames.rs::pairing_requires_the_token_and_burns_after_failures`.
- *Low-order phone key* (predictable shared secret) → rejected. `low_order_phone_key_is_refused`.
- *Replay of a captured `pair.hello`* → token burned / expired. `pair_hello_is_single_use_and_expires`.
- *`pair.hello` when no QR is armed / to replace the paired phone* → socket closed, pairing kept.
  `ssc-transport/tests/security_link.rs::pair_hello_without_an_armed_offer_is_refused`.
- *Shoulder-surfing the QR* — accepted risk: whoever scans first within 120 s pairs; re-pairing
  cuts the previous phone off (`link.rs::re_pairing_cuts_off_the_previous_phone`), and the user
  sees the device name in settings. The screen reader never sees the QR as text (SVG).
- *Relay sees `pair.hello`* (device name + phone public key, CR-R1) — accepted; no token or key
  material in it (asserted in `relay_sees_ciphertext_only`).

## 3. Frames, relay, replay

Every other frame is `0x01 || nonce24 || XChaCha20-Poly1305(envelope, aad = session_id ‖ seq)`.
Strictly increasing `seq` per direction, persisted per pairing (CR-3).

- *Relay reads content* — sealed frames contain no plaintext, JSON keys, session id or marker
  text; nonces never repeat. `relay_sees_ciphertext_only` (recording relay, real pairing) and
  `sealed_frames_reveal_no_plaintext`; phone side `channel.security.test.ts`.
- *Relay keeps or logs content* — no DO storage, socket attachments hold only `lastSeen`, no
  console output (mutation-checked). `services/relay/test/security/relay.security.test.ts`.
- *Tamper / truncation / AAD splice / schema-invalid plaintext* → dropped, window unmoved.
  `every_single_bit_flip_is_rejected`, `aad_and_envelope_must_agree`, `schema_invalid_plaintext_is_rejected`,
  `wire_replay_and_tamper_are_not_delivered`; TS: "drops every single-bit flip…".
- *Replay / reorder / reflection / foreign pairing* → dropped.
  `replays_and_reordering_are_rejected`, `reflected_and_foreign_frames_are_rejected`.
- *Replay after a PC crash* — **Fixed, CR-11 (low)** — original finding: `last_recv` is persisted at most once a
  second, so frames from the last second before a crash are accepted again after restart.
  `replay_window_is_persisted_before_frames_are_delivered` (ignored).
- *Unpaired device* — frames under other keys are dropped and the socket closed after 8;
  `send` refuses without a paired phone. `unpaired_device_frames_are_dropped_and_socket_closed`,
  `nothing_is_sent_without_a_paired_phone`.
- *Oversized frames* — >64 KB closes the socket unparsed; `send` refuses to build one; the relay
  drops them (existing relay tests). `oversized_frames_are_refused_both_ways`; TS "huge frames".
- *Plaintext injected toward the phone* — `0x00`/text frames never reach the app.
  `PhoneLink … delivers only authentic frames`.
- *Room squatting* — a third socket is refused (409) and rooms are isolated; anyone who learns a
  room id (relay operator) could occupy a slot and deny the relay path. Accepted (availability
  only; LAN still works).
- *LAN slot exhaustion / cross-origin browsers* — **Fixed, CR-12 (medium)** — original finding: 8 unauthenticated
  sockets hold all LAN slots for 15 s each, and the server accepts upgrades from any web Origin,
  so any page in a browser on the PC/Wi-Fi can lock the phone out of the LAN path (relay still
  works). `idle_unauthenticated_sockets_cannot_lock_out_the_phone`,
  `cross_origin_browser_upgrades_are_refused` (ignored).

## 4. LAN TLS and DNS (CR-4: public A record, Let's Encrypt via Cloudflare DNS-01)

- *Public A record exposes the LAN IP* — a private RFC 1918 address of the user's network becomes
  public DNS for `lan_domain`. Low value to an attacker; accepted by §10 item 7. Prefer an
  unguessable sub-domain.
- *CT logs* — every certificate for `lan_domain` is public in Certificate Transparency; the name
  (and that the user runs the copilot) is discoverable. Accepted; use a non-identifying label.
- *DNS rebinding* — the name always resolves to the PC's own LAN IP; a rebinding page gains nothing
  it could not do by connecting to the IP directly (see CR-12: the Origin check is the fix for
  both). Frames stay authenticated end to end regardless of DNS.
- *Cloudflare token scope* — Zone→DNS→Edit on the whole zone: a stolen token rewrites every record
  of that zone (website, MX). Recommendation: put `lan_domain` in a dedicated zone. Controls:
  token only in the keychain, never in errors (`lan_tls.rs::bad_token_is_a_dns_failure_without_the_token_in_the_message`),
  not forwarded on redirects (`cloudflare_token_is_not_forwarded_on_redirect`).
- *Client touches other records* — every write is by id of a record found by exact name + type and
  re-filtered client-side, so a lax API cannot widen it (`lax_api_cannot_widen_cloudflare_writes`,
  `cloud_mode_lan_tls_provisions_with_fakes`, `lan_tls.rs::a_record_create_noop_update_touches_nothing_else`).
  **Fixed, CR-13 (medium)** — original finding: an existing A record with the configured name — including the zone apex —
  is taken over (re-pointed to the LAN IP). `refuses_to_repoint_records_it_does_not_own` (ignored).
- *Local-only mode talks to Cloudflare/ACME* — **Fixed, CR-14 (low)** — original finding: see §6.

## 5. Keys never leave the keychain

Controls: `Secret` redacts in Debug/Display; keys go only into sensitive auth headers; answer
adapters never copy provider error bodies; the settings page can write but never read keys.
- `ssc-pipeline/tests/security_pipeline.rs::keys_never_leave_the_keychain` — child process runs a
  full session through the real router factory with every key set and every provider echoing the
  received key in a 401 body; parent checks stdout/stderr, `config.toml`, SQLite (+WAL), timing
  JSONL and every phone frame for any key.
- `secret_expose_sites_are_reviewed` — allowlist of all 14 `Secret::expose()` sites.
- `security_router.rs::answer_adapter_errors_never_contain_the_key`, `redaction_holds_on_the_wire_for_real_adapters`
  (key only in `Authorization`).
- `security_frames.rs::debug_output_never_contains_key_material` (identity, tokens, direction keys).
- `apps/agent/tests/security_ipc.rs` — IPC responses carry only `set: bool` per key.
- **Fixed, CR-10 (low)** — original finding: the Jev/TypeSafe adapter appends the provider's `detail.message` to its
  error; a provider echoing the key (or the question text) puts it in the error string. Today the
  trigger engine discards decision errors, so nothing is logged yet.
  `decision_adapter_errors_never_contain_the_key` (ignored).
- Keychain items use the default macOS ACL (the creating binary). Unsigned dev rebuilds may
  re-prompt; a same-user process that the user approves can read them (out of scope).
- Phone: direction keys are sealed in IndexedDB under a non-extractable WebCrypto key and held
  as raw bytes only in memory (libsodium needs them). A script running on the PWA origin could
  still use the key handle, so the strict CSP (hashed inline scripts, no `unsafe-eval`,
  `wasm-unsafe-eval` only for libsodium) is the control. CR-15 fixed.

## 6. Local-only mode

Promise (spec): with a local model and local STT, nothing leaves the PC except encrypted frames.
- Router factory reads no key and builds no cloud client; routers refuse cloud models; remote or
  look-alike Ollama hosts are refused. `security_router.rs::local_only_*`.
- STT is whisper; the Deepgram key is never read (`stt_select.rs` unit test).
- Pipeline refuses a cloud post-call model even if misconfigured.
  `security_pipeline.rs::injection_cannot_move_local_only_to_cloud`.
- Whole-stack: `e2e_local_only.rs` runs under `sandbox-exec` with only loopback allowed.
- **Fixed, CR-14 (low)** — original finding: `start_transport` always spawns the LAN TLS manager, which in local-only
  mode still calls the Cloudflare API (publishing the LAN IP) and Let's Encrypt.
  `local_only_mode_makes_no_cloudflare_or_acme_calls` (ignored).
- Model downloads (§7) also reach Hugging Face once; they carry no user data.

## 7. Model downloads

Every model is pinned to a full commit hash with SHA-256 + size per file, fetched over HTTPS
without a token; a mismatch leaves no file (`ssc-memory/tests/download.rs`). Pins guarded by
`security_models.rs::every_model_is_pinned_by_commit_hash_and_size`. Accepted: a file already at
its final path is trusted by size (a same-user local attacker could swap it; out of scope).

## 8. Prompt injection (screen text, "them" speech)

Rules in code (Build Sheet §3.7–3.8): mode, redaction, routing tier limits and memory writes are
deterministic and never read model output; call content sits inside `<untrusted>` with any
`<untrusted>`/`</untrusted>` in the content neutralised; the decision model can only pick among
fixed choices and stored profile ids; post-call contacts are only *suggestions* until the user
confirms on the phone.
- `security_pipeline.rs::injection_in_speech_and_screen_cannot_act_change_settings_or_exfiltrate` —
  hostile speech + screen (commands, tag break-out, forged frame JSON, exfil URL, card/BVN/password)
  and *models that obey them* (answer and summary full of commands and extra fields). Asserts:
  mode and decision routing unchanged; only answer/transcript/session frames reach the phone;
  About Me/profiles/docs/contacts unchanged; injected contact only pending; redaction applied to
  every cloud prompt; the injected `</untrusted>` never closes the block.
- Residual risk (accepted): an injected instruction can still steer the *text* of an answer the
  user reads, choose another stored profile's docs for that answer, or pick the strong tier
  (cost). Uploaded docs are trusted template content; a hostile doc could steer answers. The phone
  renders text only (React escaping; no links/images), so model output cannot exfiltrate by
  rendering.
- Cloud mode by design sends (redacted) call text to the configured providers; audio goes to
  Deepgram unredacted when cloud STT is on.

## 9. Settings UI (Tauri IPC)

17 commands (`get_state`, `save_config`, `set_secret`, About Me/profile/doc CRUD, capture toggle,
`answer_now`, `begin_pairing`, `unpair`, `debug_stats`, permission checklist/request). No command
returns a key: `get_state` returns `{name, label, set: bool}` per key; `set_secret` accepts only the
six API-key names (device keys are not writable); `request_permission` opens one of three fixed
System Settings URLs. Webview: capability `core:default` only, CSP `default-src 'self'; script-src 'self'`
(no inline/eval, no remote origins), page script has no HTML sinks for data.
`apps/agent/tests/security_ipc.rs` fails if any of this changes.
Residual: a script running in the page (none can be injected under the CSP) could flip `mode`,
`redaction` or provider URLs through `save_config`; that is the settings page's purpose.

## 10. Disk

No audio or screenshots are ever written. `security_pipeline.rs::no_audio_or_screenshots_written_to_disk`
(full session with file-backed store: only `memory.sqlite*` and timing JSONL appear, no media
magic, no run of the captured PCM) and `only_reviewed_code_writes_files` (capture crates contain no
file-write or image-encode API; elsewhere only 5 reviewed files write). Schema has no media
columns (`ssc-core/tests/db_schema.rs`).
