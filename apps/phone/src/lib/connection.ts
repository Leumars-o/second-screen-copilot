// Pairing persistence + the live link. localStorage holds the direction keys (not the phone
// private key, which is discarded after deriving them) and the seq state, scoped to the pairing
// (contracts/crypto/README.md § Frames, seq scope).
import { parseQr, PhoneLink, startPairing, type ChannelState, type LinkStatus, type Pairing, type WsLike } from "@ssc/protocol";
import type { Copilot } from "./copilot";

const PAIRING_KEY = "ssc.pairing.v1";
const LINK_KEY = "ssc.link.v1";
/** Seq persistence is batched, so a crash can lose the last few sends. Jump ahead on restart
 * (README: "persist a reservation and jump to it"); gaps are fine, going backwards is not. */
export const SEQ_RESERVATION = 1000;

/** Saved link state as restored on startup: next-send seq jumps past anything possibly sent. */
export function restoreLinkState(saved: { state: ChannelState; sessionId: string | null } | null) {
  if (!saved) return null;
  return { ...saved, state: { ...saved.state, nextSendSeq: saved.state.nextSendSeq + SEQ_RESERVATION } };
}

interface StoredPairing {
  pairing: Pairing;
  /** Present until the plaintext pair.hello has been sent (it is sent exactly once). */
  hello?: string;
  confirmed: boolean;
}
interface StoredLink {
  state: ChannelState;
  sessionId: string | null;
}

function read<T>(key: string): T | null {
  try {
    const v = localStorage.getItem(key);
    return v ? (JSON.parse(v) as T) : null;
  } catch {
    return null;
  }
}
function write(key: string, v: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(v));
  } catch {
    /* storage full or blocked: the link still works for this page load */
  }
}

export function loadPairing(): StoredPairing | null {
  return read<StoredPairing>(PAIRING_KEY);
}

export function forgetPairing(): void {
  localStorage.removeItem(PAIRING_KEY);
  localStorage.removeItem(LINK_KEY);
}

/** Validate the QR text, derive keys, store the pending pairing (hello not yet sent). */
export async function pairFromQr(qrText: string, deviceName: string): Promise<StoredPairing> {
  const qr = parseQr(qrText);
  const start = await startPairing(qr, deviceName);
  const stored: StoredPairing = { pairing: start.pairing, hello: start.hello, confirmed: false };
  write(PAIRING_KEY, stored);
  localStorage.removeItem(LINK_KEY); // fresh keys -> fresh seq space
  return stored;
}

export function defaultDeviceName(): string {
  if (typeof navigator === "undefined") return "Phone";
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return "iPhone";
  if (/iPad/.test(ua)) return "iPad";
  const m = /Android [\d.]+; ([^;)]+)/.exec(ua);
  if (m?.[1]) return m[1].trim().slice(0, 64);
  if (/Android/.test(ua)) return "Android phone";
  return "Phone";
}

export interface LinkHandle {
  link: PhoneLink;
  stop(): void;
}

export function connect(
  stored: StoredPairing,
  copilot: Copilot,
  hooks: { onStatus: (s: LinkStatus) => void; onPaired: () => void },
  ws: (url: string) => WsLike = (url) => new WebSocket(url) as unknown as WsLike,
): LinkHandle {
  const prev = restoreLinkState(read<StoredLink>(LINK_KEY));
  let pending: StoredLink | null = null;
  let flushScheduled = false;
  // Persist at most once per macrotask: deltas arrive in bursts.
  const flush = () => {
    flushScheduled = false;
    if (pending) write(LINK_KEY, pending);
  };
  const link = new PhoneLink({
    pairing: stored.pairing,
    ...(stored.hello ? { hello: stored.hello } : {}),
    ...(prev ? { state: prev.state, sessionId: prev.sessionId } : {}),
    ws,
    onEnvelope: (env) => {
      // Any authentic frame from the PC proves it accepted our pair.hello (even across a reload).
      if (!stored.confirmed) {
        stored.confirmed = true;
        write(PAIRING_KEY, stored);
        hooks.onPaired();
      }
      copilot.handle(env);
    },
    onStatus: hooks.onStatus,
    onPersist: ({ state, sessionId, helloSent }) => {
      pending = { state, sessionId };
      if (helloSent && stored.hello) {
        delete stored.hello;
        write(PAIRING_KEY, stored);
        write(LINK_KEY, pending); // the hello must never be re-sent: persist now, not later
      }
      if (!flushScheduled) {
        flushScheduled = true;
        setTimeout(flush, 0);
      }
    },
  });
  link.start();
  return {
    link,
    stop: () => {
      link.stop();
      flush();
    },
  };
}
