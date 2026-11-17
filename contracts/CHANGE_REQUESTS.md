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

## CR-1 — Receivers cannot know the frame AAD before decrypting
- From: W1-PHONE, W1-PIPE (filed independently, merged by lead)
- What: `aad = session_id || seq` binds two values that are only inside the ciphertext. Nothing in the wire header (`0x01 || nonce || ct`) gives them. The receiver can guess `seq = last + 1`, but it cannot guess a *new* `session_id`: the `session.start` fixture's envelope already carries the new id.
- Why: Without a defined way to recover the AAD, the PC and phone may each pick a different workaround and stop interoperating.
- Workaround in TS (`packages/protocol-ts/src/crypto.ts` `openFrame`, marked `CR-1`): compute the unauthenticated candidate plaintext (`ct_body XOR XChaCha20 keystream from block 1`), read `session_id` and `seq` from it, then run the full AEAD decrypt with that AAD. Nothing is trusted unless the tag verifies. This works with the frozen vectors and with any sender that follows the README. In Rust, `chacha20::XChaCha20` seeked to byte 64 does the same.
- Proposed diff (pick one): (a) add the paragraph above to `contracts/crypto/README.md` as the normative receive algorithm (no wire change, vectors stay valid); or (b) v2 frame `0x02 || seq u64 BE || sid_len u8 || sid || nonce || ct`, which needs new vectors. Recommendation: (a).
- Decision / Rust impl (W1-PIPE) (lead, option a, no wire change, vectors unchanged): check that the first byte is `0x01`. Decrypt the body with the XChaCha20 keystream from block 1 (`chacha20::XChaCha20` seeked to byte 64; libsodium `crypto_stream_xchacha20_xor_ic(ic = 1)`) without checking the tag. Read only `session_id` and `seq` from that candidate plaintext, then run the full AEAD open with that AAD. Trust and parse nothing unless the tag verifies; the authenticated `seq`/`session_id` must equal the AAD values. Drop the frame on any failure. Implemented in `ssc-crypto::frame::open` (`// CR-1`).
- Status: accepted (contracts-v1.1), option (a): normative receive algorithm in contracts/crypto/README.md

## CR-2 — Transport framing of the plaintext pair.hello and pairing confirmation
- From: W1-PHONE, W1-PIPE (filed independently, merged by lead)
- What: The README does not say how the single plaintext frame is carried, or how the phone learns that pairing succeeded.
- Phone behaviour (CR-2 comments in `client.ts`): `pair.hello` is sent as a WebSocket **text** message (UTF-8 envelope JSON). Every encrypted frame is a **binary** message. The phone treats the first authentic encrypted frame from the PC (the mock sends a `heartbeat`) as pairing confirmation. The PC closes the socket on a failed proof.
- Proposed diff: add those three sentences to `contracts/crypto/README.md` § Pairing.
- Decision / Rust impl (W1-PIPE) (lead): `pair.hello` is a binary frame `0x00 || envelope JSON (UTF-8)` on the LAN and the relay alike. Every other frame is binary `0x01 || nonce || ct`, and text frames are ignored. Pairing is confirmed by the first authentic encrypted frame from the PC (a `heartbeat`). On a bad proof or an expired token, the PC closes the socket. On the relay, the socket belongs to the PC, so it stays open and the frame is dropped. Also implemented: a `heartbeat` without `echo` is answered at once with `echo = its ts` (RTT = now − echo), and the PC sends on the socket that last delivered a valid phone frame, falling back to the relay socket if there is none.
- Status: accepted (contracts-v1.1) as amended: binary 0x00 framing (`0x00 || envelope JSON`), per CR-R1

