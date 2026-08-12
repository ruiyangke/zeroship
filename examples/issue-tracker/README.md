# issue-tracker

A Bugzilla-faithful issue tracker built as a zeroship example app. It keeps
Bugzilla's domain model and vocabulary: products and components, bugs with
separate status and resolution fields, comments, attachments, dependencies,
duplicates, keywords, flags, CC lists, saved searches, notifications, and
activity history.

The example is deliberately migration-first. The committed migration in
`migrations/` is the schema source of truth; the build folds it into the typed
`env.db` runtime descriptor. There is no inline `dbSchema` or server-module
schema export.

The server exposes 64 explicitly named RPC procedures. Public bug browsing and
reports are anonymous; writes, administrative operations, personal searches,
and notification data require an authenticated user. Field-changing mutations
record Bugzilla-style activity rows, and dependency/duplicate operations reject
cycles.

The app provisions an app-local profile from the authenticated identity on the
first write. Product-structure procedures are authenticated, as required by the
example contract; the current schema does not define a separate product-editor
role. The platform's present DB update path cannot bind SQL `NULL` for nullable
foreign keys or numeric columns, so RPCs reject an actual clear of those values
with a conflict error instead of corrupting the relation or surfacing a backend
cast failure.

## Run locally

From this directory, install workspace dependencies and apply the migration to
the project-local dev database before starting Vite:

```bash
pnpm install
pnpm migrate
pnpm dev
```

`pnpm migrate` is a separate, required step; `pnpm dev` does not apply committed
migrations. The local dev runtime uses SQLite and enables the built-in dev-auth
identity by default. Production auth policy still comes from
`src/server/config.ts`: only the nine procedures explicitly marked anonymous
are public behind the gateway.

The Vite app normally opens on its standard client port. Its zeroship dev RPC
runtime defaults to port `3007`; set `ISSUE_TRACKER_API_PORT` to override it.

## Test and build

```bash
pnpm test
pnpm typecheck
pnpm build
```

The unit tests exercise the pure parsers and transition/invariant helpers and
need no database. `pnpm build` folds the committed migrations, refreshes
`generated/zeroship/env.db.ts` and `schema.runtime.json`, bundles the client and
server procedures, and emits `dist/app.zship`.

## Deploy

Deploy the already-built artifact to a pre-created zeroship app:

```bash
zeroship login --control=<control-url>
zeroship deploy ./dist/app.zship \
  --app=<app-id> \
  --control=<control-url>
```

For automation, pass `--token=<PAT>` or set `ZEROSHIP_TOKEN` instead of using a
cached login. Deployment consumes the `.zship`; it does not rebuild source on
the server. The bundled migration descriptor installs the app schema before the
worker serves its RPC procedures.
