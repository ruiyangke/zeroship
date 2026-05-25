# The ZS Standard + Vite plugin v2

**Status:** Shipped (Stages 5a-5e landed; HEAD `efbac21f` + 5e cleanup)
**Authors:** ruiyang + pilot
**Date:** 2026-05-20

> Historical record. The reference doc for end users is
> `docs/reference/zs-standard.md`. One decision differs from the original
> proposal: **function-shape `default.rpc` is a permanent advanced /
> back-compat path** (Open Q1 reversed). The dev-bootstrap exports
> function-shape because dev needs per-request namespace re-resolution.
> Dict-shape stays the canonical default for production bundles and raw
> JS deploys.

## TL;DR

A deployable zeroship app is a JS module exporting `default = { schema?, fetch?, rpc? }`. Tooling (Vite, esbuild, swc, raw JS) is optional. The runtime owns dispatch; the Vite plugin's job shrinks to "transform idiomatic source code into the standard shape."

## Motivation

Today (post-Stage-4):

- The synthetic SSR entry generates ~600 lines of dispatch helpers (`_zsRpc`, `_zsFetch`, `_zsRpcWithAutoTx`, `_zsRpcPost`) into every production bundle.
- The dev-bootstrap mirrors this with its own dispatch implementation (~500 lines).
- The Vite plugin is effectively a hard dependency for any non-trivial app.
- Raw `.js` deploys work for `fetch`-only apps but cannot expose RPC procedures without rebuilding the dispatch logic.

Problems:

1. **Two dispatch implementations.** Prod (synthetic entry) and dev (dev-bootstrap) duplicate logic. Drift = subtle bugs (the R3 cold-start race, the R4 nested-tracker leak, the R7 paginate cursor bug all touched both).
2. **Vite-coupled deploy.** The contract is generated, not documented. Users can't write a portable JS file and deploy it. Other tools (esbuild, swc) cannot produce a valid deployable without re-implementing the synthetic-entry shape.
3. **Magic transformation.** Source-code conventions (`export const listTodos = query(async ...)`) are decoded by Vite-side regex/AST. A user reading their own code can't easily see what the wire shape looks like.

Goal:

- **The deploy artifact is a JS module exporting a documented standard shape.** Anyone can produce one — by hand, by Vite, by another tool.
- **The runtime owns dispatch.** Single implementation, used in dev and prod identically.
- **The Vite plugin is convenience.** HMR, source-to-standard transformation, asset pipeline. Optional.

## The ZS standard

```ts
// The deploy contract — what the runtime reads from `default` on the user entry.
interface ZeroshipApp {
  /** Database schema. Runtime calls `_installSchema(schema)` at boot. */
  schema?: Record<string, SchemaShape>;

  /** WinterCG HTTP handler. Called for all non-`/_zs/v1/<id>` URLs. */
  fetch?: (request: Request, env: Env, ctx: Ctx) => Response | Promise<Response>;

  /** RPC dispatch table. URL `/_zs/v1/<wireId>` invokes `rpc[wireId]`. */
  rpc?: Record<string, Procedure>;
}

type Procedure =
  & ((input: any, ctx?: Ctx) => any | Promise<any> | AsyncIterable<any>)
  & { config?: ProcedureConfig };

interface ProcedureConfig {
  /**
   * Capability bucket. Drives auto-tx + capability-frame enforcement:
   *   - "query"        read-only; READ ONLY tx; refuses fetch.
   *   - "mutation"     read+write; SERIALIZABLE tx; refuses fetch.
   *   - "action"       no auto-tx; can call fetch; must use runQuery/runMutation.
   *   - "stream"       AsyncIterator; no auto-tx; SSE-framed.
   *   - "subscription" AsyncIterator; no auto-tx; WS-framed.
   * Default: no capability marker, no auto-tx wrap.
   */
  kind?: "query" | "mutation" | "action" | "stream" | "subscription";

  /** Optional Zod (or .parse()-compatible) input schema. Pre-validated by dispatcher. */
  input?: { parse: (v: unknown) => unknown };

  /** Optional output schema. Dev-only post-validation. */
  output?: { parse: (v: unknown) => unknown };

  /** Override wireId. Default: property key. */
  id?: string;

  /** Postgres isolation override for query/mutation. Default: SDK default. */
  isolation?: "READ COMMITTED" | "REPEATABLE READ" | "SERIALIZABLE";
}
```

