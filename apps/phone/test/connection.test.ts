import { describe, expect, it } from "vitest";
import { qrJsonFromText, restoreLinkState, SEQ_RESERVATION, takePairFromLocation } from "../src/lib/connection";

describe("seq persistence across restarts", () => {
  it("jumps next-send past the reservation and keeps last-accepted", () => {
    expect(restoreLinkState(null)).toBeNull();
    const r = restoreLinkState({ state: { nextSendSeq: 42, lastRecvSeq: 17 }, sessionId: null });
    expect(r!.state).toEqual({ nextSendSeq: 42 + SEQ_RESERVATION, lastRecvSeq: 17 });
  });
});

describe("CR-19 pairing links", () => {
  const json = '{"v":1,"urls":["wss://lan.example.com:47823"],"pc_pub":"x","token":"y","exp":1}';
  const b64url = btoa(json).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

  it("passes raw JSON through and decodes the link form", () => {
    expect(qrJsonFromText(` ${json} `)).toBe(json);
    expect(qrJsonFromText(`https://app.example.com/#pair=${b64url}`)).toBe(json);
  });

  it("decodes non-ASCII payloads (base64url of UTF-8)", () => {
    const utf = '{"name":"Téléphone ✓"}';
    const bytes = new TextEncoder().encode(utf);
    const b = btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
    expect(qrJsonFromText(`https://a.example.com/#pair=${b}`)).toBe(utf);
  });

  it("rejects a damaged link", () => {
    expect(() => qrJsonFromText("https://app.example.com/#pair=@@@")).toThrow(/damaged/);
  });

  it("takes the payload from the address bar and strips it", () => {
    history.replaceState(null, "", `/?x=1#pair=${b64url}`);
    const href = takePairFromLocation();
    expect(href && qrJsonFromText(href)).toBe(json);
    expect(location.hash).toBe("");
    expect(location.search).toBe("?x=1");
    expect(takePairFromLocation()).toBeNull();
  });
});

describe("link state is scoped to its pairing (re-pair race)", () => {
  const s = { state: { nextSendSeq: 5, lastRecvSeq: 9 }, sessionId: null };
  it("ignores state saved by another pairing, keeps its own and untagged (pre-fix) state", () => {
    expect(restoreLinkState({ ...s, room: "old" }, "new")).toBeNull();
    expect(restoreLinkState({ ...s, room: "new" }, "new")!.state.lastRecvSeq).toBe(9);
    // Untagged state from before the fix must survive the upgrade, or the PC would reject the
    // phone's restarted (seq 0) frames as replays.
    expect(restoreLinkState(s, "new")!.state.nextSendSeq).toBe(5 + SEQ_RESERVATION);
  });
});
