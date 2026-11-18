// E2E against the mock PC (packages/protocol-ts/mock-server), real crypto over a real WebSocket:
// pair -> stream an answer -> expand -> pin -> summary, plus memory, follow-up and render timing.
import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

interface Received {
  type: string;
  seq: number;
  session_id: string | null;
  payload: Record<string, unknown> & { action?: string; item?: Record<string, unknown> };
}

const mock = {
  post: async (api: APIRequestContext, path: string, body: unknown = {}) => (await api.post(`/__mock/${path}`, { data: body })).json(),
  received: async (api: APIRequestContext): Promise<Received[]> => (await api.get("/__mock/received")).json(),
  commands: async (api: APIRequestContext) => (await mock.received(api)).filter((e) => e.type === "command").map((e) => e.payload),
};

test.describe.configure({ mode: "serial" });

let page: Page;
/** CSP violations seen by the page (event listener) or logged to the console, for the whole run. */
const cspViolations: string[] = [];

test.beforeAll(async ({ browser }) => {
  const context = await browser.newContext({ permissions: ["clipboard-read", "clipboard-write"] });
  page = await context.newPage();
  page.on("console", (m) => {
    if (/Content[- ]Security[- ]Policy/i.test(m.text())) cspViolations.push(`console: ${m.text()}`);
  });
  await page.exposeFunction("__cspViolation", (v: string) => cspViolations.push(v));
  await page.addInitScript(() => {
    document.addEventListener("securitypolicyviolation", (e) =>
      (window as unknown as { __cspViolation: (v: string) => void }).__cspViolation(`${e.violatedDirective} ${e.blockedURI}`),
    );
    const w = window as unknown as { __vib: unknown[] };
    w.__vib = [];
    Object.defineProperty(navigator, "vibrate", { value: (p: unknown) => (w.__vib.push(p), true), configurable: true });
  });
});

