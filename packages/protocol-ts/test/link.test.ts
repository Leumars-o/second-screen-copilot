// PhoneLink against the mock PC over real WebSockets (localhost only).
import { createServer } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import WebSocket from "ws";
import { MockPc } from "../mock-server/server";
import { helloFrame, parseQr, PhoneLink, startPairing, type AnyEnvelope, type ChannelState, type LinkStatus, type WsLike } from "../src/index";

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

  it("answers a PC heartbeat probe at once with echo, and never answers an echo", async () => {
    await pairedLink();
    await new Promise((r) => setTimeout(r, 1300)); // past the phone's first tick; next one is ~5 s away
    const t0 = Date.now();
    await pc!.send({ type: "heartbeat", payload: {} });
    // The echo must be of this probe (ts >= t0), well before the phone's 5 s tick.
    await until(() => pc!.received.some((e) => e.type === "heartbeat" && (e.payload.echo ?? 0) >= t0), 1000);
    const n = pc!.received.length;
    await pc!.send({ type: "heartbeat", payload: { echo: 1 } });
    await new Promise((r) => setTimeout(r, 200));
    expect(pc!.received.length).toBe(n);
  });

  it("periodic heartbeats are probes: never a stale echo of an earlier PC heartbeat (H1)", async () => {
    // Skewable clock so the 5 s periodic heartbeat comes within a second.
    let skew = 0;
    pc = await new MockPc().start();
    const start = await startPairing(parseQr(JSON.stringify(pc.qr)), "Test phone");
    let paired = false;
    link = new PhoneLink({ pairing: start.pairing, hello: start.hello, ws: wsFactory, onEnvelope: () => undefined, onPaired: () => (paired = true), now: () => Date.now() + skew });
    link.start();
    await until(() => paired);
    // The PC probes; the phone echoes that probe once, at once.
    await pc.send({ type: "heartbeat", payload: {} });
    await until(() => pc!.received.some((e) => e.type === "heartbeat" && e.payload.echo !== undefined));
    const n = pc.received.length;
    skew = 5_000; // next tick sends the periodic heartbeat
    const isProbe = (e: AnyEnvelope) => e.type === "heartbeat" && e.payload.echo === undefined;
    await until(() => pc!.received.slice(n).some(isProbe), 2500);
    // Each PC heartbeat is echoed exactly once (the old phone re-echoed the last one on its tick).
    const echoed = pc.received.flatMap((e) => (e.type === "heartbeat" && e.payload.echo !== undefined ? [e.payload.echo] : []));
    expect(echoed.length).toBeGreaterThan(0);
    expect(new Set(echoed).size).toBe(echoed.length);
  });

  it("resends pair.hello on the next connection when the first one is lost, once per connection (L4)", async () => {
    pc = await new MockPc().start();
    const start = await startPairing(parseQr(JSON.stringify(pc.qr)), "Test phone");
    const hellosPerSocket: number[] = [];
    let sockets = 0;
    // Socket 1: the hello is swallowed and the socket dies before the PC sees anything.
    const factory = (url: string): WsLike => {
      const ws = new WebSocket(url) as unknown as WsLike;
      const n = sockets++;
      hellosPerSocket[n] = 0;
      const send = ws.send.bind(ws);
      ws.send = (data) => {
        const isHello = data instanceof Uint8Array && data[0] === 0x00;
        if (isHello) hellosPerSocket[n]!++;
        if (n === 0) {
          if (isHello) setTimeout(() => ws.close(), 0);
          return;
        }
        send(data);
      };
      return ws;
    };
    let paired = false;
    let confirmed: boolean | undefined;
    link = new PhoneLink({ pairing: start.pairing, hello: start.hello, ws: factory, onEnvelope: () => undefined, onPaired: () => (paired = true), onPersist: (p) => (confirmed = p.helloConfirmed) });
    link.start();
    await until(() => paired, 5000);
    expect(pc.pairedCount).toBe(1);
    expect(confirmed).toBe(true);
    expect(hellosPerSocket.slice(0, 2)).toEqual([1, 1]);
    // Confirmed: a later reconnect sends no hello.
    await fetch(`http://127.0.0.1:${pc.port}/__mock/drop`);
    await until(() => sockets >= 3 && link!.connected, 5000);
    expect(hellosPerSocket[2]).toBe(0);
  });

  it("pairing still confirms when the PC's confirmation was lost: the repeated hello is ignored (L4)", async () => {
    pc = await new MockPc().start();
    const start = await startPairing(parseQr(JSON.stringify(pc.qr)), "Test phone");
    let sockets = 0;
    // Socket 1: everything from the PC is dropped, then the socket closes after the hello landed.
    const factory = (url: string): WsLike => {
      const ws = new WebSocket(url) as unknown as WsLike;
      if (sockets++ === 0) {
        const real = ws as unknown as WebSocket;
        real.on("message", () => undefined);
        Object.defineProperty(ws, "onmessage", { set: () => undefined, get: () => null });
        setTimeout(() => ws.close(), 300);
      }
      return ws;
    };
    let paired = false;
    link = new PhoneLink({ pairing: start.pairing, hello: start.hello, ws: factory, onEnvelope: () => undefined, onPaired: () => (paired = true) });
    link.start();
    await until(() => pc!.pairedCount === 1);
    expect(paired).toBe(false);
    await until(() => paired, 5000);
    expect(sockets).toBe(2);
    expect(pc.pairedCount).toBe(1); // the repeat did not re-pair or get the socket closed
    expect(link.connected).toBe(true);
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
    expect(pc!.pairedCount).toBe(1); // no re-pairing after the reconnect
  });

  it("mock PC closes the socket on a text frame and on a bad proof, then accepts a good hello", async () => {
    pc = await new MockPc().start();
    const closeCode = (send: (ws: WebSocket) => void) =>
      new Promise<number>((resolve) => {
        const ws = new WebSocket(pc!.lanUrl);
        ws.on("open", () => send(ws));
        ws.on("close", (code) => resolve(code));
      });
    const qr = parseQr(JSON.stringify(pc.qr));
    const good = await startPairing(qr, "Phone");
    expect(await closeCode((ws) => ws.send(good.hello))).toBe(1003); // text, not binary
    const forged = JSON.parse(good.hello);
    forged.payload.proof = "A".repeat(43) + "=";
    expect(await closeCode((ws) => ws.send(helloFrame(JSON.stringify(forged))))).toBe(4002);
    expect(pc.pairedCount).toBe(0);
    // A bad attempt does not burn the token (only 5 do): the real hello still pairs.
    const ws = new WebSocket(pc.lanUrl);
    await new Promise((r) => ws.on("open", r));
    ws.send(helloFrame(good.hello));
    await until(() => pc!.pairedCount === 1);
    ws.close();
  });
});
