import { defineConfig, devices } from "@playwright/test";

const PORT = 4710;

export default defineConfig({
  testDir: "e2e",
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 60_000,
  reporter: [["list"]],
  use: {
    ...devices["Pixel 7"],
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: "retain-on-failure",
  },
  // The mock PC serves the static export and the WebSocket on one origin (a secure context).
  webServer: {
    command: `pnpm exec tsx ../../packages/protocol-ts/mock-server/cli.ts --port ${PORT} --static out`,
    url: `http://127.0.0.1:${PORT}/__mock/state`,
    reuseExistingServer: false,
    stdout: "pipe",
    timeout: 30_000,
  },
});
