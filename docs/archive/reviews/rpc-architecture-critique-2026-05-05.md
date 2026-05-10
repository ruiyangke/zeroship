# RPC Architecture Critique — 2026-05-05

**Reviewer:** architecture-critic agent (model: opus-4-7)
**Scope:** the zeroship RPC stack — vite-plugin transform/manifest emitter, `@zeroship/server` wrappers, synthetic SSR entry, runtime kernel `default.rpc` fast path, gateway dispatch, and `@zeroship/rpc-client`.
**Verdict in one sentence:** the design proposal in `docs/proposals/rpc-v2.md` is good; the implementation that shipped is less than half of it, with several silent inconsistencies, one production-reachable security/correctness bug, and a wire format that pretends to be superjson but isn't.

---

## 1. Score (architecture-critic dimensions, 1-100)

| Dimension | Score | One-liner |
| --- | --- | --- |
| Separation of Concerns | 65 | Wire-id resolution lives in 3 files that must agree by hand; transform leaks into manifest emitter via shared mutable state. |
| Dependency Hygiene | 78 | `@zeroship/server` does a top-level await on optional Zod; `@zeroship/rpc-client` and `@zeroship/server` don't share a single types-only package. |
| Design Patterns | 55 | "Wrapper-marker" is a custom convention reinventing what tRPC does in 30 LOC of types. SSR `__makeServerProcedure` and client `__makeProcedure` are parallel structures with no shared abstraction. |
| Scalability | 60 | Manifest re-emitted as JSON literal, no streaming; `defineApp` config is `Function()`-evaled — fine at 100 procs, painful at 10k. |
| Operability | 50 | No observability primitives wired (no trace_id propagation, no per-procedure metrics). Idempotency exists but never composes with worker dispatch. |
| Testability | 70 | Good unit tests for the transform; **the `synthetic-entry-zod.test.ts` family invokes a `buildServerEntrySource` signature that no longer exists** — see Critical-1. |
| API Surface | 55 | Two SDKs (`@zeroship/server`, `@zeroship/rpc-client`) plus the implicit "synthetic entry" contract; the public stubs `user()`/`userOrNull()`/`requireRole()` all throw `Not implemented` today, but the proposal is shipped under the same name. |

**Composite: 62/100.** This is "we have a working dev loop and a clean spec, but the production surface has not yet been hardened" territory. The spec promises an end-to-end story; what's on `master` is the bottom 30% of that story.

---

## 2. Summary — the worst / most-important findings

1. **`zeroship.auth.getUser()` is structurally broken.** `crates/runtime/src/auth.rs:29` defines `set_request_user`, the `getUser/requireUser` callbacks read `per_request_user`, but **no caller in the live tree ever calls `set_request_user`**. The gateway HMAC-signs and forwards `ZeroShip-User` (`crates/gateway/src/proxy.rs:378`); the worker's dispatch handler (`crates/worker/src/handler.rs:107-200`) parses the envelope and calls `runtime.call_fetch_handler` without ever decoding the header or populating per-request user state. **Any procedure that calls the proposed `user()` helper will see `null`.** This is a production-reachable correctness hole — see Critical-2 for evidence.

2. **The wire pretends to be superjson but is plain JSON.** The transform's emitted client stub (`sdks/vite-plugin/src/transform.ts:250`) calls `JSON.stringify({ json: input })`. The runtime fast-path parser (`crates/runtime/src/core/runtime.rs:2755-2797`) reads only the `.json` field and **silently discards `meta`**. The real `@zeroship/rpc-client` (`sdks/rpc-client/src/encoding.ts:68-79`) uses real superjson with `meta`, but the lazy chunks emitted by the vite-plugin's auto-stub-transform path do not. So `Date`, `BigInt`, `Map`, `Set`, `Uint8Array` round-trip as strings or `{}` for any user calling via the auto-emitted stubs (most users) — but as the correct types for users who manually wire `client<App>(...)`. The two paths have observably different behavior.

3. **`buildServerEntrySource` API has drifted from its tests.** `sdks/vite-plugin/src/rpc-registry.ts:86` accepts only `{ userEntryRel }`, but four test files (`synthetic-entry-zod.test.ts`, `zod-output-dev-only.test.ts`, `zod-passthrough.test.ts`, `ai-sdk-stream.test.ts`) call it with `{ userEntryRel, procedures }`. The `procedures` arg is silently dropped — the runtime walks `Object.keys(_zsUser)` instead. Tests presumably "pass" because they exercise behavior that ignores the extra parameter, but the signature mismatch means the tests are not testing what their setup claims to test.

