// Runtime validation against the contract schemas (draft 2020-12), mirroring
// ssc_core::protocol::validate: the envelope first, then the payload by `type`.
import type { ValidateFunction } from "ajv";
import { MESSAGE_TYPES } from "./generated/schemas";
import type { QrPayload } from "./generated/types";
import * as compiled from "./generated/validators";
import type { AnyEnvelope, MessageType } from "./envelope";

export class ProtocolError extends Error {
  constructor(
    readonly schema: string,
    readonly detail: string,
  ) {
    super(`schema violation in ${schema}: ${detail}`);
    this.name = "ProtocolError";
  }
}

interface Validators {
  envelope: ValidateFunction;
  qr: ValidateFunction;
  payloads: Map<string, ValidateFunction>;
}

let cached: Validators | undefined;

// Precompiled at generate time (Ajv standalone, same options: strict, strictTypes off, formats
// off), so validation never evaluates generated code at runtime and the PWA's CSP can forbid
// 'unsafe-eval' (CR-15).
function validators(): Validators {
  if (cached) return cached;
  const all = compiled as unknown as Record<string, ValidateFunction>;
  const get = (name: string): ValidateFunction => {
    const v = all[name];
    if (!v) throw new Error(`no precompiled validator ${name} (run pnpm --filter @ssc/protocol generate)`);
    return v;
  };
  const payloads = new Map<string, ValidateFunction>();
  for (const t of MESSAGE_TYPES) payloads.set(t, get(`msg_${t.replace(/\W/g, "_")}`));
  cached = { envelope: get("envelope"), qr: get("qr"), payloads };
  return cached;
}

function detail(v: ValidateFunction): string {
  const e = v.errors?.[0];
  return e ? `${e.instancePath || "/"} ${e.message ?? "invalid"}` : "invalid";
}

/** Validate a parsed frame exactly like the Rust side. Throws ProtocolError. */
export function validateEnvelope(value: unknown): asserts value is AnyEnvelope {
  const v = validators();
  if (!v.envelope(value)) throw new ProtocolError("envelope", detail(v.envelope));
  const env = value as { type: MessageType; payload: unknown };
  const pv = v.payloads.get(env.type);
  if (!pv) throw new ProtocolError("envelope", `unknown type ${env.type}`);
  if (!pv(env.payload)) throw new ProtocolError(env.type, detail(pv));
}

export function isValidEnvelope(value: unknown): value is AnyEnvelope {
  try {
    validateEnvelope(value);
    return true;
  } catch {
    return false;
  }
}

export function validateQr(value: unknown): asserts value is QrPayload {
  const v = validators();
  if (!v.qr(value)) throw new ProtocolError("qr", detail(v.qr));
}
