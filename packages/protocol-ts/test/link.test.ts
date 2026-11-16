// PhoneLink against the mock PC over real WebSockets (localhost only).
import { createServer } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import WebSocket from "ws";
import { MockPc } from "../mock-server/server";
import { parseQr, PhoneLink, startPairing, type AnyEnvelope, type ChannelState, type LinkStatus, type WsLike } from "../src/index";

const wsFactory = (url: string) => new WebSocket(url) as unknown as WsLike;

async function freePort(): Promise<number> {
  return new Promise((r) => {
    const s = createServer().listen(0, "127.0.0.1", () => {
      const a = s.address();
      s.close(() => r(typeof a === "object" && a ? a.port : 0));
    });
  });
}

async function until(cond: () => boolean, ms = 3000): Promise<void> {
  const t0 = Date.now();
  while (!cond()) {
    if (Date.now() - t0 > ms) throw new Error("timeout");
    await new Promise((r) => setTimeout(r, 10));
  }
}

let pc: MockPc | undefined;
let link: PhoneLink | undefined;
afterEach(async () => {
  link?.stop();
  await pc?.close();
});

async function pairedLink(qrUrls?: (lan: string) => string[]) {
  pc = await new MockPc(qrUrls ? { qrUrls } : {}).start();
  const qr = parseQr(JSON.stringify(pc.qr));
  const start = await startPairing(qr, "Test phone");
  const got: AnyEnvelope[] = [];
  const statuses: LinkStatus[] = [];
  let persisted: { state: ChannelState; sessionId: string | null } | undefined;
  let paired = false;
  link = new PhoneLink({
    pairing: start.pairing,
    hello: start.hello,
    ws: wsFactory,
    onEnvelope: (e) => got.push(e),
    onStatus: (s) => statuses.push(s),
    onPersist: (p) => (persisted = p),
    onPaired: () => (paired = true),
  });
  link.start();
  await until(() => paired);
  return { start, got, statuses, persisted: () => persisted };
}

describe("PhoneLink <-> mock PC", () => {
  it("pairs, receives a session and a streamed answer, sends commands", async () => {
    const { got } = await pairedLink();
    const sid = await pc!.startSession();
    await until(() => link!.sessionId === sid);
    const answerId = await pc!.streamAnswer({ count: 5 });
    await until(() => got.some((e) => e.type === "answer.end"));
    const types = got.map((e) => e.type).filter((t) => t !== "heartbeat");
    expect(types).toEqual(["session.start", "answer.start", "answer.delta", "answer.delta", "answer.delta", "answer.delta", "answer.delta", "answer.end"]);

    expect(await link!.send({ type: "command", payload: { action: "pin", answer_id: answerId } })).toBe(true);
    await until(() => pc!.received.some((e) => e.type === "command"));
    const cmd = pc!.received.find((e) => e.type === "command")!;
    expect(cmd.session_id).toBe(sid);
    expect(cmd.payload).toEqual({ action: "pin", answer_id: answerId });

    await link!.send({ type: "command", payload: { action: "memory.list" } });
    await until(() => got.some((e) => e.type === "memory.snapshot"));
    expect(link!.dropped).toEqual({ crypto: 0, schema: 0, replay: 0, mismatch: 0 });
  });

  it("falls back from a dead LAN url to the next url", async () => {
    const dead = await freePort();
    const { statuses } = await pairedLink((lan) => [`ws://127.0.0.1:${dead}`, `${lan}/r/room`]);
    expect(statuses[0]).toEqual({ state: "connecting", url: `ws://127.0.0.1:${dead}` });
    expect(statuses.find((s) => s.state === "connected")).toMatchObject({ via: "relay" });
  });

  it("reconnects after the socket drops and keeps seq increasing (no replay reset)", async () => {
    const { persisted } = await pairedLink();
    const before = persisted()!.state.nextSendSeq;
    await fetch(`http://127.0.0.1:${pc!.port}/__mock/drop`);
    await until(() => !link!.connected);
    await until(() => link!.connected, 5000);
    await link!.send({ type: "command", payload: { action: "memory.list" } });
    await until(() => pc!.received.filter((e) => e.type === "command").length === 1);
    const cmd = pc!.received.find((e) => e.type === "command")!;
    expect(cmd.seq).toBeGreaterThanOrEqual(before);
    expect(pc!.pairedCount).toBe(1); // hello is sent only once
  });
});
