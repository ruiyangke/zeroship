// op.* migration fixture — setColumnType + set/drop not-null + set/drop default
// + renameColumn + addConstraint (FK) + dropConstraint. Authored via the fluent
// table() surface. Records the byte-identical frozen wire ops as before.
import { table, t } from "zero-migrate";

export const name = "ddl_alter";

export function schema() {
  const orders = table("orders");
  orders.column("total").setType({ to: t.bigInt() });
  orders.column("note").setNotNull();
  orders.column("note").dropNotNull();
  orders.column("note").setDefault("memo");
  orders.column("note").dropDefault();
  orders.column("note").rename({ to: "memo", type: t.text() });
  orders.foreignKey("orders_customer_fk").add({
    columns: ["customerId"],
    references: { table: "customers", columns: ["id"] },
  });
  orders.constraint("orders_legacy_chk").drop();
}
