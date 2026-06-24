// op.* migration fixture — dropIndex + dropColumn + dropTable.
import { dropIndex, dropColumn, dropTable } from "@zeroship/migrate";

export const name = "ddl_drop";

export function up() {
  dropIndex("orders_total_idx", { table: "orders", unique: false });
  dropColumn("orders", "memo", { ifExists: true });
  dropTable("scratch", { ifExists: true, cascade: true });
}