4. **Function-level `"use server"` (ISS-02 Phase 2) is not in the transform; reference-graph detection (#168) is not even sketched.** The current rule is: file-level `"use server"` directive + import a wrapper from `@zeroship/server`/`@zeroship/rpc` and call it on a `VariableDeclaration` initializer. That is more ceremonious than the spec promised ("just `export async function add()`") and it loses the React/Next "function-level directive lets a single file mix client+server" capability that React Server Actions and Next.js have shipped for two years. The transform has the AST machinery (`detectFileLevelUseServer` at line 95, `collectWrapperBindings` at 191), but no per-function directive scan.

5. **Three independent dispatch tables with no single source of truth.** (a) The `transform.ts` emits `globalThis.__register(wireId, fn)` calls into the user module (line 850) for the dev-bootstrap. (b) The synthetic SSR entry walks `Object.keys(_zsUser)` to build `_procedures` at module-init (`rpc-registry.ts:99-106`). (c) The gateway's manifest emits a `resources` map keyed by `rpc:<wireId>` (`manifest.ts:691-707`). These three must agree on wireId resolution, but each computes it independently (`transform.ts:765-777` "wireIdFor", `rpc-registry.ts:59-66` "pickEntryWireId", `manifest.ts:291-302` "pickWireId"). Three implementations of the same one-liner ("explicit `config.id` if string and non-empty, else `exportName`") in three files, with comments insisting they "must stay in sync." That's a class of bug waiting to happen — see High-1.

6. **Errors lie about what they are.** The transform's emitted client stub (`transform.ts:259`) constructs `new Error()` and stamps `e.code`/`e.details` on it. The proposal (`docs/proposals/rpc-v2.md` §6) defines an `RpcError` class with a fixed code enum. The class exists in `sdks/server/src/index.ts:154` but is a bare-bones stub; the runtime never wraps thrown values into it; the dispatch path's `dispatch.rs::v8_exception_to_error_value` reads `.code`/`.details` off any thrown object and ships them. Result: the user can throw `new Error("oops")` and the wire receives `{ code: undefined, message: "oops" }` — production redaction (the spec's load-bearing `RpcError`-or-redact rule, §6 Error redaction) is **not implemented anywhere**. Plain errors leak full messages and stacks to clients in production.

7. **AsyncLocalStorage has landed (ISS-01) but the RPC stack does not use it.** `crates/runtime/src/node/async_hooks/als.rs` is a real, V8-`ContinuationPreservedEmbedderData`-backed implementation. The `ctx` object passed to `rpc(name, input, ctx)` is the frozen-once singleton with `waitUntil` and `passThroughOnException` and **nothing else** (`runtime.rs:1007-1037`). No `ctx.user`, no `ctx.signal`, no `ctx.headers`, no `ctx.idempotencyKey`. The proposal's §3 "Ambient context" (`user()`, `requestStorage`, `idempotencyKey()`) is a complete design with zero implementation behind it. The pieces exist; nothing wires them together.

8. **`/_zs/v1/<id>` does not percent-decode `<id>`.** `runtime.rs:2681-2691` extracts the id with raw `find` calls on the URL string; the synthetic entry at `rpc-registry.ts:245` does `url.pathname.slice(...)` with no `decodeURIComponent`. Spec §2 allows wireIds with dots only (`[a-zA-Z0-9._*-]`), so this happens to be safe IF the gateway enforces that constraint, BUT the gateway's `KEY_FORMAT_RE` (`manifest.ts:306`) is `/^(?:\*|rpc:[a-zA-Z0-9._*-]+|...)$/` — which the worker can't enforce, only validate at build time. Anyone calling the worker directly (the bench server does, every E2E test does) bypasses that gate. A wireId containing `%2e` would silently skip the lookup map. Low impact in production, but it's a soft mismatch.

9. **The "subscription" wire is documented in two places that disagree.** Proposal §6 ("Subscription — WebSocket") describes a JSON frame protocol (`{"t":"hello",…}`). The `subscribeCall` client (`sdks/rpc-client/src/transport.ts:619-934`) implements that protocol against `globalThis.WebSocket`. The runtime side has no implementation in the codepaths I read — the synthetic entry (`rpc-registry.ts`) has no subscription branch; runtime.rs's RPC fast path returns `FallThrough` on `AsyncIterator` and routes to the SSE wrapper, not WebSocket. There is gateway scaffolding (`dispatch.rs:170-199` `subscription_affinity_key`, `is_websocket_upgrade`) but the worker-side handler is not exposed via the synthetic entry. Subscriptions are sketched, not wired.

10. **No FormData / no multipart / no binary path.** Spec §11 scenario H ("file upload (large args)") describes auto-routing >1MB to `multipart/form-data`. The runtime has `Blob`, `FormData`, multipart parsing in `web/dom/form_data.rs` and `web/blob/blob.rs` — but none of it is reachable through the RPC pipeline. Client encoding does `await encodeBody(input, transformer)` which is `JSON.stringify`, and the wire schema is JSON. `Uint8Array` is passed via superjson's base64 encoding, which the transform's auto-stub doesn't even use (it does plain `JSON.stringify`). RSC's `<form action={serverFn}>` story has no analog here. This is the single largest gap if you want to be competitive with Next.js Server Actions.

---

## 3. Detailed flaws by severity

### CRITICAL

**Critical-1 — `auth.getUser()` always returns null in the deployed worker path.**
- **Files:** `crates/runtime/src/auth.rs:29-46` (`set_request_user`, `clear_request_user`); `crates/worker/src/handler.rs:107-230` (dispatch; never calls them); `crates/gateway/src/proxy.rs:378` (forwards `ZeroShip-User`).
- **Why it matters:** the public `@zeroship/server` API includes `user()` and `requireRole()` (today they throw `Not implemented`, but ISS-01 was supposed to land them). The plumbing is half-built: HMAC sign, ship over the wire, store the function — but nothing reads the header, validates the HMAC, or calls `set_request_user`. A creator who follows the proposal will get `getUser() === null` for every request even though they're authenticated. The fact that the SDK stubs throw means it's caught at first use, but the moment those stubs become real (Phase 4 in the proposal), this latent bug surfaces immediately. **Fix before shipping `user()`.** Add an HMAC-verifying shim in `worker/src/handler.rs::dispatch` (between line 154 envelope parse and line 188 `enter_isolate`) that decodes `ZeroShip-User`, verifies signature, and calls `runtime::auth::set_request_user`.

**Critical-2 — wire format silently strips superjson `meta`.**
- **Files:** `crates/runtime/src/core/runtime.rs:2755-2797` (`parse_envelope_body`); `sdks/vite-plugin/src/transform.ts:250-303` (auto-stub uses `JSON.stringify({ json: input })`); `sdks/rpc-client/src/encoding.ts:68-107` (real client uses `sj.serialize` which yields `{ json, meta? }`).
- **Why it matters:** the proposal's §4 "Cross-boundary types" explicitly enumerates `Date / BigInt / Map / Set / URL / Uint8Array / RegExp` as types the platform preserves. With the current parser, the kernel reads `body.json` and discards `body.meta` — so any `Date` sent by the manual `client<App>()` arrives as the ISO string, never revived. Two clients (auto-stub and manual client) ship to the same wire, with different revival behavior. Worse: the runtime never returns `meta` either (`rpc-registry.ts:339` `JSON.stringify({ json: result })`), so the round-trip is asymmetric. **Fix:** decide whether superjson is the wire format. If yes, the runtime must parse `{ json, meta }` together and pass both to `_zsRpc` (or the synthetic entry must call superjson's `deserialize` before invoking the user fn). If no, drop the "transformer: superjson" claim from the manifest.

**Critical-3 — production error redaction is missing.**
- **Files:** `crates/runtime/src/core/dispatch.rs:285-349` (`v8_exception_to_error_value` reads `.code`, `.details`, `.retryable` off any thrown object verbatim); `sdks/server/src/index.ts:154-163` (`RpcError` class is a stub, no `name === "RpcError"` brand check anywhere).
- **Why it matters:** spec §6 "Error redaction" is explicit — non-`RpcError` throws must redact to `{ code: "INTERNAL", message: "Internal server error" }` in production. Today, `throw new Error("DB connection refused: postgres://prod-db:5432/secret")` ships verbatim with stack trace via `v8_exception_to_stack`. This is a credential-leak class issue. **Fix:** brand `RpcError` (e.g., `Symbol.for("zeroship/RpcError")`) so the dispatch path can distinguish "user constructed for the wire" from "incidental throw". Add the redaction step in `build_error_body` keyed by `NODE_ENV !== "development"` and the brand.

### HIGH

**High-1 — three duplicated wireId resolvers.**
- **Files:** `transform.ts:765-777` (`wireIdFor`); `rpc-registry.ts:59-66` (`pickEntryWireId`); `manifest.ts:291-302` (`pickWireId`).
- **Why it matters:** if any one drifts (somebody adds idempotency-key prefix, slug normalization, etc.) the gateway will route to a key the runtime doesn't have. Two of three already differ slightly: `manifest.ts:294` checks `explicit.length > 0`; `rpc-registry.ts:64` does the same; `transform.ts:771-774` checks `legacyId !== ""` first then `wrapperConfigId !== ""`. The legacy-vs-wrapper precedence rule is articulated in `transform.ts` and `manifest.ts` ("legacy assignment wins"), but `rpc-registry.ts` only sees `_v.config.id` at runtime — it can't distinguish wrapper-arg from legacy. Three subtly different copies with comments insisting they're equivalent.
- **Fix:** lift wireId resolution into a single function in a shared internal package (e.g., `sdks/types/src/wire-id.ts`) and import it from all three call sites. Make precedence rules data-driven, not commented.

**High-2 — function-level `"use server"` is not implemented.**
- **Files:** `transform.ts:95-117` (`detectFileLevelUseServer`); the function-level scan is mentioned only in comments at `transform.ts:91-94`.
- **Why it matters:** the proposal §1 said "no wrappers, no `useQuery` ceremony for the 80% case — just `export async function add()`". What shipped requires `import { procedure } from "@zeroship/server"; export const add = procedure(async () => …)`. That is more boilerplate than RSC, more boilerplate than Server Actions, more boilerplate than tRPC. The transform's own AST walker rejects bare `export async function fn()` (line 626-642 — "These are NOT RPCs anymore"). The spec promised opt-out (path convention removed); what shipped is opt-in via wrapper. Net: more ceremony, not less.
- **Fix:** support the function-level directive. The AST walker can already enumerate `ExportNamedDeclaration` / `FunctionDeclaration` shapes; add a body-prologue scan (`fn.body.body[0].type === "ExpressionStatement" && fn.body.body[0].expression.value === "use server"`). For the closure-server-action shape (`async (formData) => { "use server"; ... }`), do the same on `ArrowFunctionExpression`/`FunctionExpression` initializers. This is roughly 80 LOC of AST walking — see RSC's `react-server-dom-webpack` for the reference algorithm.

**High-3 — reference-graph detection (#168) is not designed.**
- **Why it matters:** "if a server-only function is imported by a client module, mark it as RPC" is how RSC, Next.js Server Actions, and Remix Actions all converge. It eliminates the manual `"use server"` directive in the common case. Without it, every refactor breaks the wire (move a function from a server file to a shared util — silent client-side import of server code). The proposal does not yet have a mechanism for this (the closest hook is `transform`'s `serverFunctionMap`, but it doesn't see imports).
- **Fix direction:** plug into Vite's module graph in `closeBundle`. Walk every client-bundle chunk's imports; any node that resolves to a `"use server"` file becomes an RPC stub. Two passes: first collect, then rewrite. This is what `@vitejs/plugin-rsc` does.

**High-4 — no idempotency at the worker.**
- **Files:** `crates/gateway/src/idempotency.rs:1-300+` (full implementation of `IdempotencyStore`, in-memory + skeleton for distributed); `crates/runtime/src/core/runtime.rs::call_fetch_handler` (no idempotency-key threading).
- **Why it matters:** the gateway dedups via `(app_id, wireId, idempotency_key)`, but the worker handler never reads the `Idempotency-Key` header into the runtime context. Per-procedure `fn.config.idempotent: true` is detected at build time (manifest.ts) and enforced at the gateway (`dispatch.rs:441-468`), but the runtime has no way to surface the idempotency key to user code (`idempotencyKey()` from spec §3 is a phantom helper). Two consequences: (a) no stored body in the dedup table actually carries the response from a worker round-trip yet (the in-memory store has the surface, but the call site is a TODO — see `idempotency.rs:434+ capture_response_for_idempotency`); (b) user code that wants to write its own idempotency logic (e.g., dedupe at the DB layer) has no way to read the key.
- **Fix:** thread `Idempotency-Key` through `RequestCtx` as a strongly-typed field; expose to JS via `ctx.idempotencyKey` on the singleton context object. Replace stub `idempotencyKey(): never` in `@zeroship/server` with `() => ctx.idempotencyKey`.

**High-5 — `@zeroship/server`'s `index.ts` does a top-level `await import("zod")`.**
- **File:** `sdks/server/src/index.ts:99-127`.
- **Why it matters:** ES2022 top-level await is fine in Node, but this is a peer-dep of the runtime. Worse: it's wrapped in try/catch, then a `Proxy` is exported in place of `z` — meaning the type signature is `ZodNamespace` (a real Zod type) but the runtime value is a Proxy that throws on access. TypeScript's structural typing accepts that, the user's code compiles, the user calls `z.object(...)` at run time and gets a clear error message. But for the library author it means the entry module forces every consumer to pay top-level-await — including consumers who never use schemas. Cold-start tax.
- **Fix:** lazy-load Zod via a getter on the `z` export. Don't run the import at module init.

**High-6 — `defineApp` config extraction is `Function()`-eval'd.**
- **File:** `sdks/vite-plugin/src/manifest.ts:475-525`.
- **Why it matters:** `extractDefineAppLiteral` strips imports and `as Foo` casts via regex, then `new Function("return (...)")(.)`'s the slice. This is fast and brittle. (a) Regex `/\s+as\s+/g` will mis-strip identifiers that contain `as` (`namespaceasync`). (b) Computed expressions (the file might do `auth: env.PROD ? "user" : "anon"`) throw a build error pointing the user at the wrong cause. (c) A maliciously-shaped config file could exfil env vars (the `Function` constructor runs in the build process's scope; `process.env.SECRET_KEY` is reachable). For a creator-platform where the config is user-authored AI-generated code, that's a footgun.
- **Fix:** use `oxc` or `@babel/parser` to AST-parse the file (dev cost: 50 LOC), then literal-walk the `defineApp` arg the same way `literalize()` already does in `transform.ts`. `literalize()` is reusable; just rename and export it.

**High-7 — `_zsRpc` is sync but the synthetic entry's `_zsRpcAndRespond` is async; the FallThrough/re-invoke pattern is benign for `async function*` but pathological for hand-rolled iterators.**
- **File:** `crates/runtime/src/core/runtime.rs:2658-2675` (RpcCallResult::FallThrough doc); `rpc-registry.ts:230-275` (re-invokes via `_zsRpc` from `_zsFetch`).
- **Why it matters:** the kernel can't encode an `AsyncIterator` inline (no native SSE encoder) so it returns FallThrough, which causes the slow path to **re-invoke the procedure** to get a fresh iterator that JS can wrap in a ReadableStream. The doc comment (`runtime.rs:2670-2674`) says: "Re-invocation is benign for `async function*` (the body only runs when iterated…)". That's correct ONLY if the user uses `async function*`. If they hand-roll an iterator factory (which is rare but legal — the spec allows any AsyncIterable), the synchronous setup work runs twice. More worryingly: the validation (`fn.config.input.parse(input)`) ALSO runs twice — first in the kernel fast path, then again in `_zsRpc` from `_zsRpcAndRespond`. For expensive validation (large Zod schemas) this is a 2x perf hit on streams.
- **Fix:** when the kernel sees an `AsyncIterator`, hand the iterator handle to a Rust-side SSE encoder instead of re-invoking JS. The Rust runtime already has a stream forwarder in `crates/runtime/src/web/streams/response_forwarder.rs`; wire it for the `0:`/`2:`/`d:` AI-SDK Data Stream framing. ~150 LOC.

### MEDIUM

**Medium-1 — type safety stops at the wire.**
- **Files:** `sdks/rpc-client/src/client.ts:174-212` (`TypedClient<App>`).
- **Why it matters:** the proposal §4 promises "build emits `virtual:zeroship/server-api.d.ts`". No such virtual module exists in the codebase. Today users get types via the manual `App` type (`type App = { listTodos: ProcedureType<"query", In, Out>; }`) — which means writing the procedure types twice (once in the server file, once in `App`). tRPC's whole value-prop is that you don't do this. Server Actions use phantom types that flow through directly. Without server-api.d.ts emission, this RPC stack offers worse end-to-end types than tRPC.
- **Fix direction:** emit the virtual `.d.ts` from the manifest emitter at `closeBundle`. Each procedure's types come from the source file's signature — easiest path is to invoke `tsc --emitDeclarationOnly` on a synthetic surface module that re-exports each `<exportName>: typeof <real-export>`.

**Medium-2 — the gateway and runtime each parse `_zs/v1/<id>` independently.**
- **Files:** `crates/runtime/src/core/runtime.rs:2681-2691` (kernel slice); `sdks/vite-plugin/src/rpc-registry.ts:245` (synthetic entry slice); `crates/gateway/src/router/dispatch.rs:594-596` (gateway slice).
- **Why it matters:** three separate slice-the-prefix-off-the-URL implementations. Two are in JS (synthetic entry, dev-bootstrap); one in Rust (kernel). Each must agree on what's allowed in `<id>`. The gateway's `dispatch_path_wire_id` does no validation (just `strip_prefix`); the kernel does no decoding; the synthetic entry slices the path. None percent-decodes; none normalizes case; the kernel rejects non-POST/GET while the synthetic entry rejects non-POST/GET with a 405. **Risk:** kernel says "no `default.rpc` for `/_zs/v1/foo%2ebar`", synthetic entry says "yes for `/_zs/v1/foo.bar`". Same id, inconsistent dispatch. Build-time we never see this because the build emits raw ASCII ids.
- **Fix:** centralize id parsing. Either reject `%` in URLs at the gateway (cheap) or decode once at the kernel.

**Medium-3 — module-init dispatch table is build-once and ignores HMR.**
- **File:** `rpc-registry.ts:99-106` — `_procedures` is built at the synthetic entry's module-evaluation time, by walking `_zsUser` once.
- **Why it matters:** dev mode patches via `globalThis.__register` (transform.ts:847-852) so HMR works; production has no equivalent because the synthetic entry walks the namespace once and there's no re-walk on user-module update. For a long-running worker that hot-swaps the user bundle (the platform's deploy story), the procedure map can go stale. The current cache-eviction behavior in `crates/worker/src/cache.rs` (LRU eviction → re-init isolate) sidesteps this, but only because eviction triggers a fresh module load. A per-app long-lived worker that doesn't evict would observe stale dispatch.
- **Fix:** make `_procedures` a getter that re-reads the namespace on first call; OR re-walk on each `_zsRpc` call (cheap — `Object.keys` is O(n) where n ≤ ~50 typical). Per-call is fine; first-call-cached is faster.

