# @zeroship/vite-plugin — Vite Environment API

## Overview

`@zeroship/vite-plugin` uses Vite's Environment API so server code in dev is
loaded through Vite's transform pipeline and evaluated inside the real
zeroship V8 runtime. The architecture is still a child-process proxy in dev:
Vite starts a zeroship runtime process, proxies server routes to it, and the
runtime fetches transformed modules back from Vite over HTTP.

The important constraint is simple: the zeroship runtime does **not** open an
outbound HotChannel/WebSocket client to Vite. The dev transport is:

- `POST /__zeroship_fetch` for ModuleRunner `fetchModule` / `getBuiltins`
- `GET /__zeroship_hmr_check` for poll-based HMR invalidation

Browser HMR remains Vite's normal browser-side WebSocket. The zeroship runtime
uses its own HTTP-only control path.

## Goals

1. Run server code in the real zeroship V8 runtime during development.
2. Keep Vite as the source of truth for transforms, module graph, and HMR.
3. Preserve the existing child-process proxy model for HTTP server routes.
4. Avoid duplicating dispatch or schema-install logic in the plugin.

## Architecture

```
Browser
  ├─ normal client assets / browser HMR ───────────────► Vite dev server
  └─ server routes (/_zs/v1/*, /api/*, /rpc, /_rpc) ──► Vite proxy middleware
                                                         │
                                                         ▼
                                                  zeroship child runtime
                                                         │
                                                         ▼
                                                dev-bootstrap ModuleRunner
                                                         │
                       POST /__zeroship_fetch ◄──────────┤
                       GET  /__zeroship_hmr_check ◄──────┘
                                                         │
                                                         ▼
                                                 Vite zeroship environment
```

### What each side owns

- Vite:
  - `"use server"` transforms and client/server code rewriting
  - the `zeroship` dev environment and `fetchModule()` implementation
  - the HTTP endpoints used by the runtime bridge
  - proxying server routes to the child runtime
- zeroship child runtime:
  - the real V8 isolate with `env.*` primitives and the virtual `zeroship` module
  - ModuleRunner evaluation of Vite-transformed server modules
  - request handling for proxied server routes

## Transport

### Module fetch

The dev bootstrap bundles `vite/module-runner` and gives it an HTTP transport.
When ModuleRunner needs a module, it sends JSON to Vite:

```http
POST /__zeroship_fetch
Content-Type: application/json
```

Vite accepts only two method names:

- `fetchModule`
- `getBuiltins`

Unknown method names are rejected. Request bodies are size-limited before they
are buffered.

### HMR

The zeroship runtime cannot use Vite's bidirectional HMR transport, so it polls:

```http
GET /__zeroship_hmr_check
```

Vite returns the list of changed server files since the last poll and clears the
pending set atomically. The dev bootstrap invalidates those module ids in
ModuleRunner's evaluated-module cache so the next import re-fetches them.

## Dev Database

When the dev server spawns the zeroship child process, it resolves
`DATABASE_URL` with explicit precedence:

1. Shell environment (`process.env.DATABASE_URL`)
2. `.env` (`DATABASE_URL=...`)
3. Default SQLite fallback: `sqlite:.zeroship/dev.sqlite`

Notes:

- The shell environment wins on purpose. It is the escape hatch for pointing
  dev at a non-default database without editing `.env`.
- The plugin no longer creates `.zeroship/` itself. The runtime's SQLite
  backend creates the parent directory when it opens the database.
- The default SQLite path is relative to the spawned runtime's cwd, which the
  plugin sets to the project root.
- `.zeroship/` is persistent local app state, not disposable boot scratch.
  Restarting Vite should reuse the same SQLite and redb files. Schema
  revalidation must treat platform-owned system columns as desired physical
  columns before diffing, otherwise a restart would incorrectly look like a
  destructive migration.

## Runtime Environment Variables

The child process receives:

- `ZEROSHIP_DEV=1`
- `ZEROSHIP_VITE_ORIGIN=http://localhost:<vite-port>`
- `ZEROSHIP_ENTRY=<absolute-path-to-server-entry>` when a server entry is known
- `DATABASE_URL=<resolved value>` per the precedence above

`ZEROSHIP_VITE_ORIGIN` is a plain HTTP origin. It is not a WebSocket URL and
does not include a path suffix.

## Main Files

```
sdks/vite-plugin/
  src/
    index.ts
    transform.ts
    environment.ts
    dev-server.ts
    constants.ts
  src/dev-bootstrap/
    index.ts
    transport.ts
    evaluator.ts
  scripts/
    build-bootstrap.ts
```

### `src/environment.ts`

Registers the `zeroship` Vite environment and customizes `fetchModule()` for
Node-compat handling. It does **not** open or manage any WebSocket transport.
The environment simply opts into `hot: true`, letting Vite provide its normal
local/no-op normalized hot channel for internal bookkeeping.

### `src/dev-server.ts`

Owns the dev bridge:

- registers `POST /__zeroship_fetch`
- registers `GET /__zeroship_hmr_check`
- spawns the zeroship child runtime after Vite is listening
- proxies server routes to the child runtime
- tracks changed server files for poll-based invalidation

### `src/dev-bootstrap/transport.ts`

Creates the ModuleRunner transport that POSTs to Vite's fetch endpoint using
the `ZEROSHIP_VITE_ORIGIN` origin.

### `src/dev-bootstrap/index.ts`

Starts the ModuleRunner, installs the framework-internal `devEntry(...)`
wrapper from `@zeroship/bootstrap/dev`, and runs the HMR polling loop.

## Request Flow

For a server route like `POST /_zs/v1/todos.add`:

1. The browser sends the request to Vite.
2. Vite's pre-middleware sees that the path is a server route and proxies it
   to the zeroship child runtime.
3. The zeroship runtime dispatches the request into V8.
4. The dev bootstrap imports the server entry through ModuleRunner.
5. If a module is missing or invalidated, ModuleRunner POSTs to
   `/__zeroship_fetch` and Vite returns transformed code.
6. The module executes inside the real zeroship runtime.
7. The request resolves back through the child runtime and Vite proxy.

## HMR Flow

For a server-file edit:

1. Vite notices a changed `.ts` / `.tsx` / `.js` / `.jsx` file.
2. The plugin records that file path in a pending set.
3. The zeroship runtime polls `GET /__zeroship_hmr_check`.
4. Vite returns the changed paths and clears the set.
5. The dev bootstrap invalidates those ids in ModuleRunner's evaluated-module
   cache.
6. The next request re-imports the changed module graph through Vite.

This is server-side HMR by invalidation and re-import, not by push.

## Production

This document covers the dev-time Environment API bridge only. Production still
uses the normal vite-plugin build path and `.zship` emission flow. The runtime
does not talk back to Vite in production.

## Non-Goals

- No runtime-side WebSocket client to Vite
- No separate HMR server for zeroship
- No extra database daemon for local dev
- No plugin-side request dispatch duplication
