import { env, runDurableObjectAlarm, runInDurableObject, SELF } from "cloudflare:test";
import { afterEach, describe, expect, it } from "vitest";
import type { RelayRoom } from "../src/index";
import { MAX_FRAME_BYTES, MAX_PAIR_HELLO_BYTES } from "../src/frame";

let roomCounter = 0;
function newRoomId(): string {
  roomCounter += 1;
  return roomCounter.toString(16).padStart(32, "0");
}

interface Client {
  ws: WebSocket;
  received: ArrayBuffer[];
  texts: string[];
  closed: Promise<CloseEvent>;
}

const open: WebSocket[] = [];

async function connect(roomId: string): Promise<Client> {
  const c = await tryConnect(roomId);
  if (!c) throw new Error("connect refused");
  return c;
}

/** Returns null when the relay answers 409 (room full). */
async function tryConnect(roomId: string): Promise<Client | null> {
  const res = await SELF.fetch(`http://relay/r/${roomId}`, { headers: { Upgrade: "websocket" } });
  if (res.status === 409) return null;
  expect(res.status).toBe(101);
  const ws = res.webSocket;
  if (!ws) throw new Error("no websocket");
  const received: ArrayBuffer[] = [];
  const texts: string[] = [];
  ws.addEventListener("message", (e: MessageEvent) => {
    if (typeof e.data === "string") texts.push(e.data);
    else received.push(e.data as ArrayBuffer);
  });
  const closed = new Promise<CloseEvent>((resolve) => ws.addEventListener("close", resolve));
  ws.accept();
  open.push(ws);
  return { ws, received, texts, closed };
}

function frame(len: number, fill: number, version = 0x01): ArrayBuffer {
  const b = new Uint8Array(len).fill(fill);
  b[0] = version;
  return b.buffer;
}

function bytes(b: ArrayBuffer | undefined): number[] {
  return b ? Array.from(new Uint8Array(b)) : [];
}

const tick = (ms = 50) => new Promise((r) => setTimeout(r, ms));

async function until(cond: () => boolean, ms = 2000): Promise<void> {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error("timed out");
    await tick(5);
  }
}

function stub(roomId: string) {
  return env.ROOMS.get(env.ROOMS.idFromName(roomId));
}

async function storageKeys(roomId: string): Promise<string[]> {
  return runInDurableObject(stub(roomId), async (_i: RelayRoom, state) => [...(await state.storage.list()).keys()]);
}

afterEach(() => {
  for (const ws of open.splice(0)) {
    try {
      ws.close(1000, "test done");
    } catch {
      // already closed
    }
  }
});

describe("routing", () => {
  it("rejects a malformed room id with 400", async () => {
    for (const bad of ["abc", "A".repeat(32), "g".repeat(32), "a".repeat(31), "a".repeat(33), ""]) {
      const res = await SELF.fetch(`http://relay/r/${bad}`, { headers: { Upgrade: "websocket" } });
      expect(res.status, bad).toBe(400);
    }
  });
  it("rejects non-websocket requests and other paths", async () => {
    expect((await SELF.fetch(`http://relay/r/${newRoomId()}`)).status).toBe(426);
    expect((await SELF.fetch("http://relay/")).status).toBe(404);
    expect((await SELF.fetch(`http://relay/r/${newRoomId()}`, { method: "POST" })).status).toBe(405);
  });
});

