// Crypto per contracts/crypto/README.md. X25519 + XChaCha20-Poly1305 from libsodium,
// HKDF-SHA256 and HMAC-SHA256 from WebCrypto.
import sodium from "libsodium-wrappers";

export const FRAME_VERSION = 0x01;
export const NONCE_BYTES = 24;
export const TAG_BYTES = 16;
export const INFO_PC_TO_PHONE = "ssc/v1/pc->phone";
export const INFO_PHONE_TO_PC = "ssc/v1/phone->pc";
export const INFO_ROOM = "ssc/v1/room";

const enc = new TextEncoder();

export async function ready(): Promise<typeof sodium> {
  await sodium.ready;
  return sodium;
}

function subtle(): SubtleCrypto {
  const s = globalThis.crypto?.subtle;
  if (!s) throw new Error("WebCrypto unavailable (needs a secure context: https or localhost)");
  return s;
}

/** Copy into a fresh ArrayBuffer-backed view (WebCrypto's BufferSource typing). */
const ab = (b: Uint8Array): Uint8Array<ArrayBuffer> => new Uint8Array(b);

// ---- encoding helpers (standard base64 with padding, lowercase hex) ----

export function toB64(b: Uint8Array): string {
  let s = "";
  for (const x of b) s += String.fromCharCode(x);
  return btoa(s);
}

