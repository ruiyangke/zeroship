# RPC — Seamless server functions

**Status:** Proposal · **Wire version:** `/_zs/v1/`

## What this fixes

Six structural flaws had been bleeding into the design: method name = file path (refactor breaks the wire), no argument validation, no idempotency story, no method-level auth/rate-limit metadata, two duplicated dispatch paths, and a bespoke streaming format that doesn't interop with the AI-SDK ecosystem. See the critique in conversation history for full inventory.

This design fixes all six while making the authoring surface **simpler**, not more ceremonious. A creator writes `export async function add(...)` and calls `await add(...)` from the client — the transport is invisible.

---

## Goals & non-goals

**Goals**

- Server functions are imported and called like any other function — no wrappers, no `useQuery` ceremony for the 80% case.
- The wire identity of every method is **stable across refactors**.
- Args validated at the boundary; the handler never sees malformed input.
- Auth, rate-limit, idempotency, caching are *declarative metadata*, enforced by the gateway before the worker touches the request.
- Streaming uses the same wire convention as Vercel `ai-sdk` and the React ecosystem — `useChat`, `useCompletion` work out of the box.
- Typed TS client surface generated from the source (Phase 3). Non-TS clients call the wire directly via plain HTTP+JSON; structured codegen for other languages is out of scope for the initial release (revisit when there's real demand — likely via OpenAPI emission rather than hand-rolled per-language generators).

**Non-goals (initial release)**

- Server-to-server bidirectional streams (gRPC bidi) — defer.
- Schema federation across apps — defer.
- Persistent / Relay-style cursors at the protocol layer — apps roll their own.

---

## 1. Authoring — two levels, one wire

### Level 1: just export a function (the 80% case)

```ts
// src/server/todos.ts            ← path-based: src/server/** (or src/server.{ts,js}) is server-only
import { user } from "@zeroship/server";
import { db } from "@zeroship/db";

export async function list({ limit = 20, cursor }: {
  limit?: number;
  cursor?: string;
}) {
  return db.todos.find({ ownerId: user().id, limit, cursor });
}

export async function add({ text }: { text: string }) {
  return db.todos.insertOne({ ownerId: user().id, text });
}
```

```tsx
// src/client/Todos.tsx
import { list, add } from "../server/todos";   // build rewrites this import on the client
const todos = await list({ limit: 50 });
await add({ text: "buy milk" });
```

That's it. No `defineRpc`, no `.useQuery()`. Just two files, two imports, two calls.

**What the build does:**

| Bundle | Each `export async function fn(...)` becomes |
| --- | --- |
| Server | Body kept verbatim; synthetic entry imports it and exposes via `default.rpc(name, input, ctx)` |
| Client | Body replaced with `(args) => __rpcCall("<wireId>", args)`; full TS types preserved |

**What gets inferred:**

| Inferred property | Rule |
| --- | --- |
| `kind: "query"` | Function name matches `/^(get|list|find|search|count|read)/` |
| `kind: "mutation"` | Default for the rest |
| `kind: "stream"` | `async function*` (generator) |
| `wireId` | Build-assigned, stable across renames (see §2) |
| `auth` | From `$config` if set, else `defineApp({ rpc: { defaults } })`, else `"anon"` with build warning |

**Path is the only marker.** A file is server-only iff it lives at `src/server.{ts,tsx,js,jsx}` or anywhere under `src/server/`. The legacy `"use server"` directive is no longer accepted — discovery is purely path-based.

### Level 2: add metadata when you need it

Functions are objects. Attach `.config`:

```ts
import { z } from "@zeroship/server";

export async function chargeCard({ amount, customerId }: ChargeArgs) {
  // ...
}
chargeCard.config = {
  idempotent: true,                          // requires Idempotency-Key on the wire
  auth:       "user",                        // overrides module default
  rateLimit:  { rpm: 10, per: "user" },
  input:      z.object({                     // runtime validation at the wire boundary
    amount:     z.number().int().positive(),
    customerId: z.string(),
  }),
  middleware: ["transaction"],               // pre-handler middleware (Phase 5)
};
```

For async-iterator streams or alternate kinds, set `kind` explicitly:

```ts
import { z } from "@zeroship/server";

export async function* search({ query }: { query: string }) {
  for await (const chunk of llm.stream(query)) yield { chunk };
}
search.config = {
  kind:   "stream",
  input:  z.object({ query: z.string() }),
  output: z.object({ chunk: z.string() }),
};
```

Or set defaults at module scope:

```ts
export const $config = {
  auth:      "user",
  rateLimit: { rpm: 600, per: "user" },
};
```

**Resolution order** (most specific wins): `fn.config` → module `$config` → `defineApp({ rpc: { defaults } })` from `src/server/config.ts` → built-in defaults.

---

## 2. Wire identity — stable across refactors

Every procedure has a `wireId` — a string the network sees. Wire identity is a pure function of current source: explicit ids win, the bare export name is the default, and production builds require every procedure to declare its id.

### How it's assigned

Resolution order — highest priority first:

1. **Creator-declared `fn.config.id`** (required for production):
   ```ts
   list.config = { id: "todos.list" };
   ```
2. **Default — bare `<exportName>`.** `export async function listTodos()` becomes `rpc:listTodos`. No path-derived slug. The wire never carries file structure.

After every procedure has been assigned a wireId, two cross-cutting checks run:

3. **Collision check.** Two procedures that resolve to the same wireId — typically two files exporting `add` with no explicit `id` — are an unrecoverable collision. The build fails with both file paths and instructs the user to pin an explicit `id` on at least one of them.
4. **Production-mode gate.** Any procedure that landed at step 2 (the bare-name default with no explicit `id`) is rejected by `vite build --mode production`. Production wire identity must be explicit.

There is no previous-build state. WireIds are derived fresh from current source on every build, which keeps identity easy to audit (read the source, you have the wire) and makes refactors visible (rename an export and the wire changes — the build refuses in production until you pin `fn.config.id` to whatever stable id you want shipped).

### Production rule

`vite build --mode production` requires every procedure to have an explicit `id`. Missing ids are a build error. Pin them once when promoting a procedure to a stable wire surface; the explicit id is then the source of truth and survives any later renames of the export.

### URL shape

```
/_zs/v1/{wireId}
```

`v1` is the protocol version. `v2` is reserved for breaking wire changes; both can coexist during a deprecation window.

---

## 3. Ambient context

The seamless model needs per-request context without threading it through every signature. Backed by `__zs_bind_request_ctx` / `__zs_get_request_ctx` (already present in `crates/runtime/src/init.rs`); the synthetic entry binds before invoking the user function.

```ts
import { user, request, env, log, waitUntil, idempotencyKey } from "@zeroship/server";

export async function add({ text }: { text: string }) {
  const me = user();                           // throws Unauthenticated if no session
  log.info("adding todo", { userId: me.id });
  const todo = await db.todos.insertOne({
    ownerId: me.id,
    text,
    idempotencyKey: idempotencyKey(),          // undefined if not provided
  });
  waitUntil(analytics.track("todo.added", { id: todo.id }));
  return todo;
}
```

| Helper | Returns | Throws |
| --- | --- | --- |
| `user()` | `User` | `UNAUTHENTICATED` if not signed in |
| `userOrNull()` | `User \| null` | never |
| `request()` | `Request` | never (constructs lazily; not free) |
| `env` | merged `vars + secrets` | never |
| `log.{info,warn,error}` | `void` | never |
| `waitUntil(p)` | `void` | never |
| `idempotencyKey()` | `string \| undefined` | never |
| `signal()` | `AbortSignal` | never |
| `traceId()` | `string` | never |

Single request per concurrent invocation in V8 isolates → no AsyncLocalStorage gymnastics; the slot is keyed by the kernel's in-flight request id.

---

## 4. Cross-boundary types

The wire is JSON. Plain `JSON.stringify` loses `Date`, `BigInt`, `Map`, `Set`, `URL`, `Uint8Array`, `RegExp`. This design uses **superjson** by default:

```ts
// Server
export async function nextDeadline() {
  return new Date(Date.now() + 86_400_000);   // returns Date
}

// Client
const d = await nextDeadline();                // typed Date, runtime Date
d.toISOString();                               // works
```

Wire (superjson):
```json
{
  "json": "2026-05-01T00:00:00.000Z",
  "meta": { "values": { "": ["Date"] } }
}
```

Cost: ~2 KB client lib, 5–10% wire overhead, sub-millisecond per call. Opt-out per app via `defineApp({ rpc: { transformer: "json" } })` in `src/server/config.ts` — disables superjson; types lie about Date but the wire is smaller.

### Validation

Validation runs against Zod schemas at runtime. Procedures opt into validation by setting `fn.config.input` / `fn.config.output` to Zod schemas. Procedures without schemas pass arguments through unchecked — the Level 1 trust model: typed clients are expected to send well-formed data; if you can't trust the caller, declare a schema.

Output validation runs in dev mode only (cheap dev-time correctness check); skipped in production for hot-path performance. To enable in production, set `defineApp({ rpc: { strictOutput: true } })`.

For multi-language clients (Python, Swift, mobile native), there's no codegen in the initial release — call the wire directly with JSON over HTTP. If demand materializes later, the right path is OpenAPI emission (`zod-to-openapi` → `openapi-generator-cli` ecosystem), not hand-rolled per-language generators.

### Type generation for the client

Build emits `virtual:zeroship/server-api.d.ts` containing the typed surface of every server export, with the transformer's input/output mapping applied. Client `import { list } from "../server/todos"` resolves to:

```ts
declare const list: (input: { limit?: number; cursor?: string }) => Promise<Todo[]>;
```

Tree-shaken; no runtime cost beyond the call site.

---

## 5. Compilation pipeline

The seamless model is wired entirely at build time via Vite 8's Environment API. Same source file, two environments, two transforms, two outputs. The original imports `from "../server/todos"` resolve to different runtime code in each bundle — the wire layer is invisible to the creator.

### The split

```
src/server/todos.ts ──┬── [client env]  → transform-client → STUBS               ──► dist/assets/...
                      │
                      └── [ssr env]     → transform-server → BARE EXPORTS, no   ──► dist/server/index.js
                                                              registration calls    (synthetic entry
                                                                                     assembles the map)
```

The transform plugin keys on `this.environment.name` (`"client"` vs `"ssr"` vs `"zeroship"` for the dev V8 environment). Vite/Rolldown invokes it once per environment per file.

### Source → server output (RPC-only app)

Body kept verbatim. **No appended registration calls** — the synthetic entry assembles all procedures into a static map (see "Synthetic entry assembly" below).

```ts
// transform-server output of src/server/todos.ts
import { user } from "@zeroship/server";
import { db } from "@zeroship/db";

export async function list({ limit = 20 }) {
  return db.todos.find({ ownerId: user().id, limit });
}
export async function add({ text }) {
  return db.todos.insertOne({ ownerId: user().id, text });
}
add.config = { idempotent: true };
```

The user module is just a normal ESM module — exported functions, no side effects. Discovery happens at build time via AST scan; routing happens at module-init time via the synthetic entry's static import map.

### Source → server output (SSR-enabled app)

Same as RPC-only, plus each procedure is wrapped with `__makeServerProcedure` so React components rendering server-side find `.useQuery` / `.prefetch` on each `list` / `add` reference (see §10):

```ts
// transform-server output (SSR-enabled app)
import { user } from "@zeroship/server";
import { db } from "@zeroship/db";
import { __makeServerProcedure } from "@zeroship/rpc/server";

async function _listImpl({ limit = 20 }) {
  return db.todos.find({ ownerId: user().id, limit });
}
async function _addImpl({ text }) {
  return db.todos.insertOne({ ownerId: user().id, text });
}
_addImpl.config = { idempotent: true };

export const list = __makeServerProcedure(_listImpl, { id: "todos.list", kind: "query" });
export const add  = __makeServerProcedure(_addImpl,  { id: "todos.add",  kind: "mutation", idempotent: true });
```

The plugin emits this variant when SSR is enabled (`default.fetch` returns React-rendered HTML, or `defineApp({ rpc: { ssr: true } })`). For pure RPC-only apps the simpler form above is used — saves ~30 bytes and a closure allocation per procedure.

The synthetic entry imports both `list` and `add` (whether they're bare functions or `__makeServerProcedure`-wrapped) and references them in `_procedures`; calling either invokes the underlying handler.

### Source → client output

Body **replaced wholesale**. Original imports (`@zeroship/db`, `@zeroship/auth`) are dropped — they'd never resolve in the browser.

```ts
// transform-client output of src/server/todos.ts
import { __rpcCall, __makeProcedure } from "@zeroship/rpc/client";

export const list = __makeProcedure(
  (input, opts) => __rpcCall("todos.list", input, { kind: "query", ...opts }),
  { id: "todos.list", kind: "query" },
);

export const add = __makeProcedure(
  (input, opts) => __rpcCall("todos.add", input, { kind: "mutation", idempotent: true, ...opts }),
  { id: "todos.add", kind: "mutation", idempotent: true },
);
```

When `src/client/Todos.tsx` does `import { list, add } from "../server/todos"`, the client bundle resolves to these stubs. `await list({ limit: 50 })` becomes `__rpcCall("todos.list", …)` — a `GET /_zs/v1/todos.list?input=…` returning the parsed result.

### Synthetic entry assembly

The vite-plugin generates a single synthetic entry module per server bundle that becomes the bundle's actual entry point. It collects every discovered procedure into a static map and exposes them via `default.rpc()` — mirroring the WinterCG `default.fetch` shape.

```ts
// auto-generated synthetic entry (lives at virtual:zeroship/_server-entry)
import * as _zsUser from "USER_ENTRY";                                    // user's own entry, for default.fetch
import { list as _t_list, add as _t_add } from "./src/server/todos";      // one import per discovered procedure
import { me   as _u_me  }                from "./src/server/users";

const _procedures = {
  "todos.list": _t_list,
  "todos.add":  _t_add,
  "users.me":   _u_me,
};

const _userDefault = (_zsUser && _zsUser.default && typeof _zsUser.default === "object")
  ? _zsUser.default : null;
const _userFetch = _userDefault?.fetch ?? null;

async function _zsRpc(name, input, ctx) {
  const fn = _procedures[name];
  if (!fn) {
    throw Object.assign(new Error("Method not found: " + name), { status: 404 });
  }
  return await fn(input, ctx);
}

async function _zsFetch(request) {
  if (_userFetch) return _userFetch.call(_userDefault, request);
  return new Response("Not Found", { status: 404 });
}

export default { fetch: _zsFetch, rpc: _zsRpc };
```

The map is built at module top-level via static imports — **no side-effect-driven registration**, no virtual `_rpc-registry` module, no `_zsRegister` calls in user modules. The `_procedures` object is closure-scoped within the synthetic entry's module scope; user code can't reach it via `globalThis`, and the resolveId hook denies user lookups of `virtual:zeroship/_server-entry`.

The runtime kernel's bootstrap calls `user.default.rpc(name, input, ctx)` for `/_zs/v1/<wireId>` requests; falls through to `user.default.fetch(request)` for everything else. **Single dispatch path, single export shape, mirrors the WinterCG fetch handler convention exactly.**

### Why types still work

TypeScript checks the **source**, not the transformed output. `tsserver` sees the original `export async function list(…): Promise<…>` regardless of which environment will eventually transform it. Every importer (client or server) gets the original signature. Compilation produces the wire; type-checking ignores it. **No `.d.ts` generation needed for within-app callers.**

For non-TS consumers (mobile, Python, Go, etc.) in the initial release, the wire format is the contract: plain HTTP+JSON over `/_zs/v1/<id>`. No codegen sidecar — call the URL with whatever HTTP client the language ships. Future OpenAPI emission could enable typed cross-language clients, but it's not part of the initial release.

### Per-file transform algorithm

```
1. Parse AST (rolldown's built-in oxc parser).
2. Is this a server module?
     path matches src/server.{ts,tsx,js,jsx}  OR  src/server/**?
     → if no: skip; pass through.
3. Walk top-level exports. Collect each async function / async generator.
     Reject other shapes (classes, plain consts, objects) with a build error.
4. For each export:
     a. Resolve wireId:
        - explicit fn.config.id (if set) → use it
        - else default to bare <exportName>
     b. Resolve metadata: fn.config (including optional `input`/`output`
        Zod schemas) + $config + defineApp({ rpc: { defaults } }) + defaults.
     c. Stash (wireId, kind, metadata) in shared state for manifest emission.
5. Emit transformed code based on this.environment.name:
     client → stub-replace the whole file (drops server-only imports)
     ssr    → keep the body; synthetic entry imports the procedures statically
6. After all files transformed, generate the synthetic entry source from
   the stashed (wireId, importPath, exportName) tuples and serve it via
   the rpcRegistryPlugin's resolveId/load hooks.
```

### Manifest emission (`closeBundle`)

After both environments finish, the plugin walks shared state and emits the unified `resources` block (see §7 for the full shape):

```jsonc
"manifest": {
  "version": 1,
  "transformer": "superjson",
  "resources": {
    "*":              { "auth": "admin" },
    "/_assets/*":     { "static": { "try": ["$path"] }, "cache": { "max_age": 31536000, "immutable": true } },
    "rpc:todos":      { "auth": "user", "override": ["auth"] },
    "rpc:todos.list": { "kind": "query"    },
    "rpc:todos.add":  { "kind": "mutation", "idempotent": true }
  }
}
```

The build emits the resource tree from two inputs: auto-derived entries (RPC procedures discovered via AST scan, static assets from `dist/`, the SSR catch-all) and `defineApp({ resources })` declarations from `src/server/config.ts`. WireIds derive from current source on every build — explicit `fn.config.id` if pinned, otherwise the bare export name. The production-mode gate refuses any procedure that didn't pin an explicit id, so the wire surface of a deployed app is always intentional.

### End-to-end runtime call path

```
client code:                     await list({ limit: 50 })
  ↓ (transform-client replaced the body)
client stub:                     __rpcCall("todos.list", {limit:50}, {kind:"query"})
  ↓ HTTP
GET /_zs/v1/todos.list?input=eyJsaW1pdCI6NTB9
  ↓ gateway:
   1. strip /_zs/v1/ prefix → lookup key = "rpc:todos.list" in manifest.resources
   2. validate auth (Authorization: Bearer ...) per effective policy
   3. enforce rate limit per effective policy
   4. validate max_input_bytes per effective policy
   5. forward to worker
  ↓
worker, kernel calls user.default.rpc("todos.list", args):
   1. lookup fn in registry  ← rolldown-mangled closure-private Map
   2. fn.config.input?.parse(args[0])  ← Zod-direct validation if declared
   3. invoke list({limit:50})  ← original body runs
   4. fn.config.output?.parse(result)  ← dev-only correctness check
  ↓
list runs:                       db.todos.find({ ownerId: user().id, limit: 50 })
  ↓
return [ ... ]                   ← serialized via superjson
  ↓ gateway adds Cache-Control + ETag
client stub returns parsed Promise<Todo[]>
```

### What about partially-server modules?

An earlier transform sketch supported "mixed" files where some functions had `"use server"` and others were pure client utilities. **This design drops that.** A file is either fully server (path matches `src/server.{ts,tsx,js,jsx}` or `src/server/**`) or fully client. Reasoning:

- Simpler mental model (one bundle per file).
- Easier to reason about which symbols leak into which bundle.
- Easier transform (no per-export client/server tagging).

Shared utilities live in non-server files. Build error if a non-async-function export appears in a server module.

### Files involved

| File | Role |
| --- | --- |
| `sdks/vite-plugin/src/transform.ts` | Per-file rewrite |
| `sdks/vite-plugin/src/synthetic-entry.ts` | Generates the bundle entry — static imports of all procedures + `default.{rpc,fetch}` (replaces v1's separate registry module) |
| `sdks/vite-plugin/src/manifest.ts` | Emits `manifest.resources` at `closeBundle` |
| `sdks/vite-plugin/src/rpc-registry.ts` | Closure-private registry virtual module + synthetic SSR entry that wraps `dispatch` to surface INVALID_ARGUMENT envelopes |

The synthetic-entry pattern from the recent cleanup expands its role: instead of just exporting `default.fetch`, it also exports `default.rpc` built from a static map of discovered procedures. EnvSnapshot stays; the closure-private registry virtual module is **retired** — the static map subsumes it.

---

## 6. Wire protocol

### Query — `GET`

```
GET /_zs/v1/{wireId}?input=<base64url-superjson>
Authorization: Bearer <jwt>
Accept: application/json
```

If `input` size > 6 KB (URL header limit), client transparently falls back to `POST` with body. Functionally equivalent.

```
200 OK
Content-Type: application/json
Cache-Control: max-age=30, stale-while-revalidate=60, private
ETag: "a3b1f9c2d4e5f678"
X-Request-Id: 01HJQK2A8R…

{ "json": [...], "meta": {...} }
```

### Mutation — `POST`

```
POST /_zs/v1/{wireId}
Authorization: Bearer <jwt>
Content-Type: application/json
Idempotency-Key: 01HJQK…              ← required if procedure idempotent: true

{ "json": {...}, "meta": {...} }
```

Response: `200 OK` + JSON body. No `ETag` or `Cache-Control`.

### Stream — `POST` + SSE (Vercel AI-SDK Data Stream Protocol)

```
POST /_zs/v1/{wireId}
Accept: text/event-stream
Content-Type: application/json

{ "json": {...} }
```

Response: SSE matching the AI-SDK Data Stream Protocol verbatim. Each line is `<typeId>:<json>\n`:

```
0:"hello "                        ← text part (when output type is string)
0:"world"
2:[{"chunk": "..."}]              ← typed object yields
e:{"code":"...","message":"..."}  ← error event
d:{}                              ← done
```

For procedures whose output is a string: yields go to `0:`. For object output: yields go to `2:`. Errors mid-stream emit `e:` then `d:`. Client async-iterator yields the typed values; AI-SDK hooks consume the same stream untouched.

### Subscription — WebSocket

```
GET /_zs/v1/{wireId}
Connection: Upgrade
Upgrade: websocket
Sec-WebSocket-Protocol: zs.v1
Authorization: Bearer <jwt>
```

Frame format (text JSON):
```
{"t":"hello","input": <json>}            ← client → server, first frame
{"t":"data","value": <json>}             ← server → client, each emit()
{"t":"error","error": <envelope>}
{"t":"end"}
{"t":"ping"}/{"t":"pong"}                ← keepalive
```

### Errors — `application/zs-error+json`

```json
{
  "code":      "INVALID_ARGUMENT",
  "message":   "input.text must be at least 1 character",
  "details":   { "path": ["text"], "expected": "min 1, max 500" },
  "trace_id":  "01HJQK…",
  "retryable": false
}
```

**Code enum** (gRPC-inspired, fixed): `UNAUTHENTICATED`, `PERMISSION_DENIED`, `NOT_FOUND`, `INVALID_ARGUMENT`, `FAILED_PRECONDITION`, `ALREADY_EXISTS`, `RESOURCE_EXHAUSTED`, `ABORTED`, `INTERNAL`, `UNAVAILABLE`, `TIMEOUT`, `CANCELLED`, `OUT_OF_RANGE`, `UNIMPLEMENTED`.

| Code | HTTP status | Retryable |
| --- | --- | --- |
| UNAUTHENTICATED | 401 | no |
| PERMISSION_DENIED | 403 | no |
| NOT_FOUND | 404 | no |
| INVALID_ARGUMENT, FAILED_PRECONDITION, OUT_OF_RANGE | 400 | no |
| ALREADY_EXISTS | 409 | no |
| RESOURCE_EXHAUSTED | 429 | yes |
| ABORTED, CANCELLED | 499 | sometimes |
| TIMEOUT | 504 | yes |
| UNAVAILABLE | 503 | yes |
| INTERNAL, UNIMPLEMENTED | 500 | no |

User code throws via:

```ts
import { RpcError } from "@zeroship/server";
throw new RpcError("NOT_FOUND", "todo not found", { details: { id } });
```

#### Error redaction (production)

In production, anything that is **not** an `RpcError` instance is redacted before it reaches the wire. The wire envelope becomes:

```json
{ "code": "INTERNAL", "message": "Internal server error", "retryable": false }
```

The full error (message, stack, type) is logged structurally by the worker, keyed by `trace_id` so it can be correlated to the wire response.

`RpcError` instances are exempt — the user constructed them for the wire, so their `code`, `message`, `details`, and `retryable` ship verbatim. To force a non-`RpcError` message through redaction (e.g., a Zod parse error message you want surfaced), use the `exposeMessage` flag:

```ts
throw new RpcError("INVALID_ARGUMENT", "Email already in use", { exposeMessage: true });
```

Two global overrides:

- **Dev mode.** `defineApp({ rpc: { dev: true } })` skips redaction entirely. Useful for staging environments where you want production-like routing but readable errors.
- **`NODE_ENV !== "production"`.** Local `vite dev` and SSR-test runs default to dev mode; redaction is off.

Plain `throw new Error("...")` always redacts in production.

### Batching

Multiple queries in the same client tick auto-merge:

```
POST /_zs/v1/_batch
Content-Type: application/zs-batch+json

[ { "id": "a", "name": "todos.list", "input": {"limit":50} },
  { "id": "b", "name": "users.me",   "input": null         } ]
```

Response:
```
[ { "id": "a", "status": 200, "output": {...} },
  { "id": "b", "status": 200, "output": {...} } ]
```

**Only queries batch.** Mutations and streams stay individual (different consistency / streaming semantics). Opt-in per client: `client({ batch: true })`. Default off — batching adds latency for the slowest of the merged calls; explicit opt-in avoids surprises.

---

## 7. Manifest — unified resource tree

The `.zsapp` manifest replaces the previous `rules` + `policies` split with a single **`resources`** block that unifies routing and protection. Every entry is a **resource** — a URL path, an RPC namespace, or an RPC procedure. Each resource carries optional **policy** fields (auth, rate_limit, cors, cache, csrf, idempotent, max_input_bytes, middleware, timeout, publicly_accessible) and an optional **routing action** (redirect, rewrite, static); when no routing action is declared, dispatch falls back to the resource's namespace default (see §7.1).

### Default dispatch

When the gateway resolves a request to a resource, the resource's routing action decides what happens. If no action is declared, the resource's **key namespace** picks the default.

```
1. resource.redirect ?  → 30x with Location header
2. resource.rewrite  ?  → restart matching with rewritten path (hop limit)
3. resource.static   ?  → serve blob from manifest.assets / runtime_assets
4. key starts with `rpc:`  → forward to worker as RPC (default.rpc)
5. key starts with `/`     → forward to worker as SSR (default.fetch)
6. no match              → 404
```

Two namespaces, two default dispatches:

| Namespace | Key format | Default dispatch | Wire URL |
| --- | --- | --- | --- |
| `rpc:` | `rpc:<dotted-id>` (e.g., `rpc:todos.add`) | worker as RPC | `/_zs/v1/<id>` |
| `/` | `/<path>` (e.g., `/api/admin`, `/blog/[slug]`) | worker as SSR (or `static`/`redirect`/`rewrite` if declared) | the path itself |

The `rpc:` prefix appears only in manifest keys — the wire URL is `/_zs/v1/<wireId>` with no `rpc:` in it. The gateway translates: incoming `/_zs/v1/todos.add` → lookup key `rpc:todos.add`.

The bare `*` resource (no prefix, no slash) holds the **root default policy** for inheritance only; it never dispatches.

Every resource — whether it has a routing action or not — carries policy that the gateway enforces around dispatch (auth, rate_limit, csrf, cache headers, etc.).

### Wire shape

```jsonc
{
  "version": 1,
  "worker": { "entry": "index.js", "modules": { "index.js": "sha256:..." } },

  "resources": {
    // ── Root default policy ─────────────────────────────────────────────────
    "*": {
      "auth":       "admin",                             // secure-by-default
      "rate_limit": { "rpm": 60, "per": "ip" }
    },

    // ── URL hierarchy: policy + custom routes co-located ────────────────────
    "/api": {
      "auth": "user", "override": ["auth"],
      "cors": { "allow_origins": ["self"] }
    },
    "/api/admin":          { "auth": "admin", "override": ["auth"] },
    "/api/admin/users":    { "rate_limit": { "rpm": 100, "per": "user" } },
    "/api/public":         { "auth": "anon", "override": ["auth"], "publicly_accessible": true },
    "/api/v1/*":           { "rewrite": "/_zs/v1/*" },

    "/old-blog/[slug]":    { "redirect": { "to": "/blog/[slug]", "status": 302 } },
    "/legacy":             { "redirect": { "to": "/", "status": 301 } },

    "/blog/[slug]":        { "cache": { "max_age": 60, "swr": 300 } },
    "/_assets/*":          { "static": { "try": ["$path"] },
                             "cache": { "max_age": 31536000, "immutable": true } },

    // ── RPC namespaces — keys carry the `rpc:` prefix ───────────────────────
    "rpc:todos":           { "auth": "user", "override": ["auth"],
                             "rate_limit": { "rpm": 600, "per": "user" } },
    "rpc:todos.list":      { "kind": "query"    },
    "rpc:todos.add":       { "kind": "mutation", "idempotent": true },
    "rpc:todos.update":    { "kind": "mutation", "idempotent": true },
    "rpc:todos.delete":    { "kind": "mutation", "auth": "admin", "override": ["auth"] },

    "rpc:billing":         { "auth": "user", "override": ["auth"] },
    "rpc:billing.charge":  { "kind": "mutation", "idempotent": true, "middleware": ["transaction"] }
  },

  "transformer": "superjson",

  "assets":         { ... },
  "runtime_assets": { ... }
}
```

### Resource shape

Each resource entry can carry:

| Family | Fields | Notes |
| --- | --- | --- |
| **Routing action** (at most one) | `redirect`, `rewrite`, `static` | When absent, dispatch is implied by key shape |
| **Policy** | `auth`, `cors`, `cache`, `rate_limit`, `csrf_origins`, `idempotent`, `middleware`, `max_input_bytes`, `timeout`, `publicly_accessible` | Any combination |
| **Override marker** | `override: ["auth", "rate_limit", ...]` | Required when this resource shadows an inherited field |
| **Procedure metadata** (RPC only) | `kind` (`query`/`mutation`/`stream`/`subscription`) | Schemas live on `fn.config.input` / `fn.config.output` at runtime — never on the wire |

A procedure declares its handler timeout via `fn.config.timeout`:

```ts
fn.config = {
  timeout: { ms: 5000 },   // worker handler aborts after 5s
};
```

The default lives in `defineApp({ rpc: { defaults } })` (see §7 Authoring); the gateway sets a request-level deadline and the worker's `ctx.signal` aborts when it expires. On abort, the gateway returns HTTP 504 with `code: "TIMEOUT"`.

The two namespaces have independent matchers — RPC ids use dot-segment globbing (`rpc:todos.*`); URL paths use path-segment globbing (`/api/*`, `/blog/[slug]`). They never collide. See §7.1 above for the dispatch decision flow.

### Authoring — `defineApp` in `src/server/config.ts`

The vite-plugin reads exactly one config file: `<projectRoot>/src/server/config.ts`. It carries (a) the resource tree and (b) app-level RPC defaults that procedures inherit when they don't override. There is no `zeroship.toml` for RPC, no `zeroship.config.ts` at the project root, no per-directory `$config.ts`. If `src/server/config.ts` is absent, `defineApp({})` is implied — auto-derived everything, secure-by-default policy.

```ts
// src/server/config.ts
import { defineApp } from "@zeroship/server";

export default defineApp({
  // App-level RPC defaults — procedures inherit unless they override.
  rpc: {
    defaults: {
      auth:      "user",
      rateLimit: { rpm: 600, per: "user" },
      timeout:   { ms: 30000 },
    },
  },

  resources: {
    "*": {
      auth: "admin",
      rateLimit: { rpm: 60, per: "ip" },
    },

    "/api": {
      auth: "user", override: ["auth"],
      cors: { allowOrigins: ["self"] },
      children: {
        "admin": {
          auth: "admin", override: ["auth"],
          children: { "users": { rateLimit: { rpm: 100, per: "user" } } },
        },
        "public": { auth: "anon", override: ["auth"], publiclyAccessible: true },
        "v1/*":   { rewrite: "/_zs/v1/*" },
      },
    },

    "/old-blog/[slug]": { redirect: "/blog/[slug]" },
    "/blog/[slug]":     { cache: { maxAge: 60, swr: 300 } },

    "rpc:todos": {
      auth: "user", override: ["auth"],
      rateLimit: { rpm: 600, per: "user" },
      children: {
        "add":    { idempotent: true },
        "update": { idempotent: true },
        "delete": { auth: "admin", override: ["auth"] },
      },
    },
  },
});
```

`children: {...}` is authoring sugar; the build flattens to fully-qualified keys, prepending the parent's namespace prefix to each child:

- `"rpc:todos"` + child `"add"` → `"rpc:todos.add"` (dot separator within the `rpc:` namespace)
- `"/api"` + child `"admin"` → `"/api/admin"` (slash separator within the URL namespace)

The wire is the flat map shown above.

### Per-field merge rules

For any resolved resource, walk the most-specific match's parent chain (root `*` → ancestors → matched node) and merge per field:

| Field | Merge | Notes |
| --- | --- | --- |
| `auth` | **stricter wins** (admin > user > anon) | Override marker required to weaken |
| `rate_limit`, `max_input_bytes`, `timeout` | **min** | Stricter cap survives |
| `cors.allow_origins`, `cors.allow_methods`, `cors.allow_headers`, `csrf_origins` | **intersect** | Child can only narrow |
| `cors.allow_credentials`, `cors.max_age_seconds` | **child overrides** | Per-resource decision |
| `middleware` | **append** (root → child order) | All run; child can't drop ancestor's |
| `cache`, `idempotent`, `publicly_accessible` | **child overrides** | Per-resource decision |

The `override: [...]` marker is required when a child weakens or widens an inherited field. Without it the build refuses: *"`todos.delete` declares `auth: user` but inherits `auth: admin` from `todos`. Add `override: [\"auth\"]`."*

### Gateway compilation (at app load)

When the gateway receives `manifest.json`, it pre-computes for every resource an `EffectivePolicy` (flattened via the merge rules above) and stores `HashMap<MatchKey, EffectivePolicy>` keyed for O(1) lookup. Per request:

```
1. Identify namespace from the request:
   - URL begins with `/_zs/v1/`  → strip prefix; lookup key = "rpc:" + remainder
   - Otherwise                    → lookup key = the URL path
2. Match against the resources map (most-specific wins):
   - rpc:  walk dot-segment Trie keyed by "rpc:<id-segments>"
   - URL:  walk path-segment Trie
3. Look up precomputed EffectivePolicy.
4. Enforce pre-dispatch policy:
   - auth check                → 401 / 403 if rejected
   - rate-limit bucket         → 429 if exceeded
   - csrf origin check         → 403 if mutation + Origin not allowed
   - max_input_bytes guard     → 413 if exceeded
   - HTTP method ↔ kind        → 405 if `GET` on `kind: "mutation"`
5. Determine action:
   - `redirect`           → 30x with Location header
   - `rewrite`            → restart matching with rewritten path (hop limit)
   - `static`             → serve blob from manifest.assets / runtime_assets
   - else `rpc:` key      → forward to worker as RPC
   - else URL path        → forward to worker as SSR
6. On response: apply policy.cors headers, policy.cache headers (queries only).
```

The gateway never walks a tree at runtime — `EffectivePolicy` is precomputed once at load and cached. Compilation step is ~30 LOC; per-request lookup is two hash probes.

### What the gateway enforces without involving the worker

- Pre-404 unknown resources.
- Reject wrong HTTP method (procedure `kind: "mutation"` via `GET` → 405).
- Validate `Authorization` for `auth: "user"` / `"admin"` — 401 before forwarding.
- Enforce `rate_limit` per the declared bucket — 429 with `Retry-After` and `X-RateLimit-Reset`.
- Reject oversized inputs (`max_input_bytes`) — 413.
- CORS preflight short-circuit.
- Set `Cache-Control` and `ETag` for queries on the response path.
- Emit Prometheus metrics keyed by resource id.

The worker only sees pre-validated, pre-authenticated, pre-rate-limited requests. **One source of truth** for routing AND policy: the `resources` block.

### Validation

Build-time and gateway-load-time checks:

- **Cycles** — inheritance chain must be acyclic (DAG check).
- **Override marker sanity** — every shadowed field needs `override: [field]`. Build error otherwise.
- **Routing-action exclusivity** — at most one of `redirect`, `rewrite`, `static` per resource.
- **Glob format** — `[name]` single-segment, `[...rest]` multi-segment, `*` wildcard.
- **Secure-by-default** — `auth: "anon"` requires `publicly_accessible: true` on the same resource. Build error in production mode (warning in dev).
- **Procedure kind matches HTTP method** — `kind: "mutation"` cannot be served via `GET`.
- **Resource-key format** — `rpc:` prefix followed by `[a-zA-Z0-9._*-]+` for RPC namespace; `/`-prefixed glob for URL namespace; bare `*` for the root default. Any other shape is a build error.

The existing `Manifest::validate()` in `crates/core/src/types.rs` absorbs these; the unified shape is simpler than the previous `Rule + Match + Action + Cors` validation matrix.

### What this collapses

| Was (previous draft) | Now |
| --- | --- |
| `manifest.rules: Vec<Rule>` (Match + Action + Cors per rule) | gone — folded into `resources` |
| `manifest.policies: { default, overrides }` block | gone — folded into `resources` |
| Two compiled-manifest lookups in the gateway (rules + policies) | one unified lookup |
| `defineApp.routes: [...]` + `defineApp.policy` + `defineApp.overrides` | one `defineApp.resources` |
| Separate rule-level `Cors` and action-level `RateLimit`/`Cache` fields | all migrate into resource policy |

`Match` and `Action` enums survive as the gateway's internal compile target — the public surface is the resource map.

---

## 8. Idempotency

Mutations declare `idempotent: true` to opt in. Effect:

- Wire **requires** `Idempotency-Key` header (gateway returns 400 if missing).
- Server-side dedupe keyed by `(app_id, wireId, idempotency_key)`. Each entry stores `(input_hash, output, status, completed_at)`. Default TTL 24 h, configurable per procedure via `fn.config.idempotencyTtl: { hours: 168 }` (max 7 days).
- Storage: `zeroship.kv` (Redis-backed in prod, in-memory in dev). Each entry costs ~200 B; quota counted against the app's KV allowance.

Mutations without `idempotent: true` may still pass `Idempotency-Key` — the gateway logs it as audit metadata but does not dedupe.

### Collision rules — what happens on key reuse

The `(app_id, wireId, idempotency_key)` tuple has three possible states when a new request arrives. Each is handled deterministically, after Stripe's pattern:

1. **Same key, same input hash, within TTL.** The original request already completed. The gateway returns the stored response — `200 OK` with the cached body, or the cached error envelope if the original failed. The handler is not re-invoked.

2. **Same key, *different* input hash, within TTL.** The caller is reusing the key with a different payload — almost always a bug, never a retry. The gateway rejects:

   ```
   HTTP/1.1 409 Conflict
   Retry-After: <seconds remaining in TTL>
   Content-Type: application/zs-error+json

   {
     "code": "ALREADY_EXISTS",
     "message": "Idempotency-Key was used with a different input within the dedupe window",
     "details": { "reason": "idempotency_key_reused_with_different_input" },
     "retryable": false
   }
   ```

3. **Same key while the original is still in flight.** The second request blocks on a per-key mutex with a short timeout (default 30 s, capped by the procedure's `timeout` policy from §7 — see also §7's per-field merge rules). If the original completes within the wait window, the second request returns its result. If the wait expires first, the second request is rejected with `409 ABORTED`.

`ALREADY_EXISTS` and `ABORTED` are existing entries in §6's error code table (HTTP 409 / 499 respectively). Both are non-retryable as documented there.

---

## 9. End-user surface — clients

### Web / TS (vanilla)

The Level-1 import-and-call surface is the default. Underneath:

```ts
// Generated for SSR / non-Vite consumers
import { client } from "@zeroship/rpc";
import type { App } from "../api";          // type-only

const rpc = client<App>({
  baseUrl: "https://myapp.zeroship.ai",
  auth:    () => getJwt(),
  fetch:   globalThis.fetch,
  batch:   true,
});

const todos = await rpc.todos.list.query({ limit: 50 });
const sub   = rpc.todoChanges.subscribe(undefined, {
  onData: ch => …, onError: e => …, signal: controller.signal,
});
```

### React

```tsx
import { rpcReact } from "@zeroship/rpc/react";
const trpc = rpcReact<App>();

function Todos() {
  const { data, error, isLoading } = trpc.todos.list.useQuery({ limit: 50 });
  const add = trpc.todos.add.useMutation({
    onSuccess: () => trpc.todos.list.invalidate(),
  });
  …
}
```

Built on TanStack Query — invalidation, optimistic updates, suspense, SSR hydration all free.

### Vercel AI-SDK

Because `stream` uses the AI-SDK Data Stream Protocol verbatim:

```tsx
import { useChat } from "ai/react";
const { messages, append } = useChat({
  api: rpc.chat.completion.streamUrl({ model: "claude-opus-4-7" }),
});
```

No bespoke parser; no adapter shim.

### Non-JS clients

Out of scope for the initial release. The wire is plain HTTP+JSON; non-TS callers (Python, Swift, Go, mobile native) hit `POST /_zs/v1/<id>` with a JSON body and parse the JSON response — no SDK required. They lose static typing across the boundary; if/when typed multi-language clients become a real need, the path is OpenAPI emission (`zod-to-openapi` then any `openapi-generator-cli` target), not bespoke per-language generators we maintain.

This deliberately keeps the platform's surface area small. Hand-rolled Python/Swift generators were proposed earlier but discarded — too much language-specific maintenance for speculative demand.

---

## 10. React Query integration

The seamless model and TanStack Query (React Query) compose cleanly: every procedure imported from a server module is **simultaneously a callable function and a hooks object**. Same import, three usage modes.

```tsx
import { list, add } from "../server/todos";

function Todos() {
  const { data } = list.useQuery({ limit: 50 });           // hook
  // or for Suspense + Error Boundaries:
  const { data } = list.useSuspenseQuery({ limit: 50 });

  const addTodo = add.useMutation({                        // mutation hook
    onSuccess: () => list.invalidate(),                    // invalidates list cache
  });

  return <button onClick={() => addTodo.mutate({ text: "hi" })}>+</button>;
}

// Same import works outside React, e.g. in an event handler:
async function exportToCSV() {
  const todos = await list({ limit: 1000 });               // direct call, no hook
  download(toCsv(todos));
}
```

No `trpc.todos.list.useQuery(...)` namespace traversal. No separate `App` type to thread through. The procedure IS the hooks object.

### What `__makeProcedure` does

The client stub from §5 wraps the raw RPC call so the resulting export is callable AND carries the kind-appropriate React Query hooks:

```ts
// @zeroship/rpc/client
export function __makeProcedure(call, meta) {
  // Plain callable (the seamless surface).
  const fn = (input, opts) => call(input, opts);
  fn.id   = meta.id;
  fn.kind = meta.kind;
  fn.queryKey = (input) => [meta.id, input];

  if (meta.kind === "query") {
    fn.useQuery = (input, options) => useQuery({
      queryKey: [meta.id, input],
      queryFn:  () => call(input),
      ...options,
    });

    fn.useSuspenseQuery = (input, options) => useSuspenseQuery({ ... });
    fn.useInfiniteQuery = (input, options) => useInfiniteQuery({
      queryKey: [meta.id, input],
      queryFn:  ({ pageParam }) => call({ ...input, cursor: pageParam }),
      getNextPageParam: (last) => last.nextCursor ?? undefined,
      ...options,
    });

    fn.invalidate = (input) =>
      queryClient.invalidateQueries({ queryKey: input ? [meta.id, input] : [meta.id] });
    fn.prefetch = (input, options) =>
      queryClient.prefetchQuery({ queryKey: [meta.id, input], queryFn: () => call(input), ...options });
    fn.setData = (input, updater) =>
      queryClient.setQueryData([meta.id, input], updater);
  }

  if (meta.kind === "mutation") {
    fn.useMutation = (options) => useMutation({
      mutationKey: [meta.id],
      mutationFn:  (input) => call(input, {
        // Auto-generated per mutate() call; React Query retries reuse it.
        idempotencyKey: meta.idempotent ? generateIdempotencyKey() : undefined,
      }),
      ...options,
    });
  }

  if (meta.kind === "stream") {
    fn.useStream = (input, options) => useStream({ key: [meta.id, input], stream: () => call(input), ...options });
  }

  if (meta.kind === "subscription") {
    fn.useSubscription = (input, options) => useSubscription({ key: [meta.id, input], subscribe: (cb) => call(input, cb), ...options });
  }

  return fn;
}
```

Hooks are attached only for the matching `kind`, so a query never carries `useMutation` and vice versa. Stable query key shape: `[wireId, input]` for queries / streams / subscriptions; `[wireId]` for mutations.

### Setup

One line at the app root:

```tsx
import { ZeroshipProvider } from "@zeroship/rpc/react";
import { QueryClient } from "@tanstack/react-query";

const qc = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 30_000,                    // matches typical resource policy.cache.max_age
      retry: (n, err) => err.retryable && n < 3,
    },
  },
});

<ZeroshipProvider client={qc}>
  <App />
</ZeroshipProvider>
```

`ZeroshipProvider` wraps `QueryClientProvider` and additionally provides the `__rpcCall` transport (auth tokens, base URL, batching opt).

### What you get for free

| TanStack Query feature | How it works |
| --- | --- |
| Stale-while-revalidate | Manifest's `cacheable.swr` becomes default `staleTime` per procedure |
| Optimistic updates | `add.useMutation({ onMutate, onError })` |
| Suspense + Error Boundaries | `list.useSuspenseQuery` |
| Infinite scroll | `list.useInfiniteQuery` for procedures returning `{ items, nextCursor }` |
| Prefetching on hover | `list.prefetch({ limit: 50 })` in `onMouseEnter` |
| SSR hydration | `dehydrate(qc)` server-side, `<HydrationBoundary>` client-side; `wireId` stable across boundaries |
| Persistent cache | `persistQueryClient` plugin works unchanged |
| DevTools panel | Keys show as `["todos.list", { limit: 50 }]` |
| Automatic retries | `retryable: true` errors retry; idempotent mutations retry safely |

### Idempotency × retry interaction

This is the load-bearing detail. React Query retries on transient errors. Without idempotency keys, a mutation that fires `add({ text: "hi" })` then 503s could double-write on retry. With this design:

```tsx
const add = add.useMutation({ retry: 3 });
add.mutate({ text: "hi" });
// Internally: idempotencyKey = uuidv7(); used for ALL retries of THIS mutate() call.
// Server dedupes — second attempt returns the first attempt's stored result.
```

The auto-generated key is bound to the React Query mutation observer's lifetime, **not per HTTP attempt**. Retry semantics become safe by default for any procedure marked `idempotent: true`. Procedures without `idempotent: true` get `retry: 0` automatically — non-idempotent retries surface to the app to handle explicitly.

### Streaming + React Query

TanStack Query v5 has experimental stream-style hooks; until they stabilize we ship a thin `useStream`:

```tsx
const { chunks, isStreaming, error, cancel } = search.useStream({ query: "..." });
// chunks: T[], appended as the server yields
// isStreaming: true until the SSE 'd:' frame
// error: Error | null
// cancel: () => void  (also fires on unmount)
```

For chat UIs: skip our hook and pass `search.streamUrl(input)` to Vercel AI-SDK's `useChat`. Same SSE wire, two consumers.

### Subscriptions

```tsx
const { value, isConnected, error } = todoChanges.useSubscription();
// value: T | undefined  (latest emitted value)
// isConnected: WS connection state
// error: Error | null  (terminal; auto-reconnects on transient drops)
```

Auto-reconnect with exponential backoff. Unmount triggers unsubscribe. No manual lifecycle.

### Server-side rendering

When the worker runs React SSR, the same component code calls `list.useQuery(...)` on both server and client. On the client side, `list` is a `__makeProcedure`-wrapped stub (HTTP). On the server, `list` is the actual function — and **must also expose `.useQuery` / `.prefetch`** (the same hook surface), or render breaks.

For SSR-enabled apps, the server transform wraps each procedure with `__makeServerProcedure` from `@zeroship/rpc/server` (see §5). Server-side hooks call the local function directly (no HTTP); results land in the per-request QueryClient and dehydrate to the client.

```ts
// @zeroship/rpc/server (excerpted)
import { useQuery, useSuspenseQuery } from "@tanstack/react-query";

export function __makeServerProcedure(impl, meta) {
  const fn = (input) => impl(input);                              // direct call, no HTTP
  fn.id = meta.id; fn.kind = meta.kind;
  fn.queryKey = (input) => [meta.id, input];

  if (meta.kind === "query") {
    fn.useQuery = (input, options) =>
      useQuery({ queryKey: [meta.id, input], queryFn: () => impl(input), ...options });
    fn.useSuspenseQuery = (input, options) =>
      useSuspenseQuery({ queryKey: [meta.id, input], queryFn: () => impl(input), ...options });
    fn.prefetch = (input, qc) =>
      qc.prefetchQuery({ queryKey: [meta.id, input], queryFn: () => impl(input) });
  }
  // mutation / stream — see below
  return fn;
}
```

Lifecycle of an SSR request:

1. Worker handles the fetch → builds a per-request `QueryClient`, mounts `<ZeroshipProvider client={qc}>` around the React tree.
2. Components call `list.useQuery(...)` → server-side hook prefetches via `_listImpl` (in-process, no RTT), stores in `qc`.
3. After render, runtime emits `<script>__zs_dehydrated = JSON.parse(...)</script>` with `dehydrate(qc)`'s payload appended to the HTML.
4. Client boots, `<HydrationBoundary state={__zs_dehydrated}>` rehydrates the cache, components render with already-fetched data — no client-side refetch.

Mutations and subscriptions don't render server-side (mutations are user-action triggered; subscriptions need a long-lived connection). Server-side `useMutation` / `useSubscription` resolve to no-ops that throw if invoked during SSR.

### How React Query gets into the bundle

`@zeroship/rpc/client` is **framework-agnostic**: it never statically imports `@tanstack/react-query`. Hooks are attached via getters that read from a side-effect-populated registry. React Query enters the bundle **only when the user imports `@zeroship/rpc/react`**.

```ts
// @zeroship/rpc/client/_hooks.ts  — small, no React deps
export const _hookRegistry: {
  useQuery?:         Function;
  useMutation?:      Function;
  useInfiniteQuery?: Function;
  useSuspenseQuery?: Function;
  useStream?:        Function;
  useSubscription?:  Function;
  queryClient?:      unknown;
} = {};

// @zeroship/rpc/client/index.ts
import { _hookRegistry } from "./_hooks";

export function __makeProcedure(call, meta) {
  const fn = (input, opts) => call(input, opts);
  fn.id = meta.id; fn.kind = meta.kind;
  fn.queryKey = (input) => [meta.id, input];

  if (meta.kind === "query") {
    Object.defineProperty(fn, "useQuery", {
      get() {
        const useQuery = _hookRegistry.useQuery;
        if (!useQuery) throw new Error(
          "Hooks unavailable. Install @zeroship/rpc/react and mount <ZeroshipProvider>.");
        return (input, options) =>
          useQuery({ queryKey: [meta.id, input], queryFn: () => call(input), ...options });
      },
    });
    // ...same pattern for useSuspenseQuery, useInfiniteQuery, prefetch, invalidate, setData
  }
  // ...mutation / stream / subscription
  return fn;
}

// @zeroship/rpc/react/index.ts
import { useQuery, useMutation, useInfiniteQuery, useSuspenseQuery,
         QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { _hookRegistry } from "@zeroship/rpc/client/_hooks";

// Side effect on module evaluation: populate the registry.
_hookRegistry.useQuery         = useQuery;
_hookRegistry.useMutation      = useMutation;
_hookRegistry.useInfiniteQuery = useInfiniteQuery;
_hookRegistry.useSuspenseQuery = useSuspenseQuery;

export function ZeroshipProvider({ client, children }) {
  _hookRegistry.queryClient = client;
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}
```

Procedure objects exist in every client bundle (~50 bytes each — getter scaffolding). React Query (~13 KB min+gzip) enters only when the user imports `@zeroship/rpc/react`.

### When hooks work — full decision table

| Context | Transform output | React Query in bundle? | `list.useQuery(...)` works? |
| --- | --- | --- | --- |
| Client bundle, app imports `@zeroship/rpc/react` + mounts `<ZeroshipProvider>` | `__makeProcedure(...)` | yes (~13 KB) | ✓ |
| Client bundle, app uses only `await list(...)` direct calls | `__makeProcedure(...)` | **no** | throws clear error if accessed |
| Client bundle, non-React app (Vue, Solid, vanilla) | `__makeProcedure(...)` | no | throws if accessed |
| Server bundle (worker), RPC-only app | bare function; synthetic entry's `_procedures` map references it | n/a | n/a — `list` is the actual function on server; direct call only |
| Server bundle (worker), SSR-enabled app | `__makeServerProcedure(...)`; synthetic entry's `_procedures` map references it | yes (server-side React Query) | ✓ via in-process prefetch (no HTTP) |

### Setting `staleTime` per procedure

The resource's `policy.cache.max_age` (when set on a query) becomes the default `staleTime` for that procedure's `useQuery`. Per-call override still works:

```tsx
list.useQuery({ limit: 50 }, { staleTime: 5_000 });   // override the manifest default
```

Aggregate invalidation by prefix:

```tsx
import { rpcInvalidate } from "@zeroship/rpc/react";
await rpcInvalidate("todos.");          // all queries with id starting with "todos."
```

---

## 11. Worked scenarios

### A — creator builds a CRUD todo app

1. Writes `src/server/todos.ts` with `list`, `get`, `add`, `complete`, `delete`. Plain async exports — no wrappers.
2. `add.config = { idempotent: true, id: "todos.add" }` to pin the wireId and make AI-driven retries safe.
3. `vite build` → transform discovers procedures by path (`src/server/**`), infers query/mutation from name, emits `manifest.resources` (auto-derived `rpc:` entries + user `defineApp.resources`); Zod schemas on `fn.config.input`/`output` validate at runtime, writes `client-api.d.ts`.
4. Deploys via `zeroship deploy`. Gateway picks up the new manifest in ≤5 s.

### B — end user (mobile, native iOS) calls the same API

1. iOS dev hits the wire directly using `URLSession`:
   ```swift
   let url = URL(string: "https://app.zeroship.ai/_zs/v1/todos.list?input=" + Data(try JSONEncoder().encode(["limit": 50])).base64EncodedString())!
   var req = URLRequest(url: url)
   req.setValue("Bearer \(jwt)", forHTTPHeaderField: "Authorization")
   let (data, _) = try await URLSession.shared.data(for: req)
   let todos = try JSONDecoder().decode([Todo].self, from: data)
   ```
2. No SDK to generate, no codegen step. Untyped relative to the source — typed clients in non-TS langs are deferred until OpenAPI emission lands.
3. Wire: `GET /_zs/v1/todos.list?input=<base64>`. Gateway validates JWT, checks rate limit, forwards to worker. Worker validates input via the procedure's Zod schema (when declared), runs handler, returns. Gateway adds `Cache-Control` + `ETag`. CDN edge can cache.

### C — AI-built chat app

1. Creator's app: `export async function* completion({ messages }) { for await (const c of llm.stream(...)) yield c.text; }`.
2. Frontend: `useChat({ api: rpc.chat.completion.streamUrl({}) })`. Vercel AI-SDK consumes the SSE.
3. **Zero custom protocol code.**

### D — refactor day

1. Creator reorganizes within `src/server/`: renames `src/server/todos.ts` → `src/server/features/todos.ts`. (Server modules must stay under `src/server/` — the transform is path-based.)
2. Procedure ids (`todos.list`, `todos.add`, …) don't change — they're explicit `fn.config.id` values, independent of the file path.
3. Wire identity preserved. Mobile clients keep working. No hidden break.

### E — incident: a hot path saturates the worker

1. `todos.list` is hammered by a partner integration.
2. Creator updates `list.config = { rateLimit: { rpm: 100 } }`. Redeploys.
3. Gateway 429s the offender within 5 s without the worker seeing the requests.
4. `Retry-After` tells the partner client to back off.

### F — adding an optional field

1. Creator changes `add`'s input from `{ text }` to `{ text, dueDate?: Date }`.
2. Old clients still send `{ text: "hi" }`. Schema accepts (`dueDate` optional).
3. New clients pass `dueDate`. No version bump needed.
4. **Removing** a field requires deprecation: declare `add` with new shape under a new `id` (`todos.add.v2`), keep the old for N months, codemod the client.

### G — auth-aware errors

1. End user's session expires mid-call.
2. Gateway returns `{ code: "UNAUTHENTICATED" }` 401.
3. Client SDK detects code, fires `onAuthExpired` hook, redirects to login.
4. After re-auth, SDK retries idempotent calls automatically; non-idempotent ones surface to the app.

### H — file upload (large args)

1. Creator: `export async function uploadAvatar({ blob }: { blob: Blob }) { ... }`. superjson encodes `Blob` as base64 (small files OK) or — for >1 MB — the SDK auto-routes via a streaming `multipart/form-data` POST that the gateway recognizes by `Content-Type` and forwards as a chunked body.
2. Server handler receives a `Blob` (or `ReadableStream` if `large: true` is opted into). Handler calls `storage.put(...)`.

---

## 12. Operability

| Surface | Where |
| --- | --- |
| Prometheus metrics | Gateway emits per-procedure counters/histograms |
| Trace propagation | W3C `traceparent` end-to-end |
| Request ID | Gateway generates UUIDv7, echoes in `X-Request-Id`, logs include it |
| Structured logs | `log.{info,warn,error}({ msg, fields })` ships JSON |
| Replay protection | Idempotency table per app, 24 h TTL default, in `zeroship.kv` |
| Audit log | Every mutation logs `{ trace_id, procedure, user_id, idempotency_key, status }` |
| Method-level dashboards | Auto-generated in creator console from manifest |
| Per-procedure tail logs | `zeroship logs tail --procedure todos.add` (CLI streams) |

---

## 13. Versioning

- URL prefix `/_zs/v1/`. Breaking wire changes → bump to `/_zs/v2/`. Both versions can coexist for a deprecation window (gateway routes both; resources can be declared under either prefix during the window).
- **Manifest schema version**: `version: 1` is the initial published shape. Future breaking changes bump to `version: 2`. There is no v0 or earlier shape — the manifest format starts here.
- **Per-procedure versioning** via the wireId — declare `todos.add.v2` next to `todos.add`; deprecate the old over a sunset window.
- Wire-format changes (transformer choice, error envelope shape) only happen on major URL-prefix bumps. Within `/_zs/v1/`, the system only adds fields; never removes.

---

## 14. What stays the same

- `.zsapp` artifact format — manifest schema is v1 with the unified `resources` block; the rest (worker, assets, runtime_assets, sourcemaps) is unchanged.
- Synthetic-entry pattern from the recent cleanup — expands to also expose `default.rpc` built from a build-time static procedure map. The closure-private registry virtual module is retired.
- Runtime kernel handles `/_zs/v1/*` by calling `user.default.rpc(name, input, ctx)`; non-RPC paths fall through to `user.default.fetch(request)`.
- EnvSnapshot, auth gateway, CHWBL routing — untouched.
- `Match` and `Action` enums survive as the gateway's internal compile target — they are no longer the public wire surface, but the existing validation, glob captures, and shadow-detection machinery in `crates/core/src/types.rs` continues to be used (now applied to the compiled output of the resource tree).

---

## 15. Open questions

1. ~~**TS-types-to-JSONSchema build pass** — needs a real perf number.~~ **Resolved (Zod-direct).** The build no longer derives JSONSchemas from TS types. Procedures opt into runtime validation by setting `fn.config.input` / `fn.config.output` to Zod schemas; the synthetic SSR entry calls `.parse()` at request time. Codegen for non-TS clients is **out of scope** — the wire is HTTP+JSON, callable from any language without an SDK. If multi-language typed clients ever become a real need, the path is OpenAPI emission via `zod-to-openapi`, not hand-rolled per-language generators.
2. **Default batching off vs. on** — proposal: off by default, opt-in via `client({ batch: true })`, on by default in the React adapter (where waterfalls are common). Revisit after early creators ship.
3. ~~**Error redaction in production**~~ **Resolved: default-on.** In production, anything thrown from a handler that is *not* an explicit `RpcError` instance becomes the opaque envelope `{ code: "INTERNAL", message: "Internal server error", retryable: false }` on the wire. The full error (message, stack, type) goes only to the worker's structured log, keyed by `trace_id`. Override paths: (a) `defineApp({ rpc: { dev: true } })` exposes raw messages even in production builds — useful for staging; (b) `throw new RpcError("INVALID_ARGUMENT", "Email already in use", { exposeMessage: true })` marks a thrown error as explicitly user-facing and bypasses redaction; (c) `RpcError` instances always serialize their own `code`, `message`, `details`, `retryable` (the user constructed them for the wire). Plain `throw new Error("...")` always gets redacted.
4. **Session-affinity for subscriptions** — WebSocket upgrades need to land on a worker that holds state for that subscription (e.g., DB watch handle). CHWBL hashes by app+session. Probably fine; verify end-to-end.
5. **Multi-tenant idempotency table sizing** — KV usage grows with mutation volume × TTL. Ship with a hard cap per app (e.g., 1 M live keys); evict oldest on overflow with a log line. Surface in usage dashboard.

---

## 16. Implementation phases

| Phase | Scope | LOC (rough) | Dependencies |
| --- | --- | --- | --- |
| 1 | Transform: discover server-module exports by path (`src/server/**`), infer kind, derive wireId from current source (explicit `fn.config.id` or bare `<exportName>`). Build: flatten `defineApp.resources` tree (auto-derived + user-declared) into `manifest.resources`. Wire Zod parse into the synthetic SSR entry (validate `fn.config.input`/`output` at runtime). | ~600 | none |
| 2 | Gateway: load `manifest.resources`, precompute `HashMap<MatchKey, EffectivePolicy>`, route + enforce per request. New `/_zs/v1/` prefix. | ~550 | Phase 1 |
| 3 | Client: typed `client<App>()` with GET/POST/idempotency/superjson. Auto-emitted `client-api.d.ts`. | ~400 | Phase 1 |
| 4 | Streaming: `function*` detection in transform; AI-SDK Data Stream wire on the runtime side. | ~250 | Phase 3 |
| 5 | React: `@zeroship/rpc/client/_hooks` registry; `@zeroship/rpc/react` populates it (`useQuery`, `useSuspenseQuery`, `useInfiniteQuery`, `useMutation`, `useStream`, `useSubscription`, `invalidate`, `prefetch`, `setData`, `<ZeroshipProvider>`); `@zeroship/rpc/server` (`__makeServerProcedure` + dehydrate/hydrate pipeline for SSR). | ~600 | Phase 4 |
| 6 | Idempotency: KV-backed dedupe table; gateway requires header for `idempotent: true`; runtime returns stored result on hit. | ~350 | Phase 2 |
| 7 | Subscriptions: WS upgrade routing through manifest; `fn.config.kind = "subscription"` runtime support. | ~600 | Phase 2 |

---

## 17. References

- React Server Actions — `"use server"` directive semantics
- tRPC v11 — typed procedure shape, link-batching, TanStack Query adapter
- Convex — query/mutation/action distinction, server-side context handles
- Vercel AI-SDK Data Stream Protocol — line-prefixed SSE
- gRPC error model — fixed code enum, status mapping
- superjson — typed-JSON transformer (Date, BigInt, Map, Set)
- Buf Connect — HTTP/2 + JSON/protobuf framing
