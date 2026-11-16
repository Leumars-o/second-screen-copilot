// Render-time acceptance: 200 deltas applied to a mounted answer card, each under 16 ms, with
// no React re-render of the card per delta (append-only). Measured again in real Chromium by the E2E.
import { act } from "react";
import { createRoot } from "react-dom/client";
import { describe, expect, it } from "vitest";
import type { AnyEnvelope } from "@ssc/protocol";
import { Copilot } from "../src/lib/copilot";
import { AnswerCard } from "../src/ui/AnswerCard";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const AID = "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d";
const e = (type: AnyEnvelope["type"], payload: unknown, seq: number) => ({ v: 1, type, seq, ts: 1, session_id: null, payload }) as AnyEnvelope;

describe("answer card render", () => {
  it("applies 200 deltas, each < 16 ms, append-only", () => {
    const c = new Copilot({ send: () => true, afterPaint: () => undefined });
    c.handle(e("answer.start", { answer_id: AID, trigger: "hotkey", format: "cue", intent: "none", urgency: "low", model: "m" }, 1));
    const host = document.createElement("div");
    document.body.appendChild(host);
    const root = createRoot(host);
    act(() => root.render(<AnswerCard copilot={c} answer={c.answer(AID)!} current />));

    const cueEl = host.querySelector('[data-testid="stream-cue"]')!;
    let firstNode: ChildNode | null = null; // the first appended node must survive: nothing is replaced
    let renders = 0;
    const unsub = c.subscribe(() => renders++);

    const times: number[] = [];
    for (let i = 0; i < 200; i++) {
      const t0 = performance.now();
      c.handle(e("answer.delta", { answer_id: AID, block: i % 10 === 9 ? "detail" : "cue", text: `w${i} ` }, i + 2));
      times.push(performance.now() - t0);
      if (i === 0) firstNode = cueEl.firstChild;
    }
    unsub();

    expect(Math.max(...times)).toBeLessThan(16);
    expect(renders).toBe(0); // no React state change per delta
    expect(cueEl.firstChild).toBe(firstNode);
    expect(firstNode).not.toBeNull();
    expect(cueEl.childNodes.length).toBe(180);
    expect(cueEl.textContent).toBe(c.answer(AID)!.text.cue);
    expect(host.querySelector('[data-testid="stream-detail"]')!.textContent).toBe(c.answer(AID)!.text.detail);
    expect(c.deltaTimings).toHaveLength(200);

    // answer.end swaps in the structured cues once.
    act(() => c.handle(e("answer.end", { answer_id: AID, reason: "done", cues: [{ text: "Scope", expand: "Agree it first" }, { text: "Ship" }], fact: "99.9%", source_doc: "sla.pdf", not_in_docs: false }, 300)));
    act(() => root.render(<AnswerCard copilot={c} answer={c.answer(AID)!} current />));
    expect(host.querySelectorAll('[data-testid="cues"] li')).toHaveLength(2);
    expect(host.querySelector('[data-testid="fact"] strong')!.textContent).toBe("99.9%");
    expect(host.querySelector('[data-testid="fact"]')!.textContent).toContain("sla.pdf");
    act(() => root.unmount());
  });
});