All three top-level keys are optional. Examples:

```js
// Minimal — static or 404 with no JS handler.
// (No entry needed at all; deploy assets only.)

// HTTP-only — raw JS deploy, no tooling required.
export default {
  fetch(req, env) { return new Response("hi"); }
};

// RPC-only — raw JS deploy.
export default {
  rpc: {
    listTodos: (input, ctx) => env.db.todos.find({}),
    addTodo: Object.assign(
      (input, ctx) => env.db.todos.insert(input),
      { config: { kind: "mutation" } }
    ),
  }
};

// Full SSR + RPC + DB.
import { t } from "@zeroship/db";
import { query, mutation } from "@zeroship/server";

const listTodos = query((input, ctx) => env.db.todos.find({}));
const addTodo = mutation((input, ctx) => env.db.todos.insert(input));

export default {
  schema: { todos: { title: t.string() } },
  fetch(req, env, ctx) { return new Response("..."); },
  rpc: { listTodos, addTodo },
};
```

### Why dict-shape `rpc`, not function?

Today's contract: `default.rpc(name, input, ctx)` is a function. The synthetic entry generates it.

The new contract: `default.rpc` is a plain object mapping wireId → handler. Two reasons:

1. **Raw deploys.** A user writing `default.rpc = (name, input, ctx) => switch(name) { ... }` by hand is awful. A dict literal is natural.
2. **Inspectable.** The wire surface is data, not code. The runtime can enumerate procedures (`Object.keys(default.rpc)`), the gateway can do introspection, future tooling can do typegen against the dict.

The dispatcher logic (input validation, capability, auto-tx, stream framing) moves to the runtime — see "Runtime contract" below.

### Procedure metadata

`fn.config = { kind, input, output, id }` is the contract for procedure metadata. The wrappers in `@zeroship/server` (`query`, `mutation`, `action`, `stream`, `subscription`) attach this:

```ts
// @zeroship/server (existing)
export function query(fn, opts) {
  return Object.assign(fn, { config: { kind: "query", ...opts } });
}
```

Users can also attach `config` directly without the wrappers if they prefer.

## Runtime contract

The runtime — `crates/runtime` — at isolate boot:

1. **Load the user entry.** Manifest's `worker.entry` (existing field) points at the bundle's main module.
2. **Read `default.schema`.** If present, `await sdk._installSchema(schema, { installOnEnvDb: true })`. (Existing Stage 4 path; unchanged.)
3. **Read `default.rpc`.** If a dict, install a kernel-side dispatcher that knows how to invoke entries with metadata. If a function (back-compat with v1 synthetic entries), use directly.
4. **Read `default.fetch`.** Bind as the WinterCG handler (existing).
5. **Bind HTTP routes:**
   - `/_zs/v1/<wireId>` → dispatcher → `rpc[wireId]` (with metadata handling).
   - WebSocket upgrade on `/_zs/v1/<wireId>` → dispatcher (subscription path).
   - everything else → `default.fetch` (existing).

The dispatcher (embedded in `crates/runtime/src/bootstrap/rpc_dispatch.js`) implements:

