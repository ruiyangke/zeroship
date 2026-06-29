# Building this zeroship app (agent guide)

This is a **zeroship** app: you build it locally (with an AI coding agent like
Claude Code or Codex), `pnpm build` produces a `dist/app.zship` artifact, and
`zeroship deploy` ships it to the platform, which hosts/runs/scales it. This file
tells you how the project is shaped and the contract to build against.

## Project shape

- `src/index.ts` — **server functions** (file starts with `"use server"`). These
  run on the zeroship runtime, not the browser.
- `src/App.tsx` / `src/main.tsx` — the **React client**. It imports the server
  functions and calls them like local async functions; the Vite plugin rewrites
  those imports into RPC calls.
- `migrations/` — committed `op.*` schema migrations. **This is the schema source
  of truth** — you do NOT export a schema object from app code.
- `generated/zeroship/env.db.ts` — the generated typing for `env.db`, folded from
  the migrations. `generated/zeroship/schema.runtime.json` — the runtime descriptor.
- `vite.config.ts` — `react()` + `zeroship()` from `@zeroship/vite-plugin`.

## Server functions (RPC)

Wrap exports in `query` / `mutation` / `stream` / `subscription` from
`@zeroship/rpc/server`; give each a stable `id`; validate inputs/outputs with Zod
via `z` from `@zeroship/server`. Plain (unwrapped) exports stay server-private.

```ts
"use server";
import { query, mutation } from "@zeroship/rpc/server";
import { env } from "zeroship";

export const listNotes = query(async () => {
  const r = await env.db.notes.find().sort({ id: -1 });
  if (r.error) throw r.error;
  return r.data;
}, { id: "notes.list" });
```

### RPC auth — authenticated by default (IMPORTANT)

Behind the gateway, **every RPC procedure requires an authenticated end-user by
default** (no policy ⇒ `auth: "user"`) — fail-closed, so a forgotten policy yields
a loud `401`, never a silent public endpoint. To make one public, opt in
explicitly in `src/server/config.ts`:

```ts
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "rpc:notes.list": { auth: "anon", publiclyAccessible: true },
  },
});
```

For user-scoped data, leave procedures at the default and read identity inside
the handler with `env.auth.getUser()` / `env.auth.requireUser()`.

## Platform primitives (server-side, via `import { env } from "zeroship"`)

- `env.db` — typed structured CRUD, `env.db.<collection>.find()/insert()/delete()`,
  **no raw SQL**. The types come from `migrations/` → `generated/zeroship/env.db.ts`.
  **To add/change a table: edit a migration** (op.* DSL), then regenerate the
  generated artifacts via the Vite build or `zeroship-migrate-js gen-types`.
- `@zeroship/storage` — `bucket("name").put/get/delete/list`, object storage.
- `@zeroship/kv` — ephemeral key-value: get/set with TTL, atomic counters, leases.
- `env.auth` — request identity: `getUser()` → user or `null`, `requireUser()`
  throws a clean 401 when anonymous.

## Build

```bash
pnpm install
pnpm build      # → dist/app.zship
```

`pnpm build` runs `vite build`; the zeroship plugin discovers the `"use server"`
RPC functions, folds migrations into the generated `env.db` types, bundles the
server module + static client, and writes `dist/app.zship`. If a build that
ships migrations runs the gen-types drift gate, keep `zeroship-migrate-js` on
PATH (or set `ZEROSHIP_MIGRATE_JS_BIN`).

## Deploy

```bash
zeroship login                                   # one-time (OAuth)
zeroship deploy ./dist/app.zship --app=<id>      # ship the built artifact
```

Keep changes local, rebuild to produce a new `dist/app.zship`, then deploy that.
