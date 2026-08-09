// op.* migration fixture — createTable + createIndex + addColumn. Authored via the
// fluent table() surface. Covers the IrDefault literal carrier (a typed-scalar
// default). Records the byte-identical frozen wire ops.
//
// NOTE: this confined-platform fixture omits `id`; policy injects the internal
// platform key and pins it as the primary key, so the golden's `primaryKey` is
// ["id"] while its constraints list stays EMPTY (it is the no-constraint
// createTable carrier). `note` is nullable-by-default (the fluent chain OMITS the
// nullable key for a nullable column).
import { table, t } from "zero-migrate";

export const name = "ddl_create";

export function up() {
  table("orders").create({
    columns: {
      total: t.int().notNull().default(0),
      note: t.text(),
    },
  });
  table("orders").index("orders_total_idx").add({ on: ["total"] });
  table("orders").column("status").add({ type: t.text().notNull().default("new") });
}
