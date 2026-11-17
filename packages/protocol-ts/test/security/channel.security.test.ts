// W2-HARDEN security suite, phone side (docs/security/threat-model.md §2–3): what an attacker on
// the relay or the Wi-Fi can do to the phone's receive path. Tamper, replay, reflection,
// plaintext injection, garbage and oversized frames are all dropped before the app sees them.
import { afterEach, describe, expect, it } from "vitest";
import { PhoneLink, SecureChannel, helloFrame, sealFrame, toB64, type AnyEnvelope, type Pairing, type WsLike } from "../../src/index";

const PC_TO_PHONE = new Uint8Array(32).fill(0x11);
const PHONE_TO_PC = new Uint8Array(32).fill(0x22);
const SID = "6f1c2b9e-7d43-4f3a-9a51-2c0d4b8e1f00";
const MARKER = "MARKER-PC-ANSWER-45000-NAIRA";

function pc(): SecureChannel {
  return new SecureChannel({ sendKey: PC_TO_PHONE, recvKey: PHONE_TO_PC, now: () => 1 });
}
function phone(): SecureChannel {
  return new SecureChannel({ sendKey: PHONE_TO_PC, recvKey: PC_TO_PHONE, now: () => 1 });
}
const delta = (text: string) =>
  ({ type: "answer.delta", payload: { answer_id: "00000000-0000-0000-0000-000000000001", block: "cue", text } }) as const;

function contains(hay: Uint8Array, needle: string): boolean {
  const n = new TextEncoder().encode(needle);
  outer: for (let i = 0; i + n.length <= hay.length; i++) {
    for (let j = 0; j < n.length; j++) if (hay[i + j] !== n[j]) continue outer;
    return true;
  }
  return false;
}

describe("phone receive path", () => {
  it("sealed frames show no plaintext and never repeat", async () => {
    const a = await pc().seal(delta(MARKER), SID);
    const b = await pc().seal(delta(MARKER), SID);
    for (const w of [a, b]) {
      expect(w[0]).toBe(0x01);
      for (const s of [MARKER, "answer.delta", '"type"', SID, "payload"]) expect(contains(w, s)).toBe(false);
    }
    expect(toB64(a)).not.toBe(toB64(b));
  });

  it("drops every single-bit flip and still opens the genuine frame", async () => {
    const wire = await pc().seal(delta("hello"), SID);
    const ph = phone();
    for (let i = 0; i < wire.length; i++) {
      const t = wire.slice();
      t[i] = t[i]! ^ 0x01;
      expect(await ph.open(t)).toBeNull();
    }
    expect(ph.dropped.crypto + ph.dropped.schema + ph.dropped.mismatch).toBe(wire.length);
    expect((await ph.open(wire))?.seq).toBe(1);
  });

  it("drops replays and older frames without moving the window", async () => {
    const sender = pc();
    const f1 = await sender.seal(delta("1"), SID);
    const f2 = await sender.seal(delta("2"), SID);
    const f3 = await sender.seal(delta("3"), SID);
    const ph = phone();
    expect((await ph.open(f2))?.seq).toBe(2);
    expect(await ph.open(f2)).toBeNull();
    expect(await ph.open(f1)).toBeNull();
    expect(ph.dropped.replay).toBe(2);
    expect((await ph.open(f3))?.seq).toBe(3);
    expect(ph.state.lastRecvSeq).toBe(3);
  });

  it("drops reflected frames (its own, bounced back) and frames from another pairing", async () => {
    const ph = phone();
    const own = await ph.seal({ type: "heartbeat", payload: {} }, null);
    expect(await ph.open(own)).toBeNull();
    const stranger = new SecureChannel({ sendKey: new Uint8Array(32).fill(0x33), recvKey: new Uint8Array(32).fill(0x44) });
    expect(await ph.open(await stranger.seal(delta("forged"), SID))).toBeNull();
    expect(ph.dropped.crypto).toBe(2);
  });

  it("drops plaintext 0x00 frames, AAD splices, garbage and huge frames", async () => {
    const ph = phone();
    const forged: AnyEnvelope = { v: 1, type: "session.end", seq: 99, ts: 1, session_id: SID, payload: { session_id: SID, reason: "user" } };
    expect(await ph.open(helloFrame(JSON.stringify(forged)))).toBeNull();
    // Ciphertext of seq 7 presented under the AAD of seq 8 (and vice versa) never verifies.
    const json = new TextEncoder().encode(JSON.stringify({ ...forged, seq: 7 }));
    expect(await ph.open(await sealFrame(PC_TO_PHONE, json, SID, 8))).toBeNull();
    for (const len of [0, 1, 25, 40, 41, 1000, 70 * 1024]) {
      const junk = new Uint8Array(len).map((_, i) => (i * 31 + 7) & 0xff);
      if (len > 0) junk[0] = 0x01;
      expect(await ph.open(junk)).toBeNull();
    }
    expect(ph.state.lastRecvSeq).toBe(-1);
  });
});

// ---- PhoneLink over a fake socket ----

class FakeWs implements WsLike {
  binaryType = "blob";
  readyState = 0;
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  sent: Array<string | Uint8Array> = [];
  send(d: string | Uint8Array): void {
    this.sent.push(d);
  }
  close(): void {
    this.readyState = 3;
  }
  open(): void {
    this.readyState = 1;
    this.onopen?.({});
  }
  deliver(data: unknown): void {
    this.onmessage?.({ data });
  }
}

const pairing: Pairing = {
  v: 1,
  pc_pub: toB64(new Uint8Array(32).fill(9)),
  urls: ["ws://192.168.1.20:47823"],
  room_id: "00".repeat(16),
  k_pc_to_phone: toB64(PC_TO_PHONE),
  k_phone_to_pc: toB64(PHONE_TO_PC),
  created_at: 0,
};

let link: PhoneLink | undefined;
afterEach(() => link?.stop());

const settle = () => new Promise((r) => setTimeout(r, 20));

describe("PhoneLink", () => {
  it("delivers only authentic frames: text, plaintext, tampered and replayed frames never reach the app", async () => {
    const got: AnyEnvelope[] = [];
    let ws: FakeWs | undefined;
    link = new PhoneLink({
      pairing,
      ws: () => (ws = new FakeWs()),
      onEnvelope: (e) => got.push(e),
    });
    link.start();
    ws!.open();
    const sender = pc();
    const good = await sender.seal(delta(MARKER), SID);
    const tampered = good.slice();
    tampered[tampered.length - 1] = tampered[tampered.length - 1]! ^ 1;
    ws!.deliver(JSON.stringify({ v: 1, type: "session.end", seq: 50, ts: 1, session_id: SID, payload: { session_id: SID, reason: "user" } }));
    ws!.deliver(helloFrame('{"v":1,"type":"session.start"}').buffer);
    ws!.deliver(tampered.buffer);
    ws!.deliver(good.buffer);
    ws!.deliver(good.buffer); // replay
    await settle();
    expect(got.map((e) => e.type)).toEqual(["answer.delta"]);
    expect(link.dropped.replay).toBe(1);
    // The phone's own frames on the wire are sealed too (heartbeat after open).
    expect(ws!.sent.every((f) => f instanceof Uint8Array && f[0] === 0x01)).toBe(true);
  });
});
