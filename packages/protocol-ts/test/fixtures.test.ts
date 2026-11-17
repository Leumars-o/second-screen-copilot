import { readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { decodeEnvelope, encodeEnvelope, isValidEnvelope, MESSAGE_TYPES, validateQr } from "../src/index";

const FIX = resolve(__dirname, "../../../contracts/protocol/fixtures");
const load = (dir: string) =>
  readdirSync(join(FIX, dir))
    .filter((f) => f.endsWith(".json"))
    .sort()
    .map((f) => ({ name: f, text: readFileSync(join(FIX, dir, f), "utf8") }));

const valid = load("valid");
const invalid = load("invalid");

describe("contract fixtures (same labels as the Rust conformance test)", () => {
  it("has fixtures", () => {
    expect(valid.length).toBeGreaterThan(10);
    expect(invalid.length).toBeGreaterThan(10);
  });

  it.each(valid)("valid/$name parses and round-trips", ({ text }) => {
    const value = JSON.parse(text);
    const env = decodeEnvelope(text);
    expect(env).toEqual(value);
    // Re-encoding keeps the canonical key order and the same JSON value.
    expect(JSON.parse(encodeEnvelope(env))).toEqual(value);
    expect(Object.keys(JSON.parse(encodeEnvelope(env)))).toEqual(["v", "type", "seq", "ts", "session_id", "payload"]);
  });

  it.each(invalid)("invalid/$name is rejected", ({ text }) => {
    expect(isValidEnvelope(JSON.parse(text))).toBe(false);
    expect(() => decodeEnvelope(text)).toThrow();
  });

  it("every message type has a valid fixture", () => {
    const types = new Set(valid.map((f) => JSON.parse(f.text).type));
    for (const t of MESSAGE_TYPES) expect(types, t).toContain(t);
  });

  it("encodeEnvelope refuses to emit an invalid frame", () => {
    const bad = { v: 1, type: "answer.delta", seq: 1, ts: 1, session_id: null, payload: { answer_id: "x", block: "cue", text: "" } };
    expect(() => encodeEnvelope(bad as never)).toThrow(/schema violation/);
  });

  it("rejects non-UTF-8 and non-JSON input", () => {
    expect(() => decodeEnvelope(new Uint8Array([0xff, 0xfe]))).toThrow();
    expect(() => decodeEnvelope("not json")).toThrow();
  });
});

describe("QR schema", () => {
  const good = {
    v: 1,
    urls: ["ws://192.168.1.20:47823", "wss://relay.example.com/r/42820c9fc7ad02737ae7dacc15a76d09"],
    pc_pub: "B6N8vBQgk8i3VdwbEOhstCY3StFqqFPtC9/AsrhtHHw=",
    token: "yMnKy8zNzs/Q0dLT1NXW19jZ2tvc3d7f4OHi4+Tl5uc=",
    exp: 1763290120,
  };
  it("accepts a well-formed payload", () => expect(() => validateQr(good)).not.toThrow());
  it.each([
    ["http url", { ...good, urls: ["http://x"] }],
    ["no urls", { ...good, urls: [] }],
    ["short key", { ...good, pc_pub: "abc=" }],
    ["extra field", { ...good, x: 1 }],
    ["v2", { ...good, v: 2 }],
  ])("rejects %s", (_n, v) => expect(() => validateQr(v)).toThrow());
});
