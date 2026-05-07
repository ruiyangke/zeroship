# RPC v2 — Seamless server functions

**Status:** Proposal · **Wire version:** `/_zs/v1/`

> Amendment 2026-05-07: `ctx.headers` and `ctx.url` are now request-scoped *mutable* native instances (no `Object.freeze`). The kernel never reads them after handing dispatch to the user procedure, so any mutation vanishes when the request ends — and dropping the freeze + per-setter shadow installs recovers ~4 µs of fixed cost per request (see `docs/perf/rpc-ctx-regression-2026-05-07.md`). The freeze approach was a defense-in-depth shim, not load-bearing on any kernel invariant; this amendment realigns the proposal with WHATWG defaults (`new Headers(...)` and `new URL(...)` are mutable).

---

## Motivation

zeroship apps need a server-function story that:

1. Lets a creator (or AI builder) write `export async function add(...)` and call `await add(...)` from the client. The transport is invisible.
2. Makes the wire identity of every method **stable across refactors**.
3. Validates args at the gateway boundary; the worker handler never sees malformed input.
4. Treats auth, rate-limit, idempotency, caching, metering as **declarative metadata** the gateway enforces before the worker runs.
5. Streams without an SDK fork — modern AI SDK 5 UI message streams, NDJSON, and raw `ReadableStream` all sit on the same content-negotiated transport.
6. Composes cleanly with the **primitives the platform already has** — native `AsyncLocalStorage`, native `Request`/`Response`/`FormData`/`Blob`/`File`, native `AbortSignal`, `#[v8_class]`/`#[v8_method(fastcall)]`, HMAC-signed `ZeroShip-User`, the `zeroship.{db,kv,storage,meter}` plugin surface.

This proposal is the second iteration. The round-01 critique identified the v1 sketch's six structural mistakes:

- §3 ambient context recommended a request-id slot when native ALS had landed (worse primitive).
- §4 / §6 specified superjson on the wire while the auto-stub strips `meta` (internal contradiction).
- §6 `RpcError` was a 9-line JS stub with `instanceof`-based brand checks (realm-fragile).
- §6 streaming was locked to the deprecated AI SDK v4 line-prefix protocol.
- §1 forbade function-level `"use server"` and reference-graph detection (locked out the RSC pattern).
- §8 idempotency lock was unspecified across N workers.

This rewrite addresses each. It also adds: native FormData/Blob/File on the wire, `#[v8_method(fastcall)]` on the dispatch hot path, `traceparent` propagation, `meter.*` integration, and an end-to-end auth chain.

---

## Goals & non-goals

**Goals**

- Server functions are imported and called like any other function — no wrappers, no `useQuery` ceremony for the 80% case.
- Wire identity is **stable across refactors**.
- Args validated at the boundary; the worker never sees malformed input.
- Auth, rate-limit, idempotency, caching, metering are *declarative metadata*, enforced by the gateway before the worker touches the request.
- Streaming is **content-negotiated**: AI SDK 5 UI message stream, NDJSON, raw bytes, SSE — picked from the `Accept` header, not baked into the platform.
- FormData / Blob / File are first-class argument and return types — no base64 retrofit.
- Typed TS surface inferred end-to-end. Non-TS clients call the wire directly via plain HTTP+JSON; OpenAPI emission is the future path for typed multi-language clients.

**Non-goals (initial release)**

- Server-to-server bidirectional streams (gRPC bidi) — defer.
- Schema federation across apps — defer.
- Persistent / Relay-style cursors at the protocol layer — apps roll their own.
- Subscriptions get a *sketch*-only contract here. Full backpressure / replay / credit-based flow control ships in a separate `rpc-subscriptions.md` proposal.

---

## 1. Authoring — RSC-aligned discovery

### Discovery is AST-based, not path-based

A function is server-callable iff it is **directly or transitively reachable from a client module via a `"use server"` boundary**. The boundary is detected at build time by walking the AST and the import graph. There is no path convention — `src/server/**` is a *suggestion*, not a rule.

Two markers, both standard `"use server"`:

**File-level** — every export becomes a server function:

```ts
// any path; convention is src/actions/todos.ts
"use server";
import { db } from "@zeroship/db";
import { ctx } from "@zeroship/server";

export async function list({ limit = 20, cursor }: ListArgs) {
  return db.todos.find({ ownerId: ctx.user.id, limit, cursor });
}

export async function add({ text }: AddArgs) {
  return db.todos.insertOne({ ownerId: ctx.user.id, text });
}
```

**Function-level** — only the marked function:

```ts
// app/posts/[id]/page.tsx — mixed file
import { EditPost } from "./edit-post";

export default async function PostPage({ params }: { params: { id: string } }) {
  const post = await getPost(params.id);

  async function updatePost(formData: FormData) {
    "use server";
    await db.posts.update(params.id, formData);
  }

  return <EditPost updatePostAction={updatePost} post={post} />;
}
```

The transform finds `"use server"` at the start of a function body **or** as the first non-import statement of a module, and tags every directly-marked or graph-reachable function.

### Reference-graph detection

After per-file directive scanning, the build walks the Vite/Rolldown module graph from the **client entry**. Any imported binding that resolves to a directly-marked function (or a module re-export of one) becomes a remote-procedure stub on the client. The transform replaces the imported symbol with a callable + branded reference; the original implementation stays in the server bundle.

This is the pattern shipped by React Server Actions, Next.js, Rspack RSC, and `@mfng/webpack-rsc`. We adopt it directly:

```
Client entry  →  imports  →  ./actions/todos      → file has "use server"
                                                    ⇒ all exports are server fns
                          →  ./components/Form    → contains updatePost() with "use server"
                                                    ⇒ updatePost is a server fn
                          →  ./lib/util.ts        → no marker, no transform
```

Per the user's "smarter AST" preference: we do not assume any path convention. A file with `"use server"` may live anywhere in the source tree.

### Strict mode (production builds)

`vite build --mode production` requires that **every server function** be in a directly-marked file or have a function-level directive. A graph-detected boundary that does not also have an explicit directive is a **build error** in production, with the message: *"`updatePost` is reachable as a server function via the reference graph but lacks a `"use server"` directive. Add the directive at function or file level for production builds."* Dev mode warns but allows.

This is the same posture Next.js takes: the directive is the source of truth; the graph is a convenience.

### What gets inferred

Inference is **only** about call shape (query vs. mutation vs. stream vs. subscription). It is never about *whether* the function is RPC — that's the directive's job.

| Inferred property | Rule |
| --- | --- |
| `kind: "query"` | Reads only — function body has no calls to `db.{insert,update,delete,upsert}*`, `kv.{set,delete,incr}`, `storage.{put,delete}`, `fetch` with a non-`GET`/`HEAD` method. **Required**: explicit `kind: "query"` on `fn.config`. |
| `kind: "mutation"` | Default for marked functions when not explicitly tagged. |
| `kind: "stream"` | `async function*` (generator) or `Promise<ReadableStream>` return type. |
| `kind: "subscription"` | Explicit `kind: "subscription"` on `fn.config`. |

The round-01 critic flagged name-regex inference (`/^(get\|list\|find\|search\|count\|read)/`) as a footgun (cache poisoning on `searchAndDestroy`). It is **deleted**. Queries opt in via `fn.config.kind = "query"` or via the `query()` wrapper from `@zeroship/server`.

### Wrappers (Level 2)

Functions are objects. Attach `.config` directly, or use a wrapper that carries types and runtime checks:

```ts
import { mutation, query, z } from "@zeroship/server";

export const list = query({
  input:  z.object({ limit: z.number().int().min(1).max(200).default(20), cursor: z.string().optional() }),
  output: z.array(TodoSchema),
})(async ({ limit, cursor }) => {
  return db.todos.find({ ownerId: ctx.user.id, limit, cursor });
});

export const add = mutation({
  input:      z.object({ text: z.string().min(1).max(500) }),
  idempotent: true,
  rateLimit:  { rpm: 100, per: "user" },
  middleware: ["transaction"],
})(async ({ text }) => {
  return db.todos.insertOne({ ownerId: ctx.user.id, text });
});
```

Wrappers (`query`, `mutation`, `stream`, `subscription`, `action`) are pure JS — they attach `.config` and return the function. They exist so the kind is **type-level**, not name-regex-inferred. The seamless `import { add } from "../actions/todos"` continues to work; the wrapper is invisible at the call site.

### Rules — what a server function may export

Per round-01 Medium-9 / Low-N feedback: rules are explicit, not implicit.

- An export reachable from a client through a `"use server"` boundary must be an `async function` or `async function*`. Other shapes (classes, plain objects, default-exported objects, sync functions) are a **build error**.
- A file-level `"use server"` directive applies to every exported async function. Non-async exports in that file are a build error.
- A function-level directive applies only to the function it heads. Other exports of the same file are normal client-bundle exports.
- Re-exports of a server function (`export { add } from "./todos"`) are tagged transitively.

### Opt-in lazy procedures (Wave #188)

A procedure marked `lazy: true` defers its module evaluation to the first call. The vite-plugin emits a dynamic-import wrapper instead of a static `import * as` line for that target file:

```ts
"use server";
import { mutation } from "@zeroship/server";
import { runWizard } from "./_wizard.js";   // heavy

export const wizard = mutation(async (input) => runWizard(input), {
  id:    "wizard",
  lazy:  true,
});
```

The synthetic SSR entry replaces the eager `import * as _user_TARGET_n_` plus `_procedures[wid] = ns.wizard` with `_procedures[wid] = async (input, ctx) => (await import("./wizard.js")).wizard(input, ctx)`. Cold-start parses only non-lazy modules. The dynamic import rides V8's host callback (Wave #187), which caches the module on first evaluation — second call is a Map lookup, not a re-import.

Detection accepts `fn.config.lazy = true` (literal boolean) or the wrapper second-arg form `mutation(handler, { lazy: true })`. Non-literal `lazy` expressions warn at build time and stay eager. Mixed files (some lazy, some eager exports) emit BOTH a static import and per-procedure dynamic-import wrappers — V8 de-dupes the module so both refer to the same namespace.

---

## 2. Wire identity

Every server function has a `wireId` — the string the network sees. The wire identity is a deterministic function of source and is **stable across refactors**.

### Resolution

1. **Creator-pinned `fn.config.id`** (e.g. `"todos.list"`) — wins.
2. **Default — bare `<exportName>`.** `export async function listTodos()` becomes `wireId = "listTodos"`. No path-derived slug.
3. **Production rule** — `vite build --mode production` requires every procedure to have an explicit pinned `id`. Bare-export defaults are a build error in production, with the file path and a suggested pin.

### Collision handling

Two procedures resolving to the same wireId is an **unrecoverable build error**. The error message names both source files and the conflicting id.

**Carve-out for procedure versions** (§13): two exports with the same `id` but different `version` fields are NOT a collision — they are two versions of the same logical procedure. The build merges them under one wireId entry in `manifest.artifact.procedures.<id>.versions`.

### URL shape

```
/_zs/v1/{wireId}
```

`v1` is the **wire-protocol** version. Major bumps (e.g., a different envelope shape) move to `/_zs/v2/`. Application-level versioning is described in §13.

---

## 3. Ambient context — `ctx`, ALS-backed

The seamless model needs per-request context without threading it through every signature. The platform shipped native `AsyncLocalStorage` (ISS-01, `crates/runtime/src/node/async_hooks/als.rs`) backed by V8's `ContinuationPreservedEmbedderData`. ALS propagates **automatically** across `await`, microtasks, `.then`, `setTimeout`, generator yields — no `bind()`, no `executing_request_id` repaint, no per-callback ceremony.

This proposal rests on ALS as the foundation primitive. The v1 draft's "no AsyncLocalStorage gymnastics; the slot is keyed by the kernel's in-flight request id" rationale is replaced: ALS *is* the gymnastics-free version, and using anything else is a worse design.

### `ctx` is a real object, populated by the host

```ts
import { ctx } from "@zeroship/server";

export async function add({ text }: { text: string }) {
  // The procedure's policy is auth: "user" — ctx.user is guaranteed
  // non-null. The TS type narrows to `User` (not `User | null`) when
  // the wrapper carries `auth: "user"`.
  ctx.log.info("adding todo", { userId: ctx.user.id });
  const todo = await db.todos.insertOne({
    ownerId: ctx.user.id,
    text,
    idempotencyKey: ctx.idempotencyKey,
  });
  ctx.waitUntil(analytics.track("todo.added", { id: todo.id }));
  return todo;
}
```

`ctx` is a request-fresh object built by the kernel (Rust) **before** the procedure runs. Accessing `ctx` from anywhere in the call tree — including transitively-deep helper modules and timers — reads the current ALS store via a single intrinsic.

The TS surface narrows `ctx.user`'s type based on the wrapper's `auth` policy:

- `auth: "anon"` → `ctx.user: User | null` — must null-check.
- `auth: "user"` or `auth: "admin"` → `ctx.user: User` — gateway already pre-rejected the request if absent.

### Field table

| Field | Type | Populated from | Notes |
| --- | --- | --- | --- |
| `ctx.user` | `User \| null` | Gateway-injected `ZeroShip-User` HMAC-signed header → exposed via `zeroship.auth.getUser()` (the existing kernel primitive); `ctx.user` is the const-time accessor. | `null` for `auth: "anon"`; throws `UNAUTHENTICATED` if read by an `auth: "user"`/`"admin"` procedure that didn't authenticate (defense-in-depth). |
| `ctx.requestId` | `TypedId<"req">` | Gateway generates UUIDv7 typed_id | Echoed in `X-Request-Id` response header; matches `typed_id` invariant. |
| `ctx.traceId` | `string` (32-char hex) | W3C `traceparent` header (gateway creates if absent) | Used for OTel correlation. |
| `ctx.signal` | `AbortSignal` | `AbortSignal.any([clientDisconnect, gatewayDeadline, isolateEviction])` (native, `crates/runtime/src/web/dom/abort_signal.rs`) | Aborts on any of the three (see "Abort source plumbing" below). Auto-passed to `fetch`, `db.*`, `kv.*`, `storage.*`. |
| `ctx.idempotencyKey` | `string \| undefined` | Client's `Idempotency-Key` header, validated by gateway | `undefined` for queries and mutations without `idempotent: true`. |
| `ctx.headers` | `Headers` (native, mutable per-request) | The procedure's request | The kernel constructs the `Headers` instance lazily on first `ctx.headers` read and caches the same wrapper for the request's lifetime. Mutations (`headers.set(...)`, `headers.append(...)`, `headers.delete(...)`) succeed but vanish at request end — the kernel never re-reads `ctx.headers` after dispatching to the user procedure. Aligns with the WHATWG default for `new Headers(...)`. To pin an immutable copy, the procedure constructs `new Headers(ctx.headers)` and freezes it itself. |
| `ctx.method` | `"GET" \| "HEAD" \| "POST" \| "PUT" \| "DELETE" \| "PATCH" \| "OPTIONS"` | The wire request | For non-raw procedures, the gateway pre-rejects bad methods (§7); for `kind: "raw"`, the procedure handles whatever the gateway forwards. |
| `ctx.url` | `URL` (native, mutable per-request) | The wire request | The kernel constructs the `URL` instance lazily on first `ctx.url` read and caches the same wrapper for the request's lifetime. Setters (`url.pathname = ...`, `url.searchParams.set(...)`, etc.) succeed but vanish at request end — the kernel never re-reads `ctx.url` after dispatching. Aligns with the WHATWG default for `new URL(...)`. To build a modified URL without affecting the cached `ctx.url`, the procedure constructs a fresh `new URL(ctx.url)` and mutates that. |
| `ctx.waitUntil` | `(p: Promise<unknown>) => void` | Kernel — extends the procedure's lifetime past the response without blocking it | For analytics, logging flushes, etc. |
| `ctx.log` | `{ info, warn, error, debug }` | Kernel structured-log emitter | Each log is a JSON line tagged with `{requestId, traceId, app, procedure}`. |
| `ctx.env` | merged `vars + secrets` | Per-app config | Read-only; immutable per request. **Merge order** (higher wins): runtime-process env vars < `defineApp.env` declarations < per-deploy secret manager values. The deploy-time `defineApp.env` is the source of truth; runtime-process env exists as a dev-mode fallback. |
| `ctx.meter` | `{ increment(metric, delta?, tags?) }` | `zeroship.meter.*` primitive | See §12 for auto-emitted RPC metrics. |

