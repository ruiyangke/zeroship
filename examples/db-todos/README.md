# db-todos

End-to-end Vite + React demo for the current `@zeroship/db` and
`@zeroship/rpc` surfaces. It is intentionally small, but it exercises the
state paths that matter for a real app: typed schema discovery, typed
`env.db`, relations, explicit RPC IDs, generated direct client calls, live
snapshots, and optimistic UI.

## What this demonstrates

| Area | Example surface |
| --- | --- |
| Schema discovery | `export default { schema: dbSchema }` in `src/index.ts` |
| Typed `env.db` | `@zeroship/db/env` + `zeroship-schema` path alias in `tsconfig.json` |
| System fields | Rows include `id`, timestamps, actor fields, `version`, and `deleted_at` automatically |
| Relations | `todos.userId: t.ref("users").required()` in `src/schema.ts` |
| RPC wrappers | `query`, `mutation`, `action`, and `stream` from `@zeroship/rpc/server` |
| Explicit wire IDs | Dotted IDs such as `todos.list`, `todos.create`, `todos.subscribe`, `users.public` |
| Generated client calls | React imports server procedures directly; no `clientProcedure(...)` wrappers |
| Live data | `subscribeTodos` wraps `db.live(...)` and yields snapshot frames |
| Stable feed ordering | List and live snapshots sort by `id: -1`, not `created_at`, to avoid timestamp ties |
| State management | React Query owns the initial public user request; the live feed owns todo snapshots |

## Procedure map

Queries:

- `todos.list`
- `todos.listPage`
- `todos.get`
- `todos.count`
- `users.getPair`
- `todos.listWithUser`

Stream:

- `todos.subscribe`

Mutations:

- `todos.create`
- `todos.setDone`
- `todos.archive`
- `todos.delete`
- `users.seed`
- `users.public`

Action:

- `todos.shareToWebhook`

`publicUser` is a mutation because it may create the shared demo user. The
client calls it through React Query with a stable key so React Strict Mode does
not create duplicate network traffic.

## Run locally

The Vite plugin defaults the dev database to project-local SQLite at
`.zeroship/dev.sqlite`. No local Postgres is required for the default path.
`.zeroship/` is persistent dev runtime state and should survive restarts.

```bash
pnpm install
pnpm typecheck
pnpm dev
```

In another shell:

```bash
pnpm smoke
```

The smoke test covers create/list/update/delete flows, FK enforcement, wrapper
registration, `db.live` snapshot delivery, and the generated RPC path.

## What is not the focus

- Multi-step business transactions. Use `db.transaction()` when multiple DB
  operations must commit or roll back together.
- Durable data migrations. See `examples/db-migrations-playground/`.
- Authenticated per-user data. This demo uses a shared public ledger user so
  every browser window sees the same live list.
- Raw file or binary RPC responses. Use stream procedures or normal fetch
  routes for those.
