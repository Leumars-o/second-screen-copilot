import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
  deriveKeys,
  frameAad,
  fromB64,
  fromHex,
  openFrame,
  openFrameWithAad,
  pairingProof,
  parseQr,
  publicKeyFor,
  ReplayWindow,
  roomId,
  sealFrame,
  SecureChannel,
  sharedSecret,
  startPairing,
  toB64,
  toHex,
  verifyPairHello,
  decodeEnvelope,
} from "../src/index";

interface Frame {
  key: "pc->phone" | "phone->pc";
  session_id: string;
  seq: number;
  nonce_hex: string;
  aad_hex: string;
  plaintext: string;
  wire_b64: string;
}
type VKey =
  | "pc_priv_hex" | "pc_pub_hex" | "pc_pub_b64" | "phone_priv_hex" | "phone_pub_hex" | "phone_pub_b64"
  | "token_hex" | "token_b64" | "shared_hex" | "key_pc_to_phone_hex" | "key_phone_to_pc_hex" | "room_id_hex" | "proof_b64";
const V = JSON.parse(readFileSync(resolve(__dirname, "../../../contracts/crypto/vectors.json"), "utf8")) as Record<VKey, string> & {
  frames: Frame[];
};

const pcPriv = fromHex(V.pc_priv_hex);
const phonePriv = fromHex(V.phone_priv_hex);
const token = fromHex(V.token_hex);
const enc = new TextEncoder();
const dec = new TextDecoder();
const keyFor = (f: Frame) => fromHex(f.key === "pc->phone" ? V.key_pc_to_phone_hex : V.key_phone_to_pc_hex);

describe("crypto vectors (byte for byte)", () => {
  it("public keys", async () => {
    expect(toHex(await publicKeyFor(pcPriv))).toBe(V.pc_pub_hex);
    expect(toHex(await publicKeyFor(phonePriv))).toBe(V.phone_pub_hex);
    expect(toB64(fromHex(V.pc_pub_hex))).toBe(V.pc_pub_b64);
    expect(toB64(fromHex(V.phone_pub_hex))).toBe(V.phone_pub_b64);
    expect(toB64(token)).toBe(V.token_b64);
  });

  it("shared secret from both sides", async () => {
    expect(toHex(await sharedSecret(pcPriv, fromHex(V.phone_pub_hex)))).toBe(V.shared_hex);
    expect(toHex(await sharedSecret(phonePriv, fromHex(V.pc_pub_hex)))).toBe(V.shared_hex);
  });

  it("direction keys, room id, proof", async () => {
    const k = await deriveKeys(fromHex(V.shared_hex), token);
    expect(toHex(k.pcToPhone)).toBe(V.key_pc_to_phone_hex);
    expect(toHex(k.phoneToPc)).toBe(V.key_phone_to_pc_hex);
    expect(await roomId(token)).toBe(V.room_id_hex);
    expect(toB64(await pairingProof(token, fromHex(V.phone_pub_hex)))).toBe(V.proof_b64);
  });

  it.each(V.frames)("frame $key seq $seq: aad, decrypt, re-seal reproduces wire", async (f) => {
    const key = keyFor(f);
    const wire = fromB64(f.wire_b64);
    expect(toHex(frameAad(f.session_id, f.seq))).toBe(f.aad_hex);
    const opened = await openFrame(key, wire);
    expect(opened).not.toBeNull();
    expect(dec.decode(opened!.plaintext)).toBe(f.plaintext);
    expect(opened!.seq).toBe(f.seq);
    expect(opened!.sessionId).toBe(f.session_id);
    expect(dec.decode((await openFrameWithAad(key, wire, fromHex(f.aad_hex)))!)).toBe(f.plaintext);
    const resealed = await sealFrame(key, enc.encode(f.plaintext), f.session_id, f.seq, fromHex(f.nonce_hex));
    expect(toB64(resealed)).toBe(f.wire_b64);
    // The plaintext is a valid envelope.
    expect(decodeEnvelope(opened!.plaintext).seq).toBe(f.seq);
  });

  it("rejects a low-order peer point", async () => {
    await expect(sharedSecret(pcPriv, new Uint8Array(32))).rejects.toThrow();
  });
});

describe("frame tampering", () => {
  const f = V.frames[0]!;
  const key = keyFor(f);
  const wire = fromB64(f.wire_b64);

  it("wrong key", async () => expect(await openFrame(fromHex(V.key_phone_to_pc_hex), wire)).toBeNull());
  it("flipped ciphertext byte", async () => {
    const t = wire.slice();
    t[40]! ^= 1;
    expect(await openFrame(key, t)).toBeNull();
  });
  it("flipped tag byte", async () => {
    const t = wire.slice();
    t[t.length - 1]! ^= 0x80;
    expect(await openFrame(key, t)).toBeNull();
  });
  it("wrong version byte", async () => {
    const t = wire.slice();
    t[0] = 2;
    expect(await openFrame(key, t)).toBeNull();
  });
  it("truncated", async () => expect(await openFrame(key, wire.subarray(0, 30))).toBeNull());
  it("aad must match (sealed with a different seq than the envelope says)", async () => {
    const forged = await sealFrame(key, enc.encode(f.plaintext), f.session_id, f.seq + 1);
    expect(await openFrame(key, forged)).toBeNull();
  });
});