```js
async function dispatch(rpc, name, input, ctx) {
  const fn = rpc[name];
  if (typeof fn !== "function") {
    throw mkErr("Method not found: " + name, 404, "NOT_FOUND");
  }

  const cfg = fn.config;

  // 1. Input validation (Zod-compatible).
  let validated = input;
  if (cfg?.input?.parse) {
    try { validated = cfg.input.parse(input); }
    catch (e) { throw mkErr("Invalid input", 400, "INVALID_ARGUMENT", { issues: zodIssues(e) }); }
  }

  // 2. Capability frame (refuses cross-kind violations natively).
  const kind = cfg?.kind;
  const tok = (kind && globalThis.__zsEnterKind) ? globalThis.__zsEnterKind(kind) : -1;

  // 3. Auto-tx for query/mutation.
  const wantsAutoTx =
    (kind === "query" || kind === "mutation") &&
    typeof globalThis.__zsBeginAutoTx === "function";

  try {
    let result;
    if (wantsAutoTx) {
      const token = await globalThis.__zsBeginAutoTx(kind, cfg?.isolation ?? "");
      try {
        result = await fn(validated, ctx);
        await globalThis.__zsEndAutoTx(token, true);
      } catch (e) {
        try { await globalThis.__zsEndAutoTx(token, false); } catch {}
        throw e;
      }
    } else {
      result = await fn(validated, ctx);
    }

    // 4. AsyncIterator stream framing tag.
    if (isAsyncIterable(result) && cfg?.output && isZodString(cfg.output)) {
      try { result.__zsOutputIsString = true; } catch {}
    }

    // 5. Output validation (dev only — handled by environment flag).
    if (cfg?.output?.parse && globalThis.__zsValidateOutput) {
      try { cfg.output.parse(result); }
      catch (e) { throw mkErr("Invalid handler output", 500, "INTERNAL", { issues: zodIssues(e) }); }
    }

    return result;
  } finally {
    if (tok >= 0 && globalThis.__zsExitKind) globalThis.__zsExitKind(tok);
  }
}
```

This module is `include_str!()`'d into the runtime crate. The runtime evaluates it once at isolate boot, then calls the exported `dispatch(...)` for every `/_zs/v1/<id>` request. ~150 lines of embedded JS, replaces the ~600 lines the Vite plugin generates per app today.

## Vite plugin v2 — what it does

**One purpose:** transform user source code into the standard shape.

### Inputs the plugin accepts

| User writes | Plugin emits on `default` |
|---|---|
| `export default { schema, fetch, rpc }` | Pass-through (explicit standard). |
| `export const listTodos = query(async ...)` (in entry or per-file) | `default.rpc.listTodos = wrapped fn` |
| `export function fetch(req, env, ctx) { ... }` | `default.fetch = fn` |
| `src/schema.ts` exports default | Re-exported as `default.schema` |
| `src/api/{query,mutation,action}/*.ts` (future: file-based routing) | Each becomes a `default.rpc[name]` entry |

### Synthetic entry (the new shape)

```js
// virtual:zeroship/_entry — Vite-generated
import * as user from "./src/index.ts";

const _userDefault = (user && typeof user.default === "object") ? user.default : {};

const _rpc = { ..._userDefault.rpc };
for (const [name, fn] of Object.entries(user)) {
  if (name === "default" || name === "fetch" || typeof fn !== "function") continue;
  const id = (fn.config?.id && typeof fn.config.id === "string") ? fn.config.id : name;
  _rpc[id] = fn;
}

export default {
  schema: _userDefault.schema,
  fetch:  _userDefault.fetch ?? user.fetch,
  rpc:    _rpc,
};
```

~30 lines. No dispatcher. No helpers. Pure normalization.

### What the plugin v2 stops doing

- `_zsRpc` / `_zsFetch` / `_zsRpcWithAutoTx` / `_zsRpcPost` generation — moved to runtime.
- Capability frame management — moved to runtime.
- Auto-tx wrapping — moved to runtime.
- Zod input/output validation — moved to runtime.
- AsyncIterator stream framing — moved to runtime.
- Schema discovery via `manifest.exports.schema` — runtime reads `default.schema` off the entry directly.

### Module layout (target)

```
sdks/vite-plugin/src/
  index.ts              # zeroship({...}) plugin factory + options
  normalize-entry.ts    # ~30-line synthetic-entry generator (above)
  client-transform.ts   # rewrite client imports of server procedures into RPC stubs (KEPT — this is the one non-trivial transform)
  node-compat.ts        # node:* polyfills (kept)
  zeroship-module.ts    # `zeroship` virtual module (kept)
  dev/
    server.ts           # spawn runtime, Vite ModuleRunner bridge, HMR ws (slimmed)
    bootstrap.ts        # normalizer-only — no RPC dispatch (~25 lines, replaces today's ~500)
  build/
    zship.ts            # .zship archive emission (kept)
    rollup.ts           # SSR Rollup invocation (kept)
```