describe("relaying", () => {
  it("forwards binary frames both ways, byte for byte", async () => {
    const room = newRoomId();
    const a = await connect(room);
    const b = await connect(room);
    const f1 = frame(100, 0xaa);
    const f2 = frame(MAX_FRAME_BYTES, 0xbb);
    a.ws.send(f1);
    b.ws.send(f2);
    await until(() => a.received.length === 1 && b.received.length === 1);
    expect(bytes(b.received[0])).toEqual(bytes(f1));
    expect(bytes(a.received[0])).toEqual(bytes(f2));
    await tick();
    expect(a.received.length).toBe(1); // no echo to the sender
    expect(b.received.length).toBe(1);
  });

  it("forwards 0x00 pair.hello frames byte for byte", async () => {
    const room = newRoomId();
    const a = await connect(room);
    const b = await connect(room);
    const json = new TextEncoder().encode('{"v":1,"type":"pair.hello","seq":0}');
    const hello = new Uint8Array(1 + json.length);
    hello[0] = 0x00;
    hello.set(json, 1);
    b.ws.send(hello.buffer);
    const max = frame(MAX_PAIR_HELLO_BYTES, 0x7b, 0x00);
    b.ws.send(max);
    await until(() => a.received.length === 2);
    expect(bytes(a.received[0])).toEqual(Array.from(hello));
    expect(bytes(a.received[1])).toEqual(bytes(max));
  });

  it("rejects a third client with 409", async () => {
    const room = newRoomId();
    await connect(room);
    await connect(room);
    const res = await SELF.fetch(`http://relay/r/${room}`, { headers: { Upgrade: "websocket" } });
    expect(res.status).toBe(409);
    expect(res.webSocket).toBeNull();
  });

  it("drops text, oversized, undersized and wrong-version frames", async () => {
    const room = newRoomId();
    const a = await connect(room);
    const b = await connect(room);
    a.ws.send("hello");
    a.ws.send(frame(MAX_FRAME_BYTES + 1, 0x11));
    a.ws.send(frame(40, 0x22));
    a.ws.send(frame(100, 0x33, 0x02));
    a.ws.send(frame(100, 0x44, 0xff));
    a.ws.send(frame(MAX_PAIR_HELLO_BYTES + 1, 0x45, 0x00)); // pair.hello over 2 KB
    a.ws.send(frame(1, 0x46, 0x00)); // pair.hello with no body
    const ok = frame(64, 0x55);
    a.ws.send(ok); // marker: ordering on one socket means everything before it was processed
    await until(() => b.received.length >= 1);
    await tick();
    expect(b.received.map(bytes)).toEqual([bytes(ok)]);
    expect(b.texts).toEqual([]);
  });

  it("lets a dropped side reconnect and resume relaying", async () => {
    const room = newRoomId();
    const a = await connect(room);
    const b = await connect(room);
    a.ws.close(1000, "leaving");
    // The slot frees once the relay has processed the close; until then a new client gets 409.
    let c: Client | null = null;
    for (let i = 0; i < 200 && !c; i++) {
      c = await tryConnect(room);
      if (!c) await tick(10);
    }
    if (!c) throw new Error("never reconnected");
    const f = frame(80, 0x66);
    b.ws.send(f);
    await until(() => c!.received.length === 1);
    expect(bytes(c.received[0])).toEqual(bytes(f));
    const g = frame(90, 0x77);
    c.ws.send(g);
    await until(() => b.received.length === 1);
    expect(bytes(b.received[0])).toEqual(bytes(g));
    expect(a.received.length).toBe(0);
  });
});

describe("rate limit", () => {
  it("drops frames beyond the room's burst", async () => {
    const room = newRoomId();
    const a = await connect(room);
    const b = await connect(room);
    // vitest.config.ts sets burst 20 frames, refill 1 frame/s for tests.
    const sent = 60;
    for (let i = 0; i < sent; i++) a.ws.send(frame(64, 1));
    const marker = frame(64, 2);
    await tick(1500);
    a.ws.send(marker); // the bucket has refilled at least one token by now
    await until(() => b.received.some((f) => new Uint8Array(f)[1] === 2));
    const relayed = b.received.length - 1;
    expect(relayed).toBeGreaterThanOrEqual(20);
    expect(relayed).toBeLessThan(30); // would need ~10 s of refill to reach 30
  });
});

describe("privacy", () => {
  it("writes nothing to Durable Object storage while relaying", async () => {
    const room = newRoomId();
    const a = await connect(room);
    const b = await connect(room);
    for (let i = 0; i < 5; i++) a.ws.send(frame(200, i + 1));
    b.ws.send(frame(200, 0x99));
    await until(() => b.received.length === 5 && a.received.length === 1);
    expect(await storageKeys(room)).toEqual([]);
    // Only the expiry alarm is scheduled.
    const alarm = await runInDurableObject(stub(room), (_i: RelayRoom, state) => state.storage.getAlarm());
    expect(alarm).not.toBeNull();
  });
});

describe("expiry", () => {
  it("keeps an active room open and re-arms the alarm", async () => {
    const room = newRoomId();
    const a = await connect(room);
    const b = await connect(room);
    a.ws.send(frame(64, 1));
    await until(() => b.received.length === 1);
    expect(await runDurableObjectAlarm(stub(room))).toBe(true);
    const alarm = await runInDurableObject(stub(room), (_i: RelayRoom, state) => state.storage.getAlarm());
    expect(alarm).not.toBeNull();
    b.ws.send(frame(64, 2));
    await until(() => a.received.length === 1);
  });

  it("closes both sockets after 24 h idle and leaves no storage", async () => {
    const room = newRoomId();
    const a = await connect(room);
    const b = await connect(room);
    await runInDurableObject(stub(room), (instance: RelayRoom) => {
      const skew = 25 * 60 * 60 * 1000;
      instance.now = () => Date.now() + skew;
    });
    expect(await runDurableObjectAlarm(stub(room))).toBe(true);
    const [ca, cb] = await Promise.all([a.closed, b.closed]);
    expect(ca.code).toBe(1001);
    expect(cb.code).toBe(1001);
    expect(await storageKeys(room)).toEqual([]);
    const alarm = await runInDurableObject(stub(room), (_i: RelayRoom, state) => state.storage.getAlarm());
    expect(alarm).toBeNull();
  });
});