**Medium-4 — the dev-bootstrap and synthetic entry have parallel implementations of `_zsRpc`.**
- **Files:** `sdks/vite-plugin/src/dev-bootstrap/index.ts:181-289` (dev path); `sdks/vite-plugin/src/rpc-registry.ts:151-280` (prod path).
- **Why it matters:** the dispatcher is reimplemented twice in TypeScript. They differ subtly: dev imports the user module via `runner.import()`, prod consumes the static import; dev's `_zsRpc` is `async`, prod's `_zsRpc` is sync (with `.then` continuation); dev does NODE_ENV check on output validation, prod does the same but routes via different code paths. Two implementations with the same intent — every fix must land in both, with no compile-time guarantee they stay aligned.
- **Fix:** lift `_zsRpc` and `_zsRpcAndRespond` into a shared helper (`sdks/vite-plugin/src/_dispatch-core.ts`); both entry points call into it, differing only in how they obtain the user module's namespace.

**Medium-5 — the `procedure()` wrapper is documented as identity but mutates the handler in place.**
- **File:** `sdks/server/src/wrappers.ts:60-104` — `attach()` calls `Object.defineProperty(handler, "config", ...)` and `Object.defineProperty(handler, "__zsKind", ...)`.
- **Why it matters:** the wrapper IS modifying the handler — it's not pure identity. If a user does `import { handler } from "./shared"; export const proc = procedure(handler);` then later imports `handler` directly elsewhere and calls it expecting no `.config` field, they'll see one. The frozen-function fallback (the empty `catch`) silently no-ops, which means a user who froze their handler (some testing setups do) gets a wrapper whose comments claim it stamped `.config` but didn't. Spooky.
- **Fix:** wrap rather than mutate. `return Object.assign(function(...args) { return handler.apply(this, args); }, { config, __zsKind })`. One closure allocation per procedure at module load — negligible vs. the type-correctness win.

