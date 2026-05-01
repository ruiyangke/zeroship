# Known Issues

Tracking known platform-level issues with no immediate fix landed. Each entry
is a self-contained workaround note for downstream work plus a `Fix path:`
pointer for whoever picks it up.

---

## ISS-01 · `node:async_hooks` / `AsyncLocalStorage` not propagated by `@zeroship/vite-plugin`

**Status:** open
**Severity:** medium-high — forces unnatural code shapes for any agent doing structured human-in-the-loop
**First observed:** 2026-05-01, building the project-creation wizard on plain LangGraph (`apps/zeroship-builder/src/server/_wizard.ts`)
**Component:** `sdks/vite-plugin` (dev mode, RPC-handler module loading)

### Symptom

Calling `interrupt()` from `@langchain/langgraph` after **any** awaited HTTP
fetch (e.g. `await model.invoke(...)`) inside a `StateGraph` node throws:

```
Error: Called interrupt() outside the context of a graph.
    at interrupt (.../dist-DKxU9jCg.js:8994:21)
    at clarifierBody (.../src/server/_wizard.ts:307:24)
    at RunnableCallable.invoke (.../dist-DKxU9jCg.js:9052)
```

The same `interrupt()` call **succeeds** if placed at the top of the node body
before any awaits.

### Root cause

`@langchain/langgraph` initializes its singleton via:

```js
import { AsyncLocalStorage } from "node:async_hooks";
AsyncLocalStorageProviderSingleton.initializeGlobalInstance(new AsyncLocalStorage());
```

then uses `runWithConfig(config, callback)` to attach the per-node config to
the storage so `interrupt()` can find the graph context via
`AsyncLocalStorageProviderSingleton.getRunnableConfig()`.

This requires:

1. A working `node:async_hooks` module in the V8 isolate.
2. An `AsyncLocalStorage` whose `.run(value, cb)` actually preserves `value`
   across `await` boundaries — including continuations that resume from
   native (Rust-implemented) async work like `fetch`.

The Vite plugin currently does NOT supply either of those:

- `crates/runtime/src/embed/` has no `async_hooks` polyfill (`node-globals.js`
  doesn't define one). `docs/reference/node-compat.md` describes one
  conceptually, but it isn't shipped.
- Whatever shim resolves `node:async_hooks` in the dev bundle either returns
  the **mock** `AsyncLocalStorage` from `@langchain/core` (a no-op whose
  `getStore()` always returns `undefined`) or a closure-based polyfill that
  doesn't survive native fetch resumption.

Empirical evidence: `interrupt()` works at the top of a node (no await
before it) but fails after `await model.invoke()`. That is the exact
signature of the storage being torn down at the V8/Rust async boundary.

### Affected code paths

Any langchain/langgraph code that calls one of these after an HTTP fetch:

- `interrupt(value)` — human-in-the-loop halt
- `getStore()` — context vars
- `getCurrentTaskInput()` — node input access
- `getWriter()` / `getStreamWriter()` — custom-stream emit
- `getStore` / `getRunnableConfig()` — generic config lookup

This affects **plain LangGraph** code we write directly. **deepagents**
agents (the Builder runtime) currently work because their natural shape
separates the LLM call (its own RunnableCallable) from the tool body that
calls `interrupt()` — the AsyncLocalStorage gets re-set when the next
RunnableCallable starts. So Builder is fine; only hand-rolled StateGraphs
have to dodge this.

### Workaround in use

Split any node that needs both an await and an `interrupt()` into **two
nodes**:

1. **`decide`** node: does the fetch (`await model.invoke(...)`) and stashes
   the result in graph state. No `interrupt()` here.
2. **`act`** node: reads the stashed result; if it needs to halt, calls
   `interrupt()` with NO awaits before it.

Routing: `decide → act → (loop or END)`.

Reference implementation: `apps/zeroship-builder/src/server/_wizard.ts`
(`decide` + `act` nodes; comment-block at the top documents the workaround).

### Related references

- `docs/reference/node-compat.md:237-313` — describes the AsyncLocalStorage
  polyfill that should exist (currently aspirational)
- `apps/zeroship-builder/src/server/_wizard.ts` — current workaround, with
  inline comment block explaining the failure mode
- `apps/zeroship-builder/node_modules/.vite/deps_zeroship/dist-DKxU9jCg.js:8994` —
  the `interrupt()` implementation that throws the visible error
- `apps/zeroship-builder/node_modules/.vite/deps_zeroship/base-BsBbQDQi.js:13775-13817` —
  the `MockAsyncLocalStorage` and `AsyncLocalStorageProvider` from
  `@langchain/core` that gets used when no real instance is initialized

---

## ISS-02 · `@zeroship/vite-plugin` registers every exported function as an RPC procedure — no opt-in marker

**Status:** open
**Severity:** medium — silently publishes internal helpers as public endpoints if a developer mis-routes an import
**First observed:** 2026-05-01, while documenting the underscore-prefix file convention in `apps/zeroship-builder/src/server/`
**Component:** `sdks/vite-plugin/src/rpc-registry.ts`

### Symptom

Any function exported from a module re-exported by `src/server.ts` becomes a
live RPC procedure at `/_zs/v1/<exportName>`. There is no opt-in marker
(decorator, `.config`, naming rule). The plugin's procedure-discovery loop is:

```js
// sdks/vite-plugin/src/rpc-registry.ts:97-106
import * as _zsUser from <userImport>;
const _procedures = {};
for (const _k of Object.keys(_zsUser)) {
  if (_k === "default") continue;
  const _v = _zsUser[_k];
  if (typeof _v !== "function") continue;
  const _id = (_v.config && typeof _v.config.id === "string" && _v.config.id) || _k;
  _procedures[_id] = _v;
}
```

If a developer writes `export * from "./server/_translator"` in `server.ts` to
get a type or share a helper, every exported function in `_translator.ts`
(`buildTranslatedStream`, `convertUIMessagesToLangChain`, …) is registered as
a public, callable, network-reachable RPC endpoint. That's an unintended
attack/abuse surface — these helpers take stream writers, abort signals,
and other server-internal arguments that have no safe public-input shape.

### Why it's a real risk, not theoretical

- Internal helpers often have **looser input validation** because they trust
  their callers (other server modules). Once exposed via RPC, they accept
  arbitrary user input.
- The leak is **silent**: there's no startup warning, no diff in the manifest
  emitter, no "this looks suspicious" check. A misplaced `export * from` in
  a code review is easy to miss.
- The current safety relies entirely on the file-naming convention
  (underscore-prefixed files = "internal"). Conventions don't enforce
  themselves; the next contributor onboarding has no automated guardrail.

### Workaround in use

Project-side discipline:

1. Underscore-prefix every file under `src/server/` that contains internal
   helpers (`_translator.ts`, `_middleware.ts`, `_critic.ts`, `_tools.ts`,
   `_prompts.ts`, `_sandbox_backend.ts`, `_survey_wire.ts`, `_wizard.ts`).
2. Only re-export non-underscore files from `src/server.ts`.
3. Comment block at the top of each `_*.ts` file noting it is internal.

This is fragile — one accidental `export * from "./server/_wizard"` would
publish `buildWizardStream` as an RPC endpoint that takes a writer and an
abort signal as arguments.

### Related references

- `sdks/vite-plugin/src/rpc-registry.ts:90-110` — the discovery loop that
  registers everything
- `apps/zeroship-builder/src/server.ts` — current opt-in re-export entry
  (the only thing standing between an internal helper and a public endpoint)
- `apps/zeroship-builder/src/server/_*.ts` — the eight files that rely on
  the underscore-naming convention to stay private