## CR-3 — Scope of `seq` across reconnects
- From: W1-PHONE, W1-PIPE (filed independently, merged by lead)
- What: The contract does not say whether `seq` (and the receiver's last-accepted value) resets on a new WebSocket connection or a LAN↔relay switch.
- Phone behaviour: seq state is per pairing. It is persisted in localStorage and keeps increasing across reconnects and reloads, so the relay cannot replay frames from an earlier connection. If the PC resets its counter on reconnect, the phone drops every frame as a replay.
- Proposed diff: README § Frames: "`seq` is scoped to the pairing (the key pair), not to a connection. Both sides persist next-send and last-accepted values across reconnects and restarts."
- Decision / Rust impl (W1-PIPE) (lead): `seq` is per pairing, not per connection. Persisted per pairing: `Counters { next_send, last_recv }`. `next_send` is a reservation: on start the sender reserves `[next_send, next_send + 1000)` and persists the new upper bound before sending; crossing a block reserves the next block and persists it again. `last_recv` is the replay window (the PC persists it at most once a second, and on clean shutdown through `device_snapshot`). Implemented in `ssc-crypto::frame::SecureChannel` (`SEQ_RESERVE_BLOCK = 1000`, `take_persist`). `PcTransport::take_device_updates` emits the records to write to the keychain (`// CR-3`).
- Status: accepted (contracts-v1.1)

## CR-4 — LAN `ws://` from an HTTPS-hosted PWA is blocked as mixed content
- From: W1-PHONE
- What: A PWA served over HTTPS (needed for the service worker, the camera and WebCrypto) cannot open `ws://<lan-ip>` URLs. Browsers block it as mixed content, and Chrome's Private Network Access rules add more limits. If the PWA is served over plain http from the PC instead, it loses the camera, WebCrypto and the service worker. The QR's first URL is therefore unusable in the hosted case, and the phone falls back to the relay.
- Why: "LAN first" will silently become "relay always" on real phones.
- Options: (a) the PC serves the PWA over HTTPS on the LAN with a cert the phone trusts (hard on phones); (b) accept relay-only for the hosted PWA and keep LAN for a wrapped/native shell later; (c) add `wss://` LAN with a public DNS name pointing at the LAN IP plus a real cert (the plex.direct pattern). The lead should decide; nothing in the phone code changes for (b) or (c).
- Status: accepted (user, 2026-11-17): option (c), LAN `wss://` on the user's Cloudflare sub-domain (DNS-only A record → LAN IP, Let's Encrypt via DNS-01). Implemented in Wave 2; see Build Sheet §10 item 7.

## CR-5 — Router settings missing from `Config`
- From: W1-MODELS
- What: add to `ssc_core::config::Models`: `gemini_tier: "paid" | "free"` (default `paid`), `strong_base_url: Option<String>`; add `testing: bool` (default false) and `redact_emails: bool` (default false) to `Config`.
- Why: the Gemini free-tier gate ("free tier only when `testing`"), optional email redaction and non-standard strong-model providers need user-visible settings. Today they live in `ssc_router::RouterOptions` (defaults: paid, testing off, emails off), so the settings UI can't persist them.
- Proposed diff: the four fields above with `#[serde(default)]`; `ssc_router::factory::build` then reads them from `Config` instead of `RouterOptions`.
- Status: accepted (contracts-v1.2). `GeminiTier` now lives in `ssc_core::config` (re-exported by `ssc-router`); `RouterOptions` keeps only internal timings and base URLs.

## CR-R1 — Wire framing of the unencrypted `pair.hello` (relay drops it today)
- From: W1-RELAY
- What: `contracts/crypto/README.md` says `pair.hello` is sent unencrypted, but not how it is framed on the WebSocket. The relay (per its brief) forwards only binary frames whose first byte is `0x01` and drops text frames, so a `pair.hello` sent through the relay as a text/JSON frame is silently dropped. Pairing therefore only works on the LAN unless the framing is pinned down.
- Why: the QR payload lists a `wss://relay…/r/<room_id>` URL, so a phone that is off the PC's network at pairing time would try to pair through the relay and hang.
- Proposed diff (pick one):
  1. `contracts/crypto/README.md`: "`pair.hello` is sent as a binary frame `0x00 || envelope JSON (UTF-8)`; every other frame uses version `0x01`." The relay then also forwards version `0x00` frames (same 64 KB / rate limits). Small relay change, owned by W1-RELAY.
  2. Or state explicitly that pairing is LAN-only and the relay URL in the QR is for later sessions only.
