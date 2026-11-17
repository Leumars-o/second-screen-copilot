import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.toml" },
      // Low, slow-refilling frame limit so the rate-limit test doesn't depend on machine speed.
      miniflare: { bindings: { RATE_FRAMES_PER_SEC: "1", RATE_BURST_FRAMES: "20" } },
    }),
  ],
});
