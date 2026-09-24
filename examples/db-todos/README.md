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
| Typed `env.db` | Local `Db<typeof dbSchema>` cast; new apps use generated `generated/zeroship/env.db.ts` |
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

The Vite plugin defaults the dev database to a project-local SQLite file.
No local Postgres is required for the default path.
`.zeroship/` is persistent dev runtime state and should survive restarts.

```bash
pnpm install
pnpm typecheck
pnpm migrate
pnpm dev
```

## Tests

The example owns its tests and service fixtures. No running development server
or shared database is needed:

```bash
pnpm test
```

Vitest runs the React tests and TypeScript acceptance tests. The acceptance
fixture builds the SDKs and Rust services, copies this app into its own workspace
directory, and applies the app's migrations. It owns a SQLite file for development
and a PostgreSQL testcontainer for the deployed app, plus the gateway, control
plane, worker, migration service, and CDC relay. Chromium exercises both targets.

The deployed tier is stood up the way a creator stands one up: a database is
created through the control plane and converged by the cluster reconciler, its
migrations are applied **with no app deployed and no binding in existence**, and
only then is the app created, refused a deploy for want of a binding, bound, and
deployed.

The tests cover CRUD, constraints, relations, pagination, transaction isolation
and overlapping requests, webhook composition, generated RPC calls, and live
snapshots between browser tabs. The backend comparison retains the named
divergences documented in `docs/reference/sqlite-divergences.md`.

Use the repository's Rust and Node toolchains, Docker, and Chromium. Install the
browser with `pnpm exec playwright install chromium` if it is not available in
the development environment. `PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH` selects an
explicit executable. Run `pnpm test --project unit` for React tests, or
`pnpm test:e2e` for acceptance tests. Missing services and migration failures fail
the acceptance suite. Logs and screenshots remain under `tests/.artifacts/`;
the fixture removes its own processes, containers, and app copy.

## What is not the focus

- Multi-step business transactions. Use `db.transaction()` when multiple DB
  operations must commit or roll back together.
- Authenticated per-user data. This demo uses a shared public ledger user so
  every browser window sees the same live list.
- Raw file or binary RPC responses. Use stream procedures or normal fetch
  routes for those.
