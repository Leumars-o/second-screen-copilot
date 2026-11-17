import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { generate, OUT_DIR } from "../scripts/generate";

describe("generated code", () => {
  it("is up to date with contracts/protocol (run `pnpm generate` if this fails)", async () => {
    const out = await generate();
    for (const [file, content] of Object.entries(out)) {
      expect(readFileSync(join(OUT_DIR, file), "utf8"), file).toBe(content);
    }
  }, 30_000);
});
