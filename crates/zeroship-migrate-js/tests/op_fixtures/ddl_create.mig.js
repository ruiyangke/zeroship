// op.* migration fixture — createTable + createIndex + addColumn.
// Covers the IrDefault carrier (a typed-scalar literal default + a synth `now()`).
import { createTable, createIndex, addColumn } from "@zeroship/migrate";

export const name = "ddl_create";

export function up() {
  createTable("orders", [
    { name: "id", type: "uuid", nullable: false, default: { fn: { fn: "genRandomUuid" } } },
    { name: "total", type: "int", nullable: false, default: { literal: { value: 0 } } },
    { name: "note", type: "text", nullable: true },
  ]);
  createIndex("orders", ["total"], { name: "orders_total_idx" });
  addColumn("orders", "status", "text", { nullable: false, default: { literal: { value: "new" } } });
}
