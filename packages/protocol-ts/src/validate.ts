// Runtime validation against the contract schemas (draft 2020-12), mirroring
// ssc_core::protocol::validate: the envelope first, then the payload by `type`.
import Ajv2020 from "ajv/dist/2020.js";
import type { ValidateFunction } from "ajv";
import { ENVELOPE_SCHEMA, MESSAGE_SCHEMAS, MESSAGE_TYPES, QR_SCHEMA } from "./generated/schemas";
import type { QrPayload } from "./generated/types";
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

function validators(): Validators {
  if (cached) return cached;
  // strict mode stays on (unknown keywords are errors) except strictTypes, which only warns about
  // `oneOf` branches declaring their own `type`, a legal pattern the contract uses on purpose.
  const ajv = new Ajv2020({ strict: true, strictTypes: false, allErrors: false, validateFormats: false });
  const payloads = new Map<string, ValidateFunction>();
  for (const t of MESSAGE_TYPES) payloads.set(t, ajv.compile(MESSAGE_SCHEMAS[t]));
  cached = { envelope: ajv.compile(ENVELOPE_SCHEMA), qr: ajv.compile(QR_SCHEMA), payloads };
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