### Abort source plumbing

`ctx.signal` aborts when any of three sources fire. Each has a defined plumbing path:

- **`clientDisconnect`** — the gateway holds the worker request connection open until the response is fully drained. On client disconnect (TCP close, HTTP/2 RST_STREAM, browser tab close), the gateway immediately closes the worker-side request stream. The runtime's `Request.body` `ReadableStream` emits an `error` event on close; the runtime's request-tracking layer (in `crates/runtime/src/rpc/dispatch.rs`) wires this to abort the per-request `AbortController` backing `ctx.signal`. For raw procedures (`kind: "raw"`) that read the body lazily, the abort fires when the next `body.getReader().read()` call encounters the stream error.
- **`gatewayDeadline`** — the gateway sets a hard timeout per request based on the resource's `policy.timeout` (or `defineApp.rpc.defaults.timeout`). When the timer fires, the gateway sends 504 to the client (if the response hasn't started) and closes the worker connection (which triggers the `clientDisconnect` path on the worker side).
- **`isolateEviction`** — the worker's LRU cache evicts an isolate when it exceeds the per-thread cap. Evictions during in-flight procedures: the worker first calls `entered_for_eviction()` on the isolate, which fires the per-request `AbortController` for every in-flight procedure (a 30-second drain window starts; see §15 OQ-2). Procedures that finish within the window respond normally; those that don't are aborted hard at window end. After the drain, the isolate enters `Disposed` state.

`AbortSignal.any` aggregates these into a single signal observable from the procedure. All native primitives (`fetch`, `db.*`, `kv.*`, `storage.*`) accept a `signal` option; the runtime injects `ctx.signal` as the default when the user code passes none.

### Functional helpers (transitively-deep code)

For code that can't take `ctx` as a parameter (e.g., `@zeroship/db` internals, third-party libs), `getRequestContext()` is exported:

```ts
import { getRequestContext, user, requestId, traceId, signal, idempotencyKey } from "@zeroship/server";

const c = getRequestContext();   // returns the ctx object
// or, narrow accessors:
const me = user();               // throws UNAUTHENTICATED if absent and policy requires auth
const rid = requestId();
```

Each helper is a one-line wrapper over `getRequestContext().X`. They exist for ergonomics; the canonical access is `ctx`.

### Population — Rust-side, before V8 sees the procedure

The runtime's `default.rpc` dispatch path:

```rust
async fn dispatch_rpc(req: &RpcRequest) -> Response {
    let ctx_obj = build_ctx_object(scope, req);          // native v8::Object, fields wired
    let als_store = build_als_map_with_ctx(ctx_obj);

    // ALS.run(store, () => user_fn(...)) — done in Rust by writing the
    // ContinuationPreservedEmbedderData slot directly, then calling the
    // user function. V8 propagates the slot across all subsequent
    // continuations.
    enter_als(scope, als_store);
    let result = user_default_rpc.call(scope, &[wire_id, input]).await;
    exit_als(scope);

    result
}
```

The `executing_request_id` slot used by the v1 draft (`crates/runtime/src/auth.rs:55-59`) is removed. Auth context, idempotency key, signal — all live in ALS, all propagate automatically.

### Why this is strictly better than v1's request-id slot

Round-01 Critical-1 named the failure mode: `setTimeout(() => user(), 1000)` re-enters the runtime in a different request's V8 turn, and the request-id slot has been repainted. With ALS the captured store is the **continuation's own** store; the slot rewrite cannot affect previously-captured continuations. Same reasoning applies to `Promise.then`, generator yield, queueMicrotask — every continuation point V8 emits.

---

## 4. Wire envelope — superjson, mandatory

The wire is JSON. Plain `JSON.stringify` loses `Date`, `BigInt`, `Map`, `Set`, `URL`, `Uint8Array`, `RegExp`. **Superjson is the canonical envelope. There is one wire format.**

The round-01 critic flagged the v1 sketch as "auto-stub strips meta; manual client preserves meta; types lie about Date round-tripping." That's resolved by removing the choice: every emitter (auto-stub, manual `client<App>()`, server response, batched envelope, cached idempotency body) emits superjson. Every parser (gateway, kernel, client SDK) reads superjson.

### Envelope shape

```json
{ "json": <payload>, "meta": <superjson-meta-or-omitted> }
```

- `json` — the value with `Date`/`BigInt`/`Map`/`Set`/`URL`/`Uint8Array`/`RegExp` stringified into superjson's compact representation.
- `meta` — a superjson `meta.values` map keyed by JSON path. **Omitted when empty** (i.e., when the payload is JSON-native). Saves bytes for the 80% case.

The gateway and runtime decode-encode pair are both Rust; superjson is implemented natively in `crates/core/src/superjson.rs` (new). Cost on the hot path: one allocation for the meta map (empty in the common case → single `null` write), one walk of the value tree on encode.

#### Gateway / worker decode contract

The gateway forwards the **original envelope bytes verbatim** to the worker. The gateway's parse is read-only — it computes the input hash (for idempotency, §8), runs schema-pre-validation (when the procedure declares `fn.config.input` and the gateway has the schema cached), and never re-serializes. The worker decodes once for handler invocation. Net cost: parse-twice (~5–15 µs per request at 1 KiB envelope) plus one decode for the handler; the gateway never spends bytes re-encoding. The forwarded body is byte-identical to the client's body, which preserves any client-side superjson canonicalization the SDK chose.

### Type table

| TypeScript type | Wire representation | Notes |
| --- | --- | --- |
| `string`, `number`, `boolean`, `null`, plain object, plain array | unchanged in `json` | No `meta` written. |
| `Date` | ISO-8601 string in `json`; `meta` tag `"Date"` at the path | Round-trips. |
| `BigInt` | decimal string in `json`; `meta` tag `"bigint"` | |
| `Map<K, V>` | `[[k, v], ...]` array in `json`; `meta` tag `"map"` | Keys recurse. |
| `Set<T>` | array in `json`; `meta` tag `"set"` | |
| `URL` | string in `json`; `meta` tag `"URL"` | |
| `RegExp` | source string + flags in `json`; `meta` tag `"regexp"` | |
| `Uint8Array` | base64 string in `json`; `meta` tag `"Uint8Array"` | **Inline only when small (<32 KiB raw); above that, switch to `wire: "multipart"`. The 32 KiB threshold reflects superjson's per-encode allocation budget — base64 inflates by 33%, so a 32 KiB `Uint8Array` becomes ~43 KiB on the wire, still well within the 1 MiB JSON envelope cap. Larger and the multipart wire is strictly cheaper.** |
| `ArrayBuffer` | as above (treated as `Uint8Array` view) | `meta` tag `"Uint8Array"` | The view's byte range is materialized at encode time. |
| `Blob`, `File` | **Not inline.** See §4b. | Triggers multipart on the wire. |
| `FormData` | **Not inline.** See §4b. | Triggers multipart on the wire. |
| `ReadableStream` | **Not inline.** See §6 streaming. | Routed via the streaming wire. |
| `undefined` (in object value) | omitted from `json`; `meta` tag `"undefined"` at path | superjson convention. |
| `Promise<T>` | Forbidden — build error (procedure must return `T`, not `Promise<T>`-of-`Promise<T>`). | |
| Class instances | **Forbidden as wire values.** Use plain objects. Build error in `--mode production`. | superjson would use the global `superjson.registerClass` registry; we don't extend it on the wire. |

### Validation

Procedures opt into runtime validation by setting `fn.config.input` / `fn.config.output` to Zod schemas. The wire pipeline:

1. Gateway decodes the superjson envelope into a `serde_json::Value`. Per round-01 Medium-1, this happens **after** auth/rate-limit, **before** worker dispatch.
2. Worker `default.rpc` calls `fn.config.input?.parse(args)` if declared; throws `INVALID_ARGUMENT` on failure.
3. After handler returns, `fn.config.output?.parse(result)` runs in dev only (or in production if `defineApp({ rpc: { strictOutput: true } })`).

For multi-language clients, the wire is plain HTTP+JSON; the OpenAPI emission path remains the future answer (deferred).

### Type generation for the client

The build emits `virtual:zeroship/server-api.d.ts` containing the typed surface of every server export, with the transformer's input/output mapping applied:

```ts
// virtual:zeroship/server-api.d.ts (excerpt)
declare module "../actions/todos" {
  export const list: ((input: { limit?: number; cursor?: string }) => Promise<Todo[]>) & ProcedureHooks<"todos.list", "query", Todo[]>;
  export const add:  ((input: { text: string })                    => Promise<Todo>)   & ProcedureHooks<"todos.add",  "mutation", Todo>;
}
```

Tree-shaken; no runtime cost beyond the call site.

---

## 4b. Binary, FormData, File — first-class

Native `FormData`, `Blob`, `File`, `ReadableStream` ship as `#[v8_class]` primitives (`crates/runtime/src/web/dom/form_data.rs`, etc.). Procedures can take them as args and return them as values. The wire **does not base64-encode** them.

### How the wire mode is chosen

TypeScript types are erased at build time, so the transform cannot rely on them to decide between JSON and multipart wires. The wire mode is chosen by **explicit, build-visible signals** — in priority order:

1. **`fn.config.wire`** — explicit override. Values: `"json"` (default), `"multipart"`, `"ai-ui-v1"` (streams), `"raw"` (escape-hatch).
2. **Wrapper input schema** — when the wrapper is `mutation({ input: ... })` and the schema includes `z.instanceof(FormData)`, `z.instanceof(File)`, `z.instanceof(Blob)`, OR a `z.object({...})` whose top-level fields include any of those types, the build emits `wire: "multipart"`.
3. **Default** — `wire: "json"`.

The build records the resolved `wire` in `manifest.artifact.procedures.<id>.wire`. The client stub uses the recorded value to pick the call shape.

**Ambiguous schemas**: when the input schema is a `z.union` or `z.discriminatedUnion` mixing JSON-native and binary types (e.g., `z.union([z.instanceof(File), z.string()])`), the build cannot statically pick a single wire. **Build error**: *"Cannot statically determine wire mode for procedure `uploadAvatar` — input schema mixes binary types and JSON-native types in a union. Set `wire: 'multipart'` explicitly, or split into two procedures (`uploadByUrl`, `uploadByFile`)."*

```ts
// (a) Pure-FormData parameter — wire: "multipart", _zs.json absent.
import { mutation, z } from "@zeroship/server";
export const uploadAvatar = mutation({
  input:      z.instanceof(FormData),
  idempotent: true,
})(async (form: FormData) => {
  const file = form.get("avatar") as File;
  const url  = await storage.put(file);
  return { url };
});

// (b) Mixed structured + binary — wire: "multipart", _zs.json carries JSON fields.
export const updateAvatar = mutation({
  input: z.object({
    avatar:  z.instanceof(File),
    caption: z.string().min(1).max(280),
  }),
  idempotent: true,
})(async ({ avatar, caption }) => {
  const url = await storage.put(avatar);
  return { url, caption };
});

// (c) Pure-JSON parameter — wire: "json".
export const renameTodo = mutation({
  input: z.object({ id: z.string(), title: z.string() }),
})(async ({ id, title }) => db.todos.update(id, { title }));

// (d) Manual override — author wants raw bytes.
export const ingestLog = mutation({
  input: z.instanceof(Uint8Array),
  wire:  "multipart",      // explicit; otherwise the schema would default to json+base64
})(async (bytes: Uint8Array) => log.append(bytes));
```

### Multipart envelope spec

When a procedure resolves to `wire: "multipart"`:

- **Request boundary**: `multipart/form-data; boundary=----zs<random32hex>`. The boundary string is client-chosen, browser-emitted in the `<form>` case.
- **`_zs.json` part** — the JSON-fields envelope.
  - **Present iff** the resolved input schema has any non-`File`/`Blob`/`FormData` fields (case (b) above).
  - **Absent iff** the schema is exactly `z.instanceof(FormData)` (case (a)) — the runtime hands the parsed multipart directly to the handler with no field extraction.
  - `Content-Type: application/json; charset=utf-8`.
  - Body is the **full superjson envelope** for the structured fields: `{ "json": { ...non-binary fields }, "meta": { ...if any Date/BigInt/etc. } }`. Empty `meta` is omitted as elsewhere.
  - The reserved field name `_zs.json` (and any field whose name starts with `_zs.`) is **forbidden** in user form schemas. Build error if a user schema declares `_zs.json` or starts a field name with `_zs.`. This eliminates the field-name collision possibility.
- **Binary parts** — one part per `File` / `Blob` field.
  - `Content-Disposition: form-data; name="<schemaFieldName>"; filename="<file.name>"`.
  - `Content-Type: <file.type>` if known, else `application/octet-stream`.
  - Body is the raw bytes (no transfer encoding beyond the multipart framing).
- **`Uint8Array` fields** — when explicitly opted into multipart (case (d)) — emitted as binary parts with `Content-Type: application/octet-stream` and a synthetic `filename` (`field.bin`).

#### Wire example — case (b)

```
POST /_zs/v1/updateAvatar HTTP/1.1
Content-Type: multipart/form-data; boundary=----zs1f2e3d4c5b6a7890
Idempotency-Key: 01HJQK…

------zs1f2e3d4c5b6a7890
Content-Disposition: form-data; name="_zs.json"
Content-Type: application/json; charset=utf-8

{"json":{"caption":"My new pic"}}
------zs1f2e3d4c5b6a7890
Content-Disposition: form-data; name="avatar"; filename="me.jpg"
Content-Type: image/jpeg

<binary>
------zs1f2e3d4c5b6a7890--
```

#### Wire example — case (a) (pure-FormData)

No `_zs.json`; the runtime hands the parsed `FormData` to the handler:

```
POST /_zs/v1/uploadAvatar HTTP/1.1
Content-Type: multipart/form-data; boundary=----zsBoundary
Idempotency-Key: 01HJQK…

------zsBoundary
Content-Disposition: form-data; name="avatar"; filename="me.jpg"
Content-Type: image/jpeg

<binary>
------zsBoundary--
```

### Progressive enhancement — `<form action={fn}>`

The branded server-reference (§5) means `<form action={uploadAvatar}>` works without JS — the browser POSTs `multipart/form-data` to the procedure's URL, the gateway dispatches normally. With JS, React's form handling intercepts and calls the procedure via the client transport. Both paths produce the same wire shape:

```tsx
<form action={uploadAvatar} encType="multipart/form-data">
  <input type="file" name="avatar" />
  <button>Upload</button>
</form>
```

