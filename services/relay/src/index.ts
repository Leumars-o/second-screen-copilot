// Blind relay: one Durable Object per room, exactly two WebSockets per room,
// binary frames forwarded byte-for-byte to the other socket.
//
// Privacy rules (Build Sheet W1-RELAY, product spec "Privacy, security and boundaries"):
// - never log payloads (this file logs nothing at all);
// - never write content to Durable Object storage: the only persisted state is the
//   expiry alarm; activity time lives in memory and in the socket attachment.
import { DurableObject } from "cloudflare:workers";
import { checkFrame, limitsFromEnv, ROOM_PATH, TokenBucket, type Limits } from "./frame";

export interface Env {
  ROOMS: DurableObjectNamespace<RelayRoom>;
  RATE_FRAMES_PER_SEC?: string;
  RATE_BURST_FRAMES?: string;
  RATE_BYTES_PER_SEC?: string;
  RATE_BURST_BYTES?: string;
  ROOM_IDLE_MS?: string;
}

export const MAX_PEERS = 2;
/** How often (at most) the activity timestamp is copied into a socket attachment. */
const ATTACHMENT_REFRESH_MS = 60_000;

interface Attachment {
  lastSeen: number;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (!url.pathname.startsWith("/r/")) return new Response("not found", { status: 404 });
    const m = ROOM_PATH.exec(url.pathname);
    if (!m || !m[1]) return new Response("bad room id", { status: 400 });
    if (request.method !== "GET") return new Response("method not allowed", { status: 405 });
    if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
      return new Response("expected websocket", { status: 426 });
    }
    const stub = env.ROOMS.get(env.ROOMS.idFromName(m[1]));
    return stub.fetch(request);
  },
} satisfies ExportedHandler<Env>;

export class RelayRoom extends DurableObject<Env> {
  private readonly limits: Limits;
  private frames: TokenBucket;
  private bytes: TokenBucket;
  /** In-memory only. Lost on hibernation; socket attachments are the fallback. */
  lastActivity: number;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.limits = limitsFromEnv(env as unknown as Record<string, unknown>);
    const now = this.now();
    this.frames = new TokenBucket(this.limits.framesPerSec, this.limits.burstFrames, now);
    this.bytes = new TokenBucket(this.limits.bytesPerSec, this.limits.burstBytes, now);
    this.lastActivity = 0;
  }

  /** Overridable clock (tests move it forward to exercise expiry). */
  now(): number {
    return Date.now();
  }

  private openSockets(): WebSocket[] {
    return this.ctx.getWebSockets().filter((ws) => ws.readyState === WebSocket.OPEN);
  }

  override async fetch(_request: Request): Promise<Response> {
    if (this.openSockets().length >= MAX_PEERS) {
      return new Response("room full", { status: 409 });
    }
    const pair = new WebSocketPair();
    const now = this.now();
    this.ctx.acceptWebSocket(pair[1]);
    pair[1].serializeAttachment({ lastSeen: now } satisfies Attachment);
    await this.touch(now, true);
    return new Response(null, { status: 101, webSocket: pair[0] });
  }

  override async webSocketMessage(ws: WebSocket, msg: string | ArrayBuffer): Promise<void> {
    if (checkFrame(msg) !== null) return; // dropped silently: text, oversized, undersized, wrong version
    const frame = msg as ArrayBuffer;
    const now = this.now();
    if (!this.frames.take(1, now) || !this.bytes.take(frame.byteLength, now)) return; // rate limited
    // Forward before any await so frame order on the wire matches arrival order.
    for (const peer of this.openSockets()) {
      if (peer !== ws) peer.send(frame);
    }
    await this.touch(now, false, ws);
  }

  override async webSocketClose(ws: WebSocket, code: number, _reason: string, _wasClean: boolean): Promise<void> {
    // Complete the close handshake. 1005/1006 are reserved and can't be sent back.
    const safe = code === 1005 || code === 1006 ? 1000 : code;
    try {
      ws.close(safe, "bye");
    } catch {
      // already closed
    }
    await this.cleanupIfEmpty(ws);
  }

  override async webSocketError(ws: WebSocket, _error: unknown): Promise<void> {
    await this.cleanupIfEmpty(ws);
  }

  override async alarm(): Promise<void> {
    const sockets = this.ctx.getWebSockets();
    let last = this.lastActivity;
    for (const ws of sockets) {
      const a = ws.deserializeAttachment() as Attachment | null;
      if (a && a.lastSeen > last) last = a.lastSeen;
    }
    const now = this.now();
    if (sockets.length > 0 && now - last < this.limits.idleMs) {
      await this.ctx.storage.setAlarm(last + this.limits.idleMs);
      return;
    }
    // Idle for the full window (or nobody left): expire the room.
    for (const ws of sockets) {
      try {
        ws.close(1001, "room expired");
      } catch {
        // already closed
      }
    }
    await this.ctx.storage.deleteAlarm();
    await this.ctx.storage.deleteAll();
  }

  /** Records activity and keeps the expiry alarm ahead of it without a storage write per frame. */
  private async touch(now: number, force: boolean, ws?: WebSocket): Promise<void> {
    const stale = now - this.lastActivity >= ATTACHMENT_REFRESH_MS;
    this.lastActivity = now;
    if (ws && stale) ws.serializeAttachment({ lastSeen: now } satisfies Attachment);
    if (force || stale) {
      // The alarm may fire early relative to later activity; alarm() then re-arms it.
      const current = await this.ctx.storage.getAlarm();
      if (current === null || force) await this.ctx.storage.setAlarm(now + this.limits.idleMs);
    }
  }

  private async cleanupIfEmpty(closing: WebSocket): Promise<void> {
    const remaining = this.openSockets().filter((s) => s !== closing);
    if (remaining.length === 0) {
      await this.ctx.storage.deleteAlarm();
      await this.ctx.storage.deleteAll();
    }
  }
}
