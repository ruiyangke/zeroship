// op.* migration fixture — alterColumnType + alterColumnNullability +
// renameColumn + addConstraint (FK) + dropConstraint. Authored via the fluent
// table() surface. Records the byte-identical frozen wire ops as before.
import { table, t } from "@zeroship/migrate";

export const name = "ddl_alter";

export function up() {
  const orders = table("orders");
  orders.column("total").alter({ type: t.bigInt() });
  orders.column("note").alter({ nullable: false });
  orders.column("note").rename({ to: "memo", type: t.text() });
  orders.foreignKey("orders_customer_fk").add({
    columns: ["customerId"],
    references: { table: "customers", columns: ["id"] },
  });
  orders.constraint("orders_legacy_chk").drop();
}