describe("replay window and channel", () => {
  it("ReplayWindow accepts strictly increasing seq only", () => {
    const w = new ReplayWindow();
    expect(w.accept(0)).toBe(true);
    expect(w.accept(0)).toBe(false);
    expect(w.accept(5)).toBe(true);
    expect(w.accept(3)).toBe(false);
    expect(w.accept(6)).toBe(true);
    expect(w.accept(-1)).toBe(false);
    expect(w.accept(1.5)).toBe(false);
  });

  it("channel round trip, replay and cross-direction frames dropped", async () => {
    const k = await deriveKeys(fromHex(V.shared_hex), token);
    const phone = new SecureChannel({ sendKey: k.phoneToPc, recvKey: k.pcToPhone, now: () => 1000 });
    const pc = new SecureChannel({ sendKey: k.pcToPhone, recvKey: k.phoneToPc, now: () => 2000 });
    const sid = V.frames[0]!.session_id;

    const w1 = await phone.seal({ type: "command", payload: { action: "pin", answer_id: "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d" } }, sid);
    const w2 = await phone.seal({ type: "heartbeat", payload: {} }, null);
    const e1 = await pc.open(w1);
    expect(e1).toMatchObject({ type: "command", seq: 1, ts: 1000, session_id: sid });
    expect(await pc.open(w1)).toBeNull(); // replay
    expect(pc.dropped.replay).toBe(1);
    expect(await pc.open(w2)).toMatchObject({ type: "heartbeat", seq: 2, session_id: null });
    expect(await phone.open(w2)).toBeNull(); // own direction key cannot open it
    expect(phone.dropped.crypto).toBe(1);
    expect(phone.state).toEqual({ nextSendSeq: 3, lastRecvSeq: -1 });

    // State survives a reconnect: a fresh channel restored from state still rejects old frames.
    const pc2 = new SecureChannel({ sendKey: k.pcToPhone, recvKey: k.phoneToPc, state: pc.state });
    expect(await pc2.open(w1)).toBeNull();
  });

  it("frames the PC sends with the vector key decode on the phone, including the fixed vector", async () => {
    const k = await deriveKeys(fromHex(V.shared_hex), token);
    const phone = new SecureChannel({ sendKey: k.phoneToPc, recvKey: k.pcToPhone });
    const env = await phone.open(fromB64(V.frames[0]!.wire_b64));
    expect(env?.type).toBe("answer.delta");
  });

  it("drops an authentic frame whose plaintext is not a valid envelope", async () => {
    const k = await deriveKeys(fromHex(V.shared_hex), token);
    const phone = new SecureChannel({ sendKey: k.phoneToPc, recvKey: k.pcToPhone });
    const bad = JSON.stringify({ v: 1, type: "heartbeat", seq: 1, ts: 1, session_id: null, payload: { nope: 1 } });
    const wire = await sealFrame(k.pcToPhone, enc.encode(bad), null, 1);
    expect(await phone.open(wire)).toBeNull();
    expect(phone.dropped.schema).toBe(1);
  });
});

describe("pairing", () => {
  const qrText = JSON.stringify({
    v: 1,
    urls: ["ws://192.168.1.20:47823", `wss://relay.example.com/r/${V.room_id_hex}`],
    pc_pub: V.pc_pub_b64,
    token: V.token_b64,
    exp: 2_000_000_000,
  });

  it("parseQr validates and checks expiry", () => {
    expect(parseQr(qrText, 1_000).pc_pub).toBe(V.pc_pub_b64);
    expect(() => parseQr(qrText, 2_000_000_000_000)).toThrow(/expired/);
    expect(() => parseQr("{}")).toThrow(/rejected/);
    expect(() => parseQr("hello")).toThrow(/not pairing JSON/);
  });

  it("startPairing with the vector phone key reproduces proof, keys and room", async () => {
    const qr = parseQr(qrText, 1_000);
    const kp = { privateKey: phonePriv, publicKey: fromHex(V.phone_pub_hex) };
    const p = await startPairing(qr, "Pixel 9", { keyPair: kp, nowMs: 1_000 });
    const hello = decodeEnvelope(p.hello);
    expect(hello).toMatchObject({ type: "pair.hello", seq: 0, session_id: null });
    if (hello.type !== "pair.hello") throw new Error();
    expect(hello.payload).toEqual({ device_name: "Pixel 9", phone_pub: V.phone_pub_b64, proof: V.proof_b64 });
    expect(p.pairing.room_id).toBe(V.room_id_hex);
    expect(toHex(fromB64(p.pairing.k_pc_to_phone))).toBe(V.key_pc_to_phone_hex);
    expect(toHex(fromB64(p.pairing.k_phone_to_pc))).toBe(V.key_phone_to_pc_hex);

    const pcKeys = await verifyPairHello(hello, { privateKey: pcPriv, token, exp: 2_000_000_000 }, 1_000);
    expect(toHex(pcKeys.pcToPhone)).toBe(V.key_pc_to_phone_hex);
  });

  it("random phone key pairs with the PC; a wrong proof or expired token fails", async () => {
    const qr = parseQr(qrText, 1_000);
    const p = await startPairing(qr, "  ", { nowMs: 1_000 });
    const hello = decodeEnvelope(p.hello);
    if (hello.type !== "pair.hello") throw new Error();
    expect(hello.payload.device_name).toBe("Phone");
    const pcKeys = await verifyPairHello(hello, { privateKey: pcPriv, token, exp: 2_000_000_000 }, 1_000);
    expect(toB64(pcKeys.phoneToPc)).toBe(p.pairing.k_phone_to_pc);

    const wrongToken = new Uint8Array(32).fill(7);
    await expect(verifyPairHello(hello, { privateKey: pcPriv, token: wrongToken, exp: 2_000_000_000 }, 1_000)).rejects.toThrow(/bad proof/);
    await expect(verifyPairHello(hello, { privateKey: pcPriv, token, exp: 1 }, 1_000_000)).rejects.toThrow(/expired/);
  });
});
