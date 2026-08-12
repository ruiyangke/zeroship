# issue-tracker

A Bugzilla-faithful issue tracker built as a zeroship example app. It keeps
Bugzilla's domain model and vocabulary: products and components, bugs with
separate status and resolution fields, comments, attachments, dependencies,
duplicates, keywords, flags, CC lists, voting, watching, cross-tracker
see-also links, saved searches, notifications, and activity history.

The example is deliberately migration-first. The committed migration in
`migrations/` is the schema source of truth; the build folds it into the typed
`env.db` runtime descriptor. There is no inline `dbSchema` or server-module
schema export.

The server exposes **82 explicitly named RPC procedures**, each with an auth
policy in `src/server/config.ts`. Nine are anonymous — bug browsing, comment
reading, product listing and the reports — and every one of those nine is a
read. Writes, administration, personal searches and notification data require
an authenticated user. Field-changing mutations record Bugzilla-style activity
rows, and dependency and duplicate operations reject cycles.

## Access control

Three mechanisms, all enforced on reads *and* writes:

- **Product groups** (`products.restrict`) hide a whole product.
- **Bug groups** (`bugs.restrict`) are Bugzilla's `bug_group_map`: one
  confidential bug inside an otherwise readable product.
- **Private comments** are withheld from everyone but their author.

Restrictions apply to searches, reports and dependency graphs, not only to the
detail route — a count or a graph node is a disclosure too. A user who cannot
read a bug also cannot comment on it, resolve it or touch its attachments.

The **first account to exist becomes an admin**, the way Bugzilla's installer
creates one. Nothing else sets `isAdmin`, so without that bootstrap the whole
admin surface would be unreachable.

## Known limits

The platform's present DB update path cannot bind SQL `NULL` for nullable
foreign keys or numeric columns, so RPCs reject an actual clear of those values
with a conflict error rather than corrupting the relation. Clearing a timestamp
stores an empty string rather than `NULL`, which is why `reports.timeToResolve`
filters on `typeof === "number"`.

`attachments.delete` checks bug access but not uploader identity, so any user
who can edit a bug can delete another user's attachment. See SPEC.md's
"Divergences from Bugzilla" for the full list of deliberate departures.

## Run locally

```bash
pnpm install
pnpm migrate
pnpm dev
```

`pnpm migrate` is a separate, required step; `pnpm dev` does **not** apply
committed migrations, and a fresh database with no migrations answers every RPC
with `no such table`. The dev runtime is SQLite-only — `DATABASE_URL` is read
but the dev migrator targets SQLite regardless, so the dev tier cannot be
pointed at Postgres.

The Vite client is pinned to port `5183` (`strictPort`, so a clash fails the
boot instead of silently moving the app). The zeroship dev RPC runtime is a
separate port, `3007`; override with `ISSUE_TRACKER_WEB_PORT` and
`ISSUE_TRACKER_API_PORT`.

## Test

Four layers, because each one catches what the layer below cannot:

```bash
pnpm typecheck
pnpm test        # 72 unit tests, no database
pnpm smoke       # 58 checks against a running `pnpm dev`
pnpm test:e2e    # 5 Chromium specs via Playwright
```

- **Unit** covers the pure parsers and transition helpers, plus two structural
  gates: every server procedure must be re-exported from `src/api.ts` (a
  procedure missing there is invisible to the browser while every other signal
  stays green), and SPEC.md's RPC surface must match the implementation in both
  directions.
- **Smoke** drives the real runtime with a signed dev session over HTTP. It
  runs **two identities**, so access control is verified as an actual denial —
  each paired with a control proving the second user could see the thing before
  it was restricted.
- **Playwright** drives the rendered UI, which is the only layer that catches
  client/server seam defects: it found a form offering "unspecified" for a
  NOT NULL column, which composed a bug that could not be stored.

`pnpm build` folds the committed migrations, refreshes
`generated/zeroship/env.db.ts` and `schema.runtime.json`, bundles the client and
server procedures, and emits `dist/app.zship`.

## Deploy

```bash
zeroship deploy ./dist/app.zship \
  --app=<app-id> \
  --control=<control-url> \
  --token=<PAT>
```

Deployment consumes the `.zship`; it does not rebuild source on the server.

**On obtaining that PAT.** `zeroship login` runs an OAuth device flow, and on a
platform-provider deployment it returns `unsupported_provider`: control's
`ensure_platform_device_provider` requires both a Supabase URL and a platform
issuer, which only the dual-issuer provider satisfies, while `--auth-provider`
defaults to `platform` (`crates/control/src/device_handlers.rs`,
`crates/control/src/main.rs`). Until that is fixed, a PAT has to come from a
Supabase-configured control plane or be minted out of band — which is what
`tests/golden_path.sh` does, by signing one with the control signing key.

The app's own migrations are applied separately by `zeroship-migrated`;
`dev-provision` alone registers the app without creating its schema or
per-app role, and the first `env.db` call then fails with
`role "app_<id>_role" does not exist`.
