// op.* migration fixture — alterColumnType + alterColumnNullability +
// renameColumn + addConstraint (FK) + dropConstraint.
import {
  alterColumnType,
  alterColumnNullability,
  renameColumn,
  addConstraint,
  dropConstraint,
} from "@zeroship/migrate";

export const name = "ddl_alter";

export function up() {
  alterColumnType("orders", "total", "bigInt");
  alterColumnNullability("orders", "note", false);
  renameColumn("orders", "note", "memo", "text");
  addConstraint("orders", {
    name: "orders_customer_fk",
    kind: {
      kind: "fk",
      columns: ["customerId"],
      referencesTable: "customers",
      referencesColumns: ["id"],
    },
  });
  dropConstraint("orders", "orders_legacy_chk");
}