test("pair -> stream -> expand -> pin -> summary", async ({ request }) => {
  await test.step("pair by pasting the QR text", async () => {
    const qr = await (await request.get("/__mock/qr")).json();
    await page.goto("/");
    await expect(page.getByTestId("pair-screen")).toBeVisible();
    await page.getByText("No camera? Paste the code").click();
    await page.getByTestId("device-name").fill("E2E Pixel");
    await page.getByTestId("qr-text").fill(JSON.stringify(qr));
    await page.getByTestId("pair-submit").click();
    await expect(page.getByTestId("status")).toHaveText(/Connected · Wi-Fi/);
    const hello = (await mock.received(request)).find((e) => e.type === "pair.hello")!;
    expect(hello.payload.device_name).toBe("E2E Pixel");
    expect(hello.seq).toBe(0);
    // Connection details: the QR's addresses and how the attempt went.
    await page.getByTestId("status").click();
    await expect(page.getByTestId("conn-attempts")).toContainText(/Wi-Fi · open after \d+ ms/);
    await page.getByTestId("status").click();
    await expect(page.getByTestId("conn-details")).toHaveCount(0);
  });

  let answerId = "";
  await test.step("stream an answer", async () => {
    await mock.post(request, "session/start");
    await mock.post(request, "transcript", { text: "Walk me through onboarding?" });
    await expect(page.getByTestId("transcript")).toHaveText("Walk me through onboarding?");
    answerId = (await mock.post(request, "answer", { count: 5, intervalMs: 30, urgency: "high" })).answer_id;
    const card = page.getByTestId("answer-current");
    await expect(card).toHaveAttribute("data-state", "done");
    await expect(card.getByTestId("cues").locator("li")).toHaveCount(5);
    await expect(card.getByTestId("fact")).toContainText("99.9% uptime SLA");
    await expect(card.getByTestId("fact")).toContainText("pricing-2026.pdf");
    expect(await page.evaluate(() => (window as unknown as { __vib: unknown[] }).__vib)).toEqual([[200, 80, 200, 80, 200]]);
    // First-paint timing goes back as a feedback command with delta_ms.
    await expect
      .poll(async () => (await mock.commands(request)).find((c) => c.action === "feedback" && c.item?.kind === "timing"))
      .toMatchObject({ answer_id: answerId, item: { stage: "phone_rendered" } });
  });

  await test.step("tap to expand one cue at a time", async () => {
    const card = page.getByTestId("answer-current");
    await card.getByTestId("cue-0").click();
    await expect(card.getByTestId("cue-expand-0")).toContainText("Agree scope");
    await card.getByTestId("cue-1").click();
    await expect(card.getByTestId("cue-expand-1")).toBeVisible();
    await expect(card.getByTestId("cue-expand-0")).toHaveCount(0);
  });

  await test.step("pin, rate and follow up", async () => {
    await page.getByTestId("act-pin").click();
    await expect(page.getByTestId("act-pin")).toHaveText("Unpin");
    await page.getByTestId("act-up").click();
    await expect.poll(async () => (await mock.commands(request)).filter((c) => c.action === "pin")).toEqual([{ action: "pin", answer_id: answerId }]);
    await expect
      .poll(async () => (await mock.commands(request)).find((c) => c.item?.kind === "rating"))
      .toEqual({ action: "feedback", answer_id: answerId, item: { kind: "rating", value: "up" } });
    // Every command carries the active session id.
    const sessionId = (await (await request.get("/__mock/state")).json()).sessionId;
    expect((await mock.received(request)).filter((e) => e.type === "command").every((e) => e.session_id === sessionId)).toBe(true);

    await page.getByTestId("followup-input").fill("What about SSO?");
    await page.getByTestId("followup-send").click();
    await expect
      .poll(async () => (await mock.commands(request)).find((c) => c.action === "followup"))
      .toEqual({ action: "followup", answer_id: answerId, text: "What about SSO?" });
    // The mock answers the follow-up with a new card; the pinned one moves to "Pinned".
    await expect(page.getByTestId("answer-current")).not.toHaveAttribute("data-answer-id", answerId);
    await expect(page.locator(`[data-answer-id="${answerId}"]`)).toContainText("Pinned");
  });

  await test.step("session ends -> summary, copy email, confirm contact", async () => {
    await mock.post(request, "session/end");
    await expect(page.getByTestId("summary-screen")).toBeVisible();
    await expect(page.getByTestId("summary-text")).toContainText("Globex wants SSO");
    await expect(page.getByTestId("action-items").locator("li")).toHaveCount(2);
    await page.getByTestId("copy-email").click();
    await expect(page.getByTestId("copy-email")).toHaveText("Copied");
    expect(await page.evaluate(() => navigator.clipboard.readText())).toContain("Thanks for the time today");
    await page.getByTestId("contact-save").click();
    await expect(page.getByTestId("contact-done")).toHaveText("Saved");
    await expect.poll(async () => (await mock.commands(request)).find((c) => c.action === "contact.confirm")).toMatchObject({ accept: true });
  });
});

test("memory browser: list, edit, delete", async ({ request }) => {
  await page.getByTestId("tab-memory").click();
  await expect(page.getByTestId("memory-item")).toHaveCount(3);
  await page.getByText("Dana Lee").click();
  await page.getByTestId("memory-body").fill("CTO at Globex. Needs SSO by Q1.");
  await page.getByTestId("memory-save").click();
  await expect(page.getByTestId("memory-screen")).toContainText("Needs SSO by Q1.");
  const update = (await mock.commands(request)).find((c) => c.action === "memory.update")!;
  expect(update.item).toMatchObject({ layer: "contact", title: "Dana Lee", body: "CTO at Globex. Needs SSO by Q1." });
  expect(update.item!.id).toMatch(/^[0-9a-f-]{36}$/);

  const first = page.getByTestId("memory-item").first();
  await first.getByTestId("memory-delete").click();
  await first.getByTestId("memory-delete").click(); // confirm
  await expect(page.getByTestId("memory-item")).toHaveCount(2);
  expect((await mock.commands(request)).filter((c) => c.action === "memory.delete")).toHaveLength(1);
});

