// Phone-side link: tries each QR url in order (LAN first, then the relay), pairs once, keeps a
// heartbeat, reconnects when the link dies. Transport-agnostic: pass any WebSocket constructor.
import { SecureChannel, type ChannelState } from "./channel";
import type { AnyEnvelope, Outgoing } from "./envelope";
import { helloFrame, pairingKeys, type Pairing } from "./pairing";

export const HEARTBEAT_MS = 5_000;
export const DEAD_AFTER_MS = 15_000;
export const CONNECT_TIMEOUT_MS = 1_500;

/** Minimal WebSocket surface shared by the browser and the `ws` package. */
export interface WsLike {
  binaryType: string;
  readyState: number;
  onopen: ((ev: unknown) => void) | null;
  onclose: ((ev: unknown) => void) | null;
  onerror: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  send(data: string | Uint8Array): void;
  close(): void;
}
export type WsFactory = (url: string) => WsLike;

export type LinkStatus =
  | { state: "connecting"; url: string }
  | { state: "connected"; url: string; via: "lan" | "relay" }
  | { state: "offline"; retryInMs: number };

export interface LinkOptions {
  pairing: Pairing;
  /** Plaintext pair.hello to send once on the first connection (fresh pairing only). */
  hello?: string;
  state?: ChannelState;
  sessionId?: string | null;
  ws: WsFactory;
  onEnvelope: (env: AnyEnvelope) => void;
  onStatus?: (s: LinkStatus) => void;
  /** Persist seq state and the active session so a reload does not reset replay protection (README § Frames, seq scope). */
  onPersist?: (s: { state: ChannelState; sessionId: string | null; helloSent: boolean }) => void;
  /** Called once the PC has answered the pair.hello with a valid encrypted frame. */
  onPaired?: () => void;
  now?: () => number;
}

const OPEN = 1;

function toBytes(data: unknown): Uint8Array | null {
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  return null;
}

export class PhoneLink {
  private readonly channel: SecureChannel;
  private ws: WsLike | null = null;
  private url = "";
  private hello: string | undefined;
  private paired: boolean;
  private sessionIdValue: string | null;
  private lastPeerHeartbeatTs: number | undefined;
  private lastHeard = 0;
  private hbTimer: ReturnType<typeof setInterval> | undefined;
  private retryTimer: ReturnType<typeof setTimeout> | undefined;
  private attempt = 0;
  private stopped = false;
  private readonly now: () => number;
  /** Serialise async sends so seq order equals wire order. */
  private sendChain: Promise<unknown> = Promise.resolve();
  /** Serialise receives so frames are opened (and replay-checked) in arrival order. */
  private recvChain: Promise<unknown> = Promise.resolve();

  constructor(private readonly opts: LinkOptions) {
    this.now = opts.now ?? Date.now;
    this.hello = opts.hello;
    this.paired = !opts.hello;
    this.sessionIdValue = opts.sessionId ?? null;
    const keys = pairingKeys(opts.pairing);
    this.channel = new SecureChannel({
      sendKey: keys.phoneToPc,
      recvKey: keys.pcToPhone,
      ...(opts.state ? { state: opts.state } : {}),
      now: this.now,
      onState: () => this.persist(),
    });
  }

  get sessionId(): string | null {
    return this.sessionIdValue;
  }
  get dropped() {
    return this.channel.dropped;
  }
  get connected(): boolean {
    return this.ws?.readyState === OPEN;
  }

  start(): void {
    this.stopped = false;
    this.connect(0);
  }

  stop(): void {
    this.stopped = true;
    clearInterval(this.hbTimer);
    clearTimeout(this.retryTimer);
    const ws = this.ws;
    this.ws = null;
    ws?.close();
  }

  /** Encrypt and send. Resolves false when offline (the caller may retry). */
  send(msg: Outgoing): Promise<boolean> {
    const run = this.sendChain.then(async () => {
      const ws = this.ws;
      if (!ws || ws.readyState !== OPEN) return false;
      const wire = await this.channel.seal(msg, this.sessionIdValue);
      ws.send(wire);
      return true;
    });
    this.sendChain = run.catch(() => undefined);
    return run;
  }

