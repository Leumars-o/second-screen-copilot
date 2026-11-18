// Runs before the static assets (wrangler.jsonc: run_worker_first). The PWA needs a secure
// context for the camera, WebCrypto and the service worker, so plain http:// is redirected, and
// HSTS makes the browser go straight to https:// next time.
export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.protocol === "http:") {
      url.protocol = "https:";
      return Response.redirect(url.toString(), 301);
    }
    const res = await env.ASSETS.fetch(request);
    const out = new Response(res.body, res);
    out.headers.set("Strict-Transport-Security", "max-age=31536000");
    return out;
  },
};
