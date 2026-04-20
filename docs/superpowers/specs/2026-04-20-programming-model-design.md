# zeroship programming model — Cloudflare-Workers-mirror refactor

**Date:** 2026-04-20
**Status:** design approved, ready for implementation plan
**Scope:** one-release big-bang rewrite — no backward compatibility, no codemod, no dual surface. The project is pre-publication; we're cutting cleanly.

---

## TL;DR

Rewrite zeroship's programming model to mirror Cloudflare Workers exactly. One handler contract:

```ts
export default {
  fetch(request, env, ctx): Response | Promise<Response>
}
```

All platform primitives (`db`, `kv`, `storage`, `auth`, `meter`) move off `globalThis.zeroship.*` and live on `env`, accessed via `import { env } from 'zeroship'` (no ALS ceremony — `env` is a module singleton). Per-request things (`waitUntil`, user identity, abort signal) are free functions that read the kernel's current-request state.

The Rust kernel loses its two-path dispatch (`dispatch_rpc` + `dispatch_http`) and exposes one primitive: `call_fetch_handler(request, env, ctx)`. The `"use server"` RPC sugar moves entirely into bootstrap JS that the compiler injects — the kernel no longer knows what RPC is. Third-party frameworks (Hono, Elysia, h3, tRPC) drop in unmodified because they produce the same WinterTC-shaped `{fetch}` default export.

**Net code change**: roughly −950 LOC from the kernel (runtime/init/dispatch/worker), +200 back for the new primitive, +350 new bootstrap JS. Runtime dispatch paths shrink from two to one; error-surface sites drop from four to three. Some new Rust goes into the control plane (secrets/vars CRUD) and compiler (bootstrap injection), but those are new features, not refactor overhead.

---

## Why

**Current state** (after the 2026-04-19 stability audit):
- Two kernel dispatch paths — `dispatch_rpc` (URL-path RPC, `__rpc` registry, DISPATCH_JS, auto-SSE) and `dispatch_http` (`onRequest` → Response).
- `DispatchOutcome` has 7 variants; worker & serve each handle all 7 with duplicated error logic.
- Platform primitives live on `globalThis.zeroship.*` as native bindings (db, kv, storage, auth, meter).
- Every runtime-level bug we fix (B3/B4/B5 for status propagation, for example) has to be fixed in two places.

**Problems this creates:**
1. **Duplication tax** — every dispatch concern (error handling, cancellation, cpu accounting, logging, streaming) is implemented twice.
2. **Ecosystem isolation** — third-party frameworks output `{fetch}`; zeroship doesn't understand it natively. Hono users have to write `export const onRequest = (req) => app.fetch(req)` adapters.
3. **Mental-model fragmentation** — "is my code an RPC function or an HTTP handler?" is a forced choice at the top of the file; AI-generated apps pick one and lose access to the other.
4. **Kernel bloat** — ~800 LOC of Rust for dispatch-path logic that could be 100 LOC of JS bootstrap.

**Why mirror Cloudflare Workers specifically:**
- WinterTC's Minimum Common API covers globals but *not* the handler shape; Cloudflare's `{fetch(req, env, ctx)}` is the de-facto standard across Workers, Deno Deploy, Bun, and Vercel Edge.
- Every major modern JS HTTP framework outputs this shape by default.
- `env` + `ctx` cleanly separates per-deployment bindings from per-request state — a distinction zeroship's current model blurs.
- Creators configure secrets/vars in a dashboard; CF's convention for surfacing that config to code (as `env.STRIPE_KEY`) is well-understood.

---

## Programming model (user-facing)

### Handler shape

```ts
export default {
  fetch(request: Request, env: Env, ctx: ExecutionContext): Response | Promise<Response>,

  // Future handlers (same shape, attached to the same default export):
  scheduled?(event: ScheduledEvent, env: Env, ctx: ExecutionContext): void,  // crons
  queue?(batch: MessageBatch, env: Env, ctx: ExecutionContext): void,         // queues
  email?(message: EmailMessage, env: Env, ctx: ExecutionContext): void,       // inbound email
}
```

