// App state outside React. Structural changes (new answer, end, session, summary, memory) notify
// React subscribers; streamed deltas bypass React and go straight to a per-answer DOM sink, so a
// delta costs one text-node append (append-only, no re-render, no reflow of earlier content).
import type {
  AnswerEnd,
  AnswerStart,
  AnyEnvelope,
  Command,
  MemorySnapshot,
  Outgoing,
  SessionStart,
  SessionSummary,
  TranscriptPartial,
} from "@ssc/protocol";
import { vibrationPattern } from "./vibrate";

export type Block = "cue" | "detail";

export interface Answer {
  id: string;
  start: AnswerStart;
  /** Streamed text per block, kept so a sink mounted late can render what came before. */
  text: Record<Block, string>;
  end?: AnswerEnd;
  pinned: boolean;
  rating?: "up" | "down";
  /** Index of the one expanded cue, or null. */
  expanded: number | null;
  expandAll: boolean;
}

export interface DeltaSink {
  append(block: Block, text: string): void;
}

export interface CopilotState {
  answers: Answer[]; // newest first
  session: SessionStart | null;
  lastSessionEnd: string | null;
  summaries: SessionSummary[]; // newest first
  memory: MemorySnapshot["items"] | null;
  transcript: TranscriptPartial[]; // last few lines
  confirmedContacts: Record<string, boolean>;
}

export interface CopilotDeps {
  send: (msg: Outgoing) => Promise<boolean> | boolean;
  vibrate?: (pattern: number[]) => void;
  now?: () => number;
  /** Runs `cb` after the next paint. Default: rAF + macrotask. */
  afterPaint?: (cb: () => void) => void;
}

const MAX_ANSWERS = 50;
const MAX_TRANSCRIPT = 6;

function defaultAfterPaint(cb: () => void): void {
  if (typeof requestAnimationFrame === "function") requestAnimationFrame(() => setTimeout(cb, 0));
  else setTimeout(cb, 0);
}

export class Copilot {
  private state: CopilotState = {
    answers: [],
    session: null,
    lastSessionEnd: null,
    summaries: [],
    memory: null,
    transcript: [],
    confirmedContacts: {},
  };
  private readonly listeners = new Set<() => void>();
  private readonly sinks = new Map<string, DeltaSink>();
  private readonly firstDelta = new Set<string>();
  /** Per-delta apply time in ms (for the render-time test and the debug readout). */
  readonly deltaTimings: number[] = [];
  private readonly now: () => number;

  constructor(private readonly deps: CopilotDeps) {
    this.now = deps.now ?? (() => (typeof performance !== "undefined" ? performance.now() : Date.now()));
  }