test("render: 200 streamed deltas, each applied in < 16 ms", async ({ request }) => {
  await page.getByTestId("tab-live").click();
  await page.evaluate(() => {
    (window as unknown as { __ssc: { deltaTimings: number[] } }).__ssc.deltaTimings.length = 0;
  });
  const words = Array.from({ length: 200 }, (_, i) => (i % 20 === 19 ? `w${i}\n` : `w${i} `));
  const { answer_id } = await mock.post(request, "answer", { deltas: words, intervalMs: 2, end: { cues: [], not_in_docs: true } });
  const card = page.locator(`[data-answer-id="${answer_id}"]`);
  await expect(card).toHaveAttribute("data-state", "done");
  await expect(card.getByTestId("not-in-docs")).toHaveText("Not in your docs");
  await expect(card.getByTestId("stream-cue")).toHaveText(words.join("").trimEnd(), { useInnerText: false });
  const timings = await page.evaluate(() => (window as unknown as { __ssc: { deltaTimings: number[] } }).__ssc.deltaTimings);
  expect(timings).toHaveLength(200);
  const max = Math.max(...timings);
  console.log(`delta apply: max ${max.toFixed(3)} ms, mean ${(timings.reduce((a, b) => a + b, 0) / timings.length).toFixed(3)} ms`);
  expect(max).toBeLessThan(16);
});

test("PWA: manifest and service worker", async ({ request }) => {
  const manifest = await (await request.get("/manifest.webmanifest")).json();
  expect(manifest).toMatchObject({ display: "standalone", start_url: "/" });
  expect(manifest.icons.map((i: { sizes: string }) => i.sizes)).toEqual(expect.arrayContaining(["192x192", "512x512"]));
  for (const i of manifest.icons) expect((await request.get(i.src)).headers()["content-type"]).toBe("image/png");
  await expect.poll(() => page.evaluate(async () => !!(await navigator.serviceWorker.getRegistration())?.active)).toBe(true);
});

test("CR-15: keys sealed in IndexedDB, nothing in localStorage", async () => {
  const ls = await page.evaluate(() => JSON.stringify({ ...localStorage }));
  expect(ls).not.toContain("k_pc_to_phone");
  expect(ls).not.toContain("ssc.pairing.v1");
  const rec = await page.evaluate(
    () =>
      new Promise<{ wrapExtractable: boolean; ivLen: number; ctLen: number; plain: boolean }>((resolve, reject) => {
        const r = indexedDB.open("ssc-phone");
        r.onerror = () => reject(r.error);
        r.onsuccess = () => {
          const store = r.result.transaction("kv").objectStore("kv");
          const k = store.get("wrap-key.v1");
          const p = store.get("pairing.v1");
          p.onsuccess = () =>
            resolve({
              wrapExtractable: (k.result as CryptoKey).extractable,
              ivLen: p.result.iv.byteLength,
              ctLen: p.result.ct.byteLength,
              plain: new TextDecoder().decode(p.result.ct).includes("k_pc_to_phone"),
            });
        };
      }),
  );
  expect(rec).toMatchObject({ wrapExtractable: false, ivLen: 12, plain: false });
  expect(rec.ctLen).toBeGreaterThan(100);
});

