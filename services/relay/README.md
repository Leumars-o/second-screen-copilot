# Blind relay (`@ssc/relay`)

A Cloudflare Worker with one Durable Object per room. It lets the phone reach the PC when the two
are not on the same network. It forwards encrypted frames between exactly two WebSockets and does
nothing else.

## Protocol

- `GET /r/<room_id_hex>` with `Upgrade: websocket`. `room_id_hex` is exactly 32 lowercase hex
  characters (`HKDF-SHA256(token, info = "ssc/v1/room", L = 16)`, see `contracts/crypto/README.md`).
  - Malformed room id: `400`. Not a WebSocket upgrade: `426`. Not `GET`: `405`. Other paths: `404`.
  - A room holds two sockets. A third connection gets `409` and is not upgraded.
    When one side drops, its slot frees up and it can reconnect.
- Binary frames from one socket are sent byte-for-byte to the other socket. Two frame versions
  pass (`contracts/crypto/README.md`, contracts-v1.1):
  - `0x01`: encrypted frame (`0x01 || nonce || ciphertext`), 41 bytes to 64 KB (65,536 bytes);
  - `0x00`: the unencrypted `pair.hello` (`0x00 || envelope JSON`), 2 bytes to 2 KB (2,048 bytes).
- Frames are dropped silently (no close, no error) when they are:
  - text frames;
  - outside the size range for their version;
  - starting with any other version byte;
  - over the room's rate limit (token buckets shared by both directions; defaults 100 frames/s with
    a burst of 200, and 512 KiB/s with a burst of 1 MiB);
  - sent while the other side isn't connected (the relay does not queue).
- A room expires after 24 hours without a forwarded frame: both sockets are closed with `1001`.
  When both sides disconnect, the room's alarm is deleted at once.

Defaults can be changed with the optional `[vars]` in `wrangler.toml`.

## What the relay can and can't see

**Can see:** the room id (a random-looking 16-byte value derived from the one-time pairing token),
the IP addresses and user agents of the two connections, when they connect, and the size and timing
of each frame. Cloudflare's own request logs see the same metadata.

It can also read the one unencrypted frame, `pair.hello` (`0x00`), if pairing happens through the
relay: the phone's X25519 public key and `proof = HMAC-SHA256(token, phone_pub)`. Neither is
secret: the public key is public by design, and the proof can't be used without the token, which
is only in the QR code and is burned after one use.

**Can't see:** keys, the pairing token, or the content of any other frame. Every frame after
`pair.hello` is `0x01 || nonce || XChaCha20-Poly1305 ciphertext`, encrypted with keys derived from
an X25519 exchange whose private halves never leave the devices. The relay cannot decrypt, forge or
replay frames undetected (the receivers check the AEAD tag and `seq`).

**Doesn't keep:** the relay logs nothing and writes no content to Durable Object storage. The only
persisted state is the room's expiry alarm. Activity timestamps live in memory and in the
WebSocket attachment, never in storage.

**Limits of the design:** anyone who learns a room id can occupy the free slot of that room and
block the real peer (they still can't read or inject frames). Room ids only appear in the QR code
and on the two paired devices.

## Self-hosting

You need a Cloudflare account (the free plan works: the Durable Object is SQLite-backed) and Node 22.12+ (required by wrangler 4; the tests run on Node 20).

```sh
cd services/relay
pnpm install
pnpm exec wrangler login
pnpm exec wrangler deploy
```

`wrangler deploy` prints the Worker URL, e.g. `https://ssc-relay.<your-subdomain>.workers.dev`.
Put the `wss://` form of it in the agent's relay URL setting:
`wss://ssc-relay.<your-subdomain>.workers.dev`. The agent appends `/r/<room_id_hex>`.

To use your own domain, add a route or custom domain to `wrangler.toml`
([docs](https://developers.cloudflare.com/workers/configuration/routing/custom-domains/)).

Check it is up (should print `400`):

```sh
curl -s -o /dev/null -w '%{http_code}\n' https://ssc-relay.<your-subdomain>.workers.dev/r/bad
```

## Development

```sh
pnpm typecheck   # tsc
pnpm test        # vitest with @cloudflare/vitest-pool-workers (local workerd, no network)
pnpm exec wrangler dev   # local relay on http://localhost:8787
```
