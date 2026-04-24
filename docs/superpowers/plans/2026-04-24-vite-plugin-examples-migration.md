# PR 5 — Vite-plugin + Examples Migration

**Goal:** Finish the PR-5 scope from `2026-04-20-programming-model-design.md`: ensure the vite-plugin's AST-level `"use server"` handling matches the new kernel + bootstrap wire, and migrate the remaining old-style `.js` examples to the new programming model.

**Architecture status (as of kernel-cut merge):**
- `sdks/vite-plugin/src/transform.ts` — already emits `__register(name, fn)` side-effects + client-side `__rpcUnary` / `__rpcStream` stubs for `"use server"` exports. Client wire uses JSON-array args and `err.status` from response body.
- `sdks/vite-plugin/src/dev-bootstrap/index.ts` — dev bootstrap is fully implemented: `dispatchRpc`, URL-path `/_rpc/<method>` routing, async-gen → SSE framing, uniform error shape.
- `crates/runtime/` — production bootstrap lives in the runtime itself (PR 2 work landed on master).
- Examples under `examples/<name>/src/index.ts` using `"use server"` — already migrated during PR 2/3.

**What's left:**
- Ad-hoc `.js` demos at `examples/*.js` still use old `onRequest` + globalThis-style APIs. These are the "showcase" single-file demos; rewrite them to the new idiom (`export default { fetch(req, env, ctx) }` or `"use server"` + named exports).
- Remove dead demo files that have been superseded by full-directory examples.
- Add a new `hono-demo/` example to prove third-party-framework drop-in compat (per spec).

**Tech stack:** TypeScript/JavaScript at the example level. No Rust or SDK source changes.

---

## Task V1: Audit + triage old `.js` examples

**Files:** `examples/*.js` + `examples/bench/`

- [ ] **Step 1: List + classify**

```bash
ls examples/*.js examples/bench/*.js 2>/dev/null
```

Current inventory (from `master`):
- `http-handler.js` — classic `onRequest` routing demo → rewrite to `export default { fetch }`.
- `url-shortener.js` — uses globalThis `kv.*` → rewrite to `env.KV`.
- `weather-proxy.js` — external fetch demo → rewrite to `export default { fetch }`.
- `ai-streaming.js` — async-gen streaming → rewrite as `"use server"` with named streaming export.
- `jwt-validator.js` — crypto + auth → rewrite to `export default { fetch }` using env.JWT_SECRET.
- `todo-api.js` — old onRequest CRUD → delete (superseded by `examples/todo-demo/`).
- `multi-module.js` — multi-module demo → delete (covered by `examples/multi-ts/`).
- `test_app.js` — scratch test file → delete.
- `bench/noop.js` — keep as-is (benchmarking ground truth).

Decision: keep 5 rewritten, delete 3.

- [ ] **Step 2: Delete the superseded ones**

```bash
git rm examples/todo-api.js examples/multi-module.js examples/test_app.js
git commit -m "examples: drop superseded single-file demos (todo-api, multi-module, test_app)"
```

---

## Task V2: Rewrite `http-handler.js`

**File:** `examples/http-handler.js`

- [ ] **Step 1: Rewrite**

Keep the same routing behaviour but use the new handler shape:

```js
// HTTP routing demo — new handler contract.
//
// Contract: `export default { fetch(request, env, ctx) }`.
// `env` is the module-singleton bindings object; `ctx` carries the
// cancel signal + waitUntil registrar. We don't touch them here.

export default {
  fetch(request) {
    const url = new URL(request.url);
    const path = url.pathname;
    const method = request.method;

    if (method === "GET" && path === "/") {
      return Response.json({ message: "Welcome to the API" });
    }

    if (method === "GET" && path === "/status") {
      return Response.json({ ok: true, uptime: performance.now() });
    }

    if (method === "POST" && path === "/echo") {
      return request.text().then((body) =>
        new Response(body, {
          status: 200,
          headers: { "content-type": request.headers.get("content-type") ?? "text/plain" },
        }),
      );
    }

    return new Response("Not Found", { status: 404 });
  },
};
```

---

## Task V3: Rewrite `url-shortener.js`

Use `env.KV` (platform binding). No more globalThis `kv`.

```js
import { env } from "zeroship";

export default {
  async fetch(request) {
    const url = new URL(request.url);
    const path = url.pathname;

    // POST /shorten — body is the URL to shorten.
    if (request.method === "POST" && path === "/shorten") {
      const target = (await request.text()).trim();
      if (!/^https?:\/\//.test(target)) {
        return Response.json({ error: "URL must start with http(s)://" }, { status: 400 });
      }
      const code = crypto.randomUUID().slice(0, 8);
      await env.KV.set(`url:${code}`, target);
      await env.KV.set(`clicks:${code}`, "0");
      return Response.json({ code, short: `https://short.app/${code}`, target });
    }

    // GET /:code — resolve + count click + redirect.
    if (request.method === "GET" && path.length > 1) {
      const code = path.slice(1);
      const target = await env.KV.get(`url:${code}`);
      if (!target) return new Response("Not Found", { status: 404 });
      const current = Number(await env.KV.get(`clicks:${code}`)) || 0;
      await env.KV.set(`clicks:${code}`, String(current + 1));
      return Response.redirect(target, 302);
    }

    return Response.json({ routes: ["POST /shorten", "GET /:code"] });
  },
};
```

---

## Task V4: Rewrite `weather-proxy.js`

Demonstrate outbound fetch + SSRF-friendly defaults (the runtime already enforces them).

```js
export default {
  async fetch(request) {
    const url = new URL(request.url);
    const city = url.searchParams.get("city") ?? "SF";

    const upstream = await fetch(
      `https://wttr.in/${encodeURIComponent(city)}?format=%C+%t&m`,
      { headers: { "user-agent": "zeroship-weather-demo/1.0" } },
    );
    if (!upstream.ok) {
      return Response.json({ error: `wttr.in ${upstream.status}` }, { status: 502 });
    }
    const summary = (await upstream.text()).trim();
    return Response.json({ city, summary });
  },
};
```

---

## Task V5: Rewrite `ai-streaming.js`

Server-side streaming via async generator (auto-SSE by bootstrap).

```js
"use server";