### The two idioms

**Idiom A — named async functions** (`"use server"` directive; AI-generated 95% of the time):

```ts
"use server";
import { env } from 'zeroship';

export async function listUsers() {
  return env.DB.query('SELECT * FROM users ORDER BY name');
}

export async function* streamChat(prompt: string) {
  const stream = await fetch('https://api.openai.com/...');
  for await (const chunk of stream.body) yield { token: decode(chunk) };
}
```

- Auto-wired by the compiler: registered as `__rpc["src/api/users/listUsers"]`, etc.
- Callable from the client via a vite-plugin-generated stub that issues `POST /_rpc/src/api/users/listUsers` with a JSON args array.
- Async generators automatically stream as SSE (`event: yield` per yield, `event: return` / `event: error` on terminate).
- Client stub for async-gen returns `AsyncIterable<T>` that parses the SSE.

**Idiom B — HTTP handler** (third-party frameworks, power users, webhooks):

```ts
import { Hono } from 'hono';
import { env } from 'zeroship';

const app = new Hono();
app.get('/stripe/webhook', async (c) => {
  // env.STRIPE_KEY is user-configured
  const event = verifyStripeSignature(await c.req.text(), env.STRIPE_KEY);
  // ... handle
  return c.text('ok');
});

export default app;  // Hono's default export IS {fetch}
```

**Mixing is allowed** — same app can have `"use server"` exports and a `default { fetch }` handler. The bootstrap router checks `/_rpc/*` first, falls through to the user's `fetch` for everything else.

### `env` — platform bindings + creator config

`env` is a **module-level singleton**. Same object reference every request. Access via either:
- Import: `import { env } from 'zeroship';`
- Handler parameter: `fetch(request, env, ctx)` (CF-compat; same object as the import).

**Contents:**

```ts
type Env = {
  // Platform bindings — auto-injected, always present
  DB: DatabaseBinding;         // zeroship-pg handle
  KV: KVBinding;                // key-value
  STORAGE: StorageBinding;      // object storage
  METER: MeterBinding;          // billing counter

  // Creator-configured — from zeroship.toml + dashboard
  // (typed; worker-configuration.d.ts generated from config)
  STRIPE_KEY?: string;           // secret
  OPENAI_API_KEY?: string;      // secret
  FEATURE_X?: string;           // var
  // ... arbitrary user-defined bindings
}
```

**Never on globalThis.** The `zeroship.*` namespace is fully removed.

### `ctx` — per-request execution context

```ts
type ExecutionContext = {
  waitUntil(promise: Promise<unknown>): void;
  passThroughOnException(): void;  // no-op in zeroship (kept for CF compat)
}
```