- Status: accepted (contracts-v1.1)

## CR-6 — Config and keychain entries for the CR-4 domain transport
- From: lead
- What: `Config.lan_domain: Option<String>` (the DNS-only host, e.g. `relay.live-preview.space`); `SecretKey::Cloudflare` (`api.cloudflare`, Zone→DNS→Edit token), `SecretKey::AcmeAccount` (`device.acme_account`), `SecretKey::LanTlsKey` (`device.lan_tls_key`).
- Why: W2-INTEGRATE needs somewhere to read the domain and keep the ACME account key and certificate private key out of plain files.
- Status: accepted (contracts-v1.2)

## CR-7 — Correct Jev defaults
- From: lead (live call, 2026-11-17)
- What: `models.decision_cloud_base_url` default `https://api.typesafe.ai`; `models.decision_cloud_model` default `jev-1.14.0` (the API rejects `jev-1.14` with `Unknown model`; `jev-latest` resolves to `jev-1.14.0`). Live response matched the documented shape: `{model, answers: {id: {type:"noul", noul} | {type:"choice", choice, confidence, probabilities}}, usage}`; 0.56 s round trip.
- Status: accepted (contracts-v1.3)

## CR-8 — Default strong model `anthropic/claude-opus-5-5`
- From: W2-ROUTER
- What: `models.strong_model` default `Some("anthropic/claude-opus-5-5")` instead of `None`; update the doc comment ("Sonnet-class model … None = disabled") to "strong tier model, provider/model; None = disabled".
- Why: Build Sheet §10 item 8 and the user rule "best available model everywhere" name Claude Opus 5.5 (native Messages adapter, adaptive thinking, `effort: low`) as the strong model. With `None` the strong tier and the cloud post-call model are off until the user types a model id. The default is safe without a key: `factory::build_strong` returns no strong tier when `api.strong` is missing, and local-only mode never reads it.
- Proposed diff: `strong_model: Some("anthropic/claude-opus-5-5".into()),` in `impl Default for Models`; `ssc_router::providers::anthropic::DEFAULT_CLAUDE_MODEL` already holds the id. Adjust any ssc-core test that asserts `strong_model.is_none()`.
- Status: proposed

## CR-9 — Optional Cloudflare zone id for the CR-4 LAN domain
- From: W2-LANTLS
- What: add `Config.lan_zone_id: Option<String>` (`#[serde(default)]`), the Cloudflare zone id of `lan_domain`'s zone.
- Why: the A-record/ACME code finds the zone with `GET /zones?name=…`, which needs Zone→Zone→Read. A token scoped to Zone→DNS→Edit only cannot list zones, so the user must be able to give the zone id instead.
- Proposed diff: the field above; `ssc_transport::lan_tls::LanTlsOptions::zone_id` is then filled from it. Until then the stub (`// CR-9`) reads `$SSC_CLOUDFLARE_ZONE_ID`.
- Status: proposed

## CR-10 — Jev/TypeSafe adapter copies the provider's error message into its error
- From: W2-HARDEN. Severity: **low**. Owner: W1-MODELS (`crates/ssc-router`).
- What: `crates/ssc-router/src/decision/mod.rs:75` wraps every non-2xx in `with_detail` (`:90`), which appends the body's `detail.message` / `error.message` (160 chars) to the `SscError`. A provider that echoes the key ("Incorrect API key provided: sk-…") or the request text puts it in the error string; with redaction off, the question is unredacted.
- Why it matters: error strings are logged and shown in the settings UI. Today `ssc-trigger` discards decision errors, so nothing leaks yet; the next `log(e)` would.
- Failing test: `crates/ssc-router/tests/security_router.rs::decision_adapter_errors_never_contain_the_key` (`#[ignore = "CR-10"]`).
- Proposed fix: keep only the sanitised `error_type`/`code` (as `http::map_status` already does), or scrub the message: drop it unless it matches a short allowlist (`Unknown model: <id>`), and never include it for 401/403.
- Status: fixed (W2-FIX-RUST, 8ace8bc) — message dropped for 401/403; otherwise the key (whole, or any word sharing its first/last 8 chars) is replaced by `[redacted]`, the rest kept.

