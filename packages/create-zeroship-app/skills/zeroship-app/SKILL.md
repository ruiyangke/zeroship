---
name: zeroship-app
description: Use when building or modifying a zeroship app - the project shape, which code runs on the server vs in the browser, the env.* platform primitives, and which other zeroship skill to load next. Load this first when you see zeroship.jsonc or a "use server" module in the project.
---

# Building a zeroship app

A zeroship app is built locally and deployed as a single artifact. You write
server functions and a client; `pnpm build` produces `dist/app.zship`;
`zeroship deploy` ships it to the platform, which hosts and scales it.

## Project shape

| Path | What it is |
| --- | --- |
| `src/index.ts` | Server functions. The file's first statement is `"use server";`. Runs on the zeroship runtime, never in the browser. |
| `src/App.tsx`, `src/main.tsx` | The React client. Imports server functions and calls them like local async functions. |
| `migrations/` | Committed `.ts` schema migrations. The schema source of truth. |
| `generated/zeroship/` | Generated and committed: `env.db.ts` (typing) and `schema.runtime.json` (runtime descriptor). Do not hand-edit. |
| `zeroship.jsonc` | Deploy target and paths. Read by the build, the dev server and the CLI. |
| `vite.config.ts` | `react()` plus `zeroship()` from `@zeroship/vite-plugin`. |

## The one rule that decides where code runs

A module enters RPC discovery only when its first statement is `"use server";`.
Drop that directive and the "server" functions get bundled into the CLIENT and
run in the browser, where `env.*` does not exist. Nothing warns you at runtime;
the symptom is a client bundle that suddenly contains your database code.

The client imports server functions directly:

```ts
import { listNotes } from "./index";
const notes = await listNotes({});
```

The Vite plugin rewrites that import into an RPC call. You do not write fetch
calls or URL paths by hand.

## Platform primitives

Server-side only, via `import { env } from "zeroship"`:

- `env.db` - typed structured CRUD. No raw SQL. See the `zeroship-data` skill.
- `env.auth` - `getUser()` returns the user or `null`; `requireUser()` throws a
  clean 401 when anonymous.
- `env.kv` - ephemeral key-value: get/set with TTL, atomic counters, leases.
- `env.storage` - object storage: put/get/delete/list.
- `env.workflows` - durable workflow runs.

There is no `env.meter`. Usage is measured by the platform so app code can
neither forge nor suppress it.

Anything reachable with `fetch` or composition belongs in a normal npm package,
not a platform primitive. Use the ecosystem for email, payments, and AI calls.

## Which skill next

- Adding or changing a server function, or a deployed call returns 401:
  `zeroship-rpc`.
- Changing the schema, or writing `env.db` queries: `zeroship-data`.
- Building, deploying, or a deployed app fails its first database call:
  `zeroship-deploy`.
