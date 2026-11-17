// Pairing: QR parse -> phone keypair -> plaintext pair.hello with proof -> direction keys.
import type { QrPayload } from "./generated/types";
import { decodeEnvelope, encodeEnvelope, type Envelope } from "./envelope";
import { validateQr } from "./validate";
import {
  deriveKeys,
  fromB64,
  generateKeyPair,
  pairingProof,
  roomId,
  sharedSecret,
  toB64,
  type DirectionKeys,
  type KeyPair,
} from "./crypto";

export class PairingError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "PairingError";
  }
}

/** Parse and validate the QR text. Rejects expired codes (`exp` is unix seconds). */
export function parseQr(text: string, nowMs: number = Date.now()): QrPayload {
  let value: unknown;
  try {
    value = JSON.parse(text.trim());
  } catch {
    throw new PairingError("QR code is not pairing JSON");
  }
  try {
    validateQr(value);
  } catch (e) {
    throw new PairingError(`QR code rejected: ${(e as Error).message}`);
  }
  if (value.exp * 1000 <= nowMs) throw new PairingError("QR code expired; show a new one on the PC");
  return value;
}

/** What the phone keeps after pairing. Keys only; the phone private key is not needed again. */
export interface Pairing {
  v: 1;
  pc_pub: string;
  urls: string[];
  room_id: string;
  /** b64 direction keys. */
  k_pc_to_phone: string;
  k_phone_to_pc: string;
  created_at: number;
}

export interface PairStart {
  pairing: Pairing;
  /** JSON of the plaintext pair.hello envelope (seq 0, session_id null). Send once with `helloFrame` (README § Pairing). */
  hello: string;
  keys: DirectionKeys;
}

export const HELLO_FRAME_VERSION = 0x00;

/** Wire form of pair.hello (contracts/crypto/README.md § Pairing): binary `0x00 || envelope JSON (UTF-8)`. */
export function helloFrame(helloJson: string): Uint8Array {
  const body = new TextEncoder().encode(helloJson);
  const out = new Uint8Array(1 + body.length);
  out[0] = HELLO_FRAME_VERSION;
  out.set(body, 1);
  return out;
}

/** PC side: parse and validate a `0x00` frame. Throws on a wrong version byte, bad JSON or a non-pair.hello envelope. */
export function parseHelloFrame(wire: Uint8Array): Envelope<"pair.hello"> {
  if (wire.length < 2 || wire[0] !== HELLO_FRAME_VERSION) throw new PairingError("not a pair.hello frame");
  const env = decodeEnvelope(wire.subarray(1));
  if (env.type !== "pair.hello") throw new PairingError("expected pair.hello");
  return env;
}

export async function startPairing(
  qr: QrPayload,
  deviceName: string,
  opts: { keyPair?: KeyPair; nowMs?: number } = {},
): Promise<PairStart> {
  const nowMs = opts.nowMs ?? Date.now();
  const kp = opts.keyPair ?? (await generateKeyPair());
  const token = fromB64(qr.token);
  const pcPub = fromB64(qr.pc_pub);
  const keys = await deriveKeys(await sharedSecret(kp.privateKey, pcPub), token);
  const proof = await pairingProof(token, kp.publicKey);
  const env: Envelope<"pair.hello"> = {
    v: 1,
    type: "pair.hello",
    seq: 0,
    ts: nowMs,
    session_id: null,
    payload: { device_name: Array.from(deviceName.trim()).slice(0, 64).join("") || "Phone", phone_pub: toB64(kp.publicKey), proof: toB64(proof) },
  };
  return {
    hello: encodeEnvelope(env),
    keys,
    pairing: {
      v: 1,
      pc_pub: qr.pc_pub,
      urls: [...qr.urls],
      room_id: await roomId(token),
      k_pc_to_phone: toB64(keys.pcToPhone),
      k_phone_to_pc: toB64(keys.phoneToPc),
      created_at: nowMs,
    },
  };
}

export function pairingKeys(p: Pairing): DirectionKeys {
  return { pcToPhone: fromB64(p.k_pc_to_phone), phoneToPc: fromB64(p.k_phone_to_pc) };
}

function constantTimeEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  let d = 0;
  for (let i = 0; i < a.length; i++) d |= a[i]! ^ b[i]!;
  return d === 0;
}

/**
 * PC side of pairing (used by the mock server and tests): check expiry and proof, derive keys.
 * The real PC implementation lives in ssc-crypto/ssc-transport (W1-PIPE).
 */
export async function verifyPairHello(
  hello: Envelope<"pair.hello">,
  pc: { privateKey: Uint8Array; token: Uint8Array; exp: number },
  nowMs: number = Date.now(),
): Promise<DirectionKeys> {
  if (pc.exp * 1000 <= nowMs) throw new PairingError("token expired");
  const phonePub = fromB64(hello.payload.phone_pub);
  const expected = await pairingProof(pc.token, phonePub);
  if (!constantTimeEqual(expected, fromB64(hello.payload.proof))) throw new PairingError("bad proof");
  return deriveKeys(await sharedSecret(pc.privateKey, phonePub), pc.token);
}