The gateway routes by `Content-Type: multipart/form-data` to the worker; the worker's runtime constructs a native `Request` and the procedure receives the parsed `FormData` (case (a)) or the structured-fields object (case (b)). The native multipart parser does the work — no JS-side decode.

### Procedure that returns `Blob` / `File` / `ReadableStream`

For `Blob` / `File` returns: response uses `Content-Type: <blob.type>` (or `application/octet-stream`), body is the binary, no envelope wrapping. The client SDK detects the content-type and surfaces a `Blob` to the caller.

For `ReadableStream` returns: see §6 streaming for the wire spec. Disambiguation rule (single source): a procedure returning `ReadableStream` must declare `kind: "stream"` (or `kind: "raw"` for full Response control). Non-stream procedures (queries / mutations) returning a `ReadableStream` are a build error — this forces the creator to declare timeouts and flow-control discipline up front.

### Procedure typed `(req: Request) => Response` — raw escape-hatch

Power users who need full HTTP control declare:

```ts
import { action } from "@zeroship/server";

export const webhookHandler = action({
  kind: "raw",
})(async (req: Request) => {
  if (req.headers.get("x-signature") !== expectedSig) {
    return new Response("forbidden", { status: 403 });
  }
  // ...
  return new Response(JSON.stringify({ ok: true }), { headers: { "content-type": "application/json" } });
});
```

`kind: "raw"` opts out of:

