// Post-build CSP for the static export (CR-15). `next.config.ts` headers() do not apply to
// `output: "export"`, so the policy ships two ways:
//   1. a <meta http-equiv="Content-Security-Policy"> as the first child of <head> in every HTML
//      file, with the sha256 of each of that file's inline scripts (Next's RSC bootstrap);
//   2. out/_headers (Cloudflare Pages / Netlify format) with the same policy (hashes of all pages)
//      plus what a <meta> cannot carry: frame-ancestors and the other security headers.
// connect-src defaults to 'self' + any wss: host, because the LAN sub-domain and the relay are
// per-user. Narrow it at build time: SSC_CSP_CONNECT="wss://*.lan.example.com wss://relay.example.com".
// Usage: node scripts/csp.mjs [outDir]
import { createHash } from "node:crypto";
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const OUT = process.argv[2] ?? "out";
const CONNECT = process.env.SSC_CSP_CONNECT?.trim() || "wss:";

export function policy(hashes, { meta }) {
  const d = [
    "default-src 'self'",
    // 'wasm-unsafe-eval' lets libsodium compile its WebAssembly; it does not allow JS eval().
    `script-src 'self' 'wasm-unsafe-eval' ${[...hashes].sort().map((h) => `'sha256-${h}'`).join(" ")}`.trim(),
    "style-src 'self'",
    "img-src 'self' data: blob:",
    "font-src 'self'",
    `connect-src 'self' ${CONNECT}`,
    "media-src 'self' blob:",
    "worker-src 'self'",
    "manifest-src 'self'",
    "frame-src 'none'",
    "object-src 'none'",
    "base-uri 'none'",
    "form-action 'none'",
  ];
  // Ignored (with a console warning) when delivered via <meta>; header only.
  if (!meta) d.push("frame-ancestors 'none'");
  return d.join("; ");
}

function htmlFiles(dir) {
  return readdirSync(dir).flatMap((n) => {
    const p = join(dir, n);
    if (statSync(p).isDirectory()) return htmlFiles(p);
    return n.endsWith(".html") ? [p] : [];
  });
}

const INLINE = /<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/g;

const all = new Set();
const files = htmlFiles(OUT);
for (const f of files) {
  let html = readFileSync(f, "utf8");
  if (html.includes('http-equiv="Content-Security-Policy"')) throw new Error(`${f}: already has a CSP meta (ran twice?)`);
  const hashes = new Set();
  for (const m of html.matchAll(INLINE)) {
    if (m[1].length) hashes.add(createHash("sha256").update(m[1], "utf8").digest("base64"));
  }
  hashes.forEach((h) => all.add(h));
  const meta = `<meta http-equiv="Content-Security-Policy" content="${policy(hashes, { meta: true })}"/>`;
  // Right after the charset (which must stay first) and before any script or stylesheet, so
  // the policy is in force for everything the page loads.
  const anchor = html.match(/<meta charSet="utf-8"\/>|<head>/i);
  if (!anchor) throw new Error(`${f}: no <head>`);
  const at = anchor.index + anchor[0].length;
  html = html.slice(0, at) + meta + html.slice(at);
  writeFileSync(f, html);
}

const headers = [
  "/*",
  `  Content-Security-Policy: ${policy(all, { meta: false })}`,
  "  X-Frame-Options: DENY",
  "  X-Content-Type-Options: nosniff",
  "  Referrer-Policy: no-referrer",
  "  Permissions-Policy: camera=(self), microphone=(self), geolocation=(), payment=(), usb=()",
  "  Cross-Origin-Opener-Policy: same-origin",
  "",
].join("\n");
writeFileSync(join(OUT, "_headers"), headers);
console.log(`csp: ${files.length} HTML files, ${all.size} inline script hashes, connect-src 'self' ${CONNECT}`);
