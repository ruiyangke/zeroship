# zero-migrate-cli

The runtime and command-line tool for zero-migrate. Install this to run
migrations authored with the [`zero-migrate`](https://www.npmjs.com/package/zero-migrate)
DSL against PostgreSQL or MySQL 8. It provides the `zero-migrate` command and a
programmatic API (`apply`, `plan`, `validate`, `status`, `history`,
`resolvePending`).

## Install

```
npm install zero-migrate-cli zero-migrate pg      # PostgreSQL
npm install zero-migrate-cli zero-migrate mysql2   # MySQL 8
```

`pg`, `mysql2`, and `tsx` are optional dependencies: install the driver for your
database. `tsx` (installed by default) lets the CLI load TypeScript migration
files directly. The matching native binary is pulled in automatically through
`zero-migrate-node`.

## CLI

```
zero-migrate new <name>            Scaffold a new migration in ./migrations
zero-migrate plan                  Offline validation of every migration (no DB)
zero-migrate preview               Print the structured operations a migration emits
zero-migrate apply                 Apply pending migrations over --database-url
zero-migrate status                Reconcile the migration set against the journal
zero-migrate history               Print the applied-migration audit trail (PostgreSQL)
zero-migrate resolve-pending <pending-version>  Finish or abort a PostgreSQL online column rename
zero-migrate --version
```

`apply`, `status`, `history`, and `resolve-pending` read `--database-url` or the
`DATABASE_URL` environment variable. All four also require at least one
operator-controlled table-shape policy file through `--policy`; there is
no embedded default. Repeat the flag to compose an ordered policy stack. The first
occurrence is the trusted root charter and bound. Each later occurrence is an
untrusted narrowing layer, and only the root may declare mandatory injects. A
later grant that exceeds the bound is rejected instead of clipped. Destructive
steps (deletes, backfills) require `--approve`.

```
DATABASE_URL=postgres://... zero-migrate apply \
  --policy ./platform-policy.toml \
  --policy ./org-policy.toml \
  --approve
```

Flag order is preserved: `platform-policy.toml` is the root/bound above, and
`org-policy.toml` narrows it. An explicit no-inject root charter is
`policy_version = 1`; save those bytes in the first file when every table column
is author-owned.

## Programmatic

```ts
import { readFile } from "node:fs/promises";
import { apply } from "zero-migrate-cli";
import * as migration from "./migrations/20260715090000_create_orders.js";

const policy = await Promise.all([
  readFile("./platform-policy.toml", "utf8"),
  readFile("./org-policy.toml", "utf8"),
]);

await apply({
  migration,
  ownerApp: "app_orders",
  projectSchema: "app_orders",
  driver: { kind: "postgres", url: process.env.DATABASE_URL! },
  policy,
  approved: false,
});
```

## Database support

PostgreSQL and MySQL 8 through the CLI and the Node API. SQLite is supported
through the Rust API only. `history` is PostgreSQL-only. Migration modules run as
ordinary JavaScript and are not sandboxed; run trusted modules only.

## Docs

See the [zero-migrate documentation](https://github.com/ruiyangke/zero-migrate/tree/main/docs).

## License

MIT