### What disappears

- `sdks/vite-plugin/src/rpc-registry.ts` — gone (dispatch logic moved to runtime).
- `sdks/vite-plugin/src/dev-bootstrap/index.ts` (~500 lines) — replaced by `dev/bootstrap.ts` normalizer.
- `sdks/vite-plugin/src/resolve-schema.ts` — gone (schema lives on `default.schema`; no manifest path).
- The manifest's `exports.schema` field — deprecated; kept as `Option<String>` on the wire but unused by runtime.

## Migration plan

Five sub-stages. Each shippable, smoke-testable, and additive (back-compat preserved through 5b; cleanup in 5d).

### 5a — Runtime grows dict-shape `default.rpc` dispatcher

**New:** `crates/runtime/src/bootstrap/rpc_dispatch.js` — the dispatcher described above. Embedded via `include_str!`. Evaluated at isolate boot. Exports `__zsKernelDispatch` (or similar) on globalThis.

**Runtime change:** `crates/runtime/src/core/init.rs` (around line 656) — when `user.default.rpc` is an object, wrap it in a dispatcher; when it's a function, use directly (back-compat).

**Acceptance:**
- Two new runtime tests: dict-shape dispatch + back-compat function-shape dispatch.
- `examples/db-todos` smoke passes (still uses function-shape via current Vite plugin; back-compat path proves it works).
- Add a fixture that exports dict-shape directly (no Vite); verify dispatch.

**Risk:** Two dispatch paths coexist. Mitigation: tests cover both; the new path is opt-in (only activates when default.rpc is a dict).

### 5b — Vite plugin v2 — synthetic entry switches to dict-shape

**Change:** `sdks/vite-plugin/src/rpc-registry.ts` → `sdks/vite-plugin/src/normalize-entry.ts`. New generator emits the 30-line normalizer above. Old `_zsRpc`/`_zsFetch` helpers deleted.

**Change:** `sdks/vite-plugin/src/dev-bootstrap/index.ts` → `sdks/vite-plugin/src/dev/bootstrap.ts`. Becomes a normalizer; runtime dispatches.

**Tests:** Update `rpc-registry.test.ts` (or rename to `normalize-entry.test.ts`) — assertions are now "default.rpc is an object literal mapping names to functions" instead of "default.rpc is a function calling _zsRpc".

**Acceptance:**
- All vite-plugin tests pass.
- db-todos smoke passes.
- Production bundle of db-todos has `default.rpc` as a dict literal (not a function).
- Dev mode HMR still works.

**Risk:** Behavior change for production deploys (new dispatch path). Mitigation: 5a's back-compat means the runtime handles both shapes during the transition.

### 5c — Drop `manifest.exports.schema` reliance

**Change:** `crates/runtime/src/bootstrap/db_init.js` reads `default.schema` off the loaded entry module instead of dynamic-importing via `manifest.exports.schema`. Removes the need for the Vite plugin to compute + bake the path.

**Change:** `sdks/vite-plugin/src/resolve-schema.ts` deleted. Schema lives on the entry's `default.schema`; Vite plugin's only schema concern is re-exporting `src/schema.ts` (when split) onto the synthetic entry's default.

**Manifest:** `Manifest.exports.schema` field deprecated (kept on the wire for graceful upgrade; future stage removes it).

**Acceptance:**
- db-todos smoke passes (no more `[zeroship] schema resolved via …` log; runtime reads from entry).
- Tests for the resolver are deleted.

### 5d — Migrate demos

**Audit:** Most demos go through the Vite plugin transparently — 5b's change to the synthetic entry covers them. No source-code change required.

Demos to verify smoke-passes:
- `db-todos` (the canonical Vite + DB + RPC + SSR demo)
- `csr-todo`, `db-chat`, `db-migrations-playground`, `hr-system`, `ssr-blog`, `ssg-docs`, `ai-chat`, `bench`, `hono-demo`, `langchain-demo`, `openai-demo` — all Vite-built; smoke after 5b.

