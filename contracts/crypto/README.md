# Crypto contract (v1)

Both the PC (`ssc-crypto`, RustCrypto) and the phone (`packages/protocol-ts`, libsodium + WebCrypto) must reproduce `vectors.json` byte for byte.

## Pairing

1. PC creates an X25519 keypair (long-lived, kept in the OS keychain) and a 32-byte random one-time `token`.
2. QR payload (JSON, UTF-8):
   ```json
   { "v": 1, "urls": ["ws://192.168.1.20:47823", "wss://relay.example.com/r/<room_id_hex>"],
     "pc_pub": "<b64 32B>", "token": "<b64 32B>", "exp": <unix seconds, now + 120> }
   ```
   `room_id = HKDF-SHA256(ikm = token, salt = empty, info = "ssc/v1/room", L = 16)`, hex. The PC stores the room id after pairing; the relay never sees keys.
3. Phone creates its own X25519 keypair and sends `pair.hello` **unencrypted, once** (envelope with `session_id: null`):
   `proof = HMAC-SHA256(key = token, msg = raw 32-byte phone_pub)`, base64.
   Framing (CR-2, CR-R1): a WebSocket **binary** message `0x00 || envelope JSON (UTF-8)`, on the LAN and through the relay alike. It is the only `0x00` frame; every other frame is `0x01` (below).
4. PC checks `exp`, verifies `proof` in constant time, then burns the token (one use only; also burned after 5 bad attempts). On a bad proof the PC closes the socket. The PC confirms pairing by sending its first encrypted frame (a `heartbeat`); the phone treats the first authentic frame from the PC as confirmation.
5. Both sides compute `shared = X25519(own_priv, peer_pub)`. **Reject an all-zero shared secret** (low-order point).
6. Keys (32 bytes each), standard base64 with padding everywhere base64 is used:
   - `k_pc_to_phone = HKDF-SHA256(ikm = shared, salt = token, info = "ssc/v1/pc->phone")`
   - `k_phone_to_pc = HKDF-SHA256(ikm = shared, salt = token, info = "ssc/v1/phone->pc")`

## Frames

```
wire = 0x01 || nonce (24 random bytes) || XChaCha20-Poly1305(key, nonce, plaintext, aad)
plaintext = envelope JSON (UTF-8)
aad = UTF-8(session_id as lowercase hyphenated UUID, or "" when null) || seq as u64 big-endian
```

- The sender uses its own direction key; the receiver uses the other one.
- Receive algorithm (CR-1, normative). `session_id` and `seq` are not in the clear, so:
  1. Check the version byte is `0x01` and the length is at least 41 bytes.
  2. Compute the *unauthenticated* candidate plaintext: ciphertext body XOR the XChaCha20 keystream starting at block 1 (byte offset 64), the same keystream the AEAD uses for encryption. Read only `session_id` and `seq` from it, as candidate AAD values.
  3. Run the full AEAD open with `aad = session_id || seq`. **Nothing from step 2 is trusted or acted on unless the tag verifies.** On failure, drop the frame.
- Receiver checks after a successful open: AEAD must verify; the decrypted envelope's `seq` and `session_id` must equal the ones used in the AAD; `seq` must be greater than the last accepted `seq` for that direction (replay protection). Any failure drops the frame silently and increments a counter.
- `seq` scope (CR-3): `seq` belongs to the **pairing** (the key pair), not to a connection. Both sides persist next-send and last-accepted per direction across reconnects, LAN↔relay switches and restarts. To survive crashes, persist a reservation (e.g. `next + 1000`) and jump to it on restart; gaps are fine, going backwards is not.
- Nonces are random per frame; never reuse a (key, nonce) pair.
- The only unencrypted frame is the single `pair.hello` (`0x00` framing above).

## Files

- `vectors.json`: fixed test keys, derived values and two sealed frames. Test-only keys.
- `gen_vectors.py`: the reference generator (OpenSSL via `cryptography`, libsodium via PyNaCl).
