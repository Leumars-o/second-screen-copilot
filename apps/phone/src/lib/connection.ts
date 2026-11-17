// Pairing persistence + the live link. The pairing (direction keys; the phone private key is
// discarded after deriving them) is sealed in IndexedDB under a non-extractable AES-GCM key
// (./keystore, CR-15). localStorage holds only the non-secret seq state, scoped to the pairing
// (contracts/crypto/README.md § Frames, seq scope).
import { parseQr, PhoneLink, startPairing, type ChannelState, type LinkStatus, type Pairing, type WsLike } from "@ssc/protocol";
import type { Copilot } from "./copilot";
import { deleteSealedPairing, loadSealedPairing, saveSealedPairing } from "./keystore";

const LINK_KEY = "ssc.link.v1";
/** Seq persistence is batched, so a crash can lose the last few sends. Jump ahead on restart
 * (README: "persist a reservation and jump to it"); gaps are fine, going backwards is not. */
export const SEQ_RESERVATION = 1000;

/** Saved link state as restored on startup: next-send seq jumps past anything possibly sent. */
export function restoreLinkState(saved: StoredLink | null) {
  if (!saved) return null;
  return { ...saved, state: { ...saved.state, nextSendSeq: saved.state.nextSendSeq + SEQ_RESERVATION } };
}

export interface StoredPairing {
  pairing: Pairing;
  /** Present until the PC has confirmed the pairing (the link resends it on each new connection
   * until then, at most once per connection). */
  hello?: string;
  confirmed: boolean;
}
interface StoredLink {
  state: ChannelState;
  sessionId: string | null;
  /** Written synchronously when the PC confirms pairing, so a reload never re-sends the hello
   * even if the async IndexedDB update that drops `hello` from the pairing did not land. */
  helloConfirmed?: boolean;
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

function savePairing(p: StoredPairing): void {
  // Storage failure (IndexedDB blocked) only costs persistence: the link works for this page load.
  saveSealedPairing(p).catch(() => undefined);
}

/** Load the stored pairing (migrating a legacy localStorage entry once). null = not paired. */
export async function loadPairing(): Promise<StoredPairing | null> {
  let p: StoredPairing | null;
  try {
    p = await loadSealedPairing<StoredPairing>();
  } catch {
    return null;
  }
  if (p?.hello && (p.confirmed || read<StoredLink>(LINK_KEY)?.helloConfirmed)) {
    delete p.hello;
    savePairing(p);
  }
  return p;
}

export async function forgetPairing(): Promise<void> {
  localStorage.removeItem(LINK_KEY);
  await deleteSealedPairing().catch(() => undefined);
}

/** Validate the QR text, derive keys, store the pending pairing (hello not yet sent). */
export async function pairFromQr(qrText: string, deviceName: string): Promise<StoredPairing> {
  const qr = parseQr(qrText);
  const start = await startPairing(qr, deviceName);
  const stored: StoredPairing = { pairing: start.pairing, hello: start.hello, confirmed: false };
  localStorage.removeItem(LINK_KEY); // fresh keys -> fresh seq space
  await saveSealedPairing(stored).catch(() => undefined);
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
        savePairing(stored);
        hooks.onPaired();
      }
      copilot.handle(env);
    },
    onStatus: hooks.onStatus,
    onPersist: ({ state, sessionId, helloConfirmed }) => {
      pending = { state, sessionId, helloConfirmed };
      if (helloConfirmed && stored.hello) {
        delete stored.hello;
        write(LINK_KEY, pending); // confirmed: the hello is no longer needed; persist now, not later
        savePairing(stored);
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
