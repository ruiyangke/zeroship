# zeroship Starter Agent Contract

Use this project as the golden path for building a zeroship app locally with an AI coding agent, then deploying the built `.zship` artifact to the platform.

## Project Shape

- `vite.config.ts` uses `react()` plus `zeroship()` from `@zeroship/vite-plugin`.
- `index.html` loads `src/main.tsx`, which mounts the React client.
- `src/server.ts` is the server module. It starts with `"use server";`.
- `src/api.ts` re-exports server functions. In the browser bundle, the Vite plugin replaces those imports with HTTP-RPC stubs.
- Client code imports from `./api` and calls server functions like local async functions.

## Server Functions

Write RPC endpoints in `"use server"` modules with wrappers from `@zeroship/rpc/server`:

```ts
"use server";

import { mutation, query } from "@zeroship/rpc/server";
import { z } from "@zeroship/server";

export const listItems = query(async () => [], {
  id: "listItems",
  output: z.array(z.object({ id: z.number(), text: z.string() })),
});

export const addItem = mutation(async (input: { text: string }) => input, {
  id: "addItem",
  input: z.object({ text: z.string().min(1).max(280) }),
  output: z.object({ text: z.string() }),
});
```

Use `query`, `mutation`, `stream`, and `subscription` from `@zeroship/rpc/server` for server functions. Use explicit `id` values for deployable builds. Validate inputs and outputs with Zod via `z` from `@zeroship/server`. Plain exports that are not wrapped stay private to the server bundle.

### RPC auth — authenticated by default (IMPORTANT)

Behind the gateway, **every RPC procedure requires an authenticated end-user by default** (no auth policy ⇒ `auth: "user"`). This is fail-closed by design: forgetting to set auth yields a loud `401`, never a silent public endpoint. So a procedure called from an anonymous browser **401s** unless you opt it into anonymous access.

Declare the policy in `src/server/config.ts` (see this starter's file):

```ts
import { defineApp } from "@zeroship/server";

export default defineApp({
  resources: {
    // intentionally public — the validator requires publiclyAccessible alongside anon
    "rpc:getMessages": { auth: "anon", publiclyAccessible: true },
  },
});
```

For procedures that need a user, leave them at the default (`auth: "user"`) and read identity inside the handler with `env.auth.getUser()` / `env.auth.requireUser()`. `auth: "admin"` restricts to platform admins.

## Client Calls

Re-export server functions from `src/api.ts`:

```ts
export { listItems, addItem } from "./server";
```

Then call them from React:

```ts
const items = await listItems();
const created = await addItem({ text: "hello" });
```

The runtime-owned RPC path is `/__zeroship/v1/<wireId>`; app code does not route it manually.

## Platform Primitives

These are available server-side through zeroship SDKs:

- `env.db` / `@zeroship/db`: structured, typed `env.db.<collection>` CRUD backed by committed migrations; no raw SQL in app code.
- `env.kv` / `@zeroship/kv`: ephemeral key-value state with TTLs, counters, leases, and prefix listing.
- `env.storage` / `@zeroship/storage`: object storage for put/get/delete/list and streaming object IO.
- `env.auth` / `@zeroship/auth`: request identity; `auth.getUser()` returns a user or `null`, and `auth.requireUser()` throws a clean 401 when anonymous.

Add these SDKs only when the app needs them. This starter intentionally uses in-memory state so the first build has no database setup.

## Build

```bash
pnpm install
pnpm build
```

`pnpm build` runs `vite build`. The zeroship Vite plugin emits static assets, bundles the server RPC module, discovers wrapped RPC functions, and writes:

```text
dist/app.zship
```

## Deploy

```bash
zeroship login
zeroship deploy ./dist/app.zship --app=<id> --control=<url> --token=<PAT>
```

The deploy command ships the pre-built artifact. Keep app changes local, rebuild to produce a new `dist/app.zship`, then deploy that artifact.
