// URL Shortener — demonstrates `env.KV` bindings.
//
// `env` comes from the zeroship module singleton (populated by the
// platform at startup from zeroship.toml + creator-configured bindings).
// KV is always present as a platform binding; no setup needed.

import { env } from "zeroship";

export default {
  async fetch(request) {
    const url = new URL(request.url);
    const path = url.pathname;
    const method = request.method;

    // POST /shorten — request body is the target URL (plain text).
    if (method === "POST" && path === "/shorten") {
      const target = (await request.text()).trim();
      if (!/^https?:\/\//.test(target)) {
        return Response.json({ error: "URL must start with http(s)://" }, { status: 400 });
      }
      const code = crypto.randomUUID().slice(0, 8);
      await env.KV.set(`url:${code}`, target);
      await env.KV.set(`clicks:${code}`, "0");
      return Response.json({ code, short: `https://short.app/${code}`, target });
    }

    // GET /stats/:code — return counters without redirecting.
    if (method === "GET" && path.startsWith("/stats/")) {
      const code = path.slice("/stats/".length);
      const target = await env.KV.get(`url:${code}`);
      if (!target) return new Response("Not Found", { status: 404 });
      const clicks = Number(await env.KV.get(`clicks:${code}`)) || 0;
      return Response.json({ code, url: target, clicks });
    }

    // GET /:code — resolve, count the click, redirect.
    if (method === "GET" && path.length > 1) {
      const code = path.slice(1);
      const target = await env.KV.get(`url:${code}`);
      if (!target) return new Response("Not Found", { status: 404 });
      const clicks = Number(await env.KV.get(`clicks:${code}`)) || 0;
      await env.KV.set(`clicks:${code}`, String(clicks + 1));
      return Response.redirect(target, 302);
    }

    return Response.json({
      routes: ["POST /shorten", "GET /:code", "GET /stats/:code"],
    });
  },
};