**Medium-6 — the synthetic entry's `_userFetch.call(_userDefault, …)` chains user fetch through the RPC wrapper, but the kernel's three-tier dispatch already invokes `default.fetch` directly when RPC misses.**
- **Files:** `rpc-registry.ts:108-110`, `:278`; `runtime.rs:1209-1366` (three-tier dispatcher: `default.rpc` → `default.fetchFast` → `default.fetch`).
- **Why it matters:** if the kernel falls through to `default.fetch` (case 3), the user sees the `_zsFetch` wrapper from the synthetic entry, not their own `_userFetch`. Requests to `/api/foo` then go: kernel calls `_zsFetch(req, env, ctx)` → `_zsFetch` checks `/_zs/v1/` prefix (no match) → `_zsFetch` calls `_userFetch.call(_userDefault, req, env, ctx)`. That's two `Function.call`s for every non-RPC request. Not a correctness bug, but a performance tax of ~50ns/request that the doc claims doesn't exist ("kernel routes through default.fetch when no match").
- **Fix:** if the kernel can detect "user has no RPCs" at module-init, skip `_zsFetch` entirely and route to `_userFetch`. Detect by whether `_procedures` is empty.

**Medium-7 — error envelope inconsistency on the wire.**
- **Files:** kernel error path: `runtime.rs:1383-1419` builds `{message, name, stack?, code?, details?, retryable?}`. Slow-path JS: `rpc-registry.ts:113-119` builds `{message, name, code}`. Idempotency rejection: `dispatch.rs:600-625` builds `{code, message, details, retryable}` with `application/zs-error+json`.
- **Why it matters:** the wire's error shape depends on which path produced it. Kernel-fast-path errors carry `name` and `stack`; JS-slow-path errors carry `name` but no `stack`; gateway-rejected errors carry no `name`. Clients that branch on shape get inconsistent results. Spec §6 says the envelope should be `{code, message, details, trace_id?, retryable}` — `name` is not in the spec but the kernel emits it, `trace_id` is in the spec but no path emits it.
- **Fix:** pick one envelope shape and build it everywhere (dispatch.rs::build_error_body is already centralized; route the slow-path JS through a helper that produces the same shape).