/**
 * Streaming counter — yields one token every 100ms.
 * Client stub returns an AsyncIterable<T> that parses the SSE frames.
 */
export async function* tick(count = 5) {
  for (let i = 1; i <= count; i++) {
    await new Promise((r) => setTimeout(r, 100));
    yield { n: i, ts: Date.now() };
  }
}

/** Demonstrates throwing mid-stream — shows up as `event: error`. */
export async function* failHalfway() {
  yield { n: 1 };
  yield { n: 2 };
  throw new Error("boom");
}
```

---

## Task V6: Rewrite `jwt-validator.js`

Stateless JWT verification using `env.JWT_SECRET`.

```js
import { env } from "zeroship";

async function hmacSha256(keyBytes, msg) {
  const key = await crypto.subtle.importKey(
    "raw", keyBytes, { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"],
  );
  const sig = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(msg));
  return new Uint8Array(sig);
}

function b64urlDecode(s) {
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((s.length + 3) % 4);
  return Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
}
function b64urlEncode(bytes) {
  return btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export default {
  async fetch(request) {
    const url = new URL(request.url);
    if (request.method === "POST" && url.pathname === "/sign") {
      const claims = await request.json();
      const header = b64urlEncode(new TextEncoder().encode(JSON.stringify({ alg: "HS256", typ: "JWT" })));
      const payload = b64urlEncode(new TextEncoder().encode(JSON.stringify(claims)));
      const input = `${header}.${payload}`;
      const secret = new TextEncoder().encode(env.JWT_SECRET ?? "dev-secret");
      const sig = b64urlEncode(await hmacSha256(secret, input));
      return Response.json({ token: `${input}.${sig}` });
    }

    if (request.method === "GET" && url.pathname === "/verify") {
      const token = url.searchParams.get("token") ?? "";
      const parts = token.split(".");
      if (parts.length !== 3) return Response.json({ valid: false, reason: "bad shape" }, { status: 400 });
      const [h, p, s] = parts;
      const secret = new TextEncoder().encode(env.JWT_SECRET ?? "dev-secret");
      const expected = b64urlEncode(await hmacSha256(secret, `${h}.${p}`));
      const valid = expected === s;
      if (!valid) return Response.json({ valid: false });
      const claims = JSON.parse(new TextDecoder().decode(b64urlDecode(p)));
      return Response.json({ valid: true, claims });
    }

    return new Response("POST /sign, GET /verify?token=...", { status: 404 });
  },
};
```

---

## Task V7: Add `examples/hono-demo/`

Third-party framework drop-in (per spec's "Third-party framework compat" section).

**Files:** `examples/hono-demo/package.json`, `examples/hono-demo/src/index.ts`, `examples/hono-demo/vite.config.ts`.

- [ ] **package.json**

```json
{
  "name": "hono-demo",
  "type": "module",
  "dependencies": {
    "hono": "^4.6.0",
    "zeroship": "workspace:*"
  },
  "devDependencies": {
    "@zeroship/vite-plugin": "workspace:*",
    "vite": "^5.4.0"
  }
}
```

- [ ] **src/index.ts**

```ts
import { Hono } from "hono";

const app = new Hono();

app.get("/", (c) => c.text("Hono works on zeroship."));
app.get("/hello/:name", (c) => c.json({ hello: c.req.param("name") }));
app.post("/echo", async (c) => c.json(await c.req.json()));

// Hono's default export IS `{fetch(req, env, ctx)}` — zero adapters.
export default app;
```

- [ ] **vite.config.ts**

```ts
import { defineConfig } from "vite";
import zeroship from "@zeroship/vite-plugin";

export default defineConfig({ plugins: [zeroship()] });
```

---

## Task V8: Final smoke + commit

- [ ] **Step 1: cargo build — nothing should break, these are leaf examples**

```bash
cargo build --workspace --release
```

- [ ] **Step 2: spot-check via `zeroship serve`**

```bash
cargo run -p zeroship -- serve examples/http-handler.js --port 3000 &
curl -s http://localhost:3000/status | jq .
kill %1
```

- [ ] **Step 3: Commit each example in its own commit so reverting is trivial**

```bash
git add examples/http-handler.js && git commit -m "examples: http-handler migrated to fetch(req, env, ctx)"
git add examples/url-shortener.js && git commit -m "examples: url-shortener migrated to env.KV bindings"
git add examples/weather-proxy.js && git commit -m "examples: weather-proxy migrated to fetch(req, env, ctx)"
git add examples/ai-streaming.js && git commit -m "examples: ai-streaming migrated to use server + async generators"
git add examples/jwt-validator.js && git commit -m "examples: jwt-validator migrated to env.JWT_SECRET"
git add examples/hono-demo/ && git commit -m "examples: hono-demo — third-party framework drop-in"
```

---

## Out of scope

- tRPC / Elysia examples (spec mentions them; they're the same story as Hono — add later if creators request).
- Production CI smoke of every example (`tests/e2e_platform.sh` uses its own tmp files).
- Docs site rewrite — the examples change is a stepping stone.