test("CR-15: strict CSP present and no violation during the whole flow", async ({ request }) => {
  const csp = await page.locator('meta[http-equiv="Content-Security-Policy"]').getAttribute("content");
  expect(csp).toContain("default-src 'self'");
  expect(csp).toMatch(/script-src 'self' 'wasm-unsafe-eval'( 'sha256-[A-Za-z0-9+/=]+')+;/);
  expect(csp).not.toContain("unsafe-inline");
  expect(csp).not.toMatch(/'unsafe-eval'/); // only 'wasm-unsafe-eval' (libsodium's WebAssembly)
  expect(csp).toContain("object-src 'none'");
  expect(csp).toContain("base-uri 'none'");
  expect(csp).toMatch(/connect-src 'self' wss:/);
  // frame-ancestors and the other headers can't go in <meta>; they ship in _headers.
  const headers = await (await request.get("/_headers")).text();
  expect(headers).toContain("frame-ancestors 'none'");
  expect(headers).toContain("X-Content-Type-Options: nosniff");
  // Reload under the policy too (pairing restored from IndexedDB, reconnects).
  await page.reload();
  await expect(page.getByTestId("status")).toHaveText(/Connected · Wi-Fi/);
  expect(cspViolations).toEqual([]);
  // The detector is live: a cross-origin image is blocked and reported.
  await page.evaluate(() => {
    const img = document.createElement("img");
    img.src = "https://example.invalid/x.png";
    document.body.append(img);
  });
  await expect.poll(() => cspViolations.some((v) => v.startsWith("img-src"))).toBe(true);
});

// Last: it replaces the pairing the earlier tests use.
test("CR-19: opening a pairing link (camera scan) pairs in one step", async ({ request }) => {
  const qr = await (await request.post("/__mock/qr")).json(); // a fresh, unused code
  const b64url = Buffer.from(JSON.stringify(qr)).toString("base64url");
  const before = (await mock.received(request)).filter((e) => e.type === "pair.hello").length;
  const dialogs: string[] = [];
  page.once("dialog", (d) => (dialogs.push(d.message()), void d.accept())); // already paired: asks first
  await page.goto("about:blank"); // a camera scan is a fresh page load, not a hash change
  await page.goto(`/#pair=${b64url}`);
  await expect(page.getByTestId("status")).toHaveText(/Connected · Wi-Fi/);
  expect(new URL(page.url()).hash).toBe("");
  const hellos = (await mock.received(request)).filter((e) => e.type === "pair.hello");
  expect(hellos.length).toBe(before + 1);
  expect(dialogs).toEqual(["Pair with this PC? It replaces your current pairing."]);
});

test("CR-19: a pairing link opened in the app's own tab (hash change only) also pairs", async ({ request }) => {
  const qr = await (await request.post("/__mock/qr")).json();
  const b64url = Buffer.from(JSON.stringify(qr)).toString("base64url");
  const before = (await mock.received(request)).filter((e) => e.type === "pair.hello").length;
  page.once("dialog", (d) => void d.accept());
  await page.evaluate((h) => (location.hash = h), `#pair=${b64url}`);
  await expect.poll(async () => (await mock.received(request)).filter((e) => e.type === "pair.hello").length).toBe(before + 1);
  await expect(page.getByTestId("status")).toHaveText(/Connected · Wi-Fi/);
  expect(new URL(page.url()).hash).toBe("");
});

test("CR-20: a screen answer shows the answer and its working as free text", async ({ request }) => {
  await mock.post(request, "answer", {
    format: "screen",
    trigger: "hotkey",
    deltas: [
      { block: "cue", text: "C — the arrow from A points to box C\n" },
      { block: "detail", text: "1: The arrow leaves A and ends at C's left edge.\n" },
      { block: "detail", text: "2: B and D have no incoming arrows.\n" },
    ],
    end: { cues: [] },
  });
  const card = page.getByTestId("answer-current");
  await expect(card).toHaveAttribute("data-state", "done");
  await expect(card.locator(".chip").first()).toHaveText("Screen");
  await expect(card.getByTestId("stream-cue")).toHaveText("C — the arrow from A points to box C\n");
  await expect(card.getByTestId("stream-detail")).toContainText("2: B and D have no incoming arrows.");
  await expect(card.getByTestId("cues")).toHaveCount(0);
});