- `ctx.waitUntil(p)` — keeps the request alive (for cleanup/logging/notifications) until `p` settles or a wall timeout fires. Does not delay the response to the client. Cancelled waitUntil promises do NOT abort on client disconnect (per CF spec — opt-in work that outlives the response). Budget: configurable per-app via existing `wall_timeout` (matches CF's 30s default for user-invisible work).
- `ctx.passThroughOnException()` — no-op stub; CF uses it to fall through to origin on exceptions, which zeroship doesn't have.

### Free-function access for nested modules

Inside `"use server"` functions, SDK internals, or anything that doesn't see the fetch signature, the `zeroship` module exposes per-request primitives as free functions:

```ts
import { env, waitUntil, getRequest } from 'zeroship';

export async function createOrder(data: OrderInput) {
  const order = await env.DB.insert(data);             // env = module singleton
  const request = getRequest();                          // lookup via kernel op
  const user = await getUser(request);                  // auth SDK
  waitUntil(sendConfirmationEmail(order, user.email));  // lookup via kernel op
  return order;
}
```

Implementation: `waitUntil` / `getRequest` internally call a native op `__zs_get_request_ctx()` that reads the kernel's `executing_request_id` (already tracked per-request for cpu/cancel accounting). Outside a request → throws. No JS-level `AsyncLocalStorage` needed; the kernel already propagates request context across await boundaries via the pump loop.

### SDK layer

One pattern, applied to every first-party SDK. The user-facing API is unchanged for `db`/`kv`/`storage`/`meter`; only internals change.

```ts
// @zeroship/db — internal
import { env } from 'zeroship';

export function createDb<S extends Schema>(schema: S) {
  validateSchema(schema);  // once, at module init
  return new Proxy({} as TypedDb<S>, {
    get(_, table: string) {
      return {
        find: (opts) => env.DB.query(buildSelect(table, opts)),
        insert: (row) => env.DB.query(buildInsert(table, row)),
        // ...
      };
    }
  });
}
```

**User code unchanged:**

```ts
import { createDb } from '@zeroship/db';
const db = createDb({ users: { name: { type: String } } });
const all = await db.users.find();  // same as today
```

**Breaking SDK change — only `@zeroship/auth`**: user identity is per-request, so the signature gains a `request` parameter:

```ts
// before
const user = await getUser();  // read gateway-injected global

// after
import { getUser } from '@zeroship/auth';
const user = await getUser(request);          // pass request explicitly, OR
const user = await getUser();                  // no-arg: reads current request via kernel op
```

Inside a `"use server"` function, the no-arg form works (same mechanism as `waitUntil`). Inside a `fetch` handler, pass `request` explicitly — more readable and avoids the kernel-op lookup.

### Third-party framework compat

Zero adapters required:

```ts
// Hono — works as-is
import { Hono } from 'hono';
const app = new Hono();
app.get('/', c => c.text('hi'));
export default app;  // {fetch(req, env, ctx)}

// Elysia — works as-is
import { Elysia } from 'elysia';
export default new Elysia().get('/', () => 'hi').compile();

// tRPC — works with their fetch-adapter
import { fetchRequestHandler } from '@trpc/server/adapters/fetch';
export default {
  fetch(request, env, ctx) {
    return fetchRequestHandler({ endpoint: '/trpc', req: request, router: appRouter });
  }
};
```

Any ecosystem package that targets WinterTC / Workers / Deno Deploy / Bun targets zeroship.

---

## Architecture

### Three layers

```
┌──────────────────────────────────────────────────────────┐
│ User code (AI-generated or hand-written)                 │
│   export default { fetch(req, env, ctx) }                │
│   "use server" functions (auto-routed to /_rpc/*)        │
│   import { env, waitUntil, getRequest } from 'zeroship'  │
│   import { createDb } from '@zeroship/db'                │
└──────────────────────────────────────────────────────────┘
                           ▲
┌──────────────────────────────────────────────────────────┐
│ Bootstrap JS (injected at bundle top by compiler)        │
│   Exports:                                               │
│     • default { fetch(req, env, ctx) } — the router      │
│   Modules provided:                                      │
│     • 'zeroship'          — env, waitUntil, getRequest   │
│     • 'zeroship/internal' — __bindRequest, __rpc, ...    │
│   Responsibilities:                                      │
│     • route /_rpc/<method> to __rpc[method]              │
│     • wrap async-gen returns as SSE Response             │
│     • catch + format all JS-thrown errors                │
│     • delegate non-RPC paths to user's fetch             │
│     • bind/unbind per-request ctx around every call      │
└──────────────────────────────────────────────────────────┘
                           ▲
┌──────────────────────────────────────────────────────────┐
│ Rust kernel (runtime crate)                              │
│   One public primitive:                                  │
│     Runtime::call_fetch_handler(request, env, ctx)       │
│       -> FetchOutcome                                    │
│   Native ops exposed to JS:                              │
│     __zs_env()                  — return env JSON        │
│     __zs_get_request_ctx()      — {requestId, ctx}       │
│     __zs_bind_request_ctx(ctx)  — stash on current req   │
│     __zs_fetch, __zs_ws_*, ...  — existing native IO ops │
│   Owns: V8 isolate, compio event loop, WebSocket,        │
│          fetch client, crypto, streams, timers, cpu/wall │
│          limits, request-id tracking, pump loop.         │
│   Does NOT know: RPC, "use server", __rpc, DISPATCH_JS,  │
│                   async-gen SSE, handler contracts.      │
└──────────────────────────────────────────────────────────┘
```

### Kernel surface

**Before:**

```rust
impl Runtime {
    pub fn dispatch_rpc(&self, method, args_json) -> Result<RequestResult, String>;
    pub fn dispatch_start(&self, method, args_json, user_json) -> DispatchOutcome;
    pub fn dispatch_http(&self, method, url, headers, body, user_json) -> DispatchOutcome;
}

enum DispatchOutcome {
    Complete(Result<RequestResult, DispatchError>),
    Pending { rx, cancel },
    HttpComplete { status, headers, body, logs },
    HttpStream { status, headers, body, logs },
    HttpPending { rx, cancel },
    WebSocketUpgrade { ws_id, headers },
    // ...
}
```

**After:**

```rust
impl Runtime {
    pub fn call_fetch_handler(
        &self,
        request: HttpRequest,      // method, url, headers, body
        env: &EnvSnapshot,         // module-singleton env (JSON-encoded once at boot)
        ctx: RequestCtx,           // request_id, waitUntil list, cancel flag
    ) -> FetchOutcome;
}

enum FetchOutcome {
    Response { status, headers, body },         // sync complete
    Stream   { status, headers, body_reader },  // chunked streaming
    Pending  { rx, cancel },                    // promise not settled yet
    WebSocketUpgrade { ws_id, headers },        // 101 Switching Protocols
}
```

Four variants, one entry point. `dispatch_rpc`, `dispatch_start`, `dispatch_http`, `DISPATCH_JS`, `__rpc` — all removed.

### Bootstrap (new JS module, ~300 LOC total)

Conceptually equivalent to:

```ts
// @zeroship/runtime-bootstrap — injected at bundle top by the compiler
import userDefault from './user-entry.js';
import { __rpc } from '@zeroship/rpc-registry';  // populated by vite AST transform
import { __bindRequest } from 'zeroship/internal';

const env = Object.freeze(__zs_env());  // snapshot once at module init

export default {
  async fetch(request, _envParam, ctx) {
    __bindRequest(ctx);
    try {
      const url = new URL(request.url);
      if (url.pathname.startsWith('/_rpc/')) {
        return await handleRpc(request, url.pathname.slice(6));
      }
      if (userDefault?.fetch) {
        return await userDefault.fetch(request, env, ctx);
      }
      return new Response('Not Found', { status: 404 });
    } catch (err) {
      return errorResponse(err);
    } finally {
      __bindRequest(null);
    }
  },
  scheduled: userDefault?.scheduled,
  queue: userDefault?.queue,
  email: userDefault?.email,
};

async function handleRpc(request, methodPath) {
  const fn = __rpc[methodPath];
  if (!fn) throw Object.assign(new Error(`Method not found: ${methodPath}`), { status: 404 });

  const bodyText = await request.text();
  let args;
  if (!bodyText) args = [];
  else {
    let parsed;
    try { parsed = JSON.parse(bodyText); }
    catch { throw Object.assign(new Error('Invalid args JSON'), { status: 400 }); }
    if (parsed == null) args = [];
    else if (Array.isArray(parsed)) args = parsed;
    else throw Object.assign(new Error('RPC args body must be a JSON array'), { status: 400 });
  }

  const result = fn.apply(null, args);

  if (result != null && typeof result === 'object'
      && typeof result[Symbol.asyncIterator] === 'function'
      && typeof result.next === 'function'
      && typeof result.return === 'function') {
    return sseFromAsyncGen(result);
  }
  if (result instanceof Response) return result;

  const value = result && typeof result.then === 'function' ? await result : result;
  return Response.json(value);
}

function sseFromAsyncGen(gen) {
  const enc = new TextEncoder();
  const stream = new ReadableStream({
    async pull(controller) {
      try {
        const { value, done } = await gen.next();
        if (done) {
          controller.enqueue(enc.encode('event: return\ndata: {}\n\n'));
          controller.close();
          return;
        }
        controller.enqueue(enc.encode(`event: yield\ndata: ${JSON.stringify(value)}\n\n`));
      } catch (e) {
        controller.enqueue(enc.encode(
          `event: error\ndata: ${JSON.stringify({ message: e?.message ?? String(e) })}\n\n`
        ));
        controller.close();
      }
    }
  });
  return new Response(stream, { headers: { 'content-type': 'text/event-stream' } });
}

function errorResponse(err) {
  const status = Number.isInteger(err?.status) && err.status >= 400 && err.status < 600
    ? err.status : 500;
  const body = JSON.stringify({
    message: err?.message ?? String(err),
    name: err?.name ?? 'Error',
  });
  return new Response(body, { status, headers: { 'content-type': 'application/json' } });
}
```

### The `zeroship` module

Tiny — ~50 LOC:

```ts
// 'zeroship'
export const env = Object.freeze(__zs_env());

export function waitUntil(promise) {
  const ctx = __zs_get_request_ctx();
  if (!ctx) throw new Error('waitUntil called outside a request');
  ctx.waitUntil(promise);
}

export function getRequest() {
  const ctx = __zs_get_request_ctx();
  if (!ctx) throw new Error('getRequest called outside a request');
  return ctx.request;
}

// 'zeroship/internal' — used only by bootstrap
export function __bindRequest(ctx) { /* native op */ }
```

### Request context (kernel, not ALS)

The Rust kernel already tracks `executing_request_id` across await boundaries (set by the pump loop on every continuation). We extend the existing per-request state with:

```rust
struct PerRequestCtx {
    request_id: u64,
    request: HttpRequest,      // method/url/headers (body is already consumed)
    wait_until: Vec<ResultReceiver<()>>,
    cancel: CancelFlag,
}
```

Native op `__zs_get_request_ctx()` looks up the current `executing_request_id` and returns a JS wrapper. No JS-level AsyncLocalStorage required — V8 microtask continuations stay in the same Rust-tracked request, and `setTimeout` callbacks are scheduled with explicit request-id tags (existing mechanism).

### Request flow (end-to-end)

```
 client
   │ HTTP request
   ▼
 gateway                  (unchanged: JWT validate, CHWBL, proxy to worker)
   │
   ▼
 worker/handler.rs        one dispatch(): builds EnvSnapshot + RequestCtx, calls
   │                      Runtime::call_fetch_handler(request, env, ctx)
   ▼
 runtime/call_fetch       enters V8, sets executing_request_id, invokes
   │                      globalThis.__zs_bootstrap.default.fetch(req, env, ctx)
   ▼
 bootstrap.fetch          router: /_rpc/* → handleRpc, else → userDefault.fetch
   │
   ├── /_rpc/<method> ──► __rpc[method].apply(null, args)
   │                       → plain value     → Response.json
   │                       → async generator → sseFromAsyncGen
   │                       → Response        → passthrough
   │
   └── other ──► userDefault.fetch(request, env, ctx)
                  → Response / Promise<Response>
                  → 101 with .webSocket → WebSocketUpgrade
```

---

## Error model

One path, one shape. Every error becomes a `Response`:

```
Body:    {"message": "...", "name": "Error"}
Status:  err.status ?? 500 (clamped 400–599; otherwise 500)
Headers: content-type: application/json
```

**Status-code sources** — only three sites decide HTTP status:

| Where | Cases | Status |
|---|---|---|
| **Bootstrap `handleRpc`** catch | Method not found; JSON parse fail; non-array body; user fn throws | 404 / 400 / 400 / `err.status ?? 500` |
| **Bootstrap `default.fetch`** top-level catch | User's `fetch()` throws uncaught | `err.status ?? 500` |
| **Kernel `call_fetch_handler`** | Isolate not init; CPU limit; wall timeout; bundle missing | 503 / 503 / 504 / 404 |

The B3/B4/B5 double-fix tax (one fix in kernel DISPATCH_JS, one in `dispatch_http`) disappears — one location for every status-code concern.

**Cancellation** — exposed as `request.signal` (standard fetch `AbortSignal`, matching CF convention — CF does not put `signal` on `ExecutionContext`). Bound to the kernel's `CancelFlag`: client disconnect / wall timeout flips the flag, the signal fires, in-flight `fetch` / `db.query` cancel via compio's io_uring abort path. Already wired in; only the surface changes.

---

## Files & crates affected

| File / crate | Delta |
|---|---|
| `crates/runtime/src/runtime.rs` | Replace `DispatchOutcome` (7 variants) with `FetchOutcome` (4); remove `dispatch_rpc`, `dispatch_start`, `dispatch_http`; add `call_fetch_handler`; collapse two-phase async dispatch to one. **−600 LOC, +150 LOC.** |
| `crates/runtime/src/init.rs` | Delete `DISPATCH_JS`, `__rpc` bootstrap scaffolding. Keep `setup_globals`. **−150 LOC.** |
| `crates/runtime/src/dispatch.rs` | Simplify return-value detection (no more RPC-specific tags). **−80 LOC.** |
| `crates/runtime/src/state.rs` | Extend per-request state with `request_ctx` (request snapshot + waitUntil list). **+50 LOC.** |
| `crates/runtime/src/{fetch,streams,websocket,url,crypto}.rs` | Mostly untouched — native IO primitives stay. Rename `zeroship.db/kv/storage/auth/meter` global bindings to internal `__zs_*` ops. |
| `crates/runtime/tests/rpc.rs` | Rewrite: call `call_fetch_handler` with a constructed `Request` targeting `/_rpc/<method>`. |
| `crates/runtime/tests/http.rs` | Rewrite similarly; delete now-redundant dual-path tests. |
| `crates/runtime/tests/bootstrap.rs` | **New** — tests the bootstrap JS (RPC routing, SSE wrap, error shapes). |
| `crates/runtime/tests/wintertc_mca.rs` | **New** — contract suite for MCA globals. |
| `crates/worker/src/handler.rs` | Collapse `dispatch()` + `http_dispatch()` into one `dispatch()`. **−130 LOC.** |
| `crates/gateway/` | No changes — gateway already doesn't differentiate RPC from HTTP. |
| `crates/control/` | Add secrets/vars CRUD; per-app encryption-at-rest for secrets. **+300 LOC.** |
| `crates/bundle/` | Add bootstrap module injection at build time. **+100 LOC.** |
| `crates/compiler/` | Emit bootstrap bundle prelude; emit `zeroship` module shim; emit `__rpc` registration calls for `"use server"` exports. **+200 LOC.** |
| `vite-plugin-zeroship` | Same as compiler but for dev mode. Bootstrap injection in dev server. |
| `packages/@zeroship/db` | Switch internals from `zeroship.db` global to `env.DB`. **Surface unchanged.** |
| `packages/@zeroship/kv`, `storage`, `meter` | Same pattern. **Surface unchanged.** |
| `packages/@zeroship/auth` | Surface change: `getUser(request?)`, `requireUser(request?)`. |
| `packages/@zeroship/runtime-bootstrap` | **New** — the bootstrap module + `zeroship` / `zeroship/internal` modules. |
| `examples/*` | Update by hand (~12 files): `onRequest` → `fetch`, `getUser()` inside fetch handlers → `getUser(request)`. |
| `docs/specs/*` | Update `api-design-guidelines.md`, `auth.md` to reflect new surface; delete references to `zeroship.*` globals. |

**Totals** (approximate):

- Kernel (runtime + worker dispatch): **−960 LOC removed, +200 LOC added → net −760 LOC**. This is the refactor's core win.
- New platform features (control-plane secrets, compiler bootstrap injection, `@zeroship/runtime-bootstrap` package): **+600 Rust + ~350 JS**. Not refactor overhead — these are net-new capabilities.
- SDK packages: API-neutral; internals only.
- Examples: ~12 files hand-edited.

---

## Rollout plan

5 PRs, one ships per week. Each is reviewable in isolation; PR 5 is the hard cut.

### PR 1 — kernel cut
- Delete `dispatch_rpc`, `dispatch_start`, `dispatch_http`, `DISPATCH_JS`, `__rpc`, auto-SSE wrapping, all `zeroship.*` native bindings.
- Add `call_fetch_handler(request, env, ctx) -> FetchOutcome` with 4 variants.
- Add `__zs_env`, `__zs_get_request_ctx`, `__zs_bind_request_ctx` native ops.
- Collapse worker + serve dispatch into one path.
- **State after:** kernel compiles and is simpler, but end-to-end is broken (no bootstrap yet).

### PR 2 — bootstrap + `zeroship` module
- New `@zeroship/runtime-bootstrap` package: bootstrap default export + `zeroship` + `zeroship/internal` modules.
- Compiler injection at bundle top.
- **State after:** kernel + bootstrap integrated. A raw `export default { fetch }` app works end-to-end. RPC via `"use server"` doesn't yet, because vite-plugin hasn't been updated.

### PR 3 — SDK updates
- `@zeroship/db`, `kv`, `storage`, `meter`, `auth` point at `env.*`.
- `auth` gains `request?` param.
- **State after:** examples that use SDKs compile against the new surface.

### PR 4 — control-plane secrets/vars
- CRUD API + encryption-at-rest in `crates/control`.
- `zeroship.toml` schema: `[secrets]`, `[vars]`, `[bindings]`.
- **State after:** creators can configure `env.STRIPE_KEY` etc.

### PR 5 — vite-plugin rework
- AST transform for `"use server"` files emits `__rpc.set("<path>", fn)` registrations.
- Bootstrap injection in dev server.
- Client stub emission updated for the new error wire (`err.status` from body, not envelope).
- Rewrite `examples/*` (by hand, ~12 files).
- **State after:** everything works end-to-end, all examples green, benchmarks within budget.

---

## Testing

### Kernel unit tests (`crates/runtime/tests/`)

| Test file | Coverage |
|---|---|
| `call_fetch_handler.rs` (new) | Four `FetchOutcome` variants covered; timeout / CPU-limit / isolate-not-init → kernel-synthesized Response |
| `native_ops.rs` (extend) | `__zs_env`, `__zs_get_request_ctx`, `__zs_bind_request_ctx` |
| `rpc.rs` (rewrite) | RPC paths via `call_fetch_handler` with constructed `/_rpc/<method>` Request |
| `http.rs` (rewrite) | HTTP handler paths via `call_fetch_handler` |
| `streams.rs` (unchanged) | ReadableStream / TextDecoder / etc. |
| `websocket.rs` (unchanged) | WebSocketPair upgrade |

### Bootstrap tests (new — JS-land, runs inside a test runtime)

- `/_rpc/*` dispatch happy path + all error cases (method not found 404, parse 400, non-array 400, user throws preserves `err.status`)
- Async-generator → SSE framing (yield / return / error events, proper close on terminate)
- Non-RPC path falls through to user `fetch`
- No user `fetch` + non-RPC path → 404
- WebSocketPair 101 response bubbles up as `WebSocketUpgrade`

### WinterTC MCA contract suite (new)

One file, ~30 tests, one per required MCA API. Runs on every PR; gates releases.

- `fetch`, `Request`, `Response`, `Headers`
- `URL`, `URLPattern`, `URLSearchParams`
- `ReadableStream`, `WritableStream`, `TransformStream`
- `TextEncoder`, `TextDecoder`
- `AbortController`, `AbortSignal`
- `EventTarget`, `CustomEvent`
- `atob`, `btoa`, `structuredClone`
- `setTimeout`, `clearTimeout`, `setInterval`, `clearInterval`, `queueMicrotask`
- `FormData`, `Blob`, `File`
- `performance.now`, `performance.timeOrigin`
- `console.*`

Same file could in principle run against Cloudflare / Deno / Bun — that's the point of having it.

### E2E (`tests/e2e_platform.sh`)

Update 20 existing tests for new wire (`err.status` from body). Add:
- **Hono drop-in**: `export default new Hono().get('/', c => c.text('hi'))` end-to-end
- **Elysia drop-in**: same
- **Mixed app**: `"use server"` functions + custom `fetch` handler in the same module

### Perf regression (`crates/runtime/benches/zeroship-bench.rhai`)

- `fetchEcho`, `jsonEcho`, `noop` — **budget 10–15% regression** on RPC path due to bootstrap router indirection. Anything worse blocks the PR.
- SSE throughput (tokens/sec) — must match or exceed today's number.
- 200K req/s pipeline target holds at ≥50 workers.

### Example smoke (runs on every PR)

- `examples/openai-demo` — async-gen SSE
- `examples/hr-system` — db-heavy
- `examples/todo-demo` — classic CRUD
- `examples/hono-app` (new) — third-party framework drop-in

---

## Non-goals

- **No `@zeroship/context` package** — replaced by `zeroship` module exports (`env`, `waitUntil`, `getRequest`).
- **No ALS implementation** — the kernel's existing request-id tracking is sufficient; Node-compat `AsyncLocalStorage` can be added later as a shim on top of native request-id tracking, but it's not part of this refactor.
- **No scheduled/queue/email handlers yet** — the `{fetch, scheduled?, queue?, email?}` default-export shape is reserved so these attach cleanly later, but implementing them is Phase 3.
- **No WebSocket protocol changes** — zeroship's existing WebSocketPair + `Response(null, { status: 101, webSocket })` pattern is already CF-compatible; untouched.
- **No bundle format changes** — `.appbundle` format unchanged; compiler just injects bootstrap at bundle top.
- **No gateway changes** — gateway continues to be HTTP-agnostic; all routing decisions are made in the worker's bootstrap.

---

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| Perf regression > 15% on RPC hot path | Budgeted 10–15%; if exceeded, profile the bootstrap router and consider inlining `handleRpc` into `call_fetch_handler` as a last resort (but only as a fallback — the whole point is kernel simplicity). |
| Third-party framework compat breaks on edge cases (Hono with WebSocket, tRPC with streaming) | Dedicated drop-in tests for Hono + Elysia in E2E suite. tRPC streaming explicitly out of scope for this refactor — tracked as follow-up. |
| `"use server"` method-path collisions across modules | Vite-plugin uses full path-based keys (`src/api/users/createUser`); collisions become a build-time error. |
| Secrets exposed in logs / error messages | `env` is frozen; SDK serializers opt in to `toJSON()` that redacts secret-typed values; logging SDK filters `env` from serialization. |
| Client disconnect mid-`waitUntil` leaks work | `waitUntil` promises run independently of client; cancelled only by wall timeout (same budget as the request). |
| `getUser()` no-arg form inside nested callbacks loses request context | Request-id tracking is propagated by the kernel across all await/setTimeout boundaries — same mechanism already used for `zeroship.auth.getUser()` today. Contract tests cover nested-module and deep-await cases. |

---

## Open questions

None blocking. All design decisions locked in:

- [x] AI-first programming model (named async functions as the primary idiom)
- [x] Kernel-agnostic RPC (bootstrap-injected userland router)
- [x] `export default { fetch(req, env, ctx) }` handler contract (Cloudflare Workers mirror)
- [x] `env` as module singleton + handler parameter (both equivalent)
- [x] `waitUntil` / `getRequest` as free functions; `getUser(request?)`
- [x] No ALS; kernel request-id tracking is sufficient
- [x] Big-bang rewrite; no backward compat; no codemod (greenfield, pre-publication)
- [x] 5-PR rollout sequence

---

## References

- [WinterTC Minimum Common API](https://min-common-api.proposal.wintertc.org)
- [Cloudflare Workers Runtime APIs — `env`, `ctx`](https://developers.cloudflare.com/workers/runtime-apis/handlers/fetch/)
- `docs/reviews/2026-04-19-runtime-stability.md` — audit that motivated collapsing the two dispatch paths
- `docs/specs/api-design-guidelines.md` — the 10 AI-friendly API principles this refactor upholds
