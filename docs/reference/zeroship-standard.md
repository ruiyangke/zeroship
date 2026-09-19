# zeroship standard

This page states the deploy contract: the module and export conventions an app
entry must satisfy, the names the platform reserves, and where the schema your
`env.db` sees comes from.

Module specifiers `zeroship`, `zeroship.js` and the `zeroship:` prefix are
reserved for the platform. A creator artifact must not supply an entry under
these names, including their `./` spellings. Import `zeroship` to reach the
runtime's exports.

The `zeroship` module exposes the request environment, the request context, and
the runtime's composition helpers:

- `env` — the per-request environment (your variables, secrets, and the `db`,
  `kv`, `storage`, `auth` and `workflows` namespaces).
- `waitUntil` — schedules a promise the request keeps alive until it settles.
- `getRequest` — the current `Request`, available only inside a `fetch` handler.
- `getRequestContext` — the current RPC context.
- `runQuery` / `runMutation` — invoke a supplied procedure under the requested
  kind (see below).
- `currentUser`, `currentRequestId`, `currentTraceId`, `currentSignal`,
  `currentHeaders`, `currentIdempotencyKey` — per-request context accessors.

`runQuery` and `runMutation` return a promise and call the supplied procedure
with its input under the requested kind. The returned promise resolves to the
procedure's return value and rejects with its original error.

## Default export

The creator-facing shape is:

```ts
export default {
  fetch?,    // request handler
  rpc?,      // RPC procedure object keyed by wire id
}
```

- `fetch` is the WinterCG-style request handler.
- `rpc` is a dictionary whose own string keys are wire IDs. A value is either a
  callable procedure or a `{ load: () => Promise<Procedure> }` record whose
  `load` returns the procedure. A lazy procedure is loaded once; the result, or
  the failure, is reused for the rest of that deploy.

The default export is not a schema contract. The runtime does not read or honor
a `schema` property on it.

## RPC authoring

`@zeroship/rpc/server` provides the wrapper helpers (`procedure`, `query`,
`mutation`, `action`, `stream`, `subscription`) — see [`rpc.md`](./rpc.md) for
the canonical list and signatures.

Exported procedures are published under the runtime-owned dispatch path
`/__zeroship/v1/<wireId>`. Your code does not route that path manually.

Production RPC resources require explicit wire IDs:

```ts
export const listTodos = query(handler, { id: "todos.list" });
```

Development accepts export-name IDs; deploy builds reject implicit ones.

`subscription` is recognized by discovery, but the public `@zeroship/rpc/client`
surface does not expose subscriptions yet — use `stream(...)` for shipped live
feeds.

## Reserved paths

The `/__zeroship/*` URL prefix is platform-reserved — user code does not route
it. Paths under it today:

- `/__zeroship/v1/<wireId>` — runtime-owned RPC dispatch (see RPC authoring
  above).
- `GET /__zeroship/health` (alias `GET /__zeroship/healthz`) — the `zeroship
  serve` (single-tenant dev) liveness probe, answered without running your code.
  The bare `GET /health` route is **not** reserved: it reaches your app's own
  handler.

## Local env injection (`zeroship serve`)

Under `zeroship serve` there is no control plane to supply per-app vars/secrets,
so the app-facing `env` (the `zeroship` module's `env` import and `fetch`'s 2nd
argument) is seeded from process-env vars carrying the `ZS_VAR_` prefix, with
the prefix stripped: `ZS_VAR_API_KEY=xyz zeroship serve app.js` makes
`env.API_KEY === "xyz"`. Only prefixed vars cross into `env`; every other host
var stays in `process.env` exclusively, so the host environment is never handed
to app code wholesale.

## Schema source of truth

The committed `op.*` migration set is the schema source of truth. The build
folds each database's migrations into two generated artifacts under that
database's own output directory:

- `env.db.ts` — the app's typed `Env.db` augmentation.
- `schema.runtime.json` — the v2 `RuntimeSchemaDescriptor` carried into the
  `.zship` manifest as one `runtime_descriptor` entry, beside the database's
  label and id.

The descriptor is version 2, and a deploy that carries any other version is
refused rather than upgraded.

The descriptor is the only source of collection schema. The runtime does not
introspect the database to discover a collection: a collection the descriptor
does not declare is refused with `collection_not_declared`, however the
underlying table came to exist. A deploy that creates tables by raw SQL and
ships no descriptor therefore has no `env.db` access to them. If you are
hand-writing a deploy and want `env.db`, ship the descriptor the build
generates.

See also:

- [`rpc.md`](rpc.md)
- [`vite-plugin.md`](vite-plugin.md)
- [`zship.md`](zship.md)