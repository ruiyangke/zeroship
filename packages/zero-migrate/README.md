# `@zeroship/migrate`

The one authoring DSL and recorder for zeroship migrations. Write one typed migration and target
PostgreSQL, MySQL 8, or SQLite. This package is pure JavaScript with no native
code and no runtime dependencies; it is what your migration files import.

To run migrations (apply, plan, status, the `zero-migrate` CLI), install
[`zero-migrate-cli`](https://www.npmjs.com/package/zero-migrate-cli).

## Install

```
npm install @zeroship/migrate
```

## Write a migration

```ts
import { now, table, t } from "@zeroship/migrate";

export default {
  name: "create_orders",
  schema() {
    table("orders").create({
      columns: {
        id: t.typedId("ord").primaryKey(),
        total: t.numeric({ precision: 12, scale: 2 }).required(),
        status: t.text().required().default("pending"),
        created_at: t.timestamp().required().default(now()),
      },
    });

    table("orders").index("orders_status_idx").add({ on: ["status"] });
  },
};
```

Schema and data changes are separate migration modules. A schema migration
exports `schema()` and receives an engine-synthesized structural inverse. A data
migration exports `data()` and must make its rollback posture explicit with
either a recorded `inverse()` or a non-empty `irreversible` reason:

```ts
import { table } from "@zeroship/migrate";

export default {
  name: "normalize_order_status",
  data() {
    table("orders").update({
      set: { status: "pending" },
      where: (col) => col("status").eq("new"),
    });
  },
  inverse() {
    table("orders").update({
      set: { status: "new" },
      where: (col) => col("status").eq("pending"),
    });
  },
};
```

A module exports exactly one forward phase: `schema()` or `data()`. There is no
`up()` compatibility alias and no authored `down()` surface.

TypeID columns do not add a database default. Supply a valid value with the
matching prefix whenever you insert a row.

Calling these helpers describes the change as structured operations; it does not
connect to a database. Common features share one API, and vendor-only behavior is
declared explicitly with `dialect(...)` so unsupported targets fail with a clear
validation error instead of guessing.

## Docs

See the [migration DSL reference](../../docs/reference/migrate-op-dsl.md).

## License

MIT