  private persist(): void {
    this.opts.onPersist?.({ state: this.channel.state, sessionId: this.sessionIdValue, helloSent: this.hello === undefined });
  }

  private connect(index: number): void {
    if (this.stopped) return;
    const urls = this.opts.pairing.urls;
    const url = urls[index % urls.length]!;
    this.url = url;
    this.opts.onStatus?.({ state: "connecting", url });
    let ws: WsLike;
    try {
      ws = this.opts.ws(url);
    } catch {
      this.next(index);
      return;
    }
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    let opened = false;
    const timeout = setTimeout(() => {
      if (!opened) {
        ws.onclose = null;
        ws.close();
        this.next(index);
      }
    }, CONNECT_TIMEOUT_MS);

    ws.onopen = () => {
      opened = true;
      clearTimeout(timeout);
      this.attempt = 0;
      this.lastHeard = this.now();
      const via = /^wss?:\/\/[^/]+\/r\//.test(url) ? "relay" : "lan";
      this.opts.onStatus?.({ state: "connected", url, via });
      if (this.hello !== undefined) {
        ws.send(helloFrame(this.hello)); // the single plaintext frame: binary 0x00 || JSON (README § Pairing)
        this.hello = undefined;
        this.persist();
      }
      void this.send({ type: "heartbeat", payload: {} });
      clearInterval(this.hbTimer);
      this.hbTimer = setInterval(() => this.tick(), HEARTBEAT_MS / 5);
    };
    ws.onmessage = (ev) => {
      const bytes = toBytes(ev.data);
      if (!bytes) return; // the PC only sends binary 0x01 frames
      this.recvChain = this.recvChain.then(() => this.receive(bytes)).catch(() => undefined);
    };
    ws.onerror = () => undefined;
    ws.onclose = () => {
      clearTimeout(timeout);
      if (this.ws !== ws) return;
      this.ws = null;
      clearInterval(this.hbTimer);
      if (!opened) this.next(index);
      else this.retry();
    };
  }

  /** Try the next url immediately; after a full pass, back off. */
  private next(index: number): void {
    if (this.stopped) return;
    if ((index + 1) % this.opts.pairing.urls.length === 0) this.retry();
    else this.connect(index + 1);
  }

  private retry(): void {
    if (this.stopped) return;
    const delay = Math.min(10_000, 500 * 2 ** this.attempt++);
    this.opts.onStatus?.({ state: "offline", retryInMs: delay });
    clearTimeout(this.retryTimer);
    this.retryTimer = setTimeout(() => this.connect(0), delay); // always LAN first again
  }

  private lastSentHb = 0;
  private tick(): void {
    const now = this.now();
    if (now - this.lastHeard > DEAD_AFTER_MS) {
      const ws = this.ws;
      this.ws = null;
      clearInterval(this.hbTimer);
      ws?.close();
      this.retry();
      return;
    }
    if (now - this.lastSentHb >= HEARTBEAT_MS) {
      this.lastSentHb = now;
      const payload = this.lastPeerHeartbeatTs !== undefined ? { echo: this.lastPeerHeartbeatTs } : {};
      void this.send({ type: "heartbeat", payload });
    }
  }

  private async receive(wire: Uint8Array): Promise<void> {
    const env = await this.channel.open(wire);
    if (!env) return;
    this.lastHeard = this.now();
    if (!this.paired) {
      this.paired = true;
      this.opts.onPaired?.();
    }
    if (env.type === "heartbeat") this.lastPeerHeartbeatTs = env.ts;
    if (env.type === "session.start") this.sessionIdValue = env.payload.session_id;
    if (env.type === "session.end" && this.sessionIdValue === env.payload.session_id) this.sessionIdValue = null;
    if (env.type === "session.start" || env.type === "session.end") this.persist();
    this.opts.onEnvelope(env);
  }

  /** Url currently in use (for the status line). */
  get currentUrl(): string {
    return this.url;
  }
}
