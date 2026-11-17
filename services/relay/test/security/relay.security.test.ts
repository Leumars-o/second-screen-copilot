// W2-HARDEN security suite for the blind relay (docs/security/threat-model.md §3): the relay
// forwards opaque bytes unchanged, keeps no copy (storage, socket attachments), logs nothing,
// and only ever admits the two room members.
import { env, runInDurableObject, SELF } from "cloudflare:test";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { RelayRoom } from "../../src/index";

let n = 0;
const room = () => (0xf0000000 + ++n).toString(16).padStart(32, "f");
const open: WebSocket[] = [];

async function connect(id: string): Promise<{ ws: WebSocket; got: ArrayBuffer[] } | null> {
  const res = await SELF.fetch(`http://relay/r/${id}`, { headers: { Upgrade: "websocket" } });
  if (res.status !== 101) return null;
  const ws = res.webSocket!;
  const got: ArrayBuffer[] = [];
  ws.addEventListener("message", (e: MessageEvent) => {
    if (typeof e.data !== "string") got.push(e.data as ArrayBuffer);
  });
  ws.accept();
  open.push(ws);
  return { ws, got };
}

const tick = (ms = 30) => new Promise((r) => setTimeout(r, ms));
async function until(cond: () => boolean, ms = 2000): Promise<void> {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error("timed out");
    await tick(5);
  }
}

afterEach(() => {
  for (const ws of open.splice(0)) {
    try {
      ws.close(1000, "done");
    } catch {
      // closed
    }
  }
  vi.restoreAllMocks();
});

/** An opaque 0x01 frame whose "ciphertext" carries a recognisable marker. */
function sealedLike(marker: string): Uint8Array {
  const m = new TextEncoder().encode(marker);
  const f = new Uint8Array(1 + 24 + m.length + 16);
  f[0] = 0x01;
  f.set(m, 25);
  return f;
}

function has(hay: string, marker: string): boolean {
  return hay.includes(marker);
}

describe("blind relay", () => {
  it("forwards bytes unchanged, keeps no copy and logs nothing", async () => {
    const logs: unknown[] = [];
    for (const k of ["log", "info", "warn", "error", "debug"] as const) {
      vi.spyOn(console, k).mockImplementation((...a: unknown[]) => void logs.push(a));
    }
    const id = room();
    const a = (await connect(id))!;
    const b = (await connect(id))!;
    const marker = "MARKER-RELAY-MUST-NOT-KEEP";
    const frames = [sealedLike(marker + "-1"), sealedLike(marker + "-2"), sealedLike(marker + "-3")];
    for (const f of frames) a.ws.send(f);
    await until(() => b.got.length === frames.length);
    b.got.forEach((g, i) => expect(Array.from(new Uint8Array(g))).toEqual(Array.from(frames[i]!)));

    const kept = await runInDurableObject(env.ROOMS.get(env.ROOMS.idFromName(id)), async (_i: RelayRoom, state) => {
      const stored = [...(await state.storage.list()).entries()].map(([k, v]) => `${k}=${JSON.stringify(v)}`);
      const attachments = state.getWebSockets().map((ws) => JSON.stringify(ws.deserializeAttachment()));
      return { stored, attachments };
    });
    expect(kept.stored).toEqual([]);
    for (const att of kept.attachments) {
      expect(has(att, marker)).toBe(false);
      expect(Object.keys(JSON.parse(att) as object)).toEqual(["lastSeen"]);
    }
    expect(logs).toEqual([]);
  });

  it("admits only two sockets per room, so a late joiner cannot listen in", async () => {
    const id = room();
    const a = (await connect(id))!;
    const b = (await connect(id))!;
    expect(await connect(id)).toBeNull();
    a.ws.send(sealedLike("x"));
    await until(() => b.got.length === 1);
  });

  it("rooms are isolated: frames never cross to another room", async () => {
    const r1 = room();
    const r2 = room();
    const a1 = (await connect(r1))!;
    const b1 = (await connect(r1))!;
    const a2 = (await connect(r2))!;
    const b2 = (await connect(r2))!;
    a1.ws.send(sealedLike("room-one"));
    await until(() => b1.got.length === 1);
    await tick(50);
    expect(a2.got.length + b2.got.length).toBe(0);
  });

  it("refuses room ids that are not 128-bit lower-case hex (no path tricks)", async () => {
    for (const bad of ["..%2f..%2fx", "%2e%2e", "f".repeat(31) + "/", "F".repeat(32)]) {
      const res = await SELF.fetch(`http://relay/r/${bad}`, { headers: { Upgrade: "websocket" } });
      expect([400, 404], bad).toContain(res.status);
    }
  });
});
