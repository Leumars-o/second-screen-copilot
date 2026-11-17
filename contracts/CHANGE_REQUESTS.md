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
- From: W1-PHONE
- What: `aad = session_id || seq` binds two values that are only inside the ciphertext. Nothing in the wire header (`0x01 || nonce || ct`) gives them. The receiver can guess `seq = last + 1`, but it cannot guess a *new* `session_id`: the `session.start` fixture's envelope already carries the new id.
- Why: Without a defined way to recover the AAD, the PC and phone may each pick a different workaround and stop interoperating.
- Workaround in TS (`packages/protocol-ts/src/crypto.ts` `openFrame`, marked `CR-1`): compute the unauthenticated candidate plaintext (`ct_body XOR XChaCha20 keystream from block 1`), read `session_id` and `seq` from it, then run the full AEAD decrypt with that AAD. Nothing is trusted unless the tag verifies. This works with the frozen vectors and with any sender that follows the README. In Rust, `chacha20::XChaCha20` seeked to byte 64 does the same.
- Proposed diff (pick one): (a) add the paragraph above to `contracts/crypto/README.md` as the normative receive algorithm (no wire change, vectors stay valid); or (b) v2 frame `0x02 || seq u64 BE || sid_len u8 || sid || nonce || ct`, which needs new vectors. Recommendation: (a).
- Status: accepted (contracts-v1.1), option (a): normative receive algorithm in contracts/crypto/README.md

## CR-2 — Transport framing of the plaintext pair.hello and pairing confirmation
- From: W1-PHONE
- What: The README does not say how the single plaintext frame is carried, or how the phone learns that pairing succeeded.
- Phone behaviour (CR-2 comments in `client.ts`): `pair.hello` is sent as a WebSocket **text** message (UTF-8 envelope JSON). Every encrypted frame is a **binary** message. The phone treats the first authentic encrypted frame from the PC (the mock sends a `heartbeat`) as pairing confirmation. The PC closes the socket on a failed proof.
- Proposed diff: add those three sentences to `contracts/crypto/README.md` § Pairing.
- Status: accepted (contracts-v1.1) as amended: binary 0x00 framing (`0x00 || envelope JSON`), per CR-R1

## CR-3 — Scope of `seq` across reconnects
- From: W1-PHONE
- What: The contract does not say whether `seq` (and the receiver's last-accepted value) resets on a new WebSocket connection or a LAN↔relay switch.
- Phone behaviour: seq state is per pairing. It is persisted in localStorage and keeps increasing across reconnects and reloads, so the relay cannot replay frames from an earlier connection. If the PC resets its counter on reconnect, the phone drops every frame as a replay.
- Proposed diff: README § Frames: "`seq` is scoped to the pairing (the key pair), not to a connection. Both sides persist next-send and last-accepted values across reconnects and restarts."
- Status: accepted (contracts-v1.1)

## CR-4 — LAN `ws://` from an HTTPS-hosted PWA is blocked as mixed content
- From: W1-PHONE
- What: A PWA served over HTTPS (needed for the service worker, the camera and WebCrypto) cannot open `ws://<lan-ip>` URLs. Browsers block it as mixed content, and Chrome's Private Network Access rules add more limits. If the PWA is served over plain http from the PC instead, it loses the camera, WebCrypto and the service worker. The QR's first URL is therefore unusable in the hosted case, and the phone falls back to the relay.
- Why: "LAN first" will silently become "relay always" on real phones.
- Options: (a) the PC serves the PWA over HTTPS on the LAN with a cert the phone trusts (hard on phones); (b) accept relay-only for the hosted PWA and keep LAN for a wrapped/native shell later; (c) add `wss://` LAN with a public DNS name pointing at the LAN IP plus a real cert (the plex.direct pattern). The lead should decide; nothing in the phone code changes for (b) or (c).
- Status: open
