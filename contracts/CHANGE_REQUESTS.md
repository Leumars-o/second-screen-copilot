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

## CR-R1 — Wire framing of the unencrypted `pair.hello` (relay drops it today)
- From: W1-RELAY
- What: `contracts/crypto/README.md` says `pair.hello` is sent unencrypted, but not how it is framed on the WebSocket. The relay (per its brief) forwards only binary frames whose first byte is `0x01` and drops text frames, so a `pair.hello` sent through the relay as a text/JSON frame is silently dropped. Pairing therefore only works on the LAN unless the framing is pinned down.
- Why: the QR payload lists a `wss://relay…/r/<room_id>` URL, so a phone that is off the PC's network at pairing time would try to pair through the relay and hang.
- Proposed diff (pick one):
  1. `contracts/crypto/README.md`: "`pair.hello` is sent as a binary frame `0x00 || envelope JSON (UTF-8)`; every other frame uses version `0x01`." The relay then also forwards version `0x00` frames (same 64 KB / rate limits). Small relay change, owned by W1-RELAY.
  2. Or state explicitly that pairing is LAN-only and the relay URL in the QR is for later sessions only.
- Status: accepted (contracts-v1.1)
