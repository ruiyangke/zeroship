# @zeroship/vite-plugin — Vite Environment API

When you run `pnpm dev`, your server code is not run against a mock: it is
evaluated inside the same zeroship V8 runtime that runs it in production. The
`zeroship` module, the `env.*` namespaces, and the request handler behave the
way they behave in a deployed app. What differs is only who feeds the runtime —
Vite transforms and serves your source, a dev database stands in for the
deployed one, and a same-origin auth surface stands in for the gateway.

This page is the contract for what app code can reach and rely on in that
environment: the surfaces it can call, the imports it can resolve, the defaults
and limits in force, and the failure it sees when it reaches for something that
is not there. It states behavior, not wiring.

## Overview

Everything your server code touches in dev is either the runtime kernel or
bundled code, exactly as in production:

- `import { env } from "zeroship"` resolves to the runtime's own module in dev
  and in the built artifact — you never configure this import.
- `env.*` is the same small native-primitive kernel (`db`, `kv`, `storage`,
  `auth`, `workflows`), plus your app's variables and secrets.
- Server dependencies are bundled into your code; the runtime loads no modules
  from the host. See [Import resolution](#import-resolution).
- Dev supplies a local SQLite database and a same-origin auth surface; neither
  exists in a deploy.

The next sections go from the surfaces you call to the edges: what resolves,
what fails, and how dev differs from production.

## The `zeroship` module

`zeroship` is the reserved module you import to reach the runtime kernel:

```ts
import { env } from "zeroship";
```

Its named exports are the whole surface. They read host state directly, so
reassigning a JavaScript global does not change what they return.

| Export | What it is |
| --- | --- |
| `env` | The frozen environment object ([below](#the-env-object)). |
| `waitUntil(promise)` | Registers work to finish before the current request completes. Throws a `TypeError` if the argument is not a Promise. |
| `getRequest()` | The current `Request` object. Throws `getRequest called outside a fetch handler (RPC fast-path has no Request)` when there is none. |
| `getRequestContext()` | The current request context object, or `undefined` outside a handler. |
| `runQuery(proc, input)` / `runMutation(proc, input)` | Calls a procedure under an explicit kind and returns a Promise. The first argument must be a function (otherwise the promise rejects with a `TypeError`). |
| `currentUser` | The authenticated identity of the current request. |
| `currentRequestId` `currentTraceId` `currentSignal` `currentHeaders` `currentIdempotencyKey` | Identity and context of the current request. Each throws `"<name>: called outside a request handler"` when no request is active. |

`zeroship` has no subpaths. The specifiers `zeroship`, `zeroship.js`, and
anything under the `zeroship:` prefix are host-owned: your app must not supply
modules under those names, in any spelling.

## The `env` object

`env` is one object reachable from three equal places: the `env` export of
`zeroship`, the second argument of `fetch(request, env, ctx)`, and
`globalThis.env.get(name)`.

It holds two kinds of members:

- **Native namespaces** — `env.db`, `env.kv`, `env.storage`, `env.auth`,
  `env.workflows`. Each is a capability handle; its method surface is its own
  reference page ([db](db.md), [kv](kv.md), [storage](storage.md),
  [auth](auth.md), [workflows](workflows.md)).
- **Scalars** — your app's variables and secrets, by name.

The rules are simple and enforced:

- **`env` is frozen.** You cannot add a member, and you cannot reassign one
  (`env.db = null` is a no-op or an error, never the new value).
- **A namespace beats a scalar of the same name.**
- **A secret beats a variable of the same name** on `env` and
  `globalThis.env.get`.
- **`process.env` carries variables plus only the secrets you exposed.** See
  [env-vars.md](env-vars.md) for the full read model.

On the local dev tier, scalars come from `.env` names carrying the `ZS_VAR_`
prefix (stripped on the way in), and the shell wins over `.env` on a shared
name — the same rule `zeroship serve` applies. Deliver a value through
`ZS_VAR_`, or set it as an app variable or secret, when you need it on both
tiers.

## Import resolution

The runtime has no `node_modules`. Every module your server code imports is
bundled into the app, and only the kernel modules are supplied by the host at
evaluation time. That single fact decides everything below.

**Resolvable:**

- `zeroship` — the kernel module ([above](#the-zeroship-module)).
- `node:*` built-ins — a supported set. The runtime owns `node:async_hooks`,
  `node:buffer`, `node:crypto`, `node:events`, `node:path`, `node:util`,
  `node:zlib`, and `node:os` (plus `node:net` and `node:tls` where the plan
  allows); the rest are polyfilled at build time. Bare `Buffer`, `process`,
  `global`, `setImmediate`, and `clearImmediate` resolve to the same
  implementations. See [node-compat.md](node-compat.md).
- npm packages and your own modules — bundled server-side. A package may carry
  a `zeroship` or `worker` export condition; import conditions are resolved in
  the order `zeroship`, `worker`, `module`, `import`, `default`.

**Not resolvable, and the failure:**

- **Host modules.** Anything that is neither bundled nor a kernel module.
  In dev the failure is explicit:

  ```
  [zeroship] Cannot import external module "<specifier>". Configure Vite to
  bundle this dependency or provide a runtime module adapter.
  ```

  The deployed artifact answers differently (the module is simply absent from
  the bundle), but the rule is the same: if it does not bundle, it does not
  exist at runtime.
- **Reserved specifiers.** `zeroship`, `zeroship.js`, and the `zeroship:`
  prefix are host-provided; creator artifacts must not supply them.
- **`zeroship` subpaths.** Only the bare specifier exists.

Server code compiles to ES2024 as its output target, so you can use syntax up
to that level without a separate downlevel step.

## Dev Database

`DATABASE_URL` resolves with explicit precedence:

1. The shell environment.
2. `.env`.
3. The project default: `sqlite:.zeroship/dev.sqlite`.

The dev tier accepts only the `sqlite:<path>` form. A non-SQLite value — for
example a Postgres DSN exported for a production tool — is refused with an
error naming the scheme and the source, rather than being silently misapplied
to the SQLite file.

`.zeroship/` is persistent local app state (the SQLite file and the KV store
live there), not disposable boot scratch. Restarting `pnpm dev` reuses it.

Migrations are **not** applied by the dev server. Apply them with the separate
`pnpm migrate` step; `pnpm dev` reports whether the declared collections exist
and names the command when they do not, but it never mutates the database as a
side effect of starting.

## Runtime Environment Variables

The names below are what the dev tooling supplies to the runtime. Your app code
does not read most of them; it reads the environment through `env`,
`process.env`, and `globalThis.env.get` as described in
[env-vars.md](env-vars.md).

- `ZEROSHIP_DEV=1`, `ZEROSHIP_VITE_ORIGIN`, `ZEROSHIP_ENTRY` — runtime plumbing
  (dev flag, the Vite origin the runtime fetches modules from, the server
  entry). Not app input.
- `DATABASE_URL` — selects the local database ([above](#dev-database)); it
  configures the database, it is not a value to read.
- `APP_ID` — the dev app identifier.
- `ZEROSHIP_DIE_WITH_PARENT` — internal reaping so the runtime dies with the
  dev server.

Local-only knobs your setup may set (see [env-vars.md](env-vars.md)):
`ZEROSHIP_HEAP_LIMIT_MB`, `ZEROSHIP_DEV_PORT`, `ZEROSHIP_STORAGE_URL`,
`ZEROSHIP_KV_PATH`, `ZEROSHIP_KV_CONFIG_FILE`.

## Request Flow

A server call under `pnpm dev` flows like production, with dev-only edges:

- The browser talks to Vite on its own port (`5173` by default).
- Server routes are forwarded to the dev runtime on a **separate** port
  (`devServerPort`, default `3001`): `/__zeroship/v1/*`, `/api/*`, `/rpc`, and
  `/_rpc`. `vite --port` does not move the runtime.
- `/__zeroship/auth/*` is served by the dev auth surface when dev auth is
  enabled; with dev auth disabled it is forwarded to your app, so the paths stay
  yours to own.

When the runtime cannot bind its port or its state dir is locked by an earlier
run, server calls answer **`503`** with an error envelope whose `code` is
`UNAVAILABLE`, and the dev server prints a banner naming the cause. A port clash
is fixed by setting `devServerPort`; a state-dir lock (`.zeroship/` held by a
stale run) is not fixed by changing the port.

Dev is looser than deployed: no wall timeout and no CPU limit apply unless you
set them, and the request-body cap is 1 MiB in dev versus 4 MiB deployed. See
[runtime-limits.md](runtime-limits.md).

## HMR Flow

- **Client (browser) assets** use ordinary Vite HMR.
- **Server code** in a `"use server"` module is not hot-swapped mid-call. After
  you save, the next request serves the new code — the dev server rebuilds and
  restarts the runtime as needed, and a request already in flight is not
  interrupted. You do not restart `pnpm dev` by hand.
- **Structural changes** — the server entry, migration files, or the generated
  schema — replace the running runtime rather than patch it. The dev server
  handles that for you.

## Production

None of the dev plumbing exists in production. `pnpm build` emits the `.zship`,
and the deployed runtime does not fetch transforms from Vite. The kernel is
identical: the `zeroship` module, `env.*`, and the import rules above are the
same contract in both tiers. What differs is exactly the parts this page marks
as dev-only — the SQLite database, the dev auth surface, the env injection, and
the looser limits.

## Non-Goals

- The dev server does not apply migrations; that is `pnpm migrate`.
- There is no outbound WebSocket from server code: WebSocket is server-side only
  (`WebSocketPair`). Dev therefore drives module loading over plain HTTP, not
  Vite's browser WebSocket.
- No separate HMR server and no separate database daemon for local dev.
- There is no `env.meter`: usage is measured server-side, so app code can
  neither forge nor suppress it.

## See also

- [vite-plugin.md](vite-plugin.md) — the plugin's options, discovery, and build.
- [zeroship-standard.md](zeroship-standard.md) — the deploy contract and the
  `zeroship` module.
- [env-vars.md](env-vars.md) — how app code reads variables and secrets.
- [node-compat.md](node-compat.md) — the supported `node:*` surface.
- [runtime-limits.md](runtime-limits.md) — the CPU, timeout, heap, and body caps.