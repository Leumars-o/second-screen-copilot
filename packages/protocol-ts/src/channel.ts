// Encrypted channel: assigns seq/ts on send, verifies + validates + replay-checks on receive.
import { decodeEnvelope, encodeEnvelope, type AnyEnvelope, type Outgoing } from "./envelope";
import { openFrame, sealFrame } from "./crypto";

/** Accepts a frame only if its seq is strictly greater than the last accepted one. */
export class ReplayWindow {
  constructor(private last: number = -1) {}
  get lastAccepted(): number {
    return this.last;
  }
  /** True (and records it) if `seq` is fresh. */
  accept(seq: number): boolean {
    if (!Number.isSafeInteger(seq) || seq <= this.last) return false;
    this.last = seq;
    return true;
  }
}

export interface ChannelState {
  /** Next seq this side sends. */
  nextSendSeq: number;
  /** Last seq accepted from the peer (-1 = none yet). */
  lastRecvSeq: number;
}

export type DropReason = "crypto" | "schema" | "replay" | "mismatch";

export interface ChannelOptions {
  sendKey: Uint8Array;
  recvKey: Uint8Array;
  state?: ChannelState;
  now?: () => number;
  /** Called after every state change, so callers can persist seq across reconnects (README § Frames, seq scope). */
  onState?: (s: ChannelState) => void;
}

export class SecureChannel {
  private nextSeq: number;
  private readonly replay: ReplayWindow;
  private readonly now: () => number;
  readonly dropped: Record<DropReason, number> = { crypto: 0, schema: 0, replay: 0, mismatch: 0 };

  constructor(private readonly opts: ChannelOptions) {
    this.nextSeq = opts.state?.nextSendSeq ?? 1;
    this.replay = new ReplayWindow(opts.state?.lastRecvSeq ?? -1);
    this.now = opts.now ?? Date.now;
  }

  get state(): ChannelState {
    return { nextSendSeq: this.nextSeq, lastRecvSeq: this.replay.lastAccepted };
  }

  /** Build, validate and encrypt the next frame. */
  async seal(msg: Outgoing, sessionId: string | null, nonce?: Uint8Array): Promise<Uint8Array> {
    const seq = this.nextSeq;
    // Validate before reserving the seq, so a rejected message does not leave a gap.
    const json = encodeEnvelope({ v: 1, type: msg.type, seq, ts: this.now(), session_id: sessionId, payload: msg.payload } as AnyEnvelope);
    // Reserve synchronously: concurrent seal() calls never share a seq. Callers that need wire
    // order == seq order must serialise their sends (PhoneLink and MockPc do).
    this.nextSeq = seq + 1;
    this.opts.onState?.(this.state);
    return sealFrame(this.opts.sendKey, new TextEncoder().encode(json), sessionId, seq, nonce);
  }

  /** Returns the envelope, or null if the frame is dropped (counted in `dropped`). */
  async open(wire: Uint8Array): Promise<AnyEnvelope | null> {
    const opened = await openFrame(this.opts.recvKey, wire);
    if (!opened) return this.drop("crypto");
    let env: AnyEnvelope;
    try {
      env = decodeEnvelope(opened.plaintext);
    } catch {
      return this.drop("schema");
    }
    if (env.seq !== opened.seq || env.session_id !== opened.sessionId) return this.drop("mismatch");
    if (!this.replay.accept(env.seq)) return this.drop("replay");
    this.opts.onState?.(this.state);
    return env;
  }

  private drop(r: DropReason): null {
    this.dropped[r]++;
    return null;
  }
}
