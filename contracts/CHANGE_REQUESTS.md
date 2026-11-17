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
