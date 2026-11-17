// At-rest storage for the pairing (CR-15). The direction keys are raw bytes because the frame
// cipher (XChaCha20-Poly1305, @noble/ciphers) cannot take a WebCrypto key, so the whole pairing
// record is sealed with AES-GCM under a *non-extractable* CryptoKey that lives in IndexedDB.
// Anything that can only read storage (localStorage/IndexedDB dumps, a profile copied off the
// device, a script that exfiltrates storage contents) gets ciphertext plus an opaque key handle
// whose bytes the browser never reveals. A script running live on the origin can still ask
// WebCrypto to decrypt with that handle; the CSP (scripts/csp.mjs) is the defence against that.
//
// Nothing secret is written to localStorage any more. Legacy `ssc.pairing.v1` entries are
// migrated into IndexedDB once and then deleted.

export const LEGACY_PAIRING_KEY = "ssc.pairing.v1";
const DB_NAME = "ssc-phone";
const DB_VERSION = 1;
const STORE = "kv";
const WRAP_KEY_ID = "wrap-key.v1";
const PAIRING_ID = "pairing.v1";
const AAD = new TextEncoder().encode("ssc.pairing.v1");

/** What is stored for the pairing: an AES-GCM ciphertext of the JSON record, never plaintext. */
export interface SealedRecord {
  v: 1;
  iv: Uint8Array<ArrayBuffer>;
  ct: Uint8Array<ArrayBuffer>;
}

function idb(): IDBFactory | null {
  return typeof indexedDB === "undefined" ? null : indexedDB;
}

function req<T>(r: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

let dbPromise: Promise<IDBDatabase> | null = null;
function openDb(): Promise<IDBDatabase> {
  const f = idb();
  if (!f) return Promise.reject(new Error("IndexedDB unavailable"));
  if (!dbPromise) {
    const r = f.open(DB_NAME, DB_VERSION);
    r.onupgradeneeded = () => {
      if (!r.result.objectStoreNames.contains(STORE)) r.result.createObjectStore(STORE);
    };
    dbPromise = req(r).catch((e) => {
      dbPromise = null;
      throw e;
    });
  }
  return dbPromise;
}

async function get<T>(id: string): Promise<T | undefined> {
  const db = await openDb();
  return (await req(db.transaction(STORE, "readonly").objectStore(STORE).get(id))) as T | undefined;
}
async function put(id: string, v: unknown): Promise<void> {
  const db = await openDb();
  await req(db.transaction(STORE, "readwrite").objectStore(STORE).put(v, id));
}
async function del(id: string): Promise<void> {
  const db = await openDb();
  await req(db.transaction(STORE, "readwrite").objectStore(STORE).delete(id));
}

let wrapKeyPromise: Promise<CryptoKey> | null = null;
/** The non-extractable AES-GCM key that seals the pairing; created on first use. */
export function wrapKey(): Promise<CryptoKey> {
  if (!wrapKeyPromise) {
    wrapKeyPromise = (async () => {
      const existing = await get<CryptoKey>(WRAP_KEY_ID);
      if (existing) return existing;
      const k = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
      await put(WRAP_KEY_ID, k);
      return k;
    })().catch((e) => {
      wrapKeyPromise = null;
      throw e;
    });
  }
  return wrapKeyPromise;
}

async function seal(value: unknown): Promise<SealedRecord> {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const pt = new TextEncoder().encode(JSON.stringify(value));
  const ct = new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: AAD }, await wrapKey(), pt));
  return { v: 1, iv, ct };
}

async function open<T>(rec: SealedRecord): Promise<T> {
  const pt = await crypto.subtle.decrypt({ name: "AES-GCM", iv: rec.iv, additionalData: AAD }, await wrapKey(), rec.ct);
  return JSON.parse(new TextDecoder().decode(pt)) as T;
}

// Writes are chained so an older record can never land after a newer one.
let queue: Promise<unknown> = Promise.resolve();
function enqueue<T>(fn: () => Promise<T>): Promise<T> {
  const p = queue.then(fn, fn);
  queue = p.catch(() => undefined);
  return p;
}

/** Seal and store the pairing record. Rejects if IndexedDB/WebCrypto is unavailable. */
export function saveSealedPairing(value: unknown): Promise<void> {
  return enqueue(async () => put(PAIRING_ID, await seal(value)));
}

/**
 * Load the pairing record. Migrates a legacy localStorage entry once (store sealed, then delete).
 * A record that no longer decrypts (wrap key lost) is discarded: the user re-pairs.
 */
export function loadSealedPairing<T>(): Promise<T | null> {
  return enqueue(async () => {
    const legacy = readLegacy<T>();
    if (legacy) {
      await put(PAIRING_ID, await seal(legacy));
      removeLegacy();
      return legacy;
    }
    removeLegacy(); // unparsable leftovers too
    const rec = await get<SealedRecord>(PAIRING_ID);
    if (!rec) return null;
    try {
      return await open<T>(rec);
    } catch {
      await del(PAIRING_ID);
      return null;
    }
  });
}

/** Delete the sealed pairing (the wrap key stays; it protects nothing on its own). */
export function deleteSealedPairing(): Promise<void> {
  removeLegacy();
  return enqueue(() => del(PAIRING_ID));
}

function readLegacy<T>(): T | null {
  try {
    const v = typeof localStorage === "undefined" ? null : localStorage.getItem(LEGACY_PAIRING_KEY);
    return v ? (JSON.parse(v) as T) : null;
  } catch {
    return null;
  }
}
function removeLegacy(): void {
  try {
    localStorage.removeItem(LEGACY_PAIRING_KEY);
  } catch {
    /* storage blocked */
  }
}

/** Test hook: forget cached handles (e.g. after the test swaps in a fresh IndexedDB). */
export function _resetForTests(): void {
  dbPromise = null;
  wrapKeyPromise = null;
  queue = Promise.resolve();
}
