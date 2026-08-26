// op.* migration fixture — PostgreSQL online constraint adoption:
// foreignKey().add / check().add with `notValid: true` (rendered `ADD CONSTRAINT
// … NOT VALID`), then a later `constraint(name).validate()` (rendered `ALTER TABLE …
// VALIDATE CONSTRAINT …`). Gates the new `not_valid` FK/CHECK facet + the new
// `Op::ValidateConstraint` through the REAL recorder → frozen wire ops.
import { table } from "zero-migrate";

export const name = "constraint_not_valid";

export function schema() {
  const lineItems = table("line_items");
  const pgLineItems = table("line_items");
  // FK added NOT VALID — skip the add-time full-table scan.
  lineItems.foreignKey("line_items_order_fkey").add({
    columns: ["order_id"],
    references: { table: "orders", columns: ["id"] },
    notValid: true,
  });
  // CHECK added NOT VALID via the selector form.
  lineItems.check("line_items_qty_positive").add({
    expr: (col) => col("qty").gt(0),
    notValid: true,
  });
  // …then validate both later under a weaker lock.
  pgLineItems.constraint("line_items_order_fkey").validate();
  pgLineItems.constraint("line_items_qty_positive").validate();
}
