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

## CR-1 — Frame AAD values are not recoverable before decryption
- From: W1-PIPE
- What: `aad = session_id || seq`, but the wire (`0x01 || nonce || ct`) sends neither in the clear, so a receiver cannot build the AAD it needs to check the tag.
- Decision (lead, option a, no wire change, vectors unchanged): check that the first byte is `0x01`. Decrypt the body with the XChaCha20 keystream from block 1 (`chacha20::XChaCha20` seeked to byte 64; libsodium `crypto_stream_xchacha20_xor_ic(ic = 1)`) without checking the tag. Read only `session_id` and `seq` from that candidate plaintext, then run the full AEAD open with that AAD. Trust and parse nothing unless the tag verifies; the authenticated `seq`/`session_id` must equal the AAD values. Drop the frame on any failure. Implemented in `ssc-crypto::frame::open` (`// CR-1`).
- Status: accepted (lead updates contracts/crypto/README.md at merge)

## CR-2 — Link framing for pair.hello (with CR-R1)
- From: W1-PIPE
- Decision (lead): `pair.hello` is a binary frame `0x00 || envelope JSON (UTF-8)` on the LAN and the relay alike. Every other frame is binary `0x01 || nonce || ct`, and text frames are ignored. Pairing is confirmed by the first authentic encrypted frame from the PC (a `heartbeat`). On a bad proof or an expired token, the PC closes the socket. On the relay, the socket belongs to the PC, so it stays open and the frame is dropped. Also implemented: a `heartbeat` without `echo` is answered at once with `echo = its ts` (RTT = now − echo), and the PC sends on the socket that last delivered a valid phone frame, falling back to the relay socket if there is none.
- Status: accepted (lead updates the README at merge)

## CR-3 — seq scoped to the pairing, persisted with block reservation
- From: W1-PIPE
- Decision (lead): `seq` is per pairing, not per connection. Persisted per pairing: `Counters { next_send, last_recv }`. `next_send` is a reservation: on start the sender reserves `[next_send, next_send + 1000)` and persists the new upper bound before sending; crossing a block reserves the next block and persists it again. `last_recv` is the replay window (the PC persists it at most once a second, and on clean shutdown through `device_snapshot`). Implemented in `ssc-crypto::frame::SecureChannel` (`SEQ_RESERVE_BLOCK = 1000`, `take_persist`). `PcTransport::take_device_updates` emits the records to write to the keychain (`// CR-3`).
- Status: accepted (lead updates the README at merge)
