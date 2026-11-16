// Mock PC: speaks the real protocol (plaintext pair.hello, then encrypted frames) over a Node
// WebSocket, and exposes a small HTTP control API for E2E tests. Optionally serves a static dir
// (the phone's `out/`) on the same origin, so the page is a secure context (localhost).
import { randomUUID } from "node:crypto";
import { createReadStream, existsSync, statSync } from "node:fs";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { extname, join, normalize, resolve } from "node:path";
import { WebSocketServer, type WebSocket } from "ws";
import {
  decodeEnvelope,
  generateKeyPair,
  roomId,
  SecureChannel,
  toB64,
  verifyPairHello,
  type AnyEnvelope,
  type AnswerEnd,
  type AnswerStart,
  type Command,
  type MemorySnapshot,
  type Outgoing,
  type QrPayload,
  type SessionSummary,
} from "../src/index";

export interface MockPcOptions {
  port?: number;
  host?: string;
  staticDir?: string;
  /** Extra url placed before/after the LAN url in the QR (e.g. a dead LAN address to test fallback). */
  qrUrls?: (lanUrl: string) => string[];
  log?: (msg: string) => void;
}

export interface StreamAnswerOptions {
  deltas?: string[];
  count?: number;
  intervalMs?: number;
  urgency?: AnswerStart["urgency"];
  format?: AnswerStart["format"];
  trigger?: AnswerStart["trigger"];
  block?: "cue" | "detail";
  end?: Partial<Omit<AnswerEnd, "answer_id">>;
}

const DEFAULT_END: Omit<AnswerEnd, "answer_id"> = {
  reason: "done",
  cues: [
    { text: "Requirements", expand: "Agree scope and success metrics with the client first." },
    { text: "Design", expand: "Architecture review, one page, signed off by both leads." },
    { text: "Build", expand: "Two-week sprints with a demo at the end of each." },
    { text: "Test" },
    { text: "Deploy" },
  ],
  fact: "99.9% uptime SLA",
  source_doc: "pricing-2026.pdf",
  not_in_docs: false,
};

const MIME: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".css": "text/css",
  ".json": "application/json",
  ".webmanifest": "application/manifest+json",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".txt": "text/plain",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
};

export class MockPc {
  readonly received: AnyEnvelope[] = [];
  qr!: QrPayload;
  sessionId: string | null = null;
  memory: MemorySnapshot["items"] = [
    { id: randomUUID(), layer: "about_me", title: "Role", body: "Solutions engineer at Acme", updated_at: 1763290000000 },
    { id: randomUUID(), layer: "profile", title: "Sales: product X", body: "Lead with time-to-value.", updated_at: 1763290000000 },
    { id: randomUUID(), layer: "contact", title: "Dana Lee", body: "CTO at Globex. Cares about SSO.", updated_at: 1763290000000 },
  ];
  pairedCount = 0;

  private pcPriv!: Uint8Array;
  private token!: Uint8Array;
  private tokenBurned = false;
  private keys: { pcToPhone: Uint8Array; phoneToPc: Uint8Array } | null = null;
  private channel: SecureChannel | null = null;
  private socket: WebSocket | null = null;
  private readonly http = createServer((req, res) => void this.handle(req, res));
  private readonly wss = new WebSocketServer({ noServer: true, maxPayload: 64 * 1024 });
  private readonly log: (m: string) => void;
  private hbTimer: ReturnType<typeof setInterval> | undefined;
  port = 0;

  constructor(private readonly opts: MockPcOptions = {}) {
    this.log = opts.log ?? (() => undefined);
    this.http.on("upgrade", (req, sock, head) => this.wss.handleUpgrade(req, sock, head, (ws) => this.onSocket(ws)));
  }

  get lanUrl(): string {
    return `ws://${this.opts.host ?? "127.0.0.1"}:${this.port}`;
  }

  get connected(): boolean {
    return this.socket?.readyState === 1;
  }

  async start(): Promise<this> {
    await new Promise<void>((r) => this.http.listen(this.opts.port ?? 0, this.opts.host ?? "127.0.0.1", r));
    const addr = this.http.address();
    this.port = typeof addr === "object" && addr ? addr.port : 0;
    await this.newQr();
    this.hbTimer = setInterval(() => void this.send({ type: "heartbeat", payload: {} }), 5_000);
    return this;
  }

  async close(): Promise<void> {
    clearInterval(this.hbTimer);
    for (const c of this.wss.clients) c.terminate();
    await new Promise<void>((r) => this.wss.close(() => r()));
    this.http.closeAllConnections();
    await new Promise<void>((r) => this.http.close(() => r()));
  }

