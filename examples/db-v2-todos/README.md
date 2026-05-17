# db-v2-todos

End-to-end smoke test for the `@zeroship/db` v2 surfaces. A small todo
list that exercises the Tier A/B features the v2 work shipped.

## What this demonstrates

| Feature | Shipped in commit | Example surface |
|---|---|---|
| Materialised indexes (`.unique()` / `.index()`) | A1 — `f9d057b` | `users.email.unique()`, `users.handle.unique()`, `todos.userId.index()` (implicit via `t.ref`) |
| Deploy-time data validation + audit log | A2+A3 — `6daef16` | `__zeroship_migrations` table populated by every DDL |
| `@zeroship/migrations` component | B1 — `7ba2869` | `backfillArchived` in `src/migrations.ts` |
| Typed `t.ref("users")` + Postgres FK | B2 — `eab163a` | `todos.userId: t.ref("users").required()` |
| Capability-scoped wrappers (TS+runtime) | B3 — `7b8074e` + `6df9097` | `query` / `mutation` / `action` in `src/index.ts` |
| Auto-tx wrapping per kind | T1 followup — `cc9ddb1` | `query` runs in READ ONLY; `mutation` in SERIALIZABLE |

The example deliberately uses each wrapper kind:

- `listTodos`, `getTodo`, `todoCount` — `query()`; cannot write to the DB
  (caught at TS compile + runtime via SQLSTATE 25006 if the gate is
  bypassed)
- `createTodo`, `completeTodo`, `archiveTodo`, `deleteTodo` — `mutation()`;
  cannot call `fetch()`; runs in a SERIALIZABLE transaction
- `shareToWebhook` — `action()`; can call `fetch()`; uses `ctx.runQuery`
  to read data because direct DB access isn't available in actions

## Run locally

Prereqs: a Postgres reachable by the dev runtime (the standard
zeroship-vite-plugin dev bootstrap handles this when configured).

```bash
npm install
npm run typecheck       # static verification
npm run dev             # vite-plugin bootstraps the dev runtime
```

In another shell:

```bash
npm run smoke           # runs scripts/smoke.sh
```

The smoke test exercises:
1. `createTodo` happy path (mutation wrapper)
2. FK enforcement — insert with non-existent userId fails (B2)
3. Manifest registration — wrapper kinds visible to the runtime (B3)
4. `listTodos` query (auto-tx READ ONLY)
5. `__zeroship_migrations` audit log accessible (A3)

## Migration walkthrough

```ts
import { backfillArchived } from "./src/migrations";
import { migrations } from "@zeroship/migrations";

// Dry-run — shows what would change without committing.
await migrations.run(backfillArchived, { dryRun: true });

// Real run, batched + resumable + online.
await migrations.run(backfillArchived);

// Inspect status.
const status = await migrations.status(backfillArchived);
// { name, status: "applied"|..., processed, isDone, lastError, cursor }

// Cancel a running migration (only valid in 'running' state).
await migrations.cancel(backfillArchived);
```

State lives in the per-app `__zeroship_migrations` table. The migration
resumes from the last `validate_cursor` on a fresh `run()` after crash;
`reset: true` clears state and starts over.

## What's NOT in this example

These surfaces ship in v2 but aren't exercised here (see other examples):

- C1 reactive queries / `useQuery` — see `examples/db-v2-chat/`
- `t.union()` discriminated documents
- `t.calendarDate()`, `t.object()`, `withVersioning()` — Tier D polish
- P8c admin role / HMAC session init — deployment-tier concern

Each is a single-line addition once the runtime is provisioned for it.
