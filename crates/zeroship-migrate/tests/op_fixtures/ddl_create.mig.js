// op.* migration fixture — createTable + createIndex + addColumn. Authored via the
// fluent table() surface. Covers the IrDefault carrier (a typed-scalar literal
// default + a synth `genRandomUuid()`). Records the byte-identical frozen wire ops.
//
// NOTE: `id` is a bare uuid (NOT t.id()) — t.id() would hoist a table-level pk
// constraint, but this fixture's golden has an EMPTY constraints list (it is the
// no-constraint createTable carrier). `note` is nullable-by-default (the fluent
// chain OMITS the nullable key for a nullable column).
import { table, t } from "@zeroship/migrate";

export const name = "ddl_create";

export function up() {
  table("orders").create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      total: t.int().notNull().default(0),
      note: t.text(),
    },
  });
  table("orders").index("orders_total_idx").add({ on: ["total"] });
  table("orders").column("status").add({ type: t.text().notNull().default("new") });
}