  /** New PC key + one-time token (unpairs any previous phone). */
  async newQr(expSeconds = 120): Promise<QrPayload> {
    const kp = await generateKeyPair();
    this.pcPriv = kp.privateKey;
    this.token = crypto.getRandomValues(new Uint8Array(32));
    this.tokenBurned = false;
    this.keys = null;
    this.channel = null;
    const lan = this.lanUrl;
    const relay = `wss://relay.invalid/r/${await roomId(this.token)}`;
    const urls = this.opts.qrUrls ? this.opts.qrUrls(lan) : [lan, relay];
    this.qr = { v: 1, urls: urls as QrPayload["urls"], pc_pub: toB64(kp.publicKey), token: toB64(this.token), exp: Math.floor(Date.now() / 1000) + expSeconds };
    return this.qr;
  }

  private onSocket(ws: WebSocket): void {
    this.socket?.terminate();
    this.socket = ws;
    this.log("phone connected");
    ws.on("message", (data, isBinary) => void this.onMessage(ws, data as Buffer, isBinary));
    ws.on("close", () => {
      if (this.socket === ws) this.socket = null;
    });
  }

  private async onMessage(ws: WebSocket, data: Buffer, isBinary: boolean): Promise<void> {
    if (!isBinary) {
      // The single plaintext frame: pair.hello (CR-2).
      if (this.tokenBurned) return void ws.close(4001, "token used");
      this.tokenBurned = true; // one use, even on failure
      try {
        const env = decodeEnvelope(data.toString("utf8"));
        if (env.type !== "pair.hello") throw new Error("expected pair.hello");
        this.keys = await verifyPairHello(env, { privateKey: this.pcPriv, token: this.token, exp: this.qr.exp });
        this.channel = new SecureChannel({ sendKey: this.keys.pcToPhone, recvKey: this.keys.phoneToPc });
        this.received.push(env);
        this.pairedCount++;
        this.log(`paired with ${env.payload.device_name}`);
        await this.send({ type: "heartbeat", payload: {} }); // confirms pairing to the phone
      } catch (e) {
        this.log(`pairing failed: ${(e as Error).message}`);
        ws.close(4002, "pairing failed");
      }
      return;
    }
    if (!this.channel) return;
    const env = await this.channel.open(new Uint8Array(data));
    if (!env) return;
    this.received.push(env);
    if (env.type === "heartbeat") {
      await this.send({ type: "heartbeat", payload: { echo: env.ts } });
    } else if (env.type === "command") {
      await this.onCommand(env.payload);
    }
  }

  private async onCommand(c: Command): Promise<unknown> {
    switch (c.action) {
      case "memory.list":
        return this.sendSnapshot();
      case "memory.update": {
        const now = Date.now();
        const existing = c.item.id ? this.memory.find((m) => m.id === c.item.id) : undefined;
        if (existing) Object.assign(existing, { title: c.item.title, body: c.item.body, layer: c.item.layer, updated_at: now });
        else this.memory.push({ id: randomUUID(), layer: c.item.layer, title: c.item.title, body: c.item.body, updated_at: now });
        return this.sendSnapshot();
      }
      case "memory.delete":
        this.memory = this.memory.filter((m) => m.id !== c.id);
        return this.sendSnapshot();
      case "regenerate":
      case "shorter":
      case "number":
      case "followup":
        await this.streamAnswer({ trigger: "command", count: 4, intervalMs: 20 });
        return;
      case "expand_all":
        await this.streamAnswer({
          trigger: "command",
          block: "detail",
          deltas: ["Full answer: scope first, ", "then a one-page design, ", "two-week build sprints."],
          intervalMs: 20,
          end: { cues: [], not_in_docs: false },
        });
        return;
      default:
        return; // pin, feedback, contact.confirm: recorded only
    }
  }

  private sendChain: Promise<unknown> = Promise.resolve();

  /** Serialised so wire order equals seq order. */
  send(msg: Outgoing): Promise<boolean> {
    const run = this.sendChain.then(async () => {
      const ws = this.socket;
      const ch = this.channel;
      if (!ws || ws.readyState !== 1 || !ch) return false;
      ws.send(await ch.seal(msg, this.sessionId));
      return true;
    });
    this.sendChain = run.catch(() => undefined);
    return run;
  }

  sendSnapshot(): Promise<boolean> {
    return this.send({ type: "memory.snapshot", payload: { items: this.memory } });
  }

