# zero-migrate-cli

The runtime and command-line tool for zeroship migrations. Install this to run
migrations authored with [`@zeroship/migrate`](https://www.npmjs.com/package/@zeroship/migrate)
DSL against PostgreSQL or MySQL 8. It provides the `zero-migrate` command and a
programmatic API (`apply`, `plan`, `validate`, `status`, `history`,
`resolvePending`).

## Install

```
npm install zero-migrate-cli @zeroship/migrate pg      # PostgreSQL
npm install zero-migrate-cli @zeroship/migrate mysql2   # MySQL 8
```

`pg`, `mysql2`, and `tsx` are optional dependencies: install the driver for your
database. `tsx` (installed by default) lets the CLI load TypeScript migration
files directly. The matching native binary is pulled in automatically through
`zeroship-migrate-node`.

## CLI

```
zero-migrate new <name>            Scaffold a new migration in ./migrations
zero-migrate lint                  Offline validation for all dialects
zero-migrate plan                  Show pending SQL against a live database
zero-migrate apply                 Apply pending migrations over --database-url
zero-migrate status                Reconcile the migration set against the journal
zero-migrate history               Print the applied-migration audit trail (PostgreSQL)
zero-migrate resolve <migration> --commit|--rollback  Resolve a PostgreSQL online rename
zero-migrate --version
```

`plan`, `apply`, `status`, `history`, and `resolve` read `--database-url`, the
selected `zero-migrate.toml` environment, or `DATABASE_URL`. Live commands also require at least one
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
import * as createOrders from "./migrations/20260715090000_create_orders.js";
import * as addTotals from "./migrations/20260716090000_add_totals.js";

const policy = await Promise.all([
  readFile("./platform-policy.toml", "utf8"),
  readFile("./org-policy.toml", "utf8"),
]);

await apply({
  migrations: [createOrders, addTotals],
  ownerApp: "app_orders",
  projectSchema: "app_orders",
  driver: { kind: "postgres", url: process.env.DATABASE_URL! },
  policy,
  approved: false,
});
```

`apply` takes the ordered migration set, oldest first, and applies every
migration the journal does not already record, in order, in one call. Each
migration commits on its own, so a failure leaves the earlier ones applied and a
rerun resumes at the first one that is not. Before it runs anything, a set of two
or more is checked against the journal: the migrations it skips must agree with
the order the journal recorded them in, and nothing listed after the first
pending migration may already be in the journal. `zero-migrate apply` makes one
such call for the whole directory and prints one result line per file.

## Database support

PostgreSQL and MySQL 8 through the CLI and the Node API. The CLI also supports
SQLite plan and apply. `status`, `history`, and `resolve` remain limited to their
documented network dialects. Migration modules run as ordinary JavaScript and are
not sandboxed; run trusted modules only.

## Docs

See the [migration DSL reference](../../docs/reference/migrate-op-dsl.md).

## License

MIT
