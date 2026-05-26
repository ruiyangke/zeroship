# db-todos

End-to-end smoke test for the `@zeroship/db` v2 surfaces. A small todo
list that exercises the Tier A/B features the v2 work shipped.

## What this demonstrates

| Feature | Shipped in commit | Example surface |
|---|---|---|
| Materialised indexes (`.unique()` / `.index()`) | A1 — `f9d057b` | `users.email.unique()`, `users.handle.unique()`, `todos.userId.index()` (implicit via `t.ref`) |
| Deploy-time data validation + audit log | A2+A3 — `6daef16` | `__zeroship_migrations` table populated by every DDL |
| Typed `t.ref("users")` + Postgres FK | B2 — `eab163a` | `todos.userId: t.ref("users").required()` |
| Capability-scoped wrappers (TS+runtime) | B3 — `7b8074e` + `6df9097` | `query` / `mutation` / `action` in `src/index.ts` |

The example deliberately uses each wrapper kind:

- `listTodos`, `getTodo`, `todoCount` — `query()`; cannot write to the DB
  (caught at TS compile + runtime capability gates)
- `createTodo`, `completeTodo`, `archiveTodo`, `deleteTodo` — `mutation()`;
  cannot call `fetch()`
- `shareToWebhook` — `action()`; can call `fetch()`; uses `ctx.runQuery`
  to read data because direct DB access isn't available in actions

Handlers currently run without an implicit transaction: individual DB
operations autocommit. Use explicit `db.transaction()` when multiple
operations must commit or roll back as a unit.

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
4. `listTodos` query
5. Schema audit endpoint accessible (A3)

## What's NOT in this example

These surfaces ship in v2 but aren't exercised here (see other examples):

- C1 reactive queries / `useQuery` — see `examples/db-chat/`
- `@zeroship/migrations` data backfills — see `examples/db-migrations-playground/`
- `t.union()` discriminated documents
- `t.calendarDate()`, `t.object()`, `withVersioning()` — Tier D polish
- P8c admin role / HMAC session init — deployment-tier concern

Each is a single-line addition once the runtime is provisioned for it.