  async startSession(): Promise<string> {
    const id = randomUUID();
    this.sessionId = id; // the session.start envelope carries the new id (see fixture)
    await this.send({ type: "session.start", payload: { session_id: id, profile_id: "sales-x", mode: "cloud" } });
    return id;
  }

  async endSession(summary?: Partial<SessionSummary>): Promise<void> {
    const id = this.sessionId ?? randomUUID();
    await this.send({ type: "session.end", payload: { session_id: id, reason: "user" } });
    await this.send({
      type: "session.summary",
      payload: {
        session_id: id,
        summary: "Globex wants SSO and a 99.9% SLA before Q1. Budget approved.",
        action_items: ["Send SSO docs", "Book security review"],
        followup_email: "Hi Dana,\n\nThanks for the time today. Attached are the SSO docs.\n\nBest,\nSam",
        contact_suggestions: [{ id: randomUUID(), name: "Dana Lee", company: "Globex", notes: "CTO; owns the SSO decision" }],
        ...summary,
      },
    });
    this.sessionId = null;
  }

  async transcript(text: string, speaker: "me" | "them" = "them"): Promise<void> {
    const t = Date.now();
    await this.send({ type: "transcript.partial", payload: { speaker, text, final: true, t0: t - 1500, t1: t } });
  }

  /** answer.start, then deltas, then answer.end. Resolves with the answer id. */
  async streamAnswer(o: StreamAnswerOptions = {}): Promise<string> {
    const answer_id = randomUUID();
    const deltas =
      o.deltas ??
      Array.from({ length: o.count ?? 5 }, (_, i) => (i < 5 ? `${i + 1}. ${DEFAULT_END.cues[i]!.text}\n` : `more ${i} `));
    await this.send({
      type: "answer.start",
      payload: { answer_id, trigger: o.trigger ?? "question", format: o.format ?? "cue", intent: "technical", urgency: o.urgency ?? "high", model: "mock/fast" },
    });
    for (const text of deltas) {
      if (o.intervalMs) await new Promise((r) => setTimeout(r, o.intervalMs));
      await this.send({ type: "answer.delta", payload: { answer_id, block: o.block ?? "cue", text } });
    }
    await this.send({ type: "answer.end", payload: { answer_id, ...DEFAULT_END, ...o.end } });
    return answer_id;
  }

  private async handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const url = new URL(req.url ?? "/", "http://x");
    if (url.pathname.startsWith("/__mock/")) {
      let body: Record<string, unknown> = {};
      if (req.method === "POST") {
        const chunks: Buffer[] = [];
        for await (const c of req) chunks.push(c as Buffer);
        const text = Buffer.concat(chunks).toString("utf8");
        body = text ? (JSON.parse(text) as Record<string, unknown>) : {};
      }
      const json = (v: unknown) => {
        res.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
        res.end(JSON.stringify(v ?? null));
      };
      try {
        switch (url.pathname) {
          case "/__mock/qr":
            return json(req.method === "POST" ? await this.newQr() : this.qr);
          case "/__mock/state":
            return json({ connected: this.connected, paired: !!this.channel, pairedCount: this.pairedCount, sessionId: this.sessionId });
          case "/__mock/received":
            return json(this.received);
          case "/__mock/session/start":
            return json({ session_id: await this.startSession() });
          case "/__mock/session/end":
            return json(await this.endSession(body as Partial<SessionSummary>));
          case "/__mock/answer":
            return json({ answer_id: await this.streamAnswer(body as StreamAnswerOptions) });
          case "/__mock/transcript":
            return json(await this.transcript(String(body.text ?? "How long does onboarding take?")));
          case "/__mock/drop":
            this.socket?.terminate();
            return json({ ok: true });
        }
      } catch (e) {
        res.writeHead(500);
        return void res.end(String(e));
      }
      res.writeHead(404);
      return void res.end();
    }
    if (!this.opts.staticDir) {
      res.writeHead(404);
      return void res.end();
    }
    const root = resolve(this.opts.staticDir);
    let file = normalize(join(root, decodeURIComponent(url.pathname)));
    if (!file.startsWith(root)) {
      res.writeHead(403);
      return void res.end();
    }
    if (existsSync(file) && statSync(file).isDirectory()) file = join(file, "index.html");
    if (!existsSync(file) && existsSync(`${file}.html`)) file = `${file}.html`;
    if (!existsSync(file)) {
      res.writeHead(404);
      return void res.end("not found");
    }
    res.writeHead(200, { "content-type": MIME[extname(file)] ?? "application/octet-stream" });
    createReadStream(file).pipe(res);
  }
}
