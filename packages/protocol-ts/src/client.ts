// Phone-side link: tries each QR url in order (LAN first, then the relay), pairs once, keeps a
// heartbeat, reconnects when the link dies. Transport-agnostic: pass any WebSocket constructor.
import { SecureChannel, type ChannelState } from "./channel";
import type { AnyEnvelope, Outgoing } from "./envelope";
import { helloFrame, pairingKeys, type Pairing } from "./pairing";

export const HEARTBEAT_MS = 5_000;
export const DEAD_AFTER_MS = 15_000;
/** Time to get a socket open (DNS + TCP + TLS + upgrade). Phones on Wi-Fi measured over 1.5 s for
 * the relay (a cold room costs ~0.8 s from a desktop), which made every attempt time out. */
export const LAN_CONNECT_TIMEOUT_MS = 4_000;
export const RELAY_CONNECT_TIMEOUT_MS = 8_000;
/** @deprecated use LAN_/RELAY_CONNECT_TIMEOUT_MS */
export const CONNECT_TIMEOUT_MS = LAN_CONNECT_TIMEOUT_MS;
/** Connections on which an unconfirmed pair.hello is (re)sent before giving up (one per connection). */
export const MAX_HELLO_SENDS = 8;

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

/** Relay urls are `ws(s)://host/r/<room>`; anything else is the PC's LAN socket. */
export function urlVia(url: string): "lan" | "relay" {
  return /^wss?:\/\/[^/]+\/r\//.test(url) ? "relay" : "lan";
}

/** How one connection attempt ended (for the phone's "connection details"). */
export interface LinkAttempt {
  url: string;
  via: "lan" | "relay";
  /** open: socket opened; timeout: not open in time; failed: closed/refused before opening;
   * closed: an open socket was closed (by the PC, the relay or the network). */
  outcome: "open" | "timeout" | "failed" | "closed";
  ms: number;
  at: number;
}

export type LinkStatus =
  | { state: "connecting"; url: string }
  | { state: "connected"; url: string; via: "lan" | "relay" }
  | { state: "offline"; retryInMs: number };

export interface LinkOptions {
  pairing: Pairing;
  /** Plaintext pair.hello (fresh pairing only). Sent once per connection until the PC's first
   * authentic frame confirms pairing, on at most MAX_HELLO_SENDS connections; the PC ignores
   * repeats of the hello it already accepted. */
  hello?: string;
  state?: ChannelState;
  sessionId?: string | null;
  ws: WsFactory;
  onEnvelope: (env: AnyEnvelope) => void;
  onStatus?: (s: LinkStatus) => void;
  /** Persist seq state and the active session so a reload does not reset replay protection (README § Frames, seq scope).
   * `helloConfirmed`: the PC confirmed pairing, so the stored hello can be dropped. */
  onPersist?: (s: { state: ChannelState; sessionId: string | null; helloConfirmed: boolean }) => void;
  /** Called once the PC has answered the pair.hello with a valid encrypted frame. */
  onPaired?: () => void;
  /** Diagnostics: one call per connection attempt outcome. */
  onAttempt?: (a: LinkAttempt) => void;
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
  private helloSends = 0;
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
    this.opts.onPersist?.({ state: this.channel.state, sessionId: this.sessionIdValue, helloConfirmed: this.paired });
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
    const via = urlVia(url);
    const started = this.now();
    const report = (outcome: LinkAttempt["outcome"]) => {
      const at = this.now();
      this.opts.onAttempt?.({ url, via, outcome, ms: at - started, at });
    };
    const timeout = setTimeout(
      () => {
        if (!opened) {
          ws.onclose = null;
          ws.close();
          report("timeout");
          this.next(index);
        }
      },
      via === "relay" ? RELAY_CONNECT_TIMEOUT_MS : LAN_CONNECT_TIMEOUT_MS,
    );

    ws.onopen = () => {
      opened = true;
      clearTimeout(timeout);
      report("open");
      this.attempt = 0;
      this.lastHeard = this.now();
      this.opts.onStatus?.({ state: "connected", url, via });
      // The single plaintext frame: binary 0x00 || JSON (README § Pairing). Once per connection
      // until confirmed: a hello (or the PC's confirmation) lost with a dying socket is retried.
      if (!this.paired && this.hello !== undefined && this.helloSends < MAX_HELLO_SENDS) {
        this.helloSends++;
        ws.send(helloFrame(this.hello));
      }
      // A probe: the PC's echo of it confirms pairing and marks this path live on the PC.
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
      report(opened ? "closed" : "failed");
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
      // Always a probe (no echo). Echoing an earlier PC heartbeat here would hand the PC a stale
      // ts and inflate its RTT; PC probes are echoed at once in receive().
      void this.send({ type: "heartbeat", payload: {} });
    }
  }

  private async receive(wire: Uint8Array): Promise<void> {
    const env = await this.channel.open(wire);
    if (!env) return;
    this.lastHeard = this.now();
    if (!this.paired) {
      this.paired = true;
      this.hello = undefined;
      this.persist();
      this.opts.onPaired?.();
    }
    if (env.type === "heartbeat") {
      // Answer a probe (no echo) at once so the PC's RTT = now - echo is not inflated by our tick.
      // Never answer an echo, or the two sides would ping-pong.
      if (env.payload.echo === undefined) void this.send({ type: "heartbeat", payload: { echo: env.ts } });
    }
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
