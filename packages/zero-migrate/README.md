# zero-migrate

The authoring DSL for zero-migrate. Write one typed migration and target
PostgreSQL, MySQL 8, or SQLite. This package is pure JavaScript with no native
code and no runtime dependencies; it is what your migration files import.

To run migrations (apply, plan, status, the `zero-migrate` CLI), install
[`zero-migrate-cli`](https://www.npmjs.com/package/zero-migrate-cli).

## Install

```
npm install zero-migrate
```

## Write a migration

```ts
import { ids, now, table, t } from "zero-migrate";

export const name = "create_orders";

export default {
  up() {
    table("orders").create({
      columns: {
        id: ids.typeId({ prefix: "ord" }).primaryKey(),
        total: t.numeric({ precision: 12, scale: 2 }).notNull(),
        status: t.text().notNull().default("pending"),
        created_at: t.timestamp().notNull().default(now()),
      },
    });

    table("orders").index("orders_status_idx").add({ on: ["status"] });
  },
};
```

TypeID columns do not add a database default. Supply a valid value with the
matching prefix whenever you insert a row.

Calling these helpers describes the change as structured operations; it does not
connect to a database. Common features share one API, and vendor-only behavior is
declared explicitly with `dialect(...)` so unsupported targets fail with a clear
validation error instead of guessing.

## Docs

See the [zero-migrate documentation](https://github.com/ruiyangke/zero-migrate/tree/main/docs).

## License

MIT
