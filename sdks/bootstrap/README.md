# @zeroship/bootstrap

Framework-internal coordination package for zeroship.

**User code MUST NOT import this package.** It is consumed by:

- The runtime crate (`crates/runtime`), via `include_str!` of files in
  `dist/` so the dispatcher and DB-init orchestrator are spliced into
  every isolate's bootstrap module.
- The Vite plugin (`@zeroship/vite-plugin`), via `import` in
  `src/dev-bootstrap/index.ts` so dev-mode dispatch + descriptor install
  share the same logic as production.

The public surface here is "framework-stable" — it can change without
deprecation as long as the runtime crate and Vite plugin are updated in
lockstep.

## What lives here

- `install-schema.ts` — `installSchema(schema, env, { descriptor }) →
  { collections, ready }`, the framework-internal installer for the
  generated `RuntimeSchemaDescriptor`. Walks the descriptor-derived schema
  map, calls `registerModel` in topo order, and plants typed Collection
  wrappers on `env.db`. Helpers: `model()`, `validateRefTargets()`,
  `topoSortByRefs()`, `normalizeSchema()`.
- `dispatcher.ts` — `__zsDispatch(rpcDict, name, input, ctx)`. Owns
  input parse / capability frame / stream framing / dev-only
  output validation. Same logic for dev and prod.
- `normalize.ts` — `normalizeUserModule(mod) → { fetch, rpc, userDefault }`.
  Turns a user module namespace into the standard ZS shape.
- `fetch-handler.ts` — WinterCG `fetch` wrapper that routes
  `/__zeroship/v1/<id>` through the dispatcher and falls through to the
  user's own `default.fetch`.
- `runtime-entry.ts` — TLA orchestrator the runtime crate
  `include_str!`s. Reads the injected `RuntimeSchemaDescriptor`, calls
  `installSchema`, and exposes readiness for the dispatcher.
- `dev-entry.ts` — dev-mode equivalent that wires the dispatcher /
  fetch handler / schema install around a user-supplied module loader
  (e.g. Vite's ModuleRunner).

## Build ordering

`pnpm -F @zeroship/bootstrap build` MUST run before
`cargo build -p zeroship-runtime`. The runtime crate's
`crates/runtime/src/core/init.rs` does `include_str!` against
`dist/runtime-entry.js` and `dist/dispatcher.js`; absent dist files
fail the cargo build with a clear "file not found" message.

Run `pnpm build` from the workspace root to ensure correct ordering.
