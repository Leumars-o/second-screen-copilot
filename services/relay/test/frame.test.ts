import { describe, expect, it } from "vitest";
import {
  checkFrame,
  DEFAULT_LIMITS,
  limitsFromEnv,
  MAX_FRAME_BYTES,
  MAX_PAIR_HELLO_BYTES,
  MIN_FRAME_BYTES,
  ROOM_PATH,
  TokenBucket,
} from "../src/frame";

function frame(len: number, version = 0x01): ArrayBuffer {
  const b = new Uint8Array(len);
  if (len > 0) b[0] = version;
  return b.buffer;
}

describe("checkFrame", () => {
  it("accepts version-1 binary frames from the minimum size up to 64 KB", () => {
    expect(checkFrame(frame(MIN_FRAME_BYTES))).toBeNull();
    expect(checkFrame(frame(MAX_FRAME_BYTES))).toBeNull();
  });
  it("drops text, oversized, undersized and wrong-version frames", () => {
    expect(checkFrame("hello")).toBe("text");
    expect(checkFrame(frame(MAX_FRAME_BYTES + 1))).toBe("too_large");
    expect(checkFrame(frame(MIN_FRAME_BYTES - 1))).toBe("too_small");
    expect(checkFrame(frame(0))).toBe("too_small");
    expect(checkFrame(frame(100, 0x02))).toBe("bad_version");
    expect(checkFrame(frame(100, 0xff))).toBe("bad_version");
    expect(checkFrame(frame(MAX_PAIR_HELLO_BYTES, 0x02))).toBe("bad_version");
  });
  it("accepts 0x00 pair.hello frames from 2 bytes up to 2 KB", () => {
    expect(checkFrame(frame(2, 0x00))).toBeNull();
    expect(checkFrame(frame(MAX_PAIR_HELLO_BYTES, 0x00))).toBeNull();
    expect(checkFrame(frame(1, 0x00))).toBe("too_small");
    expect(checkFrame(frame(MAX_PAIR_HELLO_BYTES + 1, 0x00))).toBe("too_large");
  });
});

describe("ROOM_PATH", () => {
  it("matches exactly 32 lowercase hex chars", () => {
    expect(ROOM_PATH.test("/r/" + "a".repeat(32))).toBe(true);
    expect(ROOM_PATH.test("/r/" + "A".repeat(32))).toBe(false);
    expect(ROOM_PATH.test("/r/" + "a".repeat(31))).toBe(false);
    expect(ROOM_PATH.test("/r/" + "a".repeat(33))).toBe(false);
    expect(ROOM_PATH.test("/r/" + "g".repeat(32))).toBe(false);
    expect(ROOM_PATH.test("/r/" + "a".repeat(32) + "/")).toBe(false);
  });
});

describe("TokenBucket", () => {
  it("allows the burst, then refills at the rate", () => {
    const b = new TokenBucket(10, 3, 0);
    expect([b.take(1, 0), b.take(1, 0), b.take(1, 0), b.take(1, 0)]).toEqual([true, true, true, false]);
    expect(b.take(1, 50)).toBe(false); // 0.5 token
    expect(b.take(1, 100)).toBe(true); // 1 token after 100 ms at 10/s
    expect(b.take(1, 10_000)).toBe(true);
    expect(b.take(3, 10_000)).toBe(false); // capped at burst (2 left)
  });
  it("ignores a clock that goes backwards", () => {
    const b = new TokenBucket(1, 1, 1000);
    expect(b.take(1, 1000)).toBe(true);
    expect(b.take(1, 0)).toBe(false);
  });
});

describe("limitsFromEnv", () => {
  it("uses defaults for missing or invalid values", () => {
    expect(limitsFromEnv({})).toEqual(DEFAULT_LIMITS);
    expect(limitsFromEnv({ RATE_BURST_FRAMES: "nope", ROOM_IDLE_MS: "-5" })).toEqual(DEFAULT_LIMITS);
  });
  it("never lets the byte burst fall below one max-size frame", () => {
    expect(limitsFromEnv({ RATE_BURST_BYTES: "10" }).burstBytes).toBe(MAX_FRAME_BYTES);
  });
});