**Medium-8 — no batching support implemented.**
- **Files:** `sdks/rpc-client/src/batch.ts` (237 LOC, BatchLink class) — kernel has no `_batch` endpoint; gateway has no `/_zs/v1/_batch` route.
- **Why it matters:** spec §6 "Batching" is a documented feature with a wire format (`application/zs-batch+json`). The client supports it; nothing on the server does. A user who flips `client({ batch: true })` will silently get every call rejected with 404 once batching kicks in.
- **Fix:** either implement the `_batch` endpoint (a few-shot dispatcher in the synthetic entry that loops `_procedures[req.name](req.input)`) or remove the option from `ClientOptions` until it ships.

### LOW

**Low-1 — `transform.ts` is 885 LOC and growing.** It does AST detection, AST symbol-table-build, AST traversal for procedures, AST walk for `<fn>.config = {...}`, literal evaluation, client-stub generation, server-side hook patching, dev-bootstrap registration emission. Reasonable to split into `transform-discover.ts` + `transform-emit-client.ts` + `transform-emit-server.ts` once the interfaces stabilize.

**Low-2 — `rpc-registry.ts:282` second `_zsRpcAndRespond` re-implements stream encoding from scratch.** ~80 LOC of `0:`/`2:`/`e:`/`d:` framing duplicated from the dev-bootstrap. Pull into a helper module.

**Low-3 — `wrappers.ts` `attach()` has a `try/catch {}` that silently swallows frozen-function errors.** If the wrap fails, the handler returns sans `.config` / `__zsKind`, transform-time discovery breaks silently. Should either propagate or warn at module-load.

**Low-4 — `SCHEMA_MARKER` is `Symbol.for("zeroship/zod-schema")` but the manifest emitter compares with `Object.is`-style equality (`literalize()` uses `=== SCHEMA_MARKER`).** Symbols are unique by reference, not value; `Symbol.for` interns globally so this works, but the export of `ZS_SCHEMA_MARKER` from `transform.ts:349` and the inline use suggests the intent is to compare via `Symbol.for`. If anyone in a downstream tooling chain re-creates a schema with `Symbol.for("zeroship/zod-schema")` it'll match — fine — but unintended.

**Low-5 — the dev-server's `__register` global pollutes the V8 globalThis with arbitrary keys.** `transform.ts:847-852` emits `if (typeof globalThis.__register === "function") globalThis.__register(...)` into every server module. Production builds also emit these calls (the comment at line 858 says "harmless no-op outside dev"); they're stripped by the Rolldown DCE only if `globalThis.__register` is statically `undefined`, which Rolldown can't know. Dead-code cost: a `typeof` check + a Function call per procedure on production cold-start. Tiny.

**Low-6 — no `Last-Modified` / `ETag` for queries.** Spec §6 promises `Cache-Control` and `ETag` on query responses. The kernel emits no `ETag`; the gateway has cache-policy logic in the resource tree, but the runtime never sets `Last-Modified`/`ETag` in the response headers. Lost CDN cache opportunity.

### NITPICK

