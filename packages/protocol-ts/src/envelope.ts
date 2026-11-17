// Envelope codec. Wire JSON key order matches the Rust side: v, type, seq, ts, session_id, payload.
import type { PayloadMap } from "./generated/types";
import { validateEnvelope } from "./validate";

export type MessageType = keyof PayloadMap;

export interface Envelope<T extends MessageType = MessageType> {
  v: 1;
  type: T;
  seq: number;
  ts: number;
  session_id: string | null;
  payload: PayloadMap[T];
}

/** Discriminated union over every message type: narrow with `env.type === "answer.delta"`. */
export type AnyEnvelope = { [T in MessageType]: Envelope<T> }[MessageType];

/** A message before the channel assigns `seq` and `ts`. */
export type Outgoing = { [T in MessageType]: { type: T; payload: PayloadMap[T] } }[MessageType];

export function makeEnvelope<T extends MessageType>(
  type: T,
  payload: PayloadMap[T],
  seq: number,
  ts: number,
  session_id: string | null,
): Envelope<T> {
  return { v: 1, type, seq, ts, session_id, payload };
}

/** Serialise after validating, so we never send a frame the PC would reject. */
export function encodeEnvelope(env: AnyEnvelope | Envelope): string {
  const ordered = { v: env.v, type: env.type, seq: env.seq, ts: env.ts, session_id: env.session_id, payload: env.payload };
  validateEnvelope(ordered);
  return JSON.stringify(ordered);
}

const utf8 = new TextDecoder("utf-8", { fatal: true });

/** Parse and validate. Throws on malformed JSON, bad UTF-8 or a schema violation. */
export function decodeEnvelope(input: string | Uint8Array): AnyEnvelope {
  const text = typeof input === "string" ? input : utf8.decode(input);
  const value: unknown = JSON.parse(text);
  validateEnvelope(value);
  return value;
}