- Superjson envelope encode/decode.
- ETag / If-None-Match.
- Content-negotiation (the runtime forwards the procedure's `Response` verbatim).
- Structured argument validation (the schema is the runtime's `Request`).
- The `rpc.stream_chunks` meter (the runtime cannot count chunks in an opaque stream).
- NDJSON sentinel framing for errors.

`kind: "raw"` keeps:

- Auth (the gateway still validates ZeroShip-User HMAC; `ctx.user` is populated).
- Rate limit, CSRF, max_input_bytes, max_output_bytes (gateway-enforced).
- Idempotency (when `idempotent: true` is set; the gateway-side cache stores the entire `Response` body + headers).
- `ctx` (full surface: `user`, `signal`, `requestId`, `traceId`, `idempotencyKey`, `waitUntil`, `log`, `meter`).
- The `rpc.requests`, `rpc.duration_ms`, `rpc.cpu_us`, `rpc.ingress_bytes`, `rpc.egress_bytes` meters.

#### Streaming from a raw procedure

A raw procedure may return a `Response` with a streaming body (e.g., `new Response(readableStream)`). When the response body is streaming:

- The runtime applies **inactivity timeout** — default 30 s, configurable via `fn.config.inactivityMs`.
- The runtime applies **max lifetime** — default 30 min, configurable via `fn.config.maxLifetimeMs`.
- Both are enforced via `ctx.signal` (the runtime aborts the body if either fires).

The raw procedure does NOT participate in §6 content negotiation — its `Content-Type` is whatever the procedure set. This is by design: raw is for protocols not covered by the platform's first-class wires (e.g., gRPC-Web, MQTT-over-HTTP, custom streaming binary).

To pick between `kind: "raw"` and `kind: "stream"`:

| Use case | Pick |
| --- | --- |
| Webhook (single Request → single Response, custom content-type) | `kind: "raw"` |
| Custom binary protocol (Stripe webhook signature, S3 multipart upload, SCIM) | `kind: "raw"` |
| Typed JSON streams (LLM outputs, observability events) | `kind: "stream"` (uses content negotiation) |
| AI SDK 5 UI Message Stream | `kind: "stream"` + `wire: "ai-ui-v1"` |
| Server-side events with structured frames | `kind: "stream"` (Accept: text/event-stream) |

### Size limits

| Limit | Default | Where set |
| --- | --- | --- |
| `max_input_bytes` (JSON envelope) | 1 MiB | `fn.config.maxInputBytes` or `defineApp.rpc.defaults.maxInputBytes` |
| `max_input_bytes` (multipart) | 100 MiB | Same field; the gateway enforces both. |
| `max_output_bytes` | 10 MiB | `fn.config.maxOutputBytes` |
| `max_concurrent_per_user` | 100 | `fn.config.maxConcurrentPerUser` |
| `max_concurrent_per_app` | 10000 | `fn.config.maxConcurrentPerApp` |

The 1 MiB / 100 MiB split exists because JSON-only inputs have no streaming use case worth a higher cap, while multipart commonly carries video / large images. Configurable per-procedure for cases that legitimately need more.

---

## 5. Compilation pipeline

### Per-file transform

```
1. Parse AST (oxc).
2. Detect "use server" boundaries:
     a. File-level: first non-import statement is "use server" (string-literal).
     b. Function-level: first statement of a function body is "use server".
3. Walk the module graph from the client entry. For each import edge:
     a. If target is in a directly-marked file → mark the imported binding as a server reference.
     b. Else if target is itself a re-export of a marked binding → mark transitively.
4. For each marked function:
     a. Validate shape: must be async function or async generator.
     b. Resolve wireId: explicit fn.config.id wins; else bare exportName.
     c. Resolve metadata: fn.config + module-level "use server" config (if any) +
        defineApp({ rpc: { defaults } }) + built-in defaults.
     d. Stash (wireId, kind, importPath, exportName, configMetadata) in shared state.
5. Emit transformed code by environment:
     client → replace marked exports with stub callables (ProcedureRef branded values).
     ssr    → keep body verbatim; synthetic entry imports it statically.
6. After all files transformed, generate the synthetic entry from stashed tuples.
7. Strict-mode pass: every procedure must have a directly-declared "use server"
    marker. Graph-only-detected procedures are a build error in production.
```

### Synthetic entry — wraps `default.rpc` and `default.fetch`

The synthetic entry's `USER_ENTRY` placeholder resolves to the user's app entry, picked in this order:

1. The first server-environment entry declared in `vite.config.ts` (`environments.zeroship.input` or `environments.ssr.input`).
2. `package.json::main` (when present and points to a path inside `src/`).
3. `src/index.ts` / `src/index.tsx` / `src/index.js` (in that order).

The chosen path is recorded in `manifest.artifact.worker.entry`. Apps without a top-level entry (pure RPC apps with no `default.fetch`) get a synthetic minimal entry that returns `404 Not Found` for all non-RPC paths.

```ts
// auto-generated; lives at virtual:zeroship/_server-entry
import * as _zsUser     from "USER_ENTRY";
import { __dispatchRpc } from "@zeroship/server/runtime";

import { list as _t_list, add as _t_add } from "./actions/todos";
import { me   as _u_me  }                from "./actions/users";

const _procedures = {
  "todos.list": _t_list,
  "todos.add":  _t_add,
  "users.me":   _u_me,
};

const _userDefault = (_zsUser?.default && typeof _zsUser.default === "object") ? _zsUser.default : null;
const _userFetch   = _userDefault?.fetch ?? null;

export default {
  // RPC entry — the kernel calls this for /_zs/v1/<wireId> requests.
  rpc:   (name, input, ctx) => __dispatchRpc(_procedures, name, input, ctx),
  // SSR / non-RPC fallthrough.
  fetch: (req)              => _userFetch ? _userFetch.call(_userDefault, req) : new Response("Not Found", { status: 404 }),
};
```

`__dispatchRpc` is exposed by `@zeroship/server/runtime`; it does input validation, output validation (dev-only), error redaction (§6), and meter emission (§12). The closure-private `_procedures` map is unreachable from user code (`globalThis` doesn't see it; the resolveId hook denies user lookups of `virtual:zeroship/_server-entry`).

### Client-side stub — branded reference

```ts
// transform-client output of ./actions/todos
import { __makeProcedure, __SERVER_REFERENCE } from "@zeroship/rpc/client";

export const list = __makeProcedure({
  [__SERVER_REFERENCE]: true,
  id: "todos.list", kind: "query", wire: "json",
});
export const add = __makeProcedure({
  [__SERVER_REFERENCE]: true,
  id: "todos.add", kind: "mutation", idempotent: true, wire: "json",
});
```

The `__SERVER_REFERENCE` symbol is the branding mechanism. Same pattern as RSC's server-action references: the marker survives serialization (the symbol is sent as a sentinel field), so when a server function is passed as a prop to a client component (RSC pattern) the client knows to invoke it via RPC, not call it locally.

### Gateway dispatch — fastcall hot path

The gateway-to-worker boundary uses `#[v8_method(fastcall)]` for the dispatch entry. The kernel exposes:

```rust
// crates/runtime/src/rpc/dispatch.rs
#[v8_class]
impl RpcDispatcher {
    /// Synchronous fastcall — decodes the envelope, validates,
    /// populates ALS, and looks up the user function. Returns a
    /// resolver-id (i32) that the JS side uses to await completion.
    /// Per Headers.has fastcall precedent, the fastcall ABI must
    /// return primitives; Promise resolution is threaded via a
    /// JS-side resolver pool keyed by the returned id.
    #[v8_method(fastcall)]
    fn enqueue_json(
        &self,
        scope: &mut v8::PinScope,
        wire_id: v8::Local<v8::String>,
        input_bytes: v8::Local<v8::Uint8Array>,
    ) -> i32 { /* ... */ }

    /// Same for multipart.
    #[v8_method(fastcall)]
    fn enqueue_multipart(
        &self,
        scope: &mut v8::PinScope,
        wire_id: v8::Local<v8::String>,
        request: v8::Local<v8::Object>,  // a Request #[v8_class]
    ) -> i32 { /* ... */ }

    /// Slow-path: returns the Promise for a previously-enqueued
    /// dispatch. Not fastcall (Promise return is unsupported).
    #[v8_method]
    fn await_dispatch(
        &self,
        scope: &mut v8::PinScope,
        ticket: i32,
    ) -> v8::Local<v8::Promise> { /* ... */ }
}
```

The two-step pattern (fastcall enqueue, then JS-side `await dispatcher.awaitDispatch(ticket)`) lets the hot path stay in fastcall ABI while still threading the async result back. Same approach the runtime uses elsewhere where fastcall ABI is desired but the operation is async (per `Headers.has` precedent: fastcall-only when the return is a primitive).

These kernel-side entries decode the envelope, ALS-populate `ctx`, and call into the JS-side `_procedures[name]`.

#### Performance — measured, not assumed

The fastcall enqueue is hot-path; the slow-path `awaitDispatch` is invoked once per RPC and is not a TurboFan optimization target. The measured win is therefore bounded:

- Best case: hot path is enqueue-dominated (small input, fast handler). Fastcall saves 30-100 ns on enqueue per RPC vs. `v8::Function::call`. The `awaitDispatch` adds back one normal V8 boundary crossing (~50-150 ns). Net per RPC: −20 ns to +50 ns vs. a single `v8::Function::call`. At 200K req/s, ≤10 ms/s saved across the whole worker.
- Realistic case: hot path is dominated by superjson decode, ALS write, and JS-side `_procedures[name]` lookup. The fastcall savings are <5% of total dispatch cost.

Implementation rule: phase 1 ships the two-step pattern only after a microbenchmark in `crates/runtime/benches/rpc_dispatch.rs` confirms a positive win at p50 and p99 with realistic procedure shapes (small JSON in/out; FormData multipart). If the benchmark shows a wash or negative, phase 1 ships the simpler single `v8::Function::call` path and the proposal is amended.

The synthetic entry's `default.rpc` is the JS-side wrapper that fans out to `_procedures[name]`. Most of the work happens before that — superjson decode, validation, metering — and that work runs in Rust at fastcall speed regardless of which dispatch ABI we land on.

### End-to-end runtime call path

```
Client                                          Gateway                                          Worker
─────                                           ───────                                          ──────
list({ limit: 50 })  →  __rpcCallJson(...)
                         GET /_zs/v1/todos.list?input=eyJqc29uIjp7...}
                                              →  Lookup "rpc:todos.list" in EffectivePolicy map
                                                 Auth check (ZeroShip-User HMAC)
                                                 Rate limit (per user)
                                                 max_input_bytes guard
                                                 traceparent inject (if absent)
                                                 Emit meter event "rpc.requests"
                                              →  Forward to worker (CHWBL by app+session)
                                                                                              →  Decode superjson envelope (Rust, fastcall)
                                                                                                 Build ctx (user, signal, idempotencyKey, traceId, ...)
                                                                                                 ALS.run(store, () => default.rpc("todos.list", input, ctx))
                                                                                                 → __dispatchRpc → fn.config.input.parse → list({ limit: 50 })
                                                                                                 list runs: db.todos.find(...)
                                                                                                 Encode result via superjson (Rust)
                                              ←  Response { json: [...], meta: {...} }
                                                 Apply Cache-Control + ETag
                                                 Emit meter "rpc.cpu_us", "rpc.egress_bytes"
                       ←  200 OK { json, meta }
list returns parsed Todo[]  ←  superjson decode
```

### Files involved

| File | Role |
| --- | --- |
| `sdks/vite-plugin/src/transform.ts` | Per-file rewrite, function-level + file-level directive handling |
| `sdks/vite-plugin/src/reference-graph.ts` | Module-graph walk for transitive marking |
| `sdks/vite-plugin/src/synthetic-entry.ts` | Generates the bundle entry |
| `sdks/vite-plugin/src/manifest.ts` | Emits `manifest.resources` at `closeBundle` |
| `crates/runtime/src/rpc/dispatch.rs` | NEW: fastcall dispatch entries |
| `crates/runtime/src/rpc/error.rs` | NEW: native `RpcError` `#[v8_class]` (§6) |
| `crates/runtime/src/rpc/superjson.rs` | NEW: superjson encode/decode |
| `crates/core/src/superjson.rs` | NEW: shared superjson logic (gateway + worker) |
| `crates/gateway/src/idempotency.rs` | EXTENDED: distributed lock primitive (§8) |

---

## 6. Wire protocol

### Query — `GET`

```
GET /_zs/v1/{wireId}?input=<base64url(superjson-envelope)>
Authorization: Bearer <jwt>          (or session cookie + ZeroShip-User header)
Accept: application/json
traceparent: 00-<trace-id>-<parent-id>-<flags>     (auto-injected)
```

If `input` size > 6 KB (URL header limit), client transparently falls back to `POST` with body (`Content-Type: application/json`, body = envelope). Functionally equivalent.

```
200 OK
Content-Type: application/json
Cache-Control: max-age=30, stale-while-revalidate=60, private
ETag: "a3b1f9c2d4e5f678"
Vary: Authorization, Accept-Encoding
X-Request-Id: req_01HJQK2A8R…
traceparent: 00-<trace-id>-<gateway-span-id>-01

{ "json": [...], "meta": {...} }
```

#### ETag / If-None-Match

- ETag = `"<sha256(canonical-superjson-of-result)[:16]>"` (16 hex chars, weak ETag).
- `If-None-Match` matching the current ETag → `304 Not Modified` with the same headers, no body.
- `Vary: Authorization, Origin, Accept-Encoding` is mandatory: different users see different data; CORS-cached responses must vary by Origin; gzip/br variants vary by encoding.
- CDN-cacheable iff `auth: "anon"` AND `kind: "query"` AND `cache.public: true` — the gateway sets `Cache-Control: public, max-age=...` only when all three hold.

### Mutation — `POST`

```
POST /_zs/v1/{wireId}
Authorization: Bearer <jwt>
Content-Type: application/json
Idempotency-Key: 01HJQK…              ← required iff procedure idempotent: true
traceparent: 00-...

{ "json": {...}, "meta": {...} }
```

Response: `200 OK` + JSON envelope. No `ETag` or `Cache-Control: max-age`.

### Multipart mutation — `POST` with FormData

```
POST /_zs/v1/{wireId}
Authorization: Bearer <jwt>
Content-Type: multipart/form-data; boundary=----zsBoundary
Idempotency-Key: 01HJQK…

------zsBoundary
Content-Disposition: form-data; name="_zs.json"

{ "json": { "caption": "..." } }
------zsBoundary
Content-Disposition: form-data; name="avatar"; filename="me.jpg"
Content-Type: image/jpeg

<binary>
------zsBoundary--
```

The `_zs.json` part is optional (procedures whose inputs are pure FormData may skip it). The handler receives a native `FormData` if its parameter is typed `FormData`, or an object built from the `_zs.json` part plus typed `File`/`Blob` fields if its parameter is a structured object.

### Stream — content-negotiated

The streaming wire is **chosen by the client's `Accept` header**, not baked into the platform.

```
POST /_zs/v1/{wireId}
Authorization: Bearer <jwt>
Content-Type: application/json
Accept: <one of: application/x-ndjson | text/event-stream | application/octet-stream>

{ "json": {...} }
```

| `Accept` value | Wire | Use case |
| --- | --- | --- |
| `application/x-ndjson` (default) | NDJSON: one superjson envelope per `\n`-terminated line | Generic typed streams (the default for `kind: "stream"` procedures returning typed values) |
| `text/event-stream` | SSE per W3C spec; `data:` lines, blank-line terminator | Browsers; AI SDK 5 UI message stream is layered on this when the procedure opts into it |
| `application/octet-stream` | Raw bytes; the procedure must return `ReadableStream<Uint8Array>` | Binary streams (file downloads, model outputs, video chunks) |

The client SDK picks the best `Accept` for the procedure's declared output type and the platform (browser → SSE for text streams; Node → NDJSON; binary stream return → octet-stream).

#### AI SDK 5 UI Message Stream — opt-in

A procedure that wants to drive `useChat` from `ai/react` declares `kind: "stream"` and `wire: "ai-ui-v1"`:

```ts
import { stream } from "@zeroship/server";
import { streamText } from "ai";

export const completion = stream({
  wire: "ai-ui-v1",   // tells the runtime to ship the AI SDK 5 stream protocol
  input: z.object({ messages: z.array(MessageSchema) }),
})(async ({ messages }) => {
  const result = streamText({ model: claude("opus"), messages });
  return result.toUIMessageStreamResponse();   // returns a Response we forward verbatim
});
```

On the wire:

```
HTTP/1.1 200 OK
Content-Type: text/event-stream
x-vercel-ai-ui-message-stream: v1
Cache-Control: no-cache, no-transform

data: {"type":"text-start","id":"t_1"}

data: {"type":"text-delta","id":"t_1","delta":"hello "}

data: {"type":"text-delta","id":"t_1","delta":"world"}

data: {"type":"text-end","id":"t_1"}

data: [DONE]
```

This matches AI SDK 5's UI Message Stream Protocol (the v4 line-prefix format the v1 draft specified is **deprecated** as of AI SDK 5 — round-01 Critical-4). The `x-vercel-ai-ui-message-stream: v1` header is the discriminator; `useChat` verifies it.

For NDJSON or raw streams the response shape is the natural one:

```
# NDJSON
HTTP/1.1 200 OK
Content-Type: application/x-ndjson
Transfer-Encoding: chunked
x-zs-stream: v1

{"json":{"chunk":"hello"}}
{"json":{"chunk":"world"}}
{"json":null,"meta":{"_zs_done":true}}
```

```
# Octet-stream
HTTP/1.1 200 OK
Content-Type: application/octet-stream
Transfer-Encoding: chunked

<binary bytes>
```

#### Stream control frames — normal end and mid-stream errors

The reserved `_zs_*` keys in the superjson `meta` of a streamed envelope are platform-controlled control frames; user payloads must not include `_zs_*` fields (the build refuses any Zod schema that does). Three control frames are defined:

| Control | Wire | Meaning |
| --- | --- | --- |
| `_zs_done: true` | `{ "json": null, "meta": { "_zs_done": true } }` | Normal completion — the stream ended successfully. Always the last frame on a successful stream. |
| `_zs_error: <wire envelope>` | `{ "json": null, "meta": { "_zs_error": { "code", "message", "details", "retryable", "requestId", "traceId" } } }` | Mid-stream error. Always the last frame; no `_zs_done` follows. The `wire envelope` is the same shape §6 errors use elsewhere — no double-wrap. |
| `_zs_keepalive: <epoch_ms>` | `{ "json": null, "meta": { "_zs_keepalive": 1714000000000 } }` | Optional heartbeat — runtime emits every `inactivityMs / 2` to prevent intermediaries from closing idle connections. Clients ignore. |

**SSE wire** — Same control semantics, SSE framing:

```
HTTP/1.1 200 OK
Content-Type: text/event-stream
Cache-Control: no-cache, no-transform
x-zs-stream: v1

data: {"json":{"chunk":"hello"}}

data: {"json":{"chunk":"world"}}

event: error
data: {"code":"INTERNAL","message":"...","retryable":false,"requestId":"req_…","traceId":"…"}

```

The `event: error` line uses the SSE `event:` field to mark the frame as an error; the `data:` line carries the wire-envelope JSON (no superjson `meta` wrapping — SSE consumers don't have a JSON path to thread `meta` through). Normal completion in SSE is `event: end\ndata: {}\n\n`. The presence of `x-zs-stream: v1` distinguishes our SSE wire from the AI SDK 5 wire.

**AI SDK 5 UI Message Stream** — Mid-stream errors map to the protocol's native error shape:

```
data: {"type":"text-start","id":"t_1"}

data: {"type":"text-delta","id":"t_1","delta":"hello"}

data: {"type":"error","errorText":"<message>","errorCode":"<RpcError code>"}

data: [DONE]
```

The mapping rules:

- `RpcError.message` → `errorText` (verbatim if `exposeMessage: true`, otherwise redacted to "Internal server error" per §6).
- `RpcError.code` → `errorCode` (the `ZsErrorCode` enum value, e.g., `"INTERNAL"`, `"UNAUTHENTICATED"`).
- `RpcError.retryable` → not surfaced (AI SDK 5's protocol has no retryable hint).
- `RpcError.details` → not surfaced (private to the structured log).
- The protocol always closes with `[DONE]` after `error`, per AI SDK 5's contract.

**Octet-stream wire** — The runtime cannot frame errors in raw bytes. On a mid-stream error: the runtime emits a chunked-encoding zero-length terminator (closing the stream) and returns the error in HTTP trailer headers `x-zs-error-code`, `x-zs-error-message`, `x-zs-request-id`, `x-zs-trace-id`. Clients reading the body get a clean EOF; clients that read the trailers (rare; HTTP/2 + `TE: trailers`) see the structured error. This mirrors gRPC's `grpc-status` / `grpc-message` trailer convention; clients that already speak gRPC trailers can use the same parser path with the `x-zs-*` namespace. `kind: "raw"` procedures that need rich error reporting on a wire that doesn't support trailers should pick a different wire (NDJSON or SSE).

#### Heartbeats / keepalive

For `kind: "stream"` procedures, the runtime emits `_zs_keepalive` (NDJSON) / `:heartbeat` (SSE comment line) / a no-op AI SDK 5 frame every `inactivityMs / 2` (default: every 15 s for the default 30 s inactivity timeout). The keepalive resets the inactivity counter on intermediaries; clients ignore. This makes long-idle streams (e.g., observability tail logs that emit no data for minutes) survive proxy / load-balancer idle-timeouts.

### Subscription — WebSocket (sketch)

The full subscription contract (backpressure, replay, credit-based flow control, server-emit budget, max-in-flight) ships as a separate proposal `rpc-subscriptions.md`. This proposal commits only to:

- WS upgrade at `/_zs/v1/{wireId}` with `Sec-WebSocket-Protocol: zs.v1`.
- CHWBL routing by app + session (so reconnects land on the same worker; gateway already supports this via `subscription_affinity_key`).
- JSON-frame envelope with at minimum: `{t:"hello", input}`, `{t:"data", value}`, `{t:"error", error}`, `{t:"end"}`, `{t:"ping"}` / `{t:"pong"}`.
- Auth via the standard cookie/header chain on the upgrade request.
- The hooks API in §10 (`fn.useSubscription`).

Backpressure / replay / flow-control specifications are deferred to the dedicated proposal — round-01 High-6 was correct that three paragraphs aren't enough.

### Errors

#### Wire shape

```
HTTP/1.1 <status>
Content-Type: application/zs-error+json
X-Request-Id: req_01HJQK…
traceparent: 00-...

{
  "code":      "INVALID_ARGUMENT",
  "message":   "input.text must be at least 1 character",
  "details":   { "path": ["text"], "expected": "min 1, max 500" },
  "requestId": "req_01HJQK…",
  "traceId":   "abc1234567890abcdef1234567890abc",
  "retryable": false
}
```

The single content-type (`application/zs-error+json`) is the wire signal — clients branch on it. (Status code carries the same signal but `application/zs-error+json` is unambiguous when an upstream proxy mangles status. Round-01 Low-4 wanted full bifurcation; we choose the asymmetric form because successes are nearly always `application/json` already and the value of branding errors specifically is high.)

#### Code enum

gRPC-inspired, fixed:

`UNAUTHENTICATED`, `PERMISSION_DENIED`, `NOT_FOUND`, `INVALID_ARGUMENT`, `FAILED_PRECONDITION`, `ALREADY_EXISTS`, `RESOURCE_EXHAUSTED`, `ABORTED`, `INTERNAL`, `UNAVAILABLE`, `TIMEOUT`, `CANCELLED`, `OUT_OF_RANGE`, `UNIMPLEMENTED`.

| Code | HTTP status | Retryable |
| --- | --- | --- |
| `UNAUTHENTICATED` | 401 | no |
| `PERMISSION_DENIED` | 403 | no |
| `NOT_FOUND` | 404 | no |
| `INVALID_ARGUMENT`, `FAILED_PRECONDITION`, `OUT_OF_RANGE` | 400 | no |
| `ALREADY_EXISTS` | 409 | no |
| `RESOURCE_EXHAUSTED` | 429 | yes (with `Retry-After`) |
| `ABORTED`, `CANCELLED` | 499 | sometimes |
| `TIMEOUT` | 504 | yes |
| `UNAVAILABLE` | 503 | yes (with `Retry-After`) |
| `INTERNAL`, `UNIMPLEMENTED` | 500 | no |

#### `RpcError` is a native `#[v8_class]`

The v1 draft's `RpcError` was a 9-line JS stub with `instanceof` brand checks, which doesn't survive realm boundaries. The platform pattern for branded native classes is `DOMException` (`crates/runtime/src/web/dom/exception.rs`). `RpcError` follows the same shape:

```rust
// crates/runtime/src/rpc/error.rs

#[derive(WebIdlEnum)]
#[webidl(name = "ZsErrorCode")]
pub enum ZsErrorCode {
    Unauthenticated,
    PermissionDenied,
    NotFound,
    InvalidArgument,
    FailedPrecondition,
    AlreadyExists,
    ResourceExhausted,
    Aborted,
    Internal,
    Unavailable,
    Timeout,
    Cancelled,
    OutOfRange,
    Unimplemented,
}

#[derive(Default)]
pub struct RpcError {
    pub code: ZsErrorCode,
    pub message: String,
    pub details: Option<serde_json::Value>,
    pub retryable: bool,
    pub expose_message: bool,
}

#[v8_class]
#[v8_state_marker(RpcError)]
#[v8_to_string_tag = "RpcError"]
#[v8_inherit_intrinsic = "Error"]
impl RpcError {
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        code: ZsErrorCode,
        message: String,
        opts: RpcErrorInit,    // WebIdlDict { details?, retryable?, exposeMessage? }
    ) -> Result<RpcError, OpError> {
        let retryable = opts.retryable.unwrap_or_else(|| code.default_retryable());
        Ok(RpcError {
            code, message,
            details: opts.details,
            retryable,
            expose_message: opts.expose_message.unwrap_or(false),
        })
    }

    #[v8_getter(fastcall)] fn name(&self) -> &'static str { "RpcError" }
    #[v8_getter(fastcall)] fn code(&self) -> ZsErrorCode  { self.code }
    #[v8_getter(fastcall)] fn message(&self) -> String    { self.message.clone() }
    #[v8_getter]           fn details(&self) -> Option<serde_json::Value> { self.details.clone() }
    #[v8_getter(fastcall)] fn retryable(&self) -> bool    { self.retryable }
    #[v8_getter(fastcall)] fn expose_message(&self) -> bool { self.expose_message }
}

/// Brand check via internal-field marker — same machinery DOMException uses.
/// Survives realm boundaries (cross-isolate, cross-context).
pub fn is_rpc_error(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> bool {
    RpcError::is_brand(scope, val)
}
```

The kernel-side error pipeline:

```rust
fn handle_user_thrown(scope: &mut v8::PinScope, exception: v8::Local<v8::Value>) -> WireError {
    if is_rpc_error(scope, exception) {
        // Branded — surface verbatim.
        let err = RpcError::unwrap(scope, exception);
        return WireError::from_rpc_error(err);
    }
    // Not branded — redact in production.
    log_structured_full_error(exception);
    WireError::redacted_internal()
}
```

The brand check uses `#[v8_state_marker(RpcError)]`'s internal-field tag, not `instanceof`. Realm-fragile failure modes from the v1 draft (Critical-3) are gone.

#### Redaction (production)

`RpcError` instances ship `code`, `message`, `details`, `retryable` to the wire verbatim — the user constructed them for the wire.

Anything that is **not** a branded `RpcError` becomes the redacted envelope:

```json
{
  "code": "INTERNAL",
  "message": "Internal server error",
  "requestId": "req_01HJQK…",
  "traceId":   "abc1234...",
  "retryable": false
}
```

The full error (message, stack, type) goes only to the worker's structured log, keyed by `requestId` + `traceId`. The pre-existing `rpc-architecture-critique-2026-05-05.md` Critical-3 (every `throw new Error` ships its message + stack to the wire today) is fixed by this brand check.

To force a non-`RpcError` message through redaction:

```ts
import { RpcError } from "@zeroship/server";
throw new RpcError("INVALID_ARGUMENT", "Email already in use", { exposeMessage: true });
```

#### Dev / staging

- `defineApp({ rpc: { dev: true } })` skips redaction entirely. Useful for staging with production-like routing.
- `NODE_ENV !== "production"` defaults to dev mode (vite dev, SSR-test).
- Per-environment override: `--env staging` → distinct policy. The `dev: true` flag in `defineApp` is a global toggle; the per-environment toggle is a build flag (round-01 Low-3).

### Batching

Multiple **queries OR mutations** in the same client tick auto-merge (round-01 Medium-4 corrected the v1 draft's "queries-only" rule):

```
POST /_zs/v1/_batch
Content-Type: application/zs-batch+json

{ "envelope": "v1", "calls": [
  { "id": "a", "name": "todos.list", "input": { "json": { "limit": 50 } } },
  { "id": "b", "name": "users.me",   "input": { "json": null } },
  { "id": "c", "name": "todos.add",  "input": { "json": { "text": "hi" } }, "idempotencyKey": "01HJQK..." }
] }
```

Response:

```json
{ "envelope": "v1", "results": [
  { "id": "a", "status": 200, "output": { "json": [...], "meta": {...} } },
  { "id": "b", "status": 200, "output": { "json": {...} } },
  { "id": "c", "status": 200, "output": { "json": {...} } }
] }
```

#### Batching rules

- A batched mutation declared `idempotent: true` requires its **own** `idempotencyKey` field on the call (per-call, not per-batch). The batch as a whole is **not** idempotent — the client must reissue with stable per-call keys.
- Streams and subscriptions never batch.
- Mixed-kind batches are allowed (queries + mutations in one HTTP roundtrip).
- Per-call ordering: results are returned in **request order**, regardless of internal completion order.
- Per-call failure isolation: one call failing does not abort others. Each result has its own `status` + envelope. (Same semantics as JSON-RPC batch, MongoDB ordered writes opt-out.)
- Per-call auth/rate-limit: each call is checked independently. Concretely: when call #2 fails auth, calls #1, #3, #4, #5 still run; #2's slot in the response carries `{id, status: 401, error: <wire envelope>}`. The batch never short-circuits on a per-call failure. This unlocks the "anonymous status check + authenticated mutation in one batch" pattern (e.g., a single roundtrip that fetches public config + reads user-scoped data).
- Per-call rate-limit: each call counts independently against the bucket. A batch of 5 calls under a 10/min limit consumes 5 budget units. The whole batch may 429 only if the caller's bucket is empty *before* call #1 — partial-budget mid-batch is not retroactively rejected.

Opt-in per client: `client({ batch: true })`. Default off — batching adds latency for the slowest of the merged calls. The React adapter (where waterfalls are common) defaults batching on.

---

## 7. Manifest — unified resource tree

The `.zship` manifest replaces the v0 `rules` + `policies` split with a single **`resources`** block that unifies routing and protection. Every entry is a **resource** — a URL path, an RPC namespace, or an RPC procedure. Each resource carries optional **policy** fields (auth, rate_limit, cors, cache, csrf, idempotent, max_input_bytes, max_output_bytes, middleware, timeout, publicly_accessible) and an optional **routing action** (redirect, rewrite, static); when no routing action is declared, dispatch falls back to the resource's namespace default.

The artifact / policy split (round-01 High-5) is described in §7c.

### Default dispatch

```
1. resource.redirect ?  → 30x with Location header
2. resource.rewrite  ?  → restart matching with rewritten path (hop limit)
3. resource.static   ?  → serve blob from manifest.assets / runtime_assets
4. key starts with `rpc:` → forward to worker as RPC (default.rpc)
5. key starts with `/`    → forward to worker as SSR (default.fetch)
6. no match               → 404
```

Two namespaces, two default dispatches:

| Namespace | Key format | Default dispatch | Wire URL |
| --- | --- | --- | --- |
| `rpc:` | `rpc:<dotted-id>` | worker as RPC | `/_zs/v1/<id>` |
| `/` | `/<path>` | worker as SSR (or `static`/`redirect`/`rewrite`) | the path itself |

The `rpc:` prefix appears only in manifest keys — the wire URL is `/_zs/v1/<wireId>` with no `rpc:` in it.

### Wire shape

```jsonc
{
  "version": 1,
  "worker": { "entry": "index.js", "modules": { "index.js": "sha256:..." } },

  // Artifact half — immutable per build
  "artifact": {
    "procedures": {
      "rpc:todos.list": { "kind": "query",    "wire": "json"      },
      "rpc:todos.add":  { "kind": "mutation", "wire": "json",      "idempotent": true },
      "rpc:user.uploadAvatar": { "kind": "mutation", "wire": "multipart", "idempotent": true },
      "rpc:chat.completion":   { "kind": "stream",   "wire": "ai-ui-v1" }
    },
    "transformer": "superjson"
  },

  // Policy half — hot-reloadable post-deploy
  "resources": {
    "*": { "auth": "admin", "rate_limit": { "rpm": 60, "per": "ip" } },

    "/api":              { "auth": "user", "override": ["auth"], "cors": { "allow_origins": ["self"] } },
    "/api/admin":        { "auth": "admin", "override": ["auth"] },
    "/api/admin/users":  { "rate_limit": { "rpm": 100, "per": "user" } },
    "/api/public":       { "auth": "anon", "override": ["auth"], "publicly_accessible": true },
    "/api/v1/*":         { "rewrite": "/_zs/v1/*" },

    "/old-blog/[slug]":  { "redirect": { "to": "/blog/[slug]", "status": 302 } },
    "/legacy":           { "redirect": { "to": "/", "status": 301 } },

    "/blog/[slug]":      { "cache": { "max_age": 60, "swr": 300 } },
    "/_assets/*":        { "static": { "try": ["$path"] }, "cache": { "max_age": 31536000, "immutable": true } },

    "rpc:todos":              { "auth": "user", "override": ["auth"], "rate_limit": { "rpm": 600, "per": "user" } },
    "rpc:todos.list":         { },
    "rpc:todos.add":          { "rate_limit": { "rpm": 100, "per": "user" } },
    "rpc:todos.delete":       { "auth": "admin", "override": ["auth"] },

    "rpc:billing":            { "auth": "user", "override": ["auth"] },
    "rpc:billing.charge":     { "middleware": ["transaction"] },
    "rpc:user.uploadAvatar":  { "max_input_bytes": 104857600 },
    "rpc:chat.completion":    { "rate_limit": { "rpm": 30, "per": "user" }, "timeout": { "inactivityMs": 30000, "maxLifetimeMs": 1800000 } }
  },

  "assets":         { /* ... */ },
  "runtime_assets": { /* ... */ }
}
```

### Resource shape

| Family | Fields | Notes |
| --- | --- | --- |
| **Routing action** (at most one) | `redirect`, `rewrite`, `static` | When absent, dispatch is implied by key shape. |
| **Policy** | `auth`, `cors`, `cache`, `rate_limit`, `csrf_origins`, `idempotent`, `middleware`, `max_input_bytes`, `max_output_bytes`, `max_concurrent_per_user`, `max_concurrent_per_app`, `timeout`, `publicly_accessible` | Any combination. |
| **Override marker** | `override: ["auth", "rate_limit", ...]` | Required when this resource shadows an inherited field. |

`kind` and `wire` (the procedure's serialization wire) are **artifact-half** fields — they describe the *function* and don't change post-deploy. They live in `artifact.procedures`, not `resources`. This is the round-01 High-5 split.

### Per-procedure timeout — kind-aware

Round-01 Medium-3 noted `timeout: { ms }` is the wrong shape for streams. Updated:

```ts
fn.config = {
  timeout: { ms: 5000 },                                  // for query / mutation
};

streamingFn.config = {
  kind: "stream",
  timeout: {
    inactivityMs: 30000,                                  // no data for 30s → abort
    maxLifetimeMs: 1800000,                               // hard kill at 30 min
  },
};
```

For `kind: "stream"` and `kind: "subscription"`, `timeout: { ms }` is a build error.

For all kinds, the **gateway-imposed deadline** (default 60 s for query/mutation; `maxLifetimeMs` for stream/subscription) wraps the procedure timeout — whichever is tighter wins, and `ctx.signal` aborts when it fires.

### Authoring — `defineApp` in `src/server/config.ts`

The vite-plugin reads exactly one config file: `<projectRoot>/src/server/config.ts`. It carries (a) the resource tree and (b) app-level RPC defaults that procedures inherit when they don't override.

The config must be a **literal** — no computed expressions, no env-var reads, no imports of values. The build AST-walks the `defineApp` argument as a literal (`literalize()` in `transform.ts`). This is round-01 Medium-9 — the v0 impl `Function()`-eval'd the file, which was a security and correctness bug. We pick "literal-only" as the rule.

```ts
// src/server/config.ts
import { defineApp } from "@zeroship/server";

export default defineApp({
  rpc: {
    defaults: {
      auth:      "user",
      rateLimit: { rpm: 600, per: "user" },
      timeout:   { ms: 30000 },
    },
  },

  resources: {
    "*": { auth: "admin", rateLimit: { rpm: 60, per: "ip" } },

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
        "delete": { auth: "admin", override: ["auth"] },
      },
    },
  },
});
```

`children: {...}` is authoring sugar. The build flattens to fully-qualified keys, prepending the parent's namespace prefix: `"rpc:todos"` + child `"delete"` → `"rpc:todos.delete"`; `"/api"` + `"admin"` → `"/api/admin"`. The wire is the flat map.

### Per-field merge rules

For any resolved resource, walk the most-specific match's parent chain (root `*` → ancestors → matched node) and merge per field:

| Field | Merge | Notes |
| --- | --- | --- |
| `auth` | **stricter wins** (admin > user > anon) | Override marker required to weaken |
| `rate_limit`, `max_input_bytes`, `max_output_bytes`, `timeout` | **min** | Stricter cap survives |
| `cors.allow_origins`, `cors.allow_methods`, `cors.allow_headers`, `csrf_origins` | **intersect** | Child can only narrow |
| `cors.allow_credentials`, `cors.max_age_seconds` | **child overrides** | Per-resource decision |
| `middleware` | **append** (root → child order) | All run; child can't drop ancestor's |
| `cache`, `idempotent`, `publicly_accessible` | **child overrides** | Per-resource decision |

The `override: [...]` marker is required when a child weakens or widens an inherited field. Without it the build refuses: *"`todos.delete` declares `auth: user` but inherits `auth: admin` from `todos`. Add `override: ["auth"]`."*

### 7c. Artifact / policy split

Round-01 High-5 noted that the v1 draft conflated artifact (worker entry, modules, blobs — content-addressed, immutable per build) with policy (resources, rate limits — mutable per deploy). Stripe, AWS API Gateway, Cloudflare all separate these.

| Half | Lives in | Mutable post-deploy? | Push channel |
| --- | --- | --- | --- |
| Artifact | `manifest.artifact` + content-addressed blob storage | No | Full bundle redeploy |
| Policy | `manifest.resources` | Yes | Control plane → gateway HTTP pull every 5 s |

A creator who wants to lower a rate limit edits `defineApp.resources` and pushes via `zeroship policy push` — no full redeploy, no V8 isolate restart. The control plane already pushes route updates every 5 s; policy updates piggyback on the same channel.

The **artifact half** is immutable by content hash. A new policy push that references a procedure not in the current artifact is rejected.

### Gateway compilation (at app load)

When the gateway receives `manifest.json`, it pre-computes for every resource an `EffectivePolicy` (flattened via the merge rules) and stores `HashMap<MatchKey, EffectivePolicy>` keyed for O(1) lookup. Per request:

```
1. Identify namespace from the request:
   - URL begins with /_zs/v1/  → strip prefix; lookup key = "rpc:" + remainder
   - Otherwise                 → lookup key = the URL path
2. Match against the resources map (most-specific wins):
   - rpc:  walk dot-segment Trie
   - URL:  walk path-segment Trie
3. Look up precomputed EffectivePolicy.
4. Enforce pre-dispatch policy:
   - auth check (ZeroShip-User HMAC + JWT)  → 401 / 403 if rejected
   - rate-limit bucket                       → 429 if exceeded
   - csrf origin check                       → 403 if mutation + Origin not allowed
   - max_input_bytes guard                   → 413 if exceeded
   - HTTP method ↔ kind                      → 405 if GET on kind: "mutation"
   - max_concurrent_per_user / _app          → 429 if exceeded
5. Idempotency-Key check (if procedure idempotent: true) — see §8
6. Determine action:
   - redirect → 30x
   - rewrite  → restart matching with rewritten path (hop limit)
   - static   → serve blob
   - else rpc:/url → forward to worker
7. On response: apply policy.cors, policy.cache headers (queries only).
8. Emit meter events (§12).
```

The gateway never walks a tree at runtime — `EffectivePolicy` is precomputed once at load and cached.

### What the gateway enforces without involving the worker

- Pre-404 unknown resources.
- Reject wrong HTTP method (procedure `kind: "mutation"` via `GET` → 405).
- Validate auth (ZeroShip-User HMAC + JWT) for `auth: "user"`/`"admin"` — 401 before forwarding.
- Enforce `rate_limit` per the declared bucket — 429 with `Retry-After` and `X-RateLimit-Reset`.
- Reject oversized inputs (`max_input_bytes`) — 413.
- Reject oversized outputs (`max_output_bytes`) — 502 + structured log.
- Enforce concurrent-call caps (`max_concurrent_per_user`, `max_concurrent_per_app`) — 429.
- CORS preflight short-circuit.
- Set `Cache-Control` and `ETag` for queries on the response path.
- Emit Prometheus + meter events keyed by resource id.
- Inject `traceparent` if absent; thread through.
- Verify HMAC on `ZeroShip-User` header before forwarding to worker.

The worker only sees pre-validated, pre-authenticated, pre-rate-limited, pre-idempotency-checked requests. **One source of truth** for routing AND policy: the `resources` block.

### Validation

Build-time and gateway-load-time checks:

- **Cycles** — inheritance chain must be acyclic.
- **Override marker sanity** — every shadowed field needs `override: [field]`. Build error otherwise.
- **Routing-action exclusivity** — at most one of `redirect`, `rewrite`, `static` per resource.
- **Glob format** — `[name]` single-segment, `[...rest]` multi-segment, `*` wildcard.
- **Secure-by-default** — `auth: "anon"` requires `publicly_accessible: true` on the same resource. Build error in production mode (warning in dev).
- **Procedure kind matches HTTP method** — `kind: "mutation"` cannot be served via `GET`.
- **`idempotent: true` only on mutations** — round-01 Medium-2. Build error otherwise.
- **Resource-key format** — `rpc:` prefix followed by `[a-zA-Z0-9._*-]+` for RPC namespace; `/`-prefixed glob for URL namespace; bare `*` for the root default. Any other shape is a build error.
- **Reserved field names** — `_zs.json` and any field whose name begins with `_zs.` (in user form schemas) or `_zs_` (in user JSON schemas, including streamed payloads) is forbidden. Build error.
- **Live-version cap** — at most 3 live versions per procedure in the artifact manifest (§13). Build error otherwise.
- **Anonymous mutation idempotency** — when a procedure has both `auth: "anon"` and `idempotent: true`, the build emits a warning naming the high-entropy-key requirement (gateway-enforced at runtime, but caught early at build time).
- **`breakingOk: true` attestation** — when a wire-compat check would otherwise fail, the wrapper's `breakingOk: true` field bypasses the check with a warning (recorded in the audit log). Without it, a wire-breaking change is a build error in `--mode production`.

The existing `Manifest::validate()` in `crates/core/src/types.rs` absorbs these.

### Literal-only `defineApp` — dev mode parity

The literal-only rule (§7c) is enforced both at build time and at dev-server startup. `vite dev` runs the same AST extraction over `src/server/config.ts`; if the file contains computed expressions or imported values, dev mode emits the same error as production. This avoids the case where a creator's app works in dev (where the config file was evaluated as a module) but fails at deploy time (where it must be a literal).

---

## 8. Idempotency

Mutations declare `idempotent: true` to opt in. Effect:

- Wire **requires** `Idempotency-Key` header (gateway returns 400 if missing).
- Server-side dedupe keyed by `(app_id, wireId, idempotency_key)`. Each entry stores `(input_hash, output, status, completed_at)`. Default TTL 24 h, configurable per procedure via `fn.config.idempotencyTtl: { hours: 168 }` (max 7 days).
- Storage: `zeroship.kv` (Redis-backed in prod, in-memory in dev). Each entry costs ~200 B; quota counted against the app's KV allowance.
- Mutations **without** `idempotent: true` must not include `Idempotency-Key` (gateway returns 400 — round-01 Medium-2). Future-proofs against ambiguous keys.

### Distributed lock — the missing primitive

Round-01 Critical-5: with N workers behind CHWBL, the same key can land on different workers; per-process mutex doesn't help. We pick **Redis SETNX with TTL**, gateway-enforced.

```
Gateway pseudocode for the in-flight branch:
─────────────────────────────────────────────────
key = sha256(app_id || wire_id || idempotency_key)
input_hash = sha256(canonical-superjson(envelope))

# Step 1: try to acquire the lock atomically.
if SET key:lock <gateway_id> NX EX 30:
    # Acquired. Record the input hash and the in-flight marker.
    HSET key:meta input_hash <input_hash> state in_flight gateway <gateway_id>
    EXPIRE key:meta 30                                      # lock TTL
    forward request to worker
    on response:
        HSET key:meta state completed output <output> status <status>
        EXPIRE key:meta <fn.config.idempotencyTtl or 24h>   # store TTL
        DEL key:lock
        return response

# Step 2: lock not acquired — another worker holds it.
loop with backoff (max 30 s, capped by fn.config.timeout):
    sleep 50ms..500ms
    state = HGET key:meta state
    if state == completed:
        # Original finished — return its result.
        existing_input_hash = HGET key:meta input_hash
        if existing_input_hash != input_hash:
            return 409 ALREADY_EXISTS (different input)
        return cached output / status
    if state is gone (lock holder died, key expired):
        # Restart from step 1 with a fresh attempt.
        retry the whole flow
    if elapsed > deadline:
        return 409 ABORTED ("idempotency lock wait expired")
```

This is gateway-side, not worker-side: the gateway is always the choke point for a given app's URL space (CHWBL ensures it; even multi-gateway deployments share the same Redis cluster). Picking gateway-side avoids the second-request-on-different-worker bug.

#### Redis primitives used

All ops go through `compio-redis` (the platform's Redis driver — zero-tokio, compio-native, cluster-aware; `crates/compio-redis/src/{client,cluster}.rs`):

- `SET {idem:<key-hash>}:lock value NX EX 30` — atomic acquire (Redis 2.6.12+ canonical pattern).
- `HSET {idem:<key-hash>}:meta` / `HGET {idem:<key-hash>}:meta` — meta storage.
- `EXPIRE` — TTL extension.
- `EVAL <release-lua>` — Lua script that `GET`s `{idem:<key-hash>}:lock`, compares to the holder ID, and `DEL`s only on match. Prevents releasing another holder's lock.

The `{idem:<key-hash>}` hash-tag wrapper (Redis Cluster keyspace notation) ensures `:lock` and `:meta` keys hash to the same Cluster slot, which is required for `EVAL` to operate on both atomically. Without the tag, Cluster mode would split the keys across slots and the script would fail.

We add a thin `crates/gateway/src/idempotency.rs` extension for the SETNX flow with the Lua release script.

#### Failure modes (documented)

This is Stripe-inspired with a Redis-side variant: Stripe uses a server-side row lock on the mutating record; we use a separate Redis keyspace because our handler-write paths are heterogeneous (Postgres, KV, S3, external HTTP). The semantic guarantees match.

1. **Lock holder dies before completing.** `{idem:<key-hash>}:lock` TTL of 30 s expires; second request re-acquires; runs the handler. Same as Stripe — the result of the dead handler is lost; the second execution may have different side effects but the second's `idempotency_key` semantics still hold for the second's caller.
2. **Lock holder is slow (longer than 30 s but still alive).** Lock TTL must exceed `fn.config.timeout` + buffer. We enforce `lock_ttl = max(fn.config.timeout * 2, 30s) + 5s` at gateway-load time. (Practical guideline from Redis distributed locks: 2× expected duration + network buffer.)
3. **Redis fails over mid-acquire.** Single-instance Redis lock has a limitation: the lock may be lost during failover. We accept this as a rare-violation case; the application is expected to be designed for idempotency. Redlock is **not** used (operational complexity, controversial in the literature). For applications that need stronger guarantees, the pattern is to make the **handler itself** idempotent (by writing to a database with a unique key, or `ON CONFLICT DO NOTHING`) — the lock is best-effort optimization, not safety-critical.

### Input hash — how the gateway compares payloads

The "input hash" used in the collision rules below is computed differently per `wire`:

| `wire` | Input hash |
| --- | --- |
| `"json"` (envelope) | `sha256(canonical-superjson(envelope.json))` — `meta` excluded (it's a serialization detail, not the value) |
| `"multipart"` | `sha256(_zs.json-canonicalized || sorted(name → sha256(part-body)))` — combines structured fields and binary part hashes |
| `"raw"` | `sha256(method || url-with-query || sorted(content-relevant-headers) || body)` — content-relevant headers are `Content-Type`, `Content-Length`, `Content-Encoding`. Headers known to vary per request (`Date`, `traceparent`, `User-Agent`, `X-Request-Id`, `Authorization`, `Cookie`, `X-Zs-Csrf-Token`, `Idempotency-Key`) are excluded. |
| `"ai-ui-v1"` (stream) | Streams cannot be replayed → `idempotent: true` is forbidden on streams (build error). |

For multipart, the same `_zs.json-canonicalized || part hashes` rule applies regardless of multipart boundary or part ordering — the hash is canonical.

### Collision rules — what happens on key reuse

The `(app_id, wireId, idempotency_key)` tuple has three possible states:

1. **Same key, same input hash, within TTL.** Return the stored response (`200 OK` with cached body, or the cached error envelope). Handler not re-invoked.

2. **Same key, *different* input hash, within TTL.** Caller is reusing the key with a different payload — almost always a bug:

   ```
   HTTP/1.1 409 Conflict
   Content-Type: application/zs-error+json
   X-Idempotency-Key-Reused: true
   X-Original-Completed-At: 2026-05-05T12:34:56Z

   {
     "code": "ALREADY_EXISTS",
     "message": "Idempotency-Key was used with a different input within the dedupe window",
     "details": { "reason": "idempotency_key_reused_with_different_input" },
     "retryable": false
   }
   ```

   We do **not** use `Retry-After` for this 409 (round-01 Nitpick). RFC 9110 lists 503 / 429 / 3xx as the canonical `Retry-After` users. We use a custom `X-Original-Completed-At` for context.

3. **Same key while the original is still in flight.** The flow above (gateway SETNX + polling). On wait expiry: `409 ABORTED` with `code: "ABORTED"`.

`ALREADY_EXISTS` and `ABORTED` are existing entries in §6's error code table.

### Storage cap (round-01 Low-5)

- Hard cap per app: 1 M live keys (configurable via `defineApp.kv.idempotencyCap`).
- Eviction policy: **LRU**, surfaced via Redis `MEMORY POLICY allkeys-lru` on the dedicated idempotency keyspace. `compio-redis` exposes the keyspace setup.
- A request whose key was just evicted (false-cache-miss) re-runs the handler. The handler must remain idempotent against the underlying data store; the cache is best-effort.
- Eviction emits a warn-level log line per 1000 evictions, surfaced in the creator's usage dashboard.

### Anonymous mutations + idempotency

When `auth: "anon"` and `idempotent: true` both apply to a mutation, the dedupe tuple `(app_id, wireId, idempotency_key)` has no user partition — two anonymous clients picking the same `Idempotency-Key` would otherwise see each other's responses (cross-user leak).

To prevent this, anonymous mutations require **high-entropy** idempotency keys:

- The key must parse as either a UUIDv4 (random) or UUIDv7 (timestamp + random).
- If the key fails the parse, the gateway returns `400 INVALID_ARGUMENT` with `details.reason: "anonymous_idempotency_key_must_be_uuid_v4_or_v7"` — and the build's `idempotent: true` declaration on an `auth: "anon"` mutation emits a warning at deploy time naming this constraint.
- Collision probability with proper UUIDs is `2^-122` (UUIDv4) or `2^-74` per ms (UUIDv7) — astronomically safe.

For authenticated mutations (`auth: "user"`/`"admin"`), the dedupe tuple is `(app_id, wireId, idempotency_key, user_id)` — naturally partitioned. The UUID-only constraint does not apply; any string up to 255 chars is accepted (matches Stripe).

### RPC-on-RPC — server function calling another server function

A common pattern: a server function `add()` imports and calls another server function `recompute()` to invalidate caches. With the seamless model, both are in the same server bundle; the import resolves to a direct call.

**Default: direct call**, NOT a wire roundtrip.

- The inner function's body runs in the same V8 turn as the outer's; the same `ctx` is shared (since ALS propagates).
- The inner function's wrapper schemas (`fn.config.input`/`output`) **still validate** — wrappers are part of the function value, not the wire boundary.
- The inner function's middleware (`transaction`, etc.) **still runs** — middleware is part of the wrapper.
- The inner function's `idempotent: true` is **bypassed** — the dedupe table is gateway-side; direct calls don't traverse the gateway. (This matches the natural mental model: the outer call's idempotency wraps the inner work; the inner key would be redundant.)
- The inner function's `rate_limit` is **bypassed** — same reasoning.
- A `rpc.fanout` meter event is emitted for each inner call: `{app, outer_procedure, inner_procedure}`. The audit log shows the call tree.

**Forced-wire alternative**: if the creator wants the inner call to traverse the gateway (rate-limit, idempotency, full meter) — for instance, to enforce a tenant quota — they call the URL via `fetch`:

```ts
import { recompute } from "../actions/cache";
import { ctx } from "@zeroship/server";

export const add = mutation({...})(async ({ text }) => {
  const t = await db.todos.insertOne({ text });

  // Default: direct call.
  await recompute({ ownerId: ctx.user.id });

  // Forced wire (rare): same procedure, full HTTP roundtrip.
  await fetch(new URL("/_zs/v1/cache.recompute", ctx.url.origin), {
    method: "POST",
    headers: { "content-type": "application/json", "authorization": ctx.headers.get("authorization") ?? "" },
    body: JSON.stringify({ json: { ownerId: ctx.user.id } }),
  });

  return t;
});
```

The default direct call is what creators want 99% of the time. The forced-wire pattern exists for the rare cases where rate-limiting an inner call matters.

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

`client<App>` and the seamless import-and-call surface emit identical wire bytes (the same superjson envelope). There is one wire encoder, used by both. Round-01 Critical-2's "auto-stub strips meta; manual client preserves meta" mismatch is gone.

### React

```tsx
import { rpcReact } from "@zeroship/rpc/react";
const trpc = rpcReact<App>();

function Todos() {
  const { data, error, isLoading } = trpc.todos.list.useQuery({ limit: 50 });
  const add = trpc.todos.add.useMutation({
    onSuccess: () => trpc.todos.list.invalidate(),
  });
  // ...
}
```

Built on TanStack Query — invalidation, optimistic updates, suspense, SSR hydration all free.

### Vercel AI SDK 5

Procedures declared `kind: "stream"` + `wire: "ai-ui-v1"` ship the AI SDK 5 UI Message Stream Protocol verbatim:

```tsx
import { useChat } from "ai/react";

const { messages, append } = useChat({
  api: rpc.chat.completion.streamUrl({ model: "<modelId>" }),
});
```

No bespoke parser; no adapter shim. The runtime emits `x-vercel-ai-ui-message-stream: v1` + the SSE start/delta/end frames; `useChat` consumes them as-is.

For other AI SDK adapters (e.g., `useCompletion`, `useObject`), the same `wire: "ai-ui-v1"` mode applies — the runtime emits the v5-compliant stream protocol.

### Non-JS clients

Out of scope for the initial release. The wire is plain HTTP+JSON; non-TS callers (Python, Swift, Go, mobile native) hit `POST /_zs/v1/<id>` with a JSON envelope and parse the JSON response. They lose static typing across the boundary; if/when typed multi-language clients become a real need, the path is **OpenAPI emission** via `zod-to-openapi` → `openapi-generator-cli`. Not bespoke per-language generators we maintain.

---

## 10. React Query integration

The seamless model and TanStack Query (React Query) compose cleanly: every procedure imported from a server module is **simultaneously a callable function and a hooks object**. Same import, three usage modes.

```tsx
import { list, add } from "../actions/todos";

function Todos() {
  const { data } = list.useQuery({ limit: 50 });
  const { data } = list.useSuspenseQuery({ limit: 50 });

  const addTodo = add.useMutation({
    onSuccess: () => list.invalidate(),
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
// @zeroship/rpc/client (excerpt — see §5 for the full module)
export function __makeProcedure(meta) {
  const fn = (input, opts) => __rpcCallByWire(meta, input, opts);
  fn[__SERVER_REFERENCE] = true;
  fn.id = meta.id; fn.kind = meta.kind;
  fn.queryKey = (input) => [meta.id, input];

  if (meta.kind === "query") {
    Object.defineProperty(fn, "useQuery", { get: () => /* uses _hookRegistry */ });
    Object.defineProperty(fn, "useSuspenseQuery", { get: () => /* ... */ });
    Object.defineProperty(fn, "useInfiniteQuery", { get: () => /* ... */ });
    fn.invalidate = (input) => /* ... */;
    fn.prefetch  = (input, options) => /* ... */;
    fn.setData   = (input, updater) => /* ... */;
  }
  if (meta.kind === "mutation") {
    Object.defineProperty(fn, "useMutation", { get: () => /* ... */ });
  }
  if (meta.kind === "stream") {
    Object.defineProperty(fn, "useStream", { get: () => /* ... */ });
    fn.streamUrl = (input) => /* returns a string URL with input pre-encoded */;
  }
  if (meta.kind === "subscription") {
    Object.defineProperty(fn, "useSubscription", { get: () => /* ... */ });
  }

  return fn;
}
```

Hooks are attached only for the matching `kind`; a query never carries `useMutation`. Stable query key shape: `[wireId, input]` for queries / streams / subscriptions; `[wireId]` for mutations.

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
| Stale-while-revalidate | Manifest's `cache.swr` becomes default `staleTime` per procedure |
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

The auto-generated key is bound to the React Query mutation observer's lifetime, **not per HTTP attempt**. Procedures without `idempotent: true` get `retry: 0` automatically — non-idempotent retries surface to the app to handle explicitly.

### Streaming + React Query

```tsx
const { chunks, isStreaming, error, cancel } = search.useStream({ query: "..." });
```

For chat UIs: skip our hook and pass `search.streamUrl(input)` to AI SDK's `useChat`. Same SSE wire, two consumers.

#### `streamUrl` security note

`streamUrl(input)` returns a URL with the input pre-encoded in the query string. This is fine for idempotent queries with non-sensitive inputs. For inputs that include credentials, secrets, or user-identifying data the URL may end up in browser history, intermediary access logs, or `Referer` headers — prefer `useStream`/`call` which posts the input as a body. The runtime emits a build warning when a procedure declared `auth: "user"`/`"admin"` exposes `streamUrl` and the input schema includes any field annotated `z.string().secret()` (a Zod refinement marker).

### Subscriptions

```tsx
const { value, isConnected, error } = todoChanges.useSubscription();
```

Auto-reconnect with exponential backoff. Unmount triggers unsubscribe. The full backpressure / replay / credit-based-flow-control protocol is deferred to `rpc-subscriptions.md` (round-01 High-6).

### Server-side rendering

When the worker runs React SSR, the same component code calls `list.useQuery(...)` on both server and client. On the client, `list` is a `__makeProcedure`-wrapped stub (HTTP). On the server, `list` is the actual function — and **must also expose `.useQuery` / `.prefetch`** (the same hook surface), or render breaks.

For SSR-enabled apps, the server transform wraps each procedure with `__makeServerProcedure` from `@zeroship/rpc/server`. Server-side hooks call the local function directly (no HTTP); results land in the per-request QueryClient and dehydrate to the client.

```ts
// @zeroship/rpc/server (excerpted)
import { useQuery, useSuspenseQuery } from "@tanstack/react-query";

export function __makeServerProcedure(impl, meta) {
  const fn = (input) => impl(input);
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
  return fn;
}
```

Mutations and subscriptions don't render server-side — `useMutation` / `useSubscription` resolve to no-ops that throw if invoked during SSR.

### Per-procedure `staleTime`

The resource's `policy.cache.max_age` (when set on a query) becomes the default `staleTime`. Per-call override still works:

```tsx
list.useQuery({ limit: 50 }, { staleTime: 5_000 });
```

Aggregate invalidation by prefix:

```tsx
import { rpcInvalidate } from "@zeroship/rpc/react";
await rpcInvalidate("todos.");
```

---

## 11. Worked scenarios

### A — creator builds a CRUD todo app

1. Writes `src/actions/todos.ts` starting with `"use server"`. Plain async exports for `list`, `get`, `add`, `complete`, `delete`. No wrappers.
2. `add.config = { idempotent: true, id: "todos.add" }` to pin the wireId and make AI-driven retries safe.
3. `vite build` → transform discovers procedures by directive (file-level), infers `kind: "mutation"` for `add`, emits `manifest.artifact.procedures` + `manifest.resources`. Zod schemas on `fn.config.input`/`output` validate at runtime, writes `virtual:zeroship/server-api.d.ts`.
4. Deploys via `zeroship deploy`. Gateway picks up the new manifest in ≤5 s.

### B — end user (mobile, native iOS) calls the same API

1. iOS dev hits the wire directly using `URLSession`:
   ```swift
   let url = URL(string: "https://app.zeroship.ai/_zs/v1/todos.list?input=" + base64url(envelope))!
   var req = URLRequest(url: url)
   req.setValue("Bearer \(jwt)", forHTTPHeaderField: "Authorization")
   let (data, _) = try await URLSession.shared.data(for: req)
   let envelope = try JSONDecoder().decode(SuperjsonEnvelope<[Todo]>.self, from: data)
   ```
2. Gateway validates JWT, checks rate limit, forwards to worker. Worker validates input, runs handler, returns superjson-encoded response.
3. Untyped relative to the source — typed clients in non-TS langs are deferred until OpenAPI emission lands.

### C — AI-built chat app

1. Creator's app: `export const completion = stream({ wire: "ai-ui-v1", input: ... })(async ({ messages }) => streamText({...}).toUIMessageStreamResponse())`.
2. Frontend: `useChat({ api: rpc.chat.completion.streamUrl({}) })`. AI SDK 5 consumes the SSE.
3. **Zero custom protocol code.**

### D — refactor day

1. Creator reorganizes: renames `src/actions/todos.ts` → `src/server/features/todos.ts`. Files can live anywhere — no path convention.
2. Procedure ids (`todos.list`, `todos.add`, ...) don't change — they're explicit `fn.config.id` values.
3. Wire identity preserved.

### E — incident: a hot path saturates the worker

1. `todos.list` is hammered.
2. Creator updates `defineApp.resources["rpc:todos.list"].rate_limit = { rpm: 100 }`. Pushes via `zeroship policy push` — no full redeploy.
3. Gateway 429s the offender within 5 s.
4. `Retry-After` tells the partner client to back off.

### F — adding an optional field

1. Creator changes `add`'s input from `{ text }` to `{ text, dueDate?: Date }`.
2. Old clients still send `{ text: "hi" }`. Schema accepts (`dueDate` optional).
3. **Removing** a field requires deprecation: declare under a new `id` (`todos.add.v2`), keep the old for N months, codemod the client. See §13.

### G — auth-aware errors

1. End user's session expires mid-call.
2. Gateway returns `{ code: "UNAUTHENTICATED" }` 401 with `WWW-Authenticate: Bearer realm="zeroship"`.
3. Client SDK detects code, fires `onAuthExpired` hook, redirects to login.
4. After re-auth, SDK retries idempotent calls automatically (the auto-bound idempotency key from §10 makes this safe); non-idempotent ones surface to the app.

### H — file upload

1. Creator: `export async function uploadAvatar(file: File) { const url = await storage.put(file); return { url }; }`.
2. Client: `await uploadAvatar(blob)`. The transform sees the `File` parameter and routes the call as multipart automatically. No base64 path.
3. Server handler receives a native `File` (not base64). Calls `storage.put(...)`. Native multipart parser does the work.
4. The progressive-enhancement form pattern works:
   ```tsx
   <form action={uploadAvatar}><input type="file" name="avatar" /></form>
   ```
   When JS is disabled, the form posts multipart natively to the procedure's URL; the runtime parses it and calls the handler with a `File`. Same handler code, two paths — JS-driven and form-driven.

### I — webhook handler (raw escape-hatch)

1. Creator: `export const handler = action({ kind: "raw" })(async (req: Request) => { ... })`.
2. Wire: Stripe POSTs JSON; the handler reads `req.headers.get("stripe-signature")`, verifies, processes.
3. Auth/rate-limit still apply (gateway-side).

---

## 12. Observability

### Metrics — auto-emitted per RPC call

Every RPC call emits a meter event from the gateway. Round-01 Medium-10: this was missing in v1; now it's wired by construction.

| Metric | Unit | Tags | Where |
| --- | --- | --- | --- |
| `rpc.requests` | count | `{app, procedure, status, kind, wire}` | Gateway, on response close |
| `rpc.duration_ms` | histogram | `{app, procedure, kind}` | Gateway, on response close |
| `rpc.cpu_us` | counter | `{app, procedure}` | Worker, returned via response trailer |
| `rpc.ingress_bytes` | counter | `{app, procedure, wire}` | Gateway |
| `rpc.egress_bytes` | counter | `{app, procedure, wire}` | Gateway |
| `rpc.errors` | count | `{app, procedure, code}` | Gateway, when `code != null` |
| `rpc.idempotency_hits` | count | `{app, procedure}` | Gateway, when serving cached idempotency response |
| `rpc.stream_chunks` | counter | `{app, procedure}` | Gateway, per chunk emitted |

These layer on the platform's existing `requests` / `cpu_us` / `wall_us` / `ingress_bytes` / `egress_bytes` core metrics (`docs/reference/billing-metering.md`). RPC adds the `procedure` dimension; the underlying counters are the same. **Billing is a free side-effect** — every RPC call is a billable event by construction (matches AWS API Gateway, Stripe).

### Tracing — W3C trace context

- Gateway generates a 16-byte `trace-id` (32 hex chars) per inbound request if `traceparent` is absent, mirroring W3C Trace Context level 2.
- `traceparent: 00-<trace-id>-<gateway-span-id>-01` is propagated to the worker.
- The worker constructs `ctx.traceId` from the propagated header.
- Outgoing fetches from the procedure auto-inject `traceparent` (the runtime's native `fetch` extends the existing header, creating a child span).
- The runtime emits structured logs with `{traceId, spanId, parentSpanId}`; the gateway optionally exports OTLP via `defineApp({ observability: { otelEndpoint: "..." } })`.

#### Streaming spans

For `kind: "stream"` procedures, the default span model is **one span per stream**:

- Span starts when the procedure's first frame is emitted.
- Span ends when the stream ends (`_zs_done`, `_zs_error`, or client disconnect).
- Per-frame events are recorded as span events, not separate spans (`stream.frame` event with `frame.bytes`, `frame.kind`).

Apps that need per-emit spans (e.g., to observe each LLM token-block as a span) opt in via `defineApp({ observability: { streamChunkSpans: true } })`. With the flag on, each emitted chunk is its own span as a child of the stream span. Cost: the OTel cardinality scales with chunk volume; only enable when needed.

### Request ID

- Gateway generates a `typed_id` UUIDv7 per request: `req_<base62>`.
- Echoed in `X-Request-Id` response header.
- Logged on every error response and every meter event.
- Distinct from `traceId` (16-byte W3C ID; correlation across spans). `requestId` is the human-debug handle ("look up request `req_abc123`"); `traceId` is the OTel correlation key.

### Structured logs

```json
{
  "ts":         "2026-05-05T12:34:56.789Z",
  "level":      "info",
  "msg":        "rpc.completed",
  "app":        "app_abc",
  "procedure":  "todos.add",
  "wireId":     "todos.add",
  "kind":       "mutation",
  "status":     200,
  "code":       null,
  "userId":     "usr_xyz",
  "requestId":  "req_01HJQK...",
  "traceId":    "abc123...",
  "durationMs": 23,
  "cpuUs":      1850,
  "ingressBytes": 142,
  "egressBytes":  890,
  "idempotencyKey": null
}
```

`ctx.log.{info,warn,error,debug}({ msg, ...fields })` is the user-facing API; the runtime auto-merges `{app, procedure, wireId, requestId, traceId, userId}` into every log line.

### Other operability surfaces

| Surface | Where |
| --- | --- |
| Prometheus metrics | Gateway emits per-procedure counters/histograms (the meter events double as Prometheus-scrapable values) |
| Replay protection | Idempotency table per app, 24 h TTL default, in `zeroship.kv` |
| Audit log | Every mutation logs `{traceId, requestId, procedure, userId, idempotencyKey, status}` |
| Method-level dashboards | Auto-generated in creator console from `manifest.artifact.procedures` — one tile per `wireId` showing `requests`, `p50/p99 duration_ms`, `error rate`, `idempotency hit rate` |
| Per-procedure tail logs | `zeroship logs tail --procedure todos.add` |

---

## 13. Versioning

The v1 draft punted procedure versioning to creators ("declare `todos.add.v2` next to `todos.add`"). Round-01 Major: that's *application*-level versioning treated as protocol. Real production needs platform-level versioning hooks.

### Three layers

| Layer | Mechanism | Who decides | Migration |
| --- | --- | --- | --- |
| **Wire-protocol version** | URL prefix `/_zs/v1/` | Platform | Major bump → `/_zs/v2/` co-exists for sunset window; gateway routes both. |
| **Manifest schema version** | `manifest.version: 1` field | Platform | `manifest.version: 2` is the future shape. Older gateways reject the wrong version with a clear error. |
| **Procedure version** | Field on procedure metadata: `fn.config.version: "2"` (defaults to `"1"`) **and** request header `Zs-Procedure-Version: <version>` | Creator, but platform-aware | Multiple versions of the same `wireId` co-exist; gateway routes by header; default version pinned in manifest. |

### Procedure versioning — the platform-aware shape

Round-01 Major-9: the v1 draft declared `todos.add.v2` as a sibling procedure, which gives the gateway no notion of "v1 deprecated." Now:

```ts
// Both versions exported from the same module
export const add = mutation({
  version: "1",   // current default
  input: z.object({ text: z.string().min(1).max(500) }),
})(async ({ text }) => { /* ... */ });

export const addV2 = mutation({
  id:      "todos.add",     // same wireId
  version: "2",
  input: z.object({ text: z.string().min(1).max(500), dueDate: z.date().optional() }),
  deprecates: { version: "1", sunsetAt: "2026-12-31" },   // marks v1 deprecated
})(async ({ text, dueDate }) => { /* ... */ });
```

Manifest:

```jsonc
"artifact": {
  "procedures": {
    "rpc:todos.add": {
      "kind": "mutation",
      "wire": "json",
      "versions": {
        "1": { "default": true,  "deprecated": false },
        "2": { "default": false, "deprecated": false, "supersedes": "1", "supersededAt": "2026-05-05" }
      }
    }
  }
}
```

The gateway:

- Routes by `Zs-Procedure-Version` header. Absent → uses `default: true` version.
- For deprecated versions: emits `Deprecation: true` + `Sunset: <date>` response headers per RFC 8594 (Sunset HTTP header).
- Tracks per-version metrics (`rpc.requests` tagged with `procedure_version`).
- The creator console shows usage of pinned-deprecated versions; alerting fires when sunset is near and traffic remains.

#### Live-version cap

Per-procedure versioning multiplies the gateway's lookup-table size by `O(versions_per_procedure)`. To keep this bounded:

- **Hard cap: 3 live versions per procedure** in the artifact manifest. Build error otherwise.
- A *live version* is one in the deployed manifest's `versions` map. Sunsetted versions remain in the manifest until `sunsetAt`; after that they are stripped from the manifest by the control plane and a `410 Gone` response is returned for any request still pinning them.
- The cap is platform-level (not configurable). Justification: industry comparators ship 1 live version (Stripe, GitHub, AWS); we ship up to 3 to support a "current + next + last-deprecated" rotation. More than 3 is a smell — either the creator is over-versioning, or they need a different abstraction (e.g., a feature flag on a single version).

### Within-version compatibility

Within `/_zs/v1/` and within a procedure version: only **additive** changes are wire-stable.

#### Wire-compat check — algorithm

The build runs a check at deploy time against the previous deploy's artifact manifest (stored in the control plane, last-N=10 keyed by deploy timestamp).

```
For each procedure (id, version) in the new manifest:
    if (id, version) not present in any prior manifest in the last-N window:
        skip — new procedure, no compat to check.

    let prior_schema = the most-recent prior manifest's serialized JSON Schema for (id, version).
    let new_schema   = the new manifest's serialized JSON Schema for (id, version).

    diff = json_schema_diff(prior_schema, new_schema)

    for each change in diff:
        classify the change into one of:
          - safe:        added optional input field; added output field; widened input type
                         (e.g., `string` → `string | number`); narrowed output type;
                         added enum value to output; relaxed string min/max bound; relaxed
                         number lower bound (decreased) or upper bound (increased).
          - breaking:    removed input field; required input field added; removed output field;
                         narrowed input type; widened output type; type changed; tightened
                         bound; removed enum value from input or output.

    if any change is `breaking`: build error in production-mode with the exact field path,
        the prior shape, the new shape, and a suggested action (bump version).

    if all changes are `safe`: green-light; the procedure is wire-compat.
```

Where:

- The serialized JSON Schema comes from `zod-to-json-schema` (one-directional; we never round-trip Zod ↔ JSON Schema).
- The diff is deterministic and stable: schemas are canonicalized (sorted keys, normalized literals, etc.) before comparison.
- For `kind: "stream"` procedures, the schema is the *yielded* element type (the chunk).
- Class instances are forbidden as wire values (§4); the diff treats their schemas as opaque (any change is breaking).

#### `zod-to-json-schema` compatibility caveats

`zod-to-json-schema` has known incomplete coverage for the following Zod constructs:

| Zod construct | Coverage | Implication for wire-compat |
| --- | --- | --- |
| `z.discriminatedUnion` | Partial — emits `oneOf` but discriminator field metadata may be lost | Diff may flag a non-breaking change as breaking |
| `z.lazy(() => Schema)` | Partial — emits `$ref` to a definition; recursion depth limited | Same |
| `z.transform`, `z.refine` (custom predicate) | Lossy — predicate body is opaque to the schema | Diff treats `refine` as opaque; any change is flagged breaking |
| `z.brand` | Treated as the underlying type | Brand changes are not detected |

When the build cannot produce a clean diff (false positive against a non-breaking change), the creator uses `breakingOk: true` to attest. The audit log records the attestation so the platform can revisit the algorithm if specific patterns become common.

Output:

```
✗ Procedure todos.add v1 has a wire-breaking change:
    .input.text:
      prior: { type: "string", minLength: 1 }
      new:   { type: "string", minLength: 5 }      ← tightened lower bound
    .output: -.dueDate                              ← removed output field

  Bump to v2: declare a new export with version: "2", id: "todos.add",
  and a deprecates: { version: "1", sunsetAt: <date> } block.
  See docs/proposals/rpc-v2.md §13.
```

#### Bypass for false positives

A creator may explicitly attest a change is non-breaking by adding `breakingOk: true` to the wrapper (e.g., when adding a runtime check that the old client wouldn't violate). The build emits a warning, not an error, and records the attestation in the audit log.

```ts
export const add = mutation({
  input: z.object({ text: z.string().min(5).max(500) }),    // bumped from min(1)
  breakingOk: true,                                          // attested non-breaking
})(async ({ text }) => { /* ... */ });
```

#### Audit log surface

Every `breakingOk` attestation is written to the control plane's `app_deploy_audit` table on deploy, with row shape:

```sql
-- crates/control/src/migrations/<n>_app_deploy_audit.sql (sketch)
create table app_deploy_audit (
  id              uuid primary key,
  app_id          text not null,
  deploy_id       text not null,
  procedure_id    text not null,
  procedure_version text not null,
  fields_attested jsonb not null,         -- list of breaking-change paths
  prior_schema    jsonb not null,         -- canonicalized prior shape
  new_schema      jsonb not null,         -- canonicalized new shape
  deployed_by     text not null,          -- usr_…
  deployed_at     timestamptz not null
);
```

Surfaces:

- **Creator console** — a "Wire-compat attestations" panel lists recent attestations per app; the creator can drill into the prior/new schema diff.
- **Platform team** — read-only access via the control-plane API for compliance review.
- **Retention** — 1 year minimum; aligns with deploy retention so an attestation can be correlated with the build that introduced it.
- **Revocation** — an attestation cannot be revoked (the wire change has already shipped) but a subsequent deploy can re-introduce the field with `breakingOk: false`, which then re-runs the wire-compat check against the *new* baseline. The audit row is never deleted.

#### What stays the same

- Adding an optional input field: OK (always safe).
- Adding an output field: OK (clients ignoring extra fields keep working).
- Removing a field: requires a procedure-version bump (or `breakingOk` attestation).
- Changing a field's type: requires a procedure-version bump.

#### Where the prior-manifest history lives

The control plane stores the last 10 deployed `manifest.artifact` objects per app, keyed by deploy timestamp. Older manifests are pruned. A creator's first deploy has no prior manifest — wire-compat passes trivially.

---

## 14. Auth chain — end-to-end

Round-01 High-8 noted that the v1 draft mixed two auth surfaces (Bearer JWT and `__zs_session` cookie + `ZeroShip-User` HMAC) without specifying which populates `ctx.user`. Now:

### Two ingress credentials

- **Bearer JWT** (`Authorization: Bearer <token>`) — service-to-service, mobile native, programmatic clients.
- **Session cookie** (`__zs_session`) — browser sessions. The auth service issues this on login; the gateway validates per request.

### Gateway → worker handoff

The gateway never sends raw credentials to the worker. Instead it signs an HMAC-protected payload and forwards via headers:

```
ZeroShip-User: <base64url(payload)>.<base64url(hmac_sha256(payload, secret))>
```

Where `payload` is JSON:

```json
{
  "userId":    "usr_01HJQK...",
  "email":     "alice@example.com",
  "role":      "user",
  "issuedAt":  "2026-05-05T12:34:56Z",
  "sessionId": "ses_01HJQK...",
  "scopes":    ["todos:write", "billing:read"]
}
```

The worker verifies HMAC on receipt — see `crates/gateway/src/proxy.rs:378` and `crates/gateway/src/user_auth.rs`. On verification:

- Verified payload → `ctx.user = User { id, email, role, scopes, sessionId }`.
- HMAC mismatch → connection drop + log (gateway misconfigured or attempted spoof).
- Header absent + `auth: "anon"` → `ctx.user = null`.
- Header absent + `auth: "user"` → gateway pre-rejected with 401 before forwarding (defense-in-depth: worker also checks).

### `ctx.user` access patterns

```ts
import { ctx, requireUser } from "@zeroship/server";

export async function add({ text }: { text: string }) {
  // Pattern 1: ctx.user (may be null if auth: "anon")
  if (!ctx.user) throw new RpcError("UNAUTHENTICATED", "sign in required");

  // Pattern 2: requireUser() helper (throws UNAUTHENTICATED if absent)
  const me = requireUser();

  // Pattern 3: scope check
  if (!me.scopes.includes("todos:write")) {
    throw new RpcError("PERMISSION_DENIED", "missing scope todos:write");
  }
}
```

### Auth on streaming / WebSocket

- Streaming HTTP: same `ZeroShip-User` chain as request/response.
- WebSocket upgrade: cookie + HMAC validated on the upgrade handshake; subsequent frames inherit the session.
- **Re-validation on long-lived sockets**: the gateway re-checks session validity every `min(session.ttl / 4, 5 minutes)` (configurable via `defineApp.auth.wsRevalidateMs`). The 5-min default mirrors the typical session refresh interval used by Cloudflare Workers' Durable Objects auth; the `session.ttl / 4` floor ensures the check fires at least 4 times per session lifetime even for short-TTL sessions (so a 1-min temp session re-validates every 15 s). On a failed re-check the gateway sends:

  ```
  {"t":"error","error":{"code":"UNAUTHENTICATED","message":"session expired","retryable":false,"requestId":"req_…","traceId":"…"}}
  ```

  followed by a normal close (1000) — clients re-authenticate and reconnect.

### CSRF for cookie-auth

Round-01 Completeness flag: CSRF for cookie sessions is unspecified in v1. Now:

Mutations (POST / PUT / DELETE / PATCH) from cookie-authenticated origins must pass **at least one** of the following checks. They are evaluated in order; the first that passes lets the request through.

1. **Origin allowlist** (the primary check). The `Origin` header value matches one of `csrf_origins` in the resource policy. `csrf_origins: ["self"]` is the default for cookie-auth mutations and matches `<scheme>://<host>` of the request URL.
2. **Sec-Fetch-Site** (modern browser strengthening). `Sec-Fetch-Site: same-origin` AND `Sec-Fetch-Mode: cors` are both present. These are browser-emitted Fetch Metadata headers (Chrome 76+, Firefox 91+, Safari 16.4+) that cannot be forged from cross-site contexts. Coverage caveat: older browsers and most non-browser HTTP clients (curl, Postman) do **not** emit them — the absence does not fail this check; it falls through to the next.
3. **Double-submit token**. Custom header `X-Zs-Csrf-Token` matches the cookie's `csrf_token` claim. The auth service issues this claim on session creation; the cookie is `HttpOnly` but the claim is mirrored in a separate `XSRF-TOKEN` cookie that JS can read and place in the header.

Bearer-token requests **skip** all CSRF checks — there is no ambient credential; CSRF cannot apply.

Dev mode (`vite dev`, `NODE_ENV !== "production"`) skips CSRF entirely — local development uses different origins and curl is normal. `defineApp({ rpc: { dev: true } })` extends this to staging when explicitly opted in.

The intent of the layered check: in the common case (browser; same origin), check 1 passes immediately. Check 2 hardens against header-injection attacks where an attacker could spoof `Origin` (rare, but real for some proxies / dev environments). Check 3 is the fallback for double-submit-cookie patterns common in SPAs.

---

## 15. Open questions

1. **Default batching off vs. on.** Proposal: off by default for vanilla `client<App>()`, on by default in the React adapter. Revisit after early creators ship.

2. ~~**In-flight cancellation during deploy.**~~ **Resolved (§3 abort plumbing).** On isolate eviction triggered by a deploy swap, the worker calls `entered_for_eviction()` on the isolate; this fires the per-request `AbortController` for every in-flight procedure and starts a 30-second drain window. Procedures that finish within the window respond normally; those that don't are aborted hard at window end. After the drain, the isolate enters `Disposed`. The drain window is configurable via `defineApp.deploy.drainSeconds` (default 30, max 300).

3. **Workload-identity auth for service-to-service.** Should `auth: "workload-identity"` be a first-class policy (mTLS or signed-token)? Defer — Bearer JWT covers the immediate case.

4. **Cross-app RPC.** Can a creator expose procedures to *other apps*' workers? Defer; not on the initial roadmap.

5. **OpenAPI emission.** Listed as the future answer for non-TS typed clients. Open: which OpenAPI version (3.0.x vs 3.1.0) and which generator targets to bless. Defer until demand materializes.

6. **RFC 9457 problem-details compat mode.** RFC 9457 (Problem Details for HTTP APIs, July 2023) is the modern industry default — `application/problem+json` with `{ type, title, status, detail, instance, ... }`. Hono, FastAPI, Spring Boot, and others use it. Our `application/zs-error+json` is functionally equivalent but custom. Open: should the gateway content-negotiate on `Accept: application/problem+json` and emit a parallel envelope (mapping `code → type`, `message → detail`, `requestId → instance`)? Cost: ~50 LOC in the gateway error path. Benefit: automatic compat with any client that already speaks problem-details. Defer to post-launch.

---

## 16. Implementation phases

| Phase | Scope | LOC (rough) | Dependencies |
| --- | --- | --- | --- |
| **1** | Native foundation: (a) `RpcError` `#[v8_class]` (mirroring DOMException) at `crates/runtime/src/rpc/error.rs` — class registration on every isolate, brand check, exposed as `globalThis.RpcError`. (b) `crates/runtime/src/rpc/dispatch.rs` with `#[v8_method(fastcall)]` entries. (c) Native superjson encode/decode at `crates/core/src/superjson.rs` (gateway) and `crates/runtime/src/rpc/superjson.rs` (worker, V8-aware). (d) ALS-based ctx population — kernel writes the `ContinuationPreservedEmbedderData` slot before invoking the user procedure. (e) `ctx.headers` / `ctx.url` `Object.freeze` wrapping. (f) Extend `crates/worker/src/cache.rs` with `entered_for_eviction()` to fire per-request `AbortController`s on isolate eviction (§3 abort plumbing). (g) Microbenchmark gate at `crates/runtime/benches/rpc_dispatch.rs` deciding single-call vs. two-step fastcall ABI before phase 1 commits. | ~950 (Rust) | `#[v8_class]`, `#[v8_state_marker]`, `#[v8_method(fastcall)]`, native ALS — all shipped |
| **2** | Vite plugin: AST scan for file-level + function-level `"use server"`; reference-graph walk; transform-client / transform-server emission; synthetic entry generation. Strict-mode gate. | ~700 (TS) | Phase 1 |
| **3** | Build: `manifest.artifact` + `manifest.resources` emission; literal-only `defineApp` AST extraction; reserved `_zs.*` field-name check; live-version cap (3 max); `breakingOk` attestation surface. | ~400 (TS) | Phase 2 |
| **4** | Gateway: load `manifest.resources`, precompute `EffectivePolicy`, route + enforce per request. New `/_zs/v1/` prefix. CHWBL routing. CSRF (Origin → Sec-Fetch-Site → double-submit). ETag/304. Meter event emission. traceparent injection. | ~700 (Rust) | Phase 3 |
| **5** | Idempotency: gateway-side SETNX flow with Lua-script release (`crates/gateway/src/idempotency.rs`). KV-backed dedupe table. UUIDv4/v7 entropy check for anonymous mutations. | ~400 (Rust) | Phase 4 |
| **6** | Streaming: `function*` detection + content-negotiated wires (NDJSON, SSE, octet-stream). AI SDK 5 UI Message Stream emitter. Mid-stream error frames per §6 (NDJSON / SSE / AI SDK 5 / octet-stream-trailers). Heartbeats. **Acceptance**: the `wire: "ai-ui-v1"` byte stream is byte-identical to `streamText({...}).toUIMessageStreamResponse()` for a representative input set (verified against the upstream `ai/react` lib in CI). | ~500 (Rust+TS) | Phase 4 |
| **7** | Multipart / FormData / Blob / File first-class on the dispatch path. `_zs.json` envelope spec. Native multipart parser already exists; we wire it in. | ~350 (Rust+TS) | Phase 4 |
| **8** | Client: typed `client<App>()` with seamless surface, branded `__SERVER_REFERENCE` references, batching link. Auto-emitted `virtual:zeroship/server-api.d.ts`. | ~500 (TS) | Phase 2 |
| **9** | React: `@zeroship/rpc/client/_hooks` registry; `@zeroship/rpc/react` populates it; `@zeroship/rpc/server` (`__makeServerProcedure` + dehydrate/hydrate). | ~600 (TS) | Phase 8 |
| **10** | Wire-compat check: `zod-to-json-schema` integration; canonical schema diff; safe-vs-breaking classifier; deploy-time gate; control-plane prior-manifest history (last 10). | ~450 (Rust+TS) | Phase 3 |
| **11** | Procedure versioning: `Zs-Procedure-Version` header routing; sunset/deprecation surfaces; live-version cap enforcement. | ~250 (Rust+TS) | Phase 4, 10 |
| **12** | Subscriptions sketch: WS upgrade routing through manifest; `kind: "subscription"` runtime support. (Full backpressure / replay / flow-control: separate proposal.) | ~400 (Rust+TS) | Phase 4 |

---

## 17. References

- React Server Components / Server Actions — `"use server"` directive (file + function level), reference-graph detection, `$$typeof: REACT_SERVER_REFERENCE` branding pattern.
- Next.js Server Actions — FormData first-class, redirect/revalidate, progressive enhancement.
- tRPC v11 — typed procedure shape, link-batching, TanStack Query adapter, `createContext()` + ALS-backed context.
- Hono RPC — handler-as-type, native `c.req.formData()`.
- AI SDK 5 — UI Message Stream Protocol (`x-vercel-ai-ui-message-stream: v1`, start/delta/end framing).
- Stripe — Idempotency-Key model, in-flight handling, lock-holder-dies semantics.
- gRPC — fixed error code enum, status mapping.
- W3C Trace Context Level 2 — `traceparent` / `tracestate` propagation.
- RFC 8594 — `Sunset` HTTP header for deprecated endpoints.
- RFC 9110 — `Retry-After` semantics.
- Redis distributed locks — atomic SETNX + EX, Lua-script ownership-checked release.
- superjson — typed-JSON transformer (Date, BigInt, Map, Set, RegExp, URL, Uint8Array).

---

## Bottom line

The v1 draft sketched a tRPC-meets-Server-Actions design before the platform's native primitives shipped. This rewrite slots the proposal into the platform that exists today: ALS as the foundation primitive for ambient context; `#[v8_class]` for `RpcError`; `#[v8_method(fastcall)]` for dispatch; native FormData / Blob / File on the wire; AI SDK 5 (not the deprecated v4) for streaming; gateway-side SETNX for distributed idempotency; HMAC-signed `ZeroShip-User` for the auth chain; RSC-style file-level + function-level `"use server"` with reference-graph detection; one wire envelope (superjson) shared by every emitter and parser; `meter.*` events on every RPC call by construction.

The proposal is shorter than it looks because every JS-side scaffolding from v1 ("we'll build this in TypeScript") has been replaced with "the kernel handles this" — and the kernel handles it faster, more correctly, and with better realm semantics than any JS-side equivalent could.
</content>
</invoke>