- `transform.ts:349` exports `ZS_SCHEMA_MARKER` but no caller imports it (it's only used inside the file).
- `rpc-registry.ts:121` `_zsErrResponse` emits `{ message, name: "Error", code }` — `name: "Error"` is hardcoded; should be the user error class name when available.
- `make-server-procedure.ts:71-77` uses `// @ts-ignore` for the optional Zod import; should be `// @ts-expect-error` so it errors when the suppression becomes unnecessary.
- `client.ts:174-212` `TypedClient<App>` is correct but unfriendly to read; the `NestedFor` / `NestedShape` recursion could be a single distributive conditional. (TypeScript performance reason for the current shape, but it'd be helpful to comment that.)
- The proposal's §1 says "no wrappers, no `useQuery` ceremony for the 80% case." The current code requires wrappers. Either update the spec or the impl — the ambiguity is causing user confusion (per the task brief: ISS-02 Phase 2 was built but not merged).
- `rpc-registry.ts:359` `export default { fetch: _zsFetch, rpc: _zsRpc };` — should freeze, otherwise user code can `import entry from "..."; entry.rpc = …` and replace the dispatcher. (`runtime.rs:1023-1034` has a `Object.freeze` block for `ctx_obj`; should do the same here.)

---

## 4. Industry comparison

| Capability | This stack | tRPC v11 | Next.js Server Actions | RSC (React Server Components) | Remix Actions | Hono RPC |
| --- | --- | --- | --- | --- | --- | --- |
| **Function discovery** | File-level `"use server"` directive + import-a-wrapper-call (manual). No reference-graph. | Manual builder: `t.router({ list: t.procedure.query(...) })`. | Function-level `"use server"` directive (any export) + auto reference-graph. | Function-level `"use server"` directive on closures + reference-graph. | Filesystem convention (`action` export from a route file). | Manual: `app.get('/x', handler)` — explicit registration. |
| **Wire format** | superjson (advertised) — but auto-stub uses plain JSON. Wire is `{ json, meta? }`. | superjson, devalue, or any pluggable transformer. Negotiated. | Bespoke binary format (RSC payload). Supports Date, BigInt, Map, Set, FormData, File, Blob. | Same as Next.js (RSC payload format). | FormData is the wire (multipart). | Plain JSON. |
| **Error model** | Ad-hoc — `{message, name, code?, details?, retryable?}`. Spec defines a code enum + redaction; **redaction not implemented**. `RpcError` class exists but is a stub. | `TRPCError` class with code enum (`BAD_REQUEST`, `NOT_FOUND`, etc., gRPC-style). Type-safe on client. | `notFound()`, `redirect()` are throws; user throws `Error` propagates as 500. No structured codes. | Same as Next.js. | Throws are `ErrorBoundary`-routed; FormData errors via `ValidationError`. | Manual — user throws `HTTPException`. |
| **Type safety** | Phantom-type `client<App>()` — user writes `App` manually. No virtual `.d.ts` emitted. | Inferred from server router — zero manual types, full input/output inference. | Inferred — server function's signature flows through unchanged. | Inferred. | Inferred via route module's `loader`/`action` type exports. | Inferred via `app.get<typeof router>()`. |
| **Streaming** | AI-SDK Data Stream Protocol via SSE. Sync return + AsyncIterator detection. Re-invokes procedure on FallThrough (perf wart). | Subscriptions via WebSocket (subscriptions API — full bidi). | Streaming JSX via React 19 `<Suspense>` boundary. | Streaming JSX. | `defer()` + streamed responses. | Native Web Streams. |
| **Auth-context propagation** | `ctx` arg passed positionally; `getUser()` proposed but **not wired**; AsyncLocalStorage exists but unused by RPC. | `ctx` from `createContext()` factory, threaded through middleware. AsyncLocalStorage in v11. | `cookies()`, `headers()` from `next/headers`, AsyncLocalStorage-backed. | Same as Next.js. | `request` is the entry; user does cookie-parsing themselves. | `c.get('user')` from middleware. |
| **FormData handling** | None — wire is JSON only. | Plugin-based (community). | First-class — `<form action={fn}>` automatically calls server fn with FormData. | Same as Next.js. | First-class — actions receive FormData natively. | Native via `c.req.formData()`. |
| **Idempotency** | Spec defines `Idempotency-Key`; gateway has store; **runtime doesn't expose key to user code**. | None native. | None native. | None native. | None native. | None native. |
| **Subscriptions** | Spec defines WebSocket protocol; client `subscribeCall` exists; **runtime side incomplete**. | Subscriptions API — full WebSocket support, auto-reconnect, type-safe. | Not supported. | Not supported. | Not supported. | Plugin via Hono websocket. |

**The honest assessment.** The proposal's ambition matches tRPC + Next.js Server Actions combined: typed client like tRPC, function-level `"use server"` like Server Actions, structured errors like tRPC's `TRPCError`, streaming like AI-SDK, subscriptions like tRPC, idempotency like Stripe. What's shipped today is roughly the SHELL of all of these but the load-bearing internals of NONE of them. Specifically:

- **vs. tRPC:** worse types (manual `App`), worse error model (redaction missing), comparable wire (superjson when it works).
- **vs. Server Actions/RSC:** worse discovery (wrapper required), much worse FormData/binary story (zero), comparable streaming.
- **vs. Remix:** worse FormData (zero); better structured-RPC story when it works.
- **vs. Hono RPC:** comparable; Hono is actually simpler and arguably cleaner for the procedural-API use case.

---

## 5. Recommended direction

### Should ISS-02 Phase 2 + #168 ship together?

**Yes — these are one feature, not two.** "Function-level `"use server"`" is incoherent without "reference-graph detection" because the whole point of the function-level directive is to enable mixed files: a single file has both client utilities and server actions, and the build figures out which is which. Without reference-graph detection, the function-level directive is just a more granular wrapper — same ceremony, different syntax.

What that one feature looks like:
1. Transform's discovery walks `ExportNamedDeclaration`/`ExportDefaultDeclaration`/`VariableDeclaration` initializers AND each function body's directive prologue. Any function whose body opens with `"use server"` becomes an RPC.
2. After the per-file pass, walk Vite's module graph in `closeBundle`. For every client-bundle module, follow imports. Any imported binding that points at (a) a file with file-level `"use server"`, OR (b) a function with function-level `"use server"` becomes an RPC stub.
3. Emit the client stubs only for those RPCs (most exports stay client-side). Emit the server bundle with the originals.

**Roughly 200 LOC** of net new transform code, plus ~80 LOC of module-graph traversal. The piece is small if you defer subscription/binary support to v3. The piece is large if you tangle it with those — don't.

### Design-level changes that need to happen first

Before building Phase 2 + #168 on top, fix these foundations:

**F-1. One source of truth for wireId.** Move `pickWireId` into `sdks/types/src/wire-id.ts`; import everywhere. Delete the two duplicates.

**F-2. One typed wire format.** Pick: real superjson with `meta` round-trip, OR plain JSON. Either is fine; the current "lying superjson" path is worst of both. If superjson, the synthetic entry's `_zsRpc` MUST receive `(wireId, json, meta)` and reconstruct the value via `superjson.deserialize`. If plain JSON, drop the `transformer: "superjson"` field from the manifest and stop pretending.

**F-3. One error class hierarchy.** `RpcError` with a brand symbol, fixed code enum, and `expose: boolean` flag. Dispatch path inspects the brand: branded → forward; unbranded → redact in prod. Provide `RpcError.from(any)` for ergonomic conversion.

**F-4. One context object.** `ctx` exposed to user procedures should carry `{ signal, user, idempotencyKey, headers, request, traceId, log, waitUntil }`. ALS-backed (so it survives `await`). Build it once on the kernel side from the request envelope; freeze it; pass to `default.rpc`. Wire `set_request_user` into the worker dispatch path. Remove the half-built `auth.rs` plumbing or finish it — current state is a footgun.

**F-5. One manifest schema, validated.** Add a JSON schema for `manifest.json:resources`; validate at gateway load; reject malformed shapes loudly. Today the manifest emitter validates at build time but the gateway re-validates by reading fields piecemeal — a single Schemars-generated schema would catch drift.

**F-6. One dispatcher.** Lift `_zsRpc` and `_zsRpcAndRespond` into a shared TS module. Both the dev-bootstrap and the synthetic entry call into it. Delete the duplicate.

### What a v3 RPC would look like end-to-end

```
Authoring (user writes):
  // src/api/todos.ts
  "use server";   // file-level — every export below is server-only
  export async function listTodos({ limit }: { limit?: number }) {
    return db.todos.find({ ownerId: user().id, limit });
  }
  export async function addTodo(input: { text: string }) {
    "use server";  // redundant inside a "use server" file, but legal
    return db.todos.insertOne({ ownerId: user().id, ...input });
  }

  // src/components/TodoList.tsx — client component
  import { listTodos, addTodo } from "../api/todos";
  // build's reference-graph detection sees: client component imports from
  // a "use server" file → emit client stubs for both.

Build (vite-plugin):
  1. AST scan: discover server modules + per-function directives.
  2. Walk module graph: any client-bundle import of a server symbol →
     mark the symbol as "needs client stub".
  3. For each marked symbol: emit `__rpc("<wireId>", input, opts)` stub
     in the client bundle. Server bundle keeps the original.
  4. Emit manifest.resources with auto-derived rpc:<id> entries
     and merged `defineApp` user policy.
  5. Emit virtual:zeroship/server-api.d.ts with one declaration per
     procedure for end-to-end TypeScript inference.

Wire (one transformer, one envelope, one error shape):
  POST /_zs/v1/<wireId>
    Idempotency-Key: <uuidv7>          (when fn.config.idempotent)
    Content-Type: application/json
    Authorization: Bearer <jwt>
    {"json":<value>,"meta":{...}?}     superjson, real this time

  200 OK
    Content-Type: application/json
    {"json":<result>,"meta":{...}?}

  4xx/5xx
    Content-Type: application/zs-error+json
    {"code":"<enum>","message":"...","details":...,"retryable":bool,"trace_id":"..."}

Dispatch (kernel):
  1. Path slice + percent-decode → wireId.
  2. Body parse: superjson.deserialize({json, meta}) → input.
  3. Build ctx { user, idempotencyKey, signal, traceId, log, waitUntil }.
  4. Look up _procedures[wireId]; 404 if missing.
  5. Validate via fn.config.input (Zod, Valibot, …).
  6. Run fn(input, ctx).
  7. Branded error? Forward verbatim. Unbranded? Redact in prod.
  8. AsyncIterator? Stream via Rust-side SSE encoder (no JS re-invoke).
  9. Otherwise: superjson.serialize(result) → wire.

Streaming + Subscription (one wire):
  Streams: same POST /_zs/v1/<wireId> with Accept: text/event-stream.
           AI-SDK Data Stream Protocol (current shape is fine).
  Subscriptions: GET /_zs/v1/<wireId> with Upgrade: websocket.
                 Frame protocol from spec §6 (already designed).
                 Worker-side state-machine ties iterator lifetime
                 to WS lifetime; Rust runtime owns the TCP+frame I/O.

Auth context (ALS-backed):
  Worker dispatch decodes ZeroShip-User → set_request_user(state, rid, json).
  Procedure calls user() → reads from per-request-keyed slot.
  AsyncLocalStorage propagates across `await` automatically (already implemented).

Error contract:
  - RpcError (branded) → forward { code, message, details, retryable, trace_id }.
  - Anything else in production → { code: "INTERNAL", message: "Internal server error" }.
  - Logs full error with trace_id correlation.

Idempotency:
  - fn.config.idempotent triggers gateway dedup table.
  - Worker dispatch reads Idempotency-Key from envelope, exposes via
    ctx.idempotencyKey for user-side dedupe (e.g., DB insert with the key
    as a unique constraint).

FormData / binary:
  - superjson handles Uint8Array/Date/BigInt natively.
  - For >1MB body or `Blob`/`File`: client switches to multipart/form-data
    POST; server detects content-type, parses via existing FormData impl,
    invokes fn(formData, ctx) directly (no superjson hop).
  - <form action={addTodo}> works because addTodo is a function reference;
    React serializes the form, browser sends multipart; runtime parses.
```

This is conceptually ~1500 LOC of net work to bring the implementation up to the spec. About 60% of it is already in the tree as half-built scaffolding; the gap is integration + Rust-side stream/SSE encoding + the transform's reference-graph pass.

### Foundational pieces missing

In priority order:

1. **Brand-bearing `RpcError` + redaction in dispatch.** Without it, every prod deploy leaks errors. (Critical-3.)
2. **Wire actually carries superjson `meta`.** Without it, any user with `Date`/`Map`/`BigInt` in their schema gets silent corruption. (Critical-2.)
3. **`set_request_user` actually called in worker dispatch.** Without it, `auth.getUser()` is a no-op. (Critical-1.)
4. **Single wireId resolver.** Without it, the three-table consistency is on hope. (High-1.)
5. **Function-level + reference-graph detection.** Without it, ergonomics are worse than what shipped two years ago in Next.js. (High-2/3.)
6. **Typed wire envelope (manifest schema).** Without it, gateway/build/runtime drift compounds. (F-5.)
7. **Idempotency key threading into ctx.** Without it, the gateway store is half a feature. (High-4.)
8. **Rust-side SSE encoder.** Without it, streams pay 2x cost. (High-7.)
9. **FormData / binary path.** Without it, no React-19-style server-action UX. (Summary-10.)
10. **Virtual `.d.ts` emission.** Without it, types are worse than tRPC. (Medium-1.)

---

## 6. Risks if shipped today

### Production-reachable correctness/security

- **Error leak.** `throw new Error("DB password is xyz")` from a user procedure → wire body contains the message and full stack. (Critical-3.)
- **Auth phantom.** A user calling `zeroship.auth.getUser()` from a procedure (the public API as documented in `AGENTS.md` and the `db` SDK) gets `null` even when the gateway authenticated the request — a double-failure: silent for the unprivileged user, and a security uplift for an attacker because `requireUser()` (when wired) would throw, but `getUser()` returns null and falls through to public-anon code paths. (Critical-1.)
- **Type confusion.** A procedure returning `new Date()` round-trips as a string for clients using the auto-stub but as a real Date for clients using `client<App>()`. Two callers, two values. (Critical-2.)

### User surprise

- **"Why is my helper an RPC?"** Won't happen anymore (good). But: "Why isn't my function an RPC even though it's in `src/server/`?" will — the path convention is gone, the wrapper is required, the migration warning fires only on the legacy `src/server/**` path. A new file at `src/api/foo.ts` with `"use server"` and `export async function bar()` will silently NOT publish `bar` (because it's not wrapped). The transform helpfully says "no procedures discovered" but doesn't tell the user WHY (their helper wasn't wrapped). (High-2.)
- **"Why does my Zod schema's `.transform()` not run?"** The kernel fast path doesn't call `cfg.input.parse(input)` — only the synthetic entry does. AsyncIterator returns FallThrough through the kernel's `_zsRpc` and `cfg.input.parse(input)` runs only in the slow-path JS. So a fast-path call (sync handler, no FallThrough) DOES run validation; an iterator-returning handler runs validation TWICE; a streaming handler runs validation in the slow path. Not a correctness bug per se, but unobvious behavior. (High-7.)
- **"Why does batch fail?"** `client({ batch: true })` produces 404s because `_batch` doesn't exist server-side. (Medium-8.)
- **"Why does my subscription not connect?"** Client `subscribeCall` opens a WebSocket; runtime has no subscription handler in the synthetic entry; gateway upgrades it; worker errors out. (Summary-9.)
- **"Where's my FormData?"** `<form action={addTodo}>` posts multipart, body is parsed as JSON, throws INVALID_ARGUMENT. (Summary-10.)

