// CR-15: the direction keys never sit in localStorage; at rest they are AES-GCM ciphertext under a
// non-extractable key held in IndexedDB; legacy localStorage pairings migrate once.
import { generateKeyPair, toB64 } from "@ssc/protocol";
import { IDBFactory } from "fake-indexeddb";
import { beforeEach, describe, expect, it } from "vitest";
import { forgetPairing, loadPairing, pairFromQr, type StoredPairing } from "../src/lib/connection";
import { _resetForTests, LEGACY_PAIRING_KEY, wrapKey, type SealedRecord } from "../src/lib/keystore";

const K1 = "a".repeat(43) + "=";
const K2 = "b".repeat(43) + "=";
const legacy: StoredPairing = {
  pairing: { v: 1, pc_pub: "cGM=", urls: ["wss://lan.example/ws"], room_id: "00", k_pc_to_phone: K1, k_phone_to_pc: K2, created_at: 1 },
  hello: '{"type":"pair.hello"}',
  confirmed: false,
};

function rawRecord(id: string): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const r = indexedDB.open("ssc-phone");
    r.onerror = () => reject(r.error);
    r.onsuccess = () => {
      const g = r.result.transaction("kv").objectStore("kv").get(id);
      g.onsuccess = () => {
        r.result.close();
        resolve(g.result);
      };
      g.onerror = () => reject(g.error);
    };
  });
}

/** Everything the browser stores for this origin, as text a storage-reading script could see. */
async function storageDump(): Promise<string> {
  const ls = JSON.stringify(Object.fromEntries(Object.keys(localStorage).map((k) => [k, localStorage.getItem(k)])));
  const rec = (await rawRecord("pairing.v1")) as SealedRecord | undefined;
  const idb = rec ? Buffer.from(rec.ct).toString("latin1") + Buffer.from(rec.ct).toString("base64") : "";
  return ls + idb;
}

beforeEach(() => {
  globalThis.indexedDB = new IDBFactory();
  localStorage.clear();
  _resetForTests();
});

describe("pairing storage (CR-15)", () => {
  it("migrates a legacy localStorage pairing once, then deletes it", async () => {
    localStorage.setItem(LEGACY_PAIRING_KEY, JSON.stringify(legacy));
    expect(await loadPairing()).toEqual(legacy);
    expect(localStorage.getItem(LEGACY_PAIRING_KEY)).toBeNull();
    const dump = await storageDump();
    expect(dump).not.toContain(K1.slice(0, 20));
    expect(dump).not.toContain(K2.slice(0, 20));
    // Later loads come from IndexedDB.
    _resetForTests();
    expect(await loadPairing()).toEqual(legacy);
  });

  it("stores the pairing sealed: AES-GCM ciphertext under a non-extractable IndexedDB key", async () => {
    localStorage.setItem(LEGACY_PAIRING_KEY, JSON.stringify(legacy));
    await loadPairing();
    const rec = (await rawRecord("pairing.v1")) as SealedRecord;
    expect(rec.v).toBe(1);
    // (jsdom and Node have separate Uint8Array realms, so check the shape, not instanceof.)
    expect(ArrayBuffer.isView(rec.iv) && rec.iv.byteLength).toBe(12);
    expect(ArrayBuffer.isView(rec.ct) && rec.ct.byteLength).toBeGreaterThan(16);
    expect(new TextDecoder().decode(rec.ct)).not.toContain("k_pc_to_phone");

    const k = (await rawRecord("wrap-key.v1")) as CryptoKey;
    expect(k.extractable).toBe(false);
    expect(k.algorithm).toMatchObject({ name: "AES-GCM", length: 256 });
    await expect(crypto.subtle.exportKey("raw", await wrapKey())).rejects.toThrow();
  });

  it("pairing from a QR writes no key material to localStorage", async () => {
    const kp = await generateKeyPair();
    const token = toB64(new Uint8Array(32).fill(7));
    const qr = { v: 1, urls: ["wss://lan.example/ws"], pc_pub: toB64(kp.publicKey), token, exp: Math.floor(Date.now() / 1000) + 60 };
    const stored = await pairFromQr(JSON.stringify(qr), "Test");
    const dump = await storageDump();
    expect(dump).not.toContain(stored.pairing.k_pc_to_phone);
    expect(dump).not.toContain(stored.pairing.k_phone_to_pc);
    _resetForTests();
    expect((await loadPairing())?.pairing).toEqual(stored.pairing);
  });

  it("a record that no longer decrypts (wrap key gone) is dropped, not trusted", async () => {
    localStorage.setItem(LEGACY_PAIRING_KEY, JSON.stringify(legacy));
    await loadPairing();
    await new Promise<void>((resolve) => {
      const r = indexedDB.open("ssc-phone");
      r.onsuccess = () => {
        const tx = r.result.transaction("kv", "readwrite");
        tx.objectStore("kv").delete("wrap-key.v1");
        tx.oncomplete = () => (r.result.close(), resolve());
      };
    });
    _resetForTests();
    expect(await loadPairing()).toBeNull();
    expect(await rawRecord("pairing.v1")).toBeUndefined();
  });

  it("forget deletes the sealed pairing and any legacy entry", async () => {
    localStorage.setItem(LEGACY_PAIRING_KEY, JSON.stringify(legacy));
    await loadPairing();
    await forgetPairing();
    expect(await rawRecord("pairing.v1")).toBeUndefined();
    expect(await loadPairing()).toBeNull();
  });

  it("a reload after the PC confirmed pairing does not resend the hello, even if the sealed update was lost", async () => {
    localStorage.setItem(LEGACY_PAIRING_KEY, JSON.stringify(legacy));
    localStorage.setItem("ssc.link.v1", JSON.stringify({ state: { nextSendSeq: 1, lastRecvSeq: -1 }, sessionId: null, helloConfirmed: true }));
    expect((await loadPairing())?.hello).toBeUndefined();
  });

  it("an unconfirmed pairing keeps its hello across a reload, so it is sent again (L4)", async () => {
    localStorage.setItem(LEGACY_PAIRING_KEY, JSON.stringify(legacy));
    localStorage.setItem("ssc.link.v1", JSON.stringify({ state: { nextSendSeq: 1, lastRecvSeq: -1 }, sessionId: null, helloConfirmed: false }));
    expect((await loadPairing())?.hello).toBe(legacy.hello);
  });

  it("a confirmed pairing drops a leftover hello", async () => {
    localStorage.setItem(LEGACY_PAIRING_KEY, JSON.stringify({ ...legacy, confirmed: true }));
    expect((await loadPairing())?.hello).toBeUndefined();
  });
});
