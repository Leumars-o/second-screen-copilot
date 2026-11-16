// Usage: tsx mock-server/cli.ts [--port 4710] [--host 127.0.0.1] [--static <dir>]
// Prints the pairing QR JSON (paste it into the phone app). Control API: /__mock/* (see server.ts).
import { parseArgs } from "node:util";
import { MockPc } from "./server";

const { values } = parseArgs({
  options: { port: { type: "string", default: "4710" }, host: { type: "string", default: "127.0.0.1" }, static: { type: "string" } },
});

const pc = await new MockPc({
  port: Number(values.port),
  host: values.host,
  ...(values.static ? { staticDir: values.static } : {}),
  log: (m) => console.log(`[mock-pc] ${m}`),
}).start();

console.log(`[mock-pc] listening on http://${values.host}:${pc.port}${values.static ? ` (serving ${values.static})` : ""}`);
console.log(`[mock-pc] QR: ${JSON.stringify(pc.qr)}`);

const stop = () => void pc.close().then(() => process.exit(0));
process.on("SIGINT", stop);
process.on("SIGTERM", stop);
