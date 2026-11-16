import { describe, expect, it } from "vitest";
import type { AnyEnvelope, Command, Outgoing } from "@ssc/protocol";
import { isValidEnvelope } from "@ssc/protocol";
import { Copilot } from "../src/lib/copilot";
import { VIBRATION } from "../src/lib/vibrate";

const AID = "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d";
const SID = "3f2b8c1e-9a4d-4e7b-8c21-5d6f7a8b9c0d";
let seq = 0;
const env = (type: AnyEnvelope["type"], payload: unknown): AnyEnvelope => ({ v: 1, type, seq: ++seq, ts: 1, session_id: SID, payload }) as AnyEnvelope;

function setup() {
  const sent: Outgoing[] = [];
  const vibrations: number[][] = [];
  const paints: (() => void)[] = [];
  let t = 0;
  const c = new Copilot({
    send: (m) => (sent.push(m), true),
    vibrate: (p) => vibrations.push(p),
    now: () => t,
    afterPaint: (cb) => paints.push(cb),
  });
  return { c, sent, vibrations, paints, setTime: (v: number) => (t = v), commands: () => sent.map((s) => s.payload as Command) };
}

const start = (urgency = "high") => env("answer.start", { answer_id: AID, trigger: "question", format: "cue", intent: "technical", urgency, model: "m" });
const delta = (text: string, block = "cue") => env("answer.delta", { answer_id: AID, block, text });

describe("Copilot", () => {
  it("vibrates once on the first delta with the urgency pattern and reports delta_ms after paint", () => {
    const { c, vibrations, paints, setTime, commands } = setup();
    c.handle(start("medium"));
    expect(vibrations).toEqual([]); // not on answer.start
    setTime(100);
    c.handle(delta("1. Req"));
    c.handle(delta("uirements\n"));
    expect(vibrations).toEqual([VIBRATION.medium]);
    expect(paints).toHaveLength(1);
    setTime(112.4);
    paints[0]!();
    expect(commands()).toEqual([{ action: "feedback", answer_id: AID, item: { kind: "timing", stage: "phone_rendered", delta_ms: 12 } }]);
  });

  it("every outgoing command is a valid contract frame", () => {
    const { c, sent, paints } = setup();
    c.handle(start());
    c.handle(delta("x"));
    paints[0]!();
    c.pin(AID);
    c.pin(AID); // unpin: local only
    c.rate(AID, "up");
    c.simple("regenerate", AID);
    c.simple("shorter", AID);
    c.simple("number", AID);
    c.expandAll(AID);
    void c.followup("  what about SSO?  ", AID);
    void c.followup("   "); // empty: not sent
    c.listMemory();
    c.confirmContact("b1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d", true);
    expect(sent.map((s) => (s.payload as Command).action)).toEqual([
      "feedback", "pin", "feedback", "regenerate", "shorter", "number", "expand_all", "followup", "memory.list", "contact.confirm",
    ]);
    for (const [i, s] of sent.entries()) {
      expect(isValidEnvelope({ v: 1, type: s.type, seq: i + 1, ts: 1, session_id: SID, payload: s.payload }), JSON.stringify(s)).toBe(true);
    }
    expect((sent[7]!.payload as { text: string }).text).toBe("what about SSO?");
  });

  it("streams into the sink, keeps text for late mounts, and structures on answer.end", () => {
    const { c } = setup();
    c.handle(start());
    c.handle(delta("1. A\n"));
    const got: string[] = [];
    c.registerSink(AID, { append: (_b, t) => got.push(t) });
    c.handle(delta("2. B\n"));
    c.handle(delta("more", "detail"));
    expect(got).toEqual(["2. B\n", "more"]);
    expect(c.answer(AID)!.text).toEqual({ cue: "1. A\n2. B\n", detail: "more" });
    c.handle(env("answer.end", { answer_id: AID, reason: "done", cues: [{ text: "A", expand: "a" }, { text: "B" }], not_in_docs: false }));
    expect(c.answer(AID)!.end!.cues).toHaveLength(2);
  });

  it("tap-to-expand keeps one cue open at a time", () => {
    const { c } = setup();
    c.handle(start());
    c.toggleCue(AID, 0);
    expect(c.answer(AID)!.expanded).toBe(0);
    c.toggleCue(AID, 2);
    expect(c.answer(AID)!.expanded).toBe(2);
    c.toggleCue(AID, 2);
    expect(c.answer(AID)!.expanded).toBeNull();
  });

  it("ignores deltas and ends for unknown answers, and duplicate starts", () => {
    const { c, vibrations } = setup();
    c.handle(delta("x"));
    c.handle(env("answer.end", { answer_id: AID, reason: "done", cues: [], not_in_docs: true }));
    expect(c.getState().answers).toEqual([]);
    expect(vibrations).toEqual([]);
    c.handle(start());
    c.handle(start());
    expect(c.getState().answers).toHaveLength(1);
  });

  it("tracks session, summary, memory and transcript", () => {
    const { c } = setup();
    c.handle(env("session.start", { session_id: SID, profile_id: "p", mode: "local" }));
    expect(c.getState().session?.mode).toBe("local");
    c.handle(env("transcript.partial", { speaker: "them", text: "How", final: false, t0: 0, t1: 1 }));
    c.handle(env("transcript.partial", { speaker: "them", text: "How much?", final: true, t0: 0, t1: 2 }));
    expect(c.getState().transcript.map((t) => t.text)).toEqual(["How much?"]);
    c.handle(env("session.end", { session_id: SID, reason: "user" }));
    expect(c.getState().session).toBeNull();
    c.handle(env("session.summary", { session_id: SID, summary: "s", action_items: [], followup_email: "e", contact_suggestions: [] }));
    c.handle(env("session.summary", { session_id: SID, summary: "s2", action_items: [], followup_email: "e", contact_suggestions: [] }));
    expect(c.getState().summaries.map((s) => s.summary)).toEqual(["s2"]);
    c.handle(env("memory.snapshot", { items: [] }));
    expect(c.getState().memory).toEqual([]);
  });
});
