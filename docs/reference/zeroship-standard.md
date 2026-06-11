# zeroship standard

The deploy contract is centered on the app entry module's default export. The
runtime bootstrap code is in `sdks/bootstrap/src/runtime-entry.ts`, and the
runtime-side loader lives in `crates/runtime/src/core/init.rs`.

## Default export

The current shape is:

```ts
export default {
  schema?,   // optional database schema
  fetch?,    // request handler
  rpc?,      // RPC procedure object keyed by wire id
  fetchFast? // optional fast-path handler
}
```

- `schema` is installed through `sdks/bootstrap/src/install-schema.ts`.
- `fetch` is the WinterCG-style request handler.
- `rpc` is the procedure object dispatched by
  `sdks/bootstrap/src/dispatcher.ts`.
- `fetchFast` is still recognized by the runtime in
  `crates/runtime/src/core/init.rs` and
  `crates/runtime/src/core/runtime.rs`.

## RPC authoring

`@zeroship/rpc/server` (in `sdks/rpc/src/server.ts`) provides the wrapper
helpers (`procedure`, `query`, `mutation`, `action`, `stream`,
`subscription`) — see [docs/reference/rpc.md](./rpc.md) for the canonical
list and signatures.

Named exports are normalized into the runtime RPC object by the Vite plugin's
synthetic server entry and `sdks/bootstrap/src/normalize.ts`. The runtime-owned
dispatch path is `/__zeroship/v1/<wireId>`; user code does not route that path
manually.

Production RPC resources require explicit wire IDs:

```ts
export const listTodos = query(handler, { id: "todos.list" });
```

Development accepts export-name IDs so local iteration stays fast, but deploy
builds reject implicit names.

`subscription` is currently server-side metadata plus lower-level transport work; the generic `@zeroship/rpc/client` API intentionally excludes it until the public subscription client shape is finalized.

## Reserved paths

The `/__zeroship/*` URL prefix is platform-reserved — user code does not route
it. Paths under it today:

- `/__zeroship/v1/<wireId>` — runtime-owned RPC dispatch (see RPC authoring above).
- `GET /__zeroship/health` (alias `GET /__zeroship/healthz`) — the `zeroship serve`
  (single-tenant dev) liveness probe, answered by the kernel without entering V8.
  The bare `GET /health` route is **not** reserved: it reaches the user app's
  own handler.

## Local env injection (`zeroship serve`)

Under `zeroship serve` there is no control plane to supply per-app vars/secrets,
so the app-facing `env` (the `zeroship` module's `env` import and `fetch`'s 2nd
argument) is seeded from process-env vars carrying the `ZS_VAR_` prefix, with the
prefix stripped: `ZS_VAR_API_KEY=xyz zeroship serve app.js` makes
`env.API_KEY === "xyz"`. Only prefixed vars cross into `env`; every other host
var stays in `process.env` exclusively, so the host environment is never handed
to app code wholesale.

## Current source of truth

`default.schema` is the active schema discovery path. The manifest-side
`exports.schema` field still exists in the bundle wire struct, but it is not the
authoring contract for new code. See `crates/bundle/src/manifest.rs`.

See also:

- [`rpc.md`](rpc.md)
- [`vite-plugin.md`](vite-plugin.md)
- [`zship.md`](zship.md)