### Refactor hazards

- **Renaming an export silently breaks the wire** in dev (default wireId is the export name). Production catches it (`vite build --mode production` rejects implicit ids), but dev and prod disagree. A deploy from a feature branch that never went through `--mode production` will pass CI then 404 in prod.
- **Adding a procedure with the same export name in a different file is a build error** with no migration path other than "pin an explicit id" (`manifest.ts:670-682`). Fine for new code; painful for an app that incrementally added procedures over months and now has 12 files exporting `list`. (Spec §2 Production Rule means you eventually pin every id — that's correct, but the user finds out at production-build time, not gradually.)

### Observability holes

- **No `trace_id`** propagated through the wire (spec §6 envelope includes `trace_id`; runtime never sets it).
- **No per-procedure metrics** — the worker emits `DISPATCH_TOTAL` but not per-wireId histograms (`crates/worker/src/metrics.rs` does NOT key by procedure).
- **No structured error log** linking `trace_id` to the redacted wire response, even though the spec calls for it.

### Performance traps

- **Re-invocation on FallThrough** doubles validation cost on streams. (High-7.)
- **Triple V8 round-trip on slow path: env JSON parse → request build → handler invoke.** The kernel optimized this for the fast path (`runtime.rs:1325-1330` `build_kernel_request`) but a handler that returns an AsyncIterator falls through to the slow path and pays the full cost. Production traffic hitting an `async function*` procedure (LLM streaming) takes the slow path on every call.

---

## Closing read

The spec is excellent. The authoring story is competitive; the wire format is sensible; the resource-tree manifest is a real improvement over the old `rules + policies` split. The implementation that's landed builds the **build-time pipeline** (transform, manifest, wireId resolution, validation gates) cleanly, but the **runtime** half is mostly still talking to itself: the kernel knows about `default.rpc` but doesn't know about user context; the gateway knows about idempotency but doesn't know about V8; the SDK has typed clients but no typed server emission.

The good news: there's no architectural mistake to undo. The "synthetic entry assembles a static map" pattern is right; the three-tier kernel dispatch (rpc → fetchFast → fetch) is right; the resource-tree manifest is right. The work is integration. About a quarter of the LOC needed already exists as scaffolding waiting for callers.

The bad news: the surface that's "shippable today" — what a creator interacts with — overpromises against what's wired. `user()` throws, batching 404s, subscriptions hang, errors leak, dates round-trip as strings. Anyone reading the spec and writing code against it will hit each of these in their first hour.

Net recommendation: do the foundations (F-1 through F-6) first, then ISS-02 Phase 2 + #168 together. The function-level directive without reference-graph is meaningless; the foundations without those is half a story; everything together is a coherent v1.0.

— architecture-critic

---

### Files referenced in this critique (absolute paths)

- `/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/transform.ts`
- `/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/rpc-registry.ts`
- `/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/manifest.ts`
- `/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/build.ts`
- `/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/dev-server.ts`
- `/home/ruiyang/Projects/appbase/sdks/vite-plugin/src/dev-bootstrap/index.ts`
- `/home/ruiyang/Projects/appbase/sdks/vite-plugin/test/synthetic-entry-zod.test.ts`
- `/home/ruiyang/Projects/appbase/sdks/vite-plugin/test/zod-output-dev-only.test.ts`
- `/home/ruiyang/Projects/appbase/sdks/vite-plugin/test/transform-procedures.test.ts`
- `/home/ruiyang/Projects/appbase/sdks/vite-plugin/test/transform-no-marker.test.ts`
- `/home/ruiyang/Projects/appbase/sdks/server/src/wrappers.ts`
- `/home/ruiyang/Projects/appbase/sdks/server/src/make-server-procedure.ts`
- `/home/ruiyang/Projects/appbase/sdks/server/src/index.ts`
- `/home/ruiyang/Projects/appbase/sdks/server/src/types.ts`
- `/home/ruiyang/Projects/appbase/sdks/server/src/define-app.ts`
- `/home/ruiyang/Projects/appbase/sdks/rpc-client/src/encoding.ts`
- `/home/ruiyang/Projects/appbase/sdks/rpc-client/src/transport.ts`
- `/home/ruiyang/Projects/appbase/sdks/rpc-client/src/client.ts`
- `/home/ruiyang/Projects/appbase/sdks/rpc-client/src/error.ts`
- `/home/ruiyang/Projects/appbase/sdks/rpc-client/src/idempotency.ts`
- `/home/ruiyang/Projects/appbase/crates/runtime/src/core/runtime.rs` (lines 1100-1432, 2480-2929)
- `/home/ruiyang/Projects/appbase/crates/runtime/src/core/dispatch.rs`
- `/home/ruiyang/Projects/appbase/crates/runtime/src/core/serve.rs`
- `/home/ruiyang/Projects/appbase/crates/runtime/src/transport/handler.rs`
- `/home/ruiyang/Projects/appbase/crates/runtime/src/auth.rs`
- `/home/ruiyang/Projects/appbase/crates/runtime/src/node/async_hooks/als.rs`
- `/home/ruiyang/Projects/appbase/crates/runtime/tests/rpc.rs`
- `/home/ruiyang/Projects/appbase/crates/runtime/tests/common/mod.rs`
- `/home/ruiyang/Projects/appbase/crates/gateway/src/router/dispatch.rs`
- `/home/ruiyang/Projects/appbase/crates/gateway/src/proxy.rs`
- `/home/ruiyang/Projects/appbase/crates/gateway/src/idempotency.rs`
- `/home/ruiyang/Projects/appbase/crates/gateway/src/user_auth.rs`
- `/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs`
- `/home/ruiyang/Projects/appbase/docs/proposals/rpc-v2.md`
