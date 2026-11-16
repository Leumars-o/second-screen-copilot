// Pure frame and rate-limit rules for the blind relay. No I/O, no logging.
// The relay never decrypts: it only checks the outer shape of the wire frames from
// contracts/crypto/README.md (contracts-v1.1):
//   0x01 || nonce (24 bytes) || XChaCha20-Poly1305 ciphertext (>= 16-byte tag)   encrypted frame
//   0x00 || envelope JSON (UTF-8)                                                 pair.hello only

export const FRAME_VERSION = 0x01;
export const PAIR_HELLO_VERSION = 0x00;
export const MAX_FRAME_BYTES = 64 * 1024;
/** 1 version byte + 24-byte nonce + 16-byte Poly1305 tag. Anything shorter cannot be a valid frame. */
export const MIN_FRAME_BYTES = 1 + 24 + 16;
/** pair.hello is a small JSON envelope; capped well below the encrypted-frame limit. */
export const MAX_PAIR_HELLO_BYTES = 2 * 1024;
/** Version byte plus at least one byte of JSON. */
export const MIN_PAIR_HELLO_BYTES = 2;

export const ROOM_PATH = /^\/r\/([0-9a-f]{32})$/;

export type DropReason = "text" | "too_large" | "too_small" | "bad_version";

/** Returns null when the frame may be forwarded, otherwise why it is dropped. */
export function checkFrame(msg: string | ArrayBuffer): DropReason | null {
  if (typeof msg === "string") return "text";
  if (msg.byteLength === 0) return "too_small";
  const version = new Uint8Array(msg, 0, 1)[0];
  let min: number;
  let max: number;
  if (version === FRAME_VERSION) {
    min = MIN_FRAME_BYTES;
    max = MAX_FRAME_BYTES;
  } else if (version === PAIR_HELLO_VERSION) {
    min = MIN_PAIR_HELLO_BYTES;
    max = MAX_PAIR_HELLO_BYTES;
  } else {
    return "bad_version";
  }
  if (msg.byteLength > max) return "too_large";
  if (msg.byteLength < min) return "too_small";
  return null;
}

/** Classic token bucket. `now` is injected so the rule is testable without timers. */
export class TokenBucket {
  private tokens: number;
  private last: number;

  constructor(
    private readonly ratePerSec: number,
    private readonly burst: number,
    now: number,
  ) {
    this.tokens = burst;
    this.last = now;
  }

  take(cost: number, now: number): boolean {
    const elapsed = Math.max(0, now - this.last) / 1000;
    this.tokens = Math.min(this.burst, this.tokens + elapsed * this.ratePerSec);
    this.last = now;
    if (cost > this.tokens) return false;
    this.tokens -= cost;
    return true;
  }
}

export interface Limits {
  framesPerSec: number;
  burstFrames: number;
  bytesPerSec: number;
  burstBytes: number;
  idleMs: number;
}

export const DEFAULT_LIMITS: Limits = {
  framesPerSec: 100,
  burstFrames: 200,
  bytesPerSec: 512 * 1024,
  burstBytes: 1024 * 1024,
  idleMs: 24 * 60 * 60 * 1000,
};

function positive(v: string | undefined, fallback: number): number {
  if (v === undefined || v === "") return fallback;
  const n = Number(v);
  return Number.isFinite(n) && n > 0 ? n : fallback;
}

/** Optional overrides from wrangler `[vars]`. Invalid values fall back to the defaults. */
export function limitsFromEnv(env: Partial<Record<string, unknown>>): Limits {
  const s = (k: string) => (typeof env[k] === "string" ? (env[k] as string) : undefined);
  return {
    framesPerSec: positive(s("RATE_FRAMES_PER_SEC"), DEFAULT_LIMITS.framesPerSec),
    burstFrames: positive(s("RATE_BURST_FRAMES"), DEFAULT_LIMITS.burstFrames),
    bytesPerSec: positive(s("RATE_BYTES_PER_SEC"), DEFAULT_LIMITS.bytesPerSec),
    // A burst smaller than one max-size frame would block legal frames forever.
    burstBytes: Math.max(MAX_FRAME_BYTES, positive(s("RATE_BURST_BYTES"), DEFAULT_LIMITS.burstBytes)),
    idleMs: positive(s("ROOM_IDLE_MS"), DEFAULT_LIMITS.idleMs),
  };
}