## CR-11 — Replay window persisted at most once a second: replays accepted after a crash
- From: W2-HARDEN. Severity: **low**. Owner: W1-PIPE (`crates/ssc-transport`).
- What: `crates/ssc-transport/src/core.rs:412` (`maybe_persist`) hands the device record (with `last_recv`) to the keychain writer only when a seq block is reserved or ≥1 s after the last write (`:419`), while frames are delivered at once (`:325`). After a crash/kill, the PC restarts with a stale window and accepts again any phone frame recorded in that last second (e.g. a relay operator replaying `memory.delete` / `contact.confirm`).
- Failing test: `crates/ssc-transport/tests/security_link.rs::replay_window_is_persisted_before_frames_are_delivered` (`#[ignore = "CR-11"]`).
- Proposed fix: before delivering any non-heartbeat frame, persist `last_recv` (commands are rare, so one keychain write per command is cheap), or reserve the receive window like the send side: persist `last_recv + W` and, after a restart, drop frames below the persisted mark until the peer's seq passes it.
- Status: fixed (W2-FIX-RUST, 8ace8bc) — the advanced `last_recv` is handed to the keychain writer before each non-heartbeat frame is delivered (no receive-side reservation: that would drop legitimate phone frames after a restart). Residual: the keychain write itself is async, so a crash within that write (ms) can still lose it. Loopback command round trip p50 ≈ 0.70 ms before and after.

## CR-12 — LAN socket slots: no auth deadline and no Origin check
- From: W2-HARDEN. Severity: **medium** (availability only; the relay path still works). Owner: W1-PIPE (`crates/ssc-transport`).
- What: `crates/ssc-transport/src/ws.rs:19,82` allow 8 sockets; an unauthenticated socket is only closed after `dead_after` (15 s, `core.rs:340`) or 8 bad frames. `ws.rs:105,114` accept the upgrade from any `Origin`. WebSockets are not subject to CORS, so any web page open in a browser on the PC or the Wi-Fi (no LAN foothold needed) can keep all 8 slots busy and lock the paired phone out of the LAN path.
- Failing tests: `crates/ssc-transport/tests/security_link.rs::idle_unauthenticated_sockets_cannot_lock_out_the_phone`, `::cross_origin_browser_upgrades_are_refused` (both `#[ignore = "CR-12"]`).
- Proposed fix: (1) in the upgrade callback, refuse any `Origin` other than the PWA's origin (configurable; a native client sends none); (2) give unauthenticated sockets a short deadline (≈3 s to the first valid frame or `pair.hello`) and, when slots are full, evict the oldest unauthenticated socket instead of refusing the newcomer.
- Status: fixed (W2-FIX-RUST, 8ace8bc) — `TransportConfig::auth_deadline` (3 s) closes unauthenticated LAN sockets, LAN handshake uses the same budget; when the 8 slots are full a newcomer evicts the oldest unauthenticated socket (never an authenticated one); a browser `Origin` must match `TransportConfig::allowed_origins` (no `Origin` = native client, allowed). The PWA origin is not in `Config`, so `from_core` derives it: `https://<parent of lan_domain>` and its subdomains, `https://<relay host>` exactly, plus `$SSC_PWA_ORIGINS`. A PWA hosted elsewhere needs that env var (or a future Config field, CR).