  // ---- React glue ----
  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };
  getState = (): CopilotState => this.state;

  private set(patch: Partial<CopilotState>): void {
    this.state = { ...this.state, ...patch };
    for (const l of this.listeners) l();
  }

  private updateAnswer(id: string, fn: (a: Answer) => Answer): void {
    this.set({ answers: this.state.answers.map((a) => (a.id === id ? fn(a) : a)) });
  }

  answer(id: string): Answer | undefined {
    return this.state.answers.find((a) => a.id === id);
  }

  registerSink(answerId: string, sink: DeltaSink): () => void {
    this.sinks.set(answerId, sink);
    return () => {
      if (this.sinks.get(answerId) === sink) this.sinks.delete(answerId);
    };
  }

  // ---- incoming ----
  handle(env: AnyEnvelope): void {
    switch (env.type) {
      case "answer.start": {
        if (this.answer(env.payload.answer_id)) return;
        const a: Answer = { id: env.payload.answer_id, start: env.payload, text: { cue: "", detail: "" }, pinned: false, expanded: null, expandAll: false };
        this.set({ answers: [a, ...this.state.answers].slice(0, MAX_ANSWERS) });
        return;
      }
      case "answer.delta":
        return this.applyDelta(env.payload.answer_id, env.payload.block, env.payload.text);
      case "answer.end":
        if (!this.answer(env.payload.answer_id)) return;
        return this.updateAnswer(env.payload.answer_id, (a) => ({ ...a, end: env.payload }));
      case "session.start":
        return this.set({ session: env.payload, transcript: [] });
      case "session.end":
        return this.set({ session: null, lastSessionEnd: env.payload.session_id });
      case "session.summary":
        return this.set({ summaries: [env.payload, ...this.state.summaries.filter((s) => s.session_id !== env.payload.session_id)] });
      case "memory.snapshot":
        return this.set({ memory: env.payload.items });
      case "transcript.partial": {
        const t = this.state.transcript;
        const last = t[t.length - 1];
        // Replace a non-final line from the same speaker, append otherwise.
        const base = last && !last.final && last.speaker === env.payload.speaker ? t.slice(0, -1) : t;
        return this.set({ transcript: [...base, env.payload].slice(-MAX_TRANSCRIPT) });
      }
      default:
        return; // heartbeat, and phone->PC types the PC never sends
    }
  }

  /** Hot path: no React state change, one DOM append. */
  applyDelta(answerId: string, block: Block, text: string): void {
    const t0 = this.now();
    const a = this.answer(answerId);
    if (!a) return;
    a.text[block] += text; // mutated in place on purpose: sinks render it, React does not
    this.sinks.get(answerId)?.append(block, text);
    const first = !this.firstDelta.has(answerId);
    if (first) {
      this.firstDelta.add(answerId);
      this.deps.vibrate?.(vibrationPattern(a.start.urgency));
    }
    this.deltaTimings.push(this.now() - t0);
    if (this.deltaTimings.length > 1000) this.deltaTimings.splice(0, 500);
    if (first) {
      // §4.6: report first delta received -> first paint, as delta_ms.
      (this.deps.afterPaint ?? defaultAfterPaint)(() => {
        const delta_ms = Math.max(0, Math.min(60_000, Math.round(this.now() - t0)));
        void this.command({ action: "feedback", answer_id: answerId, item: { kind: "timing", stage: "phone_rendered", delta_ms } });
      });
    }
  }

  // ---- outgoing ----
  command(c: Command): Promise<boolean> {
    return Promise.resolve(this.deps.send({ type: "command", payload: c }));
  }

  pin(id: string): void {
    const a = this.answer(id);
    if (!a) return;
    this.updateAnswer(id, (x) => ({ ...x, pinned: !x.pinned }));
    // The protocol has no unpin; only pinning is reported to the PC.
    if (!a.pinned) void this.command({ action: "pin", answer_id: id });
  }

  rate(id: string, value: "up" | "down"): void {
    this.updateAnswer(id, (x) => ({ ...x, rating: value }));
    void this.command({ action: "feedback", answer_id: id, item: { kind: "rating", value } });
  }

  simple(action: "regenerate" | "shorter" | "number", id: string): void {
    void this.command({ action, answer_id: id });
  }

  expandAll(id: string): void {
    this.updateAnswer(id, (x) => ({ ...x, expandAll: true }));
    void this.command({ action: "expand_all", answer_id: id });
  }

  /** Tap a cue: expand it, collapsing any other (one at a time). Tap again to collapse. */
  toggleCue(id: string, index: number): void {
    this.updateAnswer(id, (x) => ({ ...x, expanded: x.expanded === index ? null : index }));
  }

  followup(text: string, answerId?: string): Promise<boolean> {
    const t = text.trim().slice(0, 2000);
    if (!t) return Promise.resolve(false);
    return this.command(answerId ? { action: "followup", answer_id: answerId, text: t } : { action: "followup", text: t });
  }

  listMemory(): void {
    void this.command({ action: "memory.list" });
  }

  confirmContact(suggestionId: string, accept: boolean): void {
    this.set({ confirmedContacts: { ...this.state.confirmedContacts, [suggestionId]: accept } });
    void this.command({ action: "contact.confirm", suggestion_id: suggestionId, accept });
  }
}
