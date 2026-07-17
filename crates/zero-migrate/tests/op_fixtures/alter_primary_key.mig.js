// Explicit primary-key lifecycle regression fixture. Before AlterPrimaryKey this
// public surface and wire op did not exist.
import { table } from "zero-migrate";

export const name = "alter_primary_key";

export function up() {
  const orders = table("orders", { schema: "app" });
  orders.primaryKey().add({ columns: ["legacy_id"] });
  orders.primaryKey().replace({
    expectedColumns: ["legacy_id"],
    columns: ["tenant_id", "order_id"],
    dropIdentityFrom: ["legacy_id"],
  });
  orders.primaryKey().drop({
    expectedColumns: ["tenant_id", "order_id"],
  });
}