## CR-13 — Cloudflare client takes over A records it did not create (incl. the zone apex)
- From: W2-HARDEN. Severity: **medium**. Owner: W2-LANTLS / W1-PIPE (`crates/ssc-transport/src/lan_tls`).
- What: `crates/ssc-transport/src/lan_tls/cloudflare.rs:149` (`upsert_a`) re-points any single existing `A` record with the configured name (`:160`). `normalize_domain` (`lan_tls/mod.rs:154`) accepts the bare zone. A typo in `lan_domain`, or setting it to `example.com` / `www.example.com`, silently points the user's website at a private LAN IP.
- Failing test: `crates/ssc-transport/tests/security_link.rs::refuses_to_repoint_records_it_does_not_own` (`#[ignore = "CR-13"]`; covers the apex and a foreign record).
- Proposed fix: mark records the agent creates (Cloudflare record `comment`, e.g. `managed-by: second-screen-copilot <pc-id>`) and update only records carrying that marker; refuse (touch nothing, clear error) when an unmarked record exists. Refuse a `lan_domain` equal to the zone name. The existing `proxied_record_is_switched_to_dns_only` test then needs a marked record.
- Status: fixed (W2-FIX-RUST, 8ace8bc) — records we create carry `comment: managed-by: second-screen-copilot`; only marked A records are updated; an unmarked record is adopted only if DNS-only and already pointing at the PC's LAN IP (marker added on the same update, existing note kept after it); otherwise refused untouched with "record exists and isn't managed by Second Screen Copilot". `lan_domain` equal to the zone (name from the zone lookup; two-label names when the token lacks Zone:Read) is refused.

## CR-14 — Local-only mode still calls the Cloudflare API and Let's Encrypt
- From: W2-HARDEN. Severity: **low** (metadata, not call content). Owner: W2-INTEGRATE (`crates/ssc-pipeline/src/transport.rs`) or W1-PIPE.
- What: `crates/ssc-pipeline/src/transport.rs:95` always spawns `LanTlsManager`, which (`lan_tls/mod.rs:200` onward) reads the Cloudflare token, publishes the LAN IP in the A record and orders certificates even when `mode = local`. The spec promises that in local mode "nothing leaves the PC except encrypted frames to the phone".
- Failing test: `crates/ssc-transport/tests/security_link.rs::local_only_mode_makes_no_cloudflare_or_acme_calls` (`#[ignore = "CR-14"]`).
- Proposed fix: in local-only mode, serve a still-valid cached certificate but make no Cloudflare/ACME call (manager checks `cfg.mode`); offer an explicit settings toggle "allow DNS/certificate upkeep in local-only mode" and show the consequence (plain `ws://` LAN is blocked by the HTTPS PWA, so the phone would use the relay) in the settings UI.
- Status: fixed (W2-FIX-RUST, 8ace8bc) — in local-only mode `ensure_lan_tls_with` makes no Cloudflare/ACME call (serves a still-valid cached certificate, else plain `ws://`) and `LanTlsManager` does one offline pass and stops. The suggested "allow upkeep in local-only mode" toggle needs a Config field (ssc-core frozen) and is not done.

## CR-15 — PWA has no Content-Security-Policy while holding the direction keys in localStorage
- From: W2-HARDEN. Severity: **low** (defence in depth; no injection sink found). Owner: W1-PHONE (`apps/phone`).
- What: `apps/phone/src/lib/connection.ts:1,40` keep the pairing (both direction keys) in `localStorage`; `apps/phone/next.config.ts` (static export) and `app/layout.tsx` set no CSP, so any script that ever runs on the PWA origin (a compromised dependency, a future HTML sink) can read the keys and decrypt or forge frames.
- Failing test: none (outside HARDEN's test paths); checked by reading the files.
- Proposed fix: add `<meta http-equiv="Content-Security-Policy" content="default-src 'self'; connect-src 'self' wss: ws:; script-src 'self'; object-src 'none'; base-uri 'none'">` (plus the hashes Next needs for its inline bootstrap) and a Playwright check that it is present; consider storing the keys as non-extractable WebCrypto keys in IndexedDB.
- Status: fixed (W2-FIX-PHONE, 76b1e36). Pairing sealed in IndexedDB under a non-extractable AES-GCM key (legacy localStorage migrated + deleted); per-page CSP meta with inline-script hashes + `out/_headers` (frame-ancestors etc.); Ajv validators precompiled so no `unsafe-eval` (only `'wasm-unsafe-eval'` for libsodium). connect-src is `'self' wss:` by default, narrow with `SSC_CSP_CONNECT` at build time. A script running live on the origin can still use the key handle; the CSP is the defence there.