export function fromB64(s: string): Uint8Array {
  if (!/^[A-Za-z0-9+/]*={0,2}$/.test(s) || s.length % 4 !== 0) throw new Error("invalid base64");
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function toHex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

export function fromHex(s: string): Uint8Array {
  if (!/^(?:[0-9a-fA-F]{2})*$/.test(s)) throw new Error("invalid hex");
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(i * 2, i * 2 + 2), 16);
  return out;
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

// ---- primitives ----

export interface KeyPair {
  publicKey: Uint8Array;
  privateKey: Uint8Array;
}

export async function generateKeyPair(): Promise<KeyPair> {
  const na = await ready();
  const privateKey = na.randombytes_buf(32);
  return { privateKey, publicKey: na.crypto_scalarmult_base(privateKey) };
}

export async function publicKeyFor(privateKey: Uint8Array): Promise<Uint8Array> {
  return (await ready()).crypto_scalarmult_base(privateKey);
}

/** X25519. Rejects an all-zero shared secret (low-order peer point). */
export async function sharedSecret(ownPriv: Uint8Array, peerPub: Uint8Array): Promise<Uint8Array> {
  const na = await ready();
  if (peerPub.length !== 32 || ownPriv.length !== 32) throw new Error("X25519 keys must be 32 bytes");
  let shared: Uint8Array;
  try {
    shared = na.crypto_scalarmult(ownPriv, peerPub);
  } catch {
    throw new Error("X25519 failed (low-order point)");
  }
  if (shared.every((x) => x === 0)) throw new Error("all-zero shared secret");
  return shared;
}

export async function hkdfSha256(ikm: Uint8Array, salt: Uint8Array, info: string, length: number): Promise<Uint8Array> {
  const s = subtle();
  const key = await s.importKey("raw", ab(ikm), "HKDF", false, ["deriveBits"]);
  const bits = await s.deriveBits({ name: "HKDF", hash: "SHA-256", salt: ab(salt), info: ab(enc.encode(info)) }, key, length * 8);
  return new Uint8Array(bits);
}

export async function hmacSha256(key: Uint8Array, msg: Uint8Array): Promise<Uint8Array> {
  const s = subtle();
  const k = await s.importKey("raw", ab(key), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  return new Uint8Array(await s.sign("HMAC", k, ab(msg)));
}

export interface DirectionKeys {
  pcToPhone: Uint8Array;
  phoneToPc: Uint8Array;
}

export async function deriveKeys(shared: Uint8Array, token: Uint8Array): Promise<DirectionKeys> {
  return {
    pcToPhone: await hkdfSha256(shared, token, INFO_PC_TO_PHONE, 32),
    phoneToPc: await hkdfSha256(shared, token, INFO_PHONE_TO_PC, 32),
  };
}

export async function roomId(token: Uint8Array): Promise<string> {
  return toHex(await hkdfSha256(token, new Uint8Array(0), INFO_ROOM, 16));
}

/** pair.hello proof = HMAC-SHA256(key = token, msg = raw phone_pub). */
export async function pairingProof(token: Uint8Array, phonePub: Uint8Array): Promise<Uint8Array> {
  return hmacSha256(token, phonePub);
}

// ---- frames ----

/** aad = UTF-8(session_id or "") || seq as u64 big-endian. */
export function frameAad(sessionId: string | null, seq: number): Uint8Array {
  if (!Number.isSafeInteger(seq) || seq < 0) throw new Error("seq must be a non-negative safe integer");
  const s = enc.encode(sessionId ?? "");
  const out = new Uint8Array(s.length + 8);
  out.set(s, 0);
  new DataView(out.buffer).setBigUint64(s.length, BigInt(seq), false);
  return out;
}

/** wire = 0x01 || nonce(24) || XChaCha20-Poly1305(key, nonce, plaintext, aad). `nonce` is random unless given (tests). */
export async function sealFrame(
  key: Uint8Array,
  plaintext: Uint8Array,
  sessionId: string | null,
  seq: number,
  nonce?: Uint8Array,
): Promise<Uint8Array> {
  const na = await ready();
  const n = nonce ?? na.randombytes_buf(NONCE_BYTES);
  if (n.length !== NONCE_BYTES) throw new Error("nonce must be 24 bytes");
  const ct = na.crypto_aead_xchacha20poly1305_ietf_encrypt(plaintext, frameAad(sessionId, seq), null, n, key);
  return concat(Uint8Array.of(FRAME_VERSION), n, ct);
}

/** Decrypt with a known AAD. Returns null on any failure (wrong version, tag mismatch, short frame). */
export async function openFrameWithAad(key: Uint8Array, wire: Uint8Array, aad: Uint8Array): Promise<Uint8Array | null> {
  const na = await ready();
  if (wire.length < 1 + NONCE_BYTES + TAG_BYTES || wire[0] !== FRAME_VERSION) return null;
  try {
    return na.crypto_aead_xchacha20poly1305_ietf_decrypt(null, wire.subarray(1 + NONCE_BYTES), aad, wire.subarray(1, 1 + NONCE_BYTES), key);
  } catch {
    return null;
  }
}

/**
 * Normative receive algorithm (contracts/crypto/README.md § Frames, "Receive algorithm").
 * `session_id` and `seq` are not in the clear, so: recover the *unauthenticated* candidate
 * plaintext with the XChaCha20 keystream from block 1 (the AEAD's ciphertext is plaintext XOR
 * that keystream, so sealing the ciphertext body with the same key/nonce yields the plaintext; the
 * output is never sent), read only `session_id` and `seq` from it, then run the full AEAD open
 * with that AAD. Nothing from the candidate is trusted unless the tag verifies.
 */
export async function openFrame(
  key: Uint8Array,
  wire: Uint8Array,
): Promise<{ plaintext: Uint8Array; sessionId: string | null; seq: number } | null> {
  const na = await ready();
  if (wire.length < 1 + NONCE_BYTES + TAG_BYTES || wire[0] !== FRAME_VERSION) return null; // >= 41 bytes, 0x01
  const nonce = wire.subarray(1, 1 + NONCE_BYTES);
  const body = wire.subarray(1 + NONCE_BYTES, wire.length - TAG_BYTES);
  const candidate = na.crypto_aead_xchacha20poly1305_ietf_encrypt(body, null, null, nonce, key).subarray(0, body.length);
  let sessionId: string | null;
  let seq: number;
  try {
    const v = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(candidate)) as Record<string, unknown>;
    if (typeof v.seq !== "number" || !Number.isSafeInteger(v.seq) || v.seq < 0) return null;
    if (v.session_id !== null && typeof v.session_id !== "string") return null;
    seq = v.seq;
    sessionId = v.session_id;
  } catch {
    return null;
  }
  const plaintext = await openFrameWithAad(key, wire, frameAad(sessionId, seq));
  return plaintext ? { plaintext, sessionId, seq } : null;
}
