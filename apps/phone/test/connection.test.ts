import { describe, expect, it } from "vitest";
import { restoreLinkState, SEQ_RESERVATION } from "../src/lib/connection";

describe("seq persistence across restarts", () => {
  it("jumps next-send past the reservation and keeps last-accepted", () => {
    expect(restoreLinkState(null)).toBeNull();
    const r = restoreLinkState({ state: { nextSendSeq: 42, lastRecvSeq: 17 }, sessionId: null });
    expect(r!.state).toEqual({ nextSendSeq: 42 + SEQ_RESERVATION, lastRecvSeq: 17 });
  });
});