Raw `.js` demos (already conform to standard):
- `examples/http-handler.js` — `default.fetch` only; works pre-5b.
- `examples/url-shortener.js` — `default.fetch` + KV bindings; works pre-5b.
- `examples/jwt-validator.js`, `examples/weather-proxy.js`, `examples/ai-streaming.js` — `fetch`-only; works.

**New raw-JS RPC demo:** add `examples/raw-rpc.js` demonstrating dict-shape `default.rpc` without any tooling. Proves the contract works end-to-end without Vite. Add a smoke script.

### 5e — Cleanup

- **Function-shape `default.rpc` STAYS as a permanent advanced /
  back-compat path** (Open Q1 reversed). The dev-bootstrap uses
  function-shape because the user namespace re-resolves per request
  under HMR; raw deploys may legitimately want custom dispatch (dynamic
  routing, multi-tenant prefix matching). Dict-shape stays canonical
  for production bundles.
- `manifest.exports.schema` field is deprecated on the wire (still
  serialised for graceful upgrade; runtime never reads it).
- Update docs:
  - `docs/reference/db.md` — the schema-discovery section refers to `default.schema` on the entry.
  - New `docs/reference/zs-standard.md` — the export contract documented as a reference.
  - `AGENTS.md` — task router updated.
- Stale comments referencing removed helpers (`_zsRpc`, `_zsFetch`,
  `_zsRpcWithAutoTx`, `_zsRpcPost`, `__zsManifestSchemaPath`) cleaned
  out of active source.

## Demos audit

Quick survey of what each demo demonstrates and its current shape:

| Demo | Shape today | Notes |
|---|---|---|
| `ai-chat` | Vite + RPC + AI SDK | streaming procedures |
| `ai-streaming.js` | Raw `.js` + fetch only | no RPC |
| `bench` | Vite + RPC | perf benchmarks |
| `csr-todo` | Vite + client-side React + RPC | no DB |
| `db-chat` | Vite + DB + RPC | chat over db.live |
| `db-migrations-playground` | Vite + DB + migrations | exercises migration API |
| `db-todos` | Vite + DB + RPC + SSR | the canonical smoke target |
| `hono-demo` | Vite + Hono + RPC | hono router as default.fetch |
| `hr-system` | Vite + DB + RPC | 25-collection schema |
| `http-handler.js` | Raw `.js` + fetch + RPC (legacy function-shape) | currently uses function rpc(name,input) |
| `jwt-validator.js` | Raw `.js` + fetch only | |
| `langchain-demo` | Vite + LangChain + RPC | |
| `openai-demo` | Vite + OpenAI + RPC | |
| `ssg-docs` | Vite + SSG | no worker, just static |
| `ssr-blog` | Vite + SSR | per-request rendering |
| `url-shortener.js` | Raw `.js` + fetch + KV | |
| `weather-proxy.js` | Raw `.js` + fetch only | |

After 5b, the Vite-built demos shift transparently. `http-handler.js` needs to update its function-shape `rpc` to dict-shape (or stay function-shape if 5a's back-compat is kept through 5d).

## Risks and open questions

### 1. Function-shape `default.rpc` back-compat

Today's contract was function-shape (the old synthetic entry generated
it). After 5b, the Vite plugin emits dict-shape. The original 5e plan
said "drop function-shape support."

**Decision (final, 5e):** function-shape STAYS as a permanent
advanced / back-compat path. Two reasons:

1. The dev-bootstrap NEEDS function-shape because the user module's
   namespace re-resolves per request under HMR — a dict captured at
   module-init would go stale on every edit.
2. Raw JS deploys may legitimately want custom dispatch (dynamic
   routing, multi-tenant prefix matching, audit-log wrappers).

The runtime checks `typeof rpc === "function"` first; otherwise treats
it as a dict. Most users use dict (Vite emits it; raw deploys default
to it); advanced users override. Documented in
`docs/reference/zs-standard.md` under "Advanced: function-shape
default.rpc".

### 2. Wrappers (`query`, `mutation`, `action`)

Today they live in `@zeroship/server`. They attach `fn.config = { kind }`. After 5a, the runtime reads `fn.config.kind` to apply auto-tx + capability frames.

**Question:** should the wrappers move to a different package (e.g. `@zeroship/db` since the auto-tx is db-specific)? Or stay in `@zeroship/server`?

**Recommendation:** stay in `@zeroship/server`. The wrappers are a server-side ergonomic — they pre-date db-specific auto-tx and serve as the canonical attachment point for `kind`. Moving them couples server SDK to db SDK.

### 3. Subscriptions over WebSocket

The runtime's WebSocket upgrade path (`_zsAcceptSubscription`) currently does `await user.default.rpc(name, input)` and expects an AsyncIterator. After 5a, this routes through the new dispatcher, which handles AsyncIterator results uniformly.

**No change needed.** Subscriptions ride the same dispatcher.

### 4. fetchFast opt-in

`init.rs:647` shows the runtime supports `default.fetchFast(method, url, body, env)` as a fast-path. This is independent of `default.fetch` / `default.rpc` and stays unchanged. Documenting it in `zs-standard.md` so users know it exists.

### 5. Client RPC stub generation

The Vite plugin's `client-transform.ts` rewrites client-side imports of server procedures into RPC stubs. This stays in the Vite plugin (it's an idiomatic-source-to-build-output transform).

**Question:** should raw-JS users get a CLI to generate client stubs from their `default.rpc` dict? `zeroship gen-client ./main.js → ./client.ts`?

**Recommendation:** later. Out of scope for 5a-5e. Standalone tool when there's user demand.

### 6. HMR semantics

When the user edits their schema in dev:
- Vite invalidates the schema module.
- Dev-bootstrap (now a normalizer) re-imports → produces a new `default.schema`.
- Runtime needs to re-run `_installSchema` against the new shape.

The R3 re-entrancy guard handles single-threaded re-installs. The DDL chain serialization handles parent-before-child sequencing across reinstalls. **Documented as a feature, not a side effect.**

### 7. Bundle size

Today the synthetic entry adds ~10KB of dispatch helpers to every bundle. After 5b, those ~10KB are in the runtime crate (shipped once per worker), not per-app. Net win for cold-start + multi-app worker memory.

## Implementation order

Sequential — each depends on the prior:

1. **5a** (1-2 days): runtime dispatcher.
2. **5b** (1 day): Vite plugin v2.
3. **5c** (half day): drop `manifest.exports.schema` reliance.
4. **5d** (half day): demo migration + smokes.
5. **5e** (half day): cleanup + docs.

Each stage gets a critic+fixer pass per the established pattern. Total: ~5 days of focused work.

## Out of scope

- Runtime dispatch in Rust (vs embedded JS). The dispatch logic stays JS because it invokes user JS handlers. Moving it into Rust would require N more FFI calls per request and gain nothing.
- File-based discovery (`src/api/{query,mutation}/*.ts`). The user's earlier discussion converged toward this, but it's an opinion-bound surface change. Defer until Vite plugin v2 stabilizes.
- Collection wrappers in native. Discussed at length; not worth the iteration-speed cost pre-1.0.
- Renaming `Db<T>` → `EnvDb<T>`. Deferred from R1 of the SDK cleanup loop.

## Decision log

- **Dict-shape `default.rpc`:** chosen over function-shape because raw deploys need an ergonomic way to declare multiple procedures. Function-shape stays as a back-compat / advanced path.
- **Dispatcher in embedded JS, not Rust:** the dispatch logic invokes user JS; Rust would only add FFI overhead. Embedded JS keeps everything one language.
- **Schema on entry's `default.schema`:** simpler than the Stage-1 manifest-path approach. The Vite plugin re-exports `src/schema.ts` if the user split it out.
- **Keep `@zeroship/server` wrappers separate from `@zeroship/db`:** they're a server-side ergonomic that pre-dates db auto-tx coupling.
> Archived 2026-05-25: shipped. Live reference: docs/reference/zs-standard.md + docs/reference/vite-plugin.md.